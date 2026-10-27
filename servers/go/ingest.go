package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"regexp"
	"strconv"
	"strings"
	"time"
)

type ingestEnv struct {
	s          *Server
	source     map[string]any
	meta       map[string]any
	pipeline   string
	timestamp  string
	stack      []string
	onFailure  map[string]any
	foreachVal any
	foreachSet func(any)
	processor  string
	tag        string
	results    []any
	verbose    bool
}

func ingestTimestamp() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
}

func (e *ingestEnv) cloneDoc() map[string]any {
	out := map[string]any{}
	for k, v := range e.meta {
		out[k] = v
	}
	out["_source"] = deepCopyValue(e.source).(map[string]any)
	ing := map[string]any{"timestamp": e.timestamp}
	if e.verbose {
		ing["pipeline"] = e.pipeline
	}
	if extra, ok := e.meta["_ingest"].(map[string]any); ok {
		for k, v := range extra {
			ing[k] = v
		}
	}
	out["_ingest"] = ing
	return out
}

func deepCopyValue(v any) any {
	switch t := v.(type) {
	case map[string]any:
		m := make(map[string]any, len(t))
		for k, vv := range t {
			m[k] = deepCopyValue(vv)
		}
		return m
	case []any:
		a := make([]any, len(t))
		for i, vv := range t {
			a[i] = deepCopyValue(vv)
		}
		return a
	}
	return v
}

var ingestTemplateRE = regexp.MustCompile(`\{\{\{?\s*([^{}]+?)\s*\}?\}\}`)

func (e *ingestEnv) lookup(path string) (any, bool) {
	path = strings.TrimSpace(path)
	if path == "" {
		return nil, false
	}
	if path == "_ingest" {
		return map[string]any{"pipeline": e.pipeline, "timestamp": e.timestamp}, true
	}
	if strings.HasPrefix(path, "_ingest.") {
		key := strings.TrimPrefix(path, "_ingest.")
		if key == "pipeline" {
			return e.pipeline, true
		}
		if key == "timestamp" {
			return e.timestamp, true
		}
		if e.onFailure != nil {
			if v, ok := e.onFailure[key]; ok {
				return v, true
			}
		}
		if strings.HasPrefix(key, "_value") {
			rest := strings.TrimPrefix(key, "_value")
			rest = strings.TrimPrefix(rest, ".")
			if rest == "" {
				return e.foreachVal, true
			}
			return lookupNested(e.foreachVal, strings.Split(rest, "."), true)
		}
		return nil, false
	}
	switch path {
	case "_index", "_id", "_routing", "_version", "_version_type":
		v, ok := e.meta[path]
		return v, ok
	}
	if strings.HasPrefix(path, "_") {
		v, ok := e.meta[path]
		return v, ok
	}
	return lookupNested(e.source, strings.Split(path, "."), true)
}

func lookupNested(cur any, parts []string, safe bool) (any, bool) {
	for _, p := range parts {
		if p == "" {
			continue
		}
		if strings.HasSuffix(p, "?") {
			p = strings.TrimSuffix(p, "?")
			if cur == nil {
				return nil, false
			}
		}
		m, ok := cur.(map[string]any)
		if !ok {
			if safe {
				return nil, false
			}
			return nil, false
		}
		v, exists := m[p]
		if !exists {
			return nil, false
		}
		cur = v
	}
	return cur, true
}

func ingestString(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case json.Number:
		return t.String()
	case bool:
		if t {
			return "true"
		}
		return "false"
	case float64:
		return strconv.FormatFloat(t, 'f', -1, 64)
	case int64:
		return strconv.FormatInt(t, 10)
	case int:
		return strconv.Itoa(t)
	default:
		return fmt.Sprint(t)
	}
}

func (e *ingestEnv) render(s string) string {
	return ingestTemplateRE.ReplaceAllStringFunc(s, func(m string) string {
		sub := ingestTemplateRE.FindStringSubmatch(m)
		if len(sub) < 2 {
			return ""
		}
		v, ok := e.lookup(sub[1])
		if !ok {
			return ""
		}
		return ingestString(v)
	})
}

func (e *ingestEnv) renderValue(v any) any {
	if s, ok := v.(string); ok {
		return e.render(s)
	}
	return v
}

func setNested(root map[string]any, path string, value any) *apiError {
	parts := strings.Split(path, ".")
	cur := root
	for i, p := range parts {
		if i == len(parts)-1 {
			cur[p] = value
			return nil
		}
		next, exists := cur[p]
		if !exists || next == nil {
			m := map[string]any{}
			cur[p] = m
			cur = m
			continue
		}
		m, ok := next.(map[string]any)
		if !ok {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("cannot set [%s] with parent object of type [%s] as part of path [%s]", parts[i+1], javaTypeName(next), path)}
		}
		cur = m
	}
	return nil
}

func setAnyNested(cur any, parts []string, value any) any {
	if len(parts) == 0 {
		return value
	}
	m, ok := cur.(map[string]any)
	if !ok {
		return cur
	}
	if len(parts) == 1 {
		m[parts[0]] = value
		return m
	}
	next, _ := m[parts[0]]
	m[parts[0]] = setAnyNested(next, parts[1:], value)
	return m
}

func (e *ingestEnv) setField(path string, value any, override bool) *apiError {
	path = e.render(path)
	if strings.HasPrefix(path, "_ingest._value") {
		rest := strings.TrimPrefix(path, "_ingest._value")
		rest = strings.TrimPrefix(rest, ".")
		if rest == "" {
			if e.foreachSet != nil {
				e.foreachSet(value)
			}
			return nil
		}
		if e.foreachSet != nil {
			e.foreachSet(setAnyNested(e.foreachVal, strings.Split(rest, "."), value))
		}
		return nil
	}
	if strings.HasPrefix(path, "_") {
		if !override {
			if _, ok := e.meta[path]; ok {
				return nil
			}
		}
		e.meta[path] = value
		return nil
	}
	if !override {
		if old, ok := lookupNested(e.source, strings.Split(path, "."), true); ok && old != nil {
			return nil
		}
	}
	return setNested(e.source, path, value)
}

func removeNested(root map[string]any, path string) bool {
	parts := strings.Split(path, ".")
	cur := root
	for i, p := range parts {
		if i == len(parts)-1 {
			if _, ok := cur[p]; !ok {
				return false
			}
			delete(cur, p)
			return true
		}
		next, ok := cur[p].(map[string]any)
		if !ok {
			return false
		}
		cur = next
	}
	return false
}

func javaTypeName(v any) string {
	switch t := v.(type) {
	case nil:
		return "null"
	case string:
		return "java.lang.String"
	case bool:
		return "java.lang.Boolean"
	case json.Number:
		if strings.ContainsAny(t.String(), ".eE") {
			return "java.lang.Double"
		}
		return "java.lang.Integer"
	case float64, float32:
		return "java.lang.Double"
	case int, int64, int32:
		return "java.lang.Integer"
	case []any:
		return "java.util.ArrayList"
	case map[string]any:
		return "java.util.HashMap"
	}
	return "java.lang.Object"
}

func apiErrorObject(e *apiError) map[string]any {
	rcType, rcReason := e.Type, e.Reason
	if e.RootCause != nil {
		rcType, rcReason = e.RootCause.Type, e.RootCause.Reason
	}
	m := map[string]any{
		"root_cause": []any{map[string]any{"type": rcType, "reason": rcReason}},
		"type":       e.Type,
		"reason":     e.Reason,
	}
	for k, v := range e.Fields {
		m[k] = v
	}
	if e.CausedBy != nil {
		m["caused_by"] = apiCauseObject(e.CausedBy)
	}
	return m
}

func apiCauseObject(e *apiError) map[string]any {
	m := map[string]any{"type": e.Type, "reason": e.Reason}
	for k, v := range e.Fields {
		m[k] = v
	}
	if e.CausedBy != nil {
		m["caused_by"] = apiCauseObject(e.CausedBy)
	}
	return m
}

func (s *Server) handleIngestRoute(w http.ResponseWriter, r *http.Request, segs []string) bool {
	if len(segs) < 2 || segs[1] != "pipeline" {
		if len(segs) >= 3 && segs[1] == "processor" && segs[2] == "grok" && r.Method == http.MethodGet {
			writeJSON(w, 200, map[string]any{"patterns": grokPatterns()})
			return true
		}
		return false
	}
	rest := segs[2:]
	if len(rest) == 1 && rest[0] == "_simulate" {
		s.simulatePipeline(w, r, "")
		return true
	}
	if len(rest) == 2 && rest[1] == "_simulate" {
		s.simulatePipeline(w, r, rest[0])
		return true
	}
	if len(rest) == 0 {
		if r.Method != http.MethodGet {
			methodNotAllowed(w, r, "GET")
			return true
		}
		s.ingestMu.RLock()
		out := map[string]any{}
		for name, body := range s.ingestPipelines {
			out[name] = body
		}
		s.ingestMu.RUnlock()
		writeJSON(w, 200, out)
		return true
	}
	nameExpr := rest[0]
	switch r.Method {
	case http.MethodGet:
		names := s.resolvePipelineNames(nameExpr)
		if len(names) == 0 {
			if !strings.ContainsAny(nameExpr, "*,") {
				writeJSON(w, 404, map[string]any{})
			} else {
				writeJSON(w, 200, map[string]any{})
			}
			return true
		}
		s.ingestMu.RLock()
		out := map[string]any{}
		for _, n := range names {
			if b, ok := s.ingestPipelines[n]; ok {
				out[n] = b
			}
		}
		s.ingestMu.RUnlock()
		writeJSON(w, 200, out)
		return true
	case http.MethodPut:
		body, apiErr := readJSON(r)
		if apiErr != nil {
			writeError(w, apiErr)
			return true
		}
		norm, apiErr := validatePipelineBody(nameExpr, body)
		if apiErr != nil {
			writeError(w, apiErr)
			return true
		}
		s.ingestMu.Lock()
		s.ingestPipelines[nameExpr] = norm
		s.ingestMu.Unlock()
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	case http.MethodDelete:
		s.ingestMu.Lock()
		_, ok := s.ingestPipelines[nameExpr]
		delete(s.ingestPipelines, nameExpr)
		s.ingestMu.Unlock()
		if !ok {
			writeError(w, &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "pipeline [" + nameExpr + "] is missing"})
			return true
		}
		writeJSON(w, 200, map[string]any{"acknowledged": true})
		return true
	}
	methodNotAllowed(w, r, "GET", "PUT", "DELETE")
	return true
}

func envList(v any) []any {
	if l, ok := v.([]any); ok {
		return l
	}
	return nil
}

func (s *Server) resolvePipelineNames(expr string) []string {
	parts := strings.Split(expr, ",")
	out := []string{}
	seen := map[string]bool{}
	s.ingestMu.RLock()
	defer s.ingestMu.RUnlock()
	for _, p := range parts {
		if strings.Contains(p, "*") {
			for n := range s.ingestPipelines {
				if globMatch(p, n) && !seen[n] {
					seen[n] = true
					out = append(out, n)
				}
			}
		} else if _, ok := s.ingestPipelines[p]; ok && !seen[p] {
			seen[p] = true
			out = append(out, p)
		}
	}
	return out
}

var pipelineAllowedKeys = map[string]bool{"description": true, "version": true, "processors": true, "on_failure": true}

func validatePipelineBody(name string, body map[string]any) (map[string]any, *apiError) {
	for k := range body {
		if k == "_meta" || !pipelineAllowedKeys[k] {
			return nil, &apiError{Type: "parse_exception", Status: 400,
				Reason: "pipeline [" + name + "] doesn't support one or more provided configuration parameters [" + k + "]"}
		}
	}
	raw, ok := body["processors"]
	if !ok {
		return nil, &apiError{Type: "parse_exception", Status: 400,
			Reason: "[processors] required property is missing", Fields: map[string]any{"property_name": "processors"}}
	}
	list, ok := raw.([]any)
	if !ok {
		return nil, &apiError{Type: "parse_exception", Status: 400, Reason: "[processors] required property is missing",
			Fields: map[string]any{"property_name": "processors"}}
	}
	if _, err := validateProcessors(list, name); err != nil {
		return nil, err
	}
	if of, ok := body["on_failure"]; ok {
		ofl, ok := of.([]any)
		if !ok || len(ofl) == 0 {
			return nil, &apiError{Type: "parse_exception", Status: 400, Reason: "[on_failure] processors list cannot be empty"}
		}
		if _, err := validateProcessors(ofl, name); err != nil {
			return nil, err
		}
	}
	norm := map[string]any{"processors": list}
	if v, ok := body["description"]; ok {
		norm["description"] = v
	}
	if v, ok := body["version"]; ok {
		norm["version"] = v
	}
	if v, ok := body["on_failure"]; ok {
		norm["on_failure"] = v
	}
	return norm, nil
}

func validateProcessors(list []any, name string) ([]string, *apiError) {
	types := []string{}
	for _, raw := range list {
		def, ok := raw.(map[string]any)
		if !ok {
			return nil, &apiError{Type: "parse_exception", Status: 400, Reason: "processor must be an object"}
		}
		typ, cfg, err := processorConfig(def)
		if err != nil {
			return nil, err
		}
		types = append(types, typ)
		if err := validateProcessorConfig(typ, cfg); err != nil {
			return nil, err
		}
		if of, ok := procControl(def, cfg, "on_failure"); ok {
			ofl, ok := of.([]any)
			if !ok || len(ofl) == 0 {
				return nil, &apiError{Type: "parse_exception", Status: 400, Reason: "[on_failure] processors list cannot be empty"}
			}
			if _, err := validateProcessors(ofl, name); err != nil {
				return nil, err
			}
		}
	}
	return types, nil
}

func processorConfig(def map[string]any) (string, map[string]any, *apiError) {
	control := map[string]bool{"description": true, "tag": true, "if": true, "on_failure": true, "ignore_failure": true}
	typ := ""
	var cfg map[string]any
	for k, v := range def {
		if control[k] {
			continue
		}
		if typ != "" {
			return "", nil, &apiError{Type: "parse_exception", Status: 400, Reason: "processor must specify exactly one processor type"}
		}
		typ = k
		cfg, _ = v.(map[string]any)
		if cfg == nil {
			cfg = map[string]any{}
		}
	}
	if typ == "" {
		return "", nil, &apiError{Type: "parse_exception", Status: 400, Reason: "processor must specify a processor type"}
	}
	if !knownProcessorTypes[typ] {
		return "", nil, &apiError{Type: "parse_exception", Status: 400,
			Reason: "No processor type exists with name [" + typ + "]", Fields: map[string]any{"processor_type": typ}}
	}
	return typ, cfg, nil
}

var knownProcessorTypes = map[string]bool{
	"set": true, "remove": true, "rename": true, "copy": true, "remove_by_pattern": true,
	"append": true, "convert": true, "lowercase": true, "uppercase": true, "trim": true,
	"split": true, "join": true, "gsub": true, "sort": true, "grok": true, "dissect": true,
	"date": true, "date_index_name": true, "json": true, "kv": true, "csv": true,
	"split_to_fields": true, "urldecode": true, "html_strip": true, "bytes": true,
	"fingerprint": true, "community_id": true, "dot_expander": true, "foreach": true,
	"fail": true, "drop": true, "script": true, "pipeline": true, "user_agent": true,
}

var processorAllowed = map[string]map[string]bool{
	"set":               {"field": true, "value": true, "override": true, "ignore_empty_value": true},
	"remove":            {"field": true, "exclude_field": true, "ignore_missing": true},
	"rename":            {"field": true, "target_field": true, "ignore_missing": true, "override_target": true},
	"copy":              {"source_field": true, "target_field": true, "ignore_missing": true, "override_target": true, "remove_source": true},
	"remove_by_pattern": {"field_pattern": true, "exclude_field_pattern": true},
	"append":            {"field": true, "value": true, "allow_duplicates": true},
	"convert":           {"field": true, "type": true, "target_field": true, "ignore_missing": true},
	"lowercase":         {"field": true, "target_field": true, "ignore_missing": true},
	"uppercase":         {"field": true, "target_field": true, "ignore_missing": true},
	"trim":              {"field": true, "target_field": true, "ignore_missing": true},
	"split":             {"field": true, "separator": true, "preserve_trailing": true, "target_field": true, "ignore_missing": true},
	"join":              {"field": true, "separator": true, "target_field": true, "ignore_missing": true},
	"gsub":              {"field": true, "pattern": true, "replacement": true, "target_field": true, "ignore_missing": true},
	"sort":              {"field": true, "order": true, "target_field": true, "ignore_missing": true},
	"grok":              {"field": true, "patterns": true, "pattern_definitions": true, "trace_match": true, "ignore_missing": true},
	"dissect":           {"field": true, "pattern": true, "append_separator": true, "ignore_missing": true},
	"date":              {"field": true, "formats": true, "timezone": true, "target_field": true, "output_format": true, "locale": true, "ignore_missing": true},
	"date_index_name":   {"field": true, "date_rounding": true, "date_formats": true, "index_name_prefix": true, "index_name_format": true, "timezone": true, "locale": true, "ignore_missing": true},
	"json":              {"field": true, "target_field": true, "add_to_root": true, "ignore_missing": true},
	"kv":                {"field": true, "field_split": true, "value_split": true, "target_field": true, "include_keys": true, "exclude_keys": true, "prefix": true, "trim_key": true, "trim_value": true, "strip_brackets": true, "ignore_missing": true},
	"csv":               {"field": true, "target_fields": true, "separator": true, "quote": true, "trim": true, "empty_value": true, "ignore_missing": true},
	"split_to_fields":   {"field": true, "separator": true, "target_fields": true, "ignore_missing": true},
	"urldecode":         {"field": true, "target_field": true, "ignore_missing": true},
	"html_strip":        {"field": true, "target_field": true, "ignore_missing": true},
	"bytes":             {"field": true, "target_field": true, "ignore_missing": true},
	"fingerprint":       {"fields": true, "exclude_fields": true, "target_field": true, "hash_method": true, "ignore_missing": true},
	"community_id":      {"source_ip_field": true, "source_port_field": true, "destination_ip_field": true, "destination_port_field": true, "iana_protocol_number_field": true, "protocol_field": true, "icmp_type_field": true, "icmp_code_field": true, "seed": true, "target_field": true, "ignore_missing": true},
	"dot_expander":      {"field": true, "path": true, "override": true},
	"foreach":           {"field": true, "processor": true, "ignore_missing": true},
	"fail":              {"message": true},
	"drop":              {},
	"script":            {"source": true, "id": true, "lang": true, "params": true},
	"pipeline":          {"name": true},
	"user_agent":        {"field": true, "target_field": true, "properties": true, "ignore_missing": true},
}

func validateProcessorConfig(typ string, cfg map[string]any) *apiError {
	allowed := processorAllowed[typ]
	for k := range cfg {
		if k == "description" || k == "tag" || k == "if" || k == "on_failure" || k == "ignore_failure" {
			continue
		}
		if !allowed[k] {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "processor [" + typ + "] doesn't support one or more provided configuration parameters [" + k + "]",
				Fields: map[string]any{"processor_type": typ}}
		}
	}
	required := map[string][]string{
		"set": {"field", "value"}, "rename": {"field", "target_field"}, "copy": {"source_field", "target_field"},
		"append": {"field", "value"}, "convert": {"field", "type"}, "lowercase": {"field"},
		"uppercase": {"field"}, "trim": {"field"}, "split": {"field", "separator"},
		"join": {"field", "separator"}, "gsub": {"field", "pattern", "replacement"},
		"sort": {"field"}, "grok": {"field", "patterns"}, "dissect": {"field", "pattern"},
		"date": {"field", "formats"}, "date_index_name": {"field", "date_rounding"},
		"json": {"field"}, "kv": {"field", "field_split", "value_split"}, "csv": {"field", "target_fields"},
		"split_to_fields": {"field", "separator", "target_fields"}, "fail": {"message"},
		"pipeline": {"name"}, "user_agent": {"field"},
	}
	for _, key := range required[typ] {
		if _, ok := cfg[key]; !ok {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[" + key + "] required property is missing",
				Fields: map[string]any{"processor_type": typ, "property_name": key}}
		}
	}
	if typ == "remove_by_pattern" {
		fp, hasFP := cfg["field_pattern"]
		ep, hasEP := cfg["exclude_field_pattern"]
		if hasFP == hasEP {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[field_pattern] either field_pattern or exclude_field_pattern must be set"}
		}
		check := func(v any) *apiError {
			if s, ok := v.(string); ok && strings.HasPrefix(s, "_") {
				return &apiError{Type: "parse_exception", Status: 400,
					Reason: "[field_pattern] Validation Failed: 1: field_pattern [" + s + "] must not start with '_';"}
			}
			if list, ok := v.([]any); ok {
				for _, e := range list {
					if s, ok := e.(string); ok && strings.HasPrefix(s, "_") {
						return &apiError{Type: "parse_exception", Status: 400,
							Reason: "[field_pattern] Validation Failed: 1: field_pattern [" + s + "] must not start with '_';"}
					}
				}
			}
			return nil
		}
		if hasFP {
			if err := check(fp); err != nil {
				return err
			}
		}
		if hasEP {
			if err := check(ep); err != nil {
				return err
			}
		}
	}
	if typ == "sort" {
		if o, ok := cfg["order"].(string); ok && o != "asc" && o != "desc" {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[order] Sort direction [" + o + "] not recognized. Valid values are: [asc, desc]"}
		}
	}
	if typ == "convert" {
		t, _ := cfg["type"].(string)
		switch t {
		case "integer", "long", "float", "double", "boolean", "string", "auto", "ip":
		default:
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[type] type [" + t + "] not supported, cannot convert field.",
				Fields: map[string]any{"property_name": "type"}}
		}
	}
	if typ == "gsub" {
		p, _ := cfg["pattern"].(string)
		if _, err := regexp.Compile(p); err != nil {
			reason := "[pattern] Invalid regex pattern. " + err.Error()
			if strings.Contains(err.Error(), "missing closing ]") {
				reason = "[pattern] Invalid regex pattern. Unclosed character class"
			}
			return &apiError{Type: "parse_exception", Status: 400, Reason: reason,
				Fields: map[string]any{"processor_type": "gsub"}}
		}
	}
	if typ == "grok" {
		if list, ok := cfg["patterns"].([]any); ok && len(list) == 0 {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[patterns] List of patterns must not be empty",
				Fields: map[string]any{"processor_type": "grok"}}
		}
	}
	if typ == "json" {
		_, add := cfg["add_to_root"].(bool)
		if add {
			if _, ok := cfg["target_field"]; ok {
				return &apiError{Type: "parse_exception", Status: 400,
					Reason: "[target_field] Cannot set a target field while also setting `add_to_root` to true"}
			}
		}
	}
	if typ == "csv" {
		if s, ok := cfg["separator"].(string); ok && len([]rune(s)) != 1 {
			return &apiError{Type: "parse_exception", Status: 400,
				Reason: "[separator] separator has to be single character like , or ;"}
		}
	}
	if typ == "grok" {
		defs := map[string]string{}
		for k, v := range grokPatterns() {
			defs[k] = v.(string)
		}
		if pd, ok := cfg["pattern_definitions"].(map[string]any); ok {
			for k, v := range pd {
				defs[k] = ingestString(v)
			}
		}
		if list, ok := cfg["patterns"].([]any); ok {
			for _, raw := range list {
				if _, err := grokCompile(ingestString(raw), defs); err != nil {
					return err
				}
			}
		}
	}
	if typ == "fingerprint" {
		if _, hasF := cfg["fields"]; hasF {
			if ex, hasE := cfg["exclude_fields"]; hasE && len(envList(ex)) > 0 {
				return &apiError{Type: "parse_exception", Status: 400, Reason: "[fields] either fields or exclude_fields can be set"}
			}
		}
		if m, ok := cfg["hash_method"].(string); ok {
			up := strings.ToUpper(m)
			if up != "MD5@2.16.0" && up != "SHA-1@2.16.0" && up != "SHA-256@2.16.0" && up != "SHA3-256@2.16.0" {
				return &apiError{Type: "parse_exception", Status: 400,
					Reason: "[hash_method] hash method must be MD5@2.16.0, SHA-1@2.16.0, SHA-256@2.16.0 or SHA3-256@2.16.0"}
			}
		}
	}
	if cond, ok := cfg["if"].(string); ok {
		trimmed := strings.TrimSpace(cond)
		if trimmed == "" || strings.HasSuffix(trimmed, "=") && !strings.HasSuffix(trimmed, "==") && !strings.HasSuffix(trimmed, "!=") && !strings.HasSuffix(trimmed, ">=") && !strings.HasSuffix(trimmed, "<=") {
			return &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
				Fields: map[string]any{"processor_type": typ}}
		}
	}
	if typ == "script" {
		if lang, ok := cfg["lang"].(string); ok && lang == "mustache" {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "mustache engine does not know how to handle context [ingest]", ForceTop: true}
		}
		if src, ok := cfg["source"].(string); ok {
			trimmed := strings.TrimSpace(src)
			if trimmed == "" || strings.HasSuffix(trimmed, "=") {
				return &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
					Fields: map[string]any{"processor_type": "script", "script_stack": []any{src, "        ^---- HERE"}}}
			}
		}
	}
	if typ == "dot_expander" {
		if f, ok := cfg["field"].(string); ok && (!strings.Contains(f, ".") || strings.Contains(f, "*")) {
			return &apiError{Type: "parse_exception", Status: 400, Reason: "[field] field does not contain a dot"}
		}
	}
	if typ == "user_agent" {
		if list, ok := cfg["properties"].([]any); ok {
			for _, raw := range list {
				p := strings.ToUpper(ingestString(raw))
				switch p {
				case "NAME", "MAJOR", "MINOR", "PATCH", "BUILD", "OS", "OS_NAME", "OS_MAJOR", "OS_MINOR", "OS_FULL", "DEVICE", "VERSION", "ORIGINAL":
				default:
					return &apiError{Type: "parse_exception", Status: 400,
						Reason: "[properties] illegal property value [" + ingestString(raw) + "]. valid values are [NAME, MAJOR, MINOR, PATCH, BUILD, OS, OS_NAME, OS_MAJOR, OS_MINOR, OS_FULL, DEVICE, VERSION, ORIGINAL]"}
				}
			}
		}
	}
	return nil
}

func (s *Server) simulatePipeline(w http.ResponseWriter, r *http.Request, stored string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	var pipeline map[string]any
	name := stored
	if stored != "" {
		s.ingestMu.RLock()
		p, ok := s.ingestPipelines[stored]
		s.ingestMu.RUnlock()
		if !ok {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "pipeline [" + stored + "] does not exist"})
			return
		}
		pipeline = p
	} else {
		raw, ok := body["pipeline"]
		if !ok {
			writeError(w, &apiError{Type: "parse_exception", Status: 400,
				Reason: "[pipeline] required property is missing", Fields: map[string]any{"property_name": "pipeline"}})
			return
		}
		pm, ok := raw.(map[string]any)
		if !ok {
			writeError(w, &apiError{Type: "parse_exception", Status: 400, Reason: "[pipeline] required property is missing",
				Fields: map[string]any{"property_name": "pipeline"}})
			return
		}
		var err *apiError
		pipeline, err = validatePipelineBody("_simulate", pm)
		if err != nil {
			writeError(w, err)
			return
		}
		name = "_simulate_pipeline"
	}
	rawDocs, ok := body["docs"]
	if !ok {
		writeError(w, &apiError{Type: "parse_exception", Status: 400,
			Reason: "[docs] required property is missing", Fields: map[string]any{"property_name": "docs"}})
		return
	}
	docs, ok := rawDocs.([]any)
	if !ok {
		writeError(w, &apiError{Type: "parse_exception", Status: 400,
			Reason: "[docs] required property is missing", Fields: map[string]any{"property_name": "docs"}})
		return
	}
	if len(docs) == 0 {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "must specify at least one document in [docs]"})
		return
	}
	verbose := r.URL.Query().Get("verbose") == "true"
	out := make([]any, 0, len(docs))
	for _, raw := range docs {
		dm, ok := raw.(map[string]any)
		if !ok {
			writeError(w, &apiError{Type: "parse_exception", Status: 400, Reason: "[docs] required property is missing"})
			return
		}
		srcRaw, ok := dm["_source"]
		if !ok {
			writeError(w, &apiError{Type: "parse_exception", Status: 400,
				Reason: "[_source] required property is missing", Fields: map[string]any{"property_name": "_source"}})
			return
		}
		src, _ := srcRaw.(map[string]any)
		if src == nil {
			src = map[string]any{}
		}
		meta := map[string]any{}
		for _, k := range []string{"_index", "_id", "_routing", "_version", "_version_type"} {
			if v, ok := dm[k]; ok {
				if k == "_version" {
					meta[k] = ingestString(v)
				} else {
					meta[k] = v
				}
			}
		}
		if _, ok := meta["_index"]; !ok {
			meta["_index"] = "_index"
		}
		if _, ok := meta["_id"]; !ok {
			meta["_id"] = "_id"
		}
		env := &ingestEnv{
			s: s, source: deepCopyValue(src).(map[string]any), meta: meta,
			pipeline: name, timestamp: ingestTimestamp(), verbose: verbose,
		}
		err, dropped := s.runIngestProcessors(pipeline, env)
		if err != nil && ((err.Status >= 500 && err.Type != "fail_processor_exception") || err.Status == 404 || err.ForceTop) {
			writeError(w, err)
			return
		}
		if err != nil {
			if verbose {
				out = append(out, map[string]any{"processor_results": env.results, "error": apiErrorObject(err)})
			} else {
				out = append(out, map[string]any{"error": apiErrorObject(err)})
			}
			continue
		}
		if dropped {
			if verbose {
				out = append(out, map[string]any{"processor_results": env.results})
			} else {
				out = append(out, nil)
			}
			continue
		}
		if verbose {
			out = append(out, map[string]any{"processor_results": env.results})
		} else {
			out = append(out, map[string]any{"doc": env.cloneDoc()})
		}
	}
	writeJSON(w, 200, map[string]any{"docs": out})
}

func (s *Server) runIngestProcessors(pipeline map[string]any, env *ingestEnv) (*apiError, bool) {
	list, _ := pipeline["processors"].([]any)
	for _, raw := range list {
		def, _ := raw.(map[string]any)
		typ, cfg, err := processorConfig(def)
		if err != nil {
			return err, false
		}
		env.processor = typ
		tagVal, _ := procControl(def, cfg, "tag")
		env.tag, _ = tagVal.(string)
		if cond, ok := procControl(def, cfg, "if"); ok {
			match, cerr := evalIngestCondition(cond, env)
			if cerr != nil {
				return s.handleProcessorFailure(def, typ, cfg, env, cerr)
			}
			if !match {
				if env.verbose {
					env.results = append(env.results, map[string]any{
						"processor_type": typ, "status": "skipped",
						"if": map[string]any{"condition": cond, "result": false},
					})
				}
				continue
			}
		}
		if typ == "pipeline" {
			if env.verbose {
				env.results = append(env.results, map[string]any{"processor_type": "pipeline", "status": "success"})
			}
		}
		dropped, perr := s.runIngestProcessor(typ, cfg, env)
		if perr != nil {
			e2, d2 := s.handleProcessorFailure(def, typ, cfg, env, perr)
			if d2 {
				return nil, true
			}
			if e2 != nil {
				if of, ok := pipeline["on_failure"].([]any); ok && len(of) > 0 {
					if herr := s.runPipelineFailureHandlers(of, env, e2); herr != nil {
						return herr, false
					}
					return nil, false
				}
				return e2, false
			}
			continue
		}
		if typ == "pipeline" {
			continue
		}
		if env.verbose {
			res := map[string]any{"processor_type": typ, "status": "success"}
			if env.tag != "" {
				res["tag"] = env.tag
			}
			if cond, ok := procControl(def, cfg, "if"); ok {
				res["if"] = map[string]any{"condition": cond, "result": true}
			}
			if dropped {
				res["status"] = "dropped"
			}
			if !dropped {
				res["doc"] = env.cloneDoc()
			}
			env.results = append(env.results, res)
		}
		if dropped {
			return nil, true
		}
	}
	return nil, false
}

func procControl(def, cfg map[string]any, key string) (any, bool) {
	if v, ok := def[key]; ok {
		return v, true
	}
	v, ok := cfg[key]
	return v, ok
}

func (s *Server) runPipelineFailureHandlers(handlers []any, env *ingestEnv, cause *apiError) *apiError {
	env.onFailure = map[string]any{
		"on_failure_message":        cause.Reason,
		"on_failure_processor_type": env.processor,
		"on_failure_processor_tag":  env.tag,
	}
	for _, hraw := range handlers {
		hdef, _ := hraw.(map[string]any)
		htyp, hcfg, err := processorConfig(hdef)
		if err != nil {
			return err
		}
		dropped, herr := s.runIngestProcessor(htyp, hcfg, env)
		if herr != nil {
			return herr
		}
		if dropped {
			return nil
		}
	}
	env.onFailure = nil
	return nil
}

func (s *Server) handleProcessorFailure(def map[string]any, typ string, cfg map[string]any, env *ingestEnv, perr *apiError) (*apiError, bool) {
	ignVal, _ := procControl(def, cfg, "ignore_failure")
	if ign, _ := ignVal.(bool); ign {
		if env.verbose {
			env.results = append(env.results, map[string]any{"processor_type": typ, "status": "error", "error": apiErrorObject(perr)})
		}
		return nil, false
	}
	ofVal, _ := procControl(def, cfg, "on_failure")
	handlers, hasHandlers := ofVal.([]any)
	if !hasHandlers {
		if env.verbose {
			env.results = append(env.results, map[string]any{"processor_type": typ, "status": "error", "error": apiErrorObject(perr)})
		}
		return perr, false
	}
	env.onFailure = map[string]any{
		"on_failure_message":        perr.Reason,
		"on_failure_processor_type": typ,
		"on_failure_processor_tag":  env.tag,
	}
	for _, hraw := range handlers {
		hdef, _ := hraw.(map[string]any)
		htyp, hcfg, err := processorConfig(hdef)
		if err != nil {
			return err, false
		}
		dropped, herr := s.runIngestProcessor(htyp, hcfg, env)
		if herr != nil {
			subVal, _ := procControl(hdef, hcfg, "on_failure")
			if sub, ok := subVal.([]any); ok && len(sub) > 0 {
				old := env.onFailure
				env.onFailure = map[string]any{
					"on_failure_message":        herr.Reason,
					"on_failure_processor_type": htyp,
					"on_failure_processor_tag":  "",
				}
				for _, sraw := range sub {
					sdef, _ := sraw.(map[string]any)
					st, sc, e := processorConfig(sdef)
					if e != nil {
						return e, false
					}
					if _, e := s.runIngestProcessor(st, sc, env); e != nil {
						return e, false
					}
				}
				env.onFailure = old
				continue
			}
			return herr, false
		}
		if dropped {
			return nil, true
		}
	}
	env.onFailure = nil
	return nil, false
}

func (s *Server) applyPipelines(idx *Index, doc map[string]any, params map[string]any) (map[string]any, *apiError) {
	pipeline, _ := params["pipeline"].(string)
	if pipeline == "" {
		if v, ok := findSetting(idx.Settings, "default_pipeline"); ok {
			pipeline = settingsValueString(v)
		}
	}
	if pipeline != "" && pipeline != "_none" {
		s.ingestMu.RLock()
		p, ok := s.ingestPipelines[pipeline]
		s.ingestMu.RUnlock()
		if !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "pipeline with id [" + pipeline + "] does not exist"}
		}
		env := &ingestEnv{s: s, source: doc, meta: map[string]any{"_index": idx.Name}, pipeline: pipeline, timestamp: ingestTimestamp()}
		if err, dropped := s.runIngestProcessors(p, env); err != nil {
			return nil, err
		} else if dropped {
			return nil, &apiError{Type: "drop_processor_exception", Status: 200, Reason: "dropped"}
		}
		for _, k := range []string{"_index", "_id", "_routing", "_version"} {
			if v, ok := env.meta[k]; ok {
				params[k] = v
			}
		}
		doc = env.source
	}
	if v, ok := findSetting(idx.Settings, "final_pipeline"); ok {
		fp := settingsValueString(v)
		if fp != "" && fp != "_none" {
			s.ingestMu.RLock()
			p, ok := s.ingestPipelines[fp]
			s.ingestMu.RUnlock()
			if !ok {
				return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "pipeline with id [" + fp + "] does not exist"}
			}
			env := &ingestEnv{s: s, source: doc, meta: map[string]any{"_index": idx.Name}, pipeline: fp, timestamp: ingestTimestamp()}
			if err, _ := s.runIngestProcessors(p, env); err != nil {
				return nil, err
			}
			if ni, _ := env.meta["_index"].(string); ni != "" && ni != idx.Name {
				return nil, &apiError{Type: "illegal_state_exception", Status: 500,
					Reason: "FINAL pipeline [" + fp + "] can't change the target index"}
			}
			doc = env.source
		}
	}
	return doc, nil
}
