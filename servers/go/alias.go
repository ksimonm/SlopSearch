package main

import (
	"net/http"
	"sort"
	"strings"
)

// indexAliasesView renders the indexed alias form: {index: {"aliases": {...}}}
func indexAliasesView(indices []*Index, nameSpec string, includeAll bool) map[string]any {
	out := map[string]any{}
	for _, idx := range indices {
		aliases := map[string]any{}
		for an, def := range idx.Aliases {
			if nameSpec == "" || includeAll || globMatch(nameSpec, an) {
				entry := map[string]any{}
				for k, v := range def {
					entry[k] = v
				}
				aliases[an] = entry
			}
		}
		if nameSpec == "" || includeAll || len(aliases) > 0 {
			out[idx.Name] = map[string]any{"aliases": aliases}
		}
	}
	return out
}

func (s *Server) getAliasesIndexed(w http.ResponseWriter, r *http.Request, indexSpec, aliasSpec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(indexSpec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	if aliasSpec == "" {
		writeJSON(w, 200, indexAliasesView(indices, "", true))
		return
	}
	names := strings.Split(aliasSpec, ",")
	hasWildcard := strings.ContainsAny(aliasSpec, "*?")
	found := map[string]bool{}
	out := map[string]any{}
	for _, idx := range indices {
		aliases := map[string]any{}
		for _, want := range names {
			for an, def := range idx.Aliases {
				if an == want || globMatch(want, an) {
					entry := map[string]any{}
					for k, v := range def {
						entry[k] = v
					}
					aliases[an] = entry
					found[an] = true
					found[want] = true
				}
			}
		}
		if len(aliases) > 0 {
			out[idx.Name] = map[string]any{"aliases": aliases}
		}
	}
	if hasWildcard {
		writeJSON(w, 200, out)
		return
	}
	for _, want := range names {
		if !found[want] {
			writeSimpleError(w, 404, "alias ["+want+"] missing")
			return
		}
	}
	writeJSON(w, 200, out)
}

// global alias view: GET /_alias, /_alias/{names}, /_aliases
func (s *Server) getAliasesGlobal(w http.ResponseWriter, r *http.Request, aliasSpec string, includeNoAlias bool) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := map[string]any{}
	missing := ""
	if aliasSpec == "" {
		for _, name := range sortedIndexNames(s) {
			idx := s.indices[name]
			aliases := map[string]any{}
			for an, def := range idx.Aliases {
				entry := map[string]any{}
				for k, v := range def {
					entry[k] = v
				}
				aliases[an] = entry
			}
			if len(aliases) > 0 || includeNoAlias {
				out[idx.Name] = map[string]any{"aliases": aliases}
			}
		}
		writeJSON(w, 200, out)
		return
	}
	hasWildcard := strings.ContainsAny(aliasSpec, "*?")
	names := strings.Split(aliasSpec, ",")
	found := map[string]bool{}
	for _, name := range sortedIndexNames(s) {
		idx := s.indices[name]
		aliases := map[string]any{}
		for _, want := range names {
			for an, def := range idx.Aliases {
				if an == want || globMatch(want, an) {
					entry := map[string]any{}
					for k, v := range def {
						entry[k] = v
					}
					aliases[an] = entry
					found[want] = true
				}
			}
		}
		if len(aliases) > 0 {
			out[idx.Name] = map[string]any{"aliases": aliases}
		}
	}
	for _, want := range names {
		if !hasWildcard && !found[want] {
			missing = want
			break
		}
	}
	if hasWildcard && len(out) == 0 {
		missing = names[0]
	}
	if missing != "" {
		if len(out) > 0 {
			out["error"] = "alias [" + missing + "] missing"
			out["status"] = 404
			writeJSON(w, 404, out)
			return
		}
		writeSimpleError(w, 404, "alias ["+missing+"] missing")
		return
	}
	writeJSON(w, 200, out)
}

func (s *Server) headAliasesGlobal(w http.ResponseWriter, r *http.Request, aliasSpec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, idx := range s.indices {
		for an := range idx.Aliases {
			if an == aliasSpec || globMatch(aliasSpec, an) {
				w.WriteHeader(200)
				return
			}
		}
	}
	w.WriteHeader(404)
}

func (s *Server) aliasHiddenConflict(aliasName string, def map[string]any) *apiError {
	newHidden, hasNew := def["is_hidden"]
	for _, idx := range s.indices {
		existing, ok := idx.Aliases[aliasName]
		if !ok {
			continue
		}
		oldHidden, hasOld := existing["is_hidden"]
		if hasNew != hasOld || (hasNew && oldHidden != newHidden) {
			return &apiError{Type: "illegal_state_exception", Status: 500,
				Reason: "alias must have the same is_hidden setting on all indices"}
		}
	}
	return nil
}

func multipleWriteIndicesErr(alias string, indices []string) *apiError {
	sorted := append([]string{}, indices...)
	sort.Strings(sorted)
	return &apiError{Type: "illegal_state_exception", Status: 500,
		Reason: "alias [" + alias + "] has more than one write index [" + strings.Join(sorted, ",") + "]"}
}

func errNoWriteIndex(alias string) *apiError {
	return errBad("no write index is defined for alias [" + alias + "]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index")
}

// putAliasAPI handles PUT/POST /{idx}/_alias/{name} and /{idx}/_aliases/{name}
func (s *Server) putAliasAPI(w http.ResponseWriter, r *http.Request, indexSpec, aliasName string) {
	if err := validateAliasName(aliasName); err != nil {
		writeError(w, err)
		return
	}
	for _, an := range strings.Split(aliasName, ",") {
		if _, clash := s.indices[an]; clash {
			writeError(w, &apiError{Type: "invalid_alias_name_exception", Status: 400,
				Reason: "Invalid alias name [" + an + "], an index exists with the same name as the alias"})
			return
		}
	}
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr3 := s.resolveSpec2(indexSpec, o)
	if apiErr3 != nil {
		writeError(w, apiErr3)
		return
	}
	def := normalizeAliasDef(body)
	if filt, ok := def["filter"]; ok && filt != nil {
		if fq, ok := filt.(map[string]any); ok {
			if err := validateQuery(&Index{FieldTypes: map[string]string{}}, fq); err != nil {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason:   "failed to parse filter for alias [" + aliasName + "]",
					CausedBy: &apiError{Type: "parsing_exception", Reason: err.Reason}})
				return
			}
		}
	}
	if len(indices) == 0 {
		writeError(w, errIndexNotFoundNamed(indexSpec))
		return
	}
	if err := s.aliasHiddenConflict(aliasName, def); err != nil {
		writeError(w, err)
		return
	}
	for _, idx := range indices {
		if isWrite, ok := def["is_write_index"].(bool); ok && isWrite {
			var others []string
			for _, other := range s.indices {
				if other == idx {
					continue
				}
				if d, ok := other.Aliases[aliasName]; ok {
					if b, _ := d["is_write_index"].(bool); b {
						others = append(others, other.Name)
					}
				}
			}
			if len(others) > 0 {
				writeError(w, multipleWriteIndicesErr(aliasName, append(others, idx.Name)))
				return
			}
		}
	}
	for _, idx := range indices {
		if idx.Aliases == nil {
			idx.Aliases = map[string]map[string]any{}
		}
		entry := map[string]any{}
		for k, v := range def {
			entry[k] = v
		}
		idx.Aliases[aliasName] = entry
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

// deleteAliasAPI handles DELETE /{idx}/_alias/{names}
func (s *Server) deleteAliasAPI(w http.ResponseWriter, r *http.Request, indexSpec, aliasSpec string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(indexSpec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	if aliasSpec == "" {
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return
	}
	names := strings.Split(aliasSpec, ",")
	anyDeleted := false
	var missing []string
	for _, want := range names {
		deleted := false
		for _, idx := range indices {
			for an := range idx.Aliases {
				if an == want || globMatch(want, an) {
					delete(idx.Aliases, an)
					deleted = true
					anyDeleted = true
				}
			}
		}
		if !deleted {
			missing = append(missing, want)
		}
	}
	if !anyDeleted && len(missing) > 0 {
		writeError(w, &apiError{Type: "aliases_not_found_exception", Status: 404,
			Reason: "aliases [" + strings.Join(missing, ",") + "] missing",
			Fields: map[string]any{"resource.type": "aliases", "resource.id": strings.Join(missing, ",")}})
		return
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

// ---------------------------------------------------------------- _aliases

type aliasAction struct {
	kind string
	def  map[string]any
}

func (s *Server) postAliases(w http.ResponseWriter, r *http.Request) {
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	rawActions, ok := body["actions"].([]any)
	if !ok || len(rawActions) == 0 {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No action specified"})
		return
	}
	actions := make([]aliasAction, 0, len(rawActions))
	for _, raw := range rawActions {
		m, ok := raw.(map[string]any)
		if !ok || len(m) != 1 {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse alias action"})
			return
		}
		for kind, defRaw := range m {
			var def map[string]any
			if defRaw != nil {
				def, _ = defRaw.(map[string]any)
			}
			if def == nil {
				def = map[string]any{}
			}
			switch kind {
			case "add", "remove", "remove_index":
			default:
				writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
					Reason: "unknown alias action [" + kind + "]"})
				return
			}
			if _, hasIndex := def["index"]; !hasIndex {
				if _, hasIndices := def["indices"]; !hasIndices {
					writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
						Reason: "Failed to parse alias action"})
					return
				}
			}
			if kind == "add" || kind == "remove" {
				_, hasAlias := def["alias"]
				_, hasAliases := def["aliases"]
				if !hasAlias && !hasAliases {
					writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
						Reason: "Failed to parse alias action"})
					return
				}
			}
			actions = append(actions, aliasAction{kind: kind, def: def})
		}
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	type pendingChange struct {
		idx    *Index
		alias  string
		def    map[string]any
		delete bool
	}
	var changes []pendingChange
	var removeIndexes []*Index
	aliasMissing := ""
	applied := false

	expandNames := func(def map[string]any) ([]string, []string) {
		var indicesNames []string
		switch t := def["index"].(type) {
		case string:
			indicesNames = append(indicesNames, t)
		case []any:
			for _, v := range t {
				if s2, ok := v.(string); ok {
					indicesNames = append(indicesNames, s2)
				}
			}
		}
		switch t := def["indices"].(type) {
		case string:
			indicesNames = append(indicesNames, t)
		case []any:
			for _, v := range t {
				if s2, ok := v.(string); ok {
					indicesNames = append(indicesNames, s2)
				}
			}
		}
		var aliasNames []string
		switch t := def["alias"].(type) {
		case string:
			aliasNames = append(aliasNames, t)
		case []any:
			for _, v := range t {
				if s2, ok := v.(string); ok {
					aliasNames = append(aliasNames, s2)
				}
			}
		}
		switch t := def["aliases"].(type) {
		case string:
			aliasNames = append(aliasNames, t)
		case []any:
			for _, v := range t {
				if s2, ok := v.(string); ok {
					aliasNames = append(aliasNames, s2)
				}
			}
		}
		return indicesNames, aliasNames
	}

	resolveForAction := func(name string, requireExisting bool) ([]*Index, *apiError) {
		var matched []*Index
		direct, exists := s.indices[name]
		if exists {
			matched = append(matched, direct)
		} else if strings.ContainsAny(name, "*?") {
			for _, n := range sortedIndexNames(s) {
				idx := s.indices[n]
				if globMatch(name, idx.Name) {
					matched = append(matched, idx)
					continue
				}
				for an := range idx.Aliases {
					if globMatch(name, an) {
						matched = append(matched, idx)
						break
					}
				}
			}
		} else {
			for _, n := range sortedIndexNames(s) {
				idx := s.indices[n]
				if _, ok := idx.Aliases[name]; ok {
					matched = append(matched, idx)
				}
			}
		}
		if len(matched) == 0 && requireExisting {
			return nil, errIndexNotFoundNamed(name)
		}
		return matched, nil
	}

	for _, a := range actions {
		switch a.kind {
		case "add":
			indexNames, aliasNames := expandNames(a.def)
			for _, aliasName := range aliasNames {
				if err := validateAliasName(aliasName); err != nil {
					writeError(w, err)
					return
				}
			}
			var targetIndices []*Index
			for _, in := range indexNames {
				matched, err := resolveForAction(in, true)
				if err != nil {
					writeError(w, err)
					return
				}
				targetIndices = append(targetIndices, matched...)
			}
			def := map[string]any{}
			for k, v := range a.def {
				switch k {
				case "index", "indices", "alias", "aliases":
					continue
				}
				def[k] = v
			}
			def = normalizeAliasDef(def)
			if filt, ok := def["filter"]; ok && filt != nil {
				if fq, ok := filt.(map[string]any); ok {
					if err := validateQuery(&Index{FieldTypes: map[string]string{}}, fq); err != nil {
						writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason:   "failed to parse filter for alias [" + strings.Join(aliasNames, ",") + "]",
							CausedBy: &apiError{Type: "parsing_exception", Reason: err.Reason}})
						return
					}
				}
			}
			for _, aliasName := range aliasNames {
				if err := s.aliasHiddenConflict(aliasName, def); err != nil {
					writeError(w, err)
					return
				}
			}
			for _, idx := range targetIndices {
				for _, aliasName := range aliasNames {
					if isWrite, ok := def["is_write_index"].(bool); ok && isWrite {
						var others []string
						for _, other := range s.indices {
							if other == idx {
								continue
							}
							if d, ok := other.Aliases[aliasName]; ok {
								if b, _ := d["is_write_index"].(bool); b {
									others = append(others, other.Name)
								}
							}
						}
						// also account for changes queued in this batch
						for _, ch := range changes {
							if ch.idx != idx && ch.alias == aliasName && !ch.delete {
								if b, _ := ch.def["is_write_index"].(bool); b {
									others = append(others, ch.idx.Name)
								}
							}
						}
						if len(others) > 0 {
							writeError(w, multipleWriteIndicesErr(aliasName, append(others, idx.Name)))
							return
						}
					}
					changes = append(changes, pendingChange{idx: idx, alias: aliasName, def: def})
					applied = true
				}
			}
		case "remove":
			indexNames, aliasNames := expandNames(a.def)
			requireExisting := true
			if v, ok := a.def["must_exist"].(bool); ok {
				requireExisting = v
			}
			var targetIndices []*Index
			for _, in := range indexNames {
				matched, err := resolveForAction(in, true)
				if err != nil {
					writeError(w, err)
					return
				}
				targetIndices = append(targetIndices, matched...)
			}
			for _, idx := range targetIndices {
				for _, aliasName := range aliasNames {
					if _, ok := idx.Aliases[aliasName]; ok {
						changes = append(changes, pendingChange{idx: idx, alias: aliasName, delete: true})
						applied = true
					} else if requireExisting && aliasMissing == "" && !applied {
						aliasMissing = aliasName
					}
				}
			}
		case "remove_index":
			indexNames, _ := expandNames(a.def)
			for _, in := range indexNames {
				if _, ok := s.indices[in]; ok {
					removeIndexes = append(removeIndexes, s.indices[in])
					continue
				}
				if strings.ContainsAny(in, "*?") {
					matched, err := resolveForAction(in, true)
					if err != nil {
						writeError(w, err)
						return
					}
					removeIndexes = append(removeIndexes, matched...)
					continue
				}
				aliasHit := false
				for _, idx := range s.indices {
					if _, ok := idx.Aliases[in]; ok {
						aliasHit = true
					}
				}
				if aliasHit {
					writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
						Reason: "The provided expression [" + in + "] matches an alias, specify the corresponding concrete indices instead."})
					return
				}
				writeError(w, errIndexNotFoundNamed(in))
				return
			}
			if len(removeIndexes) > 0 {
				applied = true
			}
		}
	}
	if len(changes) == 0 && len(removeIndexes) == 0 {
		if aliasMissing != "" {
			writeError(w, &apiError{Type: "aliases_not_found_exception", Status: 404,
				Reason: "aliases [" + aliasMissing + "] missing",
				Fields: map[string]any{"resource.type": "aliases", "resource.id": aliasMissing}})
			return
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return
	}
	for _, ch := range changes {
		if ch.idx.Aliases == nil {
			ch.idx.Aliases = map[string]map[string]any{}
		}
		if ch.delete {
			delete(ch.idx.Aliases, ch.alias)
		} else {
			entry := map[string]any{}
			for k, v := range ch.def {
				entry[k] = v
			}
			if existing, ok := ch.idx.Aliases[ch.alias]; ok {
				for k, v := range entry {
					existing[k] = v
				}
			} else {
				ch.idx.Aliases[ch.alias] = entry
			}
		}
	}
	for _, idx := range removeIndexes {
		delete(s.indices, idx.Name)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

// resolveWriteTarget finds the concrete index a write operation targets.
func (s *Server) resolveWriteTarget(name string) (*Index, *apiError) {
	if idx, ok := s.indices[name]; ok {
		return idx, nil
	}
	var withAlias []*Index
	var writeIdx *Index
	for _, idx := range s.indices {
		if def, ok := idx.Aliases[name]; ok {
			withAlias = append(withAlias, idx)
			if b, _ := def["is_write_index"].(bool); b {
				writeIdx = idx
			}
		}
	}
	if len(withAlias) > 0 {
		if writeIdx != nil {
			return writeIdx, nil
		}
		if len(withAlias) == 1 {
			def := withAlias[0].Aliases[name]
			if v, has := def["is_write_index"]; has {
				if b, _ := v.(bool); !b {
					return nil, errNoWriteIndex(name)
				}
			}
			return withAlias[0], nil
		}
		return nil, errNoWriteIndex(name)
	}
	if idx, err := s.maybeAutoCreateDataStream(name); idx != nil || err != nil {
		return idx, err
	}
	if verr := validateIndexName(name); verr != nil {
		return nil, verr
	}
	return nil, errIndexNotFoundNamed(name)
}

// aliasRoutingFor returns the index_routing configured for name-as-alias on idx.
func aliasRoutingFor(idx *Index, aliasName string) string {
	if idx.Name == aliasName {
		return ""
	}
	def, ok := idx.Aliases[aliasName]
	if !ok {
		return ""
	}
	if r, ok := def["index_routing"].(string); ok {
		return r
	}
	return ""
}

// searchAliasFilter returns the filter configured on the alias name for idx.
func searchAliasFilter(idx *Index, aliasName string) map[string]any {
	if idx.Name == aliasName {
		return nil
	}
	def, ok := idx.Aliases[aliasName]
	if !ok {
		return nil
	}
	if f, ok := def["filter"].(map[string]any); ok {
		return f
	}
	return nil
}
