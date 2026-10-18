package main

import (
	"net/http"
	"sort"
	"strconv"
	"strings"
)

// ---------------------------------------------------------------- helpers

// templateSettingsCanonical converts a raw template "settings" object into the
// canonical flat form used by Index.Settings (keys prefixed "index.").
func templateSettingsCanonical(raw any) map[string]any {
	out := map[string]any{}
	if m, ok := raw.(map[string]any); ok {
		flat := map[string]any{}
		flattenSettings(m, flat)
		for k, v := range flat {
			out[k] = settingsValueString(v)
		}
	}
	return out
}

// templateSettingsView renders settings for GET responses: {"index": {...}}.
func templateSettingsView(raw any) map[string]any {
	m, ok := raw.(map[string]any)
	if !ok || len(m) == 0 {
		return map[string]any{}
	}
	canon := templateSettingsCanonical(m)
	inner := map[string]any{}
	for k, v := range canon {
		inner[strings.TrimPrefix(k, "index.")] = settingsValueString(v)
	}
	return map[string]any{"index": inner}
}

func normalizePatterns(v any) []any {
	switch t := v.(type) {
	case string:
		return []any{t}
	case []any:
		return t
	case []string:
		out := make([]any, 0, len(t))
		for _, s := range t {
			out = append(out, s)
		}
		return out
	}
	return nil
}

func patternsOverlap(a, b string) bool {
	if a == b {
		return true
	}
	if !strings.ContainsAny(a, "*?") && !strings.ContainsAny(b, "*?") {
		return false
	}
	// Simple prefix heuristic: compare up to the first wildcard character.
	prefix := func(p string) string {
		i := strings.IndexAny(p, "*?")
		if i < 0 {
			return p
		}
		return p[:i]
	}
	pa, pb := prefix(a), prefix(b)
	if pa == "" || pb == "" {
		return true
	}
	return strings.HasPrefix(pa, pb) || strings.HasPrefix(pb, pa)
}

func templateMatches(tpl map[string]any, name string) bool {
	for _, pat := range normalizePatterns(tpl["index_patterns"]) {
		if ps, ok := pat.(string); ok && globMatch(ps, name) {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------- index templates

var knownIndexTemplateFields = map[string]bool{
	"index_patterns": true, "template": true, "composed_of": true,
	"priority": true, "version": true, "_meta": true, "data_stream": true,
}

func validatePatternList(patterns []any) *apiError {
	for _, p := range patterns {
		ps, ok := p.(string)
		if !ok || ps == "" {
			return &apiError{Type: "invalid_index_template_exception", Status: 400,
				Reason: "index_pattern [" + settingsValueString(p) + "] must not be empty"}
		}
		if strings.HasPrefix(ps, "_") || strings.HasPrefix(ps, "-") || strings.HasPrefix(ps, "+") {
			return &apiError{Type: "invalid_index_template_exception", Status: 400,
				Reason: "index_pattern [" + ps + "] must not start with '_', '-', or '+'. Found your index pattern [" + ps + "]"}
		}
	}
	return nil
}

func indexTemplateGetView(raw map[string]any) map[string]any {
	out := map[string]any{}
	out["index_patterns"] = normalizePatterns(raw["index_patterns"])
	tv := map[string]any{}
	if tpl, ok := raw["template"].(map[string]any); ok {
		if s, ok := tpl["settings"]; ok {
			tv["settings"] = templateSettingsView(s)
		}
		if m, ok := tpl["mappings"]; ok && m != nil {
			if mm, ok := m.(map[string]any); !ok || len(mm) > 0 {
				tv["mappings"] = m
			}
		}
		if a, ok := tpl["aliases"]; ok && a != nil {
			if am, ok := a.(map[string]any); ok && len(am) > 0 {
				tv["aliases"] = am
			}
		}
	}
	if len(tv) > 0 {
		out["template"] = tv
	}
	if c, ok := raw["composed_of"]; ok {
		out["composed_of"] = c
	}
	if p, ok := raw["priority"]; ok {
		out["priority"] = p
	}
	if v, ok := raw["version"]; ok {
		out["version"] = v
	}
	if m, ok := raw["_meta"]; ok {
		out["_meta"] = m
	}
	if d, ok := raw["data_stream"]; ok {
		out["data_stream"] = d
	}
	return out
}

func priorityOf(raw map[string]any) int {
	if n, ok := settingsValueInt(raw["priority"]); ok {
		return n
	}
	return 0
}

func sortedNames(m map[string]map[string]any) []string {
	names := make([]string, 0, len(m))
	for n := range m {
		names = append(names, n)
	}
	sort.Strings(names)
	return names
}

func (s *Server) handleComposableTemplate(w http.ResponseWriter, r *http.Request, rest []string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(rest) == 0 {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			methodNotAllowed(w, r, "GET", "HEAD")
			return
		}
		out := []any{}
		for _, n := range sortedNames(s.composableTemplates) {
			out = append(out, map[string]any{"name": n, "index_template": indexTemplateGetView(s.composableTemplates[n])})
		}
		writeJSON(w, 200, map[string]any{"index_templates": out})
		return
	}
	name := rest[0]
	switch r.Method {
	case http.MethodPut, http.MethodPost:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		if err := s.validateAndStoreIndexTemplate(name, body, paramBool(r, "create")); err != nil {
			writeError(w, err)
			return
		}
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodDelete:
		if _, ok := s.composableTemplates[name]; !ok {
			writeError(w, &apiError{Type: "index_template_missing_exception", Status: 404,
				Reason: "index_template [" + name + "] missing"})
			return
		}
		for _, ds := range s.dataStreams {
			if ds.Template == name {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "index templates [" + name + "] cannot be removed as they are in use by a data streams [" + ds.Name + "]"})
				return
			}
		}
		delete(s.composableTemplates, name)
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodGet, http.MethodHead:
		out := []any{}
		for _, n := range sortedNames(s.composableTemplates) {
			if n == name || globMatch(name, n) {
				out = append(out, map[string]any{"name": n, "index_template": indexTemplateGetView(s.composableTemplates[n])})
			}
		}
		if len(out) == 0 {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "index template matching [" + name + "] not found"})
			return
		}
		writeJSON(w, 200, map[string]any{"index_templates": out})
	default:
		methodNotAllowed(w, r, "GET", "PUT", "POST", "DELETE")
	}
}

func (s *Server) validateAndStoreIndexTemplate(name string, body map[string]any, createOnly bool) *apiError {
	if name != strings.ToLower(name) {
		return &apiError{Type: "invalid_index_template_exception", Status: 400,
			Reason: "index_template [" + name + "] invalid, cause [Validation Failed: 1: name must be lower cased;]"}
	}
	for k := range body {
		if !knownIndexTemplateFields[k] {
			return &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "unknown field [" + k + "]"}
		}
	}
	patterns := normalizePatterns(body["index_patterns"])
	if len(patterns) == 0 {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Required [index_patterns]"}
	}
	if err := validatePatternList(patterns); err != nil {
		return err
	}
	if p, has := body["priority"]; has && p != nil {
		n, ok := settingsValueInt(p)
		if !ok || n < 0 {
			return &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: index template priority must be >= 0;"}
		}
	}
	if comps, ok := body["composed_of"].([]any); ok {
		var missing []string
		for _, c := range comps {
			cs, _ := c.(string)
			if _, ok := s.componentTemplates[cs]; !ok {
				missing = append(missing, cs)
			}
		}
		if len(missing) > 0 {
			return &apiError{Type: "invalid_index_template_exception", Status: 400,
				Reason: "index template [" + name + "] specifies component templates [" + strings.Join(missing, ", ") + "] that do not exist"}
		}
	}
	if tpl, ok := body["template"].(map[string]any); ok {
		if st, ok := tpl["settings"].(map[string]any); ok {
			canon := templateSettingsCanonical(st)
			for k, v := range canon {
				if !isKnownIndexSetting(k) {
					return &apiError{Type: "settings_exception", Status: 400,
						Reason: "unknown setting [" + k + "] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"}
				}
				if _, err := validateSettingValue(k, v, true); err != nil {
					return &apiError{Type: "invalid_index_template_exception", Status: 400,
						Reason: err.Reason}
				}
			}
		}
	}
	if createOnly {
		if _, exists := s.composableTemplates[name]; exists {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "index template [" + name + "] already exists"}
		}
	}
	prio := priorityOf(body)
	if _, has := body["priority"]; !has {
		prio = 0
	}
	for otherName, other := range s.composableTemplates {
		if otherName == name {
			continue
		}
		if priorityOf(other) != prio {
			continue
		}
		for _, p1 := range patterns {
			p1s, _ := p1.(string)
			for _, p2 := range normalizePatterns(other["index_patterns"]) {
				p2s, _ := p2.(string)
				if patternsOverlap(p1s, p2s) {
					return &apiError{Type: "illegal_argument_exception", Status: 400,
						Reason: "index template [" + name + "] has index patterns [" + p1s + "] matching patterns [" + p2s + "] from existing templates [" + otherName + "] that have the same priority [" + strconv.Itoa(prio) + "], multiple templates can be configured with different priority values or use it is recommended to use a greater priority"}
				}
			}
		}
	}
	stored := map[string]any{}
	for k, v := range body {
		stored[k] = v
	}
	s.composableTemplates[name] = stored
	return nil
}

// ------------------------------------------------------- component templates

func (s *Server) handleComponentTemplate(w http.ResponseWriter, r *http.Request, rest []string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	view := func(n string, raw map[string]any) map[string]any {
		inner := map[string]any{}
		if tpl, ok := raw["template"].(map[string]any); ok {
			tv := map[string]any{}
			if st, ok := tpl["settings"]; ok && st != nil {
				if sm, ok := st.(map[string]any); !ok || len(sm) > 0 {
					tv["settings"] = templateSettingsView(st)
				}
			}
			if m, ok := tpl["mappings"]; ok && m != nil {
				if mm, ok := m.(map[string]any); !ok || len(mm) > 0 {
					tv["mappings"] = m
				}
			}
			if a, ok := tpl["aliases"]; ok && a != nil {
				if am, ok := a.(map[string]any); ok && len(am) > 0 {
					tv["aliases"] = am
				}
			}
			inner["template"] = tv
		}
		if v, ok := raw["version"]; ok {
			inner["version"] = v
		}
		if m, ok := raw["_meta"]; ok {
			inner["_meta"] = m
		}
		return map[string]any{"name": n, "component_template": inner}
	}
	if len(rest) == 0 {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			methodNotAllowed(w, r, "GET", "HEAD")
			return
		}
		out := []any{}
		for _, n := range sortedNames(s.componentTemplates) {
			out = append(out, view(n, s.componentTemplates[n]))
		}
		writeJSON(w, 200, map[string]any{"component_templates": out})
		return
	}
	name := rest[0]
	switch r.Method {
	case http.MethodPut, http.MethodPost:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		s.componentTemplates[name] = body
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodDelete:
		if _, ok := s.componentTemplates[name]; !ok {
			writeError(w, &apiError{Type: "index_template_missing_exception", Status: 404,
				Reason: "component template [" + name + "] missing"})
			return
		}
		for otherName, tpl := range s.composableTemplates {
			if comps, ok := tpl["composed_of"].([]any); ok {
				for _, c := range comps {
					if cs, _ := c.(string); cs == name {
						writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason: "component templates [" + name + "] cannot be removed as they are still in use by index templates [" + otherName + "]"})
						return
					}
				}
			}
		}
		delete(s.componentTemplates, name)
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodGet, http.MethodHead:
		out := []any{}
		for _, n := range sortedNames(s.componentTemplates) {
			if n == name || globMatch(name, n) {
				out = append(out, view(n, s.componentTemplates[n]))
			}
		}
		if len(out) == 0 {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "component template matching [" + name + "] not found"})
			return
		}
		writeJSON(w, 200, map[string]any{"component_templates": out})
	default:
		methodNotAllowed(w, r, "GET", "PUT", "POST", "DELETE")
	}
}

// --------------------------------------------------------- legacy templates

func legacyGetView(tpl map[string]any) map[string]any {
	out := map[string]any{}
	order := 0
	if n, ok := settingsValueInt(tpl["order"]); ok {
		order = n
	}
	out["order"] = order
	out["index_patterns"] = normalizePatterns(tpl["index_patterns"])
	if s, ok := tpl["settings"]; ok && s != nil {
		out["settings"] = templateSettingsView(s)
	} else {
		out["settings"] = map[string]any{}
	}
	if m, ok := tpl["mappings"]; ok && m != nil {
		out["mappings"] = m
	} else {
		out["mappings"] = map[string]any{}
	}
	if a, ok := tpl["aliases"]; ok && a != nil {
		out["aliases"] = a
	} else {
		out["aliases"] = map[string]any{}
	}
	if v, ok := tpl["version"]; ok {
		out["version"] = v
	}
	return out
}

func (s *Server) handleLegacyTemplate(w http.ResponseWriter, r *http.Request, rest []string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(rest) == 0 {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			methodNotAllowed(w, r, "GET", "HEAD")
			return
		}
		out := map[string]any{}
		for n, tpl := range s.legacyTemplates {
			out[n] = legacyGetView(tpl)
		}
		writeJSON(w, 200, out)
		return
	}
	name := rest[0]
	switch r.Method {
	case http.MethodPut, http.MethodPost:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		patterns := normalizePatterns(body["index_patterns"])
		if len(patterns) == 0 {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: index patterns are missing;"})
			return
		}
		if paramBool(r, "create") {
			if _, exists := s.legacyTemplates[name]; exists {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "index_template [" + name + "] already exists"})
				return
			}
		}
		s.legacyTemplates[name] = body
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodDelete:
		if _, ok := s.legacyTemplates[name]; !ok {
			writeError(w, &apiError{Type: "index_template_missing_exception", Status: 404,
				Reason: "index_template [" + name + "] missing"})
			return
		}
		delete(s.legacyTemplates, name)
		s.bumpState()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
	case http.MethodGet, http.MethodHead:
		out := map[string]any{}
		for n, tpl := range s.legacyTemplates {
			if n == name || globMatch(name, n) {
				out[n] = legacyGetView(tpl)
			}
		}
		if len(out) == 0 {
			writeJSON(w, 404, map[string]any{})
			return
		}
		writeJSON(w, 200, out)
	default:
		methodNotAllowed(w, r, "GET", "PUT", "POST", "DELETE")
	}
}

// ------------------------------------------------------- template composing

type composedTemplate struct {
	settings    map[string]any
	mappings    map[string]any
	aliases     map[string]any
	dsDef       map[string]any
	template    string
	overlapping []any
}

// composeForIndex merges matching templates for the index/DS name.
// Called with s.mu held.
func (s *Server) composeForIndex(name string) *composedTemplate {
	out := &composedTemplate{
		settings: map[string]any{},
		mappings: map[string]any{},
		aliases:  map[string]any{},
	}
	var bestName string
	bestPriority := -1 << 31
	for n, tpl := range s.composableTemplates {
		if !templateMatches(tpl, name) {
			continue
		}
		p := priorityOf(tpl)
		if p > bestPriority || (p == bestPriority && (bestName == "" || n < bestName)) {
			if bestName != "" {
				out.overlapping = append(out.overlapping, map[string]any{
					"name": bestName, "index_patterns": normalizePatterns(s.composableTemplates[bestName]["index_patterns"]),
				})
			}
			bestName = n
			bestPriority = p
		} else {
			out.overlapping = append(out.overlapping, map[string]any{"name": n, "index_patterns": normalizePatterns(tpl["index_patterns"])})
		}
	}
	if bestName != "" {
		tpl := s.composableTemplates[bestName]
		out.template = bestName
		if comps, ok := tpl["composed_of"].([]any); ok {
			for _, c := range comps {
				cs, _ := c.(string)
				comp, ok := s.componentTemplates[cs]
				if !ok {
					continue
				}
				if ctpl, ok := comp["template"].(map[string]any); ok {
					if st, ok := ctpl["settings"]; ok {
						for k, v := range templateSettingsCanonical(st) {
							out.settings[k] = v
						}
					}
					if m, ok := ctpl["mappings"].(map[string]any); ok {
						mergeMappingsDeep(out.mappings, m)
					}
					if a, ok := ctpl["aliases"].(map[string]any); ok {
						for k, v := range a {
							out.aliases[k] = v
						}
					}
				}
			}
		}
		if tpl0, ok := tpl["template"].(map[string]any); ok {
			if st, ok := tpl0["settings"]; ok {
				for k, v := range templateSettingsCanonical(st) {
					out.settings[k] = v
				}
			}
			if m, ok := tpl0["mappings"].(map[string]any); ok {
				mergeMappingsDeep(out.mappings, m)
			}
			if a, ok := tpl0["aliases"].(map[string]any); ok {
				for k, v := range a {
					out.aliases[k] = v
				}
			}
		}
		if ds, ok := tpl["data_stream"].(map[string]any); ok {
			out.dsDef = ds
		} else if _, ok := tpl["data_stream"]; ok {
			out.dsDef = map[string]any{}
		}
		for n, ltpl := range s.legacyTemplates {
			if templateMatches(ltpl, name) {
				out.overlapping = append(out.overlapping, map[string]any{"name": n, "index_patterns": normalizePatterns(ltpl["index_patterns"])})
			}
		}
		sort.Slice(out.overlapping, func(i, j int) bool {
			a := out.overlapping[i].(map[string]any)["name"].(string)
			b := out.overlapping[j].(map[string]any)["name"].(string)
			return a < b
		})
		return out
	}
	// legacy templates: merge by ascending order
	names := make([]string, 0, len(s.legacyTemplates))
	for n, tpl := range s.legacyTemplates {
		if templateMatches(tpl, name) {
			names = append(names, n)
			out.overlapping = append(out.overlapping, map[string]any{"name": n, "index_patterns": normalizePatterns(tpl["index_patterns"])})
		}
	}
	sort.Slice(names, func(i, j int) bool {
		oi := 0
		if n, ok := settingsValueInt(s.legacyTemplates[names[i]]["order"]); ok {
			oi = n
		}
		oj := 0
		if n, ok := settingsValueInt(s.legacyTemplates[names[j]]["order"]); ok {
			oj = n
		}
		if oi != oj {
			return oi < oj
		}
		return names[i] < names[j]
	})
	sort.Slice(out.overlapping, func(i, j int) bool {
		a := out.overlapping[i].(map[string]any)["name"].(string)
		b := out.overlapping[j].(map[string]any)["name"].(string)
		return a < b
	})
	for _, n := range names {
		tpl := s.legacyTemplates[n]
		if st, ok := tpl["settings"]; ok {
			for k, v := range templateSettingsCanonical(st) {
				out.settings[k] = v
			}
		}
		if m, ok := tpl["mappings"].(map[string]any); ok {
			mergeMappingsDeep(out.mappings, m)
		}
		if a, ok := tpl["aliases"].(map[string]any); ok {
			for k, v := range a {
				resolved := strings.ReplaceAll(k, "{index}", name)
				out.aliases[resolved] = v
			}
		}
	}
	return out
}

// mergeMappingsDeep merges src into dst; values in src win.
func mergeMappingsDeep(dst, src map[string]any) {
	for k, v := range src {
		if k == "properties" {
			sv, ok := v.(map[string]any)
			if !ok {
				dst[k] = v
				continue
			}
			dv, ok := dst["properties"].(map[string]any)
			if !ok {
				dv = map[string]any{}
				dst["properties"] = dv
			}
			for fk, fv := range sv {
				dfv, ok := dv[fk].(map[string]any)
				sfv, ok2 := fv.(map[string]any)
				if ok && ok2 {
					mergeMappingsDeep(dfv, sfv)
				} else {
					dv[fk] = fv
				}
			}
			continue
		}
		dst[k] = v
	}
}

// overlappingFor returns other templates (composable + legacy) whose patterns
// overlap any of the given patterns.
func (s *Server) overlappingFor(patterns []any, exclude string) []any {
	seen := map[string]bool{}
	out := []any{}
	overlap := func(pats []any) bool {
		for _, p1 := range patterns {
			p1s, _ := p1.(string)
			for _, p2 := range pats {
				p2s, _ := p2.(string)
				if patternsOverlap(p1s, p2s) {
					return true
				}
			}
		}
		return false
	}
	for n, tpl := range s.composableTemplates {
		if n == exclude || seen[n] {
			continue
		}
		if overlap(normalizePatterns(tpl["index_patterns"])) {
			seen[n] = true
			out = append(out, map[string]any{"name": n, "index_patterns": normalizePatterns(tpl["index_patterns"])})
		}
	}
	for n, tpl := range s.legacyTemplates {
		if n == exclude || seen[n] {
			continue
		}
		if overlap(normalizePatterns(tpl["index_patterns"])) {
			seen[n] = true
			out = append(out, map[string]any{"name": n, "index_patterns": normalizePatterns(tpl["index_patterns"])})
		}
	}
	sort.Slice(out, func(i, j int) bool {
		a := out[i].(map[string]any)["name"].(string)
		b := out[j].(map[string]any)["name"].(string)
		return a < b
	})
	return out
}

// ------------------------------------------------------- simulate endpoint

func (s *Server) simulateTemplate(w http.ResponseWriter, r *http.Request, name string, body map[string]any) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	var composed *composedTemplate
	if body != nil {
		composed = &composedTemplate{settings: map[string]any{}, mappings: map[string]any{}, aliases: map[string]any{}}
		patterns := normalizePatterns(body["index_patterns"])
		if tpl, ok := body["template"].(map[string]any); ok {
			if st, ok := tpl["settings"]; ok {
				for k, v := range templateSettingsCanonical(st) {
					composed.settings[k] = v
				}
			}
			if m, ok := tpl["mappings"].(map[string]any); ok {
				mergeMappingsDeep(composed.mappings, m)
			}
			if a, ok := tpl["aliases"].(map[string]any); ok {
				for k, v := range a {
					composed.aliases[k] = v
				}
			}
		}
		composed.overlapping = s.overlappingFor(patterns, "")
	} else if name != "" {
		tpl, ok := s.composableTemplates[name]
		if !ok {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "index template matching [" + name + "] not found"})
			return
		}
		composed = &composedTemplate{settings: map[string]any{}, mappings: map[string]any{}, aliases: map[string]any{}}
		composed.overlapping = s.overlappingFor(normalizePatterns(tpl["index_patterns"]), name)
		if comps, ok := tpl["composed_of"].([]any); ok {
			for _, c := range comps {
				cs, _ := c.(string)
				comp, ok := s.componentTemplates[cs]
				if !ok {
					continue
				}
				if ctpl, ok := comp["template"].(map[string]any); ok {
					if st, ok := ctpl["settings"]; ok {
						for k, v := range templateSettingsCanonical(st) {
							composed.settings[k] = v
						}
					}
					if m, ok := ctpl["mappings"].(map[string]any); ok {
						mergeMappingsDeep(composed.mappings, m)
					}
					if a, ok := ctpl["aliases"].(map[string]any); ok {
						for k, v := range a {
							composed.aliases[k] = v
						}
					}
				}
			}
		}
		if tpl0, ok := tpl["template"].(map[string]any); ok {
			if st, ok := tpl0["settings"]; ok {
				for k, v := range templateSettingsCanonical(st) {
					composed.settings[k] = v
				}
			}
			if m, ok := tpl0["mappings"].(map[string]any); ok {
				mergeMappingsDeep(composed.mappings, m)
			}
			if a, ok := tpl0["aliases"].(map[string]any); ok {
				for k, v := range a {
					composed.aliases[k] = v
				}
			}
		}
	} else {
		composed = &composedTemplate{settings: map[string]any{}, mappings: map[string]any{}, aliases: map[string]any{}}
	}
	if composed.overlapping == nil {
		composed.overlapping = []any{}
	}
	sort.Slice(composed.overlapping, func(i, j int) bool {
		a := composed.overlapping[i].(map[string]any)["name"].(string)
		b := composed.overlapping[j].(map[string]any)["name"].(string)
		return a < b
	})
	inner := map[string]any{}
	for k, v := range composed.settings {
		inner[strings.TrimPrefix(k, "index.")] = settingsValueString(v)
	}
	tplView := map[string]any{"aliases": composed.aliases}
	if len(composed.settings) > 0 {
		tplView["settings"] = map[string]any{"index": inner}
	}
	if len(composed.mappings) > 0 {
		tplView["mappings"] = composed.mappings
	}
	writeJSON(w, 200, map[string]any{"template": tplView, "overlapping": composed.overlapping})
}

// simulateForIndex composes for a concrete index name and renders the view.
func (s *Server) simulateForIndex(w http.ResponseWriter, r *http.Request, indexName string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	composed := s.composeForIndex(indexName)
	if composed.overlapping == nil {
		composed.overlapping = []any{}
	}
	sort.Slice(composed.overlapping, func(i, j int) bool {
		a := composed.overlapping[i].(map[string]any)["name"].(string)
		b := composed.overlapping[j].(map[string]any)["name"].(string)
		return a < b
	})
	inner := map[string]any{}
	for k, v := range composed.settings {
		inner[strings.TrimPrefix(k, "index.")] = settingsValueString(v)
	}
	tplView := map[string]any{"aliases": composed.aliases}
	if len(composed.settings) > 0 {
		tplView["settings"] = map[string]any{"index": inner}
	}
	if len(composed.mappings) > 0 {
		tplView["mappings"] = composed.mappings
	}
	writeJSON(w, 200, map[string]any{"template": tplView, "overlapping": composed.overlapping})
}

func (s *Server) handleIndexTemplateSimulate(w http.ResponseWriter, r *http.Request, rest []string) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	if len(rest) >= 2 && rest[0] == "_simulate_index" {
		s.simulateForIndex(w, r, rest[1])
		return
	}
	if len(rest) >= 1 && rest[0] == "_simulate" {
		if len(rest) >= 2 {
			s.simulateTemplate(w, r, rest[1], nil)
			return
		}
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return
		}
		s.simulateTemplate(w, r, "", body)
		return
	}
	s.simulateTemplate(w, r, "", nil)
}

// applyTemplatesToNewIndex is retained for compatibility.
func applyTemplatesToNewIndex(s *Server, idx *Index) {}
