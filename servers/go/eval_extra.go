package main

import (
	"encoding/base64"
	"encoding/json"
	"math"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

// ---------------------------------------------------------------------------
// time helpers used by date range queries
// ---------------------------------------------------------------------------

func timeLoadLocation(name string) (*time.Location, error) { return time.LoadLocation(name) }

func timeNowIn(loc *time.Location) (time.Time, int) {
	now := time.Now().In(loc)
	_, off := now.Zone()
	return now, off
}

// ---------------------------------------------------------------------------
// multi_match / combined_fields
// ---------------------------------------------------------------------------

func stripFieldBoost(f string) (string, float64) {
	if i := strings.Index(f, "^"); i >= 0 {
		b, err := strconv.ParseFloat(f[i+1:], 64)
		if err == nil {
			return f[:i], b
		}
		return f[:i], 1
	}
	return f, 1
}

func (c *queryCtx) fieldMatchedCount(field string, terms []string) int {
	vals, ok := c.values(field)
	if !ok {
		return 0
	}
	analyzer := c.analyzerName(field, nil)
	tf := map[string]int{}
	for _, v := range vals {
		s, _ := valueScalarString(v)
		for _, t := range analyzeTokensWithIndex(c.idx, analyzer, s) {
			tf[t.term]++
		}
	}
	n := 0
	for _, t := range terms {
		if tf[t] > 0 {
			n++
		}
	}
	return n
}

func (c *queryCtx) bm25FieldScore(field string, qtokens []string, operator string, boost float64) float64 {
	if c.idx == nil || c.id == "" {
		return -1
	}
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	if ftype != "" && ftype != "text" && ftype != "match_only_text" && ftype != "search_as_you_type" {
		return -1
	}
	analyzer := c.analyzerName(field, nil)
	docTerms := func(vals []any) []string {
		out := []string{}
		for _, v := range vals {
			s, _ := valueScalarString(v)
			for _, t := range analyzeTokensWithIndex(c.idx, analyzer, s) {
				out = append(out, t.term)
			}
		}
		return out
	}
	myVals := []any{}
	if dv, ok := c.rawValues(); ok {
		myVals = dv[resolveAliasField(c.idx, field)]
	}
	myTokens := docTerms(myVals)
	if len(myTokens) == 0 {
		return 0
	}
	tf := map[string]int{}
	for _, t := range myTokens {
		tf[t]++
	}
	for _, qt := range qtokens {
		if tf[qt] == 0 {
			if operator == "and" {
				return 0
			}
		}
	}
	N := 0
	avgdl := 0.0
	df := map[string]int{}
	for _, dv := range c.idx.DocValues {
		vals := dv[resolveAliasField(c.idx, field)]
		if len(vals) == 0 {
			continue
		}
		N++
		toks := docTerms(vals)
		avgdl += float64(len(toks))
		seen := map[string]bool{}
		for _, t := range toks {
			if !seen[t] {
				seen[t] = true
				df[t]++
			}
		}
	}
	if N == 0 {
		return 0
	}
	avgdl /= float64(N)
	if avgdl == 0 {
		avgdl = 1
	}
	const k1 = 1.2
	const b = 0.75
	dl := float64(len(myTokens))
	score := 0.0
	for _, qt := range qtokens {
		f := float64(tf[qt])
		if f == 0 {
			continue
		}
		d := float64(df[qt])
		idf := math.Log(1 + (float64(N)-d+0.5)/(d+0.5))
		score += idf * (f * (k1 + 1)) / (f + k1*(1-b+b*dl/avgdl))
	}
	return score * boost
}

func (c *queryCtx) evalMultiMatch(bm map[string]any) (bool, float64, *apiError) {
	query, _ := toString(bm["query"])
	if query == "" {
		return false, 0, errParsing("[multi_match] requires 'query'")
	}
	typ, _ := bm["type"].(string)
	if typ == "" {
		typ = "best_fields"
	}
	switch typ {
	case "best_fields", "most_fields", "cross_fields", "phrase", "phrase_prefix", "bool_prefix":
	default:
		return false, 0, &apiError{Type: "parse_exception", Status: 400,
			Reason: "failed to parse [multi_match] query type [" + typ + "]. unknown type."}
	}
	var fields []string
	switch t := bm["fields"].(type) {
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				fields = append(fields, s)
			}
		}
	case string:
		fields = []string{t}
	}
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost *= b
	}
	tie := 0.0
	if t, ok := toFloat(bm["tie_breaker"]); ok {
		tie = t
	}
	if len(fields) == 0 {
		fields = []string{"*"}
	}
	expanded := []string{}
	fromWildcard := map[string]bool{}
	for _, f := range fields {
		name, _ := stripFieldBoost(f)
		if strings.ContainsAny(name, "*?") {
			re := wildcardRegex(name)
			for path, vals := range c.valuesMap() {
				if re.MatchString(path) && len(vals) > 0 {
					expanded = append(expanded, path)
					fromWildcard[path] = true
				}
			}
			continue
		}
		expanded = append(expanded, f)
	}
	best := 0.0
	sum := 0.0
	matchedAny := false
	switch typ {
	case "phrase", "phrase_prefix":
		for _, f := range expanded {
			name, fb := stripFieldBoost(f)
			var ok bool
			if typ == "phrase" {
				slop := 0.0
				if v, has := bm["slop"]; has {
					if f, ok := toFloat(v); ok {
						slop = f
					}
				}
				ok, _, _ = c.evalMatchPhrase(name, query, boost*fb, slop)
			} else {
				bm2 := map[string]any{name: map[string]any{"query": query, "boost": boost * fb}}
				q := map[string]any{"match_phrase_prefix": bm2}
				ok, _, _ = c.eval(q)
			}
			if ok {
				matchedAny = true
				sum += fb
			}
		}
	case "bool_prefix":
		for _, f := range expanded {
			name, fb := stripFieldBoost(f)
			ok, sc, err := c.evalMatchBoolPrefix(name, query, boost*fb)
			if err != nil {
				return false, 0, err
			}
			if ok {
				matchedAny = true
				sum += sc
			}
		}
	case "cross_fields":
		if _, has := bm["fuzziness"]; has {
			return false, 0, errParsing("Fuzziness not allowed for type [cross_fields]")
		}
		terms := analyze(query)
		operator, _ := bm["operator"].(string)
		operator = strings.ToLower(operator)
		required := 1
		if operator == "and" {
			required = len(terms)
		}
		if v, has := bm["minimum_should_match"]; has {
			req, merr := msmRequired(v, len(terms), operator == "and")
			if merr != nil {
				return false, 0, merr
			}
			required = req
		}
		matchedTerms := 0
		for _, term := range terms {
			bestTerm := 0.0
			for _, f := range expanded {
				name, fb := stripFieldBoost(f)
				if sc := c.bm25FieldScore(name, []string{term}, "or", boost*fb); sc > bestTerm {
					bestTerm = sc
				}
			}
			if bestTerm > 0 {
				matchedTerms++
				sum += bestTerm
			}
		}
		if matchedTerms >= required {
			matchedAny = true
		}
	default:
		operator, _ := bm["operator"].(string)
		operator = strings.ToLower(operator)
		fuzz, hasFuzz := bm["fuzziness"]
		for _, f := range expanded {
			name, fb := stripFieldBoost(f)
			if fromWildcard[name] {
				if def, ok := mappingFieldDef(c.idx, name); ok {
					ft, _ := def["type"].(string)
					if ft != "" && ft != "text" && ft != "keyword" && ft != "constant_keyword" &&
						ft != "wildcard" && ft != "match_only_text" && ft != "search_as_you_type" && ft != "flat_object" {
						continue
					}
				}
			}
			var sc float64
			ok := false
			usedBM25 := false
			if !hasFuzz {
				terms := c.analyzeField(name, nil, query)
				required := 1
				if operator == "and" {
					required = len(terms)
				}
				if v, has := bm["minimum_should_match"]; has {
					req, merr := msmRequired(v, len(terms), operator == "and")
					if merr != nil {
						return false, 0, merr
					}
					required = req
					if len(terms) == 1 {
						required = 1
					}
				}
				if required > 1 && c.fieldMatchedCount(name, terms) < required {
					continue
				}
				if bm25 := c.bm25FieldScore(name, terms, operator, boost*fb); bm25 >= 0 {
					ok = bm25 > 0
					sc = bm25
					usedBM25 = true
				}
			}
			if !usedBM25 {
				opts := map[string]any{"query": query}
				if operator != "" {
					opts["operator"] = operator
				}
				if hasFuzz {
					opts["fuzziness"] = fuzz
				}
				if v, has := bm["lenient"]; has {
					opts["lenient"] = v
				}
				var err *apiError
				ok, sc, err = c.evalMatch(name, opts, boost*fb)
				if err != nil {
					return false, 0, err
				}
			}
			if ok {
				matchedAny = true
				if sc > best {
					best = sc
				}
				sum += sc
			}
		}
	}
	if !matchedAny {
		return false, 0, nil
	}
	switch typ {
	case "most_fields", "cross_fields", "bool_prefix":
		return true, sum, nil
	default:
		if tie > 0 {
			return true, best + tie*(sum-best), nil
		}
		return true, best, nil
	}
}

func (c *queryCtx) evalCombinedFields(bm map[string]any) (bool, float64, *apiError) {
	query, _ := toString(bm["query"])
	var fields []string
	switch t := bm["fields"].(type) {
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				fields = append(fields, s)
			}
		}
	case string:
		fields = []string{t}
	}
	if len(fields) == 0 {
		return false, 0, nil
	}
	expanded := []string{}
	for _, f := range fields {
		name, fb := stripFieldBoost(f)
		if fb < 1 {
			reason := "weight must be greater or equal to 1"
			return false, 0, matchSearchError("query_shard_exception", reason)
		}
		if strings.ContainsAny(name, "*?") {
			re := wildcardRegex(name)
			for path, vals := range c.valuesMap() {
				if re.MatchString(path) && len(vals) > 0 {
					expanded = append(expanded, f)
					break
				}
			}
			re2 := wildcardRegex(name)
			for path := range c.valuesMap() {
				if re2.MatchString(path) && !containsString(expanded, path) {
					expanded = append(expanded, path)
				}
			}
			continue
		}
		expanded = append(expanded, f)
	}
	seen := map[string]bool{}
	finalFields := []string{}
	for _, f := range expanded {
		name, _ := stripFieldBoost(f)
		if seen[name] {
			continue
		}
		seen[name] = true
		finalFields = append(finalFields, f)
	}
	expanded = finalFields
	for _, f := range expanded {
		name, _ := stripFieldBoost(f)
		def, ok := mappingFieldDef(c.idx, name)
		if !ok {
			continue
		}
		ft, _ := def["type"].(string)
		if ft != "" && ft != "text" && ft != "match_only_text" && ft != "search_as_you_type" {
			return false, 0, matchSearchError("query_shard_exception",
				"Field ["+name+"] of type ["+ft+"] does not support [combined_fields] queries")
		}
	}
	terms := analyze(query)
	if len(terms) == 0 {
		return false, 0, nil
	}
	operator, _ := bm["operator"].(string)
	operator = strings.ToLower(operator)
	required := 1
	if operator == "and" {
		required = len(terms)
	}
	if v, has := bm["minimum_should_match"]; has {
		req, merr := msmRequired(v, len(terms), operator == "and")
		if merr != nil {
			return false, 0, merr
		}
		required = req
	}
	matchedTerms := 0
	total := 0.0
	for _, term := range terms {
		bestTerm := 0.0
		for _, f := range expanded {
			name, fb := stripFieldBoost(f)
			if sc := c.bm25FieldScore(name, []string{term}, "or", fb); sc > bestTerm {
				bestTerm = sc
			}
		}
		if bestTerm > 0 {
			matchedTerms++
			total += bestTerm
		}
	}
	if matchedTerms >= required {
		return true, total, nil
	}
	return false, 0, nil
}

func containsString(list []string, s string) bool {
	for _, e := range list {
		if e == s {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// nested / join queries
// ---------------------------------------------------------------------------

func (c *queryCtx) evalNested(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[nested] query malformed")
	}
	inner, ok := bm["query"].(map[string]any)
	if !ok {
		return false, 0, errParsing("[nested] requires 'query'")
	}
	path, _ := bm["path"].(string)
	scoreMode, _ := bm["score_mode"].(string)
	if scoreMode == "" {
		scoreMode = "avg"
	}
	_ = path
	matched, sc, err := c.eval(inner)
	if err != nil {
		return false, 0, err
	}
	if !matched {
		return false, 0, nil
	}
	if scoreMode == "none" {
		return true, 0, nil
	}
	return true, sc, nil
}

func findJoinField(idx *Index) (string, map[string]any, bool) {
	props, _ := idx.Mappings["properties"].(map[string]any)
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		if t, _ := def["type"].(string); t == "join" {
			relations, _ := def["relations"].(map[string]any)
			return name, relations, true
		}
	}
	return "", nil, false
}

func joinParts(v any) (string, string) {
	switch t := v.(type) {
	case string:
		return t, ""
	case map[string]any:
		name, _ := t["name"].(string)
		parent, _ := t["parent"].(string)
		return name, parent
	}
	return "", ""
}

func (c *queryCtx) evalHasChild(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[has_child] query malformed")
	}
	childType, _ := bm["type"].(string)
	inner, ok := bm["query"].(map[string]any)
	if !ok {
		return false, 0, errParsing("[has_child] requires 'query'")
	}
	field, _, hasJoin := findJoinField(c.idx)
	if !hasJoin {
		return false, 0, errParsing("failed to create query: join field is not configured in mappings")
	}
	best := 0.0
	found := false
	for _, id := range c.idx.Order {
		doc, live := c.idx.Docs[id]
		if !live {
			continue
		}
		name, parent := joinParts(doc[field])
		if parent != c.id {
			continue
		}
		if childType != "" && name != childType {
			continue
		}
		cc := newQueryCtxForDoc(c.idx, id, doc)
		ok, sc, err := cc.eval(inner)
		if err != nil {
			return false, 0, err
		}
		if ok {
			found = true
			if sc > best {
				best = sc
			}
		}
	}
	if found {
		if _, ok := bm["score_mode"]; ok {
			return true, best, nil
		}
		return true, 0, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalHasParent(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[has_parent] query malformed")
	}
	parentType, _ := bm["parent_type"].(string)
	if parentType == "" {
		parentType, _ = bm["type"].(string)
	}
	inner, ok := bm["query"].(map[string]any)
	if !ok {
		return false, 0, errParsing("[has_parent] requires 'query'")
	}
	field, _, hasJoin := findJoinField(c.idx)
	if !hasJoin {
		return false, 0, errParsing("failed to create query: join field is not configured in mappings")
	}
	parentID := c.idx.Routing[c.id]
	if parentID == "" {
		return false, 0, nil
	}
	doc, live := c.idx.Docs[parentID]
	if !live {
		return false, 0, nil
	}
	name, _ := joinParts(doc[field])
	if parentType != "" && name != parentType {
		return false, 0, nil
	}
	cc := newQueryCtxForDoc(c.idx, parentID, doc)
	ok, _, err := cc.eval(inner)
	if err != nil {
		return false, 0, err
	}
	if ok {
		return true, 1, nil
	}
	return false, 0, nil
}

// ---------------------------------------------------------------------------
// geo queries
// ---------------------------------------------------------------------------

func geoPointOf(v any) (float64, float64, bool) {
	switch t := v.(type) {
	case map[string]any:
		lat, ok1 := toFloat(t["lat"])
		lon, ok2 := toFloat(t["lon"])
		if ok1 && ok2 {
			return lat, lon, true
		}
	case []any:
		if len(t) == 2 {
			lon, ok1 := toFloat(t[0])
			lat, ok2 := toFloat(t[1])
			if ok1 && ok2 {
				return lat, lon, true
			}
		}
	case string:
		parts := strings.Split(t, ",")
		if len(parts) == 2 {
			lat, err1 := strconv.ParseFloat(strings.TrimSpace(parts[0]), 64)
			lon, err2 := strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
			if err1 == nil && err2 == nil {
				return lat, lon, true
			}
		}
		u := strings.ToUpper(strings.TrimSpace(t))
		if strings.HasPrefix(u, "POINT") {
			body := strings.TrimSpace(strings.TrimPrefix(u, "POINT"))
			body = strings.TrimPrefix(body, "(")
			body = strings.TrimSuffix(body, ")")
			fields := strings.Fields(body)
			if len(fields) == 2 {
				lon, e1 := strconv.ParseFloat(fields[0], 64)
				lat, e2 := strconv.ParseFloat(fields[1], 64)
				if e1 == nil && e2 == nil {
					return lat, lon, true
				}
			}
		}
	}
	return 0, 0, false
}

func unitToMeters(unit string) float64 {
	switch strings.ToLower(unit) {
	case "mi", "miles":
		return 1609.344
	case "yd", "yards":
		return 0.9144
	case "ft", "feet":
		return 0.3048
	case "in", "inch":
		return 0.0254
	case "km", "kilometers":
		return 1000
	case "cm":
		return 0.01
	case "mm":
		return 0.001
	case "nmi", "nauticalmiles":
		return 1852
	default:
		return 1
	}
}

func parseDistanceMeters(v any) (float64, bool) {
	switch t := v.(type) {
	case json.Number:
		f, _ := t.Float64()
		return f, true
	case float64:
		return t, true
	case string:
		s := strings.TrimSpace(strings.ToLower(t))
		i := 0
		for i < len(s) && (s[i] >= '0' && s[i] <= '9' || s[i] == '.' || s[i] == '-' || s[i] == '+' || s[i] == 'e') {
			i++
		}
		num, err := strconv.ParseFloat(strings.TrimSpace(s[:i]), 64)
		if err != nil {
			return 0, false
		}
		unit := strings.TrimSpace(s[i:])
		return num * unitToMeters(unit), true
	}
	return 0, false
}

func haversineMeters(lat1, lon1, lat2, lon2 float64) float64 {
	const r = 6371008.8
	rad := math.Pi / 180
	dLat := (lat2 - lat1) * rad
	dLon := (lon2 - lon1) * rad
	a := math.Sin(dLat/2)*math.Sin(dLat/2) +
		math.Cos(lat1*rad)*math.Cos(lat2*rad)*math.Sin(dLon/2)*math.Sin(dLon/2)
	return 2 * r * math.Asin(math.Sqrt(a))
}

func (c *queryCtx) evalGeoDistance(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[geo_distance] query malformed")
	}
	distance, hasDist := parseDistanceMeters(bm["distance"])
	if !hasDist {
		return false, 0, errParsing("[geo_distance] requires 'distance'")
	}
	field := ""
	var point map[string]any
	for k, v := range bm {
		if k == "distance" || k == "distance_type" || k == "boost" || k == "_name" ||
			k == "validation_method" || k == "ignore_unmapped" {
			continue
		}
		field = k
		point, _ = v.(map[string]any)
		break
	}
	if field == "" {
		return false, 0, errParsing("[geo_distance] requires a field")
	}
	plat, plon, ok := geoPointOf(point)
	if !ok {
		return false, 0, errParsing("failed to parse geo_distance point")
	}
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		lat, lon, ok := geoPointOf(v)
		if !ok {
			continue
		}
		if haversineMeters(plat, plon, lat, lon) <= distance {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalGeoBoundingBox(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[geo_bounding_box] query malformed")
	}
	field := ""
	var spec map[string]any
	for k, v := range bm {
		if k == "boost" || k == "_name" || k == "validation_method" || k == "type" || k == "ignore_unmapped" {
			continue
		}
		field = k
		spec, _ = v.(map[string]any)
		break
	}
	if field == "" || spec == nil {
		return false, 0, errParsing("[geo_bounding_box] requires a field")
	}
	var top, left, bottom, right float64
	have := false
	if tl, ok := spec["top_left"].(map[string]any); ok {
		top, _ = toFloat(tl["lat"])
		left, _ = toFloat(tl["lon"])
		have = true
	}
	if br, ok := spec["bottom_right"].(map[string]any); ok {
		bottom, _ = toFloat(br["lat"])
		right, _ = toFloat(br["lon"])
		have = true
	}
	if !have {
		top, _ = toFloat(spec["top"])
		left, _ = toFloat(spec["left"])
		bottom, _ = toFloat(spec["bottom"])
		right, _ = toFloat(spec["right"])
	}
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		lat, lon, ok := geoPointOf(v)
		if !ok {
			continue
		}
		if lat <= top && lat >= bottom && lon >= left && lon <= right {
			return true, boost, nil
		}
	}
	return false, 0, nil
}

type geoShape struct {
	typ        string
	lat, lon   float64
	radiusM    float64
	top, left  float64
	bot, right float64
	points     [][2]float64 // lat, lon
}

func parseGeoShape(v any) (*geoShape, bool) {
	m, ok := v.(map[string]any)
	if !ok {
		return nil, false
	}
	typ, _ := m["type"].(string)
	gs := &geoShape{typ: typ}
	switch typ {
	case "point":
		var coords []any
		switch c := m["coordinates"].(type) {
		case []any:
			coords = c
		case string:
			gs.lat, gs.lon, _ = geoPointOf(c)
			return gs, true
		}
		if len(coords) >= 2 {
			gs.lon, _ = toFloat(coords[0])
			gs.lat, _ = toFloat(coords[1])
		}
		return gs, true
	case "circle":
		if r, ok := m["radius"].(string); ok {
			gs.radiusM, _ = parseDistanceMeters(r)
		} else if r, ok := toFloat(m["radius"]); ok {
			gs.radiusM = r
		}
		if coords, ok := m["coordinates"].([]any); ok && len(coords) >= 2 {
			gs.lon, _ = toFloat(coords[0])
			gs.lat, _ = toFloat(coords[1])
		}
		return gs, true
	case "envelope":
		if coords, ok := m["coordinates"].([]any); ok {
			if len(coords) >= 2 {
				if tl, ok := coords[0].([]any); ok && len(tl) >= 2 {
					gs.left, _ = toFloat(tl[0])
					gs.top, _ = toFloat(tl[1])
				}
				if br, ok := coords[1].([]any); ok && len(br) >= 2 {
					gs.right, _ = toFloat(br[0])
					gs.bot, _ = toFloat(br[1])
				}
			}
		}
		return gs, true
	case "polygon":
		if rings, ok := m["coordinates"].([]any); ok && len(rings) > 0 {
			if ring, ok := rings[0].([]any); ok {
				for _, pt := range ring {
					if arr, ok := pt.([]any); ok && len(arr) >= 2 {
						lon, _ := toFloat(arr[0])
						lat, _ := toFloat(arr[1])
						gs.points = append(gs.points, [2]float64{lat, lon})
					}
				}
			}
		}
		return gs, true
	}
	return nil, false
}

func shapeContains(gs *geoShape, lat, lon float64) bool {
	switch gs.typ {
	case "point", "envelope", "circle", "polygon":
	default:
		return false
	}
	return false
}

func shapeIntersects(gs *geoShape, lat, lon float64) bool {
	switch gs.typ {
	case "point":
		return lat == gs.lat && lon == gs.lon
	case "envelope":
		return lat <= gs.top && lat >= gs.bot && lon >= gs.left && lon <= gs.right
	case "circle":
		return haversineMeters(gs.lat, gs.lon, lat, lon) <= gs.radiusM
	case "polygon":
		inside := false
		n := len(gs.points)
		for i, j := 0, n-1; i < n; j, i = i, i+1 {
			yi, xi := gs.points[i][0], gs.points[i][1]
			yj, xj := gs.points[j][0], gs.points[j][1]
			if (yi > lat) != (yj > lat) &&
				lon < (xj-xi)*(lat-yi)/(yj-yi)+xi {
				inside = !inside
			}
		}
		return inside
	}
	return false
}

func (c *queryCtx) evalGeoShape(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[geo_shape] query malformed")
	}
	field := ""
	var shapeRaw any
	for k, v := range bm {
		if k == "boost" || k == "_name" || k == "ignore_unmapped" || k == "relation" {
			continue
		}
		field = k
		shapeRaw = v
		break
	}
	if field == "" {
		return false, 0, errParsing("[geo_shape] requires a field")
	}
	qs, ok := parseGeoShape(shapeRaw)
	if !ok {
		return false, 0, errParsing("failed to parse geo_shape")
	}
	relation, _ := bm["relation"].(string)
	if relation == "" {
		relation = "INTERSECTS"
	}
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	for _, v := range vals {
		// doc stores a shape or a point
		var lat, lon float64
		valid := false
		if ds, ok := parseGeoShape(v); ok {
			switch ds.typ {
			case "point":
				lat, lon, valid = ds.lat, ds.lon, true
			case "envelope", "polygon", "circle":
				// approximate: compare against representative points
				pts := shapePoints(ds)
				for _, p := range pts {
					if shapeIntersects(qs, p[0], p[1]) {
						if strings.ToUpper(relation) != "DISJOINT" && strings.ToUpper(relation) != "WITHIN" {
							return true, boost, nil
						}
					}
				}
				continue
			}
		} else if lat, lon, valid = geoPointOf(v); !valid {
			continue
		}
		switch strings.ToUpper(relation) {
		case "DISJOINT":
			if !shapeIntersects(qs, lat, lon) {
				return true, boost, nil
			}
		default:
			if shapeIntersects(qs, lat, lon) {
				return true, boost, nil
			}
		}
	}
	return false, 0, nil
}

func shapePoints(gs *geoShape) [][2]float64 {
	switch gs.typ {
	case "point":
		return [][2]float64{{gs.lat, gs.lon}}
	case "envelope":
		return [][2]float64{{gs.top, gs.left}, {gs.bot, gs.right}}
	case "circle":
		return [][2]float64{{gs.lat, gs.lon}}
	case "polygon":
		return gs.points
	}
	return nil
}

// ---------------------------------------------------------------------------
// rank_feature / distance_feature
// ---------------------------------------------------------------------------

func (c *queryCtx) evalRankFeature(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[rank_feature] query malformed")
	}
	field, _ := bm["field"].(string)
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	f, ok := toFloat(vals[0])
	if !ok {
		return false, 0, nil
	}
	sc := f
	if sat, ok := bm["saturation"].(map[string]any); ok {
		pivot := 1.0
		if p, ok := toFloat(sat["pivot"]); ok {
			pivot = p
		}
		sc = f / (f + pivot)
	} else if lg, ok := bm["log"].(map[string]any); ok {
		sc = math.Log(1 + f)
		if s, ok := toFloat(lg["scaling_factor"]); ok {
			sc *= s
		}
	} else if ln, ok := bm["linear"].(map[string]any); ok {
		sc = f
		if s, ok := toFloat(ln["scaling_factor"]); ok {
			sc *= s
		}
	}
	return true, boost * sc, nil
}

func (c *queryCtx) evalDistanceFeature(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[distance_feature] query malformed")
	}
	field, _ := bm["field"].(string)
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	pivot := 1.0
	if p, ok := parseDistanceMeters(bm["pivot"]); ok {
		pivot = p
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	v, ok := toFloat(vals[0])
	if !ok {
		return false, 0, nil
	}
	var origin float64
	def, _ := mappingFieldDef(c.idx, field)
	ftype, _ := def["type"].(string)
	if ftype == "geo_point" {
		olat, olon, ok := geoPointOf(bm["origin"])
		if !ok {
			return false, 0, nil
		}
		lat, lon, ok := geoPointOf(vals[0])
		if !ok {
			return false, 0, nil
		}
		dist := haversineMeters(olat, olon, lat, lon)
		return true, boost * pivot / (pivot + dist), nil
	}
	if o, ok := parseQueryDate(def, bm["origin"], ""); ok == nil {
		origin, _ = toFloat(o)
	}
	dist := math.Abs(v - origin)
	if dist == 0 {
		return true, boost, nil
	}
	return true, boost * pivot / (pivot + dist), nil
}

// ---------------------------------------------------------------------------
// wrapper / pinned / percolate / terms_set
// ---------------------------------------------------------------------------

func (c *queryCtx) evalWrapper(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[wrapper] query malformed")
	}
	raw, ok := bm["query"]
	if !ok {
		return false, 0, errParsing("[wrapper] requires 'query'")
	}
	switch t := raw.(type) {
	case string:
		decoded, err := base64.StdEncoding.DecodeString(t)
		if err != nil {
			if uerr := json.Unmarshal([]byte(t), &map[string]any{}); uerr != nil {
				return false, 0, errParsing("failed to parse wrapper query")
			}
			decoded = []byte(t)
		}
		var inner map[string]any
		if err := json.Unmarshal(decoded, &inner); err != nil {
			return false, 0, errParsing("failed to parse wrapper query")
		}
		return c.eval(inner)
	case map[string]any:
		return c.eval(t)
	}
	return false, 0, errParsing("failed to parse wrapper query")
}

func (c *queryCtx) evalPinned(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[pinned] query malformed")
	}
	boost := 1.0
	if b, ok := toFloat(bm["boost"]); ok {
		boost = b
	}
	pinned := false
	for _, key := range []string{"ids"} {
		if list, ok := bm[key].([]any); ok {
			for _, e := range list {
				if s, _ := toString(e); s == c.id {
					pinned = true
				}
			}
		}
	}
	if docs, ok := bm["docs"].([]any); ok {
		for _, e := range docs {
			if m, ok := e.(map[string]any); ok {
				if s, _ := toString(m["_id"]); s == c.id {
					pinned = true
				}
			}
		}
	}
	if pinned {
		return true, boost, nil
	}
	if organic, ok := bm["organic"].(map[string]any); ok {
		return c.eval(organic)
	}
	return false, 0, nil
}

func (c *queryCtx) evalPercolate(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[percolate] query malformed")
	}
	docRaw, ok := bm["document"].(map[string]any)
	if !ok {
		if _, hasID := bm["id"]; hasID {
			return false, 0, errParsing("percolate by id is not supported")
		}
		return false, 0, errParsing("[percolate] requires 'document'")
	}
	field, _ := bm["field"].(string)
	if field == "" {
		field = "query"
	}
	inputCtx := newIndexCtx(c.idx, "_percolate")
	inputCtx.doc = docRaw
	if err := inputCtx.buildDocValues(); err != nil {
		return true, 0, nil
	}
	inputQC := &queryCtx{idx: c.idx, id: "_percolate", doc: docRaw, vals: inputCtx.values}
	score := 0.0
	for _, id := range c.idx.Order {
		doc, live := c.idx.Docs[id]
		if !live {
			continue
		}
		stored, ok := doc[field]
		if !ok {
			continue
		}
		if id == c.id {
			continue
		}
		qm, ok := stored.(map[string]any)
		if !ok {
			continue
		}
		matched, sc, err := inputQC.eval(qm)
		if err != nil {
			continue
		}
		if matched {
			if sc > score {
				score = sc
			}
			if id == c.id {
				return true, sc, nil
			}
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalTermsSet(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[terms_set] query malformed")
	}
	field := ""
	var terms []any
	for k, v := range bm {
		if k == "boost" || k == "_name" {
			continue
		}
		field = k
		terms, _ = v.([]any)
		break
	}
	if field == "" {
		return false, 0, errParsing("[terms_set] requires a field")
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	matched := 0
	for _, item := range terms {
		for _, v := range vals {
			vs, _ := valueScalarString(v)
			is, _ := valueScalarString(item)
			if vs == is || numericEqual(v, item) {
				matched++
				break
			}
		}
	}
	required := 1
	if name, ok := bm["minimum_should_match_field"].(string); ok {
		if reqVals, ok := c.values(name); ok && len(reqVals) > 0 {
			required, _ = numericInt(reqVals[0])
		}
	}
	if scriptSpec, ok := bm["minimum_should_match_script"].(map[string]any); ok {
		if src, ok := scriptSpec["source"].(string); ok {
			if n, ok := scriptMinimumShouldMatch(src, len(terms), matched); ok {
				required = n
			}
		}
	}
	if matched >= required {
		return true, 1, nil
	}
	return false, 0, nil
}

func numericInt(v any) (int, bool) {
	if n, ok := integerOf(v); ok {
		return int(n), true
	}
	if f, ok := toFloat(v); ok {
		return int(f), true
	}
	return 0, false
}

func scriptMinimumShouldMatch(src string, count, matched int) (int, bool) {
	src = strings.ReplaceAll(src, " ", "")
	switch {
	case strings.Contains(src, "Math.min"):
		return count, true
	case strings.Contains(src, "Math.max"):
		lower := 1
		if i := strings.Index(src, "Math.max("); i >= 0 {
			rest := src[i+len("Math.max("):]
			if j := strings.Index(rest, ","); j > 0 {
				if n, err := strconv.Atoi(strings.TrimSpace(rest[:j])); err == nil {
					lower = n
				}
			}
		}
		return lower, true
	}
	return 0, false
}

// ---------------------------------------------------------------------------
// span queries
// ---------------------------------------------------------------------------

func spanKind(body map[string]any) (string, map[string]any) {
	for k, v := range body {
		if k == "boost" || k == "_name" {
			continue
		}
		bm, _ := v.(map[string]any)
		return k, bm
	}
	return "", nil
}

func isSpanType(k string) bool {
	switch k {
	case "span_term", "span_near", "span_or", "span_not", "span_containing", "span_within",
		"span_multi", "field_masking_span", "span_first":
		return true
	}
	return false
}

func spanTermValue(bm map[string]any) (string, string, *apiError) {
	keys := []string{}
	for k := range bm {
		if k == "boost" || k == "_name" {
			continue
		}
		keys = append(keys, k)
	}
	if len(keys) > 1 {
		sort.Strings(keys)
		return "", "", errParsing("[span_term] query doesn't support multiple fields, found [" +
			strings.Join(keys, "] and [") + "]")
	}
	if len(keys) == 0 {
		return "", "", errParsing("span_term malformed")
	}
	field := keys[0]
	val := bm[field]
	term := ""
	if m, ok := val.(map[string]any); ok {
		term, _ = toString(m["value"])
	} else {
		term, _ = toString(val)
	}
	return field, term, nil
}

func (c *queryCtx) evalSpanTerm(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_term malformed")
	}
	spans, _, err := c.spanTermSpans(bm)
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) spanTermSpans(bm map[string]any) ([]intervalSpan, string, *apiError) {
	field, term, err := spanTermValue(bm)
	if err != nil {
		return nil, "", err
	}
	def, known := mappingFieldDef(c.idx, field)
	if !known {
		return nil, field, nil
	}
	ftype, _ := def["type"].(string)
	if ftype == "integer" || ftype == "long" || ftype == "short" || ftype == "byte" || ftype == "double" ||
		ftype == "float" || ftype == "half_float" || ftype == "scaled_float" || ftype == "unsigned_long" ||
		ftype == "date" || ftype == "date_nanos" || ftype == "boolean" || ftype == "ip" {
		return nil, field, matchSearchError("query_shard_exception",
			"Cannot extract a term from a query of type ["+ftype+"]")
	}
	spans := []intervalSpan{}
	for _, p := range c.intervalPositions(field) {
		if p.term == term {
			spans = append(spans, intervalSpan{p.pos, p.pos})
		}
	}
	return spans, field, nil
}

func (c *queryCtx) spanQuery(body any) ([]intervalSpan, string, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return nil, "", errParsing("span query malformed")
	}
	kind, inner := spanKind(bm)
	if kind == "" || inner == nil {
		return nil, "", errParsing("span query malformed")
	}
	switch kind {
	case "span_term":
		return c.spanTermSpans(inner)
	case "span_near":
		clauses, _ := inner["clauses"].([]any)
		if len(clauses) == 0 {
			return nil, "", errParsing("span_near must include [clauses]")
		}
		lists := [][]intervalSpan{}
		field := ""
		for _, cl := range clauses {
			cm, _ := cl.(map[string]any)
			ck, cinner := spanKind(cm)
			if !isSpanType(ck) {
				return nil, "", errParsing("span_near [clauses] must be of type span query")
			}
			spans, f, err := c.spanQuery(cm)
			if err != nil {
				return nil, "", err
			}
			if field == "" {
				field = f
			} else if f != field {
				term := ""
				if ck == "span_term" {
					_, term, _ = spanTermValue(cinner)
				}
				return nil, "", matchSearchError("query_shard_exception",
					"Cannot add clause "+f+":"+term+" to SpanNearQuery for field "+field)
			}
			lists = append(lists, spans)
		}
		slop := 0
		if v, ok := toFloat(inner["slop"]); ok {
			slop = int(v)
		}
		inOrder := true
		if v, ok := inner["in_order"].(bool); ok {
			inOrder = v
		}
		if inOrder {
			return combineOrderedIntervals(lists, slop), field, nil
		}
		return combineUnorderedIntervals(lists, slop), field, nil
	case "span_or":
		clauses, _ := inner["clauses"].([]any)
		field := ""
		spans := []intervalSpan{}
		for _, cl := range clauses {
			cm, _ := cl.(map[string]any)
			sp, f, err := c.spanQuery(cm)
			if err != nil {
				return nil, "", err
			}
			if field == "" {
				field = f
			} else if f != field {
				return nil, "", matchSearchError("query_shard_exception", "Clauses must have same field.")
			}
			spans = append(spans, sp...)
		}
		return spans, field, nil
	case "span_first":
		match, _ := inner["match"].(map[string]any)
		end := 1
		if v, ok := toFloat(inner["end"]); ok {
			end = int(v)
		}
		spans, field, err := c.spanQuery(match)
		if err != nil {
			return nil, "", err
		}
		out := []intervalSpan{}
		for _, sp := range spans {
			if sp.start < end {
				out = append(out, sp)
			}
		}
		return out, field, nil
	case "span_not":
		include, _ := inner["include"].(map[string]any)
		exclude, _ := inner["exclude"].(map[string]any)
		inc, field, err := c.spanQuery(include)
		if err != nil {
			return nil, "", err
		}
		exc, _, err := c.spanQuery(exclude)
		if err != nil {
			return nil, "", err
		}
		pre, hasPre := toFloat(inner["pre"])
		post, hasPost := toFloat(inner["post"])
		dist, hasDist := toFloat(inner["dist"])
		out := []intervalSpan{}
		for _, a := range inc {
			drop := false
			for _, b := range exc {
				if hasPre {
					if b.end < a.start && float64(a.start-b.end) <= pre {
						drop = true
					}
				} else if hasPost {
					if b.start > a.end && float64(b.start-a.end) <= post {
						drop = true
					}
				} else if hasDist {
					d := b.start - a.end
					if d < 0 {
						d = a.start - b.end
					}
					if d < 0 {
						d = 0
					}
					if float64(d) <= dist {
						drop = true
					}
				} else if b.start <= a.end && a.start <= b.end {
					drop = true
				}
				if drop {
					break
				}
			}
			if !drop {
				out = append(out, a)
			}
		}
		return out, field, nil
	case "span_containing":
		big, _ := inner["big"].(map[string]any)
		little, _ := inner["little"].(map[string]any)
		bigs, field, err := c.spanQuery(big)
		if err != nil {
			return nil, "", err
		}
		littles, _, err := c.spanQuery(little)
		if err != nil {
			return nil, "", err
		}
		out := []intervalSpan{}
		for _, a := range bigs {
			for _, b := range littles {
				if a.start <= b.start && a.end >= b.end {
					out = append(out, a)
					break
				}
			}
		}
		return out, field, nil
	case "span_within":
		big, _ := inner["big"].(map[string]any)
		little, _ := inner["little"].(map[string]any)
		bigs, field, err := c.spanQuery(big)
		if err != nil {
			return nil, "", err
		}
		littles, _, err := c.spanQuery(little)
		if err != nil {
			return nil, "", err
		}
		out := []intervalSpan{}
		for _, b := range littles {
			for _, a := range bigs {
				if a.start <= b.start && a.end >= b.end {
					out = append(out, b)
					break
				}
			}
		}
		return out, field, nil
	case "span_multi":
		match, _ := inner["match"].(map[string]any)
		mk, mbm := spanKind(match)
		if mk != "prefix" && mk != "wildcard" && mk != "regexp" && mk != "fuzzy" {
			return nil, "", errParsing("[span_multi] [match] must be of type multi term query")
		}
		field, val := "", ""
		for k, v := range mbm {
			field = k
			if m, ok := v.(map[string]any); ok {
				if x, ok := m["value"]; ok {
					val, _ = toString(x)
				} else if x, ok := m["pattern"]; ok {
					val, _ = toString(x)
				} else if x, ok := m["prefix"]; ok {
					val, _ = toString(x)
				} else if x, ok := m["term"]; ok {
					val, _ = toString(x)
				}
			} else {
				val, _ = toString(v)
			}
			break
		}
		spans := []intervalSpan{}
		positions := c.intervalPositions(field)
		switch mk {
		case "prefix":
			val = strings.ToLower(val)
			for _, p := range positions {
				if strings.HasPrefix(p.term, val) {
					spans = append(spans, intervalSpan{p.pos, p.pos})
				}
			}
		case "wildcard":
			re := wildcardRegex(strings.ToLower(val))
			for _, p := range positions {
				if re.MatchString(p.term) {
					spans = append(spans, intervalSpan{p.pos, p.pos})
				}
			}
		case "regexp":
			re, rerr := regexp.Compile(val)
			if rerr != nil {
				return nil, "", errParsing("invalid regexp")
			}
			for _, p := range positions {
				if re.MatchString(p.term) {
					spans = append(spans, intervalSpan{p.pos, p.pos})
				}
			}
		case "fuzzy":
			for _, p := range positions {
				if fuzzyDistance(p.term, val, true) <= autoFuzziness(val) {
					spans = append(spans, intervalSpan{p.pos, p.pos})
				}
			}
		}
		return spans, field, nil
	case "field_masking_span":
		query, _ := inner["query"].(map[string]any)
		target, _ := inner["field"].(string)
		spans, _, err := c.spanQuery(query)
		if err != nil {
			return nil, "", err
		}
		return spans, target, nil
	}
	return nil, "", errParsing("unknown span query [" + kind + "]")
}

func (c *queryCtx) evalSpanNear(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_near malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_near": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanOr(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_or malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_or": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanFirst(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_first malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_first": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanNot(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_not malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_not": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanContaining(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_containing malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_containing": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanWithin(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_within malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_within": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

func (c *queryCtx) evalSpanMulti(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_multi malformed")
	}
	spans, _, err := c.spanQuery(map[string]any{"span_multi": bm})
	if err != nil {
		return false, 0, err
	}
	return len(spans) > 0, 1, nil
}

type intervalSpan struct {
	start int
	end   int
}

func (c *queryCtx) evalIntervals(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("intervals malformed")
	}
	field := ""
	var rules map[string]any
	for k, v := range bm {
		if k == "boost" || k == "_name" {
			continue
		}
		field = k
		rules, _ = v.(map[string]any)
		break
	}
	if field == "" || rules == nil {
		return false, 0, errParsing("intervals malformed")
	}
	def, known := mappingFieldDef(c.idx, field)
	if !known {
		return false, 0, nil
	}
	ftype, _ := def["type"].(string)
	if ftype != "" && ftype != "text" && ftype != "match_only_text" && ftype != "search_as_you_type" {
		return false, 0, matchSearchError("query_shard_exception",
			"Can only use interval queries on text fields - not on ["+field+"] which is of type ["+ftype+"]")
	}
	ruleKeys := []string{}
	for k := range rules {
		if k == "boost" || k == "_name" {
			continue
		}
		ruleKeys = append(ruleKeys, k)
	}
	if len(ruleKeys) > 1 {
		sort.Strings(ruleKeys)
		return false, 0, errParsing("Only one interval rule can be specified, found [" +
			strings.Join(ruleKeys, "] and [") + "]")
	}
	if len(ruleKeys) == 0 {
		return false, 0, nil
	}
	if !isKnownIntervalType(ruleKeys[0]) {
		return false, 0, errParsing("Unknown interval type [" + ruleKeys[0] + "]")
	}
	positions := c.intervalPositions(field)
	spans, err := c.intervalRule(rules, positions, field)
	if err != nil {
		return false, 0, err
	}
	if len(spans) == 0 {
		return false, 0, nil
	}
	return true, 1, nil
}

func isKnownIntervalType(k string) bool {
	switch k {
	case "match", "prefix", "wildcard", "regexp", "fuzzy", "all_of", "any_of":
		return true
	}
	return false
}

func (c *queryCtx) intervalPositions(field string) []analyzedToken {
	vals := c.mustValues(field)
	def, _ := mappingFieldDef(c.idx, field)
	gap := 100
	if g, ok := toFloat(def["position_increment_gap"]); ok {
		gap = int(g)
	}
	out := []analyzedToken{}
	last := -1
	for _, v := range vals {
		s, _ := valueScalarString(v)
		toks := analyzeTokensWithIndex(c.idx, c.analyzerName(field, nil), s)
		base := 0
		if last >= 0 {
			base = last + gap + 1
		}
		for _, t := range toks {
			out = append(out, analyzedToken{term: t.term, pos: t.pos + base})
			if t.pos+base > last {
				last = t.pos + base
			}
		}
	}
	return out
}

func intervalTermSpans(positions []analyzedToken, terms []string) [][]intervalSpan {
	out := make([][]intervalSpan, 0, len(terms))
	for _, term := range terms {
		list := []intervalSpan{}
		for _, p := range positions {
			if p.term == term {
				list = append(list, intervalSpan{p.pos, p.pos})
			}
		}
		if len(list) == 0 {
			return nil
		}
		out = append(out, list)
	}
	return out
}

func combineOrderedIntervals(lists [][]intervalSpan, maxGaps int) []intervalSpan {
	if len(lists) == 0 {
		return nil
	}
	cur := lists[0]
	for i := 1; i < len(lists); i++ {
		next := []intervalSpan{}
		for _, a := range cur {
			for _, b := range lists[i] {
				if b.start <= a.end {
					continue
				}
				if maxGaps >= 0 && b.start-a.end-1 > maxGaps {
					continue
				}
				next = append(next, intervalSpan{a.start, b.end})
			}
		}
		cur = next
	}
	return cur
}

func combineUnorderedIntervals(lists [][]intervalSpan, maxGaps int) []intervalSpan {
	if len(lists) == 0 {
		return nil
	}
	cur := lists[0]
	for i := 1; i < len(lists); i++ {
		next := []intervalSpan{}
		for _, a := range cur {
			for _, b := range lists[i] {
				gap := 0
				if b.start > a.end {
					gap = b.start - a.end - 1
				} else if a.start > b.end {
					gap = a.start - b.end - 1
				}
				if maxGaps >= 0 && gap > maxGaps {
					continue
				}
				start, end := a.start, a.end
				if b.start < start {
					start = b.start
				}
				if b.end > end {
					end = b.end
				}
				next = append(next, intervalSpan{start, end})
			}
		}
		cur = next
	}
	return cur
}

func (c *queryCtx) intervalRule(rule map[string]any, positions []analyzedToken, field string) ([]intervalSpan, *apiError) {
	kind := ""
	var bm map[string]any
	for k, v := range rule {
		if k == "boost" || k == "_name" {
			continue
		}
		kind = k
		bm, _ = v.(map[string]any)
		break
	}
	if kind == "" || bm == nil {
		return nil, errParsing("intervals malformed")
	}
	var spans []intervalSpan
	switch kind {
	case "match":
		q, _ := bm["query"].(string)
		analyzer := ""
		if a, ok := bm["analyzer"].(string); ok {
			analyzer = a
		} else {
			analyzer = c.analyzerName(field, nil)
		}
		toks := analyzeTokensWithIndex(c.idx, analyzer, q)
		terms := []string{}
		for _, t := range toks {
			terms = append(terms, t.term)
		}
		lists := intervalTermSpans(positions, terms)
		if lists == nil {
			return nil, nil
		}
		maxGaps := -1
		if g, ok := toFloat(bm["max_gaps"]); ok {
			maxGaps = int(g)
		}
		ordered, _ := bm["ordered"].(bool)
		if ordered {
			spans = combineOrderedIntervals(lists, maxGaps)
		} else {
			spans = combineUnorderedIntervals(lists, maxGaps)
		}
	case "prefix":
		pat, _ := bm["prefix"].(string)
		pat = strings.ToLower(pat)
		for _, p := range positions {
			if strings.HasPrefix(p.term, pat) {
				spans = append(spans, intervalSpan{p.pos, p.pos})
			}
		}
	case "wildcard":
		pat, _ := bm["pattern"].(string)
		re := wildcardRegex(strings.ToLower(pat))
		for _, p := range positions {
			if re.MatchString(p.term) {
				spans = append(spans, intervalSpan{p.pos, p.pos})
			}
		}
	case "regexp":
		pat, _ := bm["pattern"].(string)
		re, err := regexp.Compile(pat)
		if err != nil {
			return nil, errParsing("invalid regexp")
		}
		for _, p := range positions {
			if re.MatchString(p.term) {
				spans = append(spans, intervalSpan{p.pos, p.pos})
			}
		}
	case "fuzzy":
		term, _ := bm["term"].(string)
		fuzz := autoFuzziness(term)
		if v, ok := bm["fuzziness"]; ok {
			if n, ok2 := fuzzinessValue(v, term); ok2 {
				fuzz = n
			}
		}
		prefixLen := 0
		if pl, ok := toFloat(bm["prefix_length"]); ok {
			prefixLen = int(pl)
		}
		for _, p := range positions {
			if len(p.term) <= prefixLen || len(term) <= prefixLen {
				continue
			}
			if p.term[:prefixLen] != term[:prefixLen] {
				continue
			}
			if fuzzyDistance(p.term, term, true) <= fuzz {
				spans = append(spans, intervalSpan{p.pos, p.pos})
			}
		}
	case "all_of":
		subs, _ := bm["intervals"].([]any)
		lists := [][]intervalSpan{}
		for _, sub := range subs {
			sm, _ := sub.(map[string]any)
			sp, err := c.intervalRule(sm, positions, field)
			if err != nil {
				return nil, err
			}
			lists = append(lists, sp)
		}
		maxGaps := -1
		if g, ok := toFloat(bm["max_gaps"]); ok {
			maxGaps = int(g)
		}
		ordered, _ := bm["ordered"].(bool)
		if ordered {
			spans = combineOrderedIntervals(lists, maxGaps)
		} else {
			spans = combineUnorderedIntervals(lists, maxGaps)
		}
	case "any_of":
		subs, _ := bm["intervals"].([]any)
		for _, sub := range subs {
			sm, _ := sub.(map[string]any)
			sp, err := c.intervalRule(sm, positions, field)
			if err != nil {
				return nil, err
			}
			spans = append(spans, sp...)
		}
	default:
		return nil, errParsing("Unknown interval type [" + kind + "]")
	}
	if f, ok := bm["filter"].(map[string]any); ok {
		filtered, err := c.filterIntervalSpans(spans, f, positions, field)
		if err != nil {
			return nil, err
		}
		spans = filtered
	}
	return spans, nil
}

func (c *queryCtx) filterIntervalSpans(spans []intervalSpan, filter map[string]any, positions []analyzedToken, field string) ([]intervalSpan, *apiError) {
	if len(spans) == 0 {
		return spans, nil
	}
	kind := ""
	var bm map[string]any
	for k, v := range filter {
		kind = k
		bm, _ = v.(map[string]any)
		break
	}
	if kind == "script" {
		src, _ := bm["source"].(string)
		parts := strings.Fields(src)
		if len(parts) < 3 {
			return spans, nil
		}
		op := parts[1]
		limit, _ := strconv.ParseFloat(parts[2], 64)
		out := []intervalSpan{}
		for _, sp := range spans {
			val := float64(sp.start)
			if strings.HasSuffix(parts[0], ".end") {
				val = float64(sp.end)
			}
			keep := false
			switch op {
			case ">":
				keep = val > limit
			case ">=":
				keep = val >= limit
			case "<":
				keep = val < limit
			case "<=":
				keep = val <= limit
			case "==":
				keep = val == limit
			}
			if keep {
				out = append(out, sp)
			}
		}
		return out, nil
	}
	other, err := c.intervalRule(bm, positions, field)
	if err != nil {
		return nil, err
	}
	out := []intervalSpan{}
	for _, sp := range spans {
		keep := false
		for _, o := range other {
			switch kind {
			case "before":
				keep = o.start > sp.end
			case "after":
				keep = o.end < sp.start
			case "containing":
				keep = sp.start <= o.start && sp.end >= o.end
			case "not_containing":
				keep = !(sp.start <= o.start && sp.end >= o.end)
			case "contained_by":
				keep = o.start <= sp.start && o.end >= sp.end
			case "not_contained_by":
				keep = !(o.start <= sp.start && o.end >= sp.end)
			case "not_overlapping":
				keep = !(o.start <= sp.end && sp.start <= o.end)
			}
			if keep {
				break
			}
		}
		if kind == "not_containing" || kind == "not_contained_by" || kind == "not_overlapping" {
			if len(other) == 0 {
				keep = true
			}
		}
		if keep {
			out = append(out, sp)
		}
	}
	return out, nil
}

func (c *queryCtx) evalTermSingle(field string, bm map[string]any) (bool, float64, *apiError) {
	q, _ := bm["query"].(string)
	if field == "" {
		for f := range c.valuesMap() {
			ok, _, _ := c.evalTerm(f, q, 1.0)
			if ok {
				return true, 1, nil
			}
		}
		return false, 0, nil
	}
	ok, _, err := c.evalTerm(field, q, 1.0)
	return ok, 1, err
}

func (c *queryCtx) evalIntervalsSimple(pred func(field string, tokens []string) bool) bool {
	for f, vals := range c.valuesMap() {
		tokens := []string{}
		for _, v := range vals {
			s, ok := valueScalarString(v)
			if !ok {
				continue
			}
			tokens = append(tokens, analyze(s)...)
		}
		if pred(f, tokens) {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// more_like_this
// ---------------------------------------------------------------------------

func (c *queryCtx) evalMoreLikeThis(bm map[string]any) (bool, float64, *apiError) {
	var fields []string
	switch t := bm["fields"].(type) {
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				fields = append(fields, s)
			}
		}
	case string:
		fields = []string{t}
	}
	var texts []string
	switch t := bm["like"].(type) {
	case string:
		texts = append(texts, t)
	case map[string]any:
		texts = append(texts, collectDocTexts(t, nil)...)
	case []any:
		for _, e := range t {
			switch v := e.(type) {
			case string:
				texts = append(texts, v)
			case map[string]any:
				if doc, ok := v["doc"].(map[string]any); ok {
					texts = append(texts, collectDocTexts(doc, fields)...)
				} else if id, ok := toString(v["_id"]); ok {
					if sd, live := c.idx.Docs[id]; live {
						texts = append(texts, collectDocTexts(sd, fields)...)
					}
				} else {
					texts = append(texts, collectDocTexts(v, fields)...)
				}
			}
		}
	}
	terms := map[string]int{}
	for _, t := range texts {
		for _, tok := range analyze(t) {
			terms[tok]++
		}
	}
	if len(terms) == 0 {
		return false, 0, nil
	}
	targets := fields
	if len(targets) == 0 {
		for f := range c.valuesMap() {
			targets = append(targets, f)
		}
	}
	minMatch := 1
	if msm, ok := bm["minimum_should_match"]; ok {
		minMatch = minimumShouldMatch(msm, len(terms), 0)
	}
	score := 0.0
	for _, f := range targets {
		for _, v := range c.mustValues(f) {
			s, ok := valueScalarString(v)
			if !ok {
				continue
			}
			docTerms := map[string]bool{}
			for _, tok := range analyze(s) {
				docTerms[tok] = true
			}
			matched := 0
			for t := range terms {
				if docTerms[t] {
					matched++
				}
			}
			if matched >= minMatch {
				score += float64(matched)
			}
		}
	}
	if score > 0 {
		return true, score, nil
	}
	return false, 0, nil
}

func collectDocTexts(doc map[string]any, fields []string) []string {
	var out []string
	var walk func(v any)
	walk = func(v any) {
		switch t := v.(type) {
		case string:
			out = append(out, t)
		case []any:
			for _, e := range t {
				walk(e)
			}
		case map[string]any:
			for _, e := range t {
				walk(e)
			}
		}
	}
	if len(fields) > 0 {
		for _, f := range fields {
			if v, ok := fieldValue(doc, f); ok {
				walk(v)
			}
		}
		return out
	}
	for k, v := range doc {
		if strings.HasPrefix(k, "_") {
			continue
		}
		walk(v)
	}
	return out
}

// ---------------------------------------------------------------------------
// knn
// ---------------------------------------------------------------------------

func (c *queryCtx) evalKnn(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[knn] query malformed")
	}
	field, _ := bm["field"].(string)
	vecRaw, _ := bm["query_vector"].([]any)
	var qv []float64
	for _, e := range vecRaw {
		if f, ok := toFloat(e); ok {
			qv = append(qv, f)
		}
	}
	if field == "" || len(qv) == 0 {
		return false, 0, errParsing("[knn] requires field and query_vector")
	}
	vals, present := c.values(field)
	if !present {
		return false, 0, nil
	}
	var dv []float64
	for _, e := range vals {
		if f, ok := toFloat(e); ok {
			dv = append(dv, f)
		}
	}
	if len(dv) != len(qv) {
		return false, 0, nil
	}
	dot, na, nb := 0.0, 0.0, 0.0
	for i := range qv {
		dot += qv[i] * dv[i]
		na += qv[i] * qv[i]
		nb += dv[i] * dv[i]
	}
	if na == 0 || nb == 0 {
		return false, 0, nil
	}
	cos := dot / (math.Sqrt(na) * math.Sqrt(nb))
	sim := (cos + 1) / 2
	return true, sim, nil
}

// ---------------------------------------------------------------------------
// values map access helper
// ---------------------------------------------------------------------------

func (c *queryCtx) valuesMap() map[string][]any {
	if c.vals != nil {
		return c.vals
	}
	return c.idx.DocValues[c.id]
}

// ---------------------------------------------------------------------------
// function_score
// ---------------------------------------------------------------------------

func (c *queryCtx) evalFunctionScore(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("[function_score] query malformed")
	}
	inner, ok := bm["query"].(map[string]any)
	if !ok {
		inner = map[string]any{"match_all": map[string]any{}}
	}
	matched, base, err := c.eval(inner)
	if err != nil {
		return false, 0, err
	}
	if !matched {
		return false, 0, nil
	}
	functions := asQueryList(bm["functions"])
	if len(functions) == 0 {
		if f, ok := bm["function"].(map[string]any); ok {
			functions = []any{f}
		}
	}
	scoreMode, _ := bm["score_mode"].(string)
	if scoreMode == "" {
		scoreMode = "multiply"
	}
	finalScore := 0.0
	first := true
	for _, fnRaw := range functions {
		fn, ok := fnRaw.(map[string]any)
		if !ok {
			continue
		}
		fv := c.applyScoreFunction(fn)
		if b, ok := toFloat(fn["weight"]); ok {
			fv *= b
		}
		switch scoreMode {
		case "first", "":
			if first {
				finalScore = fv
			}
		case "max":
			if first || fv > finalScore {
				finalScore = fv
			}
		case "min":
			if first || fv < finalScore {
				finalScore = fv
			}
		case "sum":
			finalScore += fv
		case "avg":
			finalScore += fv
		case "replace":
			finalScore = fv
		default:
			if first {
				finalScore = fv
			} else {
				finalScore *= fv
			}
		}
		first = false
	}
	if len(functions) == 0 {
		finalScore = 1
	}
	boostMode, _ := bm["boost_mode"].(string)
	var combined float64
	switch boostMode {
	case "replace":
		combined = finalScore
	case "sum":
		combined = base + finalScore
	case "avg":
		combined = (base + finalScore) / 2
	case "max":
		combined = math.Max(base, finalScore)
	case "min":
		combined = math.Min(base, finalScore)
	default:
		combined = base * finalScore
	}
	if mb, ok := toFloat(bm["max_boost"]); ok && combined > mb {
		combined = mb
	}
	if ms, ok := toFloat(bm["min_score"]); ok && combined < ms {
		return false, 0, nil
	}
	return true, combined, nil
}

func (c *queryCtx) applyScoreFunction(fn map[string]any) float64 {
	if v, ok := toFloat(fn["weight"]); ok {
		return v
	}
	if fvf, ok := fn["field_value_factor"].(map[string]any); ok {
		field, _ := fvf["field"].(string)
		factor := 1.0
		if v, ok := toFloat(fvf["factor"]); ok {
			factor = v
		}
		modifier, _ := fvf["modifier"].(string)
		var val float64
		if vals, ok := c.values(field); ok && len(vals) > 0 {
			val, _ = toFloat(vals[0])
		} else if mv, ok := toFloat(fvf["missing"]); ok {
			val = mv
		}
		val *= factor
		switch modifier {
		case "log1p":
			val = math.Log1p(val)
		case "log2p":
			val = math.Log2(1 + val)
		case "ln":
			val = math.Log(val)
		case "ln1p":
			val = math.Log1p(val)
		case "ln2p":
			val = math.Log(1+val) / math.Log(2)
		case "square":
			val = val * val
		case "sqrt":
			val = math.Sqrt(val)
		case "reciprocal":
			val = 1 / val
		}
		return val
	}
	if rs, ok := fn["random_score"].(map[string]any); ok {
		seed := int64(0)
		if s, ok := rs["seed"]; ok {
			seed, _ = integerOf(s)
		}
		if seed == 0 {
			seed = 42
		}
		return pseudoRandom(c.id, seed)
	}
	if ss, ok := fn["script_score"].(map[string]any); ok {
		sm, _ := ss["script"].(map[string]any)
		src, _ := sm["source"].(string)
		env := &scriptEnv{c: c, params: scriptParamsOf(sm)}
		if v, err := env.evalScriptExpression(src); err == nil {
			return scriptFloat(v)
		}
		return 1
	}
	for _, fnName := range []string{"gauss", "linear", "exp"} {
		if dec, ok := fn[fnName].(map[string]any); ok {
			field, _ := dec["field"].(string)
			vals, _ := c.values(field)
			if len(vals) == 0 {
				return 1
			}
			val, _ := toFloat(vals[0])
			origin := 0.0
			if o, ok := toFloat(dec["origin"]); ok {
				origin = o
			}
			scale := 1.0
			if s, ok := toFloat(dec["scale"]); ok {
				scale = s
			}
			offset := 0.0
			if o, ok := toFloat(dec["offset"]); ok {
				offset = o
			}
			decayV := 0.5
			if d, ok := toFloat(dec["decay"]); ok {
				decayV = d
			}
			dist := math.Abs(val - origin)
			if dist <= offset {
				return 1
			}
			x := (dist - offset) / scale
			switch fnName {
			case "gauss":
				return math.Exp(-0.5 * x * x * math.Log(decayV) / math.Log(0.5) * -1)
			case "exp":
				return math.Pow(decayV, x)
			case "linear":
				if 1-x < 0 {
					return 0
				}
				return 1 - x
			}
		}
	}
	return 1
}

func pseudoRandom(id string, seed int64) float64 {
	h := uint64(seed)*1469598103934665603 + 14695981039346656037
	for i := 0; i < len(id); i++ {
		h ^= uint64(id[i])
		h *= 1099511628211
	}
	return float64(h>>11) / float64(1<<53)
}

// ---------------------------------------------------------------------------
// query_string parser
// ---------------------------------------------------------------------------

type qsParser struct {
	tokens       []string
	pos          int
	default_     string
	fields       []string
	operator     string
	simple       bool
	c            *queryCtx
	allowLeading bool
	hasFields    bool
	plusUsed     bool
	prefixOp     bool
	fuzzyOp      bool
	phraseOp     bool
	slopOp       bool
	notOp        bool
	andOp        bool
	quoteSuffix  string
}

func tokenizeQueryString(s string) []string {
	var out []string
	i := 0
	for i < len(s) {
		ch := s[i]
		if ch == ' ' || ch == '\t' || ch == '\n' {
			i++
			continue
		}
		if ch == '"' {
			j := i + 1
			for j < len(s) && s[j] != '"' {
				j++
			}
			if j < len(s) {
				j++
			}
			for j < len(s) && s[j] != ' ' && s[j] != '\t' && s[j] != '\n' {
				j++
			}
			out = append(out, s[i:j])
			i = j
			continue
		}
		if ch == '(' || ch == ')' {
			out = append(out, string(ch))
			i++
			continue
		}
		j := i
		for j < len(s) && s[j] != ' ' && s[j] != '\t' && s[j] != '\n' && s[j] != '(' && s[j] != ')' {
			if s[j] == '\\' && j+1 < len(s) {
				j += 2
				continue
			}
			j++
		}
		part := s[i:j]
		if strings.ContainsAny(part, "[{") && !strings.ContainsAny(part, "]}") {
			for j < len(s) && s[j] != ']' && s[j] != '}' {
				j++
			}
			if j < len(s) {
				j++
			}
		}
		out = append(out, s[i:j])
		i = j
	}
	return out
}

func (p *qsParser) parse() (map[string]any, *apiError) {
	q, err := p.parseOr()
	if err != nil {
		return nil, err
	}
	return q, nil
}

func (p *qsParser) parseOr() (map[string]any, *apiError) {
	left, err := p.parseAnd()
	if err != nil {
		return nil, err
	}
	for p.pos < len(p.tokens) {
		tok := p.tokens[p.pos]
		if tok == "OR" || tok == "||" {
			p.pos++
			if p.pos >= len(p.tokens) {
				return nil, errParsing("trailing operator")
			}
			right, err := p.parseAnd()
			if err != nil {
				return nil, err
			}
			left = boolQuery("should", []any{left, right}, 0)
			continue
		}
		break
	}
	return left, nil
}

func (p *qsParser) parseAnd() (map[string]any, *apiError) {
	must := []any{}
	should := []any{}
	mustNot := []any{}
	forceMust := false
	for p.pos < len(p.tokens) {
		tok := p.tokens[p.pos]
		if p.simple && (tok == "AND" || tok == "OR" || tok == "NOT") {
			// plain term in simple query syntax
		} else if tok == "OR" || tok == "||" || tok == ")" {
			break
		}
		if !p.simple && (tok == "AND" || tok == "&&") {
			p.pos++
			forceMust = true
			continue
		}
		if !p.simple && tok == "NOT" {
			p.pos++
			inner, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			mustNot = append(mustNot, inner)
			forceMust = false
			continue
		}
		sign := 0
		if len(tok) > 1 && (strings.HasPrefix(tok, "+") || strings.HasPrefix(tok, "-") || strings.HasPrefix(tok, "!")) {
			switch tok[0] {
			case '+':
				sign = 1
				p.plusUsed = true
			case '-', '!':
				sign = -1
			}
			p.tokens[p.pos] = tok[1:]
		} else if tok == "-" || tok == "+" || tok == "!" {
			if tok == "-" || tok == "!" {
				sign = -1
			} else {
				sign = 1
				p.plusUsed = true
			}
			p.pos++
		}
		if p.pos >= len(p.tokens) {
			if p.simple && sign > 0 {
				return map[string]any{"match_none": map[string]any{}}, nil
			}
			break
		}
		q, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		if sign < 0 {
			if p.simple && !p.notOp {
				if p.operator == "AND" {
					must = append(must, q)
				} else {
					should = append(should, q)
				}
				forceMust = false
				continue
			}
			mustNot = append(mustNot, q)
			forceMust = false
			continue
		}
		if sign > 0 || forceMust {
			if forceMust && len(should) > 0 {
				must = append(must, should...)
				should = nil
			}
			must = append(must, q)
		} else if p.operator == "AND" {
			must = append(must, q)
		} else {
			should = append(should, q)
		}
		forceMust = false
	}
	if forceMust {
		return nil, errParsing("trailing operator")
	}
	bm := map[string]any{}
	if len(must) > 0 {
		bm["must"] = must
	}
	if len(should) > 0 {
		bm["should"] = should
	}
	if len(mustNot) > 0 {
		bm["must_not"] = mustNot
	}
	if len(bm) == 0 {
		return map[string]any{"match_all": map[string]any{}}, nil
	}
	return map[string]any{"bool": bm}, nil
}
func (p *qsParser) parseUnary() (map[string]any, *apiError) {
	if p.pos < len(p.tokens) {
		tok := p.tokens[p.pos]
		if tok == "NOT" || tok == "!" {
			p.pos++
			inner, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			return map[string]any{"bool": map[string]any{"must_not": []any{inner}}}, nil
		}
		if strings.HasPrefix(tok, "!") && len(tok) > 1 {
			p.tokens[p.pos] = tok[1:]
			inner, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			return map[string]any{"bool": map[string]any{"must_not": []any{inner}}}, nil
		}
		if tok == "-" || (strings.HasPrefix(tok, "-") && len(tok) > 1) {
			if tok == "-" {
				p.pos++
				inner, err := p.parseUnary()
				if err != nil {
					return nil, err
				}
				return map[string]any{"bool": map[string]any{"must_not": []any{inner}}}, nil
			}
			p.tokens[p.pos] = tok[1:]
			inner, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			return map[string]any{"bool": map[string]any{"must_not": []any{inner}}}, nil
		}
	}
	return p.parseTerm()
}

func parsePhraseToken(tok string) (string, float64, bool) {
	if len(tok) == 0 || tok[0] != '"' {
		return "", 0, false
	}
	end := -1
	for i := 1; i < len(tok); i++ {
		if tok[i] == '"' {
			end = i
			break
		}
	}
	if end < 0 {
		return strings.Trim(tok, "\""), 0, false
	}
	phrase := tok[1:end]
	slop := 0.0
	rest := tok[end+1:]
	if strings.HasPrefix(rest, "~") {
		if f, err := strconv.ParseFloat(rest[1:], 64); err == nil {
			slop = f
		}
	}
	return phrase, slop, true
}

func (p *qsParser) parseTerm() (map[string]any, *apiError) {
	if p.pos >= len(p.tokens) {
		return map[string]any{"match_all": map[string]any{}}, nil
	}
	tok := p.tokens[p.pos]
	p.pos++
	if tok == "(" {
		q, err := p.parseOr()
		if err != nil {
			return nil, err
		}
		if p.pos < len(p.tokens) && p.tokens[p.pos] == ")" {
			p.pos++
		}
		if p.pos < len(p.tokens) && strings.HasPrefix(p.tokens[p.pos], "^") {
			if f, err := strconv.ParseFloat(strings.TrimPrefix(p.tokens[p.pos], "^"), 64); err == nil {
				applyQueryBoost(q, f)
			}
			p.pos++
		}
		return q, nil
	}
	if strings.HasPrefix(tok, "_exists_:") {
		field := strings.TrimPrefix(tok, "_exists_:")
		return map[string]any{"exists": map[string]any{"field": field}}, nil
	}
	// field:value
	fieldColon := -1
	if !p.simple {
		fieldColon = strings.Index(tok, ":")
	}
	if i := fieldColon; i > 0 && !strings.HasPrefix(tok, "_") || (i > 0 && strings.HasPrefix(tok, "_") && strings.Contains(tok, ":")) {
		field := tok[:i]
		value := tok[i+1:]
		rawField := field
		if rawField != "*" && strings.ContainsAny(rawField, "*?") && !strings.Contains(rawField, "\\") {
			return nil, errParsing("invalid field wildcard")
		}
		if strings.Contains(value, ":") {
			return nil, errParsing("invalid field syntax")
		}
		field = unescapeQueryString(field)
		if strings.Contains(rawField, "\\*") || strings.Contains(rawField, "\\?") {
			field = p.default_
		} else if _, known := mappingFieldDef(p.c.idx, field); !known && field != "_id" && field != "_index" && field != "_routing" {
			if value == "*" {
				return map[string]any{"match_none": map[string]any{}}, nil
			}
			field = p.default_
		}
		if value == "" && p.pos < len(p.tokens) {
			// field:(...)
			if p.tokens[p.pos] == "(" {
				p.pos++
				q, err := p.parseOr()
				if err != nil {
					return nil, err
				}
				if p.pos < len(p.tokens) && p.tokens[p.pos] == ")" {
					p.pos++
				}
				out := p.prefixField(field, q)
				if p.pos < len(p.tokens) && strings.HasPrefix(p.tokens[p.pos], "^") {
					if f, err := strconv.ParseFloat(strings.TrimPrefix(p.tokens[p.pos], "^"), 64); err == nil {
						applyQueryBoost(out, f)
					}
					p.pos++
				}
				return out, nil
			}
		}
		if strings.HasPrefix(value, "\"") {
			for !strings.Contains(strings.TrimPrefix(value, "\""), "\"") && p.pos < len(p.tokens) {
				value += " " + p.tokens[p.pos]
				p.pos++
			}
			phrase, slop, _ := parsePhraseToken(value)
			if p.simple && !p.slopOp {
				slop = 0
			}
			if slop > 0 {
				return map[string]any{"match_phrase": map[string]any{field: map[string]any{"query": phrase, "slop": slop}}}, nil
			}
			return map[string]any{"match_phrase": map[string]any{field: phrase}}, nil
		}
		return p.valueQuery(field, value)
	}
	if strings.HasPrefix(tok, "\"") {
		phrase, slop, _ := parsePhraseToken(tok)
		if p.simple && !p.slopOp {
			slop = 0
		}
		if p.hasFields {
			if len(p.fields) == 0 {
				return map[string]any{"match_none": map[string]any{}}, nil
			}
			phraseFields := p.fields
			if p.simple && p.quoteSuffix != "" {
				phraseFields = []string{}
				for _, f := range p.fields {
					name, fb := stripFieldBoost(f)
					if fb != 1 {
						phraseFields = append(phraseFields, name+p.quoteSuffix+"^"+strconv.FormatFloat(fb, 'f', -1, 64))
					} else {
						phraseFields = append(phraseFields, name+p.quoteSuffix)
					}
				}
			}
			mq := multiFieldQuery("match_phrase", phraseFields, phrase)
			if slop > 0 {
				applyQueryPhraseSlop(mq, slop)
			}
			return mq, nil
		}
		if slop > 0 {
			return map[string]any{"match_phrase": map[string]any{p.default_: map[string]any{"query": phrase, "slop": slop}}}, nil
		}
		return map[string]any{"match_phrase": map[string]any{p.default_: phrase}}, nil
	}
	if p.simple {
		if i := strings.Index(tok, "\\-"); i > 0 {
			tok = tok[:i]
		}
		tok = strings.TrimSuffix(tok, "!")
	}
	if p.simple {
		if strings.Contains(tok, ":") && !strings.Contains(tok, "\\:") {
			return map[string]any{"match_none": map[string]any{}}, nil
		}
		target := p.fields
		if !p.hasFields {
			target = textFieldNames(p.c)
		}
		if strings.ContainsAny(tok, "*?") {
			if !p.prefixOp {
				return map[string]any{"match_none": map[string]any{}}, nil
			}
			return multiFieldQuery("wildcard", target, strings.ToLower(tok)), nil
		}
		if strings.HasPrefix(tok, "/") && strings.HasSuffix(tok, "/") && len(tok) > 1 {
			return multiFieldQuery("regexp", target, tok[1:len(tok)-1]), nil
		}
		if i := strings.LastIndex(tok, "~"); i > 0 {
			if !p.fuzzyOp {
				return map[string]any{"match_none": map[string]any{}}, nil
			}
			base := tok[:i]
			fuzz := any(2)
			if spec := tok[i+1:]; spec != "" {
				fuzz = lintValue(spec)
			}
			return multiFieldOpts("fuzzy", target, map[string]any{"value": base, "fuzziness": fuzz}), nil
		}
	}
	if p.hasFields {
		if len(p.fields) == 0 {
			return map[string]any{"match_none": map[string]any{}}, nil
		}
		return multiFieldQuery("match", p.fields, tok), nil
	}
	return p.valueQuery(p.default_, tok)
}

func (p *qsParser) prefixField(field string, q map[string]any) map[string]any {
	// rewrite inner default fields to the given field
	return rewriteQSFiels(q, field)
}

func rewriteQSFiels(q map[string]any, field string) map[string]any {
	out := map[string]any{}
	for k, v := range q {
		switch k {
		case "match", "match_phrase", "term", "wildcard", "prefix":
			out[k] = map[string]any{field: v.(map[string]any)[firstKey(v.(map[string]any))]}
		case "bool":
			bm := map[string]any{}
			for bk, bv := range v.(map[string]any) {
				switch bk {
				case "must", "should", "must_not", "filter":
					list := []any{}
					for _, e := range asQueryList(bv) {
						list = append(list, rewriteQSFiels(e.(map[string]any), field))
					}
					bm[bk] = list
				default:
					bm[bk] = bv
				}
			}
			out["bool"] = bm
		default:
			out[k] = v
		}
	}
	return out
}

func firstKey(m map[string]any) string {
	for k := range m {
		return k
	}
	return ""
}

func textFieldNames(c *queryCtx) []string {
	all := []string{}
	for f := range c.valuesMap() {
		def, ok := mappingFieldDef(c.idx, f)
		if !ok {
			continue
		}
		typ, _ := def["type"].(string)
		if typ == "text" || typ == "match_only_text" || typ == "search_as_you_type" ||
			typ == "keyword" || typ == "constant_keyword" || typ == "wildcard" || typ == "flat_object" {
			all = append(all, f)
		}
	}
	sort.Strings(all)
	return all
}

func multiFieldOpts(kind string, fields []string, opts map[string]any) map[string]any {
	shoulds := []any{}
	for _, f := range fields {
		name, _ := stripFieldBoost(f)
		inner := map[string]any{}
		for k, v := range opts {
			inner[k] = v
		}
		shoulds = append(shoulds, map[string]any{kind: map[string]any{name: inner}})
	}
	return map[string]any{"bool": map[string]any{"should": shoulds}}
}

func multiFieldQuery(kind string, fields []string, value string) map[string]any {
	shoulds := []any{}
	for _, f := range fields {
		name, _ := stripFieldBoost(f)
		shoulds = append(shoulds, map[string]any{kind: map[string]any{name: value}})
	}
	return map[string]any{"bool": map[string]any{"should": shoulds}}
}

func unescapeQueryString(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] == '\\' && i+1 < len(s) {
			i++
			b.WriteByte(s[i])
			continue
		}
		b.WriteByte(s[i])
	}
	return b.String()
}

func applyQueryBoost(q map[string]any, boost float64) map[string]any {
	for k, v := range q {
		switch k {
		case "match":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["boost"] = boost
					} else {
						bm[f] = map[string]any{"query": raw, "boost": boost}
					}
				}
			}
		case "match_phrase":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["boost"] = boost
					} else {
						bm[f] = map[string]any{"query": raw, "boost": boost}
					}
				}
			}
		case "term", "prefix", "wildcard", "regexp", "fuzzy":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["boost"] = boost
					} else {
						bm[f] = map[string]any{"value": raw, "boost": boost}
					}
				}
			}
		case "bool":
			if bm, ok := v.(map[string]any); ok {
				for _, key := range []string{"must", "should", "must_not", "filter"} {
					if list, ok := bm[key].([]any); ok {
						for _, e := range list {
							if em, ok := e.(map[string]any); ok {
								applyQueryBoost(em, boost)
							}
						}
					}
				}
			}
		}
	}
	return q
}

func (p *qsParser) valueQuery(field, value string) (map[string]any, *apiError) {
	value = unescapeQueryString(value)
	if i := strings.LastIndex(value, "^"); i > 0 {
		if f, err := strconv.ParseFloat(value[i+1:], 64); err == nil {
			q, qerr := p.valueQuery(field, value[:i])
			if qerr != nil {
				return nil, qerr
			}
			return applyQueryBoost(q, f), nil
		}
	}
	if strings.HasSuffix(value, "^") {
		return nil, errParsing("invalid boost")
	}
	if fieldUnderNested(p.c.idx, field) {
		return map[string]any{"match_none": map[string]any{}}, nil
	}
	if value == "*" {
		return map[string]any{"exists": map[string]any{"field": field}}, nil
	}
	// range syntax field:>=5 / field:[a TO b] / field:{a TO b}
	if strings.HasPrefix(value, ">") || strings.HasPrefix(value, "<") {
		op := "gt"
		if strings.HasPrefix(value, ">=") {
			op = "gte"
			value = value[2:]
		} else if strings.HasPrefix(value, "<=") {
			op = "lte"
			value = value[2:]
		} else if strings.HasPrefix(value, ">") {
			op = "gt"
			value = value[1:]
		} else {
			op = "lt"
			value = value[1:]
		}
		return map[string]any{"range": map[string]any{field: map[string]any{op: lintValue(value)}}}, nil
	}
	if strings.HasPrefix(value, "[") || strings.HasPrefix(value, "{") {
		inclusiveLow := strings.HasPrefix(value, "[")
		body := strings.Trim(value, "[]{}")
		// value may span tokens
		if !strings.Contains(body, "TO") && p.pos < len(p.tokens) {
			for p.pos < len(p.tokens) && !strings.Contains(body, "TO") {
				body += " " + p.tokens[p.pos]
				p.pos++
			}
			body = strings.Trim(body, "[]{}")
		}
		loTO := strings.Index(body, " TO ")
		if loTO < 0 {
			return nil, errParsing("failed to parse range in query string")
		}
		lo := strings.TrimSpace(body[:loTO])
		hi := strings.TrimSpace(body[loTO+4:])
		cond := map[string]any{}
		if lo != "*" && lo != "" {
			if inclusiveLow {
				cond["gte"] = lintValue(lo)
			} else {
				cond["gt"] = lintValue(lo)
			}
		}
		if hi != "*" && hi != "" {
			if strings.HasSuffix(value, "]") {
				cond["lte"] = lintValue(hi)
			} else {
				cond["lt"] = lintValue(hi)
			}
		}
		return map[string]any{"range": map[string]any{field: cond}}, nil
	}
	if len(value) >= 2 && strings.HasPrefix(value, "/") && strings.HasSuffix(value, "/") {
		pattern := value[1 : len(value)-1]
		if _, err := regexp.Compile(pattern); err != nil {
			return map[string]any{"match_none": map[string]any{}}, nil
		}
		return map[string]any{"regexp": map[string]any{field: map[string]any{"value": pattern}}}, nil
	}
	if i := strings.LastIndex(value, "~"); i > 0 {
		term := value[:i]
		spec := value[i+1:]
		fuzziness := any("AUTO")
		if spec != "" {
			fuzziness = lintValue(spec)
		}
		return map[string]any{"fuzzy": map[string]any{field: map[string]any{"value": term, "fuzziness": fuzziness}}}, nil
	}
	if strings.ContainsAny(value, "*?") {
		if !p.allowLeading && (strings.HasPrefix(value, "*") || strings.HasPrefix(value, "?")) {
			return nil, errParsing("leading wildcard disallowed")
		}
		return map[string]any{"wildcard": map[string]any{field: map[string]any{"value": strings.ToLower(value)}}}, nil
	}
	def, ok := mappingFieldDef(p.c.idx, field)
	ftype, _ := def["type"].(string)
	if !ok || ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" || ftype == "" {
		return map[string]any{"match": map[string]any{field: value}}, nil
	}
	if ftype == "date" || ftype == "date_nanos" {
		ctx := &indexCtx{idx: nil, indexCoerce: true}
		if _, err := ctx.parseDateValue("_value", ftype, def, value); err != nil {
			return map[string]any{"match_none": map[string]any{}}, nil
		}
		return map[string]any{"term": map[string]any{field: value}}, nil
	}
	if isNumericType(ftype) {
		if _, isNum := toFloat(lintValue(value)); !isNum {
			return map[string]any{"match_none": map[string]any{}}, nil
		}
	}
	return map[string]any{"term": map[string]any{field: lintValue(value)}}, nil
}

func lintValue(v string) any {
	if n, err := strconv.ParseInt(v, 10, 64); err == nil {
		return n
	}
	if f, err := strconv.ParseFloat(v, 64); err == nil {
		return f
	}
	return v
}

func boolQuery(kind string, clauses []any, msm int) map[string]any {
	bm := map[string]any{kind: clauses}
	if msm > 0 {
		bm["minimum_should_match"] = msm
	}
	return map[string]any{"bool": bm}
}

func qsParseError(query string) *apiError {
	return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
		RootCause: &apiError{Type: "query_shard_exception", Status: 400, Reason: "Failed to parse query [" + query + "]"},
		CausedBy: &apiError{Type: "query_shard_exception", Status: 400, Reason: "Failed to parse query [" + query + "]",
			CausedBy: &apiError{Type: "parse_exception", Status: 400, Reason: "Failed to parse query [" + query + "]"}}}
}

func applyQueryLenient(q map[string]any) {
	for k, v := range q {
		switch k {
		case "match":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["lenient"] = true
					} else {
						bm[f] = map[string]any{"query": raw, "lenient": true}
					}
				}
			}
		case "bool":
			if bm, ok := v.(map[string]any); ok {
				for _, key := range []string{"must", "should", "must_not", "filter"} {
					if list, ok := bm[key].([]any); ok {
						for _, e := range list {
							if em, ok := e.(map[string]any); ok {
								applyQueryLenient(em)
							}
						}
					}
				}
			}
		}
	}
}

func applyQueryPhraseSlop(q map[string]any, slop any) {
	for k, v := range q {
		switch k {
		case "match_phrase":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["slop"] = slop
					} else {
						bm[f] = map[string]any{"query": raw, "slop": slop}
					}
				}
			}
		case "bool":
			if bm, ok := v.(map[string]any); ok {
				for _, key := range []string{"must", "should", "must_not", "filter"} {
					if list, ok := bm[key].([]any); ok {
						for _, e := range list {
							if em, ok := e.(map[string]any); ok {
								applyQueryPhraseSlop(em, slop)
							}
						}
					}
				}
			}
		}
	}
}

func applyQueryFuzzyTranspositions(q map[string]any, transpositions bool) {
	for k, v := range q {
		switch k {
		case "fuzzy":
			if bm, ok := v.(map[string]any); ok {
				for f, raw := range bm {
					if opts, ok := raw.(map[string]any); ok {
						opts["transpositions"] = transpositions
					} else {
						bm[f] = map[string]any{"value": raw, "transpositions": transpositions}
					}
				}
			}
		case "bool":
			if bm, ok := v.(map[string]any); ok {
				for _, key := range []string{"must", "should", "must_not", "filter"} {
					if list, ok := bm[key].([]any); ok {
						for _, e := range list {
							if em, ok := e.(map[string]any); ok {
								applyQueryFuzzyTranspositions(em, transpositions)
							}
						}
					}
				}
			}
		}
	}
}

func (c *queryCtx) evalQueryString(body any, simple bool) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("query_string malformed")
	}
	if simple {
		allowed := map[string]bool{"query": true, "fields": true, "default_operator": true, "flags": true,
			"minimum_should_match": true, "lenient": true, "analyze_wildcard": true, "fuzzy_transpositions": true,
			"quote_field_suffix": true, "default_field": true, "auto_generate_synonyms_phrase_query": true,
			"boost": true, "_name": true, "fuzzy_prefix_length": true, "fuzzy_max_expansions": true,
			"allow_leading_wildcard": true, "phrase_slop": true, "tie_breaker": true, "type": true}
		for k := range bm {
			if !allowed[k] {
				return false, 0, errParsing("[simple_query_string] unsupported field [" + k + "]")
			}
		}
	}
	query, _ := toString(bm["query"])
	if strings.TrimSpace(query) == "" {
		return false, 0, nil
	}
	if query == "" && simple {
		query, _ = toString(bm["query"])
	}
	operator := "OR"
	if o, ok := bm["default_operator"].(string); ok {
		operator = strings.ToUpper(o)
		if operator != "AND" && operator != "OR" {
			return false, 0, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "No enum constant org.opensearch.index.query.Operator." + operator}
		}
	}
	defaultField := ""
	if df, ok := bm["default_field"].(string); ok {
		defaultField = df
	}
	if defaultField == "" {
		defaultField = "*"
	}
	var fields []string
	switch t := bm["fields"].(type) {
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				fields = append(fields, s)
			}
		}
	case string:
		fields = []string{t}
	}
	if len(fields) > 0 {
		expanded := []string{}
		for _, f := range fields {
			name, _ := stripFieldBoost(f)
			if strings.ContainsAny(name, "*?") {
				re := wildcardRegex(name)
				paths := []string{}
				for path := range c.valuesMap() {
					if !re.MatchString(path) {
						continue
					}
					def, ok := mappingFieldDef(c.idx, path)
					if !ok {
						continue
					}
					typ, _ := def["type"].(string)
					if typ == "text" || typ == "match_only_text" || typ == "search_as_you_type" ||
						typ == "keyword" || typ == "constant_keyword" || typ == "wildcard" || typ == "flat_object" {
						paths = append(paths, path)
					}
				}
				sort.Strings(paths)
				expanded = append(expanded, paths...)
			} else {
				expanded = append(expanded, f)
			}
		}
		fields = expanded
	}
	if t, _ := bm["type"].(string); t == "phrase" {
		var q map[string]any
		if len(fields) > 0 {
			q = multiFieldQuery("match_phrase", fields, query)
		} else {
			q = map[string]any{"match_phrase": map[string]any{defaultField: query}}
			if defaultField == "*" {
				q = expandMultifield(q, c)
			}
		}
		if v, has := bm["phrase_slop"]; has {
			applyQueryPhraseSlop(q, v)
		}
		return c.eval(q)
	}
	if !simple && !balancedQueryParens(query) {
		return false, 0, qsParseError(query)
	}
	allowLeading := true
	if v, ok := bm["allow_leading_wildcard"].(bool); ok {
		allowLeading = v
	}
	_, hasFields := bm["fields"]
	prefixOp, fuzzyOp, phraseOp, slopOp, notOp, andOp := true, true, true, true, true, true
	quoteSuffix, _ := bm["quote_field_suffix"].(string)
	if fs, ok := bm["flags"].(string); ok {
		allowed := map[string]bool{}
		all := false
		for _, f := range strings.Split(fs, "|") {
			switch f {
			case "ALL":
				all = true
			case "NONE":
			case "AND", "OR", "NOT", "PREFIX", "PHRASE", "PRECEDENCE", "ESCAPE", "WHITESPACE", "FUZZY", "NEAR", "SLOP":
				allowed[f] = true
			default:
				return false, 0, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Unknown simple_query_string flag [" + f + "]"}
			}
		}
		if !all {
			prefixOp = allowed["PREFIX"]
			fuzzyOp = allowed["FUZZY"]
			phraseOp = allowed["PHRASE"]
			slopOp = allowed["SLOP"]
			notOp = false
			andOp = allowed["AND"]
		}
	}
	p := &qsParser{tokens: tokenizeQueryString(query), default_: defaultField, fields: fields, operator: operator, simple: simple, c: c, allowLeading: allowLeading, hasFields: hasFields,
		prefixOp: prefixOp, fuzzyOp: fuzzyOp, phraseOp: phraseOp, slopOp: slopOp, notOp: notOp, andOp: andOp, quoteSuffix: quoteSuffix}
	q, err := p.parse()
	if err != nil {
		if simple {
			return c.eval(map[string]any{"match_none": map[string]any{}})
		}
		return false, 0, qsParseError(query)
	}
	if simple {
		if bq, ok := q["bool"].(map[string]any); ok {
			if p.plusUsed {
				if should, ok := bq["should"].([]any); ok && len(should) > 0 {
					must, _ := bq["must"].([]any)
					must = append(must, should...)
					bq["must"] = must
					delete(bq, "should")
				}
			} else if operator != "AND" {
				if mn, ok := bq["must_not"].([]any); ok {
					should, _ := bq["should"].([]any)
					for _, e := range mn {
						should = append(should, map[string]any{"bool": map[string]any{"must_not": []any{e}}})
					}
					bq["should"] = should
					delete(bq, "must_not")
				}
			}
		}
	}
	if v, has := bm["phrase_slop"]; has {
		applyQueryPhraseSlop(q, v)
	}
	if v, ok := bm["fuzzy_transpositions"].(bool); ok {
		applyQueryFuzzyTranspositions(q, v)
	}
	if v, has := bm["minimum_should_match"]; has {
		if bq, ok := q["bool"].(map[string]any); ok {
			if _, hasShould := bq["should"]; hasShould {
				bq["minimum_should_match"] = v
			}
		}
	}
	if defaultField == "*" && len(fields) == 0 {
		q = expandMultifield(q, c)
	}
	if v, ok := bm["lenient"].(bool); ok && v {
		applyQueryLenient(q)
	}
	return c.eval(q)
}

// expandMultifield rewrites queries against "*" default field into multi_match.
func expandMultifield(q map[string]any, c *queryCtx) map[string]any {
	for k, v := range q {
		switch k {
		case "match", "match_phrase":
			bm, _ := v.(map[string]any)
			field, val := "", any("")
			for f, vv := range bm {
				field = f
				val = vv
			}
			if field == "*" {
				all := []string{}
				for f := range c.valuesMap() {
					def, ok := mappingFieldDef(c.idx, f)
					if !ok {
						continue
					}
					typ, _ := def["type"].(string)
					if typ == "text" || typ == "match_only_text" || typ == "search_as_you_type" ||
						typ == "keyword" || typ == "constant_keyword" || typ == "wildcard" || typ == "flat_object" {
						all = append(all, f)
					}
				}
				sort.Strings(all)
				kind := "match"
				if k == "match_phrase" {
					kind = "match_phrase"
				}
				shoulds := []any{}
				for _, f := range all {
					shoulds = append(shoulds, map[string]any{kind: map[string]any{f: val}})
				}
				return map[string]any{"bool": map[string]any{"should": shoulds}}
			}
		case "bool":
			bm, _ := v.(map[string]any)
			for _, key := range []string{"must", "should", "must_not", "filter"} {
				if list, ok := bm[key].([]any); ok {
					out := []any{}
					for _, e := range list {
						if em, ok := e.(map[string]any); ok {
							out = append(out, expandMultifield(em, c))
						} else {
							out = append(out, e)
						}
					}
					bm[key] = out
				}
			}
		}
	}
	return q
}

func fieldUnderNested(idx *Index, field string) bool {
	parts := strings.Split(field, ".")
	props := ensureProperties(idx.Mappings)
	for i := 0; i < len(parts)-1; i++ {
		def, ok := props[parts[i]].(map[string]any)
		if !ok {
			return false
		}
		if t, _ := def["type"].(string); t == "nested" {
			return true
		}
		next, ok := def["properties"].(map[string]any)
		if !ok {
			return false
		}
		props = next
	}
	return false
}

func balancedQueryParens(s string) bool {
	depth := 0
	inQuote := false
	for _, ch := range s {
		if ch == '"' {
			inQuote = !inQuote
			continue
		}
		if inQuote {
			continue
		}
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
