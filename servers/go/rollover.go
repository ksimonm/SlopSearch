package main

import (
	"fmt"
	"net/http"
	"regexp"
	"strconv"
	"strings"
	"time"
)

var rolloverSuffixRE = regexp.MustCompile(`^(.*)-(\d+)$`)

func parseDurationMillis(v any) (int64, bool) {
	s, ok := v.(string)
	if !ok {
		if f, ok2 := toFloat(v); ok2 {
			return int64(f), true
		}
		return 0, false
	}
	s = strings.TrimSpace(s)
	if s == "" {
		return 0, false
	}
	re := regexp.MustCompile(`^(\d+)(ms|s|m|h|d|w)$`)
	m := re.FindStringSubmatch(s)
	if m == nil {
		return 0, false
	}
	n, _ := strconv.ParseInt(m[1], 10, 64)
	switch m[2] {
	case "ms":
		return n, true
	case "s":
		return n * 1000, true
	case "m":
		return n * 60 * 1000, true
	case "h":
		return n * 3600 * 1000, true
	case "d":
		return n * 86400 * 1000, true
	case "w":
		return n * 7 * 86400 * 1000, true
	}
	return 0, false
}

// conditionKey renders "[max_docs: 5]" style condition keys.
func conditionKey(name string, v any) string {
	return fmt.Sprintf("[%s: %v]", name, v)
}

func (s *Server) handleRollover(w http.ResponseWriter, r *http.Request, target, newIndexName string) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	bodyAny, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	conditionsRaw, _ := bodyAny["conditions"].(map[string]any)

	s.mu.Lock()
	defer s.mu.Unlock()

	// Validate condition names eagerly.
	condNames := []string{}
	for name := range conditionsRaw {
		switch name {
		case "max_docs", "max_age", "max_size", "max_primary_shard_size", "max_primary_shard_docs":
			condNames = append(condNames, name)
		default:
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "unknown field [" + name + "]",
				Fields: map[string]any{"rollover.conditions." + name: "unknown field"}})
			return
		}
	}

	dryRun := queryBool(r, "dry_run")

	if _, isConcrete := s.indices[target]; isConcrete {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "rollover target is a [concrete index] but one of [alias,data_stream] was expected"})
		return
	}

	if ds := s.resolveDataStream(target); ds != nil {
		if len(ds.BackingIndices) == 0 {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "rollover target [" + target + "] does not exist"})
			return
		}
		last := ds.BackingIndices[len(ds.BackingIndices)-1]
		oldIdx := s.indices[last]
		if oldIdx == nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "rollover target [" + target + "] does not exist"})
			return
		}
		conds := s.evaluateRolloverConditions(conditionsRaw, condNames, oldIdx)
		rolledOver := len(conditionsRaw) == 0
		for _, v := range conds {
			if v.(bool) {
				rolledOver = true
			}
		}
		newName := backingIndexName(target, ds.Generation+1)
		if newIndexName != "" {
			newName = newIndexName
		}
		ack := false
		if rolledOver && !dryRun {
			if _, err := s.createBackingIndex(&dataStreamInfo{Name: ds.Name, Generation: ds.Generation + 1, TimestampField: ds.TimestampField, Template: ds.Template}, nil); err != nil {
				writeError(w, err)
				return
			}
			ds.Generation++
			ds.BackingIndices = append(ds.BackingIndices, newName)
			ack = true
			s.bumpState()
		}
		resp := map[string]any{
			"acknowledged":        ack,
			"shards_acknowledged": ack,
			"old_index":           last,
			"new_index":           newName,
			"rolled_over":         rolledOver && !dryRun,
			"dry_run":             dryRun,
			"conditions":          conds,
		}
		writeJSON(w, 200, resp)
		return
	}

	// Alias path.
	var withAlias []*Index
	var writeIdx *Index
	for _, name := range sortedIndexNames(s) {
		idx := s.indices[name]
		if def, ok := idx.Aliases[target]; ok {
			withAlias = append(withAlias, idx)
			if b, _ := def["is_write_index"].(bool); b {
				writeIdx = idx
			}
		}
	}
	if len(withAlias) == 0 {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "rollover target [" + target + "] does not exist"})
		return
	}
	var oldIdx *Index
	if writeIdx != nil {
		oldIdx = writeIdx
	} else if len(withAlias) == 1 {
		oldIdx = withAlias[0]
	} else {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "rollover target [" + target + "] does not point to a write index"})
		return
	}

	newName := newIndexName
	if newName == "" {
		m := rolloverSuffixRE.FindStringSubmatch(oldIdx.Name)
		if m == nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "index name [" + oldIdx.Name + "] does not match pattern '^.*-\\d+$'"})
			return
		}
		n, _ := strconv.ParseInt(m[2], 10, 64)
		width := len(m[2])
		newName = fmt.Sprintf("%s-%0*d", m[1], width, n+1)
	}

	conds := s.evaluateRolloverConditions(conditionsRaw, condNames, oldIdx)
	rolledOver := len(conditionsRaw) == 0
	for _, v := range conds {
		if v.(bool) {
			rolledOver = true
		}
	}

	ack := false
	if rolledOver && !dryRun {
		settings := map[string]any{}
		for k, v := range oldIdx.Settings {
			settings[k] = v
		}
		mappings := deepCopyJSON(oldIdx.Mappings).(map[string]any)
		body := map[string]any{"settings": settings, "mappings": mappings}
		if _, apiErr2 := s.createIndexInternal(newName, body, false); apiErr2 != nil {
			writeError(w, apiErr2)
			return
		}
		oldDef := oldIdx.Aliases[target]
		newDef := map[string]any{}
		if _, hasWrite := oldDef["is_write_index"]; hasWrite {
			oldCopy := map[string]any{}
			for k, v := range oldDef {
				oldCopy[k] = v
			}
			oldCopy["is_write_index"] = false
			oldIdx.Aliases[target] = oldCopy
			newDef["is_write_index"] = true
		} else {
			delete(oldIdx.Aliases, target)
		}
		s.indices[newName].Aliases[target] = newDef
		ack = true
		s.bumpState()
	}

	resp := map[string]any{
		"acknowledged":        ack,
		"shards_acknowledged": ack,
		"old_index":           oldIdx.Name,
		"new_index":           newName,
		"rolled_over":         rolledOver && !dryRun,
		"dry_run":             dryRun,
		"conditions":          conds,
	}
	writeJSON(w, 200, resp)
}

// evaluateRolloverConditions evaluates each condition; true means "ready to roll over".
func (s *Server) evaluateRolloverConditions(raw map[string]any, names []string, idx *Index) map[string]any {
	out := map[string]any{}
	for _, name := range names {
		v := raw[name]
		met := false
		switch name {
		case "max_docs", "max_primary_shard_docs":
			n, ok := settingsValueInt(v)
			if ok {
				met = int64(len(idx.Order)) >= int64(n)
			}
		case "max_age":
			ms, ok := parseDurationMillis(v)
			if ok {
				met = time.Now().UnixMilli()-idx.CreationDate >= ms
			}
		case "max_size", "max_primary_shard_size":
			met = false
		}
		out[conditionKey(name, v)] = met
	}
	return out
}
