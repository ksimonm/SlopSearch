package main

import (
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"
)

type scrollContext struct {
	hits    []multiHit
	pos     int
	size    int
	total   int
	restInt bool
}

func (s *Server) newScrollID() string {
	s.scrollMu.Lock()
	defer s.scrollMu.Unlock()
	s.scrollCounter++
	return fmt.Sprintf("slopsearch-scroll-%d", s.scrollCounter)
}

func (s *Server) storeScroll(hits []multiHit, pos, size, total int, restInt bool) string {
	sid := s.newScrollID()
	s.scrollMu.Lock()
	s.scrolls[sid] = &scrollContext{hits: hits, pos: pos, size: size, total: total, restInt: restInt}
	s.scrollMu.Unlock()
	return sid
}

func parseKeepAlive(v string) (time.Duration, bool) {
	if v == "" {
		return time.Minute, true
	}
	i := 0
	for i < len(v) && v[i] >= '0' && v[i] <= '9' {
		i++
	}
	if i == 0 {
		return 0, false
	}
	n, err := strconv.Atoi(v[:i])
	if err != nil {
		return 0, false
	}
	unit := v[i:]
	var mult time.Duration
	switch unit {
	case "ms":
		mult = time.Millisecond
	case "s":
		mult = time.Second
	case "m":
		mult = time.Minute
	case "h":
		mult = time.Hour
	case "d":
		mult = 24 * time.Hour
	case "w":
		mult = 7 * 24 * time.Hour
	default:
		return 0, false
	}
	return time.Duration(n) * mult, true
}

func validateScrollParams(r *http.Request, body map[string]any, from, size int) *apiError {
	keep := r.URL.Query().Get("scroll")
	d, ok := parseKeepAlive(keep)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse [" + keep + "]"}
	}
	if d > 24*time.Hour {
		reason := fmt.Sprintf("Keep alive for request (%s) is too large. It must be less than (1d).", keep)
		return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	if from != 0 {
		return &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: using [from] is not allowed in a scroll context;"}
	}
	if _, has := r.URL.Query()["request_cache"]; has {
		return &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: [request_cache] cannot be used in a scroll context;"}
	}
	if size == 0 {
		return &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: [size] cannot be [0] in a scroll context;"}
	}
	if tv, has := body["track_total_hits"]; has {
		if b, ok := tv.(bool); !ok || !b {
			return &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: disabling [track_total_hits] is not allowed in a scroll context;"}
		}
	}
	if _, has := r.URL.Query()["track_total_hits"]; has {
		return &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: disabling [track_total_hits] is not allowed in a scroll context;"}
	}
	if body["collapse"] != nil {
		reason := "cannot use `collapse` in a scroll context"
		return &apiError{Type: "search_phase_execution_exception", Status: 500, Reason: reason,
			RootCause: &apiError{Type: "search_exception", Reason: reason}}
	}
	return nil
}

func (s *Server) handleScroll(w http.ResponseWriter, r *http.Request, rest []string) {
	switch r.Method {
	case http.MethodDelete:
		s.clearScroll(w, r, rest)
		return
	case http.MethodGet, http.MethodPost:
	default:
		methodNotAllowed(w, r, "GET", "POST", "DELETE")
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	sid := r.URL.Query().Get("scroll_id")
	if len(rest) > 0 && rest[0] != "" {
		sid = rest[0]
	}
	if sid == "" {
		if v, ok := body["scroll_id"].(string); ok {
			sid = v
		}
	}
	if sid == "" {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: scrollId is missing;"})
		return
	}
	if !strings.HasPrefix(sid, "slopsearch-scroll-") {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Cannot parse scroll id"})
		return
	}
	keep := r.URL.Query().Get("scroll")
	if keep == "" {
		keep, _ = body["scroll"].(string)
	}
	if keep != "" {
		if d, ok := parseKeepAlive(keep); !ok {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse [" + keep + "]"})
			return
		} else if d > 24*time.Hour {
			s.scrollMu.Lock()
			delete(s.scrolls, sid)
			s.scrollMu.Unlock()
			reason := fmt.Sprintf("Keep alive for request (%s) is too large. It must be less than (1d).", keep)
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
			return
		}
	}
	s.scrollMu.Lock()
	ctx, ok := s.scrolls[sid]
	if !ok {
		s.scrollMu.Unlock()
		reason := "No search context found for id [" + sid + "]"
		writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 404, Reason: reason,
			RootCause: &apiError{Type: "search_context_missing_exception", Reason: reason}})
		return
	}
	start := ctx.pos
	end := start + ctx.size
	if start > len(ctx.hits) {
		start = len(ctx.hits)
	}
	if end > len(ctx.hits) {
		end = len(ctx.hits)
	}
	page := ctx.hits[start:end]
	ctx.pos = end
	restInt := ctx.restInt || r.URL.Query().Get("rest_total_hits_as_int") == "true"
	total := ctx.total
	s.scrollMu.Unlock()

	hits := make([]any, 0, len(page))
	for _, h := range page {
		hits = append(hits, map[string]any{
			"_index": h.idx.Name, "_id": h.id, "_score": scoreJSON(h.score), "_source": h.doc,
		})
	}
	var totalOut any = map[string]any{"value": total, "relation": "eq"}
	if restInt {
		totalOut = total
	}
	resp := map[string]any{
		"_scroll_id": sid,
		"took":       1,
		"timed_out":  false,
		"_shards":    map[string]any{"total": 1, "successful": 1, "skipped": 0, "failed": 0},
		"hits": map[string]any{
			"total":     totalOut,
			"max_score": nil,
			"hits":      hits,
		},
	}
	writeJSON(w, 200, resp)
}

func (s *Server) clearScroll(w http.ResponseWriter, r *http.Request, rest []string) {
	body, _ := readJSON(r)
	var ids []string
	if len(rest) > 0 && rest[0] != "" {
		ids = append(ids, rest[0])
	}
	if raw, ok := body["scroll_id"]; ok {
		switch t := raw.(type) {
		case string:
			ids = append(ids, t)
		case []any:
			for _, v := range t {
				if sv, ok := v.(string); ok {
					ids = append(ids, sv)
				}
			}
		}
	}
	if len(ids) == 0 {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: no scroll ids specified;"})
		return
	}
	for _, id := range ids {
		if !strings.HasPrefix(id, "slopsearch-scroll-") {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Cannot parse scroll id"})
			return
		}
	}
	freed := 0
	s.scrollMu.Lock()
	for _, id := range ids {
		if _, ok := s.scrolls[id]; ok {
			delete(s.scrolls, id)
			freed++
		}
	}
	s.scrollMu.Unlock()
	status := 404
	if freed > 0 {
		status = 200
	}
	writeJSON(w, status, map[string]any{"succeeded": true, "num_freed": freed})
}
