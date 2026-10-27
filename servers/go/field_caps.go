package main

import (
	"net/http"
	"sort"
	"strings"
)

func fieldCapsCollect(props map[string]any, prefix string, out map[string]map[string]any) {
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		path := name
		if prefix != "" {
			path = prefix + "." + name
		}
		out[path] = def
		if fields, ok := def["fields"].(map[string]any); ok {
			for fname, fraw := range fields {
				if fdef, ok := fraw.(map[string]any); ok {
					out[path+"."+fname] = fdef
				}
			}
		}
		if sub, ok := def["properties"].(map[string]any); ok {
			fieldCapsCollect(sub, path, out)
		}
	}
}

func fieldCapsMetadata() map[string]map[string]any {
	return map[string]map[string]any{
		"_id":      {"type": "_id"},
		"_index":   {"type": "_index"},
		"_source":  {"type": "_source"},
		"_seq_no":  {"type": "_seq_no"},
		"_routing": {"type": "_routing"},
	}
}

func fieldCapsType(def map[string]any) string {
	if t, ok := def["type"].(string); ok && t != "" {
		return t
	}
	return "object"
}

func fieldCapsFlags(ftype string, def map[string]any) (bool, bool) {
	searchable, aggregatable := true, true
	switch ftype {
	case "object", "nested":
		searchable, aggregatable = false, false
	case "text", "match_only_text", "search_as_you_type":
		aggregatable = false
	case "unmapped":
		searchable, aggregatable = false, false
	case "_source":
		searchable, aggregatable = false, false
	case "_routing":
		aggregatable = false
	}
	if v, ok := def["index"].(bool); ok && !v {
		searchable = false
	}
	if v, ok := def["doc_values"].(bool); ok && !v {
		aggregatable = false
	}
	return searchable, aggregatable
}

func fieldCapsFields(r *http.Request) []string {
	raw := r.URL.Query()["fields"]
	out := []string{}
	seen := map[string]bool{}
	for _, item := range raw {
		for _, part := range strings.Split(item, ",") {
			part = strings.TrimSpace(part)
			if part != "" && !seen[part] {
				seen[part] = true
				out = append(out, part)
			}
		}
	}
	return out
}

func (s *Server) handleFieldCapsImpl(w http.ResponseWriter, r *http.Request, indexSpec string) {
	if r.Method != http.MethodGet && r.Method != http.MethodPost {
		methodNotAllowed(w, r, "GET", "POST")
		return
	}
	requested := fieldCapsFields(r)
	if len(requested) == 0 {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "specified fields can't be null or empty"})
		return
	}
	includeUnmapped := r.URL.Query().Get("include_unmapped") == "true"
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, apiErr := s.resolveIndices(indexSpec, resolveOpts{missing404: true, wildcardClosed: true})
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	names := []any{}
	perIndex := []map[string]map[string]any{}
	union := map[string]map[string]any{}
	unionOrder := []string{}
	for _, idx := range indices {
		names = append(names, idx.Name)
		defs := map[string]map[string]any{}
		if props, ok := idx.Mappings["properties"].(map[string]any); ok {
			fieldCapsCollect(props, "", defs)
		}
		for k, v := range fieldCapsMetadata() {
			defs[k] = v
		}
		perIndex = append(perIndex, defs)
		for name, def := range defs {
			if _, ok := union[name]; !ok {
				union[name] = def
				unionOrder = append(unionOrder, name)
			}
		}
	}
	sort.Strings(unionOrder)
	selected := []string{}
	seenSel := map[string]bool{}
	for _, pat := range requested {
		matched := false
		if strings.ContainsAny(pat, "*?") {
			for _, name := range unionOrder {
				if wildcardMatch(pat, name) && !seenSel[name] {
					seenSel[name] = true
					selected = append(selected, name)
					matched = true
				}
			}
			if matched {
				continue
			}
		} else if _, ok := union[pat]; ok {
			if !seenSel[pat] {
				seenSel[pat] = true
				selected = append(selected, pat)
			}
			continue
		}
		if includeUnmapped && !seenSel[pat] {
			seenSel[pat] = true
			selected = append(selected, pat)
		}
	}
	fieldsOut := map[string]any{}
	for _, field := range selected {
		typeEntries := map[string]map[string]any{}
		typeOrder := []string{}
		for i, idx := range indices {
			def, ok := perIndex[i][field]
			if !ok {
				if !includeUnmapped {
					continue
				}
				def = map[string]any{"type": "unmapped"}
			}
			ftype := fieldCapsType(def)
			searchable, aggregatable := fieldCapsFlags(ftype, def)
			entry, ok := typeEntries[ftype]
			if !ok {
				entry = map[string]any{"type": ftype, "searchable": searchable, "aggregatable": aggregatable}
				typeEntries[ftype] = entry
				typeOrder = append(typeOrder, ftype)
			}
			if _, has := entry["indices"]; !has {
				entry["_indices"] = []string{}
			}
			entry["_indices"] = append(entry["_indices"].([]string), idx.Name)
		}
		if len(typeEntries) == 0 {
			continue
		}
		multi := len(typeEntries) > 1 || includeUnmapped
		out := map[string]any{}
		for _, ftype := range typeOrder {
			entry := typeEntries[ftype]
			idxs := entry["_indices"].([]string)
			delete(entry, "_indices")
			if multi {
				entry["indices"] = idxs
			}
			out[ftype] = entry
		}
		fieldsOut[field] = out
	}
	writeJSON(w, 200, map[string]any{"indices": names, "fields": fieldsOut})
}
