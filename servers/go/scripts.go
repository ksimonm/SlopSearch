package main

import (
	"net/http"
)

// Stored scripts, _render and script fields.

// handleScriptsRoute dispatches /_scripts/* and /_render/* endpoints.
func (s *Server) handleScriptsRoute(w http.ResponseWriter, r *http.Request, segs []string) bool {
	return false
}

// evalScriptFields evaluates the script_fields section of a search body.
func evalScriptFields(hit multiHit, specs map[string]any) (map[string]any, *apiError) {
	return nil, nil
}

// runPainless executes a painless/expression script against a context map.
func runPainless(source string, ctx map[string]any) (any, *apiError) {
	return nil, nil
}
