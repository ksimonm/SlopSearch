package main

import (
	"fmt"
	"net/http"
	"sort"
	"strconv"
	"strings"
	"time"
)

var nodeInfoMetrics = map[string]bool{
	"all": true, "settings": true, "os": true, "process": true, "jvm": true,
	"thread_pool": true, "transport": true, "http": true, "plugins": true,
	"ingest": true, "aggregations": true, "indices": true, "search_pipelines": true,
	"_none": true, "usage": true, "stats": true, "hot_threads": true,
}

var nodeStatsMetrics = map[string]bool{
	"all": true, "jvm": true, "os": true, "process": true, "fs": true, "indices": true,
	"http": true, "transport": true, "thread_pool": true, "ingest": true, "script": true,
	"discovery": true, "breaker": true, "adaptive_selection": true, "_all": true,
	"indexing_pressure": true, "_none": true,
}

var nodeIndexStatsMetrics = map[string]bool{
	"all": true, "docs": true, "store": true, "indexing": true, "get": true, "search": true,
	"merge": true, "refresh": true, "flush": true, "warmer": true, "query_cache": true,
	"fielddata": true, "completion": true, "segments": true, "translog": true, "request_cache": true,
	"recovery": true, "suggest": true, "status_counter": true,
}

var moduleNames = []string{
	"aggs-matrix-stats", "analysis-common", "geo", "ingest-common", "lang-mustache",
	"lang-painless", "mapper-extras", "parent-join", "percolator", "rank-eval",
	"reindex", "search-pipeline-common",
}

func nodeInfoBase() map[string]any {
	return map[string]any{
		"name":              nodeName,
		"transport_address": nodeTransport,
		"host":              nodeHost,
		"ip":                nodeHost,
		"version":           nodeVersion,
		"build_type":        "tar",
		"build_hash":        "0000000000000000000000000000000000000000",
		"roles":             nodeRoles(),
		"attributes":        nodeAttributes(),
	}
}

func modulesList() []any {
	out := make([]any, 0, len(moduleNames))
	for _, name := range moduleNames {
		out = append(out, map[string]any{
			"name":                  name,
			"version":               nodeVersion,
			"opensearch_version":    nodeVersion,
			"java_version":          "25",
			"description":           name + " module",
			"classname":             "org.opensearch." + name,
			"has_native_controller": false,
		})
	}
	return out
}

func (s *Server) nodeInfoForMetric(metric string) map[string]any {
	switch metric {
	case "settings":
		return map[string]any{
			"settings": map[string]any{
				"cluster": map[string]any{"name": clusterName},
				"node":    map[string]any{"name": nodeName, "attr": map[string]any{"shard_indexing_pressure_enabled": "true"}},
				"path":    map[string]any{"home": "/tmp/os", "data": []any{}, "logs": "/tmp/logs"},
			},
		}
	case "os":
		return map[string]any{
			"os": map[string]any{
				"refresh_interval_in_millis": 1000,
				"name":                       "Linux",
				"pretty_name":                "Linux",
				"arch":                       "amd64",
				"version":                    "6.0.0",
				"available_processors":       8,
				"allocated_processors":       8,
				"cpu":                        map[string]any{"percent": 0},
				"mem":                        map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "used_in_bytes": int64(0), "free_percent": 100, "used_percent": 0},
				"swap":                       map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "used_in_bytes": int64(0)},
			},
		}
	case "process":
		return map[string]any{
			"process": map[string]any{
				"refresh_interval_in_millis": 1000,
				"id":                         1,
				"mlockall":                   false,
				"cpu":                        map[string]any{"percent": 0, "total_in_millis": int64(0)},
				"mem":                        map[string]any{"total_virtual_in_bytes": int64(1)},
				"open_file_descriptors":      100,
				"max_file_descriptors":       65535,
			},
		}
	case "jvm":
		return map[string]any{"jvm": nodeJVMInfo(paramBool(&http.Request{}, "human"))}
	case "thread_pool":
		return map[string]any{"thread_pool": nodeThreadPools()}
	case "transport":
		return map[string]any{"transport": map[string]any{
			"bound_address":   []any{nodeTransport},
			"publish_address": nodeTransport,
			"profiles":        map[string]any{},
		}}
	case "http":
		return map[string]any{"http": map[string]any{
			"bound_address":               []any{httpPublishAddress()},
			"publish_address":             httpPublishAddress(),
			"max_content_length_in_bytes": int64(104857600),
		}}
	case "plugins":
		return map[string]any{"plugins": []any{}, "modules": modulesList()}
	case "ingest":
		return map[string]any{"ingest": map[string]any{
			"processors": []any{},
		}}
	case "aggregations":
		return map[string]any{"aggregations": map[string]any{}}
	case "search_pipelines":
		return map[string]any{"search_pipelines": map[string]any{
			"request_processors":  []any{},
			"response_processors": []any{},
		}}
	case "indices":
		return map[string]any{"total_indexing_buffer_in_bytes": int64(1073741824)}
	default:
		return map[string]any{}
	}
}

func httpPublishAddress() string {
	return fmt.Sprintf("%s:%d", nodeHost, httpPortNum())
}

func nodeJVMInfo(human bool) map[string]any {
	mem := map[string]any{"heap_max_in_bytes": int64(1073741824), "heap_used_in_bytes": int64(0)}
	if human {
		mem["heap_max"] = "1gb"
	}
	return map[string]any{
		"pid":                  1234,
		"version":              "25",
		"vm_name":              "OpenJDK 64-Bit Server VM",
		"vm_version":           "25",
		"vm_vendor":            "Eclipse Adoptium",
		"bundled_jdk":          true,
		"using_bundled_jdk":    true,
		"start_time_in_millis": nowMillis(),
		"mem":                  mem,
		"gc_collectors":        []any{"G1 Young Generation", "G1 Old Generation"},
		"memory_pools":         []any{"G1 Eden Space", "G1 Old Gen", "G1 Survivor Space"},
		"input_arguments":      []any{"-Xms1g", "-Xmx1g"},
	}
}

func nodeThreadPools() map[string]any {
	pool := func(kind string, size, queue int) map[string]any {
		return map[string]any{"type": kind, "size": size, "queue_size": queue, "active": 0, "queue": 0, "rejected": 0, "largest": 0, "completed": 0}
	}
	return map[string]any{
		"search":                pool("resizable", 13, 1000),
		"write":                 pool("fixed", 8, 10000),
		"get":                   pool("fixed", 8, 1000),
		"management":            pool("scaling", 2, 10000),
		"generic":               pool("scaling", 4, 10000),
		"analyze":               pool("fixed", 1, 16),
		"snapshot":              pool("scaling", 1, 1000),
		"refresh":               pool("scaling", 2, 1000),
		"flush":                 pool("scaling", 1, 1000),
		"fetch_shard_started":   pool("scaling", 2, 1000),
		"fetch_shard_store":     pool("scaling", 2, 1000),
		"warmer":                pool("scaling", 2, 1000),
		"force_merge":           pool("fixed", 1, 1000),
		"system_read":           pool("fixed", 4, 1000),
		"system_write":          pool("fixed", 4, 1000),
		"system_critical_read":  pool("fixed", 4, 1000),
		"system_critical_write": pool("fixed", 4, 1000),
	}
}

func emptyNodesResponse() map[string]any {
	return map[string]any{
		"_nodes":       map[string]any{"total": 0, "successful": 0, "failed": 0},
		"cluster_name": clusterName,
		"nodes":        map[string]any{},
	}
}

func (s *Server) handleNodes(w http.ResponseWriter, r *http.Request, rest []string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	selector := ""
	rest2 := rest
	if len(rest) > 0 {
		first := rest[0]
		isEndpoint := first == "stats" || first == "usage" || first == "hot_threads"
		if !isEndpoint && !nodeInfoMetrics[first] {
			selector = first
			rest2 = rest[1:]
		}
	}
	if len(rest2) > 0 {
		endpoint := rest2[0]
		switch endpoint {
		case "stats":
			if len(rest2) > 3 {
				writeNoHandler(w, r)
				return
			}
			metric, indexMetric := "", ""
			if len(rest2) >= 2 {
				metric = rest2[1]
			}
			if len(rest2) >= 3 {
				indexMetric = rest2[2]
			}
			s.handleNodeStats(w, r, selector, metric, indexMetric)
			return
		case "usage":
			s.handleNodeUsage(w, r)
			return
		case "hot_threads":
			s.handleHotThreads(w, r)
			return
		}
	}
	if len(rest2) > 1 {
		writeNoHandler(w, r)
		return
	}
	if selector != "" && !nodesSelectorMatches(selector) {
		writeJSON(w, 200, emptyNodesResponse())
		return
	}
	metric := ""
	if len(rest2) == 1 {
		metric = rest2[0]
	}
	s.writeNodeInfo(w, r, metric)
}

func nodesSelectorMatches(selector string) bool {
	for _, token := range strings.Split(selector, ",") {
		token = strings.TrimSpace(token)
		switch token {
		case "_local", "_all", "*", "_cluster_manager", "_master", "data:true", "cluster_manager:true", "master:true", "ingest:true":
			return true
		case nodeID, nodeName:
			return true
		}
		if strings.HasPrefix(token, "_") {
			continue
		}
		if strings.ContainsAny(token, "*?") && (globMatch(token, nodeID) || globMatch(token, nodeName)) {
			return true
		}
	}
	return false
}

func (s *Server) writeNodeInfo(w http.ResponseWriter, r *http.Request, metric string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	metrics := []string{}
	if metric == "" || metric == "all" {
		metrics = []string{"settings", "os", "process", "jvm", "thread_pool", "transport", "http", "plugins", "ingest", "aggregations", "search_pipelines", "indices"}
	} else {
		for _, m := range strings.Split(metric, ",") {
			if nodeInfoMetrics[m] && m != "all" && m != "_none" && m != "usage" && m != "stats" && m != "hot_threads" {
				metrics = append(metrics, m)
			}
		}
	}
	info := nodeInfoBase()
	human := paramBool(r, "human")
	for _, m := range metrics {
		if m == "jvm" {
			info["jvm"] = nodeJVMInfo(human)
			continue
		}
		for k, v := range s.nodeInfoForMetric(m) {
			info[k] = v
		}
	}
	writeJSON(w, 200, map[string]any{
		"_nodes":       map[string]any{"total": 1, "successful": 1, "failed": 0},
		"cluster_name": clusterName,
		"nodes":        map[string]any{nodeID: info},
	})
}

func (s *Server) handleNodeStats(w http.ResponseWriter, r *http.Request, selector, metric, indexMetric string) {
	if selector != "" && !nodesSelectorMatches(selector) {
		writeJSON(w, 200, emptyNodesResponse())
		return
	}
	allDefault := []string{"indices", "os", "process", "jvm", "thread_pool", "fs", "transport", "http", "breakers", "script", "discovery", "ingest"}
	metrics := []string{}
	explicitMetrics := metric != ""
	if explicitMetrics {
		for _, m := range strings.Split(metric, ",") {
			if !nodeStatsMetrics[m] {
				if m == "breakers" {
					writeError(w, &apiError{Type: "illegal_argument_exception",
						Reason: "request [" + r.URL.Path + "] contains unrecognized metric: [" + m + "] -> did you mean [breaker]?", Status: 400})
					return
				}
				writeError(w, &apiError{Type: "illegal_argument_exception",
					Reason: "request [" + r.URL.Path + "] contains unrecognized metric: [" + m + "]", Status: 400})
				return
			}
		}
		if metric == "_all" || metric == "all" {
			if indexMetric != "" {
				writeError(w, &apiError{Type: "illegal_argument_exception",
					Reason: "request [" + r.URL.Path + "] contains index metrics [" + indexMetric + "] but all stats requested", Status: 400})
				return
			}
			metrics = allDefault
		} else {
			for _, m := range strings.Split(metric, ",") {
				if m != "_none" {
					metrics = append(metrics, m)
				}
			}
		}
	} else {
		metrics = allDefault
	}
	if indexMetric != "" {
		if indexMetric == "_all" || indexMetric == "all" {
			indexMetric = ""
		} else {
			for _, m := range strings.Split(indexMetric, ",") {
				if !nodeIndexStatsMetrics[m] {
					writeError(w, &apiError{Type: "illegal_argument_exception",
						Reason: "request [" + r.URL.Path + "] contains unrecognized index metric: [" + m + "]", Status: 400})
					return
				}
			}
			found := false
			for _, m := range metrics {
				if m == "indices" {
					found = true
				}
			}
			if !found {
				metrics = append(metrics, "indices")
			}
		}
	}
	level := r.URL.Query().Get("level")
	if level != "" && level != "node" && level != "indices" && level != "shards" {
		writeJSON(w, 200, map[string]any{
			"_nodes": map[string]any{
				"total": 1, "successful": 0, "failed": 1,
				"failures": []any{map[string]any{
					"type":    "failed_node_exception",
					"reason":  "failed to execute stats on node [" + nodeID + "]",
					"node_id": nodeID,
					"caused_by": map[string]any{
						"type":   "illegal_argument_exception",
						"reason": "level [" + level + "] is not supported",
					},
				}},
			},
			"cluster_name": clusterName,
			"nodes":        map[string]any{},
		})
		return
	}
	stats := s.buildNodeStats(r, metrics, indexMetric, level)
	writeJSON(w, 200, map[string]any{
		"_nodes":       map[string]any{"total": 1, "successful": 1, "failed": 0},
		"cluster_name": clusterName,
		"nodes":        map[string]any{nodeID: stats},
	})
}

func (s *Server) buildNodeStats(r *http.Request, metrics []string, indexMetric, level string) map[string]any {
	stats := map[string]any{
		"timestamp":         nowMillis(),
		"name":              nodeName,
		"transport_address": nodeTransport,
		"host":              nodeHost,
		"ip":                nodeHost,
		"roles":             nodeRoles(),
		"attributes":        nodeAttributes(),
	}
	want := map[string]bool{}
	for _, m := range metrics {
		want[m] = true
	}
	if want["breaker"] {
		want["breakers"] = true
	}
	if want["indices"] {
		s.mu.RLock()
		indices, _ := s.resolveIndices("*", resolveOpts{wildcardClosed: true})
		docsTotal, deletedTotal := 0, 0
		perIndex := map[string]any{}
		for _, idx := range indices {
			if idx.Closed {
				continue
			}
			docsTotal += len(idx.Docs)
			perIndex[idx.Name] = map[string]any{"docs": map[string]any{"count": len(idx.Docs), "deleted": 0}}
		}
		s.mu.RUnlock()
		idxStats := map[string]any{
			"docs":           map[string]any{"count": docsTotal, "deleted": deletedTotal},
			"store":          map[string]any{"size_in_bytes": docsTotal * 64, "reserved_in_bytes": 0},
			"status_counter": map[string]any{"total": 0, "successful": 0, "failed": 0},
		}
		if indexMetric != "" && !strings.Contains(indexMetric, "_all") {
			only := map[string]any{}
			for _, m := range strings.Split(indexMetric, ",") {
				if v, ok := idxStats[m]; ok {
					only[m] = v
				}
			}
			only["status_counter"] = idxStats["status_counter"]
			idxStats = only
		}
		if level == "indices" {
			idxStats["indices"] = perIndex
		}
		stats["indices"] = idxStats
	}
	if want["os"] {
		stats["os"] = map[string]any{
			"timestamp": nowMillis(),
			"cpu":       map[string]any{"percent": 0, "load_average": map[string]any{"1m": 0.0, "5m": 0.0, "15m": 0.0}},
			"mem":       map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "used_in_bytes": int64(0), "free_percent": 100, "used_percent": 0},
			"swap":      map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "used_in_bytes": int64(0)},
		}
	}
	if want["process"] {
		stats["process"] = map[string]any{
			"timestamp":             nowMillis(),
			"open_file_descriptors": 100,
			"max_file_descriptors":  65535,
			"cpu":                   map[string]any{"percent": 0, "total_in_millis": int64(0)},
			"mem":                   map[string]any{"total_virtual_in_bytes": int64(1)},
		}
	}
	if want["jvm"] {
		stats["jvm"] = map[string]any{
			"timestamp":        nowMillis(),
			"uptime_in_millis": int64(1000),
			"mem":              map[string]any{"heap_used_in_bytes": int64(0), "heap_used_percent": 0, "heap_max_in_bytes": int64(1073741824)},
			"threads":          map[string]any{"count": 50, "peak_count": 60},
			"gc":               map[string]any{"collectors": map[string]any{"young": map[string]any{"collection_count": 0}, "old": map[string]any{"collection_count": 0}}},
			"buffer_pools":     map[string]any{},
			"classes":          map[string]any{"current_loaded_count": 1000, "total_loaded_count": 1000, "total_unloaded_count": 0},
		}
	}
	if want["thread_pool"] {
		stats["thread_pool"] = nodeThreadPools()
	}
	if want["fs"] {
		stats["fs"] = map[string]any{
			"timestamp": nowMillis(),
			"total":     map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "available_in_bytes": int64(1)},
			"data":      []any{map[string]any{"path": "/tmp/os", "mount": "/", "type": "ext4", "total_in_bytes": int64(1), "free_in_bytes": int64(1), "available_in_bytes": int64(1)}},
			"io_stats":  map[string]any{"devices": []any{}, "total": map[string]any{"operations": int64(0), "read_operations": int64(0), "write_operations": int64(0), "read_kilobytes": int64(0), "write_kilobytes": int64(0)}},
		}
	}
	if want["transport"] {
		stats["transport"] = map[string]any{"server_open": 1, "total_outbound_connections": 0, "rx_count": 0, "rx_size_in_bytes": int64(0), "tx_count": 0, "tx_size_in_bytes": int64(0)}
	}
	if want["http"] {
		stats["http"] = map[string]any{"current_open": 0, "total_opened": 1}
	}
	if want["breakers"] {
		breaker := func(limit int64) map[string]any {
			return map[string]any{"limit_size_in_bytes": limit, "limit_size": "1gb", "estimated_size_in_bytes": int64(0), "estimated_size": "0b", "overhead": 1.0, "tripped": int64(0)}
		}
		stats["breakers"] = map[string]any{
			"request": breaker(536870912), "fielddata": breaker(214748364), "in_flight_requests": breaker(1073741824),
			"parent": breaker(1000000000), "accounting": breaker(1073741824),
		}
	}
	if want["script"] {
		stats["script"] = map[string]any{"compilations": int64(0), "cache_evictions": int64(0), "compilation_limit_triggered": int64(0)}
	}
	if want["discovery"] {
		stats["discovery"] = map[string]any{
			"cluster_state_queue":      map[string]any{"total": int64(0), "pending": int64(0), "committed": int64(0)},
			"published_cluster_states": map[string]any{"full_states": int64(0), "incompatible_diffs": int64(0), "compatible_diffs": int64(0)},
		}
	}
	if want["ingest"] {
		stats["ingest"] = map[string]any{"total": map[string]any{"count": int64(0), "time_in_millis": int64(0), "current": int64(0), "failed": int64(0)}}
	}
	if want["adaptive_selection"] {
		stats["adaptive_selection"] = map[string]any{}
	}
	if want["indexing_pressure"] {
		stats["indexing_pressure"] = map[string]any{
			"memory": map[string]any{"current": map[string]any{"all_in_bytes": int64(0)}, "total": map[string]any{"all_in_bytes": int64(0)}},
		}
	}
	return stats
}

func (s *Server) handleNodeUsage(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	writeJSON(w, 200, map[string]any{
		"_nodes":       map[string]any{"total": 1, "successful": 1, "failed": 0},
		"cluster_name": clusterName,
		"nodes": map[string]any{nodeID: map[string]any{
			"timestamp":    nowMillis(),
			"since":        nowMillis() - 1000,
			"rest_actions": map[string]any{},
			"aggregations": map[string]any{},
		}},
	})
}

func (s *Server) handleHotThreads(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "text/plain; charset=UTF-8")
	w.WriteHeader(200)
	ts := time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
	w.Write([]byte(fmt.Sprintf("::: {%s}{%s}\n   Hot threads at %s, interval=500ms, busiestThreads=3, ignoreIdleThreads=true:\n\n", nodeName, nodeID, ts)))
}

// -------------------------------------------------------------------- tasks

func (s *Server) syntheticTasks() []map[string]any {
	parentID := "1"
	childID := "2"
	now := nowMillis()
	parent := map[string]any{
		"node": nodeID, "id": parentID, "type": "transport",
		"action":               "cluster:monitor/tasks/lists",
		"start_time_in_millis": now, "running_time_in_nanos": int64(1000),
		"cancellable": false, "cancelled": false, "headers": map[string]any{},
		"description": "cluster:monitor/tasks/lists",
	}
	child := map[string]any{
		"node": nodeID, "id": childID, "type": "direct",
		"action":               "cluster:monitor/tasks/lists[n]",
		"start_time_in_millis": now, "running_time_in_nanos": int64(500),
		"cancellable": true, "cancelled": false, "headers": map[string]any{},
		"parent_task_id": nodeID + ":" + parentID,
		"description":    "cluster:monitor/tasks/lists[n]",
	}
	return []map[string]any{parent, child}
}

func taskActionMatches(task map[string]any, actions string) bool {
	if actions == "" || actions == "*" {
		return true
	}
	action, _ := task["action"].(string)
	for _, pat := range strings.Split(actions, ",") {
		if globMatch(pat, action) {
			return true
		}
	}
	return false
}

func taskJSON(task map[string]any, detailed bool) map[string]any {
	out := map[string]any{}
	for k, v := range task {
		if k == "description" && !detailed {
			continue
		}
		if k == "description" && detailed {
			out[k] = v
			continue
		}
		out[k] = v
	}
	return out
}

func (s *Server) handleTasks(w http.ResponseWriter, r *http.Request, rest []string) {
	q := r.URL.Query()
	if len(rest) == 1 && rest[0] == "_cancel" {
		if r.Method != http.MethodPost {
			methodNotAllowed(w, r, "POST")
			return
		}
		matched := []map[string]any{}
		for _, t := range s.syntheticTasks() {
			if taskActionMatches(t, q.Get("actions")) {
				matched = append(matched, t)
			}
		}
		if len(matched) == 0 {
			writeJSON(w, 200, map[string]any{"nodes": map[string]any{}})
			return
		}
		tasksOut := map[string]any{}
		for _, t := range matched {
			tasksOut[t["node"].(string)+":"+t["id"].(string)] = map[string]any{"action": t["action"], "cancelled": true}
		}
		writeJSON(w, 200, map[string]any{"nodes": map[string]any{nodeID: map[string]any{"name": nodeName, "tasks": tasksOut}}})
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	if len(rest) == 1 {
		id := rest[0]
		if !strings.Contains(id, ":") {
			writeError(w, &apiError{Type: "illegal_argument_exception", Reason: "malformed task id " + id, Status: 400})
			return
		}
		parts := strings.SplitN(id, ":", 2)
		if parts[0] != nodeID {
			writeError(w, &apiError{Type: "resource_not_found_exception",
				Reason: "task [" + id + "] belongs to the node [" + parts[0] + "] which is not part of the cluster", Status: 404})
			return
		}
		writeError(w, &apiError{Type: "resource_not_found_exception",
			Reason: "task [" + id + "] isn't running and hasn't stored its results", Status: 404})
		return
	}
	groupBy := q.Get("group_by")
	if groupBy == "" {
		groupBy = "nodes"
	}
	if groupBy != "nodes" && groupBy != "parents" && groupBy != "none" {
		writeError(w, &apiError{Type: "illegal_argument_exception",
			Reason: "[group_by] must be one of [nodes], [parents] or [none] but was [" + groupBy + "]", Status: 400})
		return
	}
	detailed := paramBool(r, "detailed")
	actions := q.Get("actions")
	matched := []map[string]any{}
	for _, t := range s.syntheticTasks() {
		if taskActionMatches(t, actions) {
			matched = append(matched, t)
		}
	}
	if len(matched) == 0 {
		writeJSON(w, 200, map[string]any{"nodes": map[string]any{}})
		return
	}
	switch groupBy {
	case "none":
		list := []any{}
		for _, t := range matched {
			list = append(list, taskJSON(t, detailed))
		}
		writeJSON(w, 200, map[string]any{"tasks": list})
	case "parents":
		parents := map[string]any{}
		children := map[string][]any{}
		for _, t := range matched {
			if pid, ok := t["parent_task_id"].(string); ok {
				children[pid] = append(children[pid], taskJSON(t, detailed))
			} else {
				parents[t["node"].(string)+":"+t["id"].(string)] = taskJSON(t, detailed)
			}
		}
		for pid, kids := range children {
			if parent, ok := parents[pid].(map[string]any); ok {
				parent["children"] = kids
			}
		}
		writeJSON(w, 200, map[string]any{"tasks": parents})
	default:
		tasksOut := map[string]any{}
		for _, t := range matched {
			tasksOut[t["node"].(string)+":"+t["id"].(string)] = taskJSON(t, detailed)
		}
		writeJSON(w, 200, map[string]any{
			"nodes": map[string]any{nodeID: map[string]any{"name": nodeName, "tasks": tasksOut}},
		})
	}
}

func sortedThreadPoolNames() []string {
	pools := nodeThreadPools()
	names := make([]string, 0, len(pools))
	for name := range pools {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

var _ = strconv.Itoa
