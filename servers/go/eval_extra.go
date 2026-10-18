package main

import (
	"encoding/base64"
	"encoding/json"
	"math"
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

func (c *queryCtx) evalMultiMatch(bm map[string]any) (bool, float64, *apiError) {
	query, _ := toString(bm["query"])
	if query == "" {
		return false, 0, errParsing("[multi_match] requires 'query'")
	}
	typ, _ := bm["type"].(string)
	if typ == "" {
		typ = "best_fields"
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
	for _, f := range fields {
		name, _ := stripFieldBoost(f)
		if strings.ContainsAny(name, "*?") {
			re := wildcardRegex(name)
			for path, vals := range c.valuesMap() {
				if re.MatchString(path) && len(vals) > 0 {
					expanded = append(expanded, path)
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
				ok, _, _ = c.evalMatchPhrase(name, query, boost*fb, 0)
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
	default:
		for _, f := range expanded {
			name, fb := stripFieldBoost(f)
			ok, sc, err := c.evalMatch(name, query, boost*fb)
			if err != nil {
				return false, 0, err
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
		return false, 0, errParsing("[combined_fields] requires 'fields'")
	}
	operator, _ := bm["operator"].(string)
	best := 0.0
	for _, f := range fields {
		ok, sc, err := c.evalMatch(f, map[string]any{"query": query, "operator": operator}, 1.0)
		if err != nil {
			return false, 0, err
		}
		if ok && sc > best {
			best = sc
		}
	}
	if best > 0 {
		return true, best, nil
	}
	return false, 0, nil
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
// span queries (approximate, position-free)
// ---------------------------------------------------------------------------

func spanFieldTerm(c *queryCtx, body any) (string, string, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return "", "", errParsing("span query malformed")
	}
	field, val, ok := singleEntry(bm)
	if !ok {
		return "", "", errParsing("span query malformed")
	}
	term := ""
	if m, ok := val.(map[string]any); ok {
		term, _ = toString(m["value"])
	} else {
		term, _ = toString(val)
	}
	return field, term, nil
}

func (c *queryCtx) spanMatch(field, term string) bool {
	ok, _, _ := c.evalTerm(field, term, 1.0)
	return ok
}

func (c *queryCtx) evalSpanTerm(body any) (bool, float64, *apiError) {
	field, term, err := spanFieldTerm(c, body)
	if err != nil {
		return false, 0, err
	}
	if c.spanMatch(field, term) {
		return true, 1, nil
	}
	return false, 0, nil
}

func (c *queryCtx) evalSpanNear(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_near malformed")
	}
	clauses, _ := bm["clauses"].([]any)
	inOrder, _ := bm["in_order"].(bool)
	slopF, _ := toFloat(bm["slop"])
	slop := int(slopF)
	if len(clauses) == 0 {
		return false, 0, nil
	}
	phraseParts := []string{}
	field := ""
	for _, cl := range clauses {
		f, term, err := spanFieldTerm(c, cl)
		if err != nil {
			return false, 0, err
		}
		if field == "" {
			field = f
		}
		phraseParts = append(phraseParts, term)
	}
	_ = slop
	_ = inOrder
	for _, v := range c.mustValues(field) {
		s, _ := valueScalarString(v)
		tokens := analyze(s)
		if containsSubsequence(tokens, phraseParts) {
			return true, 1, nil
		}
		if !inOrder {
			allPresent := true
			for _, p := range phraseParts {
				found := false
				for _, t := range tokens {
					if t == p {
						found = true
					}
				}
				if !found {
					allPresent = false
				}
			}
			if allPresent {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalSpanOr(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_or malformed")
	}
	clauses, _ := bm["clauses"].([]any)
	for _, cl := range clauses {
		ok, _, err := c.eval(cl.(map[string]any))
		if err != nil {
			return false, 0, err
		}
		if ok {
			return true, 1, nil
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalSpanFirst(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_first malformed")
	}
	match, _ := bm["match"].(map[string]any)
	end := 1
	if e, ok := toFloat(bm["end"]); ok {
		end = int(e)
	}
	field, term, err := spanFieldTerm(c, match)
	if err != nil {
		return false, 0, err
	}
	for _, v := range c.mustValues(field) {
		s, _ := valueScalarString(v)
		tokens := analyze(s)
		for i := 0; i < end && i < len(tokens); i++ {
			if tokens[i] == term {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func (c *queryCtx) evalSpanNot(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_not malformed")
	}
	include, _ := bm["include"].(map[string]any)
	exclude, _ := bm["exclude"].(map[string]any)
	ok1, _, err := c.eval(include)
	if err != nil || !ok1 {
		return false, 0, err
	}
	ok2, _, err := c.eval(exclude)
	if err != nil {
		return false, 0, err
	}
	if ok2 {
		return false, 0, nil
	}
	return true, 1, nil
}

func (c *queryCtx) evalSpanContaining(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_containing malformed")
	}
	big, _ := bm["big"].(map[string]any)
	little, _ := bm["little"].(map[string]any)
	ok1, _, err := c.eval(big)
	if err != nil || !ok1 {
		return false, 0, err
	}
	ok2, _, err := c.eval(little)
	if err != nil {
		return false, 0, err
	}
	if !ok2 {
		return false, 0, nil
	}
	return true, 1, nil
}

func (c *queryCtx) evalSpanWithin(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_within malformed")
	}
	big, _ := bm["big"].(map[string]any)
	little, _ := bm["little"].(map[string]any)
	ok1, _, err := c.eval(little)
	if err != nil || !ok1 {
		return false, 0, err
	}
	ok2, _, err := c.eval(big)
	if err != nil {
		return false, 0, err
	}
	if !ok2 {
		return false, 0, nil
	}
	return true, 1, nil
}

func (c *queryCtx) evalSpanMulti(body any) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("span_multi malformed")
	}
	match, ok := bm["match"].(map[string]any)
	if !ok {
		return false, 0, errParsing("span_multi requires match")
	}
	return c.eval(match)
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
	allFields := map[string]any{field: rules}
	if field == "" {
		return false, 0, errParsing("intervals malformed")
	}
	return c.evalIntervalsRule(allFields)
}

func (c *queryCtx) evalIntervalsRule(rule map[string]any) (bool, float64, *apiError) {
	for kind, body := range rule {
		bm, _ := body.(map[string]any)
		switch kind {
		case "match":
			q, _ := bm["query"].(string)
			if q == "" {
				return false, 0, nil
			}
			patterns := strings.Fields(q)
			if len(patterns) > 1 {
				return c.evalIntervalsRule(map[string]any{"phrase": map[string]any{"terms": patterns}})
			}
			return c.evalTermSingle("", bm)
		case "prefix":
			q, _ := bm["prefix"].(string)
			return c.evalIntervalsSimple(func(field string, tokens []string) bool {
				for _, t := range tokens {
					if strings.HasPrefix(t, q) {
						return true
					}
				}
				return false
			}), 1, nil
		case "wildcard":
			q, _ := bm["pattern"].(string)
			re := wildcardRegex(strings.ToLower(q))
			return c.evalIntervalsSimple(func(field string, tokens []string) bool {
				for _, t := range tokens {
					if re.MatchString(t) {
						return true
					}
				}
				return false
			}), 1, nil
		case "fuzzy":
			q, _ := bm["value"].(string)
			return c.evalIntervalsSimple(func(field string, tokens []string) bool {
				for _, t := range tokens {
					if levenshtein(t, q) <= autoFuzziness(q) {
						return true
					}
				}
				return false
			}), 1, nil
		case "all_of":
			subs, _ := bm["intervals"].([]any)
			for _, sub := range subs {
				sm, _ := sub.(map[string]any)
				ok, _, err := c.evalIntervalsRule(sm)
				if err != nil || !ok {
					return false, 0, err
				}
			}
			return true, 1, nil
		case "any_of":
			subs, _ := bm["intervals"].([]any)
			for _, sub := range subs {
				sm, _ := sub.(map[string]any)
				ok, _, err := c.evalIntervalsRule(sm)
				if err == nil && ok {
					return true, 1, nil
				}
			}
			return false, 0, nil
		case "phrase":
			terms, _ := bm["terms"].([]any)
			parts := []string{}
			for _, t := range terms {
				s, _ := toString(t)
				parts = append(parts, s)
			}
			return c.evalIntervalsSimple(func(field string, tokens []string) bool {
				return containsSubsequence(tokens, parts)
			}), 1, nil
		case "containing":
			big, _ := bm["big"].(map[string]any)
			ok, _, err := c.evalIntervalsRule(big)
			return ok, 1, err
		case "not":
			inner, _ := bm["intervals"].(map[string]any)
			ok, _, err := c.evalIntervalsRule(inner)
			if err != nil {
				return false, 0, err
			}
			if ok {
				return false, 0, nil
			}
			return true, 1, nil
		default:
			return false, 0, errParsing("unknown intervals rule [" + kind + "]")
		}
	}
	return false, 0, nil
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
	tokens   []string
	pos      int
	default_ string
	fields   []string
	operator string
	simple   bool
	c        *queryCtx
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
			out = append(out, s[i:min(j+1, len(s))])
			i = j + 1
			continue
		}
		if ch == '(' || ch == ')' {
			out = append(out, string(ch))
			i++
			continue
		}
		j := i
		for j < len(s) && s[j] != ' ' && s[j] != '\t' && s[j] != '\n' {
			j++
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
		if strings.EqualFold(tok, "OR") || tok == "||" {
			p.pos++
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
	left, err := p.parseUnary()
	if err != nil {
		return nil, err
	}
	for p.pos < len(p.tokens) {
		tok := p.tokens[p.pos]
		if strings.EqualFold(tok, "AND") || tok == "&&" {
			p.pos++
			right, err := p.parseUnary()
			if err != nil {
				return nil, err
			}
			left = boolQuery("must", []any{left, right}, 0)
			continue
		}
		if strings.EqualFold(tok, "OR") || tok == "||" || tok == ")" || strings.EqualFold(tok, "NOT") {
			break
		}
		// implicit operator
		right, err := p.parseUnary()
		if err != nil {
			return nil, err
		}
		if p.operator == "AND" {
			left = boolQuery("must", []any{left, right}, 0)
		} else {
			left = boolQuery("should", []any{left, right}, 0)
		}
	}
	return left, nil
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
		if tok == "-" || (strings.HasPrefix(tok, "-") && len(tok) > 1 && !strings.Contains(tok, ":")) {
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
		return q, nil
	}
	// field:value
	if i := strings.Index(tok, ":"); i > 0 && !strings.HasPrefix(tok, "_") || (strings.HasPrefix(tok, "_") && strings.Contains(tok, ":")) {
		field := tok[:i]
		value := tok[i+1:]
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
				return p.prefixField(field, q), nil
			}
		}
		if strings.HasPrefix(value, "\"") {
			if !strings.HasSuffix(value, "\"") && p.pos < len(p.tokens) {
				for p.pos < len(p.tokens) && !strings.HasSuffix(value, "\"") {
					value += " " + p.tokens[p.pos]
					p.pos++
				}
			}
			phrase := strings.Trim(value, "\"")
			return map[string]any{"match_phrase": map[string]any{field: phrase}}, nil
		}
		return p.valueQuery(field, value)
	}
	if strings.HasPrefix(tok, "_exists_:") {
		field := strings.TrimPrefix(tok, "_exists_:")
		return map[string]any{"exists": map[string]any{"field": field}}, nil
	}
	if strings.HasPrefix(tok, "\"") {
		phrase := strings.Trim(tok, "\"")
		if len(p.fields) > 0 {
			return multiFieldQuery("match_phrase", p.fields, phrase), nil
		}
		return map[string]any{"match_phrase": map[string]any{p.default_: phrase}}, nil
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

func multiFieldQuery(kind string, fields []string, value string) map[string]any {
	shoulds := []any{}
	for _, f := range fields {
		name, _ := stripFieldBoost(f)
		shoulds = append(shoulds, map[string]any{kind: map[string]any{name: value}})
	}
	return map[string]any{"bool": map[string]any{"should": shoulds}}
}

func (p *qsParser) valueQuery(field, value string) (map[string]any, *apiError) {
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
	if strings.ContainsAny(value, "*?") {
		return map[string]any{"wildcard": map[string]any{field: map[string]any{"value": strings.ToLower(value)}}}, nil
	}
	def, ok := mappingFieldDef(p.c.idx, field)
	ftype, _ := def["type"].(string)
	if !ok || ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" || ftype == "" {
		return map[string]any{"match": map[string]any{field: value}}, nil
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

func (c *queryCtx) evalQueryString(body any, simple bool) (bool, float64, *apiError) {
	bm, ok := body.(map[string]any)
	if !ok {
		return false, 0, errParsing("query_string malformed")
	}
	query, _ := toString(bm["query"])
	if query == "" && simple {
		query, _ = toString(bm["query"])
	}
	operator := "OR"
	if o, ok := bm["default_operator"].(string); ok {
		operator = strings.ToUpper(o)
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
	p := &qsParser{tokens: tokenizeQueryString(query), default_: defaultField, fields: fields, operator: operator, simple: simple, c: c}
	q, err := p.parse()
	if err != nil {
		return false, 0, err
	}
	if defaultField == "*" && len(fields) == 0 {
		q = expandMultifield(q, c)
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
					typ, _ := mappingFieldDef(c.idx, f)
					_ = typ
					all = append(all, f)
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
