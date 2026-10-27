package main

import (
	"fmt"
	"math"
	"sort"
	"strconv"
	"strings"
)

func matrixStatsAgg(spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string) (any, *apiError) {
	fields := anyStringList(spec["fields"])
	cols := map[string][]float64{}
	for _, f := range fields {
		cols[f] = []float64{}
	}
	selected := [][]float64{}
	for _, d := range docs {
		row := []float64{}
		ok := true
		for _, f := range fields {
			v, exists := aggFieldValue(d, f, pathPrefix)
			if !exists {
				ok = false
				break
			}
			n, valid := toFloat(v)
			if !valid {
				ok = false
				break
			}
			row = append(row, n)
		}
		if !ok {
			continue
		}
		selected = append(selected, row)
		for i, f := range fields {
			cols[f] = append(cols[f], row[i])
		}
	}
	n := len(selected)
	entries := []any{}
	for _, f := range fields {
		vals := cols[f]
		mean := meanF(vals)
		variance := 0.0
		if len(vals) > 1 {
			s := 0.0
			for _, v := range vals {
				s += (v - mean) * (v - mean)
			}
			variance = s / float64(len(vals)-1)
		}
		m2, m3, m4 := 0.0, 0.0, 0.0
		for _, v := range vals {
			d := v - mean
			m2 += d * d
			m3 += d * d * d
			m4 += d * d * d * d
		}
		skew := 0.0
		kurt := 0.0
		if len(vals) > 0 && m2 != 0 {
			skew = (m3 / float64(len(vals))) / math.Pow(m2/float64(len(vals)), 1.5)
			kurt = (m4 / float64(len(vals))) / math.Pow(m2/float64(len(vals)), 2)
		}
		cov := orderedJSON{}
		corr := orderedJSON{}
		for _, g := range fields {
			if n == 0 {
				cov = append(cov, orderedKV{g, rawFloat(0)})
				corr = append(corr, orderedKV{g, rawFloat(1)})
				continue
			}
			c := 0.0
			gi := indexOfString(fields, g)
			fi := indexOfString(fields, f)
			for _, row := range selected {
				c += (row[fi] - mean) * (row[gi] - meanF(cols[g]))
			}
			if n > 1 {
				c /= float64(n - 1)
			}
			cov = append(cov, orderedKV{g, rawFloat(c)})
			vg := 0.0
			if len(cols[g]) > 1 {
				for _, v := range cols[g] {
					vg += (v - meanF(cols[g])) * (v - meanF(cols[g]))
				}
				vg /= float64(len(cols[g]) - 1)
			}
			cc := 1.0
			if variance > 0 && vg > 0 {
				cc = c / math.Sqrt(variance*vg)
			}
			corr = append(corr, orderedKV{g, rawFloat(cc)})
		}
		entries = append(entries, orderedJSON{
			{"name", f}, {"count", len(vals)}, {"mean", rawFloat(mean)}, {"variance", rawFloat(variance)},
			{"skewness", rawFloat(skew)}, {"kurtosis", rawFloat(kurt)},
			{"covariance", cov}, {"correlation", corr},
		})
	}
	return orderedJSON{{"doc_count", n}, {"fields", entries}}, nil
}

func indexOfString(xs []string, s string) int {
	for i, x := range xs {
		if x == s {
			return i
		}
	}
	return -1
}

func geoBoundsAgg(spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string) (any, *apiError) {
	field, _ := toString(spec["field"])
	lats, lons := []float64{}, []float64{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		lat, lon, ok := geoPointOf(v)
		if !ok {
			continue
		}
		lats = append(lats, lat)
		lons = append(lons, lon)
	}
	if len(lats) == 0 {
		return map[string]any{}, nil
	}
	wrap := true
	if w, ok := spec["wrap_longitude"].(bool); ok {
		wrap = w
	}
	minLat, maxLat := lats[0], lats[0]
	minLon, maxLon := lons[0], lons[0]
	for i := range lats {
		if lats[i] < minLat {
			minLat = lats[i]
		}
		if lats[i] > maxLat {
			maxLat = lats[i]
		}
		if lons[i] < minLon {
			minLon = lons[i]
		}
		if lons[i] > maxLon {
			maxLon = lons[i]
		}
	}
	if wrap && maxLon-minLon > 180 {
		shifted := make([]float64, len(lons))
		for i, lon := range lons {
			if lon < 0 {
				shifted[i] = lon + 360
			} else {
				shifted[i] = lon
			}
		}
		sm, sx := shifted[0], shifted[0]
		for _, lon := range shifted {
			if lon < sm {
				sm = lon
			}
			if lon > sx {
				sx = lon
			}
		}
		if sx > 180 {
			sx -= 360
		}
		minLon, maxLon = sm, sx
	}
	return orderedJSON{{"bounds", orderedJSON{
		{"top_left", orderedJSON{{"lat", rawFloat(maxLat)}, {"lon", rawFloat(minLon)}}},
		{"bottom_right", orderedJSON{{"lat", rawFloat(minLat)}, {"lon", rawFloat(maxLon)}}},
	}}}, nil
}

func geoCentroidAgg(spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string) (any, *apiError) {
	field, _ := toString(spec["field"])
	latSum, lonSum := 0.0, 0.0
	count := 0
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		lat, lon, ok := geoPointOf(v)
		if !ok {
			continue
		}
		latSum += lat
		lonSum += lon
		count++
	}
	out := orderedJSON{{"count", count}}
	if count > 0 {
		out = append(out, orderedKV{"location", orderedJSON{{"lat", rawFloat(latSum / float64(count))}, {"lon", rawFloat(lonSum / float64(count))}}})
	}
	return out, nil
}

func topHitsAgg(spec map[string]any, docs []map[string]any, idx *Index) (any, *apiError) {
	if sub := subAggsOf(spec); sub != nil {
		return nil, &apiError{Type: "aggregation_initialization_exception", Status: 500,
			Reason: "Aggregator [a] of type [top_hits] cannot accept sub-aggregations"}
	}
	from := intField(spec, "from", 0)
	size := intField(spec, "size", 3)
	if from+size > 100 {
		reason := fmt.Sprintf("Top hits result window is too large, the top hits aggregator [a]'s from + size must be less than or equal to: [100] but was [%d]. This limit can be set by changing the [index.max_inner_result_window] index level setting.", from+size)
		return nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	sorts := parseSortList(spec["sort"])
	if len(sorts) > 0 {
		sort.SliceStable(docs, func(i, j int) bool {
			for _, s := range sorts {
				av, aok := sortValue(docs[i], s.field, idx)
				bv, bok := sortValue(docs[j], s.field, idx)
				if !aok || !bok {
					if aok != bok {
						return aok
					}
					continue
				}
				cmp := compareAggSortValues(av, bv)
				if cmp != 0 {
					if s.desc {
						return cmp > 0
					}
					return cmp < 0
				}
			}
			return false
		})
	}
	total := len(docs)
	start := from
	if start > total {
		start = total
	}
	end := start + size
	if end > total {
		end = total
	}
	trackScores, _ := spec["track_scores"].(bool)
	includeSource, sourceIncludes, sourceExcludes := parseTopHitsSource(spec["_source"])
	storedNone := false
	_, sourceExplicit := spec["_source"]
	if sf, ok := spec["stored_fields"]; ok {
		names := anyStringList(sf)
		for _, n := range names {
			if n == "_none_" {
				storedNone = true
			}
		}
		if storedNone && !sourceExplicit {
			includeSource = false
		}
	}
	hits := []any{}
	maxScore := 0.0
	hasScores := len(sorts) == 0 || trackScores
	for _, d := range docs {
		sc, _ := toFloat(d["__slop_score"])
		if sc > maxScore {
			maxScore = sc
		}
	}
	for i := start; i < end; i++ {
		d := docs[i]
		id, _ := d["__slop_id"].(string)
		sc, _ := toFloat(d["__slop_score"])
		h := orderedJSON{}
		if !storedNone {
			h = append(h, orderedKV{"_index", idx.Name}, orderedKV{"_id", id})
		}
		if hasScores {
			h = append(h, orderedKV{"_score", rawFloat(sc)})
		} else {
			h = append(h, orderedKV{"_score", nil})
		}
		if includeSource {
			src := filterSourceDoc(d, sourceIncludes, sourceExcludes)
			delete(src, "__slop_id")
			delete(src, "__slop_score")
			h = append(h, orderedKV{"_source", normalizeNumbers(src)})
		}
		if v, ok := spec["version"].(bool); ok && v {
			ver := int64(1)
			if idx.Versions != nil {
				if x, ok := idx.Versions[id]; ok {
					ver = x
				}
			}
			h = append(h, orderedKV{"_version", ver})
		}
		if v, ok := spec["seq_no_primary_term"].(bool); ok && v {
			seq := int64(0)
			if idx.SeqNos != nil {
				if x, ok := idx.SeqNos[id]; ok {
					seq = x
				}
			}
			pt := int64(1)
			if idx.PrimaryTerms != nil {
				if x, ok := idx.PrimaryTerms[id]; ok {
					pt = x
				}
			}
			h = append(h, orderedKV{"_seq_no", seq}, orderedKV{"_primary_term", pt})
		}
		if len(sorts) > 0 {
			sv := []any{}
			for _, s := range sorts {
				v, _ := sortValue(d, s.field, idx)
				sv = append(sv, normalizeNumbers(v))
			}
			h = append(h, orderedKV{"sort", sv})
		}
		fields := orderedJSON{}
		if dv, ok := spec["docvalue_fields"]; ok {
			for _, f := range anyStringList(dv) {
				if v, ok := fieldValue(d, f); ok {
					fields = append(fields, orderedKV{f, []any{normalizeNumbers(v)}})
				}
			}
		}
		if sf, ok := spec["script_fields"].(map[string]any); ok {
			for name, raw := range sf {
				sm, _ := raw.(map[string]any)
				src, params := scriptText(sm["script"])
				qc := newQueryCtxForDoc(idx, id, d)
				env := &scriptEnv{c: qc, params: params}
				if r, err := env.evalScriptExpression(src); err == nil {
					fields = append(fields, orderedKV{name, []any{normalizeNumbers(r)}})
				}
			}
		}
		if len(fields) > 0 {
			h = append(h, orderedKV{"fields", fields})
		}
		if hm, ok := spec["highlight"].(map[string]any); ok {
			if hf, ok := hm["fields"].(map[string]any); ok {
				terms, _ := d["__slop_query_terms"].([]string)
				hl := orderedJSON{}
				for fname := range hf {
					txt, ok := toString(d[fname])
					if !ok {
						continue
					}
					hl = append(hl, orderedKV{fname, []any{highlightText(txt, terms)}})
				}
				if len(hl) > 0 {
					h = append(h, orderedKV{"highlight", hl})
				}
			}
		}
		hits = append(hits, h)
	}
	var max any
	if hasScores && total > 0 {
		max = rawFloat(maxScore)
	}
	return orderedJSON{{"hits", orderedJSON{
		{"total", orderedJSON{{"value", total}, {"relation", "eq"}}},
		{"max_score", max},
		{"hits", hits},
	}}}, nil
}

type sortSpec struct {
	field string
	desc  bool
}

func parseSortList(raw any) []sortSpec {
	out := []sortSpec{}
	switch t := raw.(type) {
	case []any:
		for _, e := range t {
			out = append(out, parseSortEntry(e)...)
		}
	default:
		out = append(out, parseSortEntry(raw)...)
	}
	return out
}

func parseSortEntry(raw any) []sortSpec {
	switch t := raw.(type) {
	case string:
		return []sortSpec{{field: t}}
	case map[string]any:
		out := []sortSpec{}
		for k, v := range t {
			s := sortSpec{field: k}
			switch vv := v.(type) {
			case string:
				s.desc = vv == "desc"
			case map[string]any:
				if o, _ := toString(vv["order"]); o == "desc" {
					s.desc = true
				}
			}
			out = append(out, s)
		}
		return out
	}
	return nil
}

func sortValue(doc map[string]any, field string, idx *Index) (any, bool) {
	if field == "_score" {
		return doc["__slop_score"], true
	}
	v, ok := fieldValue(doc, field)
	if !ok {
		return nil, false
	}
	def, _ := mappingFieldDef(idx, field)
	ft, _ := def["type"].(string)
	if ft == "date" || ft == "date_nanos" {
		if ms, ok := dateMillisFor(idx, field, ft, v); ok {
			return float64(ms), true
		}
	}
	return v, true
}

func compareAggSortValues(a, b any) int {
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	if aok && bok {
		if af < bf {
			return -1
		}
		if af > bf {
			return 1
		}
		return 0
	}
	as, _ := toString(a)
	bs, _ := toString(b)
	if as < bs {
		return -1
	}
	if as > bs {
		return 1
	}
	return 0
}

func parseTopHitsSource(raw any) (bool, []string, []string) {
	if raw == nil {
		return true, nil, nil
	}
	switch t := raw.(type) {
	case bool:
		return t, nil, nil
	case string:
		if t == "false" {
			return false, nil, nil
		}
		if t == "true" || t == "" {
			return true, nil, nil
		}
		return true, splitSourceList(t), nil
	case []any:
		return true, anyStringList(t), nil
	case map[string]any:
		inc := anyStringList(t["includes"])
		if len(inc) == 0 {
			inc = anyStringList(t["include"])
		}
		exc := anyStringList(t["excludes"])
		if len(exc) == 0 {
			exc = anyStringList(t["exclude"])
		}
		return true, inc, exc
	}
	return true, nil, nil
}

func scriptedMetricAgg(spec map[string]any, docs []map[string]any, idx *Index) (any, *apiError) {
	mapScript, ok := spec["map_script"]
	if !ok {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Required [map_script]"}
	}
	if _, ok := spec["combine_script"]; !ok {
		return nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "[combineScript] must not be null: [a]",
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: "[combineScript] must not be null: [a]"}}
	}
	if _, ok := spec["reduce_script"]; !ok {
		return nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "[reduceScript] must not be null: [a]",
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: "[reduceScript] must not be null: [a]"}}
	}
	params, _ := spec["params"].(map[string]any)
	state := map[string]any{}
	if init, ok := spec["init_script"]; ok {
		src, ip := scriptText(init)
		if len(ip) > 0 {
			params = mergeParams(params, ip)
		}
		applyStateScript(state, src, nil, idx, params)
	}
	for _, d := range docs {
		src, _ := scriptText(mapScript)
		applyStateScript(state, src, d, idx, params)
	}
	combine, _ := scriptText(spec["combine_script"])
	combined := evalCombine(combine, state, idx)
	reduce, _ := scriptText(spec["reduce_script"])
	states := []any{combined}
	res := evalReduce(reduce, states)
	return orderedJSON{{"value", normalizeNumbers(res)}}, nil
}

func mergeParams(a, b map[string]any) map[string]any {
	out := map[string]any{}
	for k, v := range a {
		out[k] = v
	}
	for k, v := range b {
		out[k] = v
	}
	return out
}

func applyStateScript(state map[string]any, src string, doc map[string]any, idx *Index, params map[string]any) {
	src = strings.TrimSpace(src)
	if strings.HasPrefix(src, "state.") {
		rest := strings.TrimPrefix(src, "state.")
		if i := strings.Index(rest, "="); i > 0 {
			key := strings.TrimSpace(rest[:i])
			op := "="
			if strings.HasSuffix(key, "+") {
				key = strings.TrimSpace(strings.TrimSuffix(key, "+"))
				op = "+="
			}
			expr := strings.TrimSpace(rest[i+1:])
			val := evalStateExpr(expr, doc, idx, params)
			switch op {
			case "+=":
				old, _ := toFloat(state[key])
				add, _ := toFloat(val)
				state[key] = old + add
			default:
				state[key] = val
			}
			return
		}
		if strings.HasSuffix(rest, ".add(") || strings.Contains(rest, ".add(") {
			key := strings.TrimSpace(rest[:strings.Index(rest, ".add(")])
			arg := rest[strings.Index(rest, ".add(")+5:]
			arg = strings.TrimSuffix(arg, ")")
			if arr, ok := state[key].([]any); ok {
				state[key] = append(arr, evalStateExpr(arg, doc, idx, params))
			}
		}
	}
}

func evalStateExpr(expr string, doc map[string]any, idx *Index, params map[string]any) any {
	expr = strings.TrimSpace(expr)
	if expr == "[]" {
		return []any{}
	}
	id, _ := doc["__slop_id"].(string)
	qc := newQueryCtxForDoc(idx, id, doc)
	env := &scriptEnv{c: qc, params: params}
	if r, err := env.evalScriptExpression(expr); err == nil {
		return normalizeNumbers(r)
	}
	if n, err := strconv.ParseFloat(expr, 64); err == nil {
		return n
	}
	return nil
}

func evalCombine(src string, state map[string]any, idx *Index) any {
	src = strings.TrimSpace(src)
	if strings.HasPrefix(src, "return state") {
		if src == "return state" {
			return state
		}
		key := strings.TrimPrefix(src, "return state.")
		key = strings.TrimSpace(key)
		return state[key]
	}
	if strings.Contains(src, "for (") && strings.Contains(src, "+=") {
		total := 0.0
		for _, v := range state {
			if arr, ok := v.([]any); ok {
				for _, e := range arr {
					f, _ := toFloat(e)
					total += f
				}
			}
		}
		if strings.Contains(src, "double ") || strings.Contains(src, "float ") {
			return total
		}
		return int64(total)
	}
	return nil
}

func evalReduce(src string, states []any) any {
	src = strings.TrimSpace(src)
	if src == "return states" {
		return states
	}
	if src == "return states.size()" {
		return len(states)
	}
	if strings.Contains(src, "r.addAll(s)") {
		r := []any{}
		for _, s := range states {
			if arr, ok := s.([]any); ok {
				r = append(r, arr...)
			}
		}
		sort.Slice(r, func(i, j int) bool {
			a, _ := toFloat(r[i])
			b, _ := toFloat(r[j])
			return a < b
		})
		return orderedJSON{{"vals", r}, {"n", len(r)}}
	}
	if strings.Contains(src, "for (") && strings.Contains(src, "+=") {
		total := 0.0
		isInt := strings.Contains(src, "long ") || strings.Contains(src, "int ")
		for _, s := range states {
			f, _ := toFloat(s)
			total += f
		}
		if isInt {
			return int64(total)
		}
		return total
	}
	return nil
}
