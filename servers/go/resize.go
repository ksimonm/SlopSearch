package main

import (
	"fmt"
	"net/http"
	"strconv"
)

// handleResize implements POST /{src}/_clone|_shrink|_split/{target}.
func (s *Server) handleResize(w http.ResponseWriter, r *http.Request, src, op, target string) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	idx, ok := s.indices[src]
	if !ok {
		writeError(w, errIndexNotFoundNamed(src))
		return
	}
	if !idx.blockOn("write") {
		writeError(w, &apiError{Type: "illegal_state_exception", Status: 500,
			Reason: fmt.Sprintf("index %s must block write operations to resize index", src)})
		return
	}
	if _, exists := s.indices[target]; exists {
		writeError(w, &apiError{Type: "resource_already_exists_exception", Status: 400,
			Reason: "index [" + target + "] already exists", Fields: map[string]any{"index": target}})
		return
	}
	if err := validateIndexName(target); err != nil {
		writeError(w, err)
		return
	}
	settingsBody, _ := body["settings"].(map[string]any)
	bodySettings := map[string]any{}
	flattenSettings(settingsBody, bodySettings)
	nRaw, hasN := bodySettings["index.number_of_shards"]
	srcShards, ok := settingsValueInt(idx.Settings["index.number_of_shards"])
	if !ok {
		srcShards = 1
	}
	want := srcShards
	switch op {
	case "clone":
		if hasN {
			n, ok := settingsValueInt(nRaw)
			if !ok {
				writeError(w, errBad("Failed to parse value ["+settingsValueString(nRaw)+"] for setting [index.number_of_shards]"))
				return
			}
			want = n
		}
		if want != srcShards {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("the number of target shards (%d) must be the same as the number of source shards (%d)", want, srcShards)})
			return
		}
	case "shrink":
		if srcShards == 1 {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "can't shrink an index with only one shard"})
			return
		}
		want = 1
		if hasN {
			n, ok := settingsValueInt(nRaw)
			if !ok {
				writeError(w, errBad("Failed to parse value ["+settingsValueString(nRaw)+"] for setting [index.number_of_shards]"))
				return
			}
			want = n
		}
		if want >= srcShards {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("the number of target shards [%d] must be less that the number of source shards [%d]", want, srcShards)})
			return
		}
	case "split":
		if !hasN {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: index.number_of_shards is required for split operations;"})
			return
		}
		n, ok := settingsValueInt(nRaw)
		if !ok {
			writeError(w, errBad("Failed to parse value ["+settingsValueString(nRaw)+"] for setting [index.number_of_shards]"))
			return
		}
		want = n
		if srcShards == 0 || want%srcShards != 0 {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("the number of source shards [%d] must be a factor of [%d]", srcShards, want)})
			return
		}
	}
	newSettings := map[string]any{}
	for k, v := range idx.Settings {
		newSettings[k] = v
	}
	newSettings["index.number_of_shards"] = strconv.Itoa(want)
	for k, v := range bodySettings {
		newSettings[k] = v
	}
	newSettings["index.resize.source.name"] = src
	newSettings["index.resize.source.uuid"] = idx.UUID
	newSettings["index.resize.type"] = op

	newBody := map[string]any{
		"settings": newSettings,
		"mappings": deepCopyJSON(idx.Mappings),
	}
	if a, ok := body["aliases"]; ok && a != nil {
		newBody["aliases"] = a
	}
	newIdx, apiErr := s.createIndexInternal(target, newBody, false)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	for _, id := range idx.Order {
		doc := deepCopyJSON(idx.Docs[id]).(map[string]any)
		newIdx.Docs[id] = doc
		newIdx.Order = append(newIdx.Order, id)
		_ = newIdx.computeDocValues(id, doc)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true, "shards_acknowledged": true, "index": target})
}
