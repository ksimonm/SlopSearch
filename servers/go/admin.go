package main

import (
	"fmt"
	"io"
	"net/http"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"unicode"
)

// ---------------------------------------------------------------- name rules

var invalidNameChars = " ,\",*,\\,<,|,,,>,/,?"

func validateIndexName(name string) *apiError {
	return validateName(name, false)
}

func validateAliasName(name string) *apiError {
	return validateName(name, true)
}

func validateName(name string, alias bool) *apiError {
	kind, kindCaps := "index", "index"
	sep := ", "
	if alias {
		kind, kindCaps, sep = "alias", "alias", ": "
	}
	fail := func(reason string) *apiError {
		e := &apiError{Type: "invalid_" + kind + "_name_exception",
			Reason: "Invalid " + kindCaps + " name [" + name + "]" + sep + reason,
			Status: 400}
		if !alias {
			e.Fields = map[string]any{"index": name, "index_uuid": "_na_"}
		}
		return e
	}
	if strings.TrimSpace(name) == "" {
		return fail("must not be empty")
	}
	if name == "." || name == ".." {
		return fail("must not be '.' or '..'")
	}
	if strings.HasPrefix(name, "_") || strings.HasPrefix(name, "-") || strings.HasPrefix(name, "+") {
		return fail("must not start with '_', '-', or '+'")
	}
	if !alias {
		for _, r := range name {
			if unicode.IsUpper(r) {
				return fail("must be lowercase")
			}
		}
	}
	if strings.Contains(name, ":") {
		return fail("must not contain ':'")
	}
	if strings.Contains(name, "#") {
		return fail("must not contain '#'")
	}
	for _, r := range name {
		if strings.ContainsRune("\\/*?\"<>| ,", r) {
			return fail("must not contain the following characters [" + invalidNameChars + "]")
		}
	}
	if !alias {
		if n := len([]byte(name)); n > 255 {
			return fail(fmt.Sprintf("index name is too long, (%d > 255)", n))
		}
	}
	return nil
}

func errIndexNotFoundNamed(name string) *apiError {
	e := errNotFound(name)
	e.Fields = map[string]any{"index": name, "index_uuid": "_na_",
		"resource.type": "index_or_alias", "resource.id": name}
	return e
}

// ------------------------------------------------------------ index settings

var knownSettingRoots = map[string]bool{
	"number_of_shards": true, "number_of_replicas": true, "refresh_interval": true,
	"max_result_window": true, "max_inner_result_window": true, "max_terms_count": true,
	"max_regex_length": true, "max_ngram_diff": true, "max_shingle_diff": true,
	"max_refresh_listeners": true, "max_docvalue_fields_search": true,
	"max_script_fields": true, "max_analyzed_offset": true, "max_auto_expand_replicas": true,
	"hidden": true, "blocks": true, "analysis": true, "mapping": true, "routing": true,
	"default_pipeline": true, "final_pipeline": true, "auto_expand_replicas": true,
	"codec": true, "soft_deletes": true, "translog": true, "merge": true, "write": true,
	"sort": true, "shard": true, "queries": true, "requests": true, "store": true,
	"warmer": true, "gc_deletes": true, "search": true, "query": true, "highlight": true,
	"fielddata": true, "similarity": true, "replication": true, "knn": true,
	"plugins": true, "engine": true, "compound_format": true,
	"number_of_routing_shards": true, "routing_partition_size": true,
	"uuid": true, "version": true, "provided_name": true, "creation_date": true,
	"resize": true, "verified_before_close": true, "data_path": true,
	"load_fixed_bitset_filters_eagerly": true, "eager_global_ordinals": true,
	"priority": true, "allocation": true, "unassigned": true, "recovery": true,
}

func isKnownIndexSetting(key string) bool {
	if !strings.HasPrefix(key, "index.") {
		return false
	}
	rest := strings.TrimPrefix(key, "index.")
	if rest == "" {
		return false
	}
	root := rest
	if i := strings.Index(rest, "."); i >= 0 {
		root = rest[:i]
	}
	return knownSettingRoots[root]
}

var timeSettingPattern = `^-?[0-9]+(ms|s|m|h|d)?$`

func validateSettingValue(key string, raw any, creating bool) (string, *apiError) {
	rest := strings.TrimPrefix(key, "index.")
	val := settingsValueString(raw)
	switch {
	case rest == "number_of_shards":
		n, ok := settingsValueInt(raw)
		if !ok {
			e := errBad("Failed to parse value [" + val + "] for setting [index.number_of_shards]")
			e.CausedBy = &apiError{Type: "number_format_exception", Reason: "For input string: \"" + val + "\""}
			return "", e
		}
		if n < 1 {
			return "", errBad("Failed to parse value [" + val + "] for setting [index.number_of_shards] must be >= 1")
		}
		if n > 1024 {
			return "", errBad("Failed to parse value [" + val + "] for setting [index.number_of_shards] must be <= 1024")
		}
		return strconv.Itoa(n), nil
	case rest == "number_of_replicas":
		n, ok := settingsValueInt(raw)
		if !ok {
			e := errBad("Failed to parse value [" + val + "] for setting [index.number_of_replicas]")
			e.CausedBy = &apiError{Type: "number_format_exception", Reason: "For input string: \"" + val + "\""}
			return "", e
		}
		if n < 0 {
			return "", errBad("Failed to parse value [" + val + "] for setting [index.number_of_replicas] must be >= 0")
		}
		if n > 1023 {
			return "", errBad("Failed to parse value [" + val + "] for setting [index.number_of_replicas] must be <= 1023")
		}
		return strconv.Itoa(n), nil
	case rest == "refresh_interval":
		if !regexpMatchString(timeSettingPattern, val) {
			return "", errBad("failed to parse setting [index.refresh_interval] with value [" + val + "] as a time value: unit is missing or unrecognized")
		}
		return val, nil
	case rest == "max_result_window" || rest == "max_inner_result_window" ||
		rest == "max_terms_count" || rest == "max_regex_length" || rest == "max_ngram_diff" ||
		rest == "max_shingle_diff" || rest == "max_refresh_listeners":
		n, ok := settingsValueInt(raw)
		if !ok {
			return "", errBad("Failed to parse value [" + val + "] for setting [" + key + "]")
		}
		if (rest == "max_result_window" || rest == "max_inner_result_window") && n < 1 {
			return "", errBad("Failed to parse value [" + val + "] for setting [" + key + "] must be >= 1")
		}
		return strconv.Itoa(n), nil
	case rest == "hidden" || strings.HasPrefix(rest, "blocks."):
		if b, ok := raw.(bool); ok {
			return strconv.FormatBool(b), nil
		}
		if val == "true" || val == "false" {
			return val, nil
		}
		return "", errBad("Failed to parse value [" + val + "] for setting [" + key + "]")
	}
	return val, nil
}

func regexpMatchString(pattern, s string) bool {
	re, err := regexp.Compile(pattern)
	if err != nil {
		return false
	}
	return re.MatchString(s)
}

// flattenSettings turns the various settings forms into canonical dotted keys
// prefixed with "index.". Nested maps are flattened; null values are kept
// (they mean "reset" on updates).
func flattenSettings(raw map[string]any, out map[string]any) {
	for k, v := range raw {
		key := k
		if key != "index" && !strings.HasPrefix(key, "index.") {
			key = "index." + key
		}
		flattenSettingValue(key, v, out)
	}
}

func flattenSettingValue(key string, v any, out map[string]any) {
	if m, ok := v.(map[string]any); ok {
		for k, vv := range m {
			flattenSettingValue(key+"."+k, vv, out)
		}
		return
	}
	out[key] = v
}

// canonicalBodySettings extracts the settings map from an update body: the
// body itself is the settings map (either flat, dotted or nested under index).
func canonicalBodySettings(body map[string]any) map[string]any {
	inner := body
	if len(body) == 1 {
		if s, ok := body["settings"].(map[string]any); ok {
			inner = s
		}
	}
	out := map[string]any{}
	flattenSettings(inner, out)
	return out
}

var defaultSettingsFlat = map[string]any{
	"index.number_of_shards":                     "1",
	"index.number_of_replicas":                   "1",
	"index.auto_expand_replicas":                 "false",
	"index.blocks.metadata":                      "false",
	"index.blocks.read":                          "false",
	"index.blocks.read_only":                     "false",
	"index.blocks.read_only_allow_delete":        "false",
	"index.blocks.write":                         "false",
	"index.compound_format":                      "false",
	"index.default_pipeline":                     "_none",
	"index.final_pipeline":                       "_none",
	"index.eager_global_ordinals":                "false",
	"index.fielddata.cache":                      "node",
	"index.flush_threshold_size":                 "512mb",
	"index.gc_deletes":                           "60s",
	"index.hidden":                               "false",
	"index.highlight.max_analyzed_offset":        "1000000",
	"index.indexing.slowlog.level":               "info",
	"index.load_fixed_bitset_filters_eagerly":    "true",
	"index.mapping.coerce":                       "true",
	"index.mapping.depth.limit":                  "20",
	"index.mapping.field_name_length.limit":      "9223372036854775807",
	"index.mapping.ignore_malformed":             "false",
	"index.mapping.nested_fields.limit":          "50",
	"index.mapping.nested_objects.limit":         "10000",
	"index.mapping.total_fields.limit":           "1000",
	"index.max_adjacency_matrix_filters":         "100",
	"index.max_docvalue_fields_search":           "100",
	"index.max_inner_result_window":              "100",
	"index.max_ngram_diff":                       "1",
	"index.max_refresh_listeners":                "1000",
	"index.max_regex_length":                     "1000",
	"index.max_result_window":                    "10000",
	"index.max_script_fields":                    "32",
	"index.max_shingle_diff":                     "3",
	"index.max_slices_per_scroll":                "1024",
	"index.max_terms_count":                      "65536",
	"index.merge.policy.deletes_pct_allowed":     "33.0",
	"index.merge.policy.expunge_deletes_allowed": "10.0",
	"index.merge.policy.floor_segment":           "2mb",
	"index.merge.policy.max_merge_at_once":       "10",
	"index.merge.policy.max_merged_segment":      "5gb",
	"index.merge.policy.segments_per_tier":       "10",
	"index.merge.scheduler.max_thread_count":     "4",
	"index.query.default_field":                  []any{"*"},
	"index.query.parse.allow_unmapped_fields":    "true",
	"index.queries.cache.enabled":                "true",
	"index.queries.cache.size":                   "10%",
	"index.refresh_interval":                     "1s",
	"index.requests.cache.enable":                "true",
	"index.routing.allocation.enable":            "all",
	"index.search.default_pipeline":              "_none",
	"index.search.idle.after":                    "30s",
	"index.search.slowlog.level":                 "TRACE",
	"index.soft_deletes.enabled":                 "true",
	"index.store.preload":                        []any{},
	"index.store.type":                           "fs",
	"index.translog.durability":                  "REQUEST",
	"index.translog.flush_threshold_size":        "512mb",
	"index.translog.retention.age":               "12h",
	"index.translog.retention.size":              "-1",
	"index.translog.sync_interval":               "5s",
	"index.warmer.enabled":                       "true",
	"index.write.wait_for_active_shards":         "1",
}

func settingsSetNested(root map[string]any, path []string, v any) {
	m := root
	for _, p := range path[:len(path)-1] {
		next, ok := m[p].(map[string]any)
		if !ok {
			next = map[string]any{}
			m[p] = next
		}
		m = next
	}
	m[path[len(path)-1]] = v
}

func settingsToNested(flat map[string]any) map[string]any {
	root := map[string]any{}
	keys := sortedKeys(flat)
	for _, k := range keys {
		settingsSetNested(root, strings.Split(k, "."), flat[k])
	}
	return root
}

func (idx *Index) materializedFlat() map[string]any {
	out := map[string]any{}
	for k, v := range idx.Settings {
		out[k] = stringifyScalar(v)
	}
	if _, ok := out["index.number_of_shards"]; !ok {
		out["index.number_of_shards"] = strconv.Itoa(idx.numShards())
	}
	if _, ok := out["index.number_of_replicas"]; !ok {
		out["index.number_of_replicas"] = strconv.Itoa(idx.numReplicas())
	}
	out["index.uuid"] = idx.UUID
	out["index.creation_date"] = strconv.FormatInt(idx.CreationDate, 10)
	out["index.provided_name"] = idx.Name
	out["index.version.created"] = versionCreated
	out["index.replication.type"] = "DOCUMENT"
	if idx.Closed || idx.Verified {
		out["index.verified_before_close"] = "true"
	}
	return out
}

func matchAnySettingPattern(patterns []string, key string) bool {
	if len(patterns) == 0 {
		return true
	}
	for _, p := range patterns {
		if p == "" {
			continue
		}
		if globMatch(p, key) || strings.HasPrefix(key, strings.TrimSuffix(p, "*")) && strings.HasSuffix(p, "*") {
			return true
		}
	}
	return false
}

func parseSettingsFilter(filter string) []string {
	if filter == "" {
		return nil
	}
	parts := strings.Split(filter, ",")
	out := []string{}
	for _, p := range parts {
		p = strings.TrimSpace(p)
		if p != "" {
			out = append(out, p)
		}
	}
	return out
}

// settingsView renders one index's settings for GET responses. Returns nil
// when an explicit filter matched nothing.
func settingsView(idx *Index, flat, includeDefaults bool, filter string) map[string]any {
	patterns := parseSettingsFilter(filter)
	flatAll := idx.materializedFlat()
	matched := map[string]any{}
	for k, v := range flatAll {
		if matchAnySettingPattern(patterns, k) {
			matched[k] = v
		}
	}
	var defaultsMatched map[string]any
	if includeDefaults {
		defaultsMatched = map[string]any{}
		for k, v := range defaultSettingsFlat {
			if _, isSet := flatAll[k]; isSet {
				continue
			}
			if matchAnySettingPattern(patterns, k) {
				defaultsMatched[k] = v
			}
		}
	}
	if len(matched) == 0 && (defaultsMatched == nil || len(defaultsMatched) == 0) {
		return nil
	}
	out := map[string]any{}
	if flat {
		out["settings"] = matched
		if includeDefaults {
			d := map[string]any{}
			for k, v := range defaultsMatched {
				d[k] = v
			}
			out["defaults"] = d
		}
	} else {
		out["settings"] = settingsToNested(matched)
		if includeDefaults {
			out["defaults"] = settingsToNested(defaultsMatched)
		}
	}
	return out
}

// ------------------------------------------------------------ spec resolution

type resolveOpts2 struct {
	expand            map[string]bool
	allowNoIndices    *bool
	ignoreUnavailable bool
	rejectAlias       bool
	closedError       bool
}

func defaultExpand() map[string]bool { return map[string]bool{"open": true} }

func parseExpandWildcards(v string, def map[string]bool) (map[string]bool, *apiError) {
	if v == "" {
		return def, nil
	}
	if v == "none" {
		return map[string]bool{}, nil
	}
	out := map[string]bool{}
	for _, p := range strings.Split(v, ",") {
		p = strings.TrimSpace(p)
		switch p {
		case "open", "closed", "hidden":
			out[p] = true
		case "all":
			return map[string]bool{"open": true, "closed": true, "hidden": true}, nil
		case "":
		default:
			return nil, errBad("No valid expand wildcard value [" + p + "]")
		}
	}
	return out, nil
}

func acceptClass(idx *Index, expand map[string]bool) bool {
	if idx.Closed {
		if !expand["closed"] {
			return false
		}
	} else if !expand["open"] {
		return false
	}
	if idx.isHidden() && !expand["hidden"] {
		return false
	}
	return true
}

func errIndexClosed(idx *Index) *apiError {
	return &apiError{Type: "index_closed_exception", Reason: "closed", Status: 400,
		Fields: map[string]any{"index": idx.Name, "index_uuid": idx.UUID}}
}

// resolveSpec2 resolves an index expression (comma list, wildcards, exclusions,
// aliases) according to OpenSearch semantics.
func (s *Server) resolveSpec2(spec string, o resolveOpts2) ([]*Index, *apiError) {
	if spec == "" {
		spec = "*"
	}
	expand := o.expand
	if expand == nil {
		expand = defaultExpand()
	}
	allowNo := true
	if o.allowNoIndices != nil {
		allowNo = *o.allowNoIndices
	}
	seen := map[string]*Index{}
	var order []string
	add := func(idx *Index) {
		if _, ok := seen[idx.Name]; !ok {
			seen[idx.Name] = idx
			order = append(order, idx.Name)
		}
	}
	var remove func(idx *Index)
	remove = func(idx *Index) {
		delete(seen, idx.Name)
	}
	for _, tok := range strings.Split(spec, ",") {
		token := strings.TrimSpace(tok)
		if token == "" {
			continue
		}
		exclude := false
		if strings.HasPrefix(token, "-") {
			exclude = true
			token = token[1:]
		}
		var matched []*Index
		var apiErr2 *apiError
		if token == "_all" || token == "*" {
			for _, name := range sortedIndexNames(s) {
				idx := s.indices[name]
				if acceptClass(idx, expand) {
					matched = append(matched, idx)
				}
			}
		} else if strings.ContainsAny(token, "*?") {
			for _, name := range sortedIndexNames(s) {
				idx := s.indices[name]
				if globMatch(token, name) && acceptClass(idx, expand) {
					matched = append(matched, idx)
				}
			}
			if len(matched) == 0 {
				for _, name := range sortedIndexNames(s) {
					idx := s.indices[name]
					if !acceptClass(idx, expand) {
						continue
					}
					for alias := range idx.Aliases {
						if globMatch(token, alias) {
							matched = append(matched, idx)
							break
						}
					}
				}
			}
			if len(matched) == 0 && !allowNo {
				apiErr2 = errIndexNotFoundNamed(token)
			}
		} else {
			if idx, ok := s.indices[token]; ok {
				matched = append(matched, idx)
			} else {
				aliasHit := false
				for _, name := range sortedIndexNames(s) {
					idx := s.indices[name]
					if _, ok := idx.Aliases[token]; ok {
						matched = append(matched, idx)
						aliasHit = true
					}
				}
				if aliasHit && o.rejectAlias {
					return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
						Reason: "The provided expression [" + token + "] matches an alias, specify the corresponding concrete indices instead."}
				}
				if !aliasHit {
					if ds := s.resolveDataStream(token); ds != nil && !o.rejectAlias {
						matched = s.dataStreamIndices(ds)
					} else if o.ignoreUnavailable {
						continue
					} else {
						apiErr2 = errIndexNotFoundNamed(token)
					}
				}
			}
		}
		if apiErr2 != nil {
			if o.ignoreUnavailable {
				continue
			}
			return nil, apiErr2
		}
		if o.closedError {
			open := matched[:0]
			for _, idx := range matched {
				if idx.Closed {
					if o.ignoreUnavailable {
						continue
					}
					return nil, errIndexClosed(idx)
				}
				open = append(open, idx)
			}
			matched = open
		}
		for _, idx := range matched {
			if exclude {
				remove(idx)
			} else {
				add(idx)
			}
		}
	}
	sort.Strings(order)
	out := make([]*Index, 0, len(order))
	for _, name := range order {
		if idx, ok := seen[name]; ok {
			out = append(out, idx)
		}
	}
	return out, nil
}

func (s *Server) parseResolveParams(r *http.Request, o resolveOpts2) resolveOpts2 {
	q := r.URL.Query()
	if v, ok := q["expand_wildcards"]; ok && len(v) > 0 {
		if set, err := parseExpandWildcards(v[0], nil); err == nil {
			o.expand = set
		}
	}
	if v, ok := q["allow_no_indices"]; ok && len(v) > 0 {
		b := v[0] == "" || v[0] == "true"
		o.allowNoIndices = &b
	}
	if v, ok := q["ignore_unavailable"]; ok && len(v) > 0 {
		o.ignoreUnavailable = v[0] == "" || v[0] == "true"
	}
	return o
}

// ------------------------------------------------------------ create / index

func (s *Server) createIndex(w http.ResponseWriter, r *http.Request, name string) {
	if err := validateIndexName(name); err != nil {
		writeError(w, err)
		return
	}
	bodyBytes, _ := io.ReadAll(r.Body)
	var body map[string]any
	if len(strings.TrimSpace(string(bodyBytes))) > 0 {
		if err := unmarshalJSON(bodyBytes, &body); err != nil {
			writeError(w, &apiError{Type: "not_x_content_exception", Status: 400,
				Reason: "Compressor detection can only be called on some xcontent bytes or compressed xcontent bytes"})
			return
		}
	} else {
		body = map[string]any{}
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, apiErr2 := s.createIndexInternal(name, body, false); apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	writeJSON(w, 200, map[string]any{"acknowledged": true, "shards_acknowledged": true, "index": name})
}

// createIndexInternal builds and registers a new index (s.mu must be held).
// When isBacking is true, template composition is skipped (the backing body
// was built from the data stream template already).
func (s *Server) createIndexInternal(name string, body map[string]any, isBacking bool) (*Index, *apiError) {
	if _, exists := s.indices[name]; exists {
		return nil, &apiError{Type: "resource_already_exists_exception", Status: 400,
			Reason: "index [" + name + "]", Fields: map[string]any{"index": name}}
	}
	for _, idx := range s.indices {
		if _, ok := idx.Aliases[name]; ok {
			return nil, &apiError{Type: "invalid_index_name_exception", Status: 400,
				Reason: "Invalid index name [" + name + "], already exists as alias",
				Fields: map[string]any{"index": name, "index_uuid": "_na_"}}
		}
	}
	for k := range body {
		switch k {
		case "settings", "mappings", "aliases":
		default:
			return nil, &apiError{Type: "parse_exception", Status: 400,
				Reason: "unknown key [" + k + "] for create index"}
		}
	}
	var composed *composedTemplate
	if !isBacking {
		composed = s.composeForIndex(name)
		if composed.dsDef != nil {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "cannot create index with name [" + name + "], because it matches with template [" + composed.template + "] that creates data streams only, use create data stream api instead"}
		}
	}
	settingsRaw, _ := body["settings"].(map[string]any)
	canonical := map[string]any{}
	if composed != nil {
		for k, v := range composed.settings {
			canonical[k] = v
		}
	}
	flattenSettings(settingsRaw, canonical)
	for k, v := range canonical {
		if v == nil {
			delete(canonical, k)
			continue
		}
		if !isKnownIndexSetting(k) {
			return nil, &apiError{Type: "settings_exception", Status: 400,
				Reason: "unknown setting [" + k + "] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"}
		}
		val, err := validateSettingValue(k, v, true)
		if err != nil {
			return nil, err
		}
		canonical[k] = val
	}
	if _, ok := canonical["index.number_of_shards"]; !ok {
		canonical["index.number_of_shards"] = "1"
	}
	if _, ok := canonical["index.number_of_replicas"]; !ok {
		canonical["index.number_of_replicas"] = "1"
	}

	mappingsRaw, _ := body["mappings"].(map[string]any)
	if mappingsRaw != nil {
		for k, v := range mappingsRaw {
			if k == "properties" {
				continue
			}
			if _, isMap := v.(map[string]any); isMap && k == "_doc" {
				return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "The mapping definition cannot be nested under a type"}
			}
		}
	}
	mergedMappings := map[string]any{}
	if composed != nil {
		mergeMappingsDeep(mergedMappings, composed.mappings)
	}
	if mappingsRaw != nil {
		mergeMappingsDeep(mergedMappings, mappingsRaw)
	}
	if err := validateMappingFull(mergedMappings); err != nil {
		return nil, err
	}

	aliasesRaw, _ := body["aliases"].(map[string]any)
	aliases := map[string]map[string]any{}
	addAlias := func(aliasName string, defRaw any) *apiError {
		if err := validateAliasName(aliasName); err != nil {
			return err
		}
		if _, clash := s.indices[aliasName]; clash {
			return &apiError{Type: "invalid_alias_name_exception", Status: 400,
				Reason: "Invalid alias name [" + aliasName + "], an index exists with the same name as the alias"}
		}
		def, _ := defRaw.(map[string]any)
		if def == nil {
			def = map[string]any{}
		}
		if filt, ok := def["filter"]; ok && filt != nil {
			if fq, ok := filt.(map[string]any); ok {
				if err := validateQuery(&Index{FieldTypes: map[string]string{}}, fq); err != nil {
					return &apiError{Type: "illegal_argument_exception", Status: 400,
						Reason:   "failed to parse filter for alias [" + aliasName + "]",
						CausedBy: &apiError{Type: "parsing_exception", Reason: err.Reason}}
				}
			}
		}
		aliases[aliasName] = normalizeAliasDef(def)
		return nil
	}
	if composed != nil {
		for aliasName, defRaw := range composed.aliases {
			if err := addAlias(aliasName, defRaw); err != nil {
				return nil, err
			}
		}
	}
	for aliasName, defRaw := range aliasesRaw {
		if err := addAlias(aliasName, defRaw); err != nil {
			return nil, err
		}
	}

	idx := &Index{
		Name:         name,
		UUID:         randomUUID(),
		CreationDate: nowMillis(),
		Mappings:     mergedMappings,
		Settings:     canonical,
		Docs:         map[string]map[string]any{},
		FieldTypes:   map[string]string{},
		Aliases:      aliases,
		Pending:      map[string]map[string]any{},
		Routing:      map[string]string{},
	}
	ensureFieldTypes(idx)
	s.indices[name] = idx
	s.bumpState()
	return idx, nil
}

func normalizeAliasDef(def map[string]any) map[string]any {
	out := map[string]any{}
	for k, v := range def {
		out[k] = v
	}
	if r, ok := out["routing"]; ok && r != nil {
		if _, has := out["index_routing"]; !has {
			out["index_routing"] = r
		}
		if _, has := out["search_routing"]; !has {
			out["search_routing"] = r
		}
		delete(out, "routing")
	}
	return out
}

func validateMappingFull(mappings map[string]any) *apiError {
	props, ok := mappings["properties"].(map[string]any)
	if !ok {
		return nil
	}
	if err := validateProps(props); err != nil {
		reason := err.Error()
		return &apiError{Type: "mapper_parsing_exception", Status: 400,
			Reason:   "Failed to parse mapping [_doc]: " + reason,
			CausedBy: &apiError{Type: "mapper_parsing_exception", Reason: reason}}
	}
	return nil
}

// -------------------------------------------------------------- get / delete

func (s *Server) getIndexMeta(w http.ResponseWriter, r *http.Request, spec string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	if r.Method == http.MethodHead {
		w.Header().Set("Content-Type", "application/json; charset=UTF-8")
		if len(indices) > 0 || strings.ContainsAny(spec, "*?") || spec == "_all" {
			w.WriteHeader(200)
		} else {
			w.WriteHeader(404)
		}
		return
	}
	flat := paramBool(r, "flat_settings")
	out := map[string]any{}
	for _, idx := range indices {
		aliasDefs := map[string]any{}
		for an, def := range idx.Aliases {
			entry := map[string]any{}
			for k, v := range def {
				entry[k] = v
			}
			aliasDefs[an] = entry
		}
		var settings map[string]any
		if flat {
			settings = idx.materializedFlat()
		} else {
			settings = settingsToNested(idx.materializedFlat())
		}
		mappings := idx.Mappings
		if mappings == nil {
			mappings = map[string]any{}
		}
		entry := map[string]any{
			"aliases":  aliasDefs,
			"mappings": mappings,
			"settings": settings,
		}
		if idx.DataStream != "" {
			entry["data_stream"] = idx.DataStream
		}
		out[idx.Name] = entry
	}
	writeJSON(w, 200, out)
}

func (s *Server) deleteIndices(w http.ResponseWriter, r *http.Request, spec string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{rejectAlias: true})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	for _, idx := range indices {
		if idx.DataStream != "" {
			if ds := s.resolveDataStream(idx.DataStream); ds != nil && len(ds.BackingIndices) > 0 &&
				ds.BackingIndices[len(ds.BackingIndices)-1] == idx.Name {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "index [" + idx.Name + "] is the write index for data stream [" + idx.DataStream + "] and cannot be deleted"})
				return
			}
		}
		if err := indexBlockError(idx, "metadata", 9, 403); err != nil {
			writeError(w, err)
			return
		}
		if err := indexBlockError(idx, "read_only", 5, 403); err != nil {
			writeError(w, err)
			return
		}
	}
	for _, idx := range indices {
		delete(s.indices, idx.Name)
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

func indexBlockError(idx *Index, block string, code int, status int) *apiError {
	if !idx.blockOn(block) {
		return nil
	}
	return &apiError{Type: "cluster_block_exception", Status: status,
		Reason: fmt.Sprintf("index [%s] blocked by: [FORBIDDEN/%d/index %s (api)];", idx.Name, code, strings.ReplaceAll(block, "_", "-"))}
}

// ---------------------------------------------------------------- settings

func (s *Server) getSettingsAPI(w http.ResponseWriter, r *http.Request, spec, filter string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(spec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	flat := paramBool(r, "flat_settings")
	includeDefaults := paramBool(r, "include_defaults")
	out := map[string]any{}
	for _, idx := range indices {
		if err := metaReadBlock(idx); err != nil {
			writeError(w, err)
			return
		}
		view := settingsView(idx, flat, includeDefaults, filter)
		if view == nil {
			continue
		}
		out[idx.Name] = view
	}
	writeJSON(w, 200, out)
}

func metaReadBlock(idx *Index) *apiError {
	return indexBlockError(idx, "metadata", 9, 403)
}

func (s *Server) putSettingsAPI(w http.ResponseWriter, r *http.Request, spec string) {
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	updates := canonicalBodySettings(body)
	if len(updates) == 0 {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: no settings to update;"})
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	if o.allowNoIndices == nil {
		f := false
		o.allowNoIndices = &f
	}
	indices, apiErr3 := s.resolveSpec2(spec, o)
	if apiErr3 != nil {
		writeError(w, apiErr3)
		return
	}
	preserve := paramBool(r, "preserve_existing")
	onlyBlocks := true
	for k := range updates {
		if !strings.HasPrefix(k, "index.blocks.") {
			onlyBlocks = false
			break
		}
	}
	for _, idx := range indices {
		if !onlyBlocks {
			if err := indexBlockError(idx, "read_only", 5, 403); err != nil {
				writeError(w, err)
				return
			}
			if err := metaReadBlock(idx); err != nil {
				writeError(w, err)
				return
			}
		}
		var nonDynamic []string
		var closedOnly []string
		for k, v := range updates {
			if v == nil || preserve && settingPresent(idx, k) {
				continue
			}
			if !isKnownIndexSetting(k) && !strings.HasPrefix(k, "index.analysis.") {
				writeError(w, &apiError{Type: "settings_exception", Status: 400,
					Reason: "unknown setting [" + k + "] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"})
				return
			}
			if k == "index.number_of_shards" {
				nonDynamic = append(nonDynamic, k)
				continue
			}
			if strings.HasPrefix(k, "index.analysis.") {
				closedOnly = append(closedOnly, k)
				continue
			}
			if _, err := validateSettingValue(k, v, false); err != nil {
				writeError(w, err)
				return
			}
		}
		sort.Strings(nonDynamic)
		sort.Strings(closedOnly)
		if len(nonDynamic) > 0 {
			if idx.Closed {
				writeError(w, &apiError{Type: "settings_exception", Status: 400,
					Reason: "Validation Failed: 1: setting [" + nonDynamic[0] + "], not updateable;"})
			} else {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Can't update non dynamic settings [[" + strings.Join(nonDynamic, ", ") + "]] for open indices [[" + idx.Name + "/" + idx.UUID + "]]"})
			}
			return
		}
		if len(closedOnly) > 0 && !idx.Closed {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Can't update non dynamic settings [[" + strings.Join(closedOnly, ", ") + "]] for open indices [[" + idx.Name + "/" + idx.UUID + "]]"})
			return
		}
		for k, v := range updates {
			if preserve && settingPresent(idx, k) {
				continue
			}
			if v == nil {
				delete(idx.Settings, k)
				continue
			}
			if strings.HasPrefix(k, "index.analysis.") {
				idx.Settings[k] = v
				continue
			}
			val, _ := validateSettingValue(k, v, false)
			idx.Settings[k] = val
		}
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

func settingPresent(idx *Index, key string) bool {
	_, ok := idx.Settings[key]
	return ok
}

var validBlockNames = map[string]bool{
	"write": true, "read": true, "read_only": true,
	"read_only_allow_delete": true, "metadata": true,
}

func (s *Server) putBlockAPI(w http.ResponseWriter, r *http.Request, indexSpec, block string) {
	if !validBlockNames[block] {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No block found with name " + block})
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	o := s.parseResolveParams(r, resolveOpts2{})
	indices, apiErr2 := s.resolveSpec2(indexSpec, o)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	arr := []any{}
	for _, idx := range indices {
		idx.Settings["index.blocks."+block] = "true"
		arr = append(arr, map[string]any{"name": idx.Name, "blocked": true})
	}
	s.bumpState()
	writeJSON(w, 200, map[string]any{
		"acknowledged": true, "shards_acknowledged": true, "indices": arr,
	})
}
