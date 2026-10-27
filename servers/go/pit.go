package main

import (
	"fmt"
	"net/http"
	"strings"
	"time"
)

type pitContext struct {
	idx       *Index
	hits      []multiHit
	shards    int
	keepAlive int64
	creation  int64
	indexName string
}

func (s *Server) openPit(w http.ResponseWriter, r *http.Request, name string) {
	keep := r.URL.Query().Get("keep_alive")
	if keep == "" {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: keep alive not specified;"})
		return
	}
	d, ok := parseKeepAlive(keep)
	if !ok {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse [" + keep + "]"})
		return
	}
	if d > 24*time.Hour {
		reason := fmt.Sprintf("Keep alive for request (%s) is too large. It must be less than (1d). [point_in_time.max_keep_alive]", keep)
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: reason})
		return
	}
	s.mu.RLock()
	idx, ok := s.indices[name]
	if !ok {
		s.mu.RUnlock()
		writeError(w, errNotFound(name))
		return
	}
	hits := make([]multiHit, 0, len(idx.Order))
	for _, id := range idx.Order {
		doc, live := idx.Docs[id]
		if !live {
			continue
		}
		if _, pending := idx.Pending[id]; pending {
			continue
		}
		hits = append(hits, multiHit{idx: idx, id: id, doc: doc, score: 1})
	}
	shards := idx.numShards()
	creation := time.Now().UnixMilli()
	s.mu.RUnlock()

	s.pitMu.Lock()
	s.pitCounter++
	pid := fmt.Sprintf("slopensearch-pit-%d", s.pitCounter)
	s.pits[pid] = &pitContext{idx: idx, hits: hits, shards: shards,
		keepAlive: int64(d / time.Millisecond), creation: creation, indexName: name}
	s.pitMu.Unlock()

	writeJSON(w, 200, map[string]any{
		"pit_id":        pid,
		"_shards":       map[string]any{"total": shards, "successful": shards, "skipped": 0, "failed": 0},
		"creation_time": creation,
	})
}

func (s *Server) getPit(id string) (*pitContext, bool) {
	s.pitMu.Lock()
	defer s.pitMu.Unlock()
	p, ok := s.pits[id]
	return p, ok
}

func (s *Server) handlePitRoute(w http.ResponseWriter, r *http.Request, rest []string) {
	switch r.Method {
	case http.MethodGet:
		s.pitMu.Lock()
		pits := make([]any, 0, len(s.pits))
		for id, p := range s.pits {
			pits = append(pits, map[string]any{
				"pit_id":        id,
				"keep_alive":    p.keepAlive,
				"creation_time": p.creation,
			})
		}
		s.pitMu.Unlock()
		writeJSON(w, 200, map[string]any{"pits": pits})
	case http.MethodDelete:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		var ids []string
		switch t := body["pit_id"].(type) {
		case string:
			ids = append(ids, t)
		case []any:
			for _, v := range t {
				if sv, ok := v.(string); ok {
					ids = append(ids, sv)
				}
			}
		}
		if len(ids) == 0 {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: no pit ids specified;"})
			return
		}
		for _, id := range ids {
			if !strings.HasPrefix(id, "slopensearch-pit-") {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "invalid id: [" + id + "]"})
				return
			}
		}
		out := make([]any, 0, len(ids))
		s.pitMu.Lock()
		for _, id := range ids {
			delete(s.pits, id)
			out = append(out, map[string]any{"successful": true, "pit_id": id})
		}
		s.pitMu.Unlock()
		writeJSON(w, 200, map[string]any{"pits": out})
	default:
		methodNotAllowed(w, r, "GET", "DELETE")
	}
}
