package main

import (
	"encoding/json"
	"fmt"
	"net"
	"regexp"
	"strconv"
	"strings"
)

// ---------------------------------------------------------------------------
// Query evaluation against indexed (doc values) field values.
// ---------------------------------------------------------------------------

type queryCtx struct {
	idx  *Index
	id   string
	doc  map[string]any
	vals map[string][]any
}

func newQueryCtxForDoc(idx *Index, id string, doc map[string]any) *queryCtx {
	if _, ok := idx.DocValues[id]; ok {
		return &queryCtx{idx: idx, id: id, doc: doc}
	}
	c := newIndexCtx(idx, id)
	c.doc = doc
	if err := c.buildDocValues(); err != nil {
		return &queryCtx{idx: idx, id: id, doc: doc}
	}
	return &queryCtx{idx: idx, id: id, doc: doc, vals: c.values}
}

func (c *queryCtx) rawValues() (map[string][]any, bool) {
	if c.vals != nil {
		return c.vals, true
	}
	if dv, ok := c.idx.DocValues[c.id]; ok {
		return dv, true
	}
	return nil, false
}

func (c *queryCtx) values(field string) ([]any, bool) {
	resolved := resolveAliasField(c.idx, field)
	if resolved == "_id" {
		return []any{c.id}, true
	}
	if dv, ok := c.rawValues(); ok {
		if vals, ok := dv[resolved]; ok {
			return vals, len(vals) > 0
		}
		if resolved != field {
			if vals, ok := dv[field]; ok {
				return vals, len(vals) > 0
			}
		}
	}
	return nil, false
}

func (c *queryCtx) hasField(field string) bool {
	resolved := resolveAliasField(c.idx, field)
	if resolved == "_id" {
		return true
	}
	if dv, ok := c.rawValues(); ok {
		if vals, ok := dv[resolved]; ok && len(vals) > 0 {
			return true
		}
		for path := range dv {
			if strings.HasPrefix(path, resolved+".") {
				return true
			}
		}
	}
	return false
}

func resolveAliasField(idx *Index, field string) string {
	for i := 0; i < 10; i++ {
		def, ok := mappingFieldDef(idx, field)
		if !ok {
			return field
		}
		if t, _ := def["type"].(string); t == "alias" {
			path, _ := def["path"].(string)
			if path == "" || path == field {
				return field
			}
			field = path
			continue
		}
		return field
	}
	return field
}

// mappingFieldDef looks up a field definition by dotted path.
func mappingFieldDef(idx *Index, field string) (map[string]any, bool) {
	props, _ := idx.Mappings["properties"].(map[string]any)
	if props == nil {
		return nil, false
	}
	parts := strings.Split(field, ".")
	cur := props
	for i := 0; i < len(parts); i++ {
		// literal dotted key takes precedence
		if i+1 < len(parts) {
			joined := strings.Join(parts[i:], ".")
			if def, ok := cur[joined].(map[string]any); ok {
				return def, true
			}
		}
		def, ok := cur[parts[i]].(map[string]any)
		if !ok {
			return nil, false
		}
		if i == len(parts)-1 {
			return def, true
		}
		sub, ok := def["properties"].(map[string]any)
		if !ok {
			return nil, false
		}
		cur = sub
	}
	return nil, false
}

func (idx *Index) objectPaths() map[string]bool {
	out := map[string]bool{}
	var walk func(prefix string, props map[string]any)
	walk = func(prefix string, props map[string]any) {
		for name, raw := range props {
			def, ok := raw.(map[string]any)
			if !ok {
				continue
			}
			path := name
			if prefix != "" {
				path = prefix + "." + name
			}
			if sub, ok := def["properties"].(map[string]any); ok {
				out[path] = true
				walk(path, sub)
			}
		}
	}
	props, _ := idx.Mappings["properties"].(map[string]any)
	walk("", props)
	return out
}

// ---------------------------------------------------------------------------
// typed comparisons
// ---------------------------------------------------------------------------

func numericEqual(a, b any) bool {
	if ua, ok := a.(uint64); ok {
		if ub, ok := b.(uint64); ok {
			return ua == ub
		}
		if ib, ok := integerOf(b); ok && ib >= 0 {
			return ua == uint64(ib)
		}
		return false
	}
	if ub, ok := b.(uint64); ok {
		if ia, ok := integerOf(a); ok && ia >= 0 {
			return ub == uint64(ia)
		}
	}
	if ia, ok := integerOf(a); ok {
		if ib, ok := integerOf(b); ok {
			return ia == ib
		}
	}
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	return aok && bok && af == bf
}

func integerOf(v any) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case int:
		return int64(t), true
	case uint64:
		if t <= 1<<63-1 {
			return int64(t), true
		}
		return 0, false
	case json.Number:
		if n, err := t.Int64(); err == nil {
			return n, true
		}
	}
	return 0, false
}

func numericLess(a, b any) int {
	if ua, ok := a.(uint64); ok {
		var ub uint64
		if v, ok := b.(uint64); ok {
			ub = v
		} else if ib, ok := integerOf(b); ok && ib >= 0 {
			ub = uint64(ib)
		} else {
			bf, _ := toFloat(b)
			if float64(ua) < bf {
				return -1
			} else if float64(ua) > bf {
				return 1
			}
			return 0
		}
		switch {
		case ua < ub:
			return -1
		case ua > ub:
			return 1
		}
		return 0
	}
	if ub, ok := b.(uint64); ok {
		if ia, ok := integerOf(a); ok {
			if ia < 0 {
				return -1
			}
			switch {
			case uint64(ia) < ub:
				return -1
			case uint64(ia) > ub:
				return 1
			}
			return 0
		}
	}
	if ia, ok := integerOf(a); ok {
		if ib, ok := integerOf(b); ok {
			switch {
			case ia < ib:
				return -1
			case ia > ib:
				return 1
			}
			return 0
		}
	}
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	if aok && bok {
		switch {
		case af < bf:
			return -1
		case af > bf:
			return 1
		}
		return 0
	}
	as, _ := toString(a)
	bs, _ := toString(b)
	return strings.Compare(as, bs)
}

func numberKey(v any) string {
	switch t := v.(type) {
	case int64:
		return strconv.FormatInt(t, 10)
	case uint64:
		return strconv.FormatUint(t, 10)
	case json.Number:
		return t.String()
	case float64:
		return formatNumber(t)
	case bool:
		return strconv.FormatBool(t)
	case nil:
		return "null"
	default:
		return fmt.Sprintf("%v", t)
	}
}

// parseTermValue converts a term query value into the indexed representation
// for the field type.
func parseTermValue(ftype string, def map[string]any, v any) ([]any, *apiError) {
	switch ftype {
	case "integer", "long", "short", "byte", "unsigned_long":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		parsed, err := ctx.parseIntValue("_value", ftype, def, v)
		if err != nil {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("failed to parse value [%s] as %s", previewValue(v), ftype)}
		}
		return []any{parsed}, nil
	case "float", "double", "half_float", "scaled_float":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		parsed, err := ctx.parseFloatValue("_value", ftype, def, v)
		if err != nil {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("failed to parse value [%s] as %s", previewValue(v), ftype)}
		}
		return []any{parsed}, nil
	case "boolean":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		parsed, err := ctx.parseBooleanValue("_value", def, v)
		if err != nil {
			return nil, err
		}
		return []any{parsed}, nil
	case "date", "date_nanos":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		typ := ftype
		parsed, err := ctx.parseDateValue("_value", typ, def, v)
		if err != nil {
			return nil, err
		}
		return []any{parsed}, nil
	case "ip":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		parsed, err := ctx.parseIPValue("_value", v)
		if err != nil {
			return nil, err
		}
		return []any{parsed}, nil
	case "integer_range", "float_range", "long_range", "double_range", "date_range", "ip_range":
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		base := strings.TrimSuffix(ftype, "_range")
		switch base {
		case "date":
			parsed, err := ctx.parseDateValue("_value", "date", def, v)
			if err != nil {
				return nil, err
			}
			return []any{parsed}, nil
		case "ip":
			parsed, err := ctx.parseIPValue("_value", v)
			if err != nil {
				return nil, err
			}
			return []any{parsed}, nil
		case "integer", "long":
			parsed, err := ctx.parseIntValue("_value", base, def, v)
			if err != nil {
				return nil, err
			}
			return []any{parsed}, nil
		case "float", "double":
			parsed, err := ctx.parseFloatValue("_value", base, def, v)
			if err != nil {
				return nil, err
			}
			return []any{parsed}, nil
		}
		return []any{v}, nil
	default:
		s, ok := valueScalarString(v)
		if !ok {
			s = previewValue(v)
		}
		return []any{s}, nil
	}
}

// ipContains reports whether needle (an IP or CIDR) covers the doc value.
func ipContains(host, needle string) bool {
	if strings.Contains(needle, "/") {
		_, network, err := net.ParseCIDR(needle)
		if err != nil {
			return false
		}
		ip := net.ParseIP(host)
		return ip != nil && network.Contains(ip)
	}
	return host == canonicalIP(needle)
}

func canonicalIP(s string) string {
	ip := net.ParseIP(s)
	if ip == nil {
		return s
	}
	if v4 := ip.To4(); v4 != nil {
		return v4.String()
	}
	return ip.String()
}

// inRangeField reports whether v lies within a stored range value.
func inRangeField(rng map[string]any, v any) bool {
	lower, hasLower := rng["gte"]
	upper, hasUpper := rng["lte"]
	if !hasLower {
		if val, ok := rng["gt"]; ok {
			lower, hasLower = val, true
			if hasUpper {
				_ = upper
			}
		}
	}
	if !hasUpper {
		if val, ok := rng["lt"]; ok {
			upper, hasUpper = val, true
		}
	}
	if hasLower {
		op := "gte"
		if _, ok := rng["gt"]; ok {
			op = "gt"
		}
		c := compareAny(lower, v)
		if op == "gte" && c > 0 {
			return false
		}
		if op == "gt" && c >= 0 {
			return false
		}
	}
	if hasUpper {
		op := "lte"
		if _, ok := rng["lt"]; ok {
			op = "lt"
		}
		c := compareAny(upper, v)
		if op == "lte" && c < 0 {
			return false
		}
		if op == "lt" && c <= 0 {
			return false
		}
	}
	return true
}

func compareAny(a, b any) int {
	if as, ok := a.(string); ok {
		if bs, ok2 := b.(string); ok2 {
			return strings.Compare(as, bs)
		}
		return strings.Compare(as, previewValue(b))
	}
	return numericLess(a, b)
}

// rangeRelation evaluates the relation between a query range and a stored range.
func rangeRelation(relation string, qLower, qUpper any, hasLower, hasUpper bool, stored map[string]any) bool {
	sLower, sHasLower := stored["gte"]
	sUpper, sHasUpper := stored["lte"]
	if !sHasLower {
		if v, ok := stored["gt"]; ok {
			sLower, sHasLower = v, true
		}
	}
	if !sHasUpper {
		if v, ok := stored["lt"]; ok {
			sUpper, sHasUpper = v, true
		}
	}
	intersects := true
	if hasUpper && sHasLower {
		if compareAny(sLower, qUpper) > 0 {
			intersects = false
		}
	}
	if hasLower && sHasUpper {
		if compareAny(sUpper, qLower) < 0 {
			intersects = false
		}
	}
	switch strings.ToUpper(relation) {
	case "", "INTERSECTS":
		return intersects
	case "CONTAINS":
		// query range contains stored range
		if (!hasLower && sHasLower) || (!hasUpper && sHasUpper) {
			return false
		}
		if hasLower && sHasLower && compareAny(qLower, sLower) > 0 {
			return false
		}
		if hasUpper && sHasUpper && compareAny(qUpper, sUpper) < 0 {
			return false
		}
		return true
	case "WITHIN":
		// query range within stored range
		if (hasLower && !sHasLower) || (hasUpper && !sHasUpper) {
			return false
		}
		if hasLower && sHasLower && compareAny(sLower, qLower) > 0 {
			return false
		}
		if hasUpper && sHasUpper && compareAny(sUpper, qUpper) < 0 {
			return false
		}
		return true
	}
	return intersects
}

// ---------------------------------------------------------------------------
// term-level queries
// ---------------------------------------------------------------------------

func (c *queryCtx) evalTerm(field string, raw any, boost float64) (bool, float64, *apiError) {
	val := raw
	fuzz := 0
	if m, ok := raw.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if b, ok := toFloat(m["boost"]); ok {
			boost *= b
		}
		if f, ok := m["fuzziness"]; ok {
			if n, ok2 := fuzzinessValue(f, previewValue(val)); ok2 {
				fuzz = n
			}
		}
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	switch ftype {
	case "":
		term, _ := valueScalarString(val)
		for _, v := range vals {
			if v == nil {
				continue
			}
			vs, _ := valueScalarString(v)
			if vs == term {
				return true, boost, nil
			}
		}
		return false, 0, nil
	case "text", "match_only_text", "search_as_you_type":
		term, ok := valueScalarString(val)
		if !ok {
			return false, 0, errParsing("failed to parse term value")
		}
		for _, v := range vals {
			s, _ := valueScalarString(v)
			for _, tok := range analyze(s) {
				if tok == term {
					return true, boost, nil
				}
			}
		}
		return false, 0, nil
	case "keyword", "constant_keyword", "wildcard", "flat_object", "binary":
		term, ok := valueScalarString(val)
		if !ok {
			return false, 0, nil
		}
		if stored, isObj := valsAsObject(vals); isObj {
			_ = stored
		}
		for _, v := range vals {
			s, ok := valueScalarString(v)
			if !ok {
				continue
			}
			if s == term {
				return true, boost, nil
			}
		}
		return false, 0, nil
	case "completion":
		return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
			Reason: "failed to create query: Field [completion] of type [completion] is not supported for term queries",
			RootCause: &apiError{Type: "illegal_argument_exception",
				Reason: "Field [completion] of type [completion] is not supported for term queries"}}
	case "token_count", "rank_feature", "rank_features":
		parsed, err := parseTermValue(ftype, def, val)
		if err != nil {
			return false, 0, err
		}
		for _, v := range vals {
			if numericEqual(v, parsed[0]) {
				return true, boost, nil
			}
		}
		return false, 0, nil
	case "geo_point", "geo_shape":
		return false, 0, errParsing("failed to parse term value")
	case "join":
		term, _ := valueScalarString(val)
		for _, v := range vals {
			switch t := v.(type) {
			case string:
				if t == term {
					return true, boost, nil
				}
			case map[string]any:
				if n, _ := t["name"].(string); n == term {
					return true, boost, nil
				}
			}
		}
		return false, 0, nil
	case "ip_range":
		if s, ok := valueScalarString(val); ok && strings.Contains(s, "/") {
			return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: fmt.Sprintf("failed to create query: '%s' is not an IP string literal.", s),
				RootCause: &apiError{Type: "illegal_argument_exception",
					Reason: fmt.Sprintf("'%s' is not an IP string literal.", s)}}
		}
		fallthrough
	case "integer_range", "float_range", "long_range", "double_range", "date_range":
		parsed, err := parseTermValue(ftype, def, val)
		if err != nil {
			return false, 0, err
		}
		for _, v := range vals {
			if rng, ok := v.(map[string]any); ok {
				if inRangeField(rng, parsed[0]) {
					return true, boost, nil
				}
			}
		}
		return false, 0, nil
	case "ip":
		term, ok := valueScalarString(val)
		if !ok {
			return false, 0, errParsing("failed to parse ip")
		}
		if strings.Contains(term, "/") {
			if _, _, err := net.ParseCIDR(term); err != nil {
				return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
					Reason: fmt.Sprintf("failed to create query: '%s' is not an IP string literal.", term),
					RootCause: &apiError{Type: "illegal_argument_exception",
						Reason: fmt.Sprintf("'%s' is not an IP string literal.", term)}}
			}
		} else if net.ParseIP(term) == nil {
			return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: fmt.Sprintf("failed to create query: '%s' is not an IP string literal.", term),
				RootCause: &apiError{Type: "illegal_argument_exception",
					Reason: fmt.Sprintf("'%s' is not an IP string literal.", term)}}
		}
		for _, v := range vals {
			s, _ := toString(v)
			if ipContains(s, term) {
				return true, boost, nil
			}
		}
		return false, 0, nil
	case "boolean":
		parsed, err := parseTermValue(ftype, def, val)
		if err != nil {
			return false, 0, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("failed to parse value [%s] as a boolean", previewValue(val))}
		}
		for _, v := range vals {
			if b, ok := v.(bool); ok && b == parsed[0].(bool) {
				return true, boost, nil
			}
		}
		return false, 0, nil
	default:
		parsed, err := parseTermValue(ftype, def, val)
		if err != nil {
			return false, 0, err
		}
		for _, v := range vals {
			for _, p := range parsed {
				if numericEqual(v, p) {
					return true, boost, nil
				}
				if fuzz > 0 {
					vs, _ := valueScalarString(v)
					ps, _ := valueScalarString(p)
					if levenshtein(vs, ps) <= fuzz {
						return true, boost, nil
					}
				}
			}
		}
		return false, 0, nil
	}
}

func valsAsObject(vals []any) (map[string]any, bool) {
	return nil, false
}

func fuzzinessValue(v any, term string) (int, bool) {
	switch t := v.(type) {
	case string:
		if strings.HasPrefix(t, "AUTO") {
			if t == "AUTO" {
				return autoFuzziness(term), true
			}
			if strings.HasPrefix(t, "AUTO:") {
				parts := strings.Split(strings.TrimPrefix(t, "AUTO:"), ",")
				low, _ := strconv.Atoi(strings.TrimSpace(parts[0]))
				high := 0
				if len(parts) > 1 {
					high, _ = strconv.Atoi(strings.TrimSpace(parts[1]))
				}
				n := len([]rune(term))
				switch {
				case n < low:
					return 0, true
				case n >= high:
					return 2, true
				default:
					return 1, true
				}
			}
		}
	}
	if n, ok := toFloat(v); ok {
		return int(n), true
	}
	return 0, false
}

func (c *queryCtx) evalTerms(field string, raw map[string]any, boost float64) (bool, float64, *apiError) {
	listRaw, hasList := raw[field]
	if !hasList {
		return false, 0, errParsing("[" + field + "] query requires a field term to contain a value")
	}
	if b, ok := toFloat(raw["boost"]); ok {
		boost *= b
	}
	list, ok := listRaw.([]any)
	if !ok {
		return false, 0, errParsing("[terms] query does not support [" + previewValue(listRaw) + "] as field value, must be an array")
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, item := range list {
		var ok2 bool
		if _, isObj := item.(map[string]any); isObj && ftype != "range" {
			return false, 0, errParsing("[terms] query does not support map values")
		}
		switch ftype {
		case "text", "match_only_text", "search_as_you_type":
			term, _ := valueScalarString(item)
			for _, v := range vals {
				s, _ := valueScalarString(v)
				for _, tok := range analyze(s) {
					if tok == term {
						ok2 = true
						break
					}
				}
				if ok2 {
					break
				}
			}
		case "keyword", "constant_keyword", "wildcard", "flat_object", "binary", "":
			term, _ := valueScalarString(item)
			for _, v := range vals {
				if s, _ := valueScalarString(v); s == term {
					ok2 = true
					break
				}
			}
		default:
			parsed, err := parseTermValue(ftype, def, item)
			if err != nil {
				return false, 0, err
			}
			for _, v := range vals {
				rng, isRange := v.(map[string]any)
				if isRange {
					if inRangeField(rng, parsed[0]) {
						ok2 = true
						break
					}
					continue
				}
				if s, isStr := v.(string); isStr && ftype == "ip" {
					ps, _ := valueScalarString(parsed[0])
					if ipContains(s, ps) {
						ok2 = true
						break
					}
					continue
				}
				if numericEqual(v, parsed[0]) {
					ok2 = true
					break
				}
			}
		}
		if ok2 {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalIds(raw map[string]any, boost float64) (bool, float64, *apiError) {
	if b, ok := toFloat(raw["boost"]); ok {
		boost *= b
	}
	rawVals, ok := raw["values"]
	if !ok {
		return false, 0, errParsing("[ids] requires a 'values' element")
	}
	var list []any
	switch t := rawVals.(type) {
	case []any:
		list = t
	case string:
		list = []any{t}
	case json.Number:
		list = []any{t.String()}
	default:
		return false, 0, errParsing("[ids] 'values' must be an array")
	}
	for _, item := range list {
		s, _ := valueScalarString(item)
		if s == c.id {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

// ---------------------------------------------------------------------------
// range query
// ---------------------------------------------------------------------------

func (c *queryCtx) evalRange(field string, cond map[string]any, boost float64) (bool, float64, *apiError) {
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	relation := ""
	timeZone := ""
	if r, ok := cond["relation"].(string); ok {
		relation = r
	}
	if tz, ok := cond["time_zone"].(string); ok {
		timeZone = tz
	}
	if b, ok := toFloat(cond["boost"]); ok {
		boost *= b
	}
	parseBound := func(v any) (any, *apiError) {
		switch ftype {
		case "date", "date_nanos", "date_range":
			return parseQueryDate(def, v, timeZone)
		case "ip", "ip_range":
			ctx := &indexCtx{indexCoerce: true}
			if _, isRange := v.(map[string]any); isRange {
				return v, nil
			}
			return ctx.parseIPValue(field, v)
		case "integer", "long", "short", "byte", "unsigned_long", "integer_range", "long_range",
			"float", "double", "half_float", "scaled_float", "float_range", "double_range", "token_count":
			base := strings.TrimSuffix(ftype, "_range")
			if isIntegerKind(base) {
				ctx := &indexCtx{indexCoerce: true}
				return ctx.parseIntValue(field, base, def, v)
			}
			ctx := &indexCtx{indexCoerce: true}
			return ctx.parseFloatValue(field, base, def, v)
		default:
			return v, nil
		}
	}
	if _, isRangeField := map[string]bool{
		"integer_range": true, "long_range": true, "float_range": true, "double_range": true,
		"date_range": true, "ip_range": true,
	}[ftype]; isRangeField {
		lower, hasLower := cond["gte"]
		if !hasLower {
			lower, hasLower = cond["gt"]
		}
		upper, hasUpper := cond["lte"]
		if !hasUpper {
			upper, hasUpper = cond["lt"]
		}
		var qLower, qUpper any
		if hasLower {
			v, err := parseBound(lower)
			if err != nil {
				return false, 0, err
			}
			qLower = v
		}
		if hasUpper {
			v, err := parseBound(upper)
			if err != nil {
				return false, 0, err
			}
			qUpper = v
		}
		for _, v := range vals {
			rng, ok := v.(map[string]any)
			if !ok {
				continue
			}
			if rangeRelation(relation, qLower, qUpper, hasLower, hasUpper, rng) {
				return true, boost, nil
			}
		}
		return false, 0, nil
	}
	for _, v := range vals {
		ok := true
		for op, bound := range cond {
			switch op {
			case "boost", "relation", "time_zone", "format":
				continue
			}
			bv, err := parseBound(bound)
			if err != nil {
				return false, 0, err
			}
			var cmp int
			if ftype == "ip" {
				vs, _ := toString(v)
				bs, _ := toString(bv)
				cmp = compareIPLiteral(vs, bs)
			} else if ftype == "text" || ftype == "keyword" || ftype == "match_only_text" || ftype == "wildcard" || ftype == "" {
				vs, _ := toString(v)
				bs, _ := toString(bv)
				cmp = strings.Compare(vs, bs)
			} else {
				if rng, isRange := v.(map[string]any); isRange {
					if !inRangeField(rng, bv) {
						ok = false
					}
					continue
				}
				cmp = numericLess(v, bv)
			}
			switch op {
			case "gt":
				if !(cmp > 0) {
					ok = false
				}
			case "gte":
				if !(cmp >= 0) {
					ok = false
				}
			case "lt":
				if !(cmp < 0) {
					ok = false
				}
			case "lte":
				if !(cmp <= 0) {
					ok = false
				}
			}
			if !ok {
				break
			}
		}
		if ok {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

func compareIPLiteral(a, b string) int {
	if strings.Contains(b, "/") {
		if ipContains(a, b) {
			return 0
		}
		return 1
	}
	ai := net.ParseIP(a)
	bi := net.ParseIP(b)
	if ai == nil || bi == nil {
		return strings.Compare(a, b)
	}
	ab := ai.To16()
	bb := bi.To16()
	return strings.Compare(string(ab), string(bb))
}

func parseQueryDate(def map[string]any, v any, tz string) (any, *apiError) {
	ctx := &indexCtx{indexCoerce: true}
	typ := "date"
	if t, _ := def["type"].(string); t == "date_nanos" {
		typ = "date_nanos"
	}
	scalar := v
	if m, ok := v.(map[string]any); ok {
		if mv, ok := m["value"]; ok {
			scalar = mv
		}
	}
	if s, ok := scalar.(string); ok {
		dateMath := s
		if i := strings.Index(s, "||"); i >= 0 {
			s = s[:i]
		}
		if abs, ok := parseDateAny(def, s); ok {
			if tz != "" {
				if offset, err := parseTZOffset(tz); err == nil {
					abs -= offset
				}
			}
			return abs, nil
		}
		_ = dateMath
	}
	parsed, err := ctx.parseDateValue("_value", typ, def, scalar)
	if err != nil {
		return nil, &apiError{Type: "search_phase_execution_exception", Status: 400,
			Reason: "failed to parse date field [" + previewValue(scalar) + "]", RootCause: err}
	}
	return parsed, nil
}

func parseTZOffset(tz string) (int64, error) {
	tz = strings.TrimSpace(tz)
	if tz == "UTC" || tz == "utc" || tz == "Z" || tz == "" {
		return 0, nil
	}
	if strings.HasPrefix(tz, "+") || strings.HasPrefix(tz, "-") {
		sign := int64(1)
		if tz[0] == '-' {
			sign = -1
		}
		body := tz[1:]
		parts := strings.Split(body, ":")
		hours, err := strconv.ParseInt(parts[0], 10, 64)
		if err != nil {
			return 0, err
		}
		var mins int64
		if len(parts) > 1 {
			mins, _ = strconv.ParseInt(parts[1], 10, 64)
		}
		return sign * (hours*3600 + mins*60) * 1000, nil
	}
	loc, err := timeLoadLocation(tz)
	if err != nil {
		return 0, err
	}
	_, off := timeNowIn(loc)
	return int64(off) * 1000, nil
}

// ---------------------------------------------------------------------------
// exists / prefix / wildcard / regexp / fuzzy
// ---------------------------------------------------------------------------

func (c *queryCtx) evalExists(raw map[string]any) (bool, float64, *apiError) {
	field, _ := toString(raw["field"])
	if field == "" {
		return false, 0, errParsing("[exists] query requires 'field'")
	}
	boost := 1.0
	if b, ok := toFloat(raw["boost"]); ok {
		boost = b
	}
	if strings.ContainsAny(field, "*?") {
		re := wildcardRegex(field)
		for path, vals := range c.idx.DocValues[c.id] {
			if re.MatchString(path) && len(vals) > 0 {
				return true, boost, nil
			}
		}
		for path := range c.idx.objectPaths() {
			if re.MatchString(path) && c.hasField(path) {
				return true, boost, nil
			}
		}
		return false, 0, nil
	}
	if c.hasField(field) {
		return true, boost, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalPrefix(field string, raw any, boost float64) (bool, float64, *apiError) {
	val := raw
	if m, ok := raw.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if b, ok := toFloat(m["boost"]); ok {
			boost *= b
		}
	}
	prefix, ok := valueScalarString(val)
	if !ok {
		return false, 0, nil
	}
	_ = prefix
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		s, _ := valueScalarString(v)
		if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
			for _, tok := range analyze(s) {
				if strings.HasPrefix(tok, prefix) {
					return true, boost, nil
				}
			}
		} else if strings.HasPrefix(s, prefix) {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalWildcard(field string, raw any, boost float64) (bool, float64, *apiError) {
	val := raw
	if m, ok := raw.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if b, ok := toFloat(m["boost"]); ok {
			boost *= b
		}
	}
	pattern, ok := valueScalarString(val)
	if !ok {
		return false, 0, nil
	}
	pattern = strings.ToLower(pattern)
	re := wildcardRegex(pattern)
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		s, _ := valueScalarString(v)
		candidates := []string{s}
		if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
			candidates = analyze(s)
		}
		for _, cand := range candidates {
			if re.MatchString(strings.ToLower(cand)) {
				return true, boost, nil
			}
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalRegexp(field string, raw any, boost float64) (bool, float64, *apiError) {
	val := raw
	if m, ok := raw.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if b, ok := toFloat(m["boost"]); ok {
			boost *= b
		}
		if states, ok := toFloat(m["max_determinized_states"]); ok && states <= 10 {
			return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: "failed to create query: [regexp] parsing failed. would require more than 10 effort",
				RootCause: &apiError{Type: "query_shard_exception",
					Reason: "failed to create query: [regexp] parsing failed. would require more than 10 effort"}}
		}
	}
	pattern, ok := valueScalarString(val)
	if !ok {
		return false, 0, nil
	}
	whole := "^(?:" + pattern + ")$"
	re, err := regexp.Compile(whole)
	if err != nil {
		msg := "unexpected end-of-string"
		return false, 0, &apiError{Type: "search_phase_execution_exception", Status: 400,
			Reason:    "failed to create query: " + msg,
			RootCause: &apiError{Type: "query_shard_exception", Reason: msg}}
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		s, _ := valueScalarString(v)
		candidates := []string{s}
		if ftype == "text" || ftype == "match_only_text" {
			candidates = analyze(s)
		}
		for _, cand := range candidates {
			if re.MatchString(cand) {
				return true, boost, nil
			}
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalFuzzy(field string, raw any, boost float64) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("[fuzzy] query requires an object")
	}
	val, has := m["value"]
	if !has {
		return false, 0, errParsing("[fuzzy] query requires 'value'")
	}
	if b, ok := toFloat(m["boost"]); ok {
		boost *= b
	}
	term, _ := valueScalarString(val)
	fuzz := autoFuzziness(term)
	if f, ok := m["fuzziness"]; ok {
		if n, ok2 := fuzzinessValue(f, term); ok2 {
			fuzz = n
		}
	}
	prefixLength := 0
	if pl, ok := toFloat(m["prefix_length"]); ok {
		prefixLength = int(pl)
	}
	transpositions := true
	if b, ok := m["transpositions"].(bool); ok {
		transpositions = b
	}
	maxExpansions := 0
	if me, ok := toFloat(m["max_expansions"]); ok {
		maxExpansions = int(me)
	}
	_ = transpositions
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	matched := 0
	for _, v := range vals {
		s, _ := valueScalarString(v)
		candidates := []string{s}
		if ftype == "text" || ftype == "match_only_text" {
			candidates = analyze(s)
		}
		for _, cand := range candidates {
			if cand == term {
				return true, boost * 2, nil
			}
			if len(cand) <= prefixLength || len(term) <= prefixLength {
				continue
			}
			if cand[:prefixLength] != term[:prefixLength] {
				continue
			}
			if levenshtein(cand, term) <= fuzz {
				if maxExpansions > 0 && matched >= maxExpansions {
					continue
				}
				matched++
				return true, boost, nil
			}
		}
	}
	return false, 0, nil
}

// ---------------------------------------------------------------------------
// full text
// ---------------------------------------------------------------------------

func (c *queryCtx) fieldTokens(field string) ([]string, bool) {
	vals, present := c.values(field)
	if !present {
		return nil, false
	}
	out := []string{}
	for _, v := range vals {
		s, ok := valueScalarString(v)
		if !ok {
			continue
		}
		out = append(out, analyze(s)...)
	}
	return out, true
}

func (c *queryCtx) fieldStrings(field string) ([]string, bool) {
	vals, present := c.values(field)
	if !present {
		return nil, false
	}
	out := []string{}
	for _, v := range vals {
		s, ok := valueScalarString(v)
		if !ok {
			continue
		}
		out = append(out, s)
	}
	return out, true
}

func (c *queryCtx) analyzerName(field string, opts any) string {
	if m, ok := opts.(map[string]any); ok {
		if a, ok := m["analyzer"].(string); ok && a != "" {
			return a
		}
	}
	if c.idx != nil {
		if def, ok := mappingFieldDef(c.idx, field); ok {
			if a, ok := def["search_analyzer"].(string); ok && a != "" {
				return a
			}
			if a, ok := def["analyzer"].(string); ok && a != "" {
				return a
			}
		}
	}
	return ""
}

func (c *queryCtx) analyzeField(field string, opts any, text string) []string {
	a := c.analyzerName(field, opts)
	if a == "" {
		return analyze(text)
	}
	return analyzeWithIndex(c.idx, a, text)
}

func (c *queryCtx) evalMatch(field string, raw any, boost float64) (bool, float64, *apiError) {
	text, opts := queryText(raw)
	if m, ok := opts.(map[string]any); ok {
		if b, ok := toFloat(m["boost"]); ok {
			boost *= b
		}
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	operator := "or"
	if m, ok := opts.(map[string]any); ok {
		if o, ok := m["operator"].(string); ok {
			operator = strings.ToLower(o)
		}
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	if isNumericType(ftype) || ftype == "boolean" || ftype == "date" || ftype == "date_nanos" || ftype == "ip" {
		parsed, err := parseTermValue(ftype, def, text)
		if err != nil {
			return false, 0, nil
		}
		for _, v := range vals {
			for _, p := range parsed {
				if ftype == "ip" {
					s, _ := toString(v)
					ps, _ := toString(p)
					if ipContains(s, ps) {
						return true, boost, nil
					}
					continue
				}
				if numericEqual(v, p) {
					return true, boost, nil
				}
			}
		}
		return false, 0, nil
	}
	if ftype == "keyword" || ftype == "constant_keyword" || ftype == "wildcard" || ftype == "flat_object" {
		for _, v := range vals {
			s, _ := valueScalarString(v)
			if s == text {
				return true, boost, nil
			}
		}
		return false, 0, nil
	}
	qtokens := c.analyzeField(field, opts, text)
	if len(qtokens) == 0 {
		return false, 0, nil
	}
	score := 0.0
	matchedAll := true
	for _, qt := range qtokens {
		found := false
		for _, v := range vals {
			s, _ := valueScalarString(v)
			for _, tok := range c.analyzeField(field, nil, s) {
				if tok == qt {
					found = true
					break
				}
			}
			if found {
				break
			}
		}
		if found {
			score++
		} else {
			matchedAll = false
		}
	}
	if operator == "and" {
		if matchedAll {
			return true, boost * float64(len(qtokens)), nil
		}
		return false, 0, nil
	}
	if score > 0 {
		return true, boost * score, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalMatchPhrase(field string, raw any, boost, slop float64) (bool, float64, *apiError) {
	text, opts := queryText(raw)
	_ = opts
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	if ftype == "boolean" || isNumericType(ftype) || ftype == "date" || ftype == "date_nanos" || ftype == "ip" {
		return c.evalMatch(field, raw, boost)
	}
	if ftype == "keyword" || ftype == "constant_keyword" || ftype == "wildcard" || ftype == "flat_object" {
		for _, v := range c.mustValues(field) {
			s, _ := valueScalarString(v)
			if s == text {
				return true, boost, nil
			}
		}
		return false, 0, nil
	}
	qtokens := c.analyzeField(field, opts, text)
	if len(qtokens) == 0 {
		return false, 0, nil
	}
	for _, v := range c.mustValues(field) {
		s, _ := valueScalarString(v)
		dtokens := c.analyzeField(field, nil, s)
		if containsSubsequence(dtokens, qtokens) {
			return true, boost * float64(len(qtokens)), nil
		}
	}
	return false, 0, nil
}

func (c *queryCtx) mustValues(field string) []any {
	vals, _ := c.values(field)
	return vals
}

func (c *queryCtx) evalMatchBoolPrefix(field string, raw any, boost float64) (bool, float64, *apiError) {
	text, opts := queryText(raw)
	operator := "or"
	if m, ok := opts.(map[string]any); ok {
		if o, ok := m["operator"].(string); ok {
			operator = strings.ToLower(o)
		}
	}
	qtokens := c.analyzeField(field, opts, text)
	if len(qtokens) == 0 {
		return false, 0, nil
	}
	if operator == "and" {
		for i, tok := range qtokens {
			var ok bool
			var err *apiError
			if i == len(qtokens)-1 {
				ok, _, err = c.evalPrefix(field, tok, boost)
			} else {
				ok, _, err = c.evalTerm(field, tok, boost)
			}
			if err != nil {
				return false, 0, err
			}
			if !ok {
				return false, 0, nil
			}
		}
		return true, boost, nil
	}
	for i, tok := range qtokens {
		var ok bool
		if i == len(qtokens)-1 {
			ok, _, _ = c.evalPrefix(field, tok, boost)
		} else {
			ok, _, _ = c.evalTerm(field, tok, boost)
		}
		if ok {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

// ---------------------------------------------------------------------------
// bool / compound
// ---------------------------------------------------------------------------

func minimumShouldMatch(spec any, clauseCount, matched int) int {
	required := 1
	switch t := spec.(type) {
	case json.Number:
		if n, err := t.Int64(); err == nil {
			required = int(n)
		}
	case float64:
		required = int(t)
	case string:
		s := strings.TrimSpace(t)
		if strings.HasSuffix(s, "%") {
			neg := strings.HasPrefix(s, "-")
			s = strings.TrimPrefix(s, "-")
			pct, _ := strconv.ParseFloat(strings.TrimSuffix(s, "%"), 64)
			v := int(pct / 100 * float64(clauseCount))
			if neg {
				required = clauseCount - v
			} else {
				required = v
			}
		} else {
			n, err := strconv.Atoi(s)
			if err == nil {
				if n < 0 {
					required = clauseCount + n
				} else {
					required = n
				}
			}
		}
	}
	if required < 0 {
		required = 0
	}
	if required > clauseCount {
		required = clauseCount
	}
	return required
}

func (c *queryCtx) evalBool(raw map[string]any, boost float64) (bool, float64, *apiError) {
	for k := range raw {
		switch k {
		case "must", "should", "filter", "must_not", "minimum_should_match", "boost",
			"adjust_pure_negative", "_name":
		default:
			return false, 0, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[bool] unknown field [" + k + "] did you mean any of [must, mustNot]?"}
		}
	}
	if b, ok := toFloat(raw["boost"]); ok {
		boost *= b
	}
	must := asQueryList(raw["must"])
	filter := asQueryList(raw["filter"])
	should := asQueryList(raw["should"])
	mustNot := asQueryList(raw["must_not"])
	score := 0.0
	for _, list := range [][]any{must, filter} {
		for _, q := range list {
			qm, ok := q.(map[string]any)
			if !ok {
				return false, 0, errParsing("malformed bool clause")
			}
			matched, sc, err := c.eval(qm)
			if err != nil {
				return false, 0, err
			}
			if !matched {
				return false, 0, nil
			}
			if len(must) > 0 {
				_ = sc
			}
			score += sc
		}
	}
	// filter clauses contribute zero
	if len(filter) > 0 {
		for _, q := range filter {
			qm, _ := q.(map[string]any)
			_, _, _ = c.eval(qm)
		}
		var fscore float64
		for _, q := range filter {
			qm, _ := q.(map[string]any)
			_, sc, _ := c.eval(qm)
			fscore += sc
		}
		score -= fscore
	}
	shouldMatched := 0
	for _, q := range should {
		qm, ok := q.(map[string]any)
		if !ok {
			continue
		}
		matched, sc, err := c.eval(qm)
		if err != nil {
			return false, 0, err
		}
		if matched {
			shouldMatched++
			score += sc
		}
	}
	msmSpec, hasMSM := raw["minimum_should_match"]
	hasPositives := len(must) > 0 || len(filter) > 0
	msm := 0
	if hasMSM {
		msm = minimumShouldMatch(msmSpec, len(should), shouldMatched)
	} else if !hasPositives {
		msm = 1
	}
	if len(should) > 0 && shouldMatched < msm {
		return false, 0, nil
	}
	adjust := true
	if v, ok := raw["adjust_pure_negative"].(bool); ok {
		adjust = v
	}
	if len(must) == 0 && len(filter) == 0 && len(should) == 0 && len(mustNot) > 0 {
		if !adjust {
			return false, 0, nil
		}
	}
	for _, q := range mustNot {
		qm, ok := q.(map[string]any)
		if !ok {
			continue
		}
		matched, _, err := c.eval(qm)
		if err != nil {
			return false, 0, err
		}
		if matched {
			return false, 0, nil
		}
	}
	if len(must) == 0 && len(filter) == 0 && len(should) == 0 {
		return true, boost * 1.0, nil
	}
	return true, boost * score, nil
}

func (c *queryCtx) evalConstantScore(raw map[string]any, boost float64) (bool, float64, *apiError) {
	filter, ok := raw["filter"].(map[string]any)
	if !ok {
		return false, 0, errParsing("[constant_score] requires a 'filter' element")
	}
	if b, ok := toFloat(raw["boost"]); ok {
		boost *= b
	}
	matched, _, err := c.eval(filter)
	if err != nil {
		return false, 0, err
	}
	if matched {
		return true, boost, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalDisMax(raw map[string]any, boost float64) (bool, float64, *apiError) {
	if b, ok := toFloat(raw["boost"]); ok {
		boost *= b
	}
	tie := 0.0
	if t, ok := toFloat(raw["tie_breaker"]); ok {
		tie = t
	}
	queries := asQueryList(raw["queries"])
	best := 0.0
	sum := 0.0
	found := false
	for _, q := range queries {
		qm, ok := q.(map[string]any)
		if !ok {
			continue
		}
		matched, sc, err := c.eval(qm)
		if err != nil {
			return false, 0, err
		}
		if matched {
			found = true
			if sc > best {
				best = sc
			}
			sum += sc
		}
	}
	if !found {
		return false, 0, nil
	}
	return true, boost * (best + tie*(sum-best)), nil
}

func (c *queryCtx) eval(raw map[string]any) (bool, float64, *apiError) {
	if len(raw) == 0 {
		return true, 1, nil
	}
	for clause, body := range raw {
		switch clause {
		case "match_all":
			boost := 1.0
			if m, ok := body.(map[string]any); ok {
				if b, ok := toFloat(m["boost"]); ok {
					boost = b
				}
			}
			return true, boost, nil
		case "match_none":
			return false, 0, nil
		case "term":
			field, val, ok := singleEntry(asMap(body))
			if !ok {
				return false, 0, errParsing("[term] query malformed")
			}
			return c.evalTerm(field, val, 1.0)
		case "terms":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[terms] query malformed")
			}
			field := ""
			for k := range bm {
				if k != "boost" && k != "_name" {
					field = k
					break
				}
			}
			if field == "" {
				return false, 0, errParsing("[terms] query malformed")
			}
			return c.evalTerms(field, bm, 1.0)
		case "ids":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[ids] query malformed")
			}
			return c.evalIds(bm, 1.0)
		case "range":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[range] query malformed")
			}
			field, condRaw, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[range] query malformed")
			}
			cond, ok := condRaw.(map[string]any)
			if !ok {
				return false, 0, errParsing("[range] query malformed")
			}
			return c.evalRange(field, cond, 1.0)
		case "exists":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[exists] query malformed")
			}
			return c.evalExists(bm)
		case "prefix":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[prefix] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[prefix] query malformed")
			}
			return c.evalPrefix(field, val, 1.0)
		case "wildcard":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[wildcard] query malformed")
			}
			if _, isStr := bm["value"]; !isStr {
				if len(bm) == 1 {
					field, val, _ := singleEntry(bm)
					if _, isMap := val.(map[string]any); !isMap {
						return c.evalWildcard(field, map[string]any{"value": val}, 1.0)
					}
				}
				return false, 0, errParsing("bad wildcard body")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[wildcard] query malformed")
			}
			_ = field
			_ = val
			return false, 0, errParsing("bad wildcard body")
		case "regexp":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[regexp] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[regexp] query malformed")
			}
			return c.evalRegexp(field, val, 1.0)
		case "fuzzy":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[fuzzy] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[fuzzy] query malformed")
			}
			return c.evalFuzzy(field, val, 1.0)
		case "match":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[match] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[match] query malformed")
			}
			return c.evalMatch(field, val, 1.0)
		case "match_phrase":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[match_phrase] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[match_phrase] query malformed")
			}
			return c.evalMatchPhrase(field, val, 1.0, 0)
		case "match_phrase_prefix":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[match_phrase_prefix] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[match_phrase_prefix] query malformed")
			}
			text, _ := queryText(val)
			qtokens := analyze(text)
			if len(qtokens) == 0 {
				return false, 0, nil
			}
			for _, v := range c.mustValues(field) {
				s, _ := valueScalarString(v)
				dtokens := analyze(s)
				if len(dtokens) < len(qtokens) {
					continue
				}
				for i := 0; i+len(qtokens) <= len(dtokens); i++ {
					ok := true
					for j := range qtokens {
						if j == len(qtokens)-1 {
							if !strings.HasPrefix(dtokens[i+j], qtokens[j]) {
								ok = false
							}
						} else if dtokens[i+j] != qtokens[j] {
							ok = false
						}
					}
					if ok {
						return true, float64(len(qtokens)), nil
					}
				}
			}
			return false, 0, nil
		case "match_bool_prefix":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[match_bool_prefix] query malformed")
			}
			field, val, ok := singleEntry(bm)
			if !ok {
				return false, 0, errParsing("[match_bool_prefix] query malformed")
			}
			return c.evalMatchBoolPrefix(field, val, 1.0)
		case "multi_match":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[multi_match] query malformed")
			}
			return c.evalMultiMatch(bm)
		case "bool":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[bool] query malformed")
			}
			return c.evalBool(bm, 1.0)
		case "constant_score":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[constant_score] query malformed")
			}
			return c.evalConstantScore(bm, 1.0)
		case "dis_max":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[dis_max] query malformed")
			}
			return c.evalDisMax(bm, 1.0)
		case "boosting":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[boosting] query malformed")
			}
			positive, ok := bm["positive"].(map[string]any)
			if !ok {
				return false, 0, errParsing("[boosting] requires 'positive'")
			}
			matched, sc, err := c.eval(positive)
			if err != nil {
				return false, 0, err
			}
			if !matched {
				return false, 0, nil
			}
			if negRaw, ok := bm["negative"].(map[string]any); ok {
				neg, _, err := c.eval(negRaw)
				if err != nil {
					return false, 0, err
				}
				if neg {
					nf := 0.5
					if v, ok := toFloat(bm["negative_boost"]); ok {
						nf = v
					}
					sc *= nf
				}
			}
			return true, sc, nil
		case "nested":
			return c.evalNested(body)
		case "has_child":
			return c.evalHasChild(body)
		case "has_parent":
			return c.evalHasParent(body)
		case "geo_distance":
			return c.evalGeoDistance(body)
		case "geo_bounding_box":
			return c.evalGeoBoundingBox(body)
		case "geo_shape":
			return c.evalGeoShape(body)
		case "script":
			return c.evalScriptQuery(body)
		case "script_score":
			return c.evalScriptScore(body)
		case "function_score":
			return c.evalFunctionScore(body)
		case "query_string":
			return c.evalQueryString(body, false)
		case "simple_query_string":
			return c.evalQueryString(body, true)
		case "combined_fields":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[combined_fields] query malformed")
			}
			return c.evalCombinedFields(bm)
		case "more_like_this":
			bm, ok := body.(map[string]any)
			if !ok {
				return false, 0, errParsing("[more_like_this] query malformed")
			}
			return c.evalMoreLikeThis(bm)
		case "rank_feature":
			return c.evalRankFeature(body)
		case "distance_feature":
			return c.evalDistanceFeature(body)
		case "wrapper":
			return c.evalWrapper(body)
		case "pinned":
			return c.evalPinned(body)
		case "percolate":
			return c.evalPercolate(body)
		case "terms_set":
			return c.evalTermsSet(body)
		case "span_term":
			return c.evalSpanTerm(body)
		case "span_near":
			return c.evalSpanNear(body)
		case "span_or":
			return c.evalSpanOr(body)
		case "span_first":
			return c.evalSpanFirst(body)
		case "span_not":
			return c.evalSpanNot(body)
		case "span_containing":
			return c.evalSpanContaining(body)
		case "span_within":
			return c.evalSpanWithin(body)
		case "span_multi":
			return c.evalSpanMulti(body)
		case "intervals":
			return c.evalIntervals(body)
		case "random_score":
			return true, 1.0, nil
		case "knn":
			return c.evalKnn(body)
		default:
			return false, 0, errParsing("unknown query [" + clause + "]")
		}
	}
	return false, 0, errParsing("empty query")
}

func asMap(v any) map[string]any {
	m, _ := v.(map[string]any)
	return m
}
