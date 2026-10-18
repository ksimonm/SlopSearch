package main

import (
	"fmt"
	"net/http"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

type multiHit struct {
	idx   *Index
	id    string
	doc   map[string]any
	score float64
}

func checkDocValuesLoadable(idx *Index, field string) *apiError {
	def, ok := mappingFieldDef(idx, field)
	if !ok {
		return nil
	}
	ftype, _ := def["type"].(string)
	if dv, ok := def["doc_values"].(bool); ok && !dv {
		reason := "Can't load fielddata on [" + field + "] because fielddata is unsupported on fields of type [" + ftype + "]. Use doc values instead."
		return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
			CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	if ftype == "text" {
		if fd, ok := def["fielddata"].(bool); !ok || !fd {
			reason := "Fielddata is disabled on text fields by default. Set fielddata=true on [" + field + "] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
			return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
				CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
		}
	}
	return nil
}

type fieldSpec struct {
	name   string
	format string
}

func parseFieldSpecs(raw any) []fieldSpec {
	var out []fieldSpec
	list, ok := raw.([]any)
	if !ok {
		return nil
	}
	for _, e := range list {
		switch t := e.(type) {
		case string:
			out = append(out, fieldSpec{name: t})
		case map[string]any:
			name, _ := toString(t["field"])
			if name == "" {
				continue
			}
			format, _ := toString(t["format"])
			out = append(out, fieldSpec{name: name, format: format})
		}
	}
	return out
}

var textLikeTypes = map[string]bool{
	"text": true, "match_only_text": true, "search_as_you_type": true,
	"completion": true, "annotated_text": true,
}

// docValuesOutput returns the values to render for docvalue_fields / fields.
func docValuesOutput(idx *Index, id, field string, spec fieldSpec, fallbackSource bool) ([]any, bool, *apiError) {
	def, hasDef := mappingFieldDef(idx, field)
	ftype := ""
	hasDV := true
	if hasDef {
		ftype, _ = def["type"].(string)
		if dv, ok := def["doc_values"].(bool); ok && !dv {
			hasDV = false
		}
	}
	var vals []any
	if dv := idx.DocValues[id]; dv != nil {
		vals = dv[field]
	}
	formatted := func(list []any) []any {
		out := make([]any, 0, len(list))
		for _, v := range list {
			out = append(out, formatDocValue(idx, field, ftype, v, spec.format))
		}
		return out
	}
	if textLikeTypes[ftype] || !hasDV {
		if !fallbackSource {
			if !hasDV && len(vals) > 0 {
				reason := "Can't load fielddata on [" + field + "] because fielddata is unsupported on fields of type [" + ftype + "]. Use doc values instead."
				return nil, false, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
					RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
					CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
			}
			return nil, false, nil
		}
		// fall back to _source values
		if v, ok := fieldValue(idx.Docs[id], field); ok {
			list := flattenAny(v)
			return formatted(list), true, nil
		}
		return nil, false, nil
	}
	if len(vals) == 0 {
		return nil, false, nil
	}
	return formatted(vals), true, nil
}

func flattenAny(v any) []any {
	switch t := v.(type) {
	case []any:
		out := []any{}
		for _, e := range t {
			out = append(out, flattenAny(e)...)
		}
		return out
	case nil:
		return nil
	default:
		return []any{v}
	}
}

func formatDocValue(idx *Index, field, ftype string, v any, format string) any {
	switch ftype {
	case "date", "date_nanos":
		var ms int64
		switch t := v.(type) {
		case int64:
			ms = t
		case uint64:
			ms = int64(t)
		default:
			if n, ok := integerOf(v); ok {
				ms = n
			} else {
				return v
			}
		}
		if ftype == "date_nanos" {
			ms = ms / int64(1e6)
		}
		switch format {
		case "":
			return time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")
		case "strict_date_optional_time", "date_optional_time":
			return time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")
		case "epoch_millis":
			return strconv.FormatInt(ms, 10)
		case "epoch_second":
			return strconv.FormatInt(ms/1000, 10)
		default:
			if layout := javaDateFormatToGo(format); layout != "" {
				return time.UnixMilli(ms).UTC().Format(layout)
			}
			return strconv.FormatInt(ms, 10)
		}
	}
	return v
}

func (h multiHit) sortValue(field string, desc bool) (any, bool) {
	switch field {
	case "_score":
		return h.score, true
	case "_id":
		return h.id, true
	}
	if def, ok := mappingFieldDef(h.idx, field); ok {
		if t, _ := def["type"].(string); t == "alias" {
			field = resolveAliasField(h.idx, field)
		}
	}
	vals := h.idx.DocValues[h.id]
	if vals == nil {
		return nil, false
	}
	list, ok := vals[field]
	if !ok || len(list) == 0 {
		return nil, false
	}
	if len(list) == 1 {
		return list[0], true
	}
	sorted := append([]any{}, list...)
	sort.SliceStable(sorted, func(i, j int) bool {
		return compareSortValues(sorted[i], sorted[j]) < 0
	})
	if desc {
		return sorted[len(sorted)-1], true
	}
	return sorted[0], true
}

func compareSortValues(a, b any) int {
	if as, ok := a.(string); ok {
		bs, _ := toString(b)
		if _, bok := b.(string); bok {
			return strings.Compare(as, bs)
		}
		return strings.Compare(as, bs)
	}
	if af, ok := toFloat(a); ok {
		if bf, ok := toFloat(b); ok {
			switch {
			case af < bf:
				return -1
			case af > bf:
				return 1
			}
			return 0
		}
	}
	return compareValues(a, b)
}

func (s *Server) handleSearch(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{closedError: true})
	indices, apiErr := s.resolveSpec2(name, o)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}

	query, _ := body["query"].(map[string]any)
	if query == nil {
		if qp := r.URL.Query().Get("q"); qp != "" {
			query = parseQParam(qp)
		}
	}
	if query == nil {
		query = map[string]any{"match_all": map[string]any{}}
	}

	from := intField(body, "from", 0)
	size := intField(body, "size", 10)
	if v := r.URL.Query().Get("from"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			from = n
		}
	}
	if v := r.URL.Query().Get("size"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			size = n
		}
	}
	for _, idx := range indices {
		window := 10000
		if v, ok := findSetting(idx.Settings, "max_result_window"); ok {
			if n, ok := settingsValueInt(v); ok {
				window = n
			}
		}
		if from+size > window {
			reason := fmt.Sprintf("Result window is too large, from + size must be less than or equal to: [%d] but was [%d]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.", window, from+size)
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: reason, RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
			return
		}
	}

	var matched []multiHit
	shards := 0
	for _, idx := range indices {
		if err := readBlockError(idx); err != nil {
			writeError(w, err)
			return
		}
		q := query
		if f := searchAliasFilter(idx, name); f != nil {
			q = map[string]any{"bool": map[string]any{"must": []any{query, f}}}
		}
		shards += idx.numShards()
		for _, id := range idx.Order {
			doc, live := idx.Docs[id]
			if !live {
				continue
			}
			if _, pending := idx.Pending[id]; pending {
				continue
			}
			qc := newQueryCtxForDoc(idx, id, doc)
			ok, score, err := qc.eval(q)
			if err != nil {
				writeError(w, err)
				return
			}
			if ok {
				matched = append(matched, multiHit{idx: idx, id: id, doc: doc, score: score})
			}
		}
	}
	total := len(matched)

	sortedByField := false
	if sortSpec, ok := body["sort"].([]any); ok && len(sortSpec) > 0 {
		field, desc := parseSort(sortSpec[0])
		if field != "" && field != "_score" {
			sortedByField = true
			for _, idx := range indices {
				if err := checkDocValuesLoadable(idx, field); err != nil {
					writeError(w, err)
					return
				}
			}
			sort.SliceStable(matched, func(i, j int) bool {
				a, aok := matched[i].sortValue(field, desc)
				b, bok := matched[j].sortValue(field, desc)
				if !aok && !bok {
					return false
				}
				if !aok {
					return false
				}
				if !bok {
					return true
				}
				c := compareSortValues(a, b)
				if desc {
					return c > 0
				}
				return c < 0
			})
		}
	}
	trackScores, _ := body["track_scores"].(bool)

	if from > len(matched) {
		from = len(matched)
	}
	end := from + size
	if end > len(matched) {
		end = len(matched)
	}
	page := matched[from:end]

	sourceFilter, hasSource := body["_source"]
	omitSource := false
	if hasSource {
		if b, ok := sourceFilter.(bool); ok {
			omitSource = !b
			hasSource = false
		}
	}
	highlightFields := highlightFieldsOf(body)
	queryTokens := collectTokens(query)
	dvFields := parseFieldSpecs(body["docvalue_fields"])
	paramFields := parseFieldSpecs(body["fields"])

	omitScores := sortedByField && !trackScores
	var maxScore any
	if !omitScores && len(page) > 0 {
		m := page[0].score
		for _, h := range page[1:] {
			if h.score > m {
				m = h.score
			}
		}
		maxScore = m
	}
	hits := make([]any, 0, len(page))
	for _, h := range page {
		src := h.doc
		if hasSource {
			src = filterSource(h.doc, sourceFilter)
		}
		var scoreVal any = h.score
		if omitScores {
			scoreVal = nil
		}
		entry := map[string]any{
			"_index": h.idx.Name, "_id": h.id, "_score": scoreVal,
		}
		if !omitSource {
			entry["_source"] = src
		}
		if len(highlightFields) > 0 {
			hl := map[string]any{}
			for _, f := range highlightFields {
				if v, ok := fieldValue(h.doc, f); ok {
					if str, ok := toString(v); ok {
						hl[f] = []any{highlightFragment(str, queryTokens)}
					}
				}
			}
			if len(hl) > 0 {
				entry["highlight"] = hl
			}
		}
		if len(dvFields) > 0 || len(paramFields) > 0 {
			out := map[string]any{}
			collect := func(specs []fieldSpec, fallback bool) *apiError {
				for _, spec := range specs {
					names := []string{spec.name}
					if strings.ContainsAny(spec.name, "*?") {
						names = nil
						re := wildcardRegex(spec.name)
						for f := range h.idx.DocValues[h.id] {
							if re.MatchString(f) {
								names = append(names, f)
							}
						}
						sort.Strings(names)
					}
					for _, name := range names {
						if _, done := out[name]; done {
							continue
						}
						vals, found, derr := docValuesOutput(h.idx, h.id, name, spec, fallback)
						if derr != nil {
							return derr
						}
						if found {
							out[name] = vals
						}
					}
				}
				return nil
			}
			if derr := collect(dvFields, false); derr != nil {
				writeError(w, derr)
				return
			}
			if derr := collect(paramFields, true); derr != nil {
				writeError(w, derr)
				return
			}
			if len(out) > 0 {
				entry["fields"] = out
			}
		}
		if sf, ok := body["script_fields"].(map[string]any); ok && len(sf) > 0 {
			got, serr := evalScriptFields(h, sf)
			if serr != nil {
				writeError(w, serr)
				return
			}
			if existing, ok := entry["fields"].(map[string]any); ok {
				for k, v := range got {
					existing[k] = v
				}
			} else {
				entry["fields"] = got
			}
		}
		hits = append(hits, entry)
	}

	resp := map[string]any{
		"took":      1,
		"timed_out": false,
		"_shards":   map[string]any{"total": shards, "successful": shards, "skipped": 0, "failed": 0},
		"hits": map[string]any{
			"total":     map[string]any{"value": total, "relation": "eq"},
			"max_score": maxScore,
			"hits":      hits,
		},
	}

	var aggIdx *Index
	if len(indices) > 0 {
		aggIdx = indices[0]
	}
	if aggIdx != nil {
		for _, key := range []string{"aggs", "aggregations"} {
			if aggs, ok := body[key].(map[string]any); ok {
				aggHits := make([]aggHit, 0, len(matched))
				for _, h := range matched {
					aggHits = append(aggHits, aggHit{idx: h.idx, id: h.id, doc: h.doc})
				}
				out, err := evalAggs(aggs, aggHits, aggIdx, "")
				if err != nil {
					writeError(w, err)
					return
				}
				resp["aggregations"] = out
				break
			}
		}
		if suggest, ok := body["suggest"].(map[string]any); ok {
			resp["suggest"] = evalSuggest(suggest, aggIdx)
		}
	}

	writeJSON(w, 200, resp)
}

func multiDocs(hits []multiHit) []map[string]any {
	out := make([]map[string]any, 0, len(hits))
	for _, h := range hits {
		out = append(out, h.doc)
	}
	return out
}

type hit struct {
	id    string
	doc   map[string]any
	score float64
}

func matchedDocs(matched []hit) []map[string]any {
	out := make([]map[string]any, 0, len(matched))
	for _, m := range matched {
		out = append(out, m.doc)
	}
	return out
}

func intField(body map[string]any, key string, def int) int {
	if v, ok := body[key]; ok {
		if f, ok := toFloat(v); ok {
			return int(f)
		}
	}
	return def
}

func parseSort(spec any) (string, bool) {
	switch t := spec.(type) {
	case string:
		return t, false
	case map[string]any:
		for k, v := range t {
			if k == "_score" {
				continue
			}
			dir, _ := toString(v)
			return k, dir == "desc"
		}
	}
	return "", false
}

func compareValues(a, b any) int {
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	if aok && bok {
		switch {
		case af < bf:
			return -1
		case af > bf:
			return 1
		default:
			return 0
		}
	}
	as, _ := toString(a)
	bs, _ := toString(b)
	return strings.Compare(as, bs)
}

func fieldType(idx *Index, field string) string {
	if t, ok := idx.FieldTypes[field]; ok {
		return t
	}
	return "text"
}

func isNumericType(t string) bool {
	switch t {
	case "integer", "long", "short", "byte", "float", "double", "half_float", "scaled_float", "date":
		return true
	}
	return false
}

func docStrings(v any) []string {
	switch t := v.(type) {
	case []any:
		out := []string{}
		for _, e := range t {
			if s, ok := toString(e); ok {
				out = append(out, s)
			}
		}
		return out
	case nil:
		return nil
	default:
		if s, ok := toString(t); ok {
			return []string{s}
		}
	}
	return nil
}

var knownClauses = map[string]bool{
	"match_all": true, "match": true, "term": true, "terms": true, "range": true,
	"bool": true, "match_phrase": true, "multi_match": true, "wildcard": true,
	"fuzzy": true, "exists": true, "prefix": true,
}

func validateQuery(idx *Index, query map[string]any) *apiError {
	for clause, raw := range query {
		if !knownClauses[clause] {
			return errParsing("unknown query [" + clause + "]")
		}
		switch clause {
		case "range":
			m, ok := raw.(map[string]any)
			if !ok {
				return errParsing("bad range")
			}
			field, condRaw, ok := singleEntry(m)
			if !ok {
				return errParsing("empty range")
			}
			ft := fieldType(idx, field)
			if cond, ok := condRaw.(map[string]any); ok {
				for _, bound := range cond {
					if ft == "date" {
						if _, ok := parseDate(bound); !ok {
							return errParsing("failed to parse date [" + field + "]")
						}
					} else if isNumericType(ft) {
						if _, ok := toFloat(bound); !ok {
							return errParsing("failed to parse number [" + field + "]")
						}
					}
				}
			}
		case "bool":
			if bm, ok := raw.(map[string]any); ok {
				for _, key := range []string{"must", "should", "filter", "must_not"} {
					for _, q := range asQueryList(bm[key]) {
						if qm, ok := q.(map[string]any); ok {
							if err := validateQuery(idx, qm); err != nil {
								return err
							}
						}
					}
				}
			}
		}
	}
	return nil
}

func evalQuery(idx *Index, query map[string]any, doc map[string]any) (bool, float64, *apiError) {
	if len(query) == 0 {
		return true, 1, nil
	}
	for clause, raw := range query {
		switch clause {
		case "match_all":
			return true, 1, nil
		case "match":
			return evalMatch(idx, raw, doc)
		case "term":
			return evalTerm(idx, raw, doc, 0)
		case "terms":
			return evalTerms(idx, raw, doc)
		case "range":
			return evalRange(idx, raw, doc)
		case "bool":
			return evalBool(idx, raw, doc)
		case "match_phrase":
			return evalMatchPhrase(idx, raw, doc)
		case "multi_match":
			return evalMultiMatch(idx, raw, doc)
		case "wildcard":
			return evalWildcard(idx, raw, doc)
		case "fuzzy":
			return evalFuzzy(idx, raw, doc)
		case "exists":
			return evalExists(raw, doc)
		case "prefix":
			return evalPrefix(idx, raw, doc)
		default:
			return false, 0, errParsing("unknown query [" + clause + "]")
		}
	}
	return false, 0, errParsing("empty query")
}

func singleEntry(m map[string]any) (string, any, bool) {
	for k, v := range m {
		return k, v, true
	}
	return "", nil, false
}

func queryText(raw any) (string, any) {
	if m, ok := raw.(map[string]any); ok {
		if q, ok := m["query"]; ok {
			s, _ := toString(q)
			return s, m
		}
	}
	s, _ := toString(raw)
	return s, nil
}

func evalMatch(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty match")
	}
	text, opts := queryText(val)
	operator := ""
	if om, ok := opts.(map[string]any); ok {
		if o, ok := om["operator"].(string); ok {
			operator = o
		}
	}
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	if isNumericType(ft) || ft == "keyword" || ft == "boolean" {
		for _, s := range docStrings(v) {
			if s == text {
				return true, 1, nil
			}
		}
		return false, 0, nil
	}
	qTokens := analyze(text)
	var docTokens []string
	for _, s := range docStrings(v) {
		docTokens = append(docTokens, analyze(s)...)
	}
	score := 0.0
	for _, qt := range qTokens {
		for _, dt := range docTokens {
			if qt == dt {
				score++
				break
			}
		}
	}
	if operator == "and" {
		return len(qTokens) > 0 && score == float64(len(qTokens)), score, nil
	}
	return score > 0, score, nil
}

func evalPrefix(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty prefix")
	}
	prefix := ""
	if m, ok := val.(map[string]any); ok {
		prefix, _ = toString(m["value"])
	} else {
		prefix, _ = toString(val)
	}
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if strings.HasPrefix(c, prefix) {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalTerm(idx *Index, raw any, doc map[string]any, fuzz int) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty term")
	}
	if m, ok := val.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if f, ok := m["fuzziness"]; ok {
			if n, ok := toFloat(f); ok {
				fuzz = int(n)
			}
		}
	}
	term, _ := toString(val)
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if c == term {
				return true, 1, nil
			}
			if fuzz > 0 && levenshtein(c, term) <= fuzz {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalFuzzy(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, _ := singleEntry(raw.(map[string]any))
	fuzz := autoFuzziness(field)
	if m, ok := val.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			text, _ := toString(v)
			fuzz = autoFuzziness(text)
		}
	}
	return evalTerm(idx, raw, doc, fuzz)
}

func evalTerms(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty terms")
	}
	list, ok := val.([]any)
	if !ok {
		return false, 0, errParsing("terms expects array")
	}
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	ft := fieldType(idx, field)
	for _, item := range list {
		want, _ := toString(item)
		for _, s := range docStrings(v) {
			candidates := []string{s}
			if ft == "text" {
				candidates = analyze(s)
			}
			for _, c := range candidates {
				if c == want {
					return true, 1, nil
				}
			}
		}
	}
	return false, 0, nil
}

func compareRange(dv float64, cond map[string]any, field string, parse func(any) (float64, bool)) (bool, float64, *apiError) {
	for op, bound := range cond {
		bv, ok := parse(bound)
		if !ok {
			return false, 0, errParsing("failed to parse [" + field + "]")
		}
		switch op {
		case "gt":
			if !(dv > bv) {
				return false, 0, nil
			}
		case "gte":
			if !(dv >= bv) {
				return false, 0, nil
			}
		case "lt":
			if !(dv < bv) {
				return false, 0, nil
			}
		case "lte":
			if !(dv <= bv) {
				return false, 0, nil
			}
		}
	}
	return true, 1, nil
}

func evalRange(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad range")
	}
	field, condRaw, ok := singleEntry(m)
	if !ok {
		return false, 0, errParsing("empty range")
	}
	cond, ok := condRaw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad range condition")
	}
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	ft := fieldType(idx, field)
	if ft == "date" {
		dv, ok := parseDate(v)
		if !ok {
			return false, 0, nil
		}
		return compareRange(dv, cond, field, parseDate)
	}
	if isNumericType(ft) {
		dv, ok := toFloat(v)
		if !ok {
			return false, 0, nil
		}
		return compareRange(dv, cond, field, toFloat)
	}
	ds, _ := toString(v)
	for op, bound := range cond {
		bs, _ := toString(bound)
		c := strings.Compare(ds, bs)
		switch op {
		case "gt":
			if !(c > 0) {
				return false, 0, nil
			}
		case "gte":
			if !(c >= 0) {
				return false, 0, nil
			}
		case "lt":
			if !(c < 0) {
				return false, 0, nil
			}
		case "lte":
			if !(c <= 0) {
				return false, 0, nil
			}
		}
	}
	return true, 1, nil
}

func asQueryList(v any) []any {
	switch t := v.(type) {
	case []any:
		return t
	case map[string]any:
		return []any{t}
	}
	return nil
}

func evalBool(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad bool")
	}
	score := 0.0
	must := asQueryList(m["must"])
	filter := asQueryList(m["filter"])
	should := asQueryList(m["should"])
	mustNot := asQueryList(m["must_not"])

	for _, q := range append(append([]any{}, must...), filter...) {
		ok, sc, err := evalQuery(idx, q.(map[string]any), doc)
		if err != nil {
			return false, 0, err
		}
		if !ok {
			return false, 0, nil
		}
		score += sc
	}
	if len(should) > 0 {
		anyMatched := false
		for _, q := range should {
			ok, sc, err := evalQuery(idx, q.(map[string]any), doc)
			if err != nil {
				return false, 0, err
			}
			if ok {
				anyMatched = true
				score += sc
			}
		}
		if !anyMatched && len(must) == 0 && len(filter) == 0 {
			return false, 0, nil
		}
	}
	for _, q := range mustNot {
		ok, _, err := evalQuery(idx, q.(map[string]any), doc)
		if err != nil {
			return false, 0, err
		}
		if ok {
			return false, 0, nil
		}
	}
	if len(must) == 0 && len(filter) == 0 && len(should) == 0 {
		return true, 1, nil
	}
	return true, score, nil
}

func evalMatchPhrase(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty match_phrase")
	}
	text, _ := queryText(val)
	qTokens := analyze(text)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		dTokens := analyze(s)
		if containsSubsequence(dTokens, qTokens) {
			return true, float64(len(qTokens)), nil
		}
	}
	return false, 0, nil
}

func containsSubsequence(hay, needle []string) bool {
	if len(needle) == 0 {
		return false
	}
	for i := 0; i+len(needle) <= len(hay); i++ {
		ok := true
		for j := range needle {
			if hay[i+j] != needle[j] {
				ok = false
				break
			}
		}
		if ok {
			return true
		}
	}
	return false
}

func evalMultiMatch(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad multi_match")
	}
	q, _ := toString(m["query"])
	fields, _ := m["fields"].([]any)
	total := 0.0
	for _, f := range fields {
		fs, _ := toString(f)
		ok, sc, err := evalMatch(idx, map[string]any{fs: q}, doc)
		if err != nil {
			return false, 0, err
		}
		if ok {
			total += sc
		}
	}
	return total > 0, total, nil
}

var wildcardCache = map[string]*regexp.Regexp{}

func wildcardRegex(pattern string) *regexp.Regexp {
	if re, ok := wildcardCache[pattern]; ok {
		return re
	}
	var b strings.Builder
	b.WriteString("^")
	for _, ch := range pattern {
		switch ch {
		case '*':
			b.WriteString(".*")
		case '?':
			b.WriteString(".")
		default:
			b.WriteString(regexp.QuoteMeta(string(ch)))
		}
	}
	b.WriteString("$")
	re := regexp.MustCompile(b.String())
	wildcardCache[pattern] = re
	return re
}

func evalWildcard(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty wildcard")
	}
	m, ok := val.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad wildcard body")
	}
	pattern, _ := toString(m["value"])
	re := wildcardRegex(pattern)
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if re.MatchString(c) {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalExists(raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad exists")
	}
	field, _ := toString(m["field"])
	_, present := fieldValue(doc, field)
	return present, 1, nil
}

func filterSource(doc map[string]any, spec any) map[string]any {
	var includes []string
	switch t := spec.(type) {
	case string:
		includes = []string{t}
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				includes = append(includes, s)
			}
		}
	case map[string]any:
		if inc, ok := t["includes"].([]any); ok {
			for _, e := range inc {
				if s, ok := toString(e); ok {
					includes = append(includes, s)
				}
			}
		}
	}
	if len(includes) == 0 {
		return doc
	}
	out := map[string]any{}
	for _, f := range includes {
		if v, ok := doc[f]; ok {
			out[f] = v
		}
	}
	return out
}

func highlightFieldsOf(body map[string]any) []string {
	hl, ok := body["highlight"].(map[string]any)
	if !ok {
		return nil
	}
	fields, ok := hl["fields"].(map[string]any)
	if !ok {
		return nil
	}
	out := []string{}
	for f := range fields {
		out = append(out, f)
	}
	sort.Strings(out)
	return out
}

func highlightFragment(text string, tokens map[string]bool) string {
	fields := strings.Fields(text)
	for i, w := range fields {
		clean := strings.ToLower(strings.Trim(w, ".,!?"))
		if tokens[clean] {
			fields[i] = "<em>" + w + "</em>"
		}
	}
	return strings.Join(fields, " ")
}

func collectTokens(query map[string]any) map[string]bool {
	out := map[string]bool{}
	var walk func(m map[string]any)
	walk = func(m map[string]any) {
		for clause, raw := range m {
			switch clause {
			case "match", "match_phrase":
				if mm, ok := raw.(map[string]any); ok {
					_, val, _ := singleEntry(mm)
					text, _ := queryText(val)
					for _, t := range analyze(text) {
						out[t] = true
					}
				}
			case "multi_match":
				if mm, ok := raw.(map[string]any); ok {
					text, _ := toString(mm["query"])
					for _, t := range analyze(text) {
						out[t] = true
					}
				}
			case "bool":
				if bm, ok := raw.(map[string]any); ok {
					for _, key := range []string{"must", "should", "filter"} {
						for _, q := range asQueryList(bm[key]) {
							if qm, ok := q.(map[string]any); ok {
								walk(qm)
							}
						}
					}
				}
			}
		}
	}
	walk(query)
	return out
}
