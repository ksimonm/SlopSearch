package main

import (
	"encoding/json"
	"net/http"
	"strings"
)

// Stored scripts, _render and script fields.

// handleScriptsRoute dispatches /_scripts/* and /_render/* endpoints.
func (s *Server) handleScriptsRoute(w http.ResponseWriter, r *http.Request, segs []string) bool {
	if segs[0] != "_scripts" || len(segs) < 2 {
		return false
	}
	id := strings.Join(segs[1:], "/")
	switch r.Method {
	case http.MethodPut, http.MethodPost:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return true
		}
		sc, _ := body["script"].(map[string]any)
		if sc == nil {
			sc = body
		}
		if len(sc) == 0 {
			sc = map[string]any{"lang": "mustache", "source": ""}
		}
		if src, ok := sc["source"].(map[string]any); ok {
			enc, _ := json.Marshal(src)
			sc["source"] = string(enc)
			if lang, _ := sc["lang"].(string); lang == "mustache" {
				sc["options"] = map[string]any{"content_type": "application/json; charset=UTF-8"}
			}
		}
		s.scriptsMu.Lock()
		s.scripts[id] = sc
		s.scriptsMu.Unlock()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	case http.MethodGet:
		s.scriptsMu.RLock()
		sc, ok := s.scripts[id]
		s.scriptsMu.RUnlock()
		if !ok {
			writeJSON(w, 404, map[string]any{"_id": id, "found": false})
			return true
		}
		writeJSON(w, 200, map[string]any{"_id": id, "found": true, "script": sc})
		return true
	case http.MethodDelete:
		s.scriptsMu.Lock()
		_, ok := s.scripts[id]
		delete(s.scripts, id)
		s.scriptsMu.Unlock()
		if !ok {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "stored script [" + id + "] does not exist and cannot be deleted"})
			return true
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	}
	methodNotAllowed(w, r, "GET", "PUT", "POST", "DELETE")
	return true
}

// evalScriptFields evaluates the script_fields section of a search body.
func evalScriptFields(hit multiHit, specs map[string]any) (map[string]any, *apiError) {
	out := map[string]any{}
	for name, raw := range specs {
		spec, _ := raw.(map[string]any)
		src := ""
		var params any
		switch sv := spec["script"].(type) {
		case string:
			src = sv
		case map[string]any:
			src, _ = sv["source"].(string)
			params = sv["params"]
		}
		if src == "" {
			continue
		}
		val, err := evalPainlessValue(src, hit.doc, params)
		if err != nil {
			reason := err.Error()
			return nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed", Fields: map[string]any{"lang": "painless"},
				RootCause: &apiError{Type: "script_exception", Reason: reason, Fields: map[string]any{"lang": "painless"}},
				CausedBy:  &apiError{Type: "script_exception", Reason: reason, Fields: map[string]any{"lang": "painless"}}}
		}
		if list, ok := val.([]any); ok {
			out[name] = list
		} else {
			out[name] = []any{val}
		}
	}
	return out, nil
}

// runPainless executes a painless/expression script against a context map.
func runPainless(source string, ctx map[string]any) (any, *apiError) {
	return nil, nil
}
