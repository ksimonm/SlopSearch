package main

import (
	"fmt"
	"net/http"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

func (s *Server) handleSearchPipelineRoute(w http.ResponseWriter, r *http.Request, segs []string) bool {
	if len(segs) < 2 || segs[1] != "pipeline" {
		return false
	}
	id := strings.Join(segs[2:], "/")
	switch r.Method {
	case http.MethodPut:
		if id == "" {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "pipeline id is missing"})
			return true
		}
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return true
		}
		if apiErr := validateSearchPipeline(body); apiErr != nil {
			writeError(w, apiErr)
			return true
		}
		s.mu.Lock()
		s.searchPipelines[id] = body
		s.mu.Unlock()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	case http.MethodGet:
		s.mu.RLock()
		defer s.mu.RUnlock()
		if id == "" {
			out := map[string]any{}
			for k, v := range s.searchPipelines {
				out[k] = v
			}
			writeJSON(w, 200, out)
			return true
		}
		out := map[string]any{}
		for _, name := range strings.Split(id, ",") {
			if p, ok := s.searchPipelines[name]; ok {
				out[name] = p
			}
		}
		if len(out) == 0 {
			writeJSON(w, 404, map[string]any{})
			return true
		}
		writeJSON(w, 200, out)
		return true
	case http.MethodDelete:
		s.mu.Lock()
		_, ok := s.searchPipelines[id]
		delete(s.searchPipelines, id)
		s.mu.Unlock()
		if !ok {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "pipeline [" + id + "] is missing"})
			return true
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	}
	methodNotAllowed(w, r, "GET", "PUT", "DELETE")
	return true
}

func validateSearchPipeline(body map[string]any) *apiError {
	check := func(key string, allowed map[string]bool) *apiError {
		list, _ := body[key].([]any)
		for _, raw := range list {
			proc, _ := raw.(map[string]any)
			for typ, cfg := range proc {
				if !allowed[typ] {
					return &apiError{Type: "illegal_argument_exception", Status: 400,
						Reason: "Invalid processor type " + typ}
				}
				if typ == "filter_query" {
					m, _ := cfg.(map[string]any)
					if _, ok := m["query"]; !ok {
						return &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason: "Did not specify the query property in processor of type filter_query"}
					}
				}
			}
		}
		return nil
	}
	if err := check("request_processors", map[string]bool{"filter_query": true, "script": true, "oversample": true}); err != nil {
		return err
	}
	return check("response_processors", map[string]bool{
		"rename_field": true, "truncate_hits": true, "collapse": true,
		"split": true, "sort": true, "script": true,
	})
}

func (s *Server) resolveSearchPipeline(r *http.Request, body map[string]any, indices []*Index) (map[string]any, *apiError) {
	var spec any
	if qp := r.URL.Query().Get("search_pipeline"); qp != "" {
		spec = qp
	} else if v, ok := body["search_pipeline"]; ok {
		spec = v
	}
	if spec == nil && len(indices) > 0 {
		if v, ok := findSetting(indices[0].Settings, "search.default_pipeline"); ok {
			spec = settingsValueString(v)
		}
	}
	if spec == nil {
		return nil, nil
	}
	switch t := spec.(type) {
	case map[string]any:
		return t, nil
	case string:
		if t == "" || t == "_none" {
			return nil, nil
		}
		p, ok := s.searchPipelines[t]
		if !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Pipeline " + t + " is not defined"}
		}
		return p, nil
	}
	return nil, nil
}

type searchPipeState struct {
	originalSize int
	hasOriginal  bool
}

func normalizeSearchQuery(v any) any {
	switch t := v.(type) {
	case map[string]any:
		if len(t) == 1 {
			if _, ok := t["match_all"]; ok {
				return map[string]any{"match_all": map[string]any{"boost": 1.0}}
			}
			if term, ok := t["term"].(map[string]any); ok {
				out := map[string]any{}
				for f, val := range term {
					switch tv := val.(type) {
					case map[string]any:
						m := map[string]any{}
						for k, x := range tv {
							m[k] = x
						}
						if _, ok := m["boost"]; !ok {
							m["boost"] = 1.0
						}
						out[f] = m
					default:
						out[f] = map[string]any{"value": val, "boost": 1.0}
					}
				}
				return map[string]any{"term": out}
			}
		}
	}
	return v
}

func applySearchRequestProcessors(body map[string]any, pipe map[string]any, verbose bool) (*searchPipeState, []any, *apiError) {
	st := &searchPipeState{originalSize: intField(body, "size", 10)}
	if _, ok := body["size"]; ok {
		st.hasOriginal = true
	}
	list, _ := pipe["request_processors"].([]any)
	var results []any
	for _, raw := range list {
		proc, _ := raw.(map[string]any)
		for typ, cfg := range proc {
			c, _ := cfg.(map[string]any)
			out := map[string]any{}
			switch typ {
			case "filter_query":
				q := c["query"]
				old := body["query"]
				if old == nil {
					old = map[string]any{"match_all": map[string]any{}}
				}
				body["query"] = map[string]any{"bool": map[string]any{
					"must":   []any{normalizeSearchQuery(old)},
					"filter": []any{normalizeSearchQuery(q)},
				}}
				out["query"] = body["query"]
			case "script":
				src, _ := c["source"].(string)
				if err := applySearchScript(body, src); err != nil {
					return st, results, err
				}
				out["query"] = body["query"]
			case "oversample":
				factor, ok := toFloat(c["sample_factor"])
				if !ok {
					return st, results, &apiError{Type: "parse_exception", Status: 400,
						Reason: "[sample_factor] required property is missing"}
				}
				if factor < 1 {
					return st, results, &apiError{Type: "parse_exception", Status: 400,
						Reason: "[sample_factor] Value must be >= 1.0"}
				}
				sz := intField(body, "size", 10)
				st.originalSize = sz
				st.hasOriginal = true
				body["size"] = int(float64(sz) * factor)
				out["query"] = body["query"]
			}
			if verbose {
				results = append(results, map[string]any{
					"processor_name": typ, "status": "success",
					"duration_millis": 0, "output_data": out,
				})
			}
		}
	}
	return st, results, nil
}

var searchScriptAssignRE = regexp.MustCompile(`ctx\._source(?:\.([A-Za-z_][A-Za-z0-9_.]*)|\[['"]([^'"]+)['"]\])\s*=\s*([^;]+)`)

func applySearchScript(body map[string]any, src string) *apiError {
	for _, m := range searchScriptAssignRE.FindAllStringSubmatch(src, -1) {
		key := m[1]
		if key == "" {
			key = m[2]
		}
		val := strings.TrimSpace(m[3])
		var out any = val
		if n, err := strconv.Atoi(val); err == nil {
			out = n
		} else if val == "true" {
			out = true
		} else if val == "false" {
			out = false
		} else {
			out = strings.Trim(val, "'\"")
		}
		body[key] = out
	}
	return nil
}

func applySearchResponseProcessors(resp map[string]any, pipe map[string]any, st *searchPipeState) *apiError {
	hitsObj, _ := resp["hits"].(map[string]any)
	hits, _ := hitsObj["hits"].([]any)
	for _, h := range hits {
		hm, _ := h.(map[string]any)
		if src, ok := hm["_source"].(map[string]any); ok {
			hm["_source"] = deepCopyMap(src)
		}
	}
	list, _ := pipe["response_processors"].([]any)
	for _, raw := range list {
		proc, _ := raw.(map[string]any)
		for typ, cfg := range proc {
			c, _ := cfg.(map[string]any)
			switch typ {
			case "rename_field":
				field, _ := c["field"].(string)
				target, _ := c["target_field"].(string)
				ignore, _ := c["ignore_missing"].(bool)
				for _, h := range hits {
					hm, _ := h.(map[string]any)
					src, _ := hm["_source"].(map[string]any)
					if src == nil {
						continue
					}
					v, ok := src[field]
					if !ok {
						if ignore {
							continue
						}
						id, _ := hm["_id"].(string)
						return &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason: "Document with id " + id + " is missing field " + field}
					}
					src[target] = v
					delete(src, field)
				}
			case "truncate_hits":
				target := 0
				if v, ok := toFloat(c["target_size"]); ok {
					target = int(v)
				} else if st != nil && st.hasOriginal {
					target = st.originalSize
				} else {
					return &apiError{Type: "illegal_state_exception", Status: 500,
						Reason: "Must specify target_size unless an earlier processor set original_size"}
				}
				if target < len(hits) {
					hits = hits[:target]
					hitsObj["hits"] = hits
				}
			case "collapse":
				field, _ := c["field"].(string)
				seen := map[string]bool{}
				out := make([]any, 0, len(hits))
				for _, h := range hits {
					hm, _ := h.(map[string]any)
					src, _ := hm["_source"].(map[string]any)
					key := ""
					if v, ok := src[field]; ok {
						key = fmt.Sprintf("%v", v)
					}
					if seen[key] {
						continue
					}
					seen[key] = true
					out = append(out, h)
				}
				hits = out
				hitsObj["hits"] = hits
			case "split":
				field, _ := c["field"].(string)
				sep, _ := c["separator"].(string)
				if sep == "" {
					sep = ","
				}
				target, _ := c["target_field"].(string)
				for _, h := range hits {
					hm, _ := h.(map[string]any)
					src, _ := hm["_source"].(map[string]any)
					if src == nil {
						continue
					}
					if s, ok := src[field].(string); ok {
						parts := strings.Split(s, sep)
						arr := make([]any, len(parts))
						for i, p := range parts {
							arr[i] = p
						}
						if target != "" {
							src[target] = arr
						} else {
							src[field] = arr
						}
					}
				}
			case "sort":
				field, _ := c["field"].(string)
				desc := false
				if o, ok := c["order"].(string); ok && o == "desc" {
					desc = true
				}
				for _, h := range hits {
					hm, _ := h.(map[string]any)
					src, _ := hm["_source"].(map[string]any)
					arr, ok := src[field].([]any)
					if !ok {
						continue
					}
					sort.SliceStable(arr, func(i, j int) bool {
						a, _ := toString(arr[i])
						b, _ := toString(arr[j])
						if desc {
							return a > b
						}
						return a < b
					})
					src[field] = arr
				}
			}
		}
	}
	return nil
}
