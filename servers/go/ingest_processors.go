package main

import (
	"encoding/json"
	"math"
	"reflect"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

func (s *Server) runIngestProcessor(typ string, cfg map[string]any, env *ingestEnv) (bool, *apiError) {
	switch typ {
	case "set":
		return false, s.procSet(cfg, env)
	case "remove":
		return false, s.procRemove(cfg, env)
	case "rename":
		return false, s.procRename(cfg, env)
	case "copy":
		return false, s.procCopy(cfg, env)
	case "remove_by_pattern":
		return false, s.procRemoveByPattern(cfg, env)
	case "append":
		return false, s.procAppend(cfg, env)
	case "convert":
		return false, s.procConvert(cfg, env)
	case "lowercase":
		return false, s.procCase(cfg, env, strings.ToLower)
	case "uppercase":
		return false, s.procCase(cfg, env, func(x string) string { return strings.ReplaceAll(strings.ToUpper(x), "ß", "SS") })
	case "trim":
		return false, s.procCase(cfg, env, strings.TrimSpace)
	case "split":
		return false, s.procSplit(cfg, env)
	case "join":
		return false, s.procJoin(cfg, env)
	case "gsub":
		return false, s.procGsub(cfg, env)
	case "sort":
		return false, s.procSort(cfg, env)
	case "dot_expander":
		return false, s.procDotExpander(cfg, env)
	case "foreach":
		return false, s.procForeach(cfg, env)
	case "fail":
		return false, s.procFail(cfg, env)
	case "drop":
		return true, nil
	case "pipeline":
		return s.procPipeline(cfg, env)
	case "json":
		return false, s.procJSON(cfg, env)
	case "kv":
		return false, s.procKV(cfg, env)
	case "csv":
		return false, s.procCSV(cfg, env)
	case "urldecode":
		return false, s.procURLDecode(cfg, env)
	case "html_strip":
		return false, s.procHTMLStrip(cfg, env)
	case "bytes":
		return false, s.procBytes(cfg, env)
	case "dissect":
		return false, s.procDissect(cfg, env)
	case "grok":
		return false, s.procGrok(cfg, env)
	case "date":
		return false, s.procDate(cfg, env)
	case "date_index_name":
		return false, s.procDateIndexName(cfg, env)
	case "script":
		return false, s.procScript(cfg, env)
	case "fingerprint":
		return false, s.procFingerprint(cfg, env)
	case "community_id":
		return false, s.procCommunityID(cfg, env)
	case "user_agent":
		return false, s.procUserAgent(cfg, env)
	case "split_to_fields":
		return false, s.procSplitToFields(cfg, env)
	}
	return false, &apiError{Type: "parse_exception", Status: 400, Reason: "No processor type exists with name [" + typ + "]"}
}

func cfgString(cfg map[string]any, key string) string {
	v, _ := cfg[key].(string)
	return v
}

func cfgBool(cfg map[string]any, key string, def bool) bool {
	if v, ok := cfg[key].(bool); ok {
		return v
	}
	return def
}

func (s *Server) fieldMissing(field string, ignore bool) *apiError {
	if ignore {
		return nil
	}
	return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "field [" + field + "] doesn't exist"}
}

func (e *ingestEnv) resolveFieldPath(raw any) []string {
	switch t := raw.(type) {
	case string:
		return []string{t}
	case []any:
		out := make([]string, 0, len(t))
		for _, v := range t {
			out = append(out, ingestString(v))
		}
		return out
	}
	return nil
}

func (s *Server) procSet(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	value := cfg["value"]
	if sv, ok := value.(string); ok {
		value = env.render(sv)
		if cfgBool(cfg, "ignore_empty_value", false) && value == "" && strings.Contains(sv, "{{") {
			return nil
		}
	}
	return env.setField(field, deepCopyValue(value), cfgBool(cfg, "override", true))
}

func (s *Server) procRemove(cfg map[string]any, env *ingestEnv) *apiError {
	ignore := cfgBool(cfg, "ignore_missing", false)
	fields := env.resolveFieldPath(cfg["field"])
	for _, f := range fields {
		f = env.render(f)
		if strings.Contains(f, "*") {
			s.removeGlob(env, f, nil)
			continue
		}
		if strings.HasPrefix(f, "_") {
			if _, ok := env.meta[f]; !ok {
				if err := s.fieldMissing(f, ignore); err != nil {
					return err
				}
				continue
			}
			delete(env.meta, f)
			continue
		}
		if !removeNested(env.source, f) {
			if err := s.fieldMissing(f, ignore); err != nil {
				return err
			}
		}
	}
	if ex, ok := cfg["exclude_field"]; ok {
		excludes := env.resolveFieldPath(ex)
		for _, k := range collectLeafPaths(env.source, "") {
			skip := false
			for _, p := range excludes {
				if globMatch(p, k) {
					skip = true
					break
				}
			}
			if !skip {
				removeNested(env.source, k)
			}
		}
	}
	return nil
}

func (s *Server) removeGlob(env *ingestEnv, pattern string, exclude []string) {
	keys := collectLeafPaths(env.source, "")
	for _, k := range keys {
		if globMatch(pattern, k) {
			skip := false
			for _, ex := range exclude {
				if globMatch(ex, k) {
					skip = true
				}
			}
			if !skip {
				removeNested(env.source, k)
			}
		}
	}
}

func collectLeafPaths(v any, prefix string) []string {
	m, ok := v.(map[string]any)
	if !ok {
		if prefix != "" {
			return []string{prefix}
		}
		return nil
	}
	out := []string{}
	for k, vv := range m {
		p := k
		if prefix != "" {
			p = prefix + "." + k
		}
		if _, ok := vv.(map[string]any); ok {
			out = append(out, collectLeafPaths(vv, p)...)
		} else {
			out = append(out, p)
		}
	}
	return out
}

func (s *Server) procRemoveByPattern(cfg map[string]any, env *ingestEnv) *apiError {
	var patterns, excludes []string
	if v, ok := cfg["field_pattern"]; ok {
		patterns = env.resolveFieldPath(v)
	}
	if v, ok := cfg["exclude_field_pattern"]; ok {
		excludes = env.resolveFieldPath(v)
	}
	keys := collectLeafPaths(env.source, "")
	for _, k := range keys {
		matched := false
		for _, p := range patterns {
			if globMatch(p, k) {
				matched = true
				break
			}
		}
		if len(patterns) == 0 {
			matched = true
		}
		if !matched {
			continue
		}
		excluded := false
		for _, p := range excludes {
			if globMatch(p, k) {
				excluded = true
				break
			}
		}
		if !excluded {
			removeNested(env.source, k)
		}
	}
	return nil
}

func (s *Server) procRename(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	ignore := cfgBool(cfg, "ignore_missing", false)
	if field == "" || field == target {
		return nil
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, ignore)
	}
	if _, exists := env.lookup(target); exists && !cfgBool(cfg, "override_target", false) {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "field [" + target + "] already exists"}
	}
	if err := env.setField(target, v, true); err != nil {
		return err
	}
	if strings.HasPrefix(field, "_") {
		delete(env.meta, field)
	} else {
		removeNested(env.source, field)
	}
	return nil
}

func (s *Server) procCopy(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "source_field"))
	target := env.render(cfgString(cfg, "target_field"))
	ignore := cfgBool(cfg, "ignore_missing", false)
	if field == target {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "source field path and target field path cannot be same"}
	}
	v, ok := env.lookup(field)
	if !ok {
		if ignore {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "source field [" + field + "] doesn't exist"}
	}
	if _, exists := env.lookup(target); exists && !cfgBool(cfg, "override_target", false) {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "target field [" + target + "] already exists"}
	}
	if err := env.setField(target, deepCopyValue(v), true); err != nil {
		return err
	}
	if cfgBool(cfg, "remove_source", false) {
		if strings.HasPrefix(field, "_") {
			delete(env.meta, field)
		} else {
			removeNested(env.source, field)
		}
	}
	return nil
}

func (s *Server) procAppend(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	values := []any{}
	switch t := cfg["value"].(type) {
	case []any:
		values = append(values, t...)
	default:
		values = append(values, t)
	}
	for i, v := range values {
		if sv, ok := v.(string); ok {
			values[i] = env.render(sv)
		}
	}
	existing, ok := env.lookup(field)
	allowDup := cfgBool(cfg, "allow_duplicates", true)
	var out []any
	added := 0
	if !ok {
		out = append(out, values...)
	} else {
		if list, ok := existing.([]any); ok {
			out = append(out, list...)
		} else {
			out = append(out, existing)
		}
		for _, v := range values {
			if !allowDup && containsDeep(out, v) {
				continue
			}
			out = append(out, v)
			added++
		}
		if added == 0 && len(values) > 0 {
			if _, isList := existing.([]any); !isList {
				return env.setField(field, existing, true)
			}
		}
	}
	return env.setField(field, out, true)
}

func containsDeep(list []any, v any) bool {
	for _, x := range list {
		if reflect.DeepEqual(x, v) {
			return true
		}
	}
	return false
}

func ingestInt(v any) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case int:
		return int64(t), true
	case json.Number:
		if i, err := t.Int64(); err == nil {
			return i, true
		}
		if f, err := t.Float64(); err == nil && f == math.Trunc(f) {
			return int64(f), true
		}
	case float64:
		if t == math.Trunc(t) && t >= math.MinInt64 && t <= math.MaxInt64 {
			return int64(t), true
		}
	}
	return 0, false
}

func ingestFloat(v any) (float64, bool) {
	switch t := v.(type) {
	case float64:
		return t, true
	case int64:
		return float64(t), true
	case int:
		return float64(t), true
	case json.Number:
		if f, err := t.Float64(); err == nil {
			return f, true
		}
	}
	return 0, false
}

func numberFormatError(input string) *apiError {
	return &apiError{Type: "number_format_exception", Status: 400, Reason: "For input string: \"" + input + "\""}
}

func (s *Server) convertOne(v any, typ string) (any, *apiError) {
	if v == nil {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Field is null, cannot be converted to type [" + typ + "]"}
	}
	switch typ {
	case "integer", "long":
		if i, ok := ingestInt(v); ok {
			if typ == "integer" && (i < math.MinInt32 || i > math.MaxInt32) {
				return nil, numberFormatError(ingestString(v))
			}
			return i, nil
		}
		str := ingestString(v)
		if _, isStr := v.(string); isStr {
			i, err := strconv.ParseInt(str, 0, 64)
			if err != nil {
				return nil, numberFormatError(str)
			}
			if typ == "integer" && (i < math.MinInt32 || i > math.MaxInt32) {
				return nil, numberFormatError(str)
			}
			return i, nil
		}
		return nil, numberFormatError(str)
	case "float", "double":
		if f, ok := ingestFloat(v); ok {
			if math.IsNaN(f) {
				return "NaN", nil
			}
			return f, nil
		}
		str := ingestString(v)
		if _, isStr := v.(string); isStr {
			f, err := strconv.ParseFloat(str, 64)
			if err != nil {
				return nil, numberFormatError(str)
			}
			if math.IsNaN(f) {
				return "NaN", nil
			}
			return f, nil
		}
		return nil, numberFormatError(str)
	case "boolean":
		if b, ok := v.(bool); ok {
			return b, nil
		}
		if str, ok := v.(string); ok {
			switch strings.ToLower(str) {
			case "true":
				return true, nil
			case "false":
				return false, nil
			}
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[" + str + "] is not a boolean value, cannot convert to boolean"}
		}
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[" + ingestString(v) + "] is not a boolean value, cannot convert to boolean"}
	case "string":
		return ingestString(v), nil
	case "auto":
		switch t := v.(type) {
		case string:
			if i, err := strconv.ParseInt(t, 10, 64); err == nil {
				return i, nil
			}
			if f, err := strconv.ParseFloat(t, 64); err == nil {
				return f, nil
			}
			if b, err := strconv.ParseBool(t); err == nil && (t == "true" || t == "false") {
				return b, nil
			}
			return t, nil
		case json.Number:
			if i, err := t.Int64(); err == nil {
				return i, nil
			}
			if f, err := t.Float64(); err == nil {
				return f, nil
			}
		}
		return v, nil
	case "ip":
		str := ingestString(v)
		if _, ok := parseIP(str); !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[" + str + "] is not a valid ipv4/ipv6 address"}
		}
		return str, nil
	}
	return v, nil
}

func (s *Server) procConvert(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	typ := cfgString(cfg, "type")
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		if cfgBool(cfg, "ignore_missing", false) {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	if v == nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Field [" + field + "] is null, cannot be converted to type [" + typ + "]"}
	}
	if list, ok := v.([]any); ok {
		out := make([]any, 0, len(list))
		for _, e := range list {
			cv, err := s.convertOne(e, typ)
			if err != nil {
				return convertWrap(err, e, typ)
			}
			out = append(out, cv)
		}
		return env.setField(target, out, true)
	}
	cv, err := s.convertOne(v, typ)
	if err != nil {
		return convertWrap(err, v, typ)
	}
	return env.setField(target, cv, true)
}

func convertWrap(err *apiError, v any, typ string) *apiError {
	if err.Type == "number_format_exception" {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "unable to convert [" + ingestString(v) + "] to " + typ, CausedBy: err}
	}
	return err
}

func (s *Server) procCase(cfg map[string]any, env *ingestEnv, fn func(string) string) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		if cfgBool(cfg, "ignore_missing", false) {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	if v == nil {
		if cfgBool(cfg, "ignore_missing", false) {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "field [" + field + "] is null, cannot process it."}
	}
	transform := func(x any) (any, *apiError) {
		str, ok := x.(string)
		if !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "field [" + field + "] of type [" + javaTypeName(x) + "] cannot be cast to [java.lang.String]"}
		}
		return fn(str), nil
	}
	if list, ok := v.([]any); ok {
		out := make([]any, 0, len(list))
		for _, e := range list {
			if e == nil {
				return &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "value [null] of type [null] in list field [" + field + "] cannot be cast to [java.lang.String]"}
			}
			if _, ok := e.(string); !ok {
				return &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "value [" + ingestString(e) + "] of type [" + javaTypeName(e) + "] in list field [" + field + "] cannot be cast to [java.lang.String]"}
			}
			tv, err := transform(e)
			if err != nil {
				return err
			}
			out = append(out, tv)
		}
		return env.setField(target, out, true)
	}
	tv, err := transform(v)
	if err != nil {
		return err
	}
	return env.setField(target, tv, true)
}

func (s *Server) procSplit(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	sep := cfgString(cfg, "separator")
	re, err := regexp.Compile(sep)
	if err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	parts := re.Split(str, -1)
	if !cfgBool(cfg, "preserve_trailing", false) && str != "" {
		for len(parts) > 0 && parts[len(parts)-1] == "" {
			parts = parts[:len(parts)-1]
		}
	}
	out := make([]any, len(parts))
	for i, p := range parts {
		out[i] = p
	}
	return env.setField(target, out, true)
}

func (s *Server) procJoin(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	list, ok := v.([]any)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.util.List]"}
	}
	sep := cfgString(cfg, "separator")
	parts := make([]string, len(list))
	for i, e := range list {
		if e == nil {
			return &apiError{Type: "null_pointer_exception", Status: 400, Reason: "Cannot invoke \"Object.toString()\" because \"value\" is null"}
		}
		parts[i] = ingestString(e)
	}
	return env.setField(target, strings.Join(parts, sep), true)
}

func javaRegexReplace(repl string) string {
	re := regexp.MustCompile(`\$(\d+)`)
	return re.ReplaceAllString(repl, `${$1}`)
}

func (s *Server) procGsub(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	re, err := regexp.Compile(cfgString(cfg, "pattern"))
	if err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	repl := javaRegexReplace(cfgString(cfg, "replacement"))
	apply := func(x any) (any, *apiError) {
		str, ok := x.(string)
		if !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "field [" + field + "] of type [" + javaTypeName(x) + "] cannot be cast to [java.lang.String]"}
		}
		return re.ReplaceAllString(str, repl), nil
	}
	if list, ok := v.([]any); ok {
		out := make([]any, 0, len(list))
		for _, e := range list {
			tv, err := apply(e)
			if err != nil {
				return err
			}
			out = append(out, tv)
		}
		return env.setField(target, out, true)
	}
	tv, applyErr := apply(v)
	if applyErr != nil {
		return applyErr
	}
	return env.setField(target, tv, true)
}

func (s *Server) procSort(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	list, ok := v.([]any)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.util.List]"}
	}
	allNum, allStr := true, true
	for _, e := range list {
		if _, ok := ingestFloat(e); !ok {
			allNum = false
		}
		if _, ok := e.(string); !ok {
			allStr = false
		}
	}
	if !allNum && !allStr {
		return &apiError{Type: "class_cast_exception", Status: 400, Reason: "cannot sort values of mixed types"}
	}
	out := make([]any, len(list))
	copy(out, list)
	desc := cfgString(cfg, "order") == "desc"
	sort.SliceStable(out, func(i, j int) bool {
		if allNum {
			a, _ := ingestFloat(out[i])
			b, _ := ingestFloat(out[j])
			if desc {
				return a > b
			}
			return a < b
		}
		a := out[i].(string)
		b := out[j].(string)
		if desc {
			return a > b
		}
		return a < b
	})
	return env.setField(target, out, true)
}

func (s *Server) procDotExpander(cfg map[string]any, env *ingestEnv) *apiError {
	field := cfgString(cfg, "field")
	if field == "" || !strings.Contains(field, ".") {
		return nil
	}
	path := env.render(cfgString(cfg, "path"))
	base := env.source
	if path != "" {
		if pm, ok := lookupNested(env.source, strings.Split(path, "."), true); ok {
			base, _ = pm.(map[string]any)
		}
		if base == nil {
			return nil
		}
	}
	v, ok := base[field]
	if !ok {
		return nil
	}
	full := field
	if path != "" {
		full = path + "." + field
	}
	parts := strings.Split(full, ".")
	cur := env.source
	for i := 0; i < len(parts)-1; i++ {
		next, exists := cur[parts[i]]
		if !exists {
			m := map[string]any{}
			cur[parts[i]] = m
			cur = m
			continue
		}
		m, ok := next.(map[string]any)
		if !ok {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "cannot expend [" + full + "], because [" + strings.Join(parts[:i+1], ".") + "] is not an object field, but a value field"}
		}
		cur = m
	}
	last := parts[len(parts)-1]
	override := cfgBool(cfg, "override", false)
	if old, exists := cur[last]; exists && !override {
		if list, ok := old.([]any); ok {
			cur[last] = append(list, v)
		} else {
			cur[last] = []any{old, v}
		}
	} else {
		cur[last] = v
	}
	delete(base, field)
	return nil
}

func (s *Server) procForeach(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok || v == nil {
		if cfgBool(cfg, "ignore_missing", false) {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]", ForceTop: true}
	}
	list, ok := v.([]any)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.util.List]", ForceTop: true}
	}
	inner, ok := cfg["processor"].(map[string]any)
	if !ok {
		return &apiError{Type: "parse_exception", Status: 400, Reason: "[processor] required property is missing",
			Fields: map[string]any{"processor_type": "foreach", "property_name": "processor"}}
	}
	typ, icfg, err := processorConfig(inner)
	if err != nil {
		return err
	}
	for i := range list {
		oldVal, oldSet := env.foreachVal, env.foreachSet
		idx := i
		env.foreachVal = list[i]
		env.foreachSet = func(nv any) {
			list[idx] = nv
			if err := env.setField(field, list, true); err != nil {
				_ = err
			}
		}
		_, perr := s.runIngestProcessor(typ, icfg, env)
		env.foreachVal, env.foreachSet = oldVal, oldSet
		if perr != nil {
			return perr
		}
	}
	return env.setField(field, list, true)
}

func (s *Server) procFail(cfg map[string]any, env *ingestEnv) *apiError {
	msg := env.render(cfgString(cfg, "message"))
	return &apiError{Type: "fail_processor_exception", Status: 500, Reason: msg}
}

func (s *Server) procPipeline(cfg map[string]any, env *ingestEnv) (bool, *apiError) {
	name := env.render(cfgString(cfg, "name"))
	if name == "" {
		return false, &apiError{Type: "illegal_state_exception", Status: 400, Reason: "Pipeline processor configured for empty pipeline name"}
	}
	for _, n := range env.stack {
		if n == name {
			return false, &apiError{Type: "illegal_state_exception", Status: 400, Reason: "Cycle detected for pipeline: " + name}
		}
	}
	s.ingestMu.RLock()
	p, ok := s.ingestPipelines[name]
	s.ingestMu.RUnlock()
	if !ok {
		return false, &apiError{Type: "illegal_state_exception", Status: 400,
			Reason: "Pipeline processor configured for non-existent pipeline [" + name + "]"}
	}
	oldPipeline := env.pipeline
	env.stack = append(env.stack, name)
	env.pipeline = name
	err, dropped := s.runIngestProcessors(p, env)
	env.pipeline = oldPipeline
	env.stack = env.stack[:len(env.stack)-1]
	return dropped, err
}
