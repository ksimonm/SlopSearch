package main

import (
	"net/http"
	"strings"
)

var knownMappingBodyKeys = map[string]bool{
	"properties": true, "dynamic": true, "_source": true, "_meta": true,
	"dynamic_templates": true, "date_detection": true, "numeric_detection": true,
	"_routing": true, "_field_names": true, "runtime": true, "_size": true,
	"_timestamp": true, "_ttl": true, "_doc": false,
}

func (s *Server) getMappingAPI(w http.ResponseWriter, r *http.Request, spec, fields string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	includeDefaults := paramBool(r, "include_defaults")
	out := map[string]any{}
	for _, idx := range indices {
		if err := metaReadBlock(idx); err != nil {
			writeError(w, err)
			return
		}
		if fields == "" {
			mappings := idx.Mappings
			if mappings == nil {
				mappings = map[string]any{}
			}
			out[idx.Name] = map[string]any{"mappings": mappings}
		} else {
			out[idx.Name] = map[string]any{"mappings": fieldMappings(idx, fields, includeDefaults)}
		}
	}
	writeJSON(w, 200, out)
}

func fieldMappings(idx *Index, spec string, includeDefaults bool) map[string]any {
	leaves := []fieldLeaf{}
	if props, ok := idx.Mappings["properties"].(map[string]any); ok {
		collectLeaves(props, "", &leaves)
	}
	patterns := strings.Split(spec, ",")
	result := map[string]any{}
	for _, leaf := range leaves {
		if !matchAnySettingPattern(patterns, leaf.path) {
			continue
		}
		def := map[string]any{}
		for k, v := range leaf.def {
			def[k] = v
		}
		if includeDefaults {
			mergeFieldDefaults(def)
		}
		result[leaf.path] = map[string]any{
			"full_name": leaf.path,
			"mapping":   map[string]any{leaf.leafName: def},
		}
	}
	return result
}

type fieldLeaf struct {
	path     string
	leafName string
	def      map[string]any
}

func collectLeaves(props map[string]any, prefix string, out *[]fieldLeaf) {
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		path := name
		if prefix != "" {
			path = prefix + "." + name
		}
		if sub, ok := def["properties"].(map[string]any); ok {
			collectLeaves(sub, path, out)
		}
		if _, hasType := def["type"]; hasType {
			*out = append(*out, fieldLeaf{path: path, leafName: name, def: def})
		}
		if fields, ok := def["fields"].(map[string]any); ok {
			for fname, fraw := range fields {
				if fdef, ok := fraw.(map[string]any); ok {
					*out = append(*out, fieldLeaf{path: path + "." + fname, leafName: fname, def: fdef})
				}
			}
		}
	}
}

func mergeFieldDefaults(def map[string]any) {
	t, _ := def["type"].(string)
	base := map[string]any{}
	switch t {
	case "float", "double", "integer", "long", "short", "byte", "half_float", "scaled_float":
		base = map[string]any{"index": true, "doc_values": true, "store": false,
			"coerce": true, "ignore_malformed": false, "null_value": nil}
	case "keyword", "constant_keyword", "wildcard":
		base = map[string]any{"index": true, "doc_values": true, "store": false,
			"ignore_above": 2147483647, "null_value": nil}
	case "text":
		base = map[string]any{"index": true, "store": false, "analyzer": "default",
			"search_analyzer": "default", "boost": 1.0, "null_value": nil}
	case "date":
		base = map[string]any{"index": true, "doc_values": true, "store": false, "null_value": nil}
	case "boolean":
		base = map[string]any{"index": true, "doc_values": true, "store": false, "null_value": nil}
	case "object", "nested":
		base = map[string]any{"dynamic": "true", "enabled": true}
	case "ip", "binary", "geo_point", "geo_shape", "completion":
		base = map[string]any{"index": true, "doc_values": true, "store": false, "null_value": nil}
	}
	for k, v := range base {
		if _, ok := def[k]; !ok {
			def[k] = v
		}
	}
}

func (s *Server) putMappingAPI(w http.ResponseWriter, r *http.Request, spec string) {
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr3 := s.resolveSpec2(spec, o)
	if apiErr3 != nil {
		writeError(w, apiErr3)
		return
	}
	for k := range body {
		if !knownMappingBodyKeys[k] {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Types cannot be provided in put mapping requests"})
			return
		}
	}
	if props, ok := body["properties"].(map[string]any); ok {
		if err := validateProps(props); err != nil {
			reason := err.Error()
			writeError(w, &apiError{Type: "mapper_parsing_exception", Status: 400,
				Reason:   "Failed to parse mapping [_doc]: " + reason,
				CausedBy: &apiError{Type: "mapper_parsing_exception", Reason: reason}})
			return
		}
	}
	for _, idx := range indices {
		if err := metaReadBlock(idx); err != nil {
			writeError(w, err)
			return
		}
		existingProps := ensureProperties(idx.Mappings)
		if props, ok := body["properties"].(map[string]any); ok {
			if err := mergeMappingChecked(existingProps, props); err != nil {
				writeError(w, err)
				return
			}
		}
		if d, ok := body["dynamic"]; ok {
			idx.Mappings["dynamic"] = d
		}
		for k, v := range body {
			if k == "properties" || k == "dynamic" {
				continue
			}
			idx.Mappings[k] = v
		}
		idx.MappingVersion++
		ensureFieldTypes(idx)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

// mergeMappingChecked merges new mapping properties into existing ones,
// returning OpenSearch-compatible conflict errors.
func mergeMappingChecked(existing, incoming map[string]any) *apiError {
	for name, rawNew := range incoming {
		newDef, ok := rawNew.(map[string]any)
		if !ok {
			continue
		}
		oldRaw, exists := existing[name]
		if !exists {
			existing[name] = newDef
			continue
		}
		oldDef, ok := oldRaw.(map[string]any)
		if !ok {
			existing[name] = newDef
			continue
		}
		newType, _ := newDef["type"].(string)
		oldType, _ := oldDef["type"].(string)
		oldProps, oldIsObject := oldDef["properties"].(map[string]any)
		newProps, newIsObject := newDef["properties"].(map[string]any)
		if oldIsObject && newType != "" && !newIsObject {
			return errBad("can't merge a non object mapping [" + name + "] with an object mapping")
		}
		if !oldIsObject && newIsObject {
			return errBad("can't merge a non object mapping [" + name + "] with an object mapping")
		}
		if oldIsObject && newIsObject {
			if err := mergeMappingChecked(oldProps, newProps); err != nil {
				return err
			}
		}
		if newType != "" && oldType != "" && newType != oldType {
			return errBad("mapper [" + name + "] cannot be changed from type [" + oldType + "] to [" + newType + "]")
		}
		for pk, pv := range newDef {
			switch pk {
			case "type", "properties":
				continue
			case "fields":
				oldFields, _ := oldDef["fields"].(map[string]any)
				if oldFields == nil {
					oldFields = map[string]any{}
					oldDef["fields"] = oldFields
				}
				newFields, _ := pv.(map[string]any)
				for fk, fv := range newFields {
					oldFields[fk] = fv
				}
				continue
			}
			oldVal, has := oldDef[pk]
			if !has {
				if pk == "analyzer" && oldType == "text" {
					if pv != "default" && pv != "standard" {
						return &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason: "Mapper for [" + name + "] conflicts with existing mapper:\n\tCannot update parameter [analyzer] from [default] to [" + settingsValueString(pv) + "]"}
					}
				}
				oldDef[pk] = pv
				continue
			}
			if settingsValueString(oldVal) != settingsValueString(pv) {
				return &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Mapper for [" + name + "] conflicts with existing mapper:\n\tCannot update parameter [" + pk + "] from [" + settingsValueString(oldVal) + "] to [" + settingsValueString(pv) + "]"}
			}
		}
	}
	return nil
}
