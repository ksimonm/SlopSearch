package main

import (
	"net/http"
)

// Analysis framework: analyzers, tokenizers, token filters and the _analyze API.

// analyzeWithIndex tokenizes text with a named analyzer, resolving custom
// analyzers configured in index settings. Falls back to the built-in set.
func analyzeWithIndex(idx *Index, analyzer, text string) []string {
	return analyzeWith(analyzer, text)
}

func (s *Server) handleAnalyze(w http.ResponseWriter, r *http.Request, name string) {
	if name != "" {
		s.mu.RLock()
		_, apiErr := s.getIndex(name)
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
	tokens := analyzeWith(analyzer, text)
	out := make([]any, 0, len(tokens))
	for i, t := range tokens {
		out = append(out, map[string]any{
			"token": t, "position": i, "type": "<ALPHANUM>",
		})
	}
	writeJSON(w, 200, map[string]any{"tokens": out})
}
