package main

import (
	"html"
	"sort"
	"strings"
	"unicode"
)

type hlFieldSpec struct {
	pattern        string
	typ            string
	pre, post      string
	fragmentSize   int
	numFragments   int
	noMatchSize    int
	boundary       string
	requireMatch   *bool
	highlightQuery map[string]any
	orderScore     bool
	encoder        string
}

type highlightConfig struct {
	typ               string
	pre, post         string
	requireFieldMatch bool
	orderScore        bool
	encoder           string
	fields            []hlFieldSpec
	terms             map[string]bool
	queryFields       map[string]bool
	queryPatterns     []string
}

func parseHighlightConfig(body map[string]any, query map[string]any) (*highlightConfig, *apiError) {
	raw, ok := body["highlight"].(map[string]any)
	if !ok {
		return nil, nil
	}
	cfg := &highlightConfig{typ: "unified", pre: "<em>", post: "</em>", requireFieldMatch: true}
	cfg.terms, cfg.queryFields, cfg.queryPatterns = queryHighlightInfo(query)
	if v, ok := raw["type"].(string); ok && v != "" {
		cfg.typ = v
	}
	if v, ok := raw["pre_tags"].([]any); ok && len(v) > 0 {
		cfg.pre, _ = toString(v[0])
	}
	if v, ok := raw["post_tags"].([]any); ok && len(v) > 0 {
		cfg.post, _ = toString(v[0])
	}
	if _, hasPre := raw["pre_tags"]; hasPre {
		if _, hasPost := raw["post_tags"]; !hasPost {
			return nil, highlightParseError("pre_tags are set but post_tags are not set")
		}
	}
	if v, ok := raw["tags_schema"].(string); ok && v == "styled" {
		cfg.pre = `<em class="hlt1">`
		cfg.post = "</em>"
	}
	if v, ok := raw["require_field_match"].(bool); ok {
		cfg.requireFieldMatch = v
	}
	if v, ok := raw["order"].(string); ok && v == "score" {
		cfg.orderScore = true
	}
	if v, ok := raw["encoder"].(string); ok {
		cfg.encoder = v
	}
	if _, has := raw["max_analyzed_offset"]; has {
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason:    "[highlight] unknown field [max_analyzed_offset] did you mean [max_analyzer_offset]?",
			RootCause: &apiError{Type: "parsing_exception", Reason: "[highlight] unknown field [max_analyzed_offset] did you mean [max_analyzer_offset]?"}}
	}
	fieldsRaw := raw["fields"]
	specs := []hlFieldSpec{}
	addSpec := func(name string, opts map[string]any) *apiError {
		fs := hlFieldSpec{pattern: name, typ: cfg.typ, pre: cfg.pre, post: cfg.post, fragmentSize: 100, numFragments: 5, orderScore: cfg.orderScore, encoder: cfg.encoder}
		if opts != nil {
			if v, ok := opts["type"].(string); ok && v != "" {
				fs.typ = v
			}
			if v, ok := opts["pre_tags"].([]any); ok && len(v) > 0 {
				fs.pre, _ = toString(v[0])
			}
			if v, ok := opts["post_tags"].([]any); ok && len(v) > 0 {
				fs.post, _ = toString(v[0])
			}
			if _, hasPre := opts["pre_tags"]; hasPre {
				if _, hasPost := opts["post_tags"]; !hasPost {
					return highlightParseError("pre_tags are set but post_tags are not set")
				}
			}
			if v, ok := toFloat(opts["fragment_size"]); ok {
				fs.fragmentSize = int(v)
			}
			if v, ok := toFloat(opts["number_of_fragments"]); ok {
				fs.numFragments = int(v)
			}
			if v, ok := toFloat(opts["no_match_size"]); ok {
				fs.noMatchSize = int(v)
			}
			if v, ok := opts["boundary_scanner"].(string); ok {
				fs.boundary = v
			}
			if v, ok := opts["require_field_match"].(bool); ok {
				fs.requireMatch = &v
			}
			if v, ok := opts["highlight_query"].(map[string]any); ok {
				fs.highlightQuery = v
			}
			if v, ok := opts["tags_schema"].(string); ok && v == "styled" {
				fs.pre = `<em class="hlt1">`
				fs.post = "</em>"
			}
		}
		if fs.typ != "unified" && fs.typ != "plain" && fs.typ != "fvh" {
			return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: "unknown highlighter type [" + fs.typ + "] for the field [" + name + "]"}}
		}
		specs = append(specs, fs)
		return nil
	}
	switch fv := fieldsRaw.(type) {
	case map[string]any:
		names := make([]string, 0, len(fv))
		for n := range fv {
			names = append(names, n)
		}
		sort.Strings(names)
		for _, n := range names {
			opts, _ := fv[n].(map[string]any)
			if err := addSpec(n, opts); err != nil {
				return nil, err
			}
		}
	case []any:
		for _, item := range fv {
			if m, ok := item.(map[string]any); ok {
				for n, ov := range m {
					opts, _ := ov.(map[string]any)
					if err := addSpec(n, opts); err != nil {
						return nil, err
					}
				}
			}
		}
	}
	cfg.fields = specs
	return cfg, nil
}

func highlightParseError(reason string) *apiError {
	return &apiError{Type: "x_content_parse_exception", Status: 400, Reason: reason,
		RootCause: &apiError{Type: "parsing_exception", Reason: reason}}
}

func (cfg *highlightConfig) validate(idx *Index) *apiError {
	for _, f := range cfg.fields {
		names := []string{f.pattern}
		if strings.ContainsAny(f.pattern, "*?") {
			names = nil
			re := wildcardRegex(f.pattern)
			for name := range idx.DocValues {
				_ = name
				break
			}
			for name := range idx.FieldTypes {
				if re.MatchString(name) {
					names = append(names, name)
				}
			}
		}
		for _, name := range names {
			if f.typ != "fvh" {
				continue
			}
			def, ok := mappingFieldDef(idx, name)
			if !ok {
				continue
			}
			tv, _ := def["term_vector"].(string)
			if !strings.Contains(tv, "positions") {
				return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
					RootCause: &apiError{Type: "illegal_argument_exception",
						Reason: "the field [" + name + "] should be indexed with term vector with position offsets to be used with fast vector highlighter"}}
			}
			if f.boundary == "sentence" && f.fragmentSize < 18 {
				return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
					RootCause: &apiError{Type: "illegal_argument_exception",
						Reason: "fragCharSize(" + itoa(f.fragmentSize) + ") is too small. It must be 18 or higher."}}
			}
		}
	}
	return nil
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	neg := n < 0
	if neg {
		n = -n
	}
	var b [20]byte
	i := len(b)
	for n > 0 {
		i--
		b[i] = byte('0' + n%10)
		n /= 10
	}
	if neg {
		i--
		b[i] = '-'
	}
	return string(b[i:])
}

type hlSpan struct{ start, end int }

func queryHighlightInfo(q map[string]any) (map[string]bool, map[string]bool, []string) {
	terms := map[string]bool{}
	fields := map[string]bool{}
	patterns := []string{}
	var walk func(m map[string]any)
	walk = func(m map[string]any) {
		for clause, raw := range m {
			switch clause {
			case "match", "match_phrase", "term":
				mm, ok := raw.(map[string]any)
				if !ok {
					continue
				}
				for field, val := range mm {
					if field == "_name" || field == "boost" {
						continue
					}
					fields[field] = true
					if vm, ok := val.(map[string]any); ok {
						val = vm["value"]
						if val == nil {
							val = vm["query"]
						}
					}
					text, _ := queryText(val)
					for _, t := range analyze(text) {
						terms[t] = true
					}
				}
			case "terms":
				mm, ok := raw.(map[string]any)
				if !ok {
					continue
				}
				for field, val := range mm {
					if field == "_name" || field == "boost" {
						continue
					}
					fields[field] = true
					if arr, ok := val.([]any); ok {
						for _, e := range arr {
							text, _ := toString(e)
							for _, t := range analyze(text) {
								terms[t] = true
							}
						}
					}
				}
			case "multi_match":
				mm, ok := raw.(map[string]any)
				if !ok {
					continue
				}
				text, _ := toString(mm["query"])
				for _, t := range analyze(text) {
					terms[t] = true
				}
				if arr, ok := mm["fields"].([]any); ok {
					for _, e := range arr {
						name, _ := toString(e)
						if name != "" {
							patterns = append(patterns, name)
						}
					}
				}
			case "query_string", "simple_query_string":
				mm, ok := raw.(map[string]any)
				if !ok {
					continue
				}
				text, _ := toString(mm["query"])
				clean := strings.NewReplacer("(", " ", ")", " ", ":", " ", "\"", " ", "AND", " ", "OR", " ", "NOT", " ").Replace(text)
				for _, t := range analyze(clean) {
					terms[t] = true
				}
				if arr, ok := mm["fields"].([]any); ok {
					for _, e := range arr {
						name, _ := toString(e)
						if name != "" {
							patterns = append(patterns, name)
						}
					}
				}
			case "bool":
				bm, ok := raw.(map[string]any)
				if !ok {
					continue
				}
				for _, key := range []string{"must", "should", "filter", "must_not"} {
					for _, item := range asQueryList(bm[key]) {
						if child, ok := item.(map[string]any); ok {
							walk(child)
						}
					}
				}
			}
		}
	}
	walk(q)
	return terms, fields, patterns
}

func (cfg *highlightConfig) render(idx *Index, doc map[string]any) map[string]any {
	out := map[string]any{}
	for _, f := range cfg.fields {
		names := []string{f.pattern}
		if strings.ContainsAny(f.pattern, "*?") {
			names = nil
			re := wildcardRegex(f.pattern)
			for name := range doc {
				if re.MatchString(name) {
					names = append(names, name)
				}
			}
			sort.Strings(names)
		}
		for _, name := range names {
			def, mapped := mappingFieldDef(idx, name)
			ftype := ""
			if mapped {
				ftype, _ = def["type"].(string)
			}
			if ftype != "text" && ftype != "keyword" {
				continue
			}
			val, ok := fieldValue(doc, name)
			if !ok {
				continue
			}
			text, ok := toString(val)
			if !ok {
				continue
			}
			terms, qfields, qpatterns := cfg.terms, cfg.queryFields, cfg.queryPatterns
			if f.highlightQuery != nil {
				terms, qfields, qpatterns = queryHighlightInfo(f.highlightQuery)
			}
			require := cfg.requireFieldMatch
			if f.requireMatch != nil {
				require = *f.requireMatch
			}
			fieldAllowed := true
			if require && len(qfields)+len(qpatterns) > 0 {
				fieldAllowed = qfields[name]
				for _, p := range qpatterns {
					if wildcardRegex(p).MatchString(name) {
						fieldAllowed = true
					}
				}
			}
			spans := hlTokenSpans(text, terms)
			if ftype == "keyword" && len(spans) == 0 {
				for t := range terms {
					if strings.EqualFold(text, t) {
						spans = append(spans, hlSpan{0, len(text)})
						break
					}
				}
			}
			if len(spans) == 0 {
				if f.noMatchSize > 0 {
					n := f.noMatchSize
					if n > len(text) {
						n = len(text)
					}
					out[name] = []any{text[:n]}
				}
				continue
			}
			if !fieldAllowed {
				if f.noMatchSize > 0 {
					n := f.noMatchSize
					if n > len(text) {
						n = len(text)
					}
					out[name] = []any{text[:n]}
				}
				continue
			}
			out[name] = hlFragments(text, spans, f)
		}
	}
	return out
}

func hlTokenSpans(text string, terms map[string]bool) []hlSpan {
	spans := []hlSpan{}
	start := -1
	flush := func(end int) {
		if start >= 0 {
			tok := strings.ToLower(text[start:end])
			if terms[tok] {
				spans = append(spans, hlSpan{start, end})
			}
			start = -1
		}
	}
	for i, r := range text {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			if start < 0 {
				start = i
			}
		} else {
			flush(i)
		}
	}
	flush(len(text))
	return spans
}

func hlFragments(text string, spans []hlSpan, f hlFieldSpec) []any {
	fs := f.fragmentSize
	if fs <= 0 {
		fs = 100
	}
	if f.numFragments == 0 || len(text) <= fs {
		return []any{hlBuild(text, spans, 0, len(text), 0, f.pre, f.post, f.encoder)}
	}
	if f.boundary == "word" {
		out := []any{}
		for i, sp := range spans {
			if f.numFragments > 0 && i >= f.numFragments {
				break
			}
			out = append(out, hlBuild(text, []hlSpan{sp}, sp.start, sp.end, 0, f.pre, f.post, f.encoder))
		}
		return out
	}
	type frag struct {
		from, to int
		count    int
		text     string
	}
	frags := []frag{}
	if f.typ == "fvh" {
		covered := -1
		for _, sp := range spans {
			if sp.start < covered {
				continue
			}
			start := sp.start - fs*2/5
			if start < 0 {
				start = 0
			}
			start = hlSnapLeft(text, start)
			windowEnd := start + fs
			if windowEnd > len(text) {
				windowEnd = len(text)
			}
			end := hlSnapRight(text, windowEnd)
			count := 0
			for _, s2 := range spans {
				if s2.start >= start && s2.start < windowEnd {
					count++
				}
			}
			frags = append(frags, frag{start, end, count, hlBuild(text, spans, start, end, windowEnd, f.pre, f.post, f.encoder)})
			covered = end
		}
	} else if f.typ == "plain" {
		for _, sp := range spans {
			start := sp.start - fs/2
			if start < 0 {
				start = 0
			}
			end := sp.start + fs/2
			if end > len(text) {
				end = len(text)
			}
			if end <= start {
				end = sp.end
				if end > len(text) {
					end = len(text)
				}
			}
			count := 0
			for _, s2 := range spans {
				if s2.start >= start && s2.start < end {
					count++
				}
			}
			frags = append(frags, frag{start, end, count, hlBuild(text, spans, start, end, 0, f.pre, f.post, f.encoder)})
		}
	} else {
		groups := [][]hlSpan{}
		for _, sp := range spans {
			if len(groups) == 0 || sp.start-groups[len(groups)-1][0].start > fs {
				groups = append(groups, []hlSpan{sp})
			} else {
				groups[len(groups)-1] = append(groups[len(groups)-1], sp)
			}
		}
		for _, g := range groups {
			anchor := g[len(g)-1]
			start := anchor.start - fs/2
			if len(g) > 1 {
				shift := fs/2 - (anchor.end - anchor.start)
				if shift > 0 {
					start += shift
				}
			}
			if start < 0 {
				start = 0
			}
			start = hlSnapLeft(text, start)
			end := start + fs
			if end > len(text) {
				end = len(text)
			}
			if end < anchor.end {
				end = anchor.end
			}
			if end-start < fs {
				end = hlExtendRight(text, end, fs-(end-start))
			}
			end = hlSnapRight(text, end)
			if end > len(text) {
				end = len(text)
			}
			count := 0
			for _, s2 := range spans {
				if s2.start >= start && s2.start < end {
					count++
				}
			}
			frags = append(frags, frag{start, end, count, hlBuild(text, spans, start, end, 0, f.pre, f.post, f.encoder)})
		}
	}
	if f.numFragments == 1 && len(frags) > 1 {
		best := 0
		for i := range frags {
			if frags[i].count > frags[best].count {
				best = i
			}
		}
		frags = []frag{frags[best]}
	} else if f.orderScore {
		sort.SliceStable(frags, func(i, j int) bool { return frags[i].count > frags[j].count })
	}
	if f.numFragments > 0 && len(frags) > f.numFragments {
		frags = frags[:f.numFragments]
	}
	out := []any{}
	for _, fr := range frags {
		out = append(out, fr.text)
	}
	return out
}

func hlBuild(text string, spans []hlSpan, from, to, windowEnd int, pre, post, encoder string) string {
	if from < 0 {
		from = 0
	}
	if to > len(text) {
		to = len(text)
	}
	var b strings.Builder
	last := from
	for _, sp := range spans {
		if sp.start < from || sp.start >= to {
			continue
		}
		if windowEnd > 0 && sp.start >= windowEnd {
			continue
		}
		b.WriteString(hlEscape(text[last:sp.start], encoder))
		b.WriteString(pre)
		b.WriteString(hlEscape(text[sp.start:sp.end], encoder))
		b.WriteString(post)
		last = sp.end
	}
	b.WriteString(hlEscape(text[last:to], encoder))
	return b.String()
}

func hlEscape(s, encoder string) string {
	if encoder == "html" {
		return html.EscapeString(s)
	}
	return s
}

func hlSnapLeft(text string, pos int) int {
	if pos <= 0 {
		return 0
	}
	if pos >= len(text) {
		return len(text)
	}
	if !unicode.IsSpace(rune(text[pos])) {
		for pos > 0 && !unicode.IsSpace(rune(text[pos-1])) {
			pos--
		}
		return pos
	}
	for pos > 0 && unicode.IsSpace(rune(text[pos])) {
		pos--
	}
	for pos > 0 && !unicode.IsSpace(rune(text[pos-1])) {
		pos--
	}
	return pos
}

func hlSnapRight(text string, pos int) int {
	if pos <= 0 || pos >= len(text) {
		return pos
	}
	if unicode.IsSpace(rune(text[pos])) {
		return pos
	}
	for pos < len(text) && (unicode.IsLetter(rune(text[pos])) || unicode.IsDigit(rune(text[pos]))) {
		pos++
	}
	return pos
}

func hlExtendRight(text string, pos, need int) int {
	for need > 0 && pos < len(text) {
		for pos < len(text) && unicode.IsSpace(rune(text[pos])) {
			pos++
		}
		for pos < len(text) && !unicode.IsSpace(rune(text[pos])) {
			pos++
			need--
		}
	}
	return pos
}
