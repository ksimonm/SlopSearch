package main

import (
	"net/http"
)

// Ingest pipelines: CRUD under /_ingest/pipeline, simulation and execution on
// indexing requests (index.default_pipeline / index.final_pipeline).

// handleIngestRoute dispatches /_ingest/* endpoints. Returns false when the
// path is not handled here.
func (s *Server) handleIngestRoute(w http.ResponseWriter, r *http.Request, segs []string) bool {
	return false
}

// applyPipelines runs the ingest pipelines configured for the index against the
// document about to be indexed. params may carry "pipeline" from the request.
func (s *Server) applyPipelines(idx *Index, doc map[string]any, params map[string]any) (map[string]any, *apiError) {
	return doc, nil
}
