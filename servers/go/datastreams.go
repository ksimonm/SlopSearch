package main

import (
	"net/http"
	"sort"
	"strconv"
	"strings"
)

// ------------------------------------------------------------- data streams

// findDataStreamTemplate returns the matching data stream template (highest
// priority composable template with a data_stream definition).
func (s *Server) findDataStreamTemplate(name string) (string, map[string]any) {
	var bestName string
	bestPrio := -1 << 31
	for n, tpl := range s.composableTemplates {
		if _, ok := tpl["data_stream"]; !ok {
			continue
		}
		if !templateMatches(tpl, name) {
			continue
		}
		p := priorityOf(tpl)
		if p > bestPrio || (p == bestPrio && (bestName == "" || n < bestName)) {
			bestPrio = p
			bestName = n
		}
	}
	if bestName == "" {
		return "", nil
	}
	return bestName, s.composableTemplates[bestName]
}

func timestampFieldOf(tpl map[string]any) string {
	field := "@timestamp"
	if ds, ok := tpl["data_stream"].(map[string]any); ok {
		if tf, ok := ds["timestamp_field"].(map[string]any); ok {
			if n, ok := tf["name"].(string); ok && n != "" {
				field = n
			}
		}
	}
	return field
}

func padGeneration(gen int64) string {
	s := strconv.FormatInt(gen, 10)
	for len(s) < 6 {
		s = "0" + s
	}
	return s
}

func backingIndexName(dsName string, generation int64) string {
	return ".ds-" + dsName + "-" + padGeneration(generation)
}

// createDataStream creates a data stream and its first backing index.
// Called with s.mu held.
func (s *Server) createDataStream(name, tplName string, tpl map[string]any) (*dataStreamInfo, *apiError) {
	if name != strings.ToLower(name) {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "data_stream [" + name + "] must be lowercase"}
	}
	tsField := timestampFieldOf(tpl)
	info := &dataStreamInfo{
		Name:           name,
		Generation:     1,
		TimestampField: tsField,
		Template:       tplName,
	}
	idx, err := s.createBackingIndex(info, tpl)
	if err != nil {
		return nil, err
	}
	info.BackingIndices = append(info.BackingIndices, idx.Name)
	s.dataStreams[name] = info
	return info, nil
}

// createBackingIndex builds the next backing index for a data stream.
// Called with s.mu held.
func (s *Server) createBackingIndex(info *dataStreamInfo, tpl map[string]any) (*Index, *apiError) {
	composed := s.composeForIndex(info.Name)
	body := map[string]any{}
	settings := map[string]any{}
	for k, v := range composed.settings {
		settings[k] = v
	}
	settings["index.hidden"] = "true"
	body["settings"] = settings
	mappings := map[string]any{}
	mergeMappingsDeep(mappings, composed.mappings)
	props, _ := mappings["properties"].(map[string]any)
	if props == nil {
		props = map[string]any{}
		mappings["properties"] = props
	}
	if _, has := props[info.TimestampField]; !has {
		props[info.TimestampField] = map[string]any{"type": "date"}
	}
	mappings["_data_stream_timestamp"] = map[string]any{"enabled": true}
	body["mappings"] = mappings
	aliases := map[string]any{}
	for k, v := range composed.aliases {
		aliases[k] = v
	}
	if len(aliases) > 0 {
		body["aliases"] = aliases
	}
	backing := backingIndexName(info.Name, info.Generation)
	idx, err := s.createIndexInternal(backing, body, true)
	if err != nil {
		return nil, err
	}
	idx.DataStream = info.Name
	return idx, nil
}

// maybeAutoCreateDataStream creates a data stream on first document if a
// matching data stream template exists. Called with s.mu held.
func (s *Server) maybeAutoCreateDataStream(name string) (*Index, *apiError) {
	if ds := s.resolveDataStream(name); ds != nil {
		idx := s.indices[ds.BackingIndices[len(ds.BackingIndices)-1]]
		if idx != nil {
			return idx, nil
		}
	}
	tplName, tpl := s.findDataStreamTemplate(name)
	if tpl == nil {
		return nil, nil
	}
	info, err := s.createDataStream(name, tplName, tpl)
	if err != nil {
		return nil, err
	}
	idx := s.indices[info.BackingIndices[len(info.BackingIndices)-1]]
	if idx == nil {
		return nil, errIndexNotFoundNamed(name)
	}
	return idx, nil
}

func (s *Server) resolveDataStream(name string) *dataStreamInfo {
	if ds, ok := s.dataStreams[name]; ok {
		return ds
	}
	return nil
}

func (s *Server) dataStreamIndices(ds *dataStreamInfo) []*Index {
	out := []*Index{}
	for _, name := range ds.BackingIndices {
		if idx, ok := s.indices[name]; ok {
			out = append(out, idx)
		}
	}
	return out
}

func (s *Server) dataStreamView(ds *dataStreamInfo) map[string]any {
	indices := []any{}
	for _, name := range ds.BackingIndices {
		idx, ok := s.indices[name]
		if !ok {
			continue
		}
		indices = append(indices, map[string]any{"index_name": name, "index_uuid": idx.UUID})
	}
	return map[string]any{
		"name":            ds.Name,
		"timestamp_field": map[string]any{"name": ds.TimestampField},
		"indices":         indices,
		"generation":      ds.Generation,
		"status":          "GREEN",
		"template":        ds.Template,
	}
}

func (s *Server) handleDataStream(w http.ResponseWriter, r *http.Request, spec string) {
	switch r.Method {
	case http.MethodPut, http.MethodPost:
		s.mu.Lock()
		defer s.mu.Unlock()
		if ds := s.resolveDataStream(spec); ds != nil {
			writeError(w, &apiError{Type: "resource_already_exists_exception", Status: 400,
				Reason: "data_stream [" + spec + "] already exists"})
			return
		}
		tplName, tpl := s.findDataStreamTemplate(spec)
		if tpl == nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "no matching index template found for data stream [" + spec + "]"})
			return
		}
		if _, err := s.createDataStream(spec, tplName, tpl); err != nil {
			writeError(w, err)
			return
		}
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodGet, http.MethodHead:
		s.mu.RLock()
		defer s.mu.RUnlock()
		names := strings.Split(spec, ",")
		out := []any{}
		missing := ""
		matched := map[string]bool{}
		for _, n := range names {
			n = strings.TrimSpace(n)
			if n == "" {
				continue
			}
			if ds := s.resolveDataStream(n); ds != nil {
				out = append(out, s.dataStreamView(ds))
				matched[n] = true
				continue
			}
			if strings.ContainsAny(n, "*?") {
				for _, dsName := range sortedDataStreamNames(s) {
					ds := s.dataStreams[dsName]
					if globMatch(n, dsName) && !matched[dsName] {
						out = append(out, s.dataStreamView(ds))
						matched[dsName] = true
					}
				}
				continue
			}
			missing = n
		}
		if missing != "" {
			writeError(w, &apiError{Type: "index_not_found_exception", Status: 404,
				Reason: "no such index [" + missing + "]",
				Fields: map[string]any{"index": missing, "index_uuid": "_na_"}})
			return
		}
		writeJSON(w, 200, map[string]any{"data_streams": out})
	case http.MethodDelete:
		s.mu.Lock()
		defer s.mu.Unlock()
		names := strings.Split(spec, ",")
		deleted := false
		for _, n := range names {
			n = strings.TrimSpace(n)
			if n == "" {
				continue
			}
			if ds := s.resolveDataStream(n); ds != nil {
				for _, bn := range ds.BackingIndices {
					delete(s.indices, bn)
				}
				delete(s.dataStreams, n)
				deleted = true
				continue
			}
			if strings.ContainsAny(n, "*?") {
				for _, dsName := range sortedDataStreamNames(s) {
					if globMatch(n, dsName) {
						ds := s.dataStreams[dsName]
						for _, bn := range ds.BackingIndices {
							delete(s.indices, bn)
						}
						delete(s.dataStreams, dsName)
						deleted = true
					}
				}
				continue
			}
			writeError(w, &apiError{Type: "index_not_found_exception", Status: 404,
				Reason: "no such index [" + n + "]"})
			return
		}
		if deleted {
			s.bumpState()
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	default:
		methodNotAllowed(w, r, "PUT", "GET", "DELETE")
	}
}

func sortedDataStreamNames(s *Server) []string {
	names := make([]string, 0, len(s.dataStreams))
	for n := range s.dataStreams {
		names = append(names, n)
	}
	sort.Strings(names)
	return names
}

// handleDataStreamStats renders GET /_data_stream/{name}/_stats.
func (s *Server) handleDataStreamStats(w http.ResponseWriter, r *http.Request, spec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := []any{}
	count := 0
	backing := 0
	for _, n := range sortedDataStreamNames(s) {
		if !globMatch(spec, n) {
			continue
		}
		ds := s.dataStreams[n]
		count++
		backing += len(ds.BackingIndices)
		maxTs := int64(0)
		for _, bn := range ds.BackingIndices {
			idx, ok := s.indices[bn]
			if !ok {
				continue
			}
			for _, id := range idx.Order {
				doc := idx.Docs[id]
				if v, ok := fieldValue(doc, ds.TimestampField); ok {
					if t, ok := parseDate(v); ok {
						if int64(t) > maxTs {
							maxTs = int64(t)
						}
					}
				}
			}
		}
		out = append(out, map[string]any{
			"data_stream":       n,
			"backing_indices":   len(ds.BackingIndices),
			"store_size_bytes":  int64(0),
			"maximum_timestamp": maxTs,
		})
	}
	if out == nil {
		out = []any{}
	}
	writeJSON(w, 200, map[string]any{
		"data_stream_count":      count,
		"backing_indices":        backing,
		"total_store_size_bytes": int64(0),
		"data_streams":           out,
		"_shards":                map[string]any{"total": backing, "successful": backing, "failed": 0},
	})
}

// validateDataStreamDoc enforces timestamp requirements for DS writes.
func (s *Server) validateDataStreamDoc(idx *Index, ds *dataStreamInfo, body map[string]any) *apiError {
	if ds == nil {
		return nil
	}
	v, ok := body[ds.TimestampField]
	if !ok {
		return &apiError{Type: "mapper_parsing_exception", Status: 400,
			Reason: "failed to parse",
			CausedBy: &apiError{Type: "mapper_parsing_exception",
				Reason: "documents must contain a single-valued timestamp field '" + ds.TimestampField + "' of date type"}}
	}
	if _, isSlice := v.([]any); isSlice {
		return &apiError{Type: "mapper_parsing_exception", Status: 400,
			Reason: "failed to parse",
			CausedBy: &apiError{Type: "mapper_parsing_exception",
				Reason: "documents must contain a single-valued timestamp field '" + ds.TimestampField + "' of date type"}}
	}
	if str, ok := v.(string); ok {
		if _, ok := parseDate(str); !ok {
			return &apiError{Type: "mapper_parsing_exception", Status: 400,
				Reason: "failed to parse",
				CausedBy: &apiError{Type: "mapper_parsing_exception",
					Reason: "documents must contain a single-valued timestamp field '" + ds.TimestampField + "' of date type"}}
		}
	}
	return nil
}
