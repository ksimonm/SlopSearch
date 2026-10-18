package main

import (
	"net/http"
	"strconv"
	"strings"
)

// ----------------------------------------------------------- refresh/flush

func (s *Server) maintenanceOp(w http.ResponseWriter, r *http.Request, spec, op string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	shards := 0
	for _, idx := range indices {
		if idx.Closed {
			writeError(w, errIndexClosed(idx))
			return
		}
		shards += idx.numShards()
		switch op {
		case "refresh":
			idx.Pending = nil
		case "forcemerge":
			idx.ForceMerged = true
		}
	}
	if op == "forcemerge" {
		// flush everything on forcemerge
		for _, idx := range indices {
			idx.Pending = nil
		}
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{
		"_shards": map[string]any{"total": shards, "successful": shards, "failed": 0},
	})
}

// ------------------------------------------------------------------ stats

var statsMetricNames = []string{
	"docs", "store", "indexing", "get", "search", "merges", "refresh", "flush",
	"warmer", "query_cache", "fielddata", "completion", "segments", "translog",
	"request_cache", "recovery",
}

func emptyMetric(name string) map[string]any {
	switch name {
	case "docs":
		return map[string]any{"count": 0, "deleted": 0}
	case "store":
		return map[string]any{"size_in_bytes": 0, "total_data_set_size_in_bytes": 0, "reserved_in_bytes": 0}
	case "indexing":
		return map[string]any{"index_total": 0, "index_time_in_millis": 0, "index_current": 0,
			"index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0, "delete_current": 0,
			"noop_update_total": 0, "is_throttled": false, "throttle_time_in_millis": 0}
	case "get":
		return map[string]any{"total": 0, "time_in_millis": 0, "exists_total": 0, "exists_time_in_millis": 0,
			"missing_total": 0, "missing_time_in_millis": 0, "current": 0}
	case "search":
		return map[string]any{"query_total": 0, "query_time_in_millis": 0, "query_current": 0,
			"fetch_total": 0, "fetch_time_in_millis": 0, "fetch_current": 0,
			"scroll_total": 0, "scroll_time_in_millis": 0, "scroll_current": 0,
			"suggest_total": 0, "suggest_time_in_millis": 0, "suggest_current": 0}
	case "merges":
		return map[string]any{"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
			"total": 0, "total_time_in_millis": 0, "total_docs": 0, "total_size_in_bytes": 0,
			"total_stopped_time_in_millis": 0, "total_throttled_time_in_millis": 0, "total_auto_throttle_in_bytes": 0}
	case "refresh":
		return map[string]any{"total": 0, "total_time_in_millis": 0, "external_total": 0,
			"external_total_time_in_millis": 0, "listeners": 0}
	case "flush":
		return map[string]any{"total": 0, "periodic": 0, "total_time_in_millis": 0}
	case "warmer":
		return map[string]any{"current": 0, "total": 0, "total_time_in_millis": 0}
	case "query_cache":
		return map[string]any{"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
			"miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0}
	case "fielddata":
		return map[string]any{"memory_size_in_bytes": 0, "evictions": 0}
	case "completion":
		return map[string]any{"size_in_bytes": 0}
	case "segments":
		return map[string]any{"count": 0, "memory_in_bytes": 0, "terms_memory_in_bytes": 0,
			"stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0,
			"norms_memory_in_bytes": 0, "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0,
			"index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0,
			"fixed_bit_set_memory_in_bytes": 0, "max_unsafe_auto_id_timestamp": -1,
			"file_sizes": map[string]any{}}
	case "translog":
		return map[string]any{"operations": 0, "size_in_bytes": 0, "uncommitted_operations": 0,
			"uncommitted_size_in_bytes": 0, "earliest_last_modified_age": 0}
	case "request_cache":
		return map[string]any{"memory_size_in_bytes": 0, "evictions": 0, "hit_count": 0,
			"miss_count": 0}
	case "recovery":
		return map[string]any{"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0}
	}
	return map[string]any{}
}

func (s *Server) indexMetrics(idx *Index, requested map[string]bool) map[string]any {
	all := requested == nil
	out := map[string]any{}
	for _, name := range statsMetricNames {
		if !all && !requested[name] {
			continue
		}
		m := emptyMetric(name)
		switch name {
		case "docs":
			m["count"] = len(idx.Docs)
			m["deleted"] = idx.DeleteTotal
		case "store":
			m["size_in_bytes"] = idx.storeBytes()
			m["total_data_set_size_in_bytes"] = idx.storeBytes()
		case "indexing":
			m["index_total"] = idx.IndexTotal
			m["delete_total"] = idx.DeleteTotal
		case "segments":
			m["count"] = 1
			m["memory_in_bytes"] = 0
		}
		out[name] = m
	}
	return out
}

func (s *Server) handleStatsAPI(w http.ResponseWriter, r *http.Request, spec, metrics string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	var requested map[string]bool
	if metrics != "" {
		requested = map[string]bool{}
		for _, m := range strings.Split(metrics, ",") {
			m = strings.TrimSpace(m)
			if m == "" {
				continue
			}
			if m == "_all" || m == "*" {
				requested = nil
				break
			}
			known := false
			for _, k := range statsMetricNames {
				if k == m {
					known = true
					break
				}
			}
			if !known {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "request [" + r.URL.Path + "] contains unrecognized metric: [" + m + "]"})
				return
			}
			requested[m] = true
		}
	}
	level := r.URL.Query().Get("level")
	if level == "" {
		level = "indices"
	}
	switch level {
	case "cluster", "indices", "shards":
	default:
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "level parameter must be one of [cluster] or [indices] or [shards] but was [" + level + "]"})
		return
	}
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	shards := 0
	indicesOut := map[string]any{}
	allPrim := map[string]any{}
	allTotal := map[string]any{}
	for _, idx := range indices {
		if idx.Closed {
			writeError(w, errIndexClosed(idx))
			return
		}
		shards += idx.numShards()
		prim := s.indexMetrics(idx, requested)
		total := s.indexMetrics(idx, requested)
		entry := map[string]any{
			"uuid":      idx.UUID,
			"primaries": prim,
			"total":     total,
		}
		if level == "shards" {
			shardArr := []any{map[string]any{
				"id": 0, "routing": map[string]any{"state": "STARTED", "primary": true, "node": nodeID},
				"docs": prim["docs"], "store": prim["store"], "indexing": prim["indexing"],
				"search": prim["search"], "merges": prim["merges"], "refresh": prim["refresh"],
				"flush": prim["flush"], "segments": map[string]any{"count": 1},
			}}
			entry["shards"] = map[string]any{"0": shardArr}
		}
		indicesOut[idx.Name] = entry
		mergeMetrics(allPrim, prim)
		mergeMetrics(allTotal, total)
	}
	resp := map[string]any{
		"_shards": map[string]any{"total": shards, "successful": shards, "failed": 0},
		"_all":    map[string]any{"primaries": allPrim, "total": allTotal},
	}
	if level != "cluster" {
		resp["indices"] = indicesOut
	}
	writeJSON(w, 200, resp)
}

func mergeMetrics(dst, src map[string]any) {
	for k, v := range src {
		sv, ok := v.(map[string]any)
		if !ok {
			continue
		}
		dv, ok := dst[k].(map[string]any)
		if !ok {
			dv = map[string]any{}
			dst[k] = dv
		}
		for mk, mv := range sv {
			switch a := mv.(type) {
			case int:
				if b, ok := dv[mk].(int); ok {
					dv[mk] = a + b
				} else {
					dv[mk] = a
				}
			case int64:
				if b, ok := dv[mk].(int64); ok {
					dv[mk] = a + b
				} else {
					dv[mk] = a
				}
			default:
				if _, exists := dv[mk]; !exists {
					dv[mk] = mv
				}
			}
		}
	}
}

// --------------------------------------------------------------- segments

func (s *Server) handleSegmentsAPI(w http.ResponseWriter, r *http.Request, spec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	shards := 0
	out := map[string]any{}
	for _, idx := range indices {
		if idx.Closed {
			writeError(w, errIndexClosed(idx))
			return
		}
		shards += idx.numShards()
		shardMap := map[string]any{}
		for i := 0; i < idx.numShards(); i++ {
			numDocs := 0
			deleted := 0
			if i == 0 {
				numDocs = len(idx.Docs)
			}
			segs := map[string]any{
				"_0": map[string]any{
					"generation":    0,
					"num_docs":      numDocs,
					"deleted_docs":  deleted,
					"size_in_bytes": idx.storeBytes() / int64(idx.numShards()+1),
					"committed":     true,
					"search":        true,
					"compound":      false,
					"version":       "8.11.1",
				},
			}
			entry := map[string]any{
				"routing":                map[string]any{"state": "STARTED", "primary": true, "node": nodeID},
				"num_committed_segments": 1,
				"num_search_segments":    1,
				"segments":               segs,
			}
			shardMap[strconv.Itoa(i)] = []any{entry}
		}
		out[idx.Name] = map[string]any{"shards": shardMap}
	}
	writeJSON(w, 200, map[string]any{
		"_shards": map[string]any{"total": shards, "successful": shards, "failed": 0},
		"indices": out,
	})
}

// --------------------------------------------------------------- recovery

func (s *Server) handleRecoveryAPI(w http.ResponseWriter, r *http.Request, spec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	if paramBool(r, "active_only") {
		writeJSON(w, 200, map[string]any{})
		return
	}
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	out := map[string]any{}
	for _, idx := range indices {
		shardList := []any{}
		for i := 0; i < idx.numShards(); i++ {
			shardList = append(shardList, map[string]any{
				"id": i, "type": "EMPTY_STORE", "stage": "DONE", "primary": true,
				"start_time_in_millis": idx.CreationDate,
				"stop_time_in_millis":  idx.CreationDate,
				"total_time_in_millis": 0,
				"source":               map[string]any{},
				"target":               map[string]any{"id": nodeID, "host": "127.0.0.1", "transport_address": "127.0.0.1:9300", "ip": "127.0.0.1", "name": nodeName},
				"index":                map[string]any{"size": map[string]any{"total_in_bytes": idx.storeBytes()}, "files": map[string]any{"total": 1, "reused": 0, "recovered": 0}, "total_time_in_millis": 0},
				"translog":             map[string]any{"recovered": 0, "total": 0, "percent": "100.0%", "total_on_start": 0, "total_time_in_millis": 0},
				"verify_index":         map[string]any{"check_index_time_in_millis": 0, "total_time_in_millis": 0},
			})
		}
		out[idx.Name] = map[string]any{"shards": shardList}
	}
	writeJSON(w, 200, out)
}
