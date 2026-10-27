package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"
)

const (
	nodeHost       = "127.0.0.1"
	nodeEphemeral  = "slopsearch-ephemeral-id"
	nodeTransport  = "127.0.0.1:9300"
	luceneVersion  = "10.5.1"
	stateUUIDConst = "slopsearch-state-uuid"
)

func nowMillis() int64 { return time.Now().UnixMilli() }

func nodeRoles() []any {
	return []any{"cluster_manager", "data", "ingest", "remote_cluster_client"}
}

func nodeAttributes() map[string]any {
	return map[string]any{"shard_indexing_pressure_enabled": "true"}
}

func nodeShortRoles() string { return "dimr" }

func isTrue(v string) bool { return v == "" || v == "true" }

func paramPresent(r *http.Request, name string) bool {
	_, ok := r.URL.Query()[name]
	return ok
}

func paramBool(r *http.Request, name string) bool {
	v, ok := r.URL.Query()[name]
	if !ok {
		return false
	}
	if len(v) == 0 {
		return true
	}
	return v[0] == "" || v[0] == "true"
}

// resolveOpts controls index pattern resolution.
type resolveOpts struct {
	missing404     bool // explicit missing name -> 404
	wildcardClosed bool // wildcards match closed indices
	namedClosed    bool // explicit names match closed indices
}

func (s *Server) resolveIndices(spec string, opts resolveOpts) ([]*Index, *apiError) {
	if spec == "" {
		spec = "*"
	}
	seen := map[string]*Index{}
	var order []string
	add := func(idx *Index) {
		if _, ok := seen[idx.Name]; !ok {
			seen[idx.Name] = idx
			order = append(order, idx.Name)
		}
	}
	for _, token := range strings.Split(spec, ",") {
		token = strings.TrimSpace(token)
		if token == "" {
			continue
		}
		if token == "_all" || token == "*" {
			for _, idx := range s.indices {
				if idx.Closed && !opts.wildcardClosed {
					continue
				}
				add(idx)
			}
			continue
		}
		if strings.ContainsAny(token, "*?") {
			matched := false
			for _, idx := range s.indices {
				if globMatch(token, idx.Name) {
					if idx.Closed && !opts.wildcardClosed {
						continue
					}
					add(idx)
					matched = true
				}
			}
			if !matched {
				for _, idx := range s.indices {
					if idx.Closed && !opts.wildcardClosed {
						continue
					}
					for alias := range idx.Aliases {
						if globMatch(token, alias) {
							add(idx)
							break
						}
					}
				}
			}
			continue
		}
		if idx, ok := s.indices[token]; ok {
			if idx.Closed && !opts.namedClosed && !opts.wildcardClosed {
				add(idx)
				continue
			}
			add(idx)
			continue
		}
		matchedAlias := false
		for _, idx := range s.indices {
			if _, ok := idx.Aliases[token]; ok {
				add(idx)
				matchedAlias = true
			}
		}
		if matchedAlias {
			continue
		}
		if opts.missing404 {
			return nil, errNotFound(token)
		}
	}
	out := make([]*Index, 0, len(order))
	sortStrings(order)
	for _, name := range order {
		out = append(out, seen[name])
	}
	return out, nil
}

// ------------------------------------------------------------------- health

var healthKnownParams = map[string]bool{
	"level": true, "wait_for_status": true, "wait_for_active_shards": true,
	"wait_for_nodes": true, "wait_for_events": true, "wait_for_no_relocating_shards": true,
	"wait_for_no_initializing_shards": true, "timeout": true, "cluster_manager_timeout": true,
	"master_timeout": true, "local": true, "human": true, "expand_wildcards": true,
	"ignore_unavailable": true, "allow_no_indices": true, "filter_path": true,
	"pretty": true, "format": true, "error_trace": true,
}

func validateKnownParams(r *http.Request, known map[string]bool, reqPath string) *apiError {
	for name := range r.URL.Query() {
		if !known[name] {
			return &apiError{Type: "illegal_argument_exception",
				Reason: fmt.Sprintf("request [%s] contains unrecognized parameter: [%s]", reqPath, name), Status: 400}
		}
	}
	return nil
}

func parseTimeout(v string) (time.Duration, bool) {
	if v == "" {
		return 30 * time.Second, true
	}
	units := []struct {
		suffix string
		mult   time.Duration
	}{{"ms", time.Millisecond}, {"s", time.Second}, {"m", time.Minute}, {"h", time.Hour}, {"d", 24 * time.Hour}}
	for _, u := range units {
		if strings.HasSuffix(v, u.suffix) {
			n, err := strconv.ParseFloat(strings.TrimSuffix(v, u.suffix), 64)
			if err != nil {
				return 0, false
			}
			return time.Duration(n * float64(u.mult)), true
		}
	}
	n, err := strconv.ParseFloat(v, 64)
	if err != nil {
		return 0, false
	}
	return time.Duration(n * float64(time.Millisecond)), true
}

type shardHealth struct {
	Status        string
	PrimaryActive bool
	ActiveShards  int
	Relocating    int
	Initializing  int
	Unassigned    int
}

type indexHealth struct {
	Status        string
	NumShards     int
	NumReplicas   int
	ActivePrimary int
	Active        int
	Unassigned    int
	Shards        []shardHealth
}

func computeIndexHealth(idx *Index) indexHealth {
	h := indexHealth{NumShards: idx.numShards(), NumReplicas: idx.numReplicas(), Status: "green"}
	if idx.Closed {
		h.Shards = nil
		return h
	}
	h.ActivePrimary = h.NumShards
	h.Active = h.NumShards
	h.Unassigned = h.NumShards * h.NumReplicas
	if h.Unassigned > 0 {
		h.Status = "yellow"
	}
	for i := 0; i < h.NumShards; i++ {
		sh := shardHealth{Status: "green", PrimaryActive: true, ActiveShards: 1}
		if h.NumReplicas > 0 {
			sh.Status = "yellow"
			sh.Unassigned = h.NumReplicas
		}
		h.Shards = append(h.Shards, sh)
	}
	return h
}

func worstStatus(a, b string) string {
	rank := map[string]int{"green": 0, "yellow": 1, "red": 2}
	if rank[b] > rank[a] {
		return b
	}
	return a
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request, indexSpec string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	if err := validateKnownParams(r, healthKnownParams, "/_cluster/health"); err != nil {
		writeError(w, err)
		return
	}
	q := r.URL.Query()
	if v := q.Get("wait_for_status"); v != "" {
		up := strings.ToUpper(v)
		if up != "GREEN" && up != "YELLOW" && up != "RED" {
			writeError(w, &apiError{Type: "illegal_argument_exception",
				Reason: "No enum constant org.opensearch.cluster.health.ClusterHealthStatus." + up, Status: 400})
			return
		}
	}
	if v := q.Get("wait_for_active_shards"); v != "" && v != "all" {
		if n, err := strconv.Atoi(v); err != nil || n < 0 {
			writeError(w, &apiError{Type: "illegal_argument_exception",
				Reason: "cannot parse ActiveShardCount[" + v + "]", Status: 400})
			return
		}
	}
	if v := q.Get("wait_for_events"); v != "" {
		valid := map[string]bool{"immediate": true, "urgent": true, "high": true, "normal": true, "low": true, "languid": true}
		if !valid[strings.ToLower(v)] {
			writeError(w, &apiError{Type: "illegal_argument_exception",
				Reason: "No enum constant org.opensearch.common.Priority." + strings.ToUpper(v), Status: 400})
			return
		}
	}
	timeout, okTimeout := parseTimeout(q.Get("timeout"))
	if !okTimeout {
		writeError(w, &apiError{Type: "illegal_argument_exception",
			Reason: "failed to parse [" + q.Get("timeout") + "]", Status: 400})
		return
	}
	_ = timeout

	level := q.Get("level")
	includeIndices := level == "indices" || level == "shards"
	missing := false
	var indices []*Index
	if indexSpec != "" {
		var err *apiError
		indices, err = s.resolveIndices(indexSpec, resolveOpts{missing404: true, wildcardClosed: true, namedClosed: true})
		if err != nil {
			missing = true
			if q.Get("ignore_unavailable") == "true" || q.Get("allow_no_indices") == "true" {
				missing = false
				indices = nil
			}
		}
	} else {
		indices, _ = s.resolveIndices("*", resolveOpts{wildcardClosed: true})
	}

	status := "green"
	totalPrim, totalActive, totalUnassigned := 0, 0, 0
	if missing {
		status = "red"
	}
	perIndex := map[string]indexHealth{}
	for _, idx := range indices {
		h := computeIndexHealth(idx)
		perIndex[idx.Name] = h
		status = worstStatus(status, h.Status)
		totalPrim += h.ActivePrimary
		totalActive += h.Active
		totalUnassigned += h.Unassigned
	}

	timedOut := missing
	if !missing {
		if req := q.Get("wait_for_status"); req != "" {
			rank := map[string]int{"green": 0, "yellow": 1, "red": 2}
			if rank[status] > rank[strings.ToLower(req)] {
				timedOut = true
			}
		}
		if v := q.Get("wait_for_active_shards"); v != "" {
			if v == "all" {
				if totalUnassigned > 0 {
					timedOut = true
				}
			} else if n, _ := strconv.Atoi(v); n > totalActive {
				timedOut = true
			}
		}
		if v := q.Get("wait_for_nodes"); v != "" {
			if !waitForNodesSatisfied(v, 1) {
				timedOut = true
			}
		}
	}

	total := totalActive + totalUnassigned
	pct := 100.0
	if total > 0 {
		pct = float64(totalActive) / float64(total) * 100
	}

	body := map[string]any{
		"cluster_name":                     clusterName,
		"status":                           status,
		"timed_out":                        timedOut,
		"number_of_nodes":                  1,
		"number_of_data_nodes":             1,
		"discovered_master":                true,
		"discovered_cluster_manager":       true,
		"active_primary_shards":            totalPrim,
		"active_shards":                    totalActive,
		"relocating_shards":                0,
		"initializing_shards":              0,
		"unassigned_shards":                totalUnassigned,
		"delayed_unassigned_shards":        0,
		"number_of_pending_tasks":          0,
		"number_of_in_flight_fetch":        0,
		"task_max_waiting_in_queue_millis": 0,
		"active_shards_percent_as_number":  json.Number(strconv.FormatFloat(pct, 'f', 1, 64)),
	}
	if paramBool(r, "human") {
		body["task_max_waiting_in_queue"] = "0s"
	}
	if includeIndices {
		out := map[string]any{}
		for name, h := range perIndex {
			entry := map[string]any{
				"status":                h.Status,
				"number_of_shards":      h.NumShards,
				"number_of_replicas":    h.NumReplicas,
				"active_primary_shards": h.ActivePrimary,
				"active_shards":         h.Active,
				"relocating_shards":     0,
				"initializing_shards":   0,
				"unassigned_shards":     h.Unassigned,
			}
			if level == "shards" {
				shards := map[string]any{}
				for i, sh := range h.Shards {
					shards[strconv.Itoa(i)] = map[string]any{
						"status":              sh.Status,
						"primary_active":      sh.PrimaryActive,
						"active_shards":       sh.ActiveShards,
						"relocating_shards":   sh.Relocating,
						"initializing_shards": sh.Initializing,
						"unassigned_shards":   sh.Unassigned,
					}
				}
				entry["shards"] = shards
			}
			out[name] = entry
		}
		body["indices"] = out
	}
	code := 200
	if timedOut {
		code = 408
	}
	writeJSON(w, code, body)
}

func waitForNodesSatisfied(expr string, nodes int) bool {
	expr = strings.TrimSpace(expr)
	if strings.HasPrefix(expr, ">=") {
		n, err := strconv.Atoi(strings.TrimPrefix(expr, ">="))
		return err == nil && nodes >= n
	}
	if strings.HasPrefix(expr, "<=") {
		n, err := strconv.Atoi(strings.TrimPrefix(expr, "<="))
		return err == nil && nodes <= n
	}
	for _, prefix := range []string{"le(", "ge(", "lt(", "gt("} {
		if strings.HasPrefix(expr, prefix) && strings.HasSuffix(expr, ")") {
			n, err := strconv.Atoi(expr[len(prefix) : len(expr)-1])
			if err != nil {
				return false
			}
			switch prefix {
			case "le(":
				return nodes <= n
			case "ge(":
				return nodes >= n
			case "lt(":
				return nodes < n
			case "gt(":
				return nodes > n
			}
		}
	}
	n, err := strconv.Atoi(expr)
	return err == nil && nodes == n
}

// -------------------------------------------------------------------- state

func (s *Server) stateNodes() map[string]any {
	return map[string]any{
		nodeID: map[string]any{
			"name":              nodeName,
			"ephemeral_id":      nodeEphemeral,
			"transport_address": nodeTransport,
			"attributes":        nodeAttributes(),
			"roles":             nodeRoles(),
			"version":           nodeVersion,
		},
	}
}

func (s *Server) handleClusterState(w http.ResponseWriter, r *http.Request, metrics, indexSpec string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	requested := map[string]bool{}
	if metrics == "" {
		metrics = "_all"
	}
	if metrics == "_all" {
		for _, m := range []string{"version", "master_node", "cluster_manager_node", "nodes", "metadata", "routing_table", "routing_nodes", "blocks"} {
			requested[m] = true
		}
	} else {
		for _, m := range strings.Split(metrics, ",") {
			requested[m] = true
		}
	}
	if requested["master_node"] {
		w.Header().Set("Warning", `299 OpenSearch-3.9.0-00000000 "[cluster_manager_node] is deprecated"`)
	}
	body := map[string]any{
		"cluster_name": clusterName,
		"cluster_uuid": clusterUUID,
	}
	if requested["version"] {
		body["version"] = s.stateVersion
		body["state_uuid"] = stateUUIDConst
	}
	if requested["master_node"] {
		body["master_node"] = nodeID
	}
	if requested["cluster_manager_node"] {
		body["cluster_manager_node"] = nodeID
	}
	if requested["nodes"] {
		body["nodes"] = s.stateNodes()
	}
	var indices []*Index
	if indexSpec != "" {
		indices, _ = s.resolveIndices(indexSpec, resolveOpts{namedClosed: true, wildcardClosed: true})
	} else {
		indices, _ = s.resolveIndices("*", resolveOpts{wildcardClosed: true})
	}
	if requested["metadata"] {
		body["metadata"] = s.buildStateMetadata(indices)
	}
	if requested["routing_table"] {
		body["routing_table"] = s.buildRoutingTable(indices)
	}
	if requested["routing_nodes"] {
		body["routing_nodes"] = s.buildRoutingNodes(indices)
	}
	if requested["blocks"] {
		body["blocks"] = s.buildBlocks(indices)
	}
	writeJSON(w, 200, body)
}

func settingsString(v any) string {
	switch t := v.(type) {
	case string:
		return t
	case json.Number:
		return t.String()
	case float64:
		return formatNumber(t)
	case bool:
		if t {
			return "true"
		}
		return "false"
	default:
		return fmt.Sprintf("%v", v)
	}
}

func (s *Server) buildStateMetadata(indices []*Index) map[string]any {
	idxMap := map[string]any{}
	for _, idx := range indices {
		state := "open"
		if idx.Closed {
			state = "close"
		}
		aliases := map[string]any{}
		for name, def := range idx.Aliases {
			entry := map[string]any{}
			for k, v := range def {
				entry[k] = v
			}
			aliases[name] = entry
		}
		idxMap[idx.Name] = map[string]any{
			"version":            1,
			"mapping_version":    idx.MappingVersion,
			"settings_version":   1,
			"aliases_version":    1,
			"routing_num_shards": 1,
			"state":              state,
			"settings":           indexSettingsForMetadata(idx),
			"mappings":           mappingsForMetadata(idx),
			"aliases":            aliases,
		}
	}
	return map[string]any{
		"cluster_uuid": clusterUUID,
		"templates":    map[string]any{},
		"indices":      idxMap,
	}
}

func indexSettingsForMetadata(idx *Index) map[string]any {
	inner := map[string]any{
		"number_of_shards":   settingsString(idx.numShards()),
		"number_of_replicas": settingsString(idx.numReplicas()),
		"provided_name":      idx.Name,
		"creation_date":      settingsString(idx.CreationDate),
		"uuid":               idx.UUID,
		"version":            map[string]any{"created": "137307827"},
	}
	if idx.Settings != nil {
		for k, v := range idx.Settings {
			flat := k
			if m, ok := v.(map[string]any); ok && k != "index" {
				for k2, v2 := range m {
					inner[k2] = settingsString(v2)
				}
				continue
			}
			key := strings.TrimPrefix(flat, "index.")
			if key != "index" {
				inner[key] = settingsString(v)
			}
		}
	}
	return map[string]any{"index": inner}
}

func mappingsForMetadata(idx *Index) map[string]any {
	props := map[string]any{}
	if m, ok := idx.Mappings["properties"].(map[string]any); ok {
		props = m
	}
	return map[string]any{"_doc": map[string]any{"properties": props}}
}

func (s *Server) buildRoutingTable(indices []*Index) map[string]any {
	idxMap := map[string]any{}
	for _, idx := range indices {
		shards := map[string]any{}
		if !idx.Closed {
			for i := 0; i < idx.numShards(); i++ {
				copies := []any{}
				copies = append(copies, map[string]any{
					"index":           idx.Name,
					"shard":           i,
					"primary":         true,
					"state":           "STARTED",
					"node":            nodeID,
					"allocation_id":   map[string]any{"id": nodeID},
					"recovery_source": map[string]any{"type": "PEER"},
					"unassigned_info": nil,
				})
				for r := 0; r < idx.numReplicas(); r++ {
					copies = append(copies, map[string]any{
						"index":           idx.Name,
						"shard":           i,
						"primary":         false,
						"state":           "UNASSIGNED",
						"node":            nil,
						"allocation_id":   map[string]any{"id": ""},
						"recovery_source": map[string]any{"type": "PEER"},
						"unassigned_info": map[string]any{
							"reason":                 "INDEX_CREATED",
							"at":                     time.UnixMilli(idx.CreationDate).UTC().Format("2006-01-02T15:04:05.000Z"),
							"last_allocation_status": "no_valid_shard_copy",
						},
					})
				}
				shards[strconv.Itoa(i)] = copies
			}
		}
		idxMap[idx.Name] = map[string]any{"shards": shards}
	}
	return map[string]any{"indices": idxMap}
}

func (s *Server) buildRoutingNodes(indices []*Index) map[string]any {
	assigned := []any{}
	unassigned := []any{}
	for _, idx := range indices {
		if idx.Closed {
			continue
		}
		for i := 0; i < idx.numShards(); i++ {
			assigned = append(assigned, map[string]any{
				"index": idx.Name, "shard": i, "primary": true, "state": "STARTED", "node": nodeID,
			})
			for r := 0; r < idx.numReplicas(); r++ {
				unassigned = append(unassigned, map[string]any{
					"index": idx.Name, "shard": i, "primary": false, "state": "UNASSIGNED", "node": nil,
				})
			}
		}
	}
	return map[string]any{
		"nodes":      map[string]any{nodeID: assigned},
		"unassigned": unassigned,
	}
}

func (s *Server) buildBlocks(indices []*Index) map[string]any {
	blocks := map[string]any{}
	for _, idx := range indices {
		if idx.Closed {
			blocks[idx.Name] = map[string]any{
				"4": map[string]any{
					"description": "index closed",
					"retryable":   false,
					"levels":      []any{"read", "write"},
				},
			}
		}
	}
	return map[string]any{"indices": blocks}
}

// -------------------------------------------------------------------- stats

func (s *Server) handleClusterStats(w http.ResponseWriter, r *http.Request, nodeSpec string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, _ := s.resolveIndices("*", resolveOpts{wildcardClosed: true})
	open := 0
	fieldTypes := map[string][2]int{}
	docCount := 0
	var createdAt int64 = nowMillis()
	for _, idx := range indices {
		if idx.Closed {
			continue
		}
		open++
		docCount += len(idx.Docs)
		ensureFieldTypes(idx)
		for _, ft := range idx.FieldTypes {
			entry := fieldTypes[ft]
			entry[0]++
			fieldTypes[ft] = entry
		}
		for _, v := range idx.Mappings {
			_ = v
		}
		if idx.CreationDate > 0 && idx.CreationDate < createdAt {
			createdAt = idx.CreationDate
		}
	}
	var ftList []any
	for name, counts := range fieldTypes {
		ftList = append(ftList, map[string]any{
			"name": name, "count": counts[0], "index_count": 1,
		})
	}
	sortAnyByKey(ftList, "name")
	status := "green"
	if open == 0 {
		status = "green"
	}
	_ = createdAt
	body := map[string]any{
		"_nodes":       map[string]any{"total": 1, "successful": 1, "failed": 0},
		"cluster_name": clusterName,
		"cluster_uuid": clusterUUID,
		"timestamp":    nowMillis(),
		"status":       status,
		"indices": map[string]any{
			"count": open,
			"shards": map[string]any{
				"total":       open,
				"primaries":   open,
				"replication": 0.0,
				"index":       map[string]any{"shards": map[string]any{"min": open, "max": open, "avg": float64Ptr(open)}},
			},
			"docs":        map[string]any{"count": docCount, "deleted": 0},
			"store":       map[string]any{"size_in_bytes": 0, "reserved_in_bytes": 0},
			"fielddata":   map[string]any{"memory_size_in_bytes": 0, "evictions": 0},
			"query_cache": map[string]any{"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0, "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0},
			"completion":  map[string]any{"size_in_bytes": 0},
			"segments":    map[string]any{"count": open, "memory_in_bytes": 0, "terms_memory_in_bytes": 0, "stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0, "norms_memory_in_bytes": 0, "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0, "index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0, "fixed_bit_set_memory_in_bytes": 0},
			"mappings":    map[string]any{"field_types": ftList},
			"analysis":    map[string]any{"char_filter_types": []any{}, "tokenizer_types": []any{}, "filter_types": []any{}, "analyzer_types": []any{}, "built_in_char_filters": []any{}, "built_in_tokenizers": []any{}, "built_in_filters": []any{}, "built_in_analyzers": []any{}},
		},
		"nodes": map[string]any{
			"count":    map[string]any{"total": 1, "data": 1, "cluster_manager": 1, "ingest": 1},
			"versions": []any{nodeVersion},
			"os":       map[string]any{"available_processors": int64(8), "allocated_processors": int64(8), "names": []any{}, "pretty_names": []any{}, "mem": map[string]any{"total_in_bytes": int64(1)}},
			"process":  map[string]any{"cpu": map[string]any{"percent": 0}, "open_file_descriptors": map[string]any{"min": 0, "max": 0, "avg": 0.0}},
			"jvm":      map[string]any{"max_uptime_in_millis": int64(1), "versions": []any{}, "mem": map[string]any{"heap_used_in_bytes": int64(1), "heap_max_in_bytes": int64(1)}, "threads": int64(1)},
			"fs":       map[string]any{"total_in_bytes": int64(1), "free_in_bytes": int64(1), "available_in_bytes": int64(1)},
			"plugins":  []any{},
		},
	}
	writeJSON(w, 200, body)
}

func float64Ptr(v int) float64 { return float64(v) }

func sortAnyByKey(list []any, key string) {
	for i := 1; i < len(list); i++ {
		for j := i; j > 0; j-- {
			a, _ := list[j-1].(map[string]any)[key].(string)
			b, _ := list[j].(map[string]any)[key].(string)
			if b < a {
				list[j], list[j-1] = list[j-1], list[j]
			}
		}
	}
}

// ---------------------------------------------------------- cluster settings

var clusterSettingValidators = map[string]func(any) (string, *apiError){
	"cluster.max_shards_per_node": func(v any) (string, *apiError) {
		s := settingsString(v)
		if _, err := strconv.Atoi(s); err != nil {
			return "", &apiError{Type: "illegal_argument_exception",
				Reason: "Failed to parse value [" + s + "] for setting [cluster.max_shards_per_node]", Status: 400}
		}
		return s, nil
	},
	"cluster.routing.allocation.enable": func(v any) (string, *apiError) {
		s := settingsString(v)
		valid := map[string]bool{"all": true, "primaries": true, "new_primaries": true, "none": true}
		if !valid[s] {
			return "", &apiError{Type: "illegal_argument_exception",
				Reason: "Illegal allocation.enable value [" + strings.ToUpper(s) + "]", Status: 400}
		}
		return s, nil
	},
}

var clusterSettingPrefixes = []string{"cluster.", "indices.", "index.", "search.", "discovery.", "node.", "action.", "gateway.", "http.", "transport.", "thread_pool.", "path.", "bootstrap.", "script.", "snapshot.", "repositories.", "ingest.", "plugins."}

var clusterSettingExact = map[string]bool{
	"cluster.name": true, "discovery.type": true, "node.name": true, "node.roles": true,
	"search.max_open_scroll_context": true, "indices.query.bool.max_clause_count": true,
	"search.max_keep_alive": true, "indices.recovery.max_bytes_per_sec": true,
	"cluster.max_shards_per_node": true, "cluster.routing.allocation.enable": true,
}

func isKnownClusterSetting(key string) bool {
	if clusterSettingExact[key] {
		return true
	}
	if strings.HasPrefix(key, "index.") {
		return false
	}
	for _, p := range clusterSettingPrefixes {
		if strings.HasPrefix(key, p) {
			return true
		}
	}
	return false
}

var clusterDefaults = map[string]any{
	"cluster.name":                        clusterName,
	"discovery.type":                      "single-node",
	"search.max_open_scroll_context":      "500",
	"indices.query.bool.max_clause_count": "1024",
	"search.max_keep_alive":               "24h",
	"node.roles":                          []any{"ingest", "remote_cluster_client", "data", "cluster_manager"},
	"cluster.max_shards_per_node":         "1000",
}

func clusterDefaultsFor() map[string]any {
	d := map[string]any{}
	for k, v := range clusterDefaults {
		d[k] = v
	}
	d["node.name"] = nodeName
	return d
}

func flattenMap(prefix string, v any, out map[string]any) {
	if m, ok := v.(map[string]any); ok {
		for k, val := range m {
			flattenMap(prefix+k+".", val, out)
		}
		return
	}
	out[strings.TrimSuffix(prefix, ".")] = v
}

func nestFlat(flat map[string]any) map[string]any {
	out := map[string]any{}
	for k, v := range flat {
		parts := strings.Split(k, ".")
		cur := out
		for i, p := range parts {
			if i == len(parts)-1 {
				cur[p] = v
				break
			}
			next, ok := cur[p].(map[string]any)
			if !ok {
				next = map[string]any{}
				cur[p] = next
			}
			cur = next
		}
	}
	return out
}

func (s *Server) handleClusterSettings(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet, http.MethodHead:
		flat := paramBool(r, "flat_settings")
		includeDefaults := paramBool(r, "include_defaults")
		s.mu.RLock()
		defer s.mu.RUnlock()
		body := map[string]any{}
		if flat {
			body["persistent"] = flatCopy(s.clusterPersistent)
			body["transient"] = flatCopy(s.clusterTransient)
		} else {
			body["persistent"] = nestFlat(flatCopy(s.clusterPersistent))
			body["transient"] = nestFlat(flatCopy(s.clusterTransient))
		}
		if includeDefaults {
			defs := clusterDefaultsFor()
			for k := range s.clusterPersistent {
				delete(defs, k)
			}
			for k := range s.clusterTransient {
				delete(defs, k)
			}
			if flat {
				body["defaults"] = defs
			} else {
				body["defaults"] = nestFlat(defs)
			}
		}
		writeJSON(w, 200, body)
	case http.MethodPut:
		s.handleClusterSettingsPut(w, r)
	default:
		methodNotAllowed(w, r, "PUT,GET")
	}
}

func flatCopy(m map[string]any) map[string]any {
	out := map[string]any{}
	for k, v := range m {
		out[k] = v
	}
	return out
}

func (s *Server) handleClusterSettingsPut(w http.ResponseWriter, r *http.Request) {
	ctype := r.Header.Get("Content-Type")
	body, apiErr := readBodyJSON(w, r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	flat := paramBool(r, "flat_settings")
	apply := func(scope string, raw any, store map[string]any) (map[string]any, *apiError) {
		updated := map[string]any{}
		rawMap, ok := raw.(map[string]any)
		if !ok {
			return updated, nil
		}
		candidates := map[string]any{}
		flattenMap("", rawMap, candidates)
		for key, val := range candidates {
			if !isKnownClusterSetting(key) {
				return nil, &apiError{Type: "settings_exception",
					Reason: scope + " setting [" + key + "], not recognized", Status: 400}
			}
			if val == nil {
				delete(store, key)
				continue
			}
			strVal := settingsString(val)
			if validator, ok := clusterSettingValidators[key]; ok {
				validated, verr := validator(val)
				if verr != nil {
					return nil, verr
				}
				strVal = validated
			}
			store[key] = strVal
		}
		return updated, nil
	}
	var persistentRaw, transientRaw any
	if v, ok := body["persistent"]; ok {
		persistentRaw = v
	}
	if v, ok := body["transient"]; ok {
		transientRaw = v
	}
	hasSettings := false
	for _, raw := range []any{persistentRaw, transientRaw} {
		if m, ok := raw.(map[string]any); ok && len(m) > 0 {
			hasSettings = true
		}
	}
	if !hasSettings {
		writeError(w, &apiError{Type: "action_request_validation_exception",
			Reason: "Validation Failed: 1: no settings to update;", Status: 400})
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := apply("persistent", persistentRaw, s.clusterPersistent); err != nil {
		writeError(w, err)
		return
	}
	if _, err := apply("transient", transientRaw, s.clusterTransient); err != nil {
		writeError(w, err)
		return
	}
	s.bumpState()
	resp := map[string]any{"acknowledged": true}
	if flat {
		resp["persistent"] = flatCopy(s.clusterPersistent)
		resp["transient"] = flatCopy(s.clusterTransient)
	} else {
		resp["persistent"] = nestFlat(flatCopy(s.clusterPersistent))
		resp["transient"] = nestFlat(flatCopy(s.clusterTransient))
	}
	if ctype == "application/yaml" {
		w.Header().Set("Content-Type", "application/yaml")
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		cw := w.(*captureWriter)
		cw.h.Set("Content-Type", "application/yaml")
		cw.buf.Reset()
		cw.buf.WriteString("---\nacknowledged: true\n")
		return
	}
	writeJSON(w, 200, resp)
}

// readBodyJSON reads a JSON or YAML request body, enforcing content types.
func readBodyJSON(w http.ResponseWriter, r *http.Request) (map[string]any, *apiError) {
	ctype := r.Header.Get("Content-Type")
	if r.ContentLength == 0 {
		return nil, &apiError{Type: "parse_exception", Reason: "request body is required", Status: 400}
	}
	base := strings.TrimSpace(strings.Split(ctype, ";")[0])
	switch base {
	case "application/json", "application/x-ndjson":
	case "application/yaml", "application/x-yaml", "text/yaml":
		body, apiErr := readAllBody(r)
		if apiErr != nil {
			return nil, apiErr
		}
		return parseSimpleYAML(string(body)), nil
	default:
		return nil, nil
	}
	body, apiErr := readAllBody(r)
	if apiErr != nil {
		return nil, apiErr
	}
	var out map[string]any
	if err := unmarshalJSON(body, &out); err != nil {
		return nil, &apiError{Type: "json_parse_exception", Reason: err.Error(), Status: 400}
	}
	return out, nil
}

func readAllBody(r *http.Request) ([]byte, *apiError) {
	buf := make([]byte, 0, 64*1024)
	tmp := make([]byte, 4096)
	for {
		n, err := r.Body.Read(tmp)
		buf = append(buf, tmp[:n]...)
		if err != nil {
			break
		}
	}
	return buf, nil
}

func parseSimpleYAML(text string) map[string]any {
	out := map[string]any{}
	var current map[string]any = out
	for _, line := range strings.Split(text, "\n") {
		if strings.TrimSpace(line) == "" || strings.HasPrefix(strings.TrimSpace(line), "#") {
			continue
		}
		if !strings.HasPrefix(line, " ") && strings.HasSuffix(strings.TrimSpace(line), ":") {
			key := strings.TrimSuffix(strings.TrimSpace(line), ":")
			child := map[string]any{}
			out[key] = child
			current = child
			continue
		}
		trimmed := strings.TrimSpace(line)
		if i := strings.Index(trimmed, ":"); i >= 0 {
			key := trimmed[:i]
			val := strings.TrimSpace(trimmed[i+1:])
			switch val {
			case "", "null", "~":
				current[key] = nil
			default:
				current[key] = strings.Trim(val, `"'`)
			}
		}
	}
	return out
}

// -------------------------------------------------------- allocation explain

func (s *Server) handleAllocationExplain(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodPost {
		methodNotAllowed(w, r, "GET,POST")
		return
	}
	body := map[string]any{}
	if r.ContentLength > 0 {
		raw, apiErr := readAllBody(r)
		if apiErr == nil {
			if err := unmarshalJSON(raw, &body); err != nil {
				writeError(w, &apiError{Type: "x_content_parse_exception", Reason: err.Error(), Status: 400})
				return
			}
		}
	}
	for k := range body {
		if k != "index" && k != "shard" && k != "primary" && k != "current_node" && k != "include_yes_decisions" && k != "include_disk_info" {
			writeError(w, &apiError{Type: "x_content_parse_exception",
				Reason: fmt.Sprintf("[1:2] [cluster/allocation/explain] unknown field [%s]", k), Status: 400})
			return
		}
	}
	s.mu.RLock()
	defer s.mu.RUnlock()

	indexName, _ := body["index"].(string)
	var shardNum int = -1
	var primary bool
	hasShard := false
	if v, ok := body["shard"]; ok {
		if f, ok := toFloat(v); ok {
			shardNum = int(f)
		}
		hasShard = true
	}
	if v, ok := body["primary"]; ok {
		primary, _ = v.(bool)
	}
	if indexName == "" {
		for _, name := range sortedIndexNames(s) {
			idx := s.indices[name]
			if idx.Closed {
				continue
			}
			h := computeIndexHealth(idx)
			if h.Unassigned > 0 {
				indexName = idx.Name
				shardNum = 0
				primary = false
				hasShard = true
				break
			}
		}
		if indexName == "" {
			writeError(w, &apiError{Type: "illegal_argument_exception",
				Reason: "unable to find any unassigned shards to explain", Status: 400})
			return
		}
	}
	idx, ok := s.indices[indexName]
	if !ok {
		writeError(w, errNotFound(indexName))
		return
	}
	if !hasShard || shardNum < 0 {
		writeError(w, &apiError{Type: "action_request_validation_exception",
			Reason: "Validation Failed: 1: shard must be specified;2: primary must be specified;", Status: 400})
		return
	}
	if shardNum >= idx.numShards() {
		writeError(w, &apiError{Type: "shard_not_found_exception",
			Reason: "no shard with id [" + strconv.Itoa(shardNum) + "]", Status: 404, Extra: strconv.Itoa(shardNum)})
		return
	}
	h := computeIndexHealth(idx)
	if !primary && h.Unassigned == 0 || primary && idx.numReplicas() == 0 && false {
	}
	if primary {
		writeJSON(w, 200, map[string]any{
			"index":                       idx.Name,
			"shard":                       shardNum,
			"primary":                     true,
			"current_state":               "started",
			"current_node":                map[string]any{"id": nodeID, "name": nodeName, "ephemeral_id": nodeEphemeral, "transport_address": nodeTransport, "attributes": nodeAttributes()},
			"can_remain_on_current_node":  "yes",
			"can_rebalance_cluster":       "yes",
			"can_rebalance_to_other_node": "no",
			"rebalance_explanation":       "no attempt to rebalance",
			"can_allocate":                "no",
			"allocate_explanation":        "the shard is already allocated",
		})
		return
	}
	writeJSON(w, 200, map[string]any{
		"index":         idx.Name,
		"shard":         shardNum,
		"primary":       false,
		"current_state": "unassigned",
		"unassigned_info": map[string]any{
			"reason":                 "INDEX_CREATED",
			"at":                     time.UnixMilli(idx.CreationDate).UTC().Format("2006-01-02T15:04:05.000Z"),
			"last_allocation_status": "no_valid_shard_copy",
		},
		"can_allocate":         "no",
		"allocate_explanation": "cannot allocate because a previous copy of the primary shard existed but can no longer be found on the nodes in the cluster",
		"node_allocation_decisions": []any{
			map[string]any{
				"node_id":           nodeID,
				"node_name":         nodeName,
				"transport_address": nodeTransport,
				"node_attributes":   nodeAttributes(),
				"node_decision":     "no",
				"weight_ranking":    1,
				"deciders": []any{
					map[string]any{
						"decider":     "same_shard",
						"decision":    "NO",
						"explanation": "a copy of this shard is already allocated to this node [[%s][%d], node[%s], [P], s[STARTED]]",
					},
				},
			},
		},
	})
}

func sortedIndexNames(s *Server) []string {
	names := make([]string, 0, len(s.indices))
	for name := range s.indices {
		names = append(names, name)
	}
	sortStrings(names)
	return names
}

// --------------------------------------------------------------------- misc

func (s *Server) handlePendingTasks(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	writeJSON(w, 200, map[string]any{"tasks": []any{}})
}

func (s *Server) handleRemoteInfo(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	writeJSON(w, 200, map[string]any{})
}

// ------------------------------------------------------------------ aliases

func (s *Server) handleAliases(w http.ResponseWriter, r *http.Request, name string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	aliasName := name
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := map[string]any{}
	spec := "*"
	if aliasName != "" {
		spec = aliasName
	}
	for _, idx := range s.indices {
		for alias, def := range idx.Aliases {
			if globMatch(spec, alias) {
				entry := map[string]any{}
				for k, v := range def {
					entry[k] = v
				}
				out[alias] = entry
			}
		}
	}
	writeJSON(w, 200, out)
}

func (s *Server) handleGlobalAliases(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet, http.MethodHead:
		s.handleAliases(w, r, "")
	case http.MethodPost:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		actions, _ := body["actions"].([]any)
		s.mu.Lock()
		defer s.mu.Unlock()
		for _, a := range actions {
			action, _ := a.(map[string]any)
			for kind, raw := range action {
				def, _ := raw.(map[string]any)
				switch kind {
				case "add":
					s.applyAliasAdd(def)
				case "remove":
					s.applyAliasRemove(def, false)
				case "remove_index":
					name, _ := def["index"].(string)
					delete(s.indices, name)
				}
			}
		}
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	default:
		methodNotAllowed(w, r, "GET,POST")
	}
}

func (s *Server) applyAliasAdd(def map[string]any) {
	alias, _ := def["alias"].(string)
	indexRaw := def["index"]
	var names []string
	switch t := indexRaw.(type) {
	case string:
		names = []string{t}
	case []any:
		for _, n := range t {
			if str, ok := n.(string); ok {
				names = append(names, str)
			}
		}
	}
	for _, name := range names {
		idx, ok := s.indices[name]
		if !ok {
			continue
		}
		entry := map[string]any{}
		for k, v := range def {
			if k == "index" || k == "alias" {
				continue
			}
			entry[k] = v
		}
		if _, ok := idx.Aliases[alias]; !ok {
			idx.Aliases[alias] = entry
		} else {
			for k, v := range entry {
				idx.Aliases[alias][k] = v
			}
		}
	}
}

func (s *Server) applyAliasRemove(def map[string]any, removeIndex bool) {
	alias, _ := def["alias"].(string)
	indexRaw := def["index"]
	var names []string
	switch t := indexRaw.(type) {
	case string:
		names = []string{t}
	case []any:
		for _, n := range t {
			if str, ok := n.(string); ok {
				names = append(names, str)
			}
		}
	}
	for _, name := range names {
		idx, ok := s.indices[name]
		if !ok {
			continue
		}
		if alias != "" {
			delete(idx.Aliases, alias)
		}
	}
}

func (s *Server) handleIndexAlias(w http.ResponseWriter, r *http.Request, indexSpec, aliasName string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	indices, apiErr := s.resolveIndices(indexSpec, resolveOpts{wildcardClosed: true})
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	switch r.Method {
	case http.MethodGet, http.MethodHead:
		out := map[string]any{}
		for _, idx := range indices {
			for alias, def := range idx.Aliases {
				if aliasName == "" || globMatch(aliasName, alias) {
					entry := map[string]any{}
					for k, v := range def {
						entry[k] = v
					}
					out[alias] = entry
				}
			}
		}
		writeJSON(w, 200, out)
	case http.MethodPut, http.MethodPost:
		body, _ := readJSON(r)
		if body == nil {
			body = map[string]any{}
		}
		s.bumpState()
		for _, idx := range indices {
			if idx.Aliases == nil {
				idx.Aliases = map[string]map[string]any{}
			}
			entry := map[string]any{}
			for k, v := range body {
				if k == "aliases" {
					continue
				}
				entry[k] = v
			}
			idx.Aliases[aliasName] = entry
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodDelete:
		for _, idx := range indices {
			for alias := range idx.Aliases {
				if aliasName == "" || globMatch(aliasName, alias) {
					delete(idx.Aliases, alias)
				}
			}
		}
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	default:
		methodNotAllowed(w, r, "GET,PUT,POST,DELETE")
	}
}

// -------------------------------------------------------------- close / open

func (s *Server) handleCloseOpen(w http.ResponseWriter, r *http.Request, indexSpec string, close bool) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	o := resolveOpts2{expand: map[string]bool{"open": true, "closed": true}}
	o = s.parseResolveParams(r, o)
	indices, apiErr := s.resolveSpec2(indexSpec, o)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	out := map[string]any{}
	changed := 0
	for _, idx := range indices {
		if close {
			if idx.Closed {
				continue
			}
			idx.Closed = true
			idx.Settings["index.verified_before_close"] = "true"
			changed++
			out[idx.Name] = map[string]any{"closed": true}
		} else {
			if !idx.Closed {
				continue
			}
			idx.Closed = false
			delete(idx.Settings, "index.verified_before_close")
			changed++
		}
	}
	s.bumpState()
	if close {
		writeJSON(w, 200, map[string]any{
			"acknowledged": true, "shards_acknowledged": changed > 0, "indices": out,
		})
		return
	}
	writeJSON(w, 200, map[string]any{"acknowledged": true, "shards_acknowledged": true})
}

// --------------------------------------------------------------- put mapping

func (s *Server) handlePutMapping(w http.ResponseWriter, r *http.Request, indexSpec string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	indices, apiErr := s.resolveIndices(indexSpec, resolveOpts{missing404: true, namedClosed: true, wildcardClosed: true})
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	for _, idx := range indices {
		props := ensureProperties(idx.Mappings)
		mergeMappingForIndex(idx, props, body)
		idx.MappingVersion++
		ensureFieldTypes(idx)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

func (s *Server) handlePutIndexSettings(w http.ResponseWriter, r *http.Request, indexSpec string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	indices, apiErr := s.resolveIndices(indexSpec, resolveOpts{missing404: true, namedClosed: true, wildcardClosed: true})
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	for _, idx := range indices {
		idx.Settings = mergeSettings(idx.Settings, body)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

func mergeSettings(dst, src map[string]any) map[string]any {
	if dst == nil {
		dst = map[string]any{}
	}
	for k, v := range src {
		if m, ok := v.(map[string]any); ok {
			existing, _ := dst[k].(map[string]any)
			dst[k] = mergeSettings(existing, m)
		} else {
			dst[k] = v
		}
	}
	return dst
}

// ------------------------------------------------------------- field caps

func (s *Server) handleFieldCaps(w http.ResponseWriter, r *http.Request, indexSpec string) {
	s.handleFieldCapsImpl(w, r, indexSpec)
}

// -------------------------------------------------------------- server routes

func (s *Server) routeServerEndpoints(w http.ResponseWriter, r *http.Request, segs []string) bool {
	switch segs[0] {
	case "_cluster":
		if len(segs) == 1 {
			writeNoHandler(w, r)
			return true
		}
		sub := segs[1]
		switch sub {
		case "health":
			spec := ""
			if len(segs) == 3 {
				spec = segs[2]
			} else if len(segs) > 3 {
				writeNoHandler(w, r)
				return true
			}
			s.handleHealth(w, r, spec)
		case "state":
			metrics, spec := "", ""
			if len(segs) == 3 {
				metrics = segs[2]
			} else if len(segs) == 4 {
				metrics, spec = segs[2], segs[3]
			} else if len(segs) > 4 {
				writeNoHandler(w, r)
				return true
			}
			s.handleClusterState(w, r, metrics, spec)
		case "stats":
			spec := ""
			if len(segs) == 3 {
				spec = segs[2]
			} else if len(segs) == 4 && segs[2] == "nodes" {
				spec = segs[3]
			} else if len(segs) > 3 {
				writeNoHandler(w, r)
				return true
			}
			s.handleClusterStats(w, r, spec)
		case "settings":
			if len(segs) > 2 {
				writeNoHandler(w, r)
				return true
			}
			s.handleClusterSettings(w, r)
		case "allocation":
			if len(segs) == 3 && segs[2] == "explain" {
				s.handleAllocationExplain(w, r)
			} else {
				writeNoHandler(w, r)
			}
		case "pending_tasks":
			s.handlePendingTasks(w, r)
		case "voting_config_exclusions":
			writeJSON(w, 200, map[string]any{})
		case "reroute":
			writeJSON(w, 200, map[string]any{"acknowledged": true, "state": s.snapshotState()})
		default:
			writeNoHandler(w, r)
		}
		return true
	case "_nodes":
		s.handleNodes(w, r, segs[1:])
		return true
	case "_tasks":
		s.handleTasks(w, r, segs[1:])
		return true
	case "_remote":
		if len(segs) == 2 && segs[1] == "info" {
			s.handleRemoteInfo(w, r)
		} else {
			writeNoHandler(w, r)
		}
		return true
	case "_template":
		s.handleLegacyTemplate(w, r, segs[1:])
		return true
	case "_index_template":
		if len(segs) >= 2 && (segs[1] == "_simulate" || segs[1] == "_simulate_index") {
			s.handleIndexTemplateSimulate(w, r, segs[1:])
			return true
		}
		s.handleComposableTemplate(w, r, segs[1:])
		return true
	case "_component_template":
		s.handleComponentTemplate(w, r, segs[1:])
		return true
	case "_data_stream":
		if len(segs) == 1 {
			s.handleDataStream(w, r, "*")
			return true
		}
		if len(segs) == 2 {
			s.handleDataStream(w, r, segs[1])
			return true
		}
		if len(segs) == 3 && segs[2] == "_stats" {
			s.handleDataStreamStats(w, r, segs[1])
			return true
		}
		writeNoHandler(w, r)
		return true
	case "_resolve":
		if len(segs) == 3 && segs[1] == "index" {
			s.handleResolveIndex(w, r, segs[2])
			return true
		}
		writeNoHandler(w, r)
		return true
	case "_reindex":
		s.handleReindex(w, r)
		return true
	}
	return false
}

func (s *Server) snapshotState() map[string]any {
	return map[string]any{}
}

func (s *Server) handlePutSettings(w http.ResponseWriter, r *http.Request, indexSpec string) {
	s.handlePutIndexSettings(w, r, indexSpec)
}

func (s *Server) handleAlias(w http.ResponseWriter, r *http.Request, indexSpec, aliasName string) {
	s.handleIndexAlias(w, r, indexSpec, aliasName)
}
