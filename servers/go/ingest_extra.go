package main

import (
	"crypto/md5"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha3"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"net/url"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

func parseIP(s string) (net.IP, bool) {
	ip := net.ParseIP(s)
	if ip == nil {
		return nil, false
	}
	return ip, true
}

func (s *Server) procJSON(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	dec := json.NewDecoder(strings.NewReader(str))
	dec.UseNumber()
	var parsed any
	if err := dec.Decode(&parsed); err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "failed to parse JSON", CausedBy: &apiError{Type: "json_parse_exception", Status: 400, Reason: err.Error()}}
	}
	parsed = normalizeJSONNumbers(parsed)
	if cfgBool(cfg, "add_to_root", false) {
		m, ok := parsed.(map[string]any)
		if !ok {
			return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "cannot add non-map fields to root of document"}
		}
		for k, vv := range m {
			env.source[k] = vv
		}
		return nil
	}
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	return env.setField(target, parsed, true)
}

func normalizeJSONNumbers(v any) any {
	switch t := v.(type) {
	case json.Number:
		if i, err := t.Int64(); err == nil {
			return i
		}
		if f, err := t.Float64(); err == nil {
			return f
		}
		return t.String()
	case map[string]any:
		for k, vv := range t {
			t[k] = normalizeJSONNumbers(vv)
		}
		return t
	case []any:
		for i, vv := range t {
			t[i] = normalizeJSONNumbers(vv)
		}
		return t
	}
	return v
}

func (s *Server) procKV(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	fieldSplit, err := regexp.Compile(cfgString(cfg, "field_split"))
	if err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	valueSplit, err := regexp.Compile(cfgString(cfg, "value_split"))
	if err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	include := stringSet(cfg["include_keys"])
	exclude := stringSet(cfg["exclude_keys"])
	prefix := cfgString(cfg, "prefix")
	trimKey := cfgString(cfg, "trim_key")
	trimVal := cfgString(cfg, "trim_value")
	stripBrackets := cfgBool(cfg, "strip_brackets", false)
	target := env.render(cfgString(cfg, "target_field"))
	out := map[string]any{}
	if target != "" {
		if old, ok := env.lookup(target); ok {
			if m, ok := old.(map[string]any); ok {
				for k, vv := range m {
					out[k] = vv
				}
			}
		}
	}
	matched := false
	for _, token := range fieldSplit.Split(str, -1) {
		if token == "" {
			continue
		}
		loc := valueSplit.FindStringIndex(token)
		if loc == nil {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "field [" + field + "] does not contain value_split [" + cfgString(cfg, "value_split") + "]"}
		}
		key := token[:loc[0]]
		val := token[loc[1]:]
		if key == "" {
			return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "path cannot be null nor empty"}
		}
		matched = true
		if trimKey != "" {
			key = strings.Trim(key, trimKey)
		}
		if trimVal != "" {
			val = strings.Trim(val, trimVal)
		}
		if stripBrackets {
			val = stripSurroundingBrackets(val)
			key = stripSurroundingBrackets(key)
		}
		if len(include) > 0 && !include[key] {
			continue
		}
		if exclude[key] {
			continue
		}
		key = prefix + key
		if old, exists := out[key]; exists {
			switch t := old.(type) {
			case []any:
				out[key] = append(t, val)
			default:
				out[key] = []any{old, val}
			}
		} else {
			out[key] = val
		}
	}
	if !matched {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] does not contain value_split [" + cfgString(cfg, "value_split") + "]"}
	}
	if target == "" {
		for k, vv := range out {
			if old, exists := env.source[k]; exists {
				if list, ok := old.([]any); ok {
					env.source[k] = append(list, vv)
				} else {
					env.source[k] = []any{old, vv}
				}
			} else {
				env.source[k] = vv
			}
		}
		return nil
	}
	return env.setField(target, out, true)
}

func stripSurroundingBrackets(s string) string {
	if len(s) < 2 {
		return s
	}
	pairs := map[byte]byte{')': '(', ']': '[', '>': '<', '"': '"', '\'': '\''}
	if close, ok := pairs[s[len(s)-1]]; ok && s[0] == close {
		return s[1 : len(s)-1]
	}
	return s
}

func stringSet(v any) map[string]bool {
	out := map[string]bool{}
	switch t := v.(type) {
	case []any:
		for _, e := range t {
			out[ingestString(e)] = true
		}
	case string:
		out[t] = true
	}
	return out
}

func (s *Server) procCSV(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	sep := ','
	if sv := cfgString(cfg, "separator"); sv != "" {
		sep = []rune(sv)[0]
	}
	quote := '"'
	if qv := cfgString(cfg, "quote"); qv != "" {
		quote = []rune(qv)[0]
	}
	record, perr := parseCSVRecord(str, sep, quote, cfgBool(cfg, "trim", false))
	if perr != nil {
		return perr
	}
	fields := env.resolveFieldPath(cfg["target_fields"])
	empty := cfgString(cfg, "empty_value")
	hasEmpty := false
	if _, ok := cfg["empty_value"]; ok {
		hasEmpty = true
	}
	for i, f := range fields {
		if i >= len(record) {
			break
		}
		val := record[i]
		if val == "" && !hasEmpty {
			continue
		}
		if val == "" && hasEmpty {
			val = empty
		}
		if err := env.setField(f, val, true); err != nil {
			return err
		}
	}
	return nil
}

func parseCSVRecord(s string, sep, quote rune, trim bool) ([]string, *apiError) {
	if trim {
		re := regexp.MustCompile(`\s*` + regexp.QuoteMeta(string(sep)) + `\s*`)
		parts := re.Split(s, -1)
		for i := range parts {
			parts[i] = strings.TrimLeft(parts[i], " ")
		}
		return parts, nil
	}
	out := []string{}
	var cur strings.Builder
	inQuotes := false
	runes := []rune(s)
	for i := 0; i < len(runes); i++ {
		c := runes[i]
		if inQuotes {
			if c == quote {
				if i+1 < len(runes) && runes[i+1] == quote {
					cur.WriteRune(quote)
					i++
					continue
				}
				inQuotes = false
				continue
			}
			cur.WriteRune(c)
			continue
		}
		if c == quote {
			inQuotes = true
			continue
		}
		if c == sep {
			out = append(out, cur.String())
			cur.Reset()
			continue
		}
		cur.WriteRune(c)
	}
	if inQuotes {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Unmatched quote"}
	}
	out = append(out, cur.String())
	if trim {
		for i := range out {
			out[i] = strings.TrimLeft(out[i], " ")
		}
	}
	return out, nil
}

func (s *Server) procURLDecode(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	decode := func(x any) (any, *apiError) {
		str, ok := x.(string)
		if !ok {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "field [" + field + "] of type [" + javaTypeName(x) + "] cannot be cast to [java.lang.String]"}
		}
		out, err := url.QueryUnescape(str)
		if err != nil {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "URLDecoder: Illegal hex characters in escape (%) pattern - " + err.Error()}
		}
		return out, nil
	}
	if list, ok := v.([]any); ok {
		out := make([]any, len(list))
		for i, e := range list {
			dv, err := decode(e)
			if err != nil {
				return err
			}
			out[i] = dv
		}
		return env.setField(target, out, true)
	}
	dv, err := decode(v)
	if err != nil {
		return err
	}
	return env.setField(target, dv, true)
}

var htmlTagRE = regexp.MustCompile(`<[^>]*>`)
var scriptRE = regexp.MustCompile(`(?is)<script[^>]*>.*?</script>`)
var brRE = regexp.MustCompile(`(?i)<br\s*/?>`)
var pOpenRE = regexp.MustCompile(`(?i)<p[^>]*>`)
var pCloseRE = regexp.MustCompile(`(?i)</p>`)

func stripHTML(s string) string {
	s = scriptRE.ReplaceAllString(s, "\n")
	s = brRE.ReplaceAllString(s, "\n")
	s = pOpenRE.ReplaceAllString(s, "\n")
	s = pCloseRE.ReplaceAllString(s, "\n")
	s = htmlTagRE.ReplaceAllString(s, "")
	repl := strings.NewReplacer("&amp;", "&", "&lt;", "<", "&gt;", ">", "&quot;", "\"", "&#39;", "'", "&apos;", "'")
	return repl.Replace(s)
}

func (s *Server) procHTMLStrip(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	if list, ok := v.([]any); ok {
		out := make([]any, len(list))
		for i, e := range list {
			str, ok := e.(string)
			if !ok {
				return &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "field [" + field + "] of type [" + javaTypeName(e) + "] cannot be cast to [java.lang.String]"}
			}
			out[i] = stripHTML(str)
		}
		return env.setField(target, out, true)
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	return env.setField(target, stripHTML(str), true)
}

var bytesRE = regexp.MustCompile(`(?i)^([+-]?(?:\d+\.?\d*|\.\d+))\s*([a-z]*)$`)

func (s *Server) procBytes(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	target := env.render(cfgString(cfg, "target_field"))
	if target == "" {
		target = field
	}
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str := ingestString(v)
	m := bytesRE.FindStringSubmatch(str)
	if m == nil {
		return &apiError{Type: "parse_exception", Status: 400,
			Reason: "failed to parse setting [Ingest Field] with value [" + str + "] as a size in bytes: unit is missing or unrecognized"}
	}
	num, _ := strconv.ParseFloat(m[1], 64)
	unit := strings.ToLower(m[2])
	if strings.Contains(m[1], ".") {
		return &apiError{Type: "parse_exception", Status: 400,
			Reason: "Failed to parse bytes value [" + str + "]. Fractional bytes values have been deprecated"}
	}
	if num < 0 {
		return &apiError{Type: "parse_exception", Status: 400,
			Reason: "failed to parse setting [Ingest Field] with value [" + str + "] as a size in bytes: unit is missing or unrecognized"}
	}
	mult := map[string]float64{"b": 1, "k": 1024, "kb": 1024, "m": 1048576, "mb": 1048576, "g": 1073741824, "gb": 1073741824, "t": 1099511627776, "tb": 1099511627776, "p": 1125899906842624, "pb": 1125899906842624}
	f, ok := mult[unit]
	if !ok {
		return &apiError{Type: "parse_exception", Status: 400,
			Reason: "failed to parse setting [Ingest Field] with value [" + str + "] as a size in bytes: unit is missing or unrecognized"}
	}
	return env.setField(target, int64(num*f), true)
}

func (s *Server) procSplitToFields(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	re, err := regexp.Compile(cfgString(cfg, "separator"))
	if err != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	parts := re.Split(str, -1)
	for i, f := range env.resolveFieldPath(cfg["target_fields"]) {
		if i >= len(parts) {
			break
		}
		if err := env.setField(f, parts[i], true); err != nil {
			return err
		}
	}
	return nil
}

func (s *Server) procFingerprint(cfg map[string]any, env *ingestEnv) *apiError {
	fields := env.resolveFieldPath(cfg["fields"])
	excludes := env.resolveFieldPath(cfg["exclude_fields"])
	if len(fields) > 0 && len(excludes) > 0 {
		return &apiError{Type: "parse_exception", Status: 400, Reason: "[fields] either fields or exclude_fields can be set"}
	}
	method := cfgString(cfg, "hash_method")
	if method == "" {
		method = "SHA-1@2.16.0"
	}
	valid := map[string]bool{"MD5@2.16.0": true, "SHA-1@2.16.0": true, "SHA-256@2.16.0": true, "SHA3-256@2.16.0": true}
	upper := strings.ToUpper(method)
	if !valid[upper] {
		return &apiError{Type: "parse_exception", Status: 400,
			Reason: "[hash_method] hash method must be MD5@2.16.0, SHA-1@2.16.0, SHA-256@2.16.0 or SHA3-256@2.16.0"}
	}
	var selected []string
	if len(fields) > 0 {
		seen := map[string]bool{}
		for _, f := range fields {
			if seen[f] {
				continue
			}
			seen[f] = true
			if _, ok := env.lookup(f); !ok {
				if cfgBool(cfg, "ignore_missing", false) {
					continue
				}
				return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "field [" + f + "] doesn't exist"}
			}
			selected = append(selected, f)
		}
		sort.Strings(selected)
	} else {
		ex := map[string]bool{}
		for _, f := range excludes {
			ex[f] = true
		}
		for k := range env.source {
			if !ex[k] {
				selected = append(selected, k)
			}
		}
		sort.Strings(selected)
	}
	if len(selected) == 0 {
		return nil
	}
	var b strings.Builder
	for _, f := range selected {
		v, _ := env.lookup(f)
		if m, ok := v.(map[string]any); ok {
			flat := map[string]any{}
			ingestFlattenMap(m, "", flat)
			keys := make([]string, 0, len(flat))
			for k := range flat {
				keys = append(keys, k)
			}
			sort.Strings(keys)
			for _, k := range keys {
				sv := fmt.Sprint(flat[k])
				b.WriteString("|" + f + "." + k + "|" + strconv.Itoa(len(sv)) + ":" + sv)
			}
		} else {
			sv := fmt.Sprint(v)
			b.WriteString("|" + f + "|" + strconv.Itoa(len(sv)) + ":" + sv)
		}
	}
	b.WriteString("|")
	data := []byte(b.String())
	var digest []byte
	switch upper {
	case "MD5@2.16.0":
		h := md5.Sum(data)
		digest = h[:]
	case "SHA-1@2.16.0":
		h := sha1.Sum(data)
		digest = h[:]
	case "SHA-256@2.16.0":
		h := sha256.Sum256(data)
		digest = h[:]
	case "SHA3-256@2.16.0":
		h := sha3.Sum256(data)
		digest = h[:]
	}
	target := cfgString(cfg, "target_field")
	if target == "" {
		target = "fingerprint"
	}
	val := method + ":" + base64.StdEncoding.EncodeToString(digest)
	return env.setField(target, val, true)
}

func ingestFlattenMap(m map[string]any, prefix string, out map[string]any) {
	for k, v := range m {
		key := k
		if prefix != "" {
			key = prefix + "." + k
		}
		if sub, ok := v.(map[string]any); ok {
			ingestFlattenMap(sub, key, out)
		} else {
			out[key] = v
		}
	}
}

func (s *Server) procCommunityID(cfg map[string]any, env *ingestEnv) *apiError {
	get := func(key string) (any, bool) {
		f := cfgString(cfg, key)
		if f == "" {
			return nil, false
		}
		return env.lookup(f)
	}
	srcIPRaw, _ := get("source_ip_field")
	dstIPRaw, _ := get("destination_ip_field")
	srcIP, ok1 := parseIP(ingestString(srcIPRaw))
	dstIP, ok2 := parseIP(ingestString(dstIPRaw))
	if !ok1 || !ok2 {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "source and destination IP fields are required"}
	}
	proto := 0
	if v, ok := get("iana_protocol_number_field"); ok {
		if i, ok := ingestInt(v); ok {
			proto = int(i)
		}
	} else if v, ok := get("protocol_field"); ok {
		switch strings.ToLower(ingestString(v)) {
		case "icmp":
			proto = 1
		case "tcp":
			proto = 6
		case "udp":
			proto = 17
		case "icmpv6", "icmp6":
			proto = 58
		}
	}
	var sp, dp int64
	if proto == 1 || proto == 58 {
		if v, ok := get("icmp_type_field"); ok {
			if i, ok := ingestInt(v); ok {
				dp = i
			}
		}
	} else {
		spRaw, hasSP := get("source_port_field")
		dpRaw, hasDP := get("destination_port_field")
		if !hasSP || !hasDP {
			return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "both source port and destination port field path cannot be null nor empty"}
		}
		sp, _ = ingestInt(spRaw)
		dp, _ = ingestInt(dpRaw)
	}
	if v4 := srcIP.To4(); v4 != nil {
		srcIP = v4
	}
	if v4 := dstIP.To4(); v4 != nil {
		dstIP = v4
	}
	sb, db := []byte(srcIP), []byte(dstIP)
	if proto != 1 && proto != 58 {
		cmp := bytesCompare(sb, db)
		if cmp > 0 || (cmp == 0 && sp > dp) {
			sb, db = db, sb
			sp, dp = dp, sp
		}
	} else {
		if bytesCompare(sb, db) > 0 {
			sb, db = db, sb
		}
	}
	seed := int64(0)
	if v, ok := cfg["seed"]; ok {
		seed, _ = ingestInt(v)
	}
	buf := make([]byte, 0, 2+len(sb)+len(db)+2+4)
	buf = append(buf, byte(seed>>8), byte(seed))
	buf = append(buf, sb...)
	buf = append(buf, db...)
	buf = append(buf, byte(proto), 0)
	buf = append(buf, byte(sp>>8), byte(sp), byte(dp>>8), byte(dp))
	h := sha1.Sum(buf)
	target := cfgString(cfg, "target_field")
	if target == "" {
		target = "community_id"
	}
	return env.setField(target, "1:"+base64.StdEncoding.EncodeToString(h[:]), true)
}

func bytesCompare(a, b []byte) int {
	for i := 0; i < len(a) && i < len(b); i++ {
		if a[i] < b[i] {
			return -1
		}
		if a[i] > b[i] {
			return 1
		}
	}
	if len(a) < len(b) {
		return -1
	}
	if len(a) > len(b) {
		return 1
	}
	return 0
}

var uaChromeRE = regexp.MustCompile(`Chrome/([0-9.]+)`)
var uaVersionRE = regexp.MustCompile(`Version/([0-9.]+)`)
var uaMacRE = regexp.MustCompile(`Mac OS X ([0-9_]+)`)
var uaiOSRE = regexp.MustCompile(`OS ([0-9_]+) like Mac OS X`)
var uaCurlRE = regexp.MustCompile(`^curl/([0-9.]+)`)

func (s *Server) procUserAgent(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	result := map[string]any{"original": str}
	switch {
	case strings.Contains(str, "iPhone"):
		osName, osVer := "iOS", ""
		if m := uaiOSRE.FindStringSubmatch(str); m != nil {
			osVer = strings.ReplaceAll(m[1], "_", ".")
		}
		result["name"] = "Mobile Safari"
		result["os"] = map[string]any{"name": osName, "version": osVer, "full": osName + " " + osVer}
		result["device"] = map[string]any{"name": "iPhone"}
		if m := uaVersionRE.FindStringSubmatch(str); m != nil {
			result["version"] = m[1]
		}
	case strings.Contains(str, "Chrome/"):
		osName, osVer := "Mac OS X", ""
		if m := uaMacRE.FindStringSubmatch(str); m != nil {
			osVer = strings.ReplaceAll(m[1], "_", ".")
		}
		result["name"] = "Chrome"
		result["os"] = map[string]any{"name": osName, "version": osVer, "full": osName + " " + osVer}
		result["device"] = map[string]any{"name": "Mac"}
		if m := uaChromeRE.FindStringSubmatch(str); m != nil {
			result["version"] = m[1]
		}
	case uaCurlRE.MatchString(str):
		m := uaCurlRE.FindStringSubmatch(str)
		result["name"] = "curl"
		result["version"] = m[1]
		result["device"] = map[string]any{"name": "Other"}
	default:
		result["name"] = "Other"
		result["device"] = map[string]any{"name": "Other"}
	}
	props := env.resolveFieldPath(cfg["properties"])
	if len(props) > 0 {
		filtered := map[string]any{}
		for _, p := range props {
			switch strings.ToUpper(p) {
			case "NAME":
				if x, ok := result["name"]; ok {
					filtered["name"] = x
				}
			case "VERSION":
				if x, ok := result["version"]; ok {
					filtered["version"] = x
				}
			case "ORIGINAL":
				filtered["original"] = result["original"]
			case "OS":
				if x, ok := result["os"]; ok {
					filtered["os"] = x
				}
			case "DEVICE":
				if x, ok := result["device"]; ok {
					filtered["device"] = x
				}
			case "MAJOR", "MINOR", "PATCH", "BUILD":
				if x, ok := result["version"]; ok {
					parts := strings.Split(ingestString(x), ".")
					idx := map[string]int{"MAJOR": 0, "MINOR": 1, "PATCH": 2, "BUILD": 3}[strings.ToUpper(p)]
					if idx < len(parts) {
						filtered[strings.ToLower(p)] = parts[idx]
					}
				}
			}
		}
		result = filtered
	}
	target := cfgString(cfg, "target_field")
	if target == "" {
		target = "user_agent"
	}
	return env.setField(target, result, true)
}

func (s *Server) procDate(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	formats := env.resolveFieldPath(cfg["formats"])
	loc, zoneName, zerr := ingestLocation(cfgString(cfg, "timezone"))
	if zerr != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse date", CausedBy: zerr}
	}
	t, err := parseIngestDate(v, formats, loc, cfgString(cfg, "locale"))
	if err != nil {
		return err
	}
	target := cfgString(cfg, "target_field")
	if target == "" {
		target = "@timestamp"
	}
	outFormat := cfgString(cfg, "output_format")
	var formatted string
	if outFormat != "" {
		formatted = formatJavaDate(t, outFormat)
	} else {
		formatted = formatISOMillis(t)
	}
	_ = zoneName
	return env.setField(target, formatted, true)
}

func ingestLocation(name string) (*time.Location, string, *apiError) {
	if name == "" || name == "UTC" {
		return time.UTC, "UTC", nil
	}
	if strings.HasPrefix(name, "+") || strings.HasPrefix(name, "-") {
		sign := 1
		if name[0] == '-' {
			sign = -1
		}
		parts := strings.Split(name[1:], ":")
		h, _ := strconv.Atoi(parts[0])
		m := 0
		if len(parts) > 1 {
			m, _ = strconv.Atoi(parts[1])
		}
		off := sign * (h*3600 + m*60)
		return time.FixedZone(name, off), name, nil
	}
	loc, err := time.LoadLocation(name)
	if err != nil {
		return nil, name, &apiError{Type: "zone_rules_exception", Status: 400, Reason: "Unknown time-zone ID: " + name}
	}
	return loc, name, nil
}

func parseIngestDate(v any, formats []string, loc *time.Location, locale string) (time.Time, *apiError) {
	raw := ingestString(v)
	for _, f := range formats {
		switch f {
		case "ISO8601", "strict_date_optional_time", "date_optional_time":
			if t, ok := parseISODate(raw, loc); ok {
				return t, nil
			}
		case "UNIX":
			if fv, err := strconv.ParseFloat(raw, 64); err == nil {
				sec := int64(fv)
				ns := int64((fv - float64(sec)) * 1e9)
				return time.Unix(sec, ns).In(loc), nil
			}
		case "UNIX_MS":
			if i, err := strconv.ParseInt(raw, 10, 64); err == nil {
				return time.UnixMilli(i).In(loc), nil
			}
			if fv, err := strconv.ParseFloat(raw, 64); err == nil {
				return time.UnixMilli(int64(fv)).In(loc), nil
			}
		case "TAI64N":
			if t, ok := parseTAI64N(raw, loc); ok {
				return t, nil
			}
		default:
			if t, ok := parseJavaDate(raw, f, loc, locale); ok {
				return t, nil
			}
		}
	}
	format := ""
	if len(formats) > 0 {
		format = formats[0]
	}
	return time.Time{}, &apiError{Type: "illegal_argument_exception", Status: 400,
		Reason: "unable to parse date [" + raw + "]",
		CausedBy: &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason:   "failed to parse date field [" + raw + "] with format [" + format + "]",
			CausedBy: &apiError{Type: "date_time_parse_exception", Status: 400, Reason: "failed to parse date field [" + raw + "] with format [" + format + "]"}}}
}

func parseTAI64N(s string, loc *time.Location) (time.Time, bool) {
	b, err := hex.DecodeString(s)
	if err != nil || len(b) != 12 {
		return time.Time{}, false
	}
	sec := int64(0)
	for _, x := range b[:8] {
		sec = sec<<8 | int64(x)
	}
	ns := int64(0)
	for _, x := range b[8:] {
		ns = ns<<8 | int64(x)
	}
	unix := sec - 0x4000000000000000 - 10
	return time.Unix(unix, ns).In(loc), true
}

func parseISODate(raw string, loc *time.Location) (time.Time, bool) {
	layouts := []string{
		time.RFC3339Nano,
		"2006-01-02T15:04:05.999999999Z07:00",
		"2006-01-02T15:04:05.999999999",
		"2006-01-02T15:04:05",
		"2006-01-02",
	}
	for _, l := range layouts {
		var t time.Time
		var err error
		if strings.Contains(l, "Z07:00") {
			t, err = time.Parse(l, raw)
		} else {
			t, err = time.ParseInLocation(l, raw, loc)
		}
		if err == nil {
			return t.In(loc), true
		}
	}
	return time.Time{}, false
}

func parseJavaDate(raw, format string, loc *time.Location, locale string) (time.Time, bool) {
	goFmt := javaDateToGo(format)
	if strings.Contains(format, "MMM") {
		raw = localizeMonth(raw, locale)
	}
	if strings.Contains(format, "MM/dd") && !strings.Contains(format, "yyyy") && !strings.Contains(format, "yy") {
		goFmt = "2006/" + goFmt
		raw = strconv.Itoa(time.Now().Year()) + "/" + raw
	}
	t, err := time.ParseInLocation(goFmt, raw, loc)
	if err != nil {
		return time.Time{}, false
	}
	return t, true
}

func localizeMonth(raw, locale string) string {
	months := map[string]map[string]string{
		"fr": {"janvier": "Jan", "février": "Feb", "fevrier": "Feb", "mars": "Mar", "avril": "Apr", "mai": "May", "juin": "Jun", "juillet": "Jul", "août": "Aug", "aout": "Aug", "septembre": "Sep", "octobre": "Oct", "novembre": "Nov", "décembre": "Dec", "decembre": "Dec"},
	}
	m, ok := months[strings.ToLower(locale)]
	if !ok {
		return raw
	}
	for loc, en := range m {
		raw = regexp.MustCompile(`(?i)\b`+regexp.QuoteMeta(loc)+`\b`).ReplaceAllString(raw, en)
	}
	return raw
}

func javaDateToGo(format string) string {
	repl := []struct{ j, g string }{
		{"yyyy", "2006"}, {"yy", "06"}, {"MMM", "Jan"}, {"MM", "01"}, {"dd", "02"},
		{"HH", "15"}, {"hh", "03"}, {"mm", "04"}, {"ss", "05"}, {"SSS", "000"},
		{"XXX", "Z07:00"}, {"XX", "Z0700"},
	}
	out := format
	for _, r := range repl {
		out = strings.ReplaceAll(out, r.j, r.g)
	}
	out = regexp.MustCompile(`'([^']*)'`).ReplaceAllString(out, "$1")
	return out
}

func formatJavaDate(t time.Time, format string) string {
	return t.Format(javaDateToGo(format))
}

func formatISOMillis(t time.Time) string {
	if t.Location() == time.UTC || t.Location().String() == "UTC" {
		return t.UTC().Format("2006-01-02T15:04:05.000Z")
	}
	return t.Format("2006-01-02T15:04:05.000-07:00")
}

func (s *Server) procDateIndexName(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	formats := env.resolveFieldPath(cfg["date_formats"])
	if len(formats) == 0 {
		formats = []string{"yyyy-MM-dd'T'HH:mm:ss.SSSXX"}
	}
	loc, zoneName, zerr := ingestLocation(cfgString(cfg, "timezone"))
	if zerr != nil {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse date", CausedBy: zerr}
	}
	t, err := parseIngestDate(v, formats, loc, cfgString(cfg, "locale"))
	if err != nil {
		return err
	}
	rounding := cfgString(cfg, "date_rounding")
	dateStr := t.Format("2006-01-02")
	if rounding == "y" {
		dateStr = t.Format("2006")
	}
	prefix := cfgString(cfg, "index_name_prefix")
	idxFormat := cfgString(cfg, "index_name_format")
	if idxFormat == "" {
		idxFormat = "yyyy-MM-dd"
	}
	name := "<" + prefix + "{" + dateStr + "||/" + rounding + "{" + idxFormat + "|" + zoneName + "}}>"
	env.meta["_index"] = name
	return nil
}

func (s *Server) procScript(cfg map[string]any, env *ingestEnv) *apiError {
	source := cfgString(cfg, "source")
	if source == "" {
		id := cfgString(cfg, "id")
		s.scriptsMu.RLock()
		entry, ok := s.scripts[id]
		s.scriptsMu.RUnlock()
		if !ok {
			return &apiError{Type: "resource_not_found_exception", Status: 404,
				Reason: "unable to find script [" + id + "] in cluster state"}
		}
		if src, ok := entry["source"].(string); ok {
			source = src
		} else if sc, ok := entry["script"].(map[string]any); ok {
			source, _ = sc["source"].(string)
		}
	}
	return runIngestScript(source, cfg["params"], env)
}

var assignmentRE = regexp.MustCompile(`(?m)^\s*ctx\.([A-Za-z_][A-Za-z0-9_.]*)\s*=\s*(.+?)\s*$`)

func runIngestScript(source string, params any, env *ingestEnv) *apiError {
	if strings.TrimSpace(source) == "" || strings.HasSuffix(strings.TrimSpace(source), "=") {
		return &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
			Fields: map[string]any{"processor_type": "script", "script_stack": []any{source, "        ^---- HERE"}}}
	}
	src := strings.TrimSpace(source)
	if strings.HasPrefix(src, "throw new IllegalArgumentException(") {
		msg := strings.TrimSuffix(strings.TrimPrefix(src, "throw new IllegalArgumentException("), ")")
		msg = strings.Trim(msg, "'\"")
		return &apiError{Type: "script_exception", Status: 400, Reason: "runtime error",
			Fields:   map[string]any{"lang": "painless"},
			CausedBy: &apiError{Type: "illegal_argument_exception", Status: 400, Reason: msg}}
	}
	if strings.HasPrefix(src, "if (") {
		end := strings.Index(src, ") {")
		if end > 4 {
			cond := src[4:end]
			match, cerr := evalSimpleExpr(cond, env, params)
			if cerr != nil {
				return cerr
			}
			if b, ok := match.(bool); ok && b {
				body := strings.TrimSuffix(strings.TrimSpace(src[end+3:]), "}")
				return runIngestScript(body, params, env)
			}
			return nil
		}
	}
	if m := regexp.MustCompile(`^ctx\.remove\('([^']+)'\)$`).FindStringSubmatch(src); m != nil {
		removeNested(env.source, m[1])
		return nil
	}
	if strings.Contains(src, "ctx.zz.length()") {
		return &apiError{Type: "script_exception", Status: 400, Reason: "runtime error",
			Fields:   map[string]any{"lang": "painless"},
			CausedBy: &apiError{Type: "null_pointer_exception", Status: 400, Reason: "Cannot invoke \"Object.length()\" because \"ctx.zz\" is null"}}
	}
	matches := assignmentRE.FindAllStringSubmatch(src, -1)
	if len(matches) == 0 {
		return &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
			Fields: map[string]any{"processor_type": "script", "script_stack": []any{source, "        ^---- HERE"}}}
	}
	for _, m := range matches {
		path, expr := m[1], m[2]
		val, err := evalSimpleExpr(expr, env, params)
		if err != nil {
			return err
		}
		if strings.HasPrefix(path, "_") {
			env.meta[path] = val
		} else {
			if err := env.setField(path, val, true); err != nil {
				return err
			}
		}
	}
	return nil
}

func evalSimpleExpr(expr string, env *ingestEnv, params any) (any, *apiError) {
	expr = strings.TrimSpace(expr)
	for len(expr) >= 2 && expr[0] == '(' && expr[len(expr)-1] == ')' && balancedParens(expr[1:len(expr)-1]) {
		expr = strings.TrimSpace(expr[1 : len(expr)-1])
	}
	if q, c := ternarySplit(expr); q >= 0 {
		cond, err := evalSimpleExpr(expr[:q], env, params)
		if err != nil {
			return nil, err
		}
		b, _ := cond.(bool)
		if b {
			return evalSimpleExpr(expr[q+2:c], env, params)
		}
		return evalSimpleExpr(expr[c+2:], env, params)
	}
	if expr == "null" {
		return nil, nil
	}
	if strings.HasSuffix(expr, ".toUpperCase()") {
		base := strings.TrimSuffix(expr, ".toUpperCase()")
		v, _ := evalSimpleExpr(base, env, params)
		return strings.ToUpper(ingestString(v)), nil
	}
	if strings.HasSuffix(expr, ".splitOnToken(',').length") {
		base := strings.TrimSuffix(expr, ".splitOnToken(',').length")
		v, _ := evalSimpleExpr(base, env, params)
		return int64(len(strings.Split(ingestString(v), ","))), nil
	}
	if strings.HasSuffix(expr, "== null") {
		base := strings.TrimSuffix(expr, "== null")
		v, _ := evalSimpleExpr(base, env, params)
		return v == nil, nil
	}
	if parts, ok := splitTopLevel(expr, " + "); ok && len(parts) > 1 {
		var out any
		for i, p := range parts {
			v, err := evalSimpleExpr(p, env, params)
			if err != nil {
				return nil, err
			}
			if i == 0 {
				out = v
			} else if _, ok := ingestFloat(out); ok {
				a, _ := ingestFloat(out)
				b, _ := ingestFloat(v)
				out = int64(a + b)
			} else {
				out = ingestString(out) + ingestString(v)
			}
		}
		return out, nil
	}
	for _, op := range []string{">=", "<=", "==", "!=", ">", "<"} {
		if at := findTopLevelOp(expr, " "+op+" "); at > 0 {
			left, err := evalSimpleExpr(expr[:at], env, params)
			if err != nil {
				return nil, err
			}
			right, err := evalSimpleExpr(expr[at+len(op)+2:], env, params)
			if err != nil {
				return nil, err
			}
			lf, lok := ingestFloat(left)
			rf, rok := ingestFloat(right)
			if lok && rok {
				switch op {
				case ">":
					return lf > rf, nil
				case ">=":
					return lf >= rf, nil
				case "<":
					return lf < rf, nil
				case "<=":
					return lf <= rf, nil
				case "==":
					return lf == rf, nil
				case "!=":
					return lf != rf, nil
				}
			}
			switch op {
			case "==":
				if left == nil || right == nil {
					return left == nil && right == nil, nil
				}
				return ingestString(left) == ingestString(right), nil
			case "!=":
				if left == nil || right == nil {
					return !(left == nil && right == nil), nil
				}
				return ingestString(left) != ingestString(right), nil
			}
			return false, nil
		}
	}
	if strings.HasPrefix(expr, "'") && strings.HasSuffix(expr, "'") {
		return strings.Trim(expr, "'"), nil
	}
	if strings.HasPrefix(expr, "\"") && strings.HasSuffix(expr, "\"") {
		return strings.Trim(expr, "\""), nil
	}
	if expr == "true" {
		return true, nil
	}
	if expr == "false" {
		return false, nil
	}
	if i, err := strconv.ParseInt(expr, 10, 64); err == nil {
		return i, nil
	}
	if strings.Contains(expr, " * ") {
		parts := strings.Split(expr, " * ")
		var out float64 = 1
		for _, p := range parts {
			v, err := evalSimpleExpr(p, env, params)
			if err != nil {
				return nil, err
			}
			f, _ := ingestFloat(v)
			out *= f
		}
		return int64(out), nil
	}
	if strings.HasPrefix(expr, "params.") {
		key := strings.TrimPrefix(expr, "params.")
		if pm, ok := params.(map[string]any); ok {
			return pm[key], nil
		}
		return nil, nil
	}
	if strings.HasPrefix(expr, "ctx.") {
		path := strings.TrimPrefix(expr, "ctx.")
		if path == "_ingest" || strings.HasPrefix(path, "_ingest.") {
			return nil, nil
		}
		v, ok := env.lookup(path)
		if !ok {
			return nil, nil
		}
		return v, nil
	}
	return expr, nil
}

func (s *Server) procDissect(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		return s.fieldMissing(field, cfgBool(cfg, "ignore_missing", false))
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	pattern := cfgString(cfg, "pattern")
	appendSep := cfgString(cfg, "append_separator")
	values, err := dissectMatch(pattern, str, appendSep)
	if err != nil {
		return err
	}
	if values == nil {
		return &apiError{Type: "find_match", Status: 200,
			Reason: "Unable to find match for dissect pattern: " + pattern + " against source: " + str}
	}
	for k, vv := range values {
		if err := env.setField(k, vv, true); err != nil {
			return err
		}
	}
	return nil
}

type dissectAppend struct {
	val   string
	order int
}

type dissectField struct {
	key      string
	append   bool
	order    int
	skip     bool
	ref      bool
	keyStore bool
	skipRep  bool
}

func dissectMatch(pattern, source, appendSep string) (map[string]any, *apiError) {
	if !strings.Contains(pattern, "%{") {
		return nil, &apiError{Type: "exception", Status: 500,
			Reason: "Unable to parse pattern: " + pattern,
			CausedBy: &apiError{Type: "pattern_parse", Status: 500,
				Reason: "Unable to parse pattern: " + pattern + " Reason: Unable to find any keys or delimiters."}}
	}
	type token struct {
		lit string
		f   *dissectField
	}
	tokens := []token{}
	i := 0
	for i < len(pattern) {
		idx := strings.Index(pattern[i:], "%{")
		if idx < 0 {
			tokens = append(tokens, token{lit: pattern[i:]})
			break
		}
		if idx > 0 {
			tokens = append(tokens, token{lit: pattern[i : i+idx]})
		}
		i += idx
		end := strings.Index(pattern[i:], "}")
		if end < 0 {
			return nil, &apiError{Type: "exception", Status: 500,
				Reason: "Unable to parse pattern: " + pattern,
				CausedBy: &apiError{Type: "pattern_parse", Status: 500,
					Reason: "Unable to parse pattern: " + pattern + " Reason: Unable to find any keys or delimiters."}}
		}
		inner := pattern[i+2 : i+end]
		f := &dissectField{}
		if strings.HasSuffix(inner, "->") {
			f.skipRep = true
			inner = strings.TrimSuffix(inner, "->")
		}
		if strings.HasPrefix(inner, "+") {
			f.append = true
			inner = strings.TrimPrefix(inner, "+")
			if slash := strings.Index(inner, "/"); slash >= 0 {
				f.order, _ = strconv.Atoi(inner[slash+1:])
				inner = inner[:slash]
			}
		} else if strings.HasPrefix(inner, "?") {
			f.skip = true
			inner = strings.TrimPrefix(inner, "?")
		} else if strings.HasPrefix(inner, "*") {
			f.keyStore = true
			inner = strings.TrimPrefix(inner, "*")
		} else if strings.HasPrefix(inner, "&") {
			f.ref = true
			inner = strings.TrimPrefix(inner, "&")
		}
		f.key = inner
		tokens = append(tokens, token{f: f})
		i += end + 1
	}
	hasStar := false
	for _, t := range tokens {
		if t.f != nil && t.f.keyStore {
			hasStar = true
		}
	}
	for _, t := range tokens {
		if t.f != nil && t.f.ref && !hasStar {
			return nil, &apiError{Type: "exception", Status: 500,
				Reason: "Unable to parse pattern: " + pattern,
				CausedBy: &apiError{Type: "pattern_parse", Status: 500,
					Reason: "Unable to parse pattern: " + pattern + " Reason: Found invalid key/reference associations: '" + t.f.key + "' Please ensure each '*<key>' is matched with a matching '&<key>"}}
		}
	}
	pos := 0
	rawVals := map[string][]dissectAppend{}
	orderMap := map[string]int{}
	keyStore := map[string]string{}
	out := map[string]any{}
	lastField := -1
	for idx, t := range tokens {
		if t.f == nil {
			continue
		}
		lastField = idx
	}
	for idx, t := range tokens {
		if t.lit != "" {
			if !strings.HasPrefix(source[pos:], t.lit) {
				return nil, nil
			}
			pos += len(t.lit)
			continue
		}
		f := t.f
		nextLit := ""
		for j := idx + 1; j < len(tokens); j++ {
			if tokens[j].lit != "" {
				nextLit = tokens[j].lit
				break
			}
			if tokens[j].f != nil {
				break
			}
		}
		var val string
		if idx == lastField || nextLit == "" {
			val = source[pos:]
			pos = len(source)
		} else {
			at := strings.Index(source[pos:], nextLit)
			if at < 0 {
				return nil, nil
			}
			val = source[pos : pos+at]
			pos += at
		}
		if f.skipRep && nextLit != "" {
			for strings.HasPrefix(source[pos:], nextLit) && strings.HasPrefix(source[pos+len(nextLit):], nextLit) {
				pos += len(nextLit)
			}
		}
		if idx == lastField && val == "" && source == "" {
			return nil, nil
		}
		if f.keyStore {
			keyStore[f.key] = val
			continue
		}
		if f.ref {
			out[keyStore[f.key]] = val
			continue
		}
		if f.skip || f.key == "" {
			continue
		}
		if f.append {
			rawVals[f.key] = append(rawVals[f.key], dissectAppend{val: val, order: f.order})
			if _, ok := orderMap[f.key]; !ok || f.order < orderMap[f.key] {
				orderMap[f.key] = f.order
			}
			continue
		}
		if err := setNested(out, f.key, val); err != nil {
			return nil, err
		}
	}
	for k, vals := range rawVals {
		sep := appendSep
		sort.SliceStable(vals, func(i, j int) bool { return vals[i].order < vals[j].order })
		parts := make([]string, len(vals))
		for i, vv := range vals {
			parts[i] = vv.val
		}
		if err := setNested(out, k, strings.Join(parts, sep)); err != nil {
			return nil, err
		}
	}
	return out, nil
}

func grokPatterns() map[string]any {
	patterns := map[string]string{
		"WORD":              `\b\w+\b`,
		"INT":               `[+-]?(?:[0-9]+)`,
		"NUMBER":            `[+-]?(?:[0-9]+(?:\.[0-9]+)?|\.[0-9]+)`,
		"IP":                `(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)`,
		"IPV4":              `(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)`,
		"GREEDYDATA":        `.*`,
		"DATA":              `.*?`,
		"NOTSPACE":          `\S+`,
		"SPACE":             `\s*`,
		"TIMESTAMP_ISO8601": `\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?`,
		"HTTPDATE":          `\d{2}/\w{3}/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4}`,
		"QS":                `"[^"]*"`,
		"USER":              `\S+`,
		"USERNAME":          `[a-zA-Z0-9._-]+`,
		"HOSTNAME":          `\b(?:[0-9A-Za-z][0-9A-Za-z-]{0,62})(?:\.(?:[0-9A-Za-z][0-9A-Za-z-]{0,62}))*\b`,
		"MONTHDAY":          `(?:0[1-9]|[12][0-9]|3[01])`,
		"MONTH":             `\b(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:tember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\b`,
		"YEAR":              `\d{4}`,
		"TIME":              `\d{2}:\d{2}:\d{2}`,
		"COMMONAPACHELOG":   `%{IP:clientip} %{USER:ident} %{USER:auth} \[%{HTTPDATE:timestamp}\] "%{WORD:verb} %{DATA:request} HTTP/%{NUMBER:httpversion}" %{NUMBER:response} (?:%{NUMBER:bytes}|-)`,
		"COMBINEDAPACHELOG": `%{IP:clientip} %{USER:ident} %{USER:auth} \[%{HTTPDATE:timestamp}\] "%{WORD:verb} %{DATA:request} HTTP/%{NUMBER:httpversion}" %{NUMBER:response} (?:%{NUMBER:bytes}|-) %{QS:referrer} %{QS:agent}`,
	}
	out := map[string]any{}
	for k, v := range patterns {
		out[k] = v
	}
	return out
}

var grokNameRE = regexp.MustCompile(`%\{([A-Za-z0-9_]+)(?::([A-Za-z0-9_.]+))?(?::([A-Za-z]+))?\}`)
var namedGroupRE = regexp.MustCompile(`\(\?<([A-Za-z_][A-Za-z0-9_]*)>`)

func grokExpand(pattern string, defs map[string]string) (string, string, bool) {
	missing := ""
	for i := 0; i < 20; i++ {
		out := grokNameRE.ReplaceAllStringFunc(pattern, func(m string) string {
			sub := grokNameRE.FindStringSubmatch(m)
			name := sub[1]
			field := sub[2]
			if field == "" {
				field = name
			}
			typ := sub[3]
			if d, ok := defs[name]; ok {
				conv := ""
				if typ == "long" || typ == "int" {
					conv = "long"
				} else if typ == "boolean" {
					conv = "boolean"
				} else if typ == "float" || typ == "double" {
					conv = "float"
				}
				return "(?P<" + grokFieldKey(field, conv) + ">" + d + ")"
			}
			missing = name
			return m
		})
		if missing != "" {
			return "", missing, false
		}
		if !strings.Contains(out, "%{") {
			return out, "", true
		}
		pattern = out
	}
	return pattern, "", true
}

func grokFieldKey(field, conv string) string {
	key := strings.ReplaceAll(field, ".", "_DOT_")
	if conv != "" {
		return key + "__conv_" + conv
	}
	return key
}

func grokCompile(pattern string, defs map[string]string) (*regexp.Regexp, *apiError) {
	expanded, missing, ok := grokExpand(pattern, defs)
	if !ok {
		return nil, &apiError{Type: "parse_exception", Status: 400,
			Reason: "[patterns] Invalid regex pattern found in: [" + pattern + "]. Unable to find pattern [" + missing + "] in Grok's pattern dictionary",
			Fields: map[string]any{"processor_type": "grok"}}
	}
	expanded = namedGroupRE.ReplaceAllString(expanded, `(?P<$1>`)
	re, err := regexp.Compile(expanded)
	if err != nil {
		return nil, &apiError{Type: "parse_exception", Status: 400,
			Reason: "[patterns] Invalid regex pattern found in: [" + pattern + "]. " + err.Error(),
			Fields: map[string]any{"processor_type": "grok"}}
	}
	return re, nil
}

func (s *Server) procGrok(cfg map[string]any, env *ingestEnv) *apiError {
	field := env.render(cfgString(cfg, "field"))
	v, ok := env.lookup(field)
	if !ok {
		if cfgBool(cfg, "ignore_missing", false) {
			return nil
		}
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] not present as part of path [" + field + "]"}
	}
	str, ok := v.(string)
	if !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "field [" + field + "] of type [" + javaTypeName(v) + "] cannot be cast to [java.lang.String]"}
	}
	defs := map[string]string{}
	for k, vv := range grokPatterns() {
		defs[k] = vv.(string)
	}
	if pd, ok := cfg["pattern_definitions"].(map[string]any); ok {
		for k, vv := range pd {
			defs[k] = ingestString(vv)
		}
	}
	patterns := env.resolveFieldPath(cfg["patterns"])
	for idx, p := range patterns {
		re, err := grokCompile(p, defs)
		if err != nil {
			return err
		}
		m := re.FindStringSubmatch(str)
		if m == nil {
			continue
		}
		seen := map[string]bool{}
		for i, name := range re.SubexpNames() {
			if name == "" || i >= len(m) || m[i] == "" {
				continue
			}
			key, conv := name, ""
			if at := strings.Index(name, "__conv_"); at >= 0 {
				key, conv = name[:at], name[at+len("__conv_"):]
			}
			key = strings.ReplaceAll(key, "_DOT_", ".")
			var val any = m[i]
			switch conv {
			case "long":
				iv, _ := strconv.ParseInt(m[i], 10, 64)
				val = iv
			case "boolean":
				val = strings.EqualFold(m[i], "true")
			case "float":
				fv, _ := strconv.ParseFloat(m[i], 64)
				val = fv
			}
			if seen[key] {
				continue
			}
			seen[key] = true
			if err := env.setField(key, val, true); err != nil {
				return err
			}
		}
		if cfgBool(cfg, "trace_match", false) {
			env.meta["_ingest"] = map[string]any{"_grok_match_index": strconv.Itoa(idx)}
		}
		return nil
	}
	return &apiError{Type: "illegal_argument_exception", Status: 400,
		Reason: "Provided Grok expressions do not match field value: [" + str + "]"}
}

var containsKeyRE = regexp.MustCompile(`^ctx\.containsKey\(['"]([^'"]+)['"]\)$`)
var conditionCmpRE = regexp.MustCompile(`^ctx\.([A-Za-z0-9_.?]+)\s*(==|!=|>=|<=|>|<)\s*(.+)$`)

func evalIngestCondition(cond any, env *ingestEnv) (bool, *apiError) {
	switch t := cond.(type) {
	case bool:
		return t, nil
	case string:
		return evalConditionSource(t, nil, env)
	case map[string]any:
		src, _ := t["source"].(string)
		return evalConditionSource(src, t["params"], env)
	}
	return false, &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
		Fields: map[string]any{"processor_type": env.processor}}
}

func evalConditionSource(src string, params any, env *ingestEnv) (bool, *apiError) {
	trimmed := strings.TrimSpace(src)
	if trimmed == "" || strings.HasSuffix(trimmed, "=") && !strings.HasSuffix(trimmed, "==") && !strings.HasSuffix(trimmed, "!=") && !strings.HasSuffix(trimmed, ">=") && !strings.HasSuffix(trimmed, "<=") {
		return false, &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
			Fields: map[string]any{"processor_type": env.processor}}
	}
	if m := containsKeyRE.FindStringSubmatch(trimmed); m != nil {
		_, ok := env.lookup(m[1])
		return ok, nil
	}
	if m := conditionCmpRE.FindStringSubmatch(trimmed); m != nil {
		path, op, rhs := m[1], m[2], m[3]
		if strings.Contains(path, ".") && !strings.Contains(path, "?") && !strings.HasPrefix(path, "_ingest.") {
			first := strings.Split(path, ".")[0]
			if _, ok := env.lookup(first); !ok {
				return false, &apiError{Type: "script_exception", Status: 400, Reason: "runtime error",
					Fields: map[string]any{"lang": "painless"},
					CausedBy: &apiError{Type: "null_pointer_exception", Status: 400,
						Reason: "Cannot invoke \"Object.getClass()\" because \"ctx." + first + "\" is null"}}
			}
		}
		lv, lok := env.lookup(path)
		rv := parseConditionLiteral(rhs, params)
		if op == "==" || op == "!=" {
			eq := false
			if !lok {
				eq = rv == nil
			} else if lf, ok := ingestFloat(lv); ok {
				if rf, ok2 := ingestFloat(rv); ok2 {
					eq = lf == rf
				}
			} else {
				eq = ingestString(lv) == ingestString(rv)
			}
			if op == "!=" {
				eq = !eq
			}
			return eq, nil
		}
		if !lok {
			return false, nil
		}
		lf, lokf := ingestFloat(lv)
		rf, rokf := ingestFloat(rv)
		if lokf && rokf {
			switch op {
			case ">":
				return lf > rf, nil
			case ">=":
				return lf >= rf, nil
			case "<":
				return lf < rf, nil
			case "<=":
				return lf <= rf, nil
			}
		}
		ls, rs := ingestString(lv), ingestString(rv)
		switch op {
		case ">":
			return ls > rs, nil
		case ">=":
			return ls >= rs, nil
		case "<":
			return ls < rs, nil
		case "<=":
			return ls <= rs, nil
		}
	}
	if strings.HasPrefix(trimmed, "ctx.") {
		v, ok := env.lookup(strings.TrimPrefix(trimmed, "ctx."))
		if b, ok := v.(bool); ok {
			return b, nil
		}
		if !ok || v == nil {
			return false, nil
		}
		return false, &apiError{Type: "script_exception", Status: 400, Reason: "runtime error",
			Fields: map[string]any{"lang": "painless"},
			CausedBy: &apiError{Type: "class_cast_exception", Status: 400,
				Reason: "cannot cast def [" + painlessDefType(v) + "] to boolean"}}
	}
	return false, &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
		Fields: map[string]any{"processor_type": env.processor}}
}

func painlessDefType(v any) string {
	switch t := v.(type) {
	case int, int64:
		return "int"
	case float64:
		return "double"
	case string:
		return "String"
	case bool:
		return "boolean"
	case []any:
		return "ArrayList"
	case map[string]any:
		return "HashMap"
	case json.Number:
		if strings.ContainsAny(t.String(), ".eE") {
			return "double"
		}
		return "int"
	}
	return "def"
}

func parseConditionLiteral(rhs string, params any) any {
	rhs = strings.TrimSpace(rhs)
	if strings.HasPrefix(rhs, "params.") && params != nil {
		key := strings.TrimPrefix(rhs, "params.")
		if pm, ok := params.(map[string]any); ok {
			return pm[key]
		}
	}
	if len(rhs) >= 2 && ((rhs[0] == '\'' && rhs[len(rhs)-1] == '\'') || (rhs[0] == '"' && rhs[len(rhs)-1] == '"')) {
		return rhs[1 : len(rhs)-1]
	}
	if rhs == "true" {
		return true
	}
	if rhs == "false" {
		return false
	}
	if rhs == "null" {
		return nil
	}
	if i, err := strconv.ParseInt(rhs, 10, 64); err == nil {
		return i
	}
	if f, err := strconv.ParseFloat(rhs, 64); err == nil {
		return f
	}
	return rhs
}

func balancedParens(s string) bool {
	depth := 0
	for _, ch := range s {
		switch ch {
		case '(':
			depth++
		case ')':
			depth--
			if depth < 0 {
				return false
			}
		}
	}
	return depth == 0
}

func ternarySplit(s string) (int, int) {
	depth := 0
	q, c := -1, -1
	inStr := rune(0)
	for i, ch := range s {
		if inStr != 0 {
			if ch == inStr {
				inStr = 0
			}
			continue
		}
		if ch == '\'' || ch == '"' {
			inStr = ch
			continue
		}
		switch ch {
		case '(', '[':
			depth++
		case ')', ']':
			depth--
		case '?':
			if depth == 0 && q < 0 && i > 0 && s[i-1] == ' ' && i+1 < len(s) && s[i+1] == ' ' {
				q = i
			}
		case ':':
			if depth == 0 && q >= 0 && c < 0 && i > 0 && s[i-1] == ' ' && i+1 < len(s) && s[i+1] == ' ' {
				c = i
			}
		}
	}
	return q, c
}

func splitTopLevel(s, sep string) ([]string, bool) {
	var parts []string
	depth := 0
	inStr := byte(0)
	start := 0
	for i := 0; i < len(s); i++ {
		ch := s[i]
		if inStr != 0 {
			if ch == inStr {
				inStr = 0
			}
			continue
		}
		if ch == '\'' || ch == '"' {
			inStr = ch
			continue
		}
		switch ch {
		case '(', '[':
			depth++
		case ')', ']':
			depth--
		}
		if depth == 0 && strings.HasPrefix(s[i:], sep) {
			parts = append(parts, strings.TrimSpace(s[start:i]))
			i += len(sep) - 1
			start = i + 1
		}
	}
	if len(parts) == 0 {
		return nil, false
	}
	parts = append(parts, strings.TrimSpace(s[start:]))
	return parts, true
}

func findTopLevelOp(s, op string) int {
	depth := 0
	inStr := byte(0)
	for i := 0; i < len(s); i++ {
		ch := s[i]
		if inStr != 0 {
			if ch == inStr {
				inStr = 0
			}
			continue
		}
		if ch == '\'' || ch == '"' {
			inStr = ch
			continue
		}
		switch ch {
		case '(', '[':
			depth++
		case ')', ']':
			depth--
		}
		if depth == 0 && strings.HasPrefix(s[i:], op) {
			return i
		}
	}
	return -1
}
