package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
)

// ---------------------------------------------------------------- pretty JSON

func prettyJSON(v any) []byte {
	var buf bytes.Buffer
	writePretty(&buf, v, 0)
	buf.WriteByte('\n')
	return buf.Bytes()
}

func writePretty(buf *bytes.Buffer, v any, indent int) {
	switch t := v.(type) {
	case map[string]any:
		writePrettyObject(buf, t, indent)
	case []any:
		writePrettyArray(buf, t, indent)
	case string:
		buf.WriteString(encodeString(t))
	case json.Number:
		buf.WriteString(t.String())
	case float64:
		buf.WriteString(formatNumber(t))
	case bool:
		if t {
			buf.WriteString("true")
		} else {
			buf.WriteString("false")
		}
	case nil:
		buf.WriteString("null")
	default:
		b, _ := json.Marshal(t)
		buf.Write(b)
	}
}

func writePrettyObject(buf *bytes.Buffer, m map[string]any, indent int) {
	if len(m) == 0 {
		buf.WriteString("{ }")
		return
	}
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sortStrings(keys)
	buf.WriteString("{\n")
	for i, k := range keys {
		writeIndent(buf, indent+1)
		buf.WriteString(encodeString(k))
		buf.WriteString(" : ")
		writePretty(buf, m[k], indent+1)
		if i < len(keys)-1 {
			buf.WriteByte(',')
		}
		buf.WriteByte('\n')
	}
	writeIndent(buf, indent)
	buf.WriteByte('}')
}

func writePrettyArray(buf *bytes.Buffer, a []any, indent int) {
	if len(a) == 0 {
		buf.WriteString("[ ]")
		return
	}
	buf.WriteString("[\n")
	for i, e := range a {
		writeIndent(buf, indent+1)
		writePretty(buf, e, indent+1)
		if i < len(a)-1 {
			buf.WriteByte(',')
		}
		buf.WriteByte('\n')
	}
	writeIndent(buf, indent)
	buf.WriteByte(']')
}

func writeIndent(buf *bytes.Buffer, n int) {
	for i := 0; i < n; i++ {
		buf.WriteString("  ")
	}
}

func formatNumber(f float64) string {
	if f == float64(int64(f)) && f < 1e15 && f > -1e15 {
		return strconv.FormatInt(int64(f), 10)
	}
	return strconv.FormatFloat(f, 'g', -1, 64)
}

var jsonStringEscaper = strings.NewReplacer(
	"\\", "\\\\", "\"", "\\\"", "\n", "\\n", "\r", "\\r", "\t", "\\t",
	"\b", "\\b", "\f", "\\f",
)

func encodeString(s string) string {
	var sb strings.Builder
	sb.WriteByte('"')
	for _, r := range s {
		if r < 0x20 {
			sb.WriteString(fmt.Sprintf("\\u%04x", r))
		} else {
			sb.WriteString(jsonStringEscaper.Replace(string(r)))
		}
	}
	sb.WriteByte('"')
	return sb.String()
}

func sortStrings(a []string) {
	for i := 1; i < len(a); i++ {
		for j := i; j > 0 && a[j] < a[j-1]; j-- {
			a[j], a[j-1] = a[j-1], a[j]
		}
	}
}

// --------------------------------------------------------------- filter_path

type pathPattern struct {
	exclude bool
	segs    []string
}

func parseFilterPath(fp string) []pathPattern {
	pats := []pathPattern{}
	for _, raw := range strings.Split(fp, ",") {
		raw = strings.TrimSpace(raw)
		if raw == "" {
			continue
		}
		p := pathPattern{}
		if strings.HasPrefix(raw, "-") {
			p.exclude = true
			raw = raw[1:]
		}
		p.segs = strings.Split(raw, ".")
		pats = append(pats, p)
	}
	return pats
}

func globMatch(pat, s string) bool {
	if pat == "*" || pat == s {
		return true
	}
	if !strings.Contains(pat, "*") {
		return false
	}
	parts := strings.Split(pat, "*")
	if !strings.HasPrefix(s, parts[0]) {
		return false
	}
	s = s[len(parts[0]):]
	for i := 1; i < len(parts)-1; i++ {
		idx := strings.Index(s, parts[i])
		if idx < 0 {
			return false
		}
		s = s[idx+len(parts[i]):]
	}
	return strings.HasSuffix(s, parts[len(parts)-1])
}

func filterPathValue(v any, fp string) any {
	pats := parseFilterPath(fp)
	var includes, excludes [][]string
	for _, p := range pats {
		if p.exclude {
			excludes = append(excludes, expandDoubleStar(p.segs)...)
		} else {
			includes = append(includes, expandDoubleStar(p.segs)...)
		}
	}
	var out any
	if len(includes) == 0 {
		out = deepCopyJSON(v)
	} else {
		res, matched := filterNode(v, includes)
		if !matched {
			out = map[string]any{}
		} else {
			out = res
		}
	}
	if len(excludes) > 0 {
		excl := make([]pathPattern, 0, len(excludes))
		for _, segs := range excludes {
			excl = append(excl, pathPattern{exclude: true, segs: segs})
		}
		out = prunePaths(out, excl, nil)
	}
	return out
}

// expandDoubleStar rewrites a leading "**" into concrete patterns of one to
// eight wildcard segments, mirroring OpenSearch's "**" (which must match at
// least one path element).
func expandDoubleStar(segs []string) [][]string {
	if len(segs) == 0 || segs[0] != "**" {
		return [][]string{segs}
	}
	rest := segs[1:]
	out := [][]string{}
	for depth := 1; depth <= 8; depth++ {
		pat := make([]string, 0, depth+len(rest))
		for i := 0; i < depth; i++ {
			pat = append(pat, "*")
		}
		pat = append(pat, rest...)
		out = append(out, pat)
	}
	return out
}

func filterNode(v any, pats [][]string) (any, bool) {
	for _, p := range pats {
		if len(p) == 0 {
			return deepCopyJSON(v), true
		}
	}
	switch t := v.(type) {
	case map[string]any:
		out := map[string]any{}
		for k, val := range t {
			var sub [][]string
			for _, p := range pats {
				if len(p) > 0 && globMatch(p[0], k) {
					sub = append(sub, p[1:])
				}
			}
			if len(sub) == 0 {
				continue
			}
			res, matched := filterNode(val, sub)
			if matched {
				out[k] = res
			}
		}
		return out, len(out) > 0
	case []any:
		out := []any{}
		matchedAny := false
		for _, e := range t {
			res, matched := filterNode(e, pats)
			if matched {
				out = append(out, res)
				matchedAny = true
			}
		}
		return out, matchedAny
	default:
		return nil, false
	}
}

func deepCopyJSON(v any) any {
	switch t := v.(type) {
	case map[string]any:
		m := make(map[string]any, len(t))
		for k, val := range t {
			m[k] = deepCopyJSON(val)
		}
		return m
	case []any:
		a := make([]any, len(t))
		for i, e := range t {
			a[i] = deepCopyJSON(e)
		}
		return a
	default:
		return v
	}
}

func collectPath(v any, segs []string, path []string, out any) {
	if len(segs) == 0 {
		insertPath(out, path, deepCopyJSON(v))
		return
	}
	seg := segs[0]
	if seg == "**" {
		collectPath(v, segs[1:], path, out)
		switch t := v.(type) {
		case map[string]any:
			for k, val := range t {
				collectPath(val, segs, append(path, k), out)
			}
		case []any:
			for _, e := range t {
				collectPath(e, segs, path, out)
			}
		}
		return
	}
	switch t := v.(type) {
	case map[string]any:
		for k, val := range t {
			if globMatch(seg, k) {
				collectPath(val, segs[1:], append(path, k), out)
			}
		}
	case []any:
		if n, err := strconv.Atoi(seg); err == nil {
			if n >= 0 && n < len(t) {
				collectPath(t[n], segs[1:], path, out)
			}
			return
		}
		for _, e := range t {
			collectPath(e, segs, path, out)
		}
	}
}

func insertPath(out any, path []string, v any) {
	m, ok := out.(map[string]any)
	if !ok || len(path) == 0 {
		return
	}
	cur := m
	for i := 0; i < len(path)-1; i++ {
		next, ok := cur[path[i]].(map[string]any)
		if !ok {
			next = map[string]any{}
			cur[path[i]] = next
		}
		cur = next
	}
	last := path[len(path)-1]
	if existing, ok := cur[last]; ok {
		cur[last] = mergeJSON(existing, v)
	} else {
		cur[last] = v
	}
}

func mergeJSON(a, b any) any {
	am, aok := a.(map[string]any)
	bm, bok := b.(map[string]any)
	if aok && bok {
		for k, v := range bm {
			if ev, ok := am[k]; ok {
				am[k] = mergeJSON(ev, v)
			} else {
				am[k] = v
			}
		}
		return am
	}
	return b
}

func prunePaths(v any, excl []pathPattern, path []string) any {
	switch t := v.(type) {
	case map[string]any:
		for k, val := range t {
			p := append(path, k)
			if pathExcluded(excl, p, false) {
				delete(t, k)
				continue
			}
			t[k] = prunePaths(val, excl, p)
		}
		return t
	case []any:
		for i, e := range t {
			t[i] = prunePaths(e, excl, path)
		}
		return t
	default:
		return v
	}
}

func pathExcluded(excl []pathPattern, path []string, partial bool) bool {
	for _, p := range excl {
		if matchPathPattern(p.segs, path) {
			return true
		}
	}
	return false
}

func matchPathPattern(segs, path []string) bool {
	if len(segs) == 0 {
		return len(path) == 0
	}
	if segs[0] == "**" {
		for i := 0; i <= len(path); i++ {
			if matchPathPattern(segs[1:], path[i:]) {
				return true
			}
		}
		return false
	}
	if len(path) == 0 {
		return false
	}
	if !globMatch(segs[0], path[0]) {
		return false
	}
	return matchPathPattern(segs[1:], path[1:])
}

// enforce sorted output for deterministic responses
func sortedKeys(m map[string]any) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sortStrings(keys)
	return keys
}
