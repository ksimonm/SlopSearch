package main

import (
	"net/http"
	"strings"
)

func (s *Server) handleValidateQuery(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	explain := r.URL.Query().Get("explain") == "true" || r.URL.Query().Get("rewrite") == "true"
	allShards := r.URL.Query().Get("all_shards") == "true"
	query, _ := body["query"].(map[string]any)
	if vals, has := r.URL.Query()["q"]; has {
		var qerr *apiError
		query, qerr = parseQParamOpts(vals[0], r.URL.Query().Get("df"), r.URL.Query().Get("default_operator"))
		if qerr != nil {
			if explain {
				writeJSON(w, 200, map[string]any{"valid": false, "explanations": []any{map[string]any{
					"index": name, "valid": false, "error": qerr.Reason}}})
				return
			}
			writeJSON(w, 200, map[string]any{"valid": false})
			return
		}
	}
	if query == nil {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: query cannot be null;"})
		return
	}
	for k := range body {
		if k != "query" {
			writeJSON(w, 200, map[string]any{"valid": false})
			return
		}
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{closedError: true})
	indices, apiErr := s.resolveSpec2(name, o)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	idx := (*Index)(nil)
	for _, cand := range indices {
		idx = cand
		break
	}
	if idx == nil {
		writeJSON(w, 200, map[string]any{"valid": true, "_shards": map[string]any{"total": 0, "successful": 0, "failed": 0}})
		return
	}
	id := ""
	var doc map[string]any
	for _, did := range idx.Order {
		if d, live := idx.Docs[did]; live {
			id, doc = did, d
			break
		}
	}
	if doc == nil {
		doc = map[string]any{}
	}
	qc := newQueryCtxForDoc(idx, id, doc)
	_, _, qerr := qc.eval(query)
	if qerr != nil {
		reason := qerr.Reason
		if qerr.RootCause != nil && qerr.RootCause.Reason != "" {
			reason = qerr.RootCause.Reason
		}
		errText := reason
		switch qerr.Type {
		case "parsing_exception":
			errText = "ParsingException[" + reason + "]"
		case "illegal_argument_exception":
			if strings.Contains(reason, "failed to parse value") {
				val := reason
				if i := strings.Index(val, "["); i >= 0 {
					if j := strings.Index(val[i+1:], "]"); j >= 0 {
						val = val[i+1 : i+1+j]
					}
				}
				errText = "NumberFormatException[For input string: \"" + val + "\"]"
			}
		}
		if explain {
			exp := map[string]any{"index": idx.Name, "valid": false, "error": errText}
			if allShards {
				exp["shard"] = 0
			}
			writeJSON(w, 200, map[string]any{"valid": false, "error": errText, "explanations": []any{exp}})
			return
		}
		if qerr.Type == "parsing_exception" {
			writeJSON(w, 200, map[string]any{"valid": false})
		} else {
			writeJSON(w, 200, map[string]any{"valid": false, "_shards": map[string]any{
				"total": idx.numShards(), "successful": idx.numShards(), "failed": 0}})
		}
		return
	}
	shards := map[string]any{"total": idx.numShards(), "successful": idx.numShards(), "failed": 0}
	if explain {
		exp := map[string]any{"index": idx.Name, "valid": true, "explanation": validateExplanationWithCtx(query, qc)}
		if allShards {
			exp["shard"] = 0
		}
		writeJSON(w, 200, map[string]any{"_shards": shards, "valid": true, "explanations": []any{exp}})
		return
	}
	writeJSON(w, 200, map[string]any{"_shards": shards, "valid": true})
}

func validateExplanationWithCtx(q map[string]any, qc *queryCtx) string {
	if qs, ok := q["query_string"].(map[string]any); ok {
		query, _ := toString(qs["query"])
		operator := "OR"
		if o, ok := qs["default_operator"].(string); ok {
			operator = strings.ToUpper(o)
		}
		defaultField := "*"
		if df, ok := qs["default_field"].(string); ok && df != "" {
			defaultField = df
		}
		p := &qsParser{tokens: tokenizeQueryString(query), default_: defaultField, operator: operator, c: qc}
		if parsed, err := p.parse(); err == nil {
			return explainParsed(parsed)
		}
	}
	return validateExplanation(q)
}

func explainParsed(q map[string]any) string {
	if bm, ok := q["bool"].(map[string]any); ok {
		parts := []string{}
		for _, item := range asQueryList(bm["must"]) {
			if cm, ok := item.(map[string]any); ok {
				if _, isTerm := cm["term"]; isTerm {
					parts = append(parts, "+ConstantScore("+explainParsed(cm)+")")
				} else {
					parts = append(parts, "+"+explainParsed(cm))
				}
			}
		}
		for _, item := range asQueryList(bm["filter"]) {
			if cm, ok := item.(map[string]any); ok {
				parts = append(parts, "+ConstantScore("+explainParsed(cm)+")")
			}
		}
		return strings.Join(parts, " ")
	}
	if m, ok := q["match"].(map[string]any); ok {
		for field, raw := range m {
			s, _ := toString(raw)
			return field + ":" + s
		}
	}
	return validateExplanation(q)
}

func validateExplanation(q map[string]any) string {
	if m, ok := q["match"].(map[string]any); ok {
		parts := []string{}
		for field, raw := range m {
			text, _ := queryText(raw)
			for _, tok := range analyze(text) {
				parts = append(parts, field+":"+tok)
			}
		}
		return strings.Join(parts, " ")
	}
	if bm, ok := q["bool"].(map[string]any); ok {
		parts := []string{}
		for _, item := range asQueryList(bm["must"]) {
			if cm, ok := item.(map[string]any); ok {
				parts = append(parts, "+"+validateExplanation(cm))
			}
		}
		for _, item := range asQueryList(bm["filter"]) {
			if cm, ok := item.(map[string]any); ok {
				parts = append(parts, "+ConstantScore("+validateExplanation(cm)+")")
			}
		}
		return strings.Join(parts, " ")
	}
	if m, ok := q["term"].(map[string]any); ok {
		for field, raw := range m {
			val := raw
			if vm, ok := raw.(map[string]any); ok {
				val = vm["value"]
			}
			s, _ := toString(val)
			return field + ":" + s
		}
	}
	if m, ok := q["fuzzy"].(map[string]any); ok {
		for field, raw := range m {
			val := raw
			if vm, ok := raw.(map[string]any); ok {
				val = vm["value"]
			}
			s, _ := toString(val)
			return field + ":" + s
		}
	}
	if _, ok := q["match_all"]; ok {
		return "match_all"
	}
	return "match_all"
}
