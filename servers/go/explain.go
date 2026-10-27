package main

import (
	"fmt"
	"math"
	"net/http"
)

func (c *queryCtx) bm25Explain(field, term string, score float64) map[string]any {
	desc := fmt.Sprintf("weight(%s:%s in 0) [PerFieldSimilarity], result of:", field, term)
	generic := map[string]any{
		"value":       score,
		"description": desc,
		"details": []any{map[string]any{
			"value":       score,
			"description": "sum of:",
			"details":     []any{},
		}},
	}
	if c.idx == nil || field == "" || term == "" {
		return generic
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	if ftype != "" && ftype != "text" && ftype != "match_only_text" && ftype != "search_as_you_type" {
		return generic
	}
	analyzer := c.analyzerName(field, nil)
	docTerms := func(vals []any) []string {
		out := []string{}
		for _, v := range vals {
			s, _ := valueScalarString(v)
			for _, t := range analyzeTokensWithIndex(c.idx, analyzer, s) {
				out = append(out, t.term)
			}
		}
		return out
	}
	myVals := []any{}
	if dv, ok := c.rawValues(); ok {
		myVals = dv[resolveAliasField(c.idx, field)]
	}
	myTokens := docTerms(myVals)
	freq := 0
	for _, t := range myTokens {
		if t == term {
			freq++
		}
	}
	N := 0
	avgdl := 0.0
	df := 0
	for _, dv := range c.idx.DocValues {
		vals := dv[resolveAliasField(c.idx, field)]
		if len(vals) == 0 {
			continue
		}
		N++
		toks := docTerms(vals)
		avgdl += float64(len(toks))
		seen := false
		for _, t := range toks {
			if t == term {
				seen = true
				break
			}
		}
		if seen {
			df++
		}
	}
	if N == 0 || freq == 0 {
		return generic
	}
	avgdl /= float64(N)
	if avgdl == 0 {
		avgdl = 1
	}
	const k1 = 1.2
	const b = 0.75
	dl := float64(len(myTokens))
	idf := math.Log(1 + (float64(N)-float64(df)+0.5)/(float64(df)+0.5))
	tf := float64(freq) / (float64(freq) + k1*(1-b+b*dl/avgdl))
	inner := map[string]any{
		"value":       score,
		"description": fmt.Sprintf("score(freq=%.1f), computed as boost * idf * tf from:", float64(freq)),
		"details": []any{
			map[string]any{
				"value":       idf,
				"description": "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:",
				"details": []any{
					map[string]any{"value": float64(df), "description": "n, number of documents containing term", "details": []any{}},
					map[string]any{"value": float64(N), "description": "N, total number of documents with field", "details": []any{}},
				},
			},
			map[string]any{
				"value":       tf,
				"description": "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
				"details": []any{
					map[string]any{"value": k1, "description": "k1, term saturation parameter", "details": []any{}},
					map[string]any{"value": b, "description": "b, length normalization parameter", "details": []any{}},
					map[string]any{"value": dl, "description": "dl, length of field", "details": []any{}},
					map[string]any{"value": avgdl, "description": "avgdl, average length of field", "details": []any{}},
				},
			},
		},
	}
	return map[string]any{"value": score, "description": desc, "details": []any{inner}}
}

func (s *Server) handleExplain(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	doc, found := idx.Docs[id]
	if !found {
		writeJSON(w, 404, map[string]any{"_index": idx.Name, "_id": id, "matched": false})
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	query, hasQuery := body["query"]
	if qp := r.URL.Query().Get("q"); qp != "" {
		query = parseQParam(qp)
		hasQuery = true
	}
	if !hasQuery {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: query is missing;"})
		return
	}
	qmap, ok := query.(map[string]any)
	if !ok {
		writeError(w, errParsing("[query] malformed"))
		return
	}
	qc := newQueryCtxForDoc(idx, id, doc)
	matched, score, qerr := qc.eval(qmap)
	if qerr != nil {
		writeError(w, qerr)
		return
	}
	field, term := explainLabel(qmap)
	var explanation map[string]any
	if matched {
		explanation = qc.bm25Explain(field, term, score)
	} else {
		explanation = map[string]any{
			"value":       0.0,
			"description": "no matching term",
			"details":     []any{},
		}
	}
	out := map[string]any{"_index": idx.Name, "_id": id, "matched": matched, "explanation": explanation}
	if sourceRequested(r, body) {
		out["get"] = map[string]any{"_seq_no": idx.SeqNos[id], "_primary_term": int64(1), "found": true,
			"_source": filterSourceDoc(doc, nil, nil)}
	}
	writeJSON(w, 200, out)
}

func explainLabel(query map[string]any) (string, string) {
	for clause, raw := range query {
		switch clause {
		case "match", "term":
			m, _ := raw.(map[string]any)
			for f, v := range m {
				if vm, ok := v.(map[string]any); ok {
					if val, ok := vm["query"]; ok {
						s, _ := valueToScriptString(val)
						return f, s
					}
					if val, ok := vm["value"]; ok {
						s, _ := valueToScriptString(val)
						return f, s
					}
				}
				s, _ := valueToScriptString(v)
				return f, s
			}
		}
	}
	return "_id", ""
}

func sourceRequested(r *http.Request, body map[string]any) bool {
	if v, ok := body["_source"]; ok {
		if b, ok := v.(bool); ok {
			return b
		}
		return true
	}
	if v, ok := r.URL.Query()["_source"]; ok {
		return len(v) == 0 || v[0] != "false"
	}
	return false
}
