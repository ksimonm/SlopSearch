package main

import (
	"net/http"
	"sort"
	"strings"
	"unicode"
)

// Analysis framework: analyzers, tokenizers, token filters and the _analyze API.

// analyzeWithIndex tokenizes text with a named analyzer, resolving custom
// analyzers configured in index settings. Falls back to the built-in set.
var englishStopWords = map[string]bool{
	"a": true, "an": true, "and": true, "are": true, "as": true, "at": true,
	"be": true, "but": true, "by": true, "for": true, "if": true, "in": true,
	"into": true, "is": true, "it": true, "no": true, "not": true, "of": true,
	"on": true, "or": true, "such": true, "that": true, "the": true, "their": true,
	"then": true, "there": true, "these": true, "they": true, "this": true,
	"to": true, "was": true, "will": true, "with": true, "were": true, "through": true,
}

func englishStem(word string) string {
	w := strings.ToLower(word)
	if len(w) <= 3 {
		return w
	}
	switch {
	case strings.HasSuffix(w, "ies") && len(w) > 4:
		return w[:len(w)-3] + "i"
	case strings.HasSuffix(w, "ing") && len(w) > 5:
		w = w[:len(w)-3]
		if len(w) > 2 && w[len(w)-1] == w[len(w)-2] && !strings.ContainsRune("aeiou", rune(w[len(w)-1])) {
			w = w[:len(w)-1]
		}
		return w
	case strings.HasSuffix(w, "ed") && len(w) > 4:
		w = w[:len(w)-2]
		if len(w) > 2 && w[len(w)-1] == w[len(w)-2] && !strings.ContainsRune("aeiou", rune(w[len(w)-1])) {
			w = w[:len(w)-1]
		}
		return w
	case strings.HasSuffix(w, "es") && len(w) > 3:
		return w[:len(w)-2]
	case strings.HasSuffix(w, "s") && !strings.HasSuffix(w, "ss") && len(w) > 3:
		return w[:len(w)-1]
	}
	return w
}

func simpleAnalyze(text string) []string {
	out := []string{}
	var buf []rune
	flush := func() {
		if len(buf) > 0 {
			out = append(out, strings.ToLower(string(buf)))
			buf = buf[:0]
		}
	}
	for _, r := range text {
		if unicode.IsLetter(r) {
			buf = append(buf, r)
		} else {
			flush()
		}
	}
	flush()
	return out
}

type analyzedToken struct {
	term string
	pos  int
}

func tokenizePositions(text string, simple bool) []analyzedToken {
	out := []analyzedToken{}
	rs := []rune(text)
	var buf []rune
	pos := 0
	flush := func() {
		if len(buf) > 0 {
			out = append(out, analyzedToken{term: strings.ToLower(string(buf)), pos: pos})
			pos++
			buf = buf[:0]
		}
	}
	isWord := func(r rune) bool {
		return unicode.IsLetter(r) || (!simple && unicode.IsDigit(r))
	}
	isHan := func(r rune) bool {
		return (r >= 0x3400 && r <= 0x4DBF) || (r >= 0x4E00 && r <= 0x9FFF) ||
			(r >= 0xF900 && r <= 0xFAFF)
	}
	for i, r := range rs {
		switch {
		case isHan(r):
			flush()
			out = append(out, analyzedToken{term: strings.ToLower(string(r)), pos: pos})
			pos++
		case isWord(r):
			buf = append(buf, r)
		case !simple && r == '.' && len(buf) > 0 && i+1 < len(rs) &&
			isWord(buf[len(buf)-1]) && isWord(rs[i+1]):
			buf = append(buf, r)
		case !simple && r == '\'' && len(buf) > 0 && i+1 < len(rs) &&
			unicode.IsLetter(buf[len(buf)-1]) && unicode.IsLetter(rs[i+1]):
			buf = append(buf, r)
		default:
			flush()
		}
	}
	flush()
	return out
}

type synonymGroup struct {
	phrases [][]string
}

func synonymGroups(idx *Index, analyzer string) []synonymGroup {
	if idx == nil || analyzer == "" {
		return nil
	}
	raw, ok := findSetting(idx.Settings, "analysis.analyzer."+analyzer+".filter")
	if !ok {
		return nil
	}
	names := []string{}
	switch t := raw.(type) {
	case []any:
		for _, e := range t {
			if s, ok := e.(string); ok {
				names = append(names, s)
			}
		}
	case string:
		v := strings.TrimSpace(t)
		if strings.HasPrefix(v, "[") && strings.HasSuffix(v, "]") {
			v = strings.TrimSpace(v[1 : len(v)-1])
		}
		for _, s := range strings.Fields(v) {
			if s != "" {
				names = append(names, s)
			}
		}
	}
	groups := []synonymGroup{}
	for _, name := range names {
		typ, _ := findSetting(idx.Settings, "analysis.filter."+name+".type")
		ts, _ := typ.(string)
		if ts != "synonym" && ts != "synonym_graph" {
			continue
		}
		syn, _ := findSetting(idx.Settings, "analysis.filter."+name+".synonyms")
		lines := []string{}
		switch t := syn.(type) {
		case []any:
			for _, e := range t {
				if s, ok := e.(string); ok {
					lines = append(lines, s)
				}
			}
		case string:
			v := strings.TrimSpace(t)
			if strings.HasPrefix(v, "[") && strings.HasSuffix(v, "]") {
				v = strings.TrimSpace(v[1 : len(v)-1])
			}
			lines = append(lines, v)
		}
		for _, line := range lines {
			line = strings.TrimSpace(line)
			if line == "" || strings.HasPrefix(line, "#") {
				continue
			}
			parts := strings.Split(line, ",")
			phrases := [][]string{}
			for _, p := range parts {
				terms := strings.Fields(strings.ToLower(strings.TrimSpace(p)))
				if len(terms) > 0 {
					phrases = append(phrases, terms)
				}
			}
			if len(phrases) > 0 {
				groups = append(groups, synonymGroup{phrases: phrases})
			}
		}
	}
	return groups
}

func synonymAlternatives(idx *Index, analyzer, text string) ([][]string, bool) {
	groups := synonymGroups(idx, analyzer)
	if len(groups) == 0 {
		return nil, false
	}
	base := tokenizePositions(text, false)
	terms := make([]string, 0, len(base))
	for _, t := range base {
		terms = append(terms, t.term)
	}
	alts := [][]string{terms}
	matched := false
	for _, g := range groups {
		for _, phrase := range g.phrases {
			for i := 0; i+len(phrase) <= len(terms); i++ {
				ok := true
				for j, p := range phrase {
					if terms[i+j] != p {
						ok = false
						break
					}
				}
				if !ok {
					continue
				}
				for _, alt := range g.phrases {
					if len(alt) == len(phrase) {
						same := true
						for j := range alt {
							if alt[j] != phrase[j] {
								same = false
								break
							}
						}
						if same {
							continue
						}
					}
					alts = append(alts, alt)
					matched = true
				}
			}
		}
	}
	return alts, matched
}

func analyzeWithSynonyms(idx *Index, analyzer, text string) ([]analyzedToken, bool) {
	groups := synonymGroups(idx, analyzer)
	if len(groups) == 0 {
		return nil, false
	}
	base := tokenizePositions(text, false)
	out := append([]analyzedToken{}, base...)
	for _, g := range groups {
		for _, phrase := range g.phrases {
			for i := 0; i+len(phrase) <= len(base); i++ {
				ok := true
				for j, p := range phrase {
					if base[i+j].term != p {
						ok = false
						break
					}
				}
				if !ok {
					continue
				}
				for _, alt := range g.phrases {
					for j, term := range alt {
						out = append(out, analyzedToken{term: term, pos: base[i].pos + j})
					}
				}
			}
		}
	}
	sort.SliceStable(out, func(i, j int) bool {
		if out[i].pos != out[j].pos {
			return out[i].pos < out[j].pos
		}
		return out[i].term < out[j].term
	})
	dedup := out[:0]
	lastTerm, lastPos := "", -1
	for _, t := range out {
		if t.term == lastTerm && t.pos == lastPos {
			continue
		}
		dedup = append(dedup, t)
		lastTerm, lastPos = t.term, t.pos
	}
	return dedup, true
}

var asciiFoldReplacer = strings.NewReplacer(
	"ß", "ss", "ẞ", "SS", "æ", "ae", "Æ", "AE", "œ", "oe", "Œ", "OE",
	"ø", "o", "Ø", "O", "ł", "l", "Ł", "L", "đ", "d", "Đ", "D", "þ", "th", "Þ", "TH",
	"à", "a", "á", "a", "â", "a", "ã", "a", "ä", "a", "å", "a",
	"À", "A", "Á", "A", "Â", "A", "Ã", "A", "Ä", "A", "Å", "A",
	"ç", "c", "Ç", "C", "è", "e", "é", "e", "ê", "e", "ë", "e",
	"È", "E", "É", "E", "Ê", "E", "Ë", "E", "ì", "i", "í", "i", "î", "i", "ï", "i",
	"Ì", "I", "Í", "I", "Î", "I", "Ï", "I", "ñ", "n", "Ñ", "N",
	"ò", "o", "ó", "o", "ô", "o", "õ", "o", "ö", "o",
	"Ò", "O", "Ó", "O", "Ô", "O", "Õ", "O", "Ö", "O",
	"ù", "u", "ú", "u", "û", "u", "ü", "u",
	"Ù", "U", "Ú", "U", "Û", "U", "Ü", "U", "ý", "y", "ÿ", "y", "Ý", "Y",
)

func filterNames(raw any) []string {
	out := []string{}
	switch t := raw.(type) {
	case []any:
		for _, e := range t {
			if s, ok := e.(string); ok {
				out = append(out, s)
			}
		}
	case string:
		v := strings.TrimSpace(t)
		if strings.HasPrefix(v, "[") && strings.HasSuffix(v, "]") {
			v = strings.TrimSpace(v[1 : len(v)-1])
		}
		out = append(out, strings.Fields(v)...)
	}
	return out
}

func analyzeCustomWithIndex(idx *Index, analyzer, text string) ([]analyzedToken, bool) {
	if idx == nil || analyzer == "" {
		return nil, false
	}
	rawTok, ok := findSetting(idx.Settings, "analysis.analyzer."+analyzer+".tokenizer")
	if !ok {
		return nil, false
	}
	tok, _ := rawTok.(string)
	var toks []analyzedToken
	switch tok {
	case "keyword":
		toks = []analyzedToken{{term: text, pos: 0}}
	case "whitespace":
		for i, t := range strings.Fields(text) {
			toks = append(toks, analyzedToken{term: t, pos: i})
		}
	case "lowercase":
		toks = []analyzedToken{{term: strings.ToLower(text), pos: 0}}
	default:
		toks = tokenizePositions(text, false)
	}
	rawFilters, _ := findSetting(idx.Settings, "analysis.analyzer."+analyzer+".filter")
	for _, f := range filterNames(rawFilters) {
		switch f {
		case "lowercase":
			for i := range toks {
				toks[i].term = strings.ToLower(toks[i].term)
			}
		case "asciifolding":
			for i := range toks {
				toks[i].term = asciiFoldReplacer.Replace(toks[i].term)
			}
		case "stop":
			out := toks[:0]
			for _, t := range toks {
				if !englishStopWords[t.term] {
					out = append(out, t)
				}
			}
			toks = out
		case "english":
			for i := range toks {
				toks[i].term = englishStem(toks[i].term)
			}
		}
	}
	return toks, true
}

func analyzeTokensWithIndex(idx *Index, analyzer, text string) []analyzedToken {
	if toks, ok := analyzeWithSynonyms(idx, analyzer, text); ok {
		return toks
	}
	if toks, ok := analyzeCustomWithIndex(idx, analyzer, text); ok {
		return toks
	}
	switch analyzer {
	case "keyword":
		return []analyzedToken{{term: text, pos: 0}}
	case "whitespace":
		out := []analyzedToken{}
		for i, t := range strings.Fields(text) {
			out = append(out, analyzedToken{term: t, pos: i})
		}
		return out
	case "lowercase":
		return []analyzedToken{{term: strings.ToLower(text), pos: 0}}
	case "simple":
		return tokenizePositions(text, true)
	case "stop":
		out := []analyzedToken{}
		for _, t := range tokenizePositions(text, true) {
			if !englishStopWords[t.term] {
				out = append(out, t)
			}
		}
		return out
	case "english":
		out := []analyzedToken{}
		for _, t := range tokenizePositions(text, false) {
			if englishStopWords[t.term] {
				continue
			}
			out = append(out, analyzedToken{term: englishStem(t.term), pos: t.pos})
		}
		return out
	}
	return tokenizePositions(text, false)
}

func analyzeWithIndex(idx *Index, analyzer, text string) []string {
	if toks, ok := analyzeWithSynonyms(idx, analyzer, text); ok {
		out := make([]string, 0, len(toks))
		for _, t := range toks {
			out = append(out, t.term)
		}
		return out
	}
	if toks, ok := analyzeCustomWithIndex(idx, analyzer, text); ok {
		out := make([]string, 0, len(toks))
		for _, t := range toks {
			out = append(out, t.term)
		}
		return out
	}
	switch analyzer {
	case "simple":
		return simpleAnalyze(text)
	case "stop":
		toks := simpleAnalyze(text)
		out := make([]string, 0, len(toks))
		for _, t := range toks {
			if !englishStopWords[t] {
				out = append(out, t)
			}
		}
		return out
	case "english":
		toks := analyze(text)
		out := make([]string, 0, len(toks))
		for _, t := range toks {
			if englishStopWords[t] {
				continue
			}
			out = append(out, englishStem(t))
		}
		return out
	default:
		if analyzer == "" || analyzer == "standard" {
			out := []string{}
			for _, t := range tokenizePositions(text, false) {
				out = append(out, t.term)
			}
			return out
		}
		return analyzeWith(analyzer, text)
	}
}

func (s *Server) handleAnalyze(w http.ResponseWriter, r *http.Request, name string) {
	var idx *Index
	if name != "" {
		s.mu.RLock()
		var apiErr *apiError
		idx, apiErr = s.getIndex(name)
		s.mu.RUnlock()
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	text, _ := body["text"].(string)
	analyzer, _ := body["analyzer"].(string)
	if analyzer == "" {
		analyzer, _ = body["tokenizer"].(string)
	}
	if analyzer == "" {
		if field, ok := body["field"].(string); ok && idx != nil {
			if def, ok := mappingFieldDef(idx, field); ok {
				if a, ok := def["search_analyzer"].(string); ok && a != "" {
					analyzer = a
				} else if a, ok := def["analyzer"].(string); ok && a != "" {
					analyzer = a
				}
			}
		}
	}
	var toks []analyzedToken
	if idx != nil {
		toks = analyzeTokensWithIndex(idx, analyzer, text)
	} else {
		for i, t := range analyzeWith(analyzer, text) {
			toks = append(toks, analyzedToken{term: t, pos: i})
		}
	}
	out := make([]any, 0, len(toks))
	for _, t := range toks {
		out = append(out, map[string]any{
			"token": t.term, "position": t.pos, "type": "<ALPHANUM>",
		})
	}
	writeJSON(w, 200, map[string]any{"tokens": out})
}
