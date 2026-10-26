package main

import (
	"net/url"
	"strings"
)

func splitSourceList(raw string) []string {
	out := []string{}
	for _, part := range strings.Split(raw, ",") {
		part = strings.TrimSpace(part)
		if part != "" {
			out = append(out, part)
		}
	}
	return out
}

func anyStringList(v any) []string {
	out := []string{}
	switch t := v.(type) {
	case string:
		out = append(out, t)
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				out = append(out, s)
			}
		}
	}
	return out
}

func sourceFilterFromBody(body map[string]any) (includes, excludes []string, omit bool, ok bool) {
	raw, exists := body["_source"]
	if !exists {
		return nil, nil, false, false
	}
	switch t := raw.(type) {
	case bool:
		return nil, nil, !t, true
	case string:
		if t == "false" {
			return nil, nil, true, true
		}
		if t == "true" || t == "" {
			return nil, nil, false, true
		}
		return splitSourceList(t), nil, false, true
	case []any:
		return anyStringList(t), nil, false, true
	case map[string]any:
		inc := anyStringList(t["includes"])
		if len(inc) == 0 {
			inc = anyStringList(t["include"])
		}
		exc := anyStringList(t["excludes"])
		if len(exc) == 0 {
			exc = anyStringList(t["exclude"])
		}
		return inc, exc, false, true
	}
	return nil, nil, false, false
}

func sourceFilterFromQuery(q url.Values) (includes, excludes []string, omit bool, ok bool) {
	if v := q.Get("_source"); v != "" {
		if v == "false" {
			return nil, nil, true, true
		}
		if v == "true" {
			return nil, nil, false, true
		}
		includes = splitSourceList(v)
	}
	if v := q.Get("_source_includes"); v != "" {
		includes = splitSourceList(v)
	}
	if v := q.Get("_source_excludes"); v != "" {
		excludes = splitSourceList(v)
	}
	return includes, excludes, false, len(includes) > 0 || len(excludes) > 0
}

func wildcardMatch(pattern, s string) bool {
	pi, si := 0, 0
	star, mark := -1, 0
	for si < len(s) {
		if pi < len(pattern) && (pattern[pi] == '?' || pattern[pi] == s[si]) {
			pi++
			si++
		} else if pi < len(pattern) && pattern[pi] == '*' {
			star = pi
			mark = si
			pi++
		} else if star != -1 {
			pi = star + 1
			mark++
			si = mark
		} else {
			return false
		}
	}
	for pi < len(pattern) && pattern[pi] == '*' {
		pi++
	}
	return pi == len(pattern)
}

func deepCopySource(v any) any {
	switch t := v.(type) {
	case map[string]any:
		out := make(map[string]any, len(t))
		for k, e := range t {
			out[k] = deepCopySource(e)
		}
		return out
	case []any:
		out := make([]any, len(t))
		for i, e := range t {
			out[i] = deepCopySource(e)
		}
		return out
	default:
		return v
	}
}

func includeSourcePath(dst, src map[string]any, segs []string) {
	if len(segs) == 0 {
		return
	}
	seg := segs[0]
	last := len(segs) == 1
	for key, val := range src {
		if !wildcardMatch(seg, key) {
			continue
		}
		if last {
			dst[key] = deepCopySource(val)
			continue
		}
		switch t := val.(type) {
		case map[string]any:
			child, _ := dst[key].(map[string]any)
			if child == nil {
				child = map[string]any{}
				dst[key] = child
			}
			includeSourcePath(child, t, segs[1:])
		case []any:
			arr := []any{}
			for _, elem := range t {
				if em, ok := elem.(map[string]any); ok {
					child := map[string]any{}
					includeSourcePath(child, em, segs[1:])
					if len(child) > 0 {
						arr = append(arr, child)
					}
				}
			}
			if len(arr) > 0 {
				dst[key] = arr
			}
		}
	}
}

func excludeSourcePath(dst map[string]any, segs []string) {
	if len(segs) == 0 {
		return
	}
	seg := segs[0]
	last := len(segs) == 1
	for key, val := range dst {
		if !wildcardMatch(seg, key) {
			continue
		}
		if last {
			delete(dst, key)
			continue
		}
		switch t := val.(type) {
		case map[string]any:
			excludeSourcePath(t, segs[1:])
		case []any:
			for _, elem := range t {
				if em, ok := elem.(map[string]any); ok {
					excludeSourcePath(em, segs[1:])
				}
			}
		}
	}
}

func filterSourceDoc(doc map[string]any, includes, excludes []string) map[string]any {
	if len(includes) == 0 && len(excludes) == 0 {
		return doc
	}
	out := map[string]any{}
	if len(includes) == 0 {
		for k, v := range doc {
			out[k] = deepCopySource(v)
		}
	} else {
		for _, inc := range includes {
			segs := strings.Split(inc, ".")
			includeSourcePath(out, doc, segs)
		}
	}
	for _, exc := range excludes {
		excludeSourcePath(out, strings.Split(exc, "."))
	}
	return out
}
