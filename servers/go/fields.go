package main

import (
	"encoding/json"
	"fmt"
	"math"
	"net"
	"strconv"
	"strings"
	"time"
)

// ---------------------------------------------------------------------------
// Index-time field value extraction.
//
// OpenSearch stores an indexed (normalized) form of each field value that is
// independent of the original _source. Term/range queries, sorting and
// aggregations run against these values, while _source is echoed verbatim.
// ---------------------------------------------------------------------------

type indexCtx struct {
	idx            *Index
	indexCoerce    bool
	indexIgnoreMal bool
	docID          string
	doc            map[string]any
	ignored        []string
	ignoredSet     map[string]bool
	values         map[string][]any
	totalFields    int
	fieldNameLimit int
}

func newIndexCtx(idx *Index, docID string) *indexCtx {
	c := &indexCtx{
		idx:        idx,
		docID:      docID,
		ignoredSet: map[string]bool{},
		values:     map[string][]any{},
	}
	c.indexCoerce = true
	if v, ok := findSetting(idx.Settings, "mapping.coerce"); ok {
		c.indexCoerce = settingsValueString(v) != "false"
	}
	if v, ok := findSetting(idx.Settings, "mapping.ignore_malformed"); ok {
		c.indexIgnoreMal = settingsValueString(v) == "true"
	}
	c.totalFields = 1000
	if v, ok := settingInt(idx.Settings, "mapping.total_fields.limit"); ok {
		c.totalFields = v
	}
	c.fieldNameLimit = 1000
	if v, ok := settingInt(idx.Settings, "mapping.field_name_length.limit"); ok {
		c.fieldNameLimit = v
	}
	return c
}

func mapperErr(typ, reason string, causedType, causedReason string) *apiError {
	e := &apiError{Type: "mapper_parsing_exception", Reason: reason, Status: 400}
	if causedType != "" {
		e.CausedBy = &apiError{Type: causedType, Reason: causedReason}
	} else if causedReason != "" {
		e.CausedBy = &apiError{Type: "illegal_argument_exception", Reason: causedReason}
	}
	return e
}

// fieldValueError builds the standard "failed to parse field [f] of type [t] in
// document with id 'x'" wrapper.
func (c *indexCtx) fieldValueError(name, typ string, v any, causeType, causeReason string) *apiError {
	preview := previewValue(v)
	reason := fmt.Sprintf("failed to parse field [%s] of type [%s] in document with id '%s'. Preview of field's value: '%s'", name, typ, c.docID, preview)
	return mapperErr("mapper_parsing_exception", reason, causeType, causeReason)
}

func previewValue(v any) string {
	switch t := v.(type) {
	case nil:
		return "null"
	case string:
		return t
	case bool:
		return strconv.FormatBool(t)
	case float64:
		return formatNumber(t)
	case json.Number:
		return t.String()
	case map[string]any:
		return fmt.Sprintf("{...}")
	case []any:
		return "[...]"
	default:
		return fmt.Sprintf("%v", v)
	}
}

// numericKind classifies the numeric field types.
func isIntegerKind(t string) bool {
	switch t {
	case "byte", "short", "integer", "long", "unsigned_long":
		return true
	}
	return false
}

func intBounds(t string) (int64, uint64, bool) {
	switch t {
	case "byte":
		return math.MaxInt8, math.MaxUint8 + 1, true
	case "short":
		return math.MaxInt16, math.MaxUint16 + 1, true
	case "integer":
		return math.MaxInt32, math.MaxUint32 + 1, true
	case "long":
		return math.MaxInt64, 0, false
	}
	return 0, 0, false
}

func typeDisplayName(t string) string {
	switch t {
	case "byte":
		return "byte"
	case "short":
		return "short"
	case "integer":
		return "integer"
	case "long":
		return "long"
	case "unsigned_long":
		return "unsigned long"
	case "float":
		return "float"
	case "double":
		return "double"
	case "half_float":
		return "half_float"
	}
	return t
}

// parseIntValue converts a raw JSON number/string into an int64 value subject
// to OpenSearch coercion rules.
func (c *indexCtx) parseIntValue(name, typ string, def map[string]any, v any) (any, *apiError) {
	coerce := c.indexCoerce
	if b, ok := def["coerce"].(bool); ok {
		coerce = b
	}
	switch raw := v.(type) {
	case json.Number:
		s := raw.String()
		return c.parseIntString(name, typ, def, s, coerce, false)
	case float64:
		return c.parseIntFloat(name, typ, def, raw, coerce)
	case bool:
		return nil, c.fieldValueError(name, typ, v, "input_coercion_exception",
			fmt.Sprintf("Cannot coerce value %v to %s", raw, typ))
	case string:
		return c.parseIntString(name, typ, def, raw, coerce, true)
	}
	return nil, c.fieldValueError(name, typ, v, "input_coercion_exception", "For input string: \""+previewValue(v)+"\"")
}

func (c *indexCtx) parseIntFloat(name, typ string, def map[string]any, f float64, coerce bool) (any, *apiError) {
	if !coerce {
		if f != math.Trunc(f) {
			return nil, c.fieldValueError(name, typ, formatNumber(f), "illegal_argument_exception",
				fmt.Sprintf("%s cannot be converted to Integer without data loss", formatNumber(f)))
		}
	}
	iv := int64(f)
	return c.checkIntRange(name, typ, iv, f)
}

func (c *indexCtx) checkIntRange(name, typ string, iv int64, orig float64) (any, *apiError) {
	maxI, _, bounded := intBounds(typ)
	switch {
	case typ == "byte":
		if iv > math.MaxInt8 || iv < math.MinInt8 {
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("Value [%d] is out of range for a byte", iv))
		}
	case typ == "short":
		if iv > math.MaxInt16 || iv < math.MinInt16 {
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("Value [%d] is out of range for a short", iv))
		}
	case typ == "integer":
		if iv > math.MaxInt32 || iv < math.MinInt32 {
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("Value [%d] is out of range for an integer", iv))
		}
	}
	_ = maxI
	_ = bounded
	if typ == "unsigned_long" {
		if iv < 0 {
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("Value [%d] is out of range for an unsigned long", iv))
		}
	}
	switch typ {
	case "byte", "short", "integer", "long":
		return iv, nil
	case "unsigned_long":
		return uint64(iv), nil
	}
	return iv, nil
}

func (c *indexCtx) parseIntString(name, typ string, def map[string]any, s string, coerce, trim bool) (any, *apiError) {
	coerceFalse := false
	if b, ok := def["coerce"].(bool); ok {
		coerceFalse = !b
	}
	if !coerce && !coerceFalse {
		coerce = false
	}
	if coerceFalse {
		word := "Integer"
		if !isIntegerKind(typ) {
			word = "Float"
		}
		return nil, c.fieldValueError(name, typ, s, "illegal_argument_exception",
			fmt.Sprintf("%s value passed as String", word))
	}
	orig := s
	if trim {
		s = strings.TrimSpace(s)
	}
	if typ == "unsigned_long" {
		u, err := strconv.ParseUint(s, 10, 64)
		if err != nil {
			if ne, ok := err.(*strconv.NumError); ok && ne.Err == strconv.ErrRange {
				return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
					fmt.Sprintf("Value [%s] is out of range for an unsigned long", orig))
			}
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("For input string: \"%s\"", orig))
		}
		return u, nil
	}
	iv, err := strconv.ParseInt(s, 10, 64)
	if err != nil {
		if ne, ok := err.(*strconv.NumError); ok && ne.Err == strconv.ErrRange {
			return nil, c.fieldValueError(name, typ, orig, "illegal_argument_exception",
				fmt.Sprintf("Value [%s] is out of range for a %s", orig, typeDisplayName(typ)))
		}
		// integers accept decimal-like strings with coercion; floats parse natively
		if isIntegerKind(typ) {
			if f, ferr := strconv.ParseFloat(s, 64); ferr == nil {
				return c.checkIntRange(name, typ, int64(f), f)
			}
			causeType := "number_format_exception"
			if typ == "long" {
				causeType = "illegal_argument_exception"
			}
			return nil, c.fieldValueError(name, typ, orig, causeType,
				fmt.Sprintf("For input string: \"%s\"", orig))
		}
		return nil, c.fieldValueError(name, typ, orig, "number_format_exception",
			fmt.Sprintf("For input string: \"%s\"", orig))
	}
	return c.checkIntRange(name, typ, iv, float64(iv))
}

// parseFloatValue implements float/double/half_float/scaled_float coercion.
func (c *indexCtx) parseFloatValue(name, typ string, def map[string]any, v any) (any, *apiError) {
	coerceFalse := false
	if b, ok := def["coerce"].(bool); ok {
		coerceFalse = !b
	}
	var f float64
	switch raw := v.(type) {
	case json.Number:
		if coerceFalse {
			if !strings.ContainsAny(raw.String(), ".eE") {
				return nil, c.fieldValueError(name, typ, raw.String(), "illegal_argument_exception",
					"Float value passed as String")
			}
			return nil, c.fieldValueError(name, typ, raw.String(), "illegal_argument_exception",
				"Float value passed as String")
		}
		fv, err := raw.Float64()
		if err != nil {
			return nil, c.fieldValueError(name, typ, raw.String(), "number_format_exception",
				"For input string: \""+raw.String()+"\"")
		}
		f = fv
	case float64:
		f = raw
	case bool:
		return nil, c.fieldValueError(name, typ, v, "input_coercion_exception", "Cannot coerce boolean")
	case string:
		if coerceFalse {
			return nil, c.fieldValueError(name, typ, raw, "illegal_argument_exception",
				"Float value passed as String")
		}
		lv := strings.ToLower(strings.TrimSpace(raw))
		if lv == "nan" || lv == "infinity" || lv == "-infinity" || lv == "inf" || lv == "-inf" {
			return nil, c.fieldValueError(name, typ, raw, "illegal_argument_exception",
				fmt.Sprintf("[%s] supports only finite values, but got [%s]", typ, raw))
		}
		if strings.Contains(raw, " ") {
			return nil, c.fieldValueError(name, typ, raw, "number_format_exception",
				fmt.Sprintf("For input string: \"%s\"", raw))
		}
		fv, err := strconv.ParseFloat(strings.TrimSpace(raw), 64)
		if err != nil {
			if ne, ok := err.(*strconv.NumError); ok && ne.Err == strconv.ErrRange {
				return nil, c.fieldValueError(name, typ, raw, "illegal_argument_exception",
					fmt.Sprintf("[%s] supports only finite values, but got [%s]", typ, raw))
			}
			return nil, c.fieldValueError(name, typ, raw, "number_format_exception",
				fmt.Sprintf("For input string: \"%s\"", raw))
		}
		f = fv
	default:
		return nil, c.fieldValueError(name, typ, v, "input_coercion_exception", "malformed")
	}
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return nil, c.fieldValueError(name, typ, v, "illegal_argument_exception",
			fmt.Sprintf("[%s] supports only finite values, but got [%s]", typ, previewValue(v)))
	}
	switch typ {
	case "float":
		if f > math.MaxFloat32 || f < -math.MaxFloat32 {
			return nil, c.fieldValueError(name, typ, v, "input_coercion_exception",
				fmt.Sprintf("Value [%s] is out of range for a float", formatNumber(f)))
		}
		return float64(float32(f)), nil
	case "half_float":
		if f > 65504 || f < -65504 {
			return nil, c.fieldValueError(name, typ, v, "input_coercion_exception",
				fmt.Sprintf("Value [%s] is out of range for a half_float", formatNumber(f)))
		}
		return float64(float32FromHalf(float32Half(f))), nil
	case "scaled_float":
		scale := 1.0
		if s, ok := toFloat(def["scaling_factor"]); ok {
			scale = s
		}
		return math.Round(f*scale) / scale, nil
	default:
		return f, nil
	}
}

func float32Half(f float64) uint16 {
	// round-to-nearest-even conversion to IEEE 754 half precision
	bits := math.Float32bits(float32(f))
	sign := uint16(bits>>31) << 15
	exp := int32((bits>>23)&0xff) - 127
	frac := bits & 0x7fffff
	if exp > 15 {
		return sign | 0x7c00
	}
	if exp < -24 {
		return sign
	}
	if exp < -14 {
		shift := uint32(-exp - 14)
		m := frac | 0x800000
		half := uint16(m >> (13 + shift))
		rem := m & ((1 << (13 + shift)) - 1)
		halfRound := uint32(1) << (12 + shift)
		if rem > halfRound || (rem == halfRound && half&1 == 1) {
			half++
		}
		return sign | half
	}
	half := uint16(exp+15)<<10 | uint16(frac>>13)
	rem := frac & 0x1fff
	if rem > 0x1000 || (rem == 0x1000 && half&1 == 1) {
		half++
	}
	return sign | half
}

func float32FromHalf(h uint16) float32 {
	sign := uint32(h>>15) << 31
	exp := uint32(h>>10) & 0x1f
	frac := uint32(h & 0x3ff)
	switch {
	case exp == 0:
		if frac == 0 {
			return math.Float32frombits(sign)
		}
		e := uint32(127 - 15 + 1)
		for frac&0x400 == 0 {
			frac <<= 1
			e--
		}
		frac &= 0x3ff
		return math.Float32frombits(sign | e<<23 | frac<<13)
	case exp == 0x1f:
		return math.Float32frombits(sign | 0x7f800000 | frac<<13)
	default:
		return math.Float32frombits(sign | (exp+127-15)<<23 | frac<<13)
	}
}

// parseBooleanValue implements boolean coercion.
func (c *indexCtx) parseBooleanValue(name string, def map[string]any, v any) (any, *apiError) {
	switch raw := v.(type) {
	case bool:
		return raw, nil
	case string:
		if raw == "" {
			return false, nil
		}
		if raw == "true" || raw == "false" {
			return raw == "true", nil
		}
		return nil, c.fieldValueError(name, "boolean", v, "illegal_argument_exception",
			fmt.Sprintf("Failed to parse value [%s] as only [true] or [false] are allowed.", raw))
	case json.Number:
		s := raw.String()
		if s == "" {
			return false, nil
		}
		return nil, c.fieldValueError(name, "boolean", v, "illegal_argument_exception",
			fmt.Sprintf("Failed to parse value [%s] as only [true] or [false] are allowed.", s))
	default:
		return nil, c.fieldValueError(name, "boolean", v, "illegal_argument_exception",
			fmt.Sprintf("Failed to parse value [%s] as only [true] or [false] are allowed.", previewValue(v)))
	}
}

// parseDateValue stores epoch millis (date) or epoch nanos (date_nanos).
func (c *indexCtx) parseDateValue(name, typ string, def map[string]any, v any) (any, *apiError) {
	if s, ok := v.(string); ok {
		if s == "" {
			return nil, c.fieldValueError(name, typ, v, "parse_exception", "failed to parse date field [] with format")
		}
	}
	if raw, ok := v.(json.Number); ok {
		// numeric epoch millis; negative values parse as year by default format
		f, _ := raw.Float64()
		if typ == "date_nanos" {
			return int64(f), nil
		}
		return int64(f), nil
	}
	s, ok := v.(string)
	if !ok {
		return nil, c.fieldValueError(name, typ, v, "illegal_argument_exception", "failed to parse date field")
	}
	ms, ok2 := parseDateAny(def, s)
	if !ok2 {
		return nil, c.fieldValueError(name, typ, v, "parse_exception",
			fmt.Sprintf("failed to parse date field [%s] with format [%s]", s, dateFormatOf(def)))
	}
	if typ == "date_nanos" {
		if ns, ok := parseDateNanosAny(def, s); ok {
			return ns, nil
		}
		return ms * int64(1e6), nil
	}
	return ms, nil
}

func dateFormatOf(def map[string]any) string {
	if f, ok := def["format"].(string); ok {
		return f
	}
	return "strict_date_optional_time||epoch_millis"
}

var defaultDateLayouts = []string{
	time.RFC3339Nano,
	"2006-01-02T15:04:05.000Z07:00",
	"2006-01-02T15:04:05Z07:00",
	"2006-01-02T15:04:05",
	"2006-01-02 15:04:05",
	"2006-01-02",
	"2006-01",
	"2006",
}

// javaDateFormatToGo converts the common subset of Java date format letters.
func javaDateFormatToGo(f string) string {
	if f == "epoch_millis" || f == "epoch_second" || f == "date_optional_time" ||
		f == "strict_date_optional_time" || f == "strict_date_optional_time_nanos" ||
		f == "date_time" || f == "basic_date_time" || f == "strict_date_optional_time||epoch_millis" {
		return ""
	}
	repl := []struct{ from, to string }{
		{"yyyy", "2006"}, {"yy", "06"},
		{"MM", "01"},
		{"dd", "02"},
		{"HH", "15"}, {"mm", "04"}, {"ss", "05"},
		{"SSS", "000"}, {"SSSSSSSSS", "000000000"},
	}
	out := f
	if strings.Contains(out, "'T'") {
		out = strings.ReplaceAll(out, "'T'", "T")
	}
	for _, r := range repl {
		out = strings.ReplaceAll(out, r.from, r.to)
	}
	return out
}

func parseDateAny(def map[string]any, s string) (int64, bool) {
	if f, ok := def["format"].(string); ok && f != "" {
		for _, fmtPart := range strings.Split(f, "||") {
			fmtPart = strings.TrimSpace(fmtPart)
			switch fmtPart {
			case "epoch_millis":
				if n, err := strconv.ParseInt(s, 10, 64); err == nil {
					return n, true
				}
				continue
			case "epoch_second":
				if n, err := strconv.ParseInt(s, 10, 64); err == nil {
					return n * 1000, true
				}
				continue
			case "date_optional_time", "strict_date_optional_time", "date_time",
				"strict_date_optional_time_nanos", "basic_date_time":
				if ms, ok := parseDateDefault(s); ok {
					return ms, true
				}
				continue
			}
			if layout := javaDateFormatToGo(fmtPart); layout != "" {
				if t, err := time.Parse(layout, s); err == nil {
					return t.UnixMilli(), true
				}
				if t, err := time.ParseInLocation(layout, s, time.UTC); err == nil {
					return t.UnixMilli(), true
				}
			}
		}
		return 0, false
	}
	if ms, ok := parseDateDefault(s); ok {
		return ms, true
	}
	if n, err := strconv.ParseInt(s, 10, 64); err == nil {
		return n, true
	}
	return 0, false
}

func parseDateDefault(s string) (int64, bool) {
	for _, layout := range defaultDateLayouts {
		if t, err := time.Parse(layout, s); err == nil {
			return t.UnixMilli(), true
		}
	}
	return 0, false
}

func parseDateNanosAny(def map[string]any, s string) (int64, bool) {
	if f, ok := def["format"].(string); ok && f != "" {
		for _, fmtPart := range strings.Split(f, "||") {
			fmtPart = strings.TrimSpace(fmtPart)
			if fmtPart == "epoch_millis" {
				if n, err := strconv.ParseInt(s, 10, 64); err == nil {
					return n * int64(1e6), true
				}
			}
		}
	}
	if strings.Contains(s, ".") {
		base := s
		frac := ""
		if i := strings.IndexAny(s, ".Z+-"); i > 0 {
			base = s[:i]
			rest := s[i:]
			if strings.HasPrefix(rest, ".") {
				j := 1
				for j < len(rest) && rest[j] >= '0' && rest[j] <= '9' {
					j++
				}
				frac = rest[1:j]
			}
		}
		if ms, ok := parseDateDefault(base); ok {
			pad := frac
			for len(pad) < 9 {
				pad += "0"
			}
			ns, _ := strconv.ParseInt(pad[:9], 10, 64)
			return ms*int64(1e6) + ns, true
		}
	}
	if ms, ok := parseDateDefault(s); ok {
		return ms * int64(1e6), true
	}
	return 0, false
}

// parseIPValue canonicalizes an IPv4/IPv6 string or CIDR.
func (c *indexCtx) parseIPValue(name string, v any) (any, *apiError) {
	s, ok := toString(v)
	if !ok {
		return nil, c.fieldValueError(name, "ip", v, "illegal_argument_exception", "failed to parse ip")
	}
	if strings.Contains(s, "/") {
		_, ipnet, err := net.ParseCIDR(s)
		if err != nil {
			ones := 0
			if i := strings.Index(s, "/"); i >= 0 {
				ones, _ = strconv.Atoi(s[i+1:])
			}
			return nil, c.fieldValueError(name, "ip", s, "illegal_argument_exception",
				fmt.Sprintf("Illegal prefix length [%d] in [%s]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges", ones, s))
		}
		return ipnet.String(), nil
	}
	ip := net.ParseIP(s)
	if ip == nil {
		return nil, c.fieldValueError(name, "ip", s, "illegal_argument_exception",
			fmt.Sprintf("'%s' is not an IP string literal.", s))
	}
	if v4 := ip.To4(); v4 != nil {
		return v4.String(), nil
	}
	return ip.String(), nil
}

// indexRawField handles a single declared field definition with raw values.
func (c *indexCtx) indexRawField(path string, def map[string]any, raw any) *apiError {
	typ, _ := def["type"].(string)
	if typ == "" {
		if _, hasProps := def["properties"].(map[string]any); hasProps {
			typ = "object"
		} else {
			typ = "object"
		}
	}
	if _, hasProps := def["properties"].(map[string]any); hasProps && (typ == "object" || typ == "nested") {
		return c.indexObject(path, def, raw)
	}
	values, err := c.flattenScalars(path, raw)
	if err != nil {
		return err
	}
	out := []any{}
	ignoreMalformed := c.indexIgnoreMal
	if b, ok := def["ignore_malformed"].(bool); ok {
		ignoreMalformed = b
	}
	nullValue, hasNull := def["null_value"]
	for _, v := range values {
		if v == nil {
			if hasNull {
				nv, err := c.coerce(path, def, typ, nullValue)
				if err != nil {
					return err
				}
				if nv != nil {
					out = append(out, nv)
				}
			}
			continue
		}
		if typ == "object" || typ == "nested" {
			continue
		}
		nv, err := c.coerce(path, def, typ, v)
		if err != nil {
			if ignoreMalformed {
				if _, isObj := v.(map[string]any); !isObj {
					c.addIgnored(path)
				}
				continue
			}
			return err
		}
		if nv != nil {
			out = append(out, nv)
		}
	}
	if len(out) > 0 || hasNull {
		c.values[path] = append(c.values[path], out...)
	}
	// multi-fields
	if fields, ok := def["fields"].(map[string]any); ok {
		for fname, fraw := range fields {
			fdef, ok := fraw.(map[string]any)
			if !ok {
				continue
			}
			sub := path + "." + fname
			subVals := []any{}
			for _, v := range values {
				if v == nil {
					continue
				}
				nv, err := c.coerce(sub, fdef, strOf(fdef["type"]), v)
				if err != nil {
					return err
				}
				if nv != nil {
					subVals = append(subVals, nv)
				}
			}
			if len(subVals) > 0 {
				c.values[sub] = append(c.values[sub], subVals...)
			}
		}
	}
	// copy_to
	if ct, ok := def["copy_to"].(string); ok && ct != "" {
		c.copyTo(ct, c.values[path])
	} else if cts, ok := def["copy_to"].([]any); ok {
		for _, e := range cts {
			if s, ok := e.(string); ok {
				c.copyTo(s, c.values[path])
			}
		}
	}
	return nil
}

func (c *indexCtx) copyTo(target string, vals []any) {
	for _, v := range vals {
		c.values[target] = append(c.values[target], v)
	}
}

func (c *indexCtx) addIgnored(path string) {
	if !c.ignoredSet[path] {
		c.ignoredSet[path] = true
		c.ignored = append(c.ignored, path)
	}
}

func (c *indexCtx) coerce(path string, def map[string]any, typ string, v any) (any, *apiError) {
	switch typ {
	case "text", "match_only_text", "search_as_you_type", "wildcard":
		s, ok := valueScalarString(v)
		if !ok {
			return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception", "malformed")
		}
		if typ == "wildcard" {
			return s, nil
		}
		return s, nil
	case "keyword", "constant_keyword":
		s, ok := valueScalarString(v)
		if !ok {
			return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception", "malformed")
		}
		if norm, ok := def["normalizer"].(string); ok && norm != "" {
			s = applyNormalizer(norm, s)
		}
		return s, nil
	case "integer", "long", "short", "byte", "unsigned_long":
		return c.parseIntValue(path, typ, def, v)
	case "float", "double", "half_float", "scaled_float":
		return c.parseFloatValue(path, typ, def, v)
	case "boolean":
		return c.parseBooleanValue(path, def, v)
	case "date", "date_nanos":
		return c.parseDateValue(path, typ, def, v)
	case "ip":
		return c.parseIPValue(path, v)
	case "binary":
		s, ok := valueScalarString(v)
		if !ok {
			return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception", "malformed")
		}
		if _, present := def["store"]; present {
			if _, err := base64DecodeLenient(s); err != nil {
				return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception",
					"failed to parse [binary] field")
			}
		}
		return s, nil
	case "token_count":
		return c.parseTokenCount(path, def, v)
	case "flat_object":
		s, ok := valueScalarString(v)
		if !ok {
			return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception", "malformed")
		}
		return s, nil
	case "integer_range", "float_range", "long_range", "double_range", "date_range", "ip_range":
		return c.parseRangeValue(path, typ, def, v)
	case "geo_point":
		return c.parseGeoPoint(path, v)
	case "completion":
		return c.parseCompletion(path, def, v)
	case "rank_feature":
		return c.parseRankFeature(path, v)
	case "rank_features":
		return c.parseRankFeature(path, v)
	case "constant_keyword_default":
		return nil, nil
	case "alias":
		return nil, nil
	case "join":
		return c.parseJoin(path, def, v)
	case "percolator":
		return v, nil
	case "derived":
		return nil, nil
	}
	if isNumericType(typ) {
		return c.parseFloatValue(path, typ, def, v)
	}
	if s, ok := valueScalarString(v); ok {
		return s, nil
	}
	return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception", "malformed")
}

func strOf(v any) string {
	s, _ := v.(string)
	return s
}

func valueScalarString(v any) (string, bool) {
	switch t := v.(type) {
	case string:
		return t, true
	case float64:
		return formatNumber(t), true
	case json.Number:
		return t.String(), true
	case bool:
		return strconv.FormatBool(t), true
	}
	return "", false
}

func base64DecodeLenient(s string) ([]byte, error) {
	return nil, nil
}

// flattenScalars flattens arrays into individual scalar values. An empty array
// yields no values; nested arrays are flattened recursively.
func (c *indexCtx) flattenScalars(path string, raw any) ([]any, *apiError) {
	switch t := raw.(type) {
	case []any:
		out := []any{}
		for _, e := range t {
			sub, err := c.flattenScalars(path, e)
			if err != nil {
				return nil, err
			}
			out = append(out, sub...)
		}
		return out, nil
	case map[string]any:
		return []any{t}, nil
	default:
		return []any{raw}, nil
	}
}

func (c *indexCtx) indexObject(path string, def map[string]any, raw any) *apiError {
	props, _ := def["properties"].(map[string]any)
	enabled := true
	if b, ok := def["enabled"].(bool); ok {
		enabled = b
	}
	if !enabled {
		return nil
	}
	if raw == nil {
		return nil
	}
	switch t := raw.(type) {
	case []any:
		for _, e := range t {
			if err := c.indexObject(path, def, e); err != nil {
				return err
			}
		}
		return nil
	case map[string]any:
		return c.indexObjectMap(path, props, t)
	default:
		return nil
	}
}

func (c *indexCtx) indexObjectMap(path string, props map[string]any, src map[string]any) *apiError {
	for k, v := range src {
		if strings.HasPrefix(k, "_") {
			if err := c.rejectMetadataField(k); err != nil {
				return err
			}
			continue
		}
		childPath := k
		if path != "" {
			childPath = path + "." + k
		}
		def, declared := props[k].(map[string]any)
		if !declared {
			// dynamic
			dyn := "true"
			if d, ok := c.idx.Mappings["dynamic"].(string); ok {
				dyn = d
			}
			if dyn == "strict" {
				name := "root"
				if path != "" {
					name = path
				}
				return &apiError{Type: "strict_dynamic_mapping_exception", Status: 400,
					Reason: fmt.Sprintf("mapping set to strict, dynamic introduction of [%s] within [%s] is not allowed", k, name)}
			}
			if dyn == "false" {
				continue
			}
			inferred := inferType(v)
			if inferred == nil {
				continue
			}
			def = inferred
			props[k] = def
			c.bumpTotalFields()
		}
		if err := c.indexRawField(childPath, def, v); err != nil {
			return err
		}
	}
	return nil
}

func (c *indexCtx) rejectMetadataField(name string) *apiError {
	return mapperErr("mapper_parsing_exception",
		fmt.Sprintf("Field [%s] is a metadata field and cannot be added inside a document. Use the index API request parameters.", name),
		"mapper_parsing_exception",
		fmt.Sprintf("Field [%s] is a metadata field and cannot be added inside a document. Use the index API request parameters.", name))
}

func (c *indexCtx) bumpTotalFields() {}

// constantKeywordDefault fills the fixed value for docs missing the field.
func (c *indexCtx) fillConstants(props map[string]any, src map[string]any) {
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		if t, _ := def["type"].(string); t == "constant_keyword" {
			if _, present := src[name]; !present {
				if v, ok := def["value"]; ok {
					c.values[name] = []any{fmt.Sprint(v)}
				}
			}
		}
	}
}

// buildDocValues computes indexed values for a document.
func (c *indexCtx) buildDocValues() *apiError {
	props, _ := c.idx.Mappings["properties"].(map[string]any)
	if props == nil {
		props = map[string]any{}
		c.idx.Mappings["properties"] = props
	}
	if err := c.indexObjectMap("", props, c.doc); err != nil {
		return err
	}
	c.fillConstants(props, c.doc)
	if len(c.ignored) > 0 {
		c.values["_ignored"] = []any{}
		for _, f := range c.ignored {
			c.values["_ignored"] = append(c.values["_ignored"], f)
		}
	}
	return nil
}

// computeDocValues validates and indexes a document, storing normalized field
// values used by search, sort, aggregations and docvalue_fields.
func (idx *Index) computeDocValues(id string, doc map[string]any) *apiError {
	c := newIndexCtx(idx, id)
	c.doc = doc
	if err := c.buildDocValues(); err != nil {
		return err
	}
	if idx.DocValues == nil {
		idx.DocValues = map[string]map[string][]any{}
	}
	idx.DocValues[id] = c.values
	if len(c.ignored) > 0 {
		if idx.DocIgnored == nil {
			idx.DocIgnored = map[string][]string{}
		}
		idx.DocIgnored[id] = c.ignored
		doc["_ignored"] = ignoredValues(c.ignored)
	} else if idx.DocIgnored != nil {
		delete(idx.DocIgnored, id)
		delete(doc, "_ignored")
	}
	return nil
}

func ignoredValues(fields []string) []any {
	out := make([]any, 0, len(fields))
	for _, f := range fields {
		out = append(out, f)
	}
	return out
}

func (idx *Index) removeDocValues(id string) {
	delete(idx.DocValues, id)
	delete(idx.DocIgnored, id)
}

// recomputeAllDocValues rebuilds doc values for every document (used when
// mappings change).
func (idx *Index) recomputeAllDocValues() {
	idx.DocValues = map[string]map[string][]any{}
	idx.DocIgnored = map[string][]string{}
	for _, id := range idx.Order {
		doc, live := idx.Docs[id]
		if !live {
			continue
		}
		_ = idx.computeDocValues(id, doc)
	}
}

func applyNormalizer(name, s string) string {
	switch name {
	case "lowercase":
		return strings.ToLower(s)
	}
	return s
}

func (c *indexCtx) parseTokenCount(path string, def map[string]any, v any) (any, *apiError) {
	s, ok := valueScalarString(v)
	if !ok {
		return nil, c.fieldValueError(path, "token_count", v, "illegal_argument_exception", "malformed")
	}
	tokens := analyze(s)
	return int64(len(tokens)), nil
}

func (c *indexCtx) parseGeoPoint(path string, v any) (any, *apiError) {
	switch t := v.(type) {
	case string:
		parts := strings.Split(t, ",")
		if len(parts) == 2 {
			lat, err1 := strconv.ParseFloat(strings.TrimSpace(parts[0]), 64)
			lon, err2 := strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
			if err1 == nil && err2 == nil {
				return map[string]any{"lat": lat, "lon": lon}, nil
			}
		}
		return nil, c.fieldValueError(path, "geo_point", v, "illegal_argument_exception", "failed to parse geo_point")
	case map[string]any:
		if lat, ok := toFloat(t["lat"]); ok {
			if lon, ok := toFloat(t["lon"]); ok {
				return map[string]any{"lat": lat, "lon": lon}, nil
			}
		}
		return nil, c.fieldValueError(path, "geo_point", v, "illegal_argument_exception", "failed to parse geo_point")
	case []any:
		if len(t) == 2 {
			lon, ok1 := toFloat(t[0])
			lat, ok2 := toFloat(t[1])
			if ok1 && ok2 {
				return map[string]any{"lat": lat, "lon": lon}, nil
			}
		}
		return nil, c.fieldValueError(path, "geo_point", v, "illegal_argument_exception", "failed to parse geo_point")
	}
	if s, ok := valueScalarString(v); ok {
		return c.parseGeoPoint(path, s)
	}
	return nil, c.fieldValueError(path, "geo_point", v, "illegal_argument_exception", "failed to parse geo_point")
}

func (c *indexCtx) parseCompletion(path string, def map[string]any, v any) (any, *apiError) {
	if m, ok := v.(map[string]any); ok {
		in, _ := m["input"].([]any)
		out := []any{}
		for _, e := range in {
			if s, ok := valueScalarString(e); ok {
				out = append(out, s)
			}
		}
		if len(out) == 0 {
			out = append(out, m)
		}
		return out, nil
	}
	if s, ok := valueScalarString(v); ok {
		return s, nil
	}
	return nil, c.fieldValueError(path, "completion", v, "illegal_argument_exception", "failed to parse completion")
}

func (c *indexCtx) parseRankFeature(path string, v any) (any, *apiError) {
	f, ok := toFloat(v)
	if !ok {
		if s, ok := valueScalarString(v); ok {
			if fv, err := strconv.ParseFloat(s, 64); err == nil {
				f = fv
				ok = true
			}
		}
	}
	if !ok {
		return nil, c.fieldValueError(path, "rank_feature", v, "illegal_argument_exception", "failed to parse rank_feature")
	}
	if f < 0 {
		return nil, c.fieldValueError(path, "rank_feature", v, "illegal_argument_exception",
			"Field value must be >= 0")
	}
	return f, nil
}

func (c *indexCtx) parseJoin(path string, def map[string]any, v any) (any, *apiError) {
	relations, _ := def["relations"].(map[string]any)
	switch t := v.(type) {
	case string:
		if _, ok := relations[t]; ok {
			return t, nil
		}
		return nil, c.fieldValueError(path, "join", v, "illegal_argument_exception",
			fmt.Sprintf("[%s] is not a valid join name", t))
	case map[string]any:
		name, _ := t["name"].(string)
		parent, _ := t["parent"].(string)
		if name == "" {
			return nil, c.fieldValueError(path, "join", v, "illegal_argument_exception", "join name is missing")
		}
		if _, ok := relations[name]; !ok {
			children, ok2 := relations[parent]
			valid := false
			if ok2 {
				if list, ok3 := children.([]any); ok3 {
					for _, e := range list {
						if s, ok := e.(string); ok && s == name {
							valid = true
						}
					}
				}
			}
			if !valid {
				return nil, c.fieldValueError(path, "join", v, "illegal_argument_exception",
					fmt.Sprintf("[%s] is not a valid join name", name))
			}
		}
		return map[string]any{"name": name, "parent": parent}, nil
	}
	return nil, c.fieldValueError(path, "join", v, "illegal_argument_exception", "join name is missing")
}

// ---------------------------------------------------------------------------
// Range field types
// ---------------------------------------------------------------------------

func (c *indexCtx) parseRangeValue(path, typ string, def map[string]any, v any) (any, *apiError) {
	if s, ok := v.(string); ok {
		if typ == "ip_range" {
			vals, err := c.parseIPValue(path, s)
			if err != nil {
				return nil, err
			}
			return map[string]any{"gte": vals, "lte": vals}, nil
		}
		return nil, c.fieldValueError(path, typ, v, "mapper_parsing_exception",
			fmt.Sprintf("error parsing field [%s], expected an object but got %s", path, s))
	}
	m, ok := v.(map[string]any)
	if !ok {
		return nil, c.fieldValueError(path, typ, v, "mapper_parsing_exception",
			fmt.Sprintf("error parsing field [%s], expected an object but got %v", path, previewValue(v)))
	}
	hasGt, hasGte := m["gt"] != nil, m["gte"] != nil
	if hasGt && hasGte {
		return nil, c.fieldValueError(path, typ, v, "mapper_parsing_exception",
			fmt.Sprintf("error parsing field [%s], invalid lower bound (gt/gte)", path))
	}
	hasLt, hasLte := m["lt"] != nil, m["lte"] != nil
	if hasLt && hasLte {
		return nil, c.fieldValueError(path, typ, v, "mapper_parsing_exception",
			fmt.Sprintf("error parsing field [%s], invalid upper bound (lt/lte)", path))
	}
	parse := func(raw any) (any, bool, *apiError) {
		switch typ {
		case "date_range":
			rv, err := c.parseDateValue(path, "date", def, raw)
			if err != nil {
				return nil, false, err
			}
			return rv, true, nil
		case "ip_range":
			rv, err := c.parseIPValue(path, raw)
			if err != nil {
				return nil, false, err
			}
			return rv, true, nil
		default:
			isFloat := typ == "float_range" || typ == "double_range"
			if isFloat {
				rv, err := c.parseFloatValue(path, strings.TrimSuffix(typ, "_range"), def, raw)
				if err != nil {
					if isIntegerKind(strings.TrimSuffix(typ, "_range")) {
						return nil, false, err
					}
					return nil, false, err
				}
				return rv, true, nil
			}
			rv, err := c.parseIntValue(path, strings.TrimSuffix(typ, "_range"), def, raw)
			if err != nil {
				return nil, false, err
			}
			return rv, true, nil
		}
	}
	out := map[string]any{}
	for _, key := range []string{"gt", "gte", "lt", "lte"} {
		raw, present := m[key]
		if !present {
			continue
		}
		pv, _, err := parse(raw)
		if err != nil {
			return nil, err
		}
		out[key] = pv
	}
	for k := range m {
		if k != "gt" && k != "gte" && k != "lt" && k != "lte" {
			return nil, c.fieldValueError(path, typ, v, "mapper_parsing_exception",
				fmt.Sprintf("error parsing field [%s], with unknown parameter [%s]", path, k))
		}
	}
	gte, hasLower := out["gte"]
	if !hasLower {
		gte = out["gt"]
	}
	lte, hasUpper := out["lte"]
	if !hasUpper {
		lte = out["lt"]
	}
	if hasLower && hasUpper {
		if compareValuesForType(typ, gte, lte) > 0 {
			return nil, c.fieldValueError(path, typ, v, "illegal_argument_exception",
				fmt.Sprintf("min value (%v) is greater than max value (%v)", boundsValue(gte), boundsValue(lte)))
		}
	}
	return out, nil
}

func boundsValue(v any) string {
	switch t := v.(type) {
	case float64:
		return formatNumber(t)
	case int64:
		return strconv.FormatInt(t, 10)
	case uint64:
		return strconv.FormatUint(t, 10)
	case string:
		return t
	default:
		return fmt.Sprintf("%v", v)
	}
}

func compareValuesForType(typ string, a, b any) int {
	switch typ {
	case "ip_range":
		return compareIPs(a.(string), b.(string))
	default:
		af, _ := toFloat(a)
		bf, _ := toFloat(b)
		switch {
		case af < bf:
			return -1
		case af > bf:
			return 1
		}
		return 0
	}
}

func compareIPs(a, b string) int {
	_, an, _ := net.ParseCIDR(a)
	if an == nil {
		ip := net.ParseIP(a)
		if ip == nil {
			return 0
		}
		return 0
	}
	_, bn, _ := net.ParseCIDR(b)
	if bn == nil {
		return 0
	}
	return strings.Compare(an.String(), bn.String())
}
