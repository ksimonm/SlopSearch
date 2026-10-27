package main

import (
	"net/http"
	"sort"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

type tvToken struct {
	term       string
	position   int
	start, end int
}

func tvTokens(text, analyzer string) []tvToken {
	if analyzer == "keyword" {
		return []tvToken{{term: text, position: 0, start: 0, end: len(text)}}
	}
	out := []tvToken{}
	pos := 0
	start := -1
	for i, r := range text {
		if isCJK(r) {
			if start >= 0 {
				out = append(out, tvToken{term: strings.ToLower(text[start:i]), position: pos, start: start, end: i})
				pos++
				start = -1
			}
			out = append(out, tvToken{term: strings.ToLower(string(r)), position: pos, start: i, end: i + utf8.RuneLen(r)})
			pos++
			continue
		}
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			if start < 0 {
				start = i
			}
			continue
		}
		if start >= 0 {
			out = append(out, tvToken{term: strings.ToLower(text[start:i]), position: pos, start: start, end: i})
			pos++
			start = -1
		}
	}
	if start >= 0 {
		out = append(out, tvToken{term: strings.ToLower(text[start:]), position: pos, start: start, end: len(text)})
	}
	return out
}

func fieldAnalyzer(idx *Index, field string, override string) string {
	if override != "" {
		return override
	}
	def, ok := mappingFieldDef(idx, field)
	if !ok {
		return "standard"
	}
	if t, _ := def["type"].(string); t == "keyword" {
		return "keyword"
	}
	if a, _ := def["analyzer"].(string); a != "" {
		return a
	}
	return "standard"
}

func tvFieldStats(idx *Index, field string) map[string]any {
	docCount, sumDocFreq, sumTTF := 0, 0, 0
	analyzer := fieldAnalyzer(idx, field, "")
	for _, id := range idx.Order {
		doc, live := idx.Docs[id]
		if !live {
			continue
		}
		v, ok := fieldValue(doc, field)
		if !ok {
			continue
		}
		docCount++
		seen := map[string]bool{}
		for _, s := range docStrings(v) {
			toks := tvTokens(s, analyzer)
			sumTTF += len(toks)
			for _, t := range toks {
				seen[t.term] = true
			}
		}
		sumDocFreq += len(seen)
	}
	return map[string]any{"sum_doc_freq": sumDocFreq, "doc_count": docCount, "sum_ttf": sumTTF}
}

func tvIndexTermStats(idx *Index, field string) map[string]map[string]int {
	out := map[string]map[string]int{}
	analyzer := fieldAnalyzer(idx, field, "")
	for _, id := range idx.Order {
		doc, live := idx.Docs[id]
		if !live {
			continue
		}
		v, ok := fieldValue(doc, field)
		if !ok {
			continue
		}
		seen := map[string]bool{}
		for _, s := range docStrings(v) {
			for _, t := range tvTokens(s, analyzer) {
				if out[t.term] == nil {
					out[t.term] = map[string]int{"doc_freq": 0, "ttf": 0}
				}
				out[t.term]["ttf"]++
				seen[t.term] = true
			}
		}
		for term := range seen {
			out[term]["doc_freq"]++
		}
	}
	return out
}

func termVectorFor(idx *Index, field string, doc map[string]any, analyzer string, termStats, fieldStats, positions, offsets bool, filter map[string]any) map[string]any {
	v, ok := fieldValue(doc, field)
	if !ok {
		return nil
	}
	analyzer = fieldAnalyzer(idx, field, analyzer)
	stats := map[string]map[string]int(nil)
	if termStats {
		stats = tvIndexTermStats(idx, field)
	}
	type agg struct {
		freq   int
		tokens []tvToken
	}
	byTerm := map[string]*agg{}
	for _, s := range docStrings(v) {
		for _, t := range tvTokens(s, analyzer) {
			a := byTerm[t.term]
			if a == nil {
				a = &agg{}
				byTerm[t.term] = a
			}
			a.freq++
			a.tokens = append(a.tokens, t)
		}
	}
	terms := make([]string, 0, len(byTerm))
	for t := range byTerm {
		terms = append(terms, t)
	}
	sort.Strings(terms)
	maxNum := 0
	minFreq := 0
	minDocFreq := 0
	if filter != nil {
		if n, ok := integerOf(filter["max_num_terms"]); ok {
			maxNum = int(n)
		}
		if n, ok := integerOf(filter["min_term_freq"]); ok {
			minFreq = int(n)
		}
		if n, ok := integerOf(filter["min_doc_freq"]); ok {
			minDocFreq = int(n)
		}
	}
	termsOut := map[string]any{}
	count := 0
	for _, term := range terms {
		a := byTerm[term]
		if minFreq > 0 && a.freq < minFreq {
			continue
		}
		if minDocFreq > 0 {
			df := 0
			if stats != nil && stats[term] != nil {
				df = stats[term]["doc_freq"]
			}
			if df < minDocFreq {
				continue
			}
		}
		obj := map[string]any{"term_freq": a.freq}
		if stats != nil {
			if st, ok := stats[term]; ok {
				obj["doc_freq"] = st["doc_freq"]
				obj["ttf"] = st["ttf"]
			}
		}
		if positions || offsets {
			toks := make([]any, 0, len(a.tokens))
			for _, t := range a.tokens {
				tok := map[string]any{}
				if positions {
					tok["position"] = t.position
				}
				if offsets {
					tok["start_offset"] = t.start
					tok["end_offset"] = t.end
				}
				toks = append(toks, tok)
			}
			obj["tokens"] = toks
		}
		termsOut[term] = obj
		count++
		if maxNum > 0 && count >= maxNum {
			break
		}
	}
	tv := map[string]any{"terms": termsOut}
	if fieldStats {
		tv["field_statistics"] = tvFieldStats(idx, field)
	}
	return tv
}

func tvBoolParam(r *http.Request, body map[string]any, key string, def bool) bool {
	if v, ok := body[key]; ok {
		if b, ok := v.(bool); ok {
			return b
		}
	}
	if v, ok := r.URL.Query()[key]; ok {
		if len(v) == 0 || v[0] == "" || v[0] == "true" {
			return true
		}
		if v[0] == "false" {
			return false
		}
	}
	return def
}

func tvFields(r *http.Request, body map[string]any, idx *Index, doc map[string]any) []string {
	var fields []string
	if v, ok := body["fields"]; ok {
		switch t := v.(type) {
		case []any:
			for _, e := range t {
				if s, ok := e.(string); ok {
					fields = append(fields, s)
				}
			}
		case string:
			fields = append(fields, t)
		}
	}
	if len(fields) == 0 {
		if v := r.URL.Query().Get("fields"); v != "" {
			fields = strings.Split(v, ",")
		}
	}
	if len(fields) == 0 && doc != nil {
		for k := range doc {
			if !strings.HasPrefix(k, "_") {
				fields = append(fields, k)
			}
		}
		sort.Strings(fields)
	}
	return fields
}

func (s *Server) termVectorResponse(r *http.Request, idx *Index, id string, doc map[string]any, artificial bool) map[string]any {
	body, _ := readJSON(r)
	if body == nil {
		body = map[string]any{}
	}
	termStats := tvBoolParam(r, body, "term_statistics", false)
	fieldStats := tvBoolParam(r, body, "field_statistics", true)
	positions := tvBoolParam(r, body, "positions", true)
	offsets := tvBoolParam(r, body, "offsets", true)
	filter, _ := body["filter"].(map[string]any)
	analyzers, _ := body["per_field_analyzer"].(map[string]any)
	out := map[string]any{"_index": idx.Name, "found": true, "took": int64(0)}
	if artificial {
		out["_version"] = int64(0)
	} else {
		out["_id"] = id
		out["_version"] = currentVersion(idx, id)
	}
	tvs := map[string]any{}
	for _, f := range tvFields(r, body, idx, doc) {
		override := ""
		if analyzers != nil {
			override, _ = analyzers[f].(string)
		}
		if tv := termVectorFor(idx, f, doc, override, termStats, fieldStats, positions, offsets, filter); tv != nil {
			tvs[f] = tv
		}
	}
	out["term_vectors"] = tvs
	return out
}

func (s *Server) handleTermVectors(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if id != "" {
		doc, ok := idx.Docs[id]
		if !ok {
			writeJSON(w, 200, map[string]any{"_index": idx.Name, "_id": id, "found": false})
			return
		}
		body, _ := readJSON(r)
		if body == nil {
			body = map[string]any{}
		}
		start := time.Now()
		out := s.termVectorResponseWithBody(r, idx, id, doc, false, body)
		out["took"] = time.Since(start).Milliseconds()
		writeJSON(w, 200, out)
		return
	}
	body, _ := readJSON(r)
	if body == nil {
		body = map[string]any{}
	}
	doc, _ := body["doc"].(map[string]any)
	if doc == nil {
		doc = map[string]any{}
	}
	out := s.termVectorResponseWithBody(r, idx, "", doc, true, body)
	writeJSON(w, 200, out)
}

func (s *Server) termVectorResponseWithBody(r *http.Request, idx *Index, id string, doc map[string]any, artificial bool, body map[string]any) map[string]any {
	termStats := tvBoolParam(r, body, "term_statistics", false)
	fieldStats := tvBoolParam(r, body, "field_statistics", true)
	positions := tvBoolParam(r, body, "positions", true)
	offsets := tvBoolParam(r, body, "offsets", true)
	filter, _ := body["filter"].(map[string]any)
	analyzers, _ := body["per_field_analyzer"].(map[string]any)
	out := map[string]any{"_index": idx.Name, "found": true, "took": int64(0)}
	if artificial {
		out["_version"] = int64(0)
	} else {
		out["_id"] = id
		out["_version"] = currentVersion(idx, id)
	}
	tvs := map[string]any{}
	for _, f := range tvFields(r, body, idx, doc) {
		override := ""
		if analyzers != nil {
			override, _ = analyzers[f].(string)
		}
		if tv := termVectorFor(idx, f, doc, override, termStats, fieldStats, positions, offsets, filter); tv != nil {
			tvs[f] = tv
		}
	}
	out["term_vectors"] = tvs
	return out
}

func (s *Server) handleMTermVectors(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	params, _ := body["parameters"].(map[string]any)
	docs := []any{}
	if raw, ok := body["docs"].([]any); ok {
		for _, e := range raw {
			m, _ := e.(map[string]any)
			if m == nil {
				m = map[string]any{}
			}
			merged := map[string]any{}
			for k, v := range params {
				merged[k] = v
			}
			for k, v := range m {
				merged[k] = v
			}
			if d, ok := m["doc"].(map[string]any); ok {
				out := s.termVectorResponseWithBody(r, idx, "", d, true, merged)
				out["_index"] = idx.Name
				docs = append(docs, out)
				continue
			}
			id, _ := valueToScriptString(m["_id"])
			doc, found := idx.Docs[id]
			if !found {
				docs = append(docs, map[string]any{"_index": idx.Name, "_id": id, "found": false})
				continue
			}
			docs = append(docs, s.termVectorResponseWithBody(r, idx, id, doc, false, merged))
		}
	} else if raw, ok := body["ids"].([]any); ok {
		for _, e := range raw {
			id, _ := valueToScriptString(e)
			doc, found := idx.Docs[id]
			if !found {
				docs = append(docs, map[string]any{"_index": idx.Name, "_id": id, "found": false})
				continue
			}
			merged := map[string]any{}
			for k, v := range params {
				merged[k] = v
			}
			docs = append(docs, s.termVectorResponseWithBody(r, idx, id, doc, false, merged))
		}
	}
	writeJSON(w, 200, map[string]any{"docs": docs})
}
