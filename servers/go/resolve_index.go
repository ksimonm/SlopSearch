package main

import (
	"net/http"
	"sort"
	"strings"
)

// handleResolveIndex implements GET /_resolve/index/{spec}.
func (s *Server) handleResolveIndex(w http.ResponseWriter, r *http.Request, spec string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET", "HEAD")
		return
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	expand := defaultExpand()
	if v := r.URL.Query().Get("expand_wildcards"); v != "" {
		if set, err := parseExpandWildcards(v, nil); err == nil {
			expand = set
		}
	}

	seenIdx := map[string]bool{}
	var idxNames []string
	seenAlias := map[string]map[string]bool{}
	addAlias := func(aliasName, indexName string) {
		if seenAlias[aliasName] == nil {
			seenAlias[aliasName] = map[string]bool{}
		}
		seenAlias[aliasName][indexName] = true
	}
	seenDS := map[string]bool{}

	for _, tok := range strings.Split(spec, ",") {
		token := strings.TrimSpace(tok)
		if token == "" {
			continue
		}
		wildcard := token == "_all" || token == "*" || strings.ContainsAny(token, "*?")
		if wildcard {
			for _, name := range sortedIndexNames(s) {
				idx := s.indices[name]
				if !globMatch(token, name) && token != "_all" && token != "*" {
					continue
				}
				if !acceptClass(idx, expand) {
					continue
				}
				if !seenIdx[name] {
					seenIdx[name] = true
					idxNames = append(idxNames, name)
				}
			}
			// aliases whose target indices all pass the expansion filter
			for _, name := range sortedIndexNames(s) {
				idx := s.indices[name]
				if !acceptClass(idx, expand) {
					continue
				}
				for alias := range idx.Aliases {
					if globMatch(token, alias) || token == "_all" || token == "*" {
						addAlias(alias, name)
					}
				}
			}
			for _, dsName := range sortedDataStreamNames(s) {
				if globMatch(token, dsName) || token == "_all" || token == "*" {
					seenDS[dsName] = true
				}
			}
		} else {
			if idx, ok := s.indices[token]; ok {
				if acceptClass(idx, expand) && !seenIdx[token] {
					seenIdx[token] = true
					idxNames = append(idxNames, token)
				}
			} else {
				for _, name := range sortedIndexNames(s) {
					idx := s.indices[name]
					if _, ok := idx.Aliases[token]; ok {
						addAlias(token, name)
					}
				}
				if ds := s.resolveDataStream(token); ds != nil {
					seenDS[token] = true
				}
			}
		}
	}

	sort.Strings(idxNames)
	indicesOut := []any{}
	for _, name := range idxNames {
		idx := s.indices[name]
		entry := map[string]any{"name": name}
		var aliasNames []string
		for alias := range idx.Aliases {
			aliasNames = append(aliasNames, alias)
		}
		if len(aliasNames) > 0 {
			sort.Strings(aliasNames)
			arr := []any{}
			for _, a := range aliasNames {
				arr = append(arr, a)
			}
			entry["aliases"] = arr
		}
		attrs := []any{}
		if idx.isHidden() {
			attrs = append(attrs, "hidden")
		}
		if idx.Closed {
			attrs = append(attrs, "closed")
		} else {
			attrs = append(attrs, "open")
		}
		entry["attributes"] = attrs
		indicesOut = append(indicesOut, entry)
	}

	aliasList := make([]string, 0, len(seenAlias))
	for a := range seenAlias {
		aliasList = append(aliasList, a)
	}
	sort.Strings(aliasList)
	aliasesOut := []any{}
	for _, a := range aliasList {
		var indexNames []string
		for n := range seenAlias[a] {
			indexNames = append(indexNames, n)
		}
		sort.Strings(indexNames)
		arr := []any{}
		for _, n := range indexNames {
			arr = append(arr, n)
		}
		aliasesOut = append(aliasesOut, map[string]any{"name": a, "indices": arr})
	}

	dsList := make([]string, 0, len(seenDS))
	for n := range seenDS {
		dsList = append(dsList, n)
	}
	sort.Strings(dsList)
	dsOut := []any{}
	for _, n := range dsList {
		ds := s.dataStreams[n]
		bi := []any{}
		for _, b := range ds.BackingIndices {
			bi = append(bi, b)
		}
		dsOut = append(dsOut, map[string]any{
			"name":            n,
			"backing_indices": bi,
			"timestamp_field": ds.TimestampField,
		})
	}

	writeJSON(w, 200, map[string]any{
		"indices":      indicesOut,
		"aliases":      aliasesOut,
		"data_streams": dsOut,
	})
}
