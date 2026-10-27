package main

import (
	"encoding/json"
	"fmt"
	"math"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
	_ "time/tzdata"
)

func resolveField(field, pathPrefix string) string {
	if pathPrefix != "" && strings.HasPrefix(field, pathPrefix+".") {
		return strings.TrimPrefix(field, pathPrefix+".")
	}
	return field
}

func aggFieldValue(doc map[string]any, field, pathPrefix string) (any, bool) {
	return fieldValue(doc, resolveField(field, pathPrefix))
}

func numericValues(docs []map[string]any, field, pathPrefix string) []float64 {
	out := []float64{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		if f, ok := toFloat(v); ok {
			out = append(out, f)
		}
	}
	return out
}

type aggHit struct {
	idx   *Index
	id    string
	doc   map[string]any
	score float64
	terms []string
}

func aggDocs(hits []aggHit) []map[string]any {
	out := make([]map[string]any, 0, len(hits))
	for _, h := range hits {
		d := deepCopySource(h.doc).(map[string]any)
		if h.id != "" {
			d["__slop_id"] = h.id
			d["__slop_score"] = h.score
		}
		if len(h.terms) > 0 {
			d["__slop_query_terms"] = h.terms
		}
		out = append(out, d)
	}
	return out
}

func evalAggs(aggs map[string]any, hits []aggHit, idx *Index, pathPrefix string, typed bool) (map[string]any, *apiError) {
	return evalAggsMode(aggs, hits, idx, pathPrefix, typed, true)
}

func evalAggsMode(aggs map[string]any, hits []aggHit, idx *Index, pathPrefix string, typed bool, allowParent bool) (map[string]any, *apiError) {
	docs := aggDocs(hits)
	out := map[string]any{}
	for name, raw := range aggs {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		if strings.ContainsAny(name, "[]>") {
			return nil, &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Invalid aggregation name [" + name + "]. Aggregation names can contain any character except '[', ']', and '>'"}
		}
		if len(def) == 0 {
			return nil, &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Missing definition for aggregation [" + name + "]"}
		}
		if types := aggDefinitionTypes(def); len(types) > 1 {
			return nil, &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Found two aggregation type definitions in [" + name + "]: [" + types[0] + "] and [" + types[1] + "]"}
		}
		t := aggTypeOf(def)
		if isPipelineType(t) {
			continue
		}
		res, err := evalOneAgg(name, def, docs, idx, pathPrefix, typed)
		if err != nil {
			return nil, err
		}
		if meta, ok := def["meta"]; ok {
			if m, ok := res.(map[string]any); ok {
				m["meta"] = meta
			}
		}
		key := name
		if typed {
			if p := typedAggPrefix(def, idx, pathPrefix); p != "" {
				key = p + "#" + name
			}
		}
		out[key] = res
	}
	for name, raw := range aggs {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		t := aggTypeOf(def)
		if isParentPipelineType(t) {
			if !allowParent {
				continue
			}
			if t == "bucket_script" {
				return nil, &apiError{Type: "action_request_validation_exception", Status: 400,
					Reason: "Validation Failed: 1: bucket_script aggregation [" + name + "] must be declared inside of another aggregation;"}
			}
			return nil, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: No aggregation found for path [" + bucketPathsOf(def[t].(map[string]any))[0] + "];"}
		}
		if !isSiblingPipelineType(t) {
			continue
		}
		res, err := evalSiblingPipeline(name, def[t].(map[string]any), t, out)
		if err != nil {
			return nil, err
		}
		out[name] = res
	}
	return out, nil
}

func typedAggPrefix(def map[string]any, idx *Index, pathPrefix string) string {
	if t, ok := def["terms"].(map[string]any); ok {
		field, _ := toString(t["field"])
		switch fieldType(idx, resolveField(field, pathPrefix)) {
		case "byte", "short", "integer", "long", "unsigned_long", "date", "date_nanos", "boolean":
			return "lterms"
		case "float", "double", "half_float", "scaled_float":
			return "dterms"
		}
		return "sterms"
	}
	if t, ok := def["rare_terms"].(map[string]any); ok {
		field, _ := toString(t["field"])
		switch fieldType(idx, resolveField(field, pathPrefix)) {
		case "byte", "short", "integer", "long", "unsigned_long", "date", "date_nanos", "boolean":
			return "lrareterms"
		case "float", "double", "half_float", "scaled_float":
			return "drareterms"
		}
		return "srareterms"
	}
	prefixes := map[string]string{
		"significant_terms": "sigsterms", "significant_text": "sigsterms",
		"multi_terms": "multi_terms", "histogram": "histogram",
		"date_histogram": "date_histogram", "auto_date_histogram": "auto_date_histogram",
		"variable_width_histogram": "variable_width_histogram",
		"range":                    "range", "date_range": "date_range", "ip_range": "ip_range",
		"filter": "filter", "filters": "filters", "missing": "missing",
		"global": "global", "nested": "nested", "composite": "composite",
		"sampler": "sampler", "diversified_sampler": "sampler",
		"reverse_nested": "reverse_nested", "adjacency_matrix": "adjacency_matrix",
		"geohash_grid": "geohash_grid", "geotile_grid": "geotile_grid",
		"geo_distance": "geo_distance", "avg": "avg", "min": "min", "max": "max",
		"sum": "sum", "stats": "stats", "cardinality": "cardinality",
		"value_count": "value_count", "percentiles": "percentiles",
		"percentile_ranks": "percentile_ranks", "extended_stats": "extended_stats",
		"weighted_avg": "weighted_avg", "top_hits": "top_hits",
		"scripted_metric": "scripted_metric", "matrix_stats": "matrix_stats",
	}
	for k, p := range prefixes {
		if _, ok := def[k]; ok {
			return p
		}
	}
	return ""
}

func aggDefinitionTypes(def map[string]any) []string {
	out := []string{}
	for k := range def {
		switch k {
		case "aggs", "aggregations", "meta":
			continue
		}
		if knownAggTypes[k] || isPipelineType(k) || isSiblingPipelineType(k) || isParentPipelineType(k) {
			out = append(out, k)
		}
	}
	sort.SliceStable(out, func(i, j int) bool {
		rank := func(s string) int {
			switch s {
			case "terms":
				return 0
			case "histogram":
				return 2
			}
			return 1
		}
		return rank(out[i]) < rank(out[j])
	})
	return out
}

func subAggsOf(def map[string]any) map[string]any {
	if sub, ok := def["aggs"].(map[string]any); ok {
		return sub
	}
	if sub, ok := def["aggregations"].(map[string]any); ok {
		return sub
	}
	return nil
}

func evalOneAgg(name string, def map[string]any, docs []map[string]any, idx *Index, pathPrefix string, typed bool) (any, *apiError) {
	if t, ok := def["terms"].(map[string]any); ok {
		return termsAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["significant_terms"].(map[string]any); ok {
		return significantAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed, false)
	}
	if t, ok := def["significant_text"].(map[string]any); ok {
		return significantAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed, true)
	}
	if t, ok := def["rare_terms"].(map[string]any); ok {
		return rareTermsAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["date_histogram"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return dateHistogramAgg(field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["histogram"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return histogramAgg(name, field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["filter"].(map[string]any); ok {
		return filterAgg(t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["nested"].(map[string]any); ok {
		path, _ := toString(t["path"])
		return nestedAgg(path, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["reverse_nested"].(map[string]any); ok {
		path, _ := toString(t["path"])
		return reverseNestedAgg(name, path, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["children"].(map[string]any); ok {
		typ, _ := toString(t["type"])
		return childrenAgg(typ, docs, idx, subAggsOf(def), typed)
	}
	if t, ok := def["parent"].(map[string]any); ok {
		typ, _ := toString(t["type"])
		return parentAgg(typ, docs, idx, subAggsOf(def), typed)
	}
	if t, ok := def["avg"].(map[string]any); ok {
		if err := validateMetricParams("avg", t, commonMetricParams); err != nil {
			return nil, err
		}
		field, _ := toString(t["field"])
		if err := metricUnsupported(idx, field, "avg", pathPrefix); err != nil {
			return nil, err
		}
		vals, err := metricValues(t, docs, idx, pathPrefix)
		if err != nil {
			return nil, err
		}
		return metricValueObject(meanF(vals), len(vals) > 0, t, idx, field), nil
	}
	if t, ok := def["min"].(map[string]any); ok {
		if err := validateMetricParams("min", t, commonMetricParams); err != nil {
			return nil, err
		}
		field, _ := toString(t["field"])
		if err := metricUnsupported(idx, field, "min", pathPrefix); err != nil {
			return nil, err
		}
		vals, err := metricValues(t, docs, idx, pathPrefix)
		if err != nil {
			return nil, err
		}
		return metricValueObject(minF(vals), len(vals) > 0, t, idx, field), nil
	}
	if t, ok := def["max"].(map[string]any); ok {
		if err := validateMetricParams("max", t, commonMetricParams); err != nil {
			return nil, err
		}
		field, _ := toString(t["field"])
		if err := metricUnsupported(idx, field, "max", pathPrefix); err != nil {
			return nil, err
		}
		vals, err := metricValues(t, docs, idx, pathPrefix)
		if err != nil {
			return nil, err
		}
		return metricValueObject(maxF(vals), len(vals) > 0, t, idx, field), nil
	}
	if t, ok := def["sum"].(map[string]any); ok {
		if err := validateMetricParams("sum", t, commonMetricParams); err != nil {
			return nil, err
		}
		field, _ := toString(t["field"])
		if err := metricUnsupported(idx, field, "sum", pathPrefix); err != nil {
			return nil, err
		}
		vals, err := metricValues(t, docs, idx, pathPrefix)
		if err != nil {
			return nil, err
		}
		return metricValueObject(sumOf(vals), true, t, idx, field), nil
	}
	if t, ok := def["value_count"].(map[string]any); ok {
		if err := validateMetricParams("value_count", t, commonMetricParams); err != nil {
			return nil, err
		}
		return map[string]any{"value": countValues(t, docs, idx, pathPrefix)}, nil
	}
	if t, ok := def["stats"].(map[string]any); ok {
		if err := validateMetricParams("stats", t, commonMetricParams); err != nil {
			return nil, err
		}
		return statsAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["extended_stats"].(map[string]any); ok {
		allowed := map[string]bool{"field": true, "missing": true, "script": true, "format": true, "sigma": true}
		if err := validateMetricParams("extended_stats", t, allowed); err != nil {
			return nil, err
		}
		if v, ok := toFloat(t["sigma"]); ok && v < 0 {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [a]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: fmt.Sprintf("[sigma] must be greater than or equal to 0. Found [%s] in [a]", formatFloatOne(v))}}
		}
		return extendedStatsAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["cardinality"].(map[string]any); ok {
		allowed := map[string]bool{"field": true, "missing": true, "script": true, "precision_threshold": true, "format": true}
		if err := validateMetricParams("cardinality", t, allowed); err != nil {
			return nil, err
		}
		if v, ok := toFloat(t["precision_threshold"]); ok && v < 0 {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [a]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: fmt.Sprintf("[precisionThreshold] must be greater than or equal to 0. Found [%s] in [a]", formatNumber(v))}}
		}
		return cardinalityAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["percentiles"].(map[string]any); ok {
		return percentilesAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["percentile_ranks"].(map[string]any); ok {
		return percentileRanksAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["median_absolute_deviation"].(map[string]any); ok {
		if err := validateMetricParams("median_absolute_deviation", t, commonMetricParams); err != nil {
			return nil, err
		}
		return medianAbsoluteDeviation(t, docs, idx, pathPrefix)
	}
	if t, ok := def["weighted_avg"].(map[string]any); ok {
		allowed := map[string]bool{"value": true, "weight": true, "format": true, "missing": true}
		if err := validateMetricParams("weighted_avg", t, allowed); err != nil {
			return nil, err
		}
		return weightedAvgAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["matrix_stats"].(map[string]any); ok {
		return matrixStatsAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["geo_bounds"].(map[string]any); ok {
		return geoBoundsAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["geo_centroid"].(map[string]any); ok {
		return geoCentroidAgg(t, docs, idx, pathPrefix)
	}
	if t, ok := def["top_hits"].(map[string]any); ok {
		if subAggsOf(def) != nil {
			return nil, &apiError{Type: "aggregation_initialization_exception", Status: 500,
				Reason: "Aggregator [a] of type [top_hits] cannot accept sub-aggregations"}
		}
		return topHitsAgg(t, docs, idx)
	}
	if t, ok := def["scripted_metric"].(map[string]any); ok {
		return scriptedMetricAgg(t, docs, idx)
	}
	if _, ok := def["missing"].(map[string]any); ok {
		t := def["missing"].(map[string]any)
		field, _ := toString(t["field"])
		return missingAgg(field, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["global"].(map[string]any); ok {
		if len(t) > 0 {
			return nil, &apiError{Type: "parsing_exception", Status: 400, Reason: "Unknown key for a VALUE_OBJECT in [global]."}
		}
		return globalAgg(subAggsOf(def), idx, pathPrefix, typed)
	}
	if t, ok := def["filters"].(map[string]any); ok {
		return filtersAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["range"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return rangeAggSpec(name, field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["date_range"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return dateRangeAgg(name, field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["ip_range"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return ipRangeAgg(name, field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["multi_terms"].(map[string]any); ok {
		return multiTermsAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["composite"].(map[string]any); ok {
		return compositeAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["geohash_grid"].(map[string]any); ok {
		return geoHashGridAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed, false)
	}
	if t, ok := def["geotile_grid"].(map[string]any); ok {
		return geoHashGridAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed, true)
	}
	if t, ok := def["geo_distance"].(map[string]any); ok {
		return geoDistanceAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["adjacency_matrix"].(map[string]any); ok {
		return adjacencyMatrixAgg(name, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["sampler"].(map[string]any); ok {
		return samplerAgg(t, docs, idx, pathPrefix, subAggsOf(def), typed, false)
	}
	if t, ok := def["diversified_sampler"].(map[string]any); ok {
		return samplerAgg(t, docs, idx, pathPrefix, subAggsOf(def), typed, true)
	}
	if t, ok := def["auto_date_histogram"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return autoDateHistogramAgg(field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if t, ok := def["variable_width_histogram"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return variableWidthHistogramAgg(field, t, docs, idx, pathPrefix, subAggsOf(def), typed)
	}
	if aggName := unknownAggName(def); aggName != "" {
		reason := "Unknown aggregation type [" + aggName + "]"
		if s := aggSuggestion(aggName); s != "" {
			reason += " did you mean [" + s + "]?"
		}
		return nil, &apiError{Type: "parsing_exception", Status: 400, Reason: reason,
			CausedBy: &apiError{Type: "named_object_not_found_exception", Reason: "[1:14] not found"}}
	}
	return map[string]any{}, nil
}

type termBucket struct {
	key    any
	kas    string
	hasKas bool
	count  int
	docs   []map[string]any
	canon  string
}

func flattenTermValues(v any) []any {
	switch t := v.(type) {
	case []any:
		out := []any{}
		for _, e := range t {
			out = append(out, flattenTermValues(e)...)
		}
		return out
	}
	return []any{v}
}

func canonicalTermKey(v any) string {
	switch t := v.(type) {
	case int64:
		return "i:" + strconv.FormatInt(t, 10)
	case float64:
		return "f:" + strconv.FormatFloat(t, 'g', -1, 64)
	case string:
		return "s:" + t
	case bool:
		if t {
			return "b:1"
		}
		return "b:0"
	}
	return fmt.Sprintf("x:%v", v)
}

func termKeyFor(v any, ft, valueType, format string, idx *Index, field string) (any, string, bool, string, bool) {
	switch ft {
	case "integer", "long", "short", "byte", "unsigned_long":
		if f, ok := toFloat(v); ok {
			k := int64(f)
			return k, "", false, canonicalTermKey(k), true
		}
		if valueType == "long" {
			return v, "", false, canonicalTermKey(v), true
		}
	case "float", "double", "half_float", "scaled_float":
		if f, ok := toFloat(v); ok {
			return f, "", false, canonicalTermKey(f), true
		}
	case "boolean":
		if b, ok := v.(bool); ok {
			if b {
				return int64(1), "true", true, "b:1", true
			}
			return int64(0), "false", true, "b:0", true
		}
		if f, ok := toFloat(v); ok {
			if f != 0 {
				return int64(1), "true", true, "b:1", true
			}
			return int64(0), "false", true, "b:0", true
		}
	case "date", "date_nanos":
		ms, ok := dateMillisFor(idx, field, ft, v)
		if !ok {
			return nil, "", false, "", false
		}
		kas := time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")
		if format != "" {
			if layout := javaDateFormatToGo(format); layout != "" {
				kas = time.UnixMilli(ms).UTC().Format(layout)
			}
		}
		return ms, kas, true, canonicalTermKey(ms), true
	case "":
		if valueType == "long" {
			if f, ok := toFloat(v); ok {
				k := int64(f)
				return k, "", false, canonicalTermKey(k), true
			}
		}
		if s, ok := toString(v); ok {
			return s, "", false, canonicalTermKey(s), true
		}
	}
	if s, ok := toString(v); ok {
		return s, "", false, canonicalTermKey(s), true
	}
	return nil, "", false, "", false
}

func termsAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	allowed := map[string]bool{"field": true, "size": true, "order": true, "min_doc_count": true,
		"shard_size": true, "shard_min_doc_count": true, "include": true, "exclude": true,
		"missing": true, "value_type": true, "format": true, "script": true, "execution_hint": true,
		"collect_mode": true, "show_term_doc_count_error": true}
	for k := range spec {
		if !allowed[k] {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[terms] unknown field [" + k + "]"}
		}
	}
	field, _ := toString(spec["field"])
	resolved := resolveField(field, pathPrefix)
	ft, mapped := idx.FieldTypes[resolved]
	if !mapped {
		ft = ""
	}
	if ft == "text" {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [" + field + "] in order to load field data by uninverting the inverted index."}
	}
	size := intField(spec, "size", 10)
	if size <= 0 {
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "Failed to parse aggregation [" + name + "]",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: fmt.Sprintf("[size] must be greater than 0. Found [%d] in [%s]", size, name)}}
	}
	if hint, ok := toString(spec["execution_hint"]); ok && hint != "" && hint != "map" && hint != "global_ordinals" {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Unknown `execution_hint`: [" + hint + "], expected any of [map, global_ordinals]"}
	}
	if mode, ok := toString(spec["collect_mode"]); ok && mode != "" && mode != "breadth_first" && mode != "depth_first" {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "no [collect_mode] found for value [" + mode + "]"}
	}
	valueType, _ := toString(spec["value_type"])
	format, _ := toString(spec["format"])
	missingRaw, hasMissing := spec["missing"]
	inc, hasInc := spec["include"]
	exc, hasExc := spec["exclude"]
	incRegex, excRegex, incSet, excSet, partFilter, err := termsFilters(inc, hasInc, exc, hasExc, ft, idx, resolved, valueType)
	if err != nil {
		return nil, err
	}
	if err := globalSubError(name, sub); err != nil {
		return nil, err
	}
	groups := map[string]*termBucket{}
	orderKeys := []string{}
	addVal := func(v any, doc map[string]any, missingUsed bool) {
		if v == nil {
			return
		}
		key, kas, hasKas, canon, ok := termKeyFor(v, ft, valueType, format, idx, resolved)
		if !ok {
			return
		}
		if !missingUsed {
			if incRegex != nil && !incRegex.MatchString(fmt.Sprint(key)) {
				return
			}
			if excRegex != nil && excRegex.MatchString(fmt.Sprint(key)) {
				return
			}
			if incSet != nil && !incSet[canon] {
				return
			}
			if excSet != nil && excSet[canon] {
				return
			}
		}
		if partFilter != nil && !partFilter(key) {
			return
		}
		tb := groups[canon]
		if tb == nil {
			tb = &termBucket{key: key, kas: kas, hasKas: hasKas, canon: canon}
			groups[canon] = tb
			orderKeys = append(orderKeys, canon)
		}
		tb.count++
		tb.docs = append(tb.docs, doc)
	}
	scriptSpec, hasScript := spec["script"]
	joinField, _, hasJoin := findJoinField(idx)
	joinType := ""
	isJoinField := false
	if hasJoin {
		if field == joinField {
			isJoinField = true
		} else if strings.HasPrefix(field, joinField+"#") {
			isJoinField = true
			joinType = strings.TrimPrefix(field, joinField+"#")
		}
	}
	for _, d := range docs {
		if isJoinField {
			id, _ := d["__slop_id"].(string)
			src := idx.Docs[id]
			if src == nil {
				src = d
			}
			name, parent := joinParts(src[joinField])
			if joinType == "" {
				if name != "" {
					addVal(name, d, false)
				}
			} else if parent != "" {
				addVal(parent, d, false)
			} else if name == joinType && id != "" {
				addVal(id, d, false)
			}
			continue
		}
		raw, ok := aggFieldValue(d, field, pathPrefix)
		var vals []any
		if ok {
			vals = flattenTermValues(raw)
		}
		if hasScript {
			if field != "" && len(vals) > 0 {
				src, _ := scriptText(scriptSpec)
				for _, v := range vals {
					if f, ok := toFloat(v); ok {
						if strings.Contains(src, "_value") {
							val := strings.ReplaceAll(src, "_value", strconv.FormatFloat(f, 'f', -1, 64))
							if r, e := (&scriptEnv{}).evalScriptExpression(val); e == nil {
								if f, ok := toFloat(r); ok {
									addVal(f, d, false)
								}
							}
						}
					}
				}
				continue
			}
			if r, ok := evalTermScript(d, idx, scriptSpec); ok {
				addVal(r, d, false)
			}
			continue
		}
		if len(vals) == 0 {
			if hasMissing {
				addVal(missingRaw, d, true)
			}
			continue
		}
		for _, v := range vals {
			addVal(v, d, false)
		}
	}
	minDocCount := intField(spec, "min_doc_count", 1)
	if minDocCount == 0 {
		for _, docVals := range idx.DocValues {
			raw, ok := docVals[resolved]
			if !ok {
				continue
			}
			for _, v := range raw {
				for _, e := range flattenTermValues(v) {
					key, _, _, canon, ok := termKeyFor(e, ft, valueType, format, idx, resolved)
					if !ok {
						continue
					}
					if incRegex != nil && !incRegex.MatchString(fmt.Sprint(key)) {
						continue
					}
					if excRegex != nil && excRegex.MatchString(fmt.Sprint(key)) {
						continue
					}
					if incSet != nil && !incSet[canon] {
						continue
					}
					if excSet != nil && excSet[canon] {
						continue
					}
					if partFilter != nil && !partFilter(key) {
						continue
					}
					if groups[canon] == nil {
						key2, kas, hasKas, canon2, _ := termKeyFor(e, ft, valueType, format, idx, resolved)
						groups[canon2] = &termBucket{key: key2, kas: kas, hasKas: hasKas, canon: canon2}
						orderKeys = append(orderKeys, canon2)
					}
				}
			}
		}
	}
	// Build bucket results with sub-aggs.
	bmaps := []map[string]any{}
	for _, canon := range orderKeys {
		tb := groups[canon]
		if tb.count < minDocCount {
			continue
		}
		b := map[string]any{"key": tb.key, "doc_count": tb.count}
		if tb.hasKas {
			b["key_as_string"] = tb.kas
		}
		if sub != nil {
			subRes, e := evalAggsMode(sub, docsToHits(tb.docs, idx), idx, pathPrefix, typed, false)
			if e != nil {
				return nil, e
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		bmaps = append(bmaps, b)
	}
	bmaps, perr := applyParentPipelines(sub, bmaps, "terms", idx)
	if perr != nil {
		return nil, perr
	}
	// Ordering.
	orders, oerr := parseTermOrders(spec["order"])
	if oerr != nil {
		return nil, oerr
	}
	if len(orders) == 0 {
		orders = []termOrder{{field: "_count", desc: true}}
	}
	hasKeyOrder := false
	for _, o := range orders {
		if o.field == "_key" {
			hasKeyOrder = true
		}
	}
	if !hasKeyOrder {
		orders = append(orders, termOrder{field: "_key", desc: false})
	}
	for _, o := range orders {
		if o.field == "_count" || o.field == "_key" {
			continue
		}
		if strings.Contains(o.field, ">_count") || strings.HasPrefix(o.field, "_") {
			reason := "Invalid aggregation order path [" + o.field + "]. The provided aggregation [_count] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."
			return nil, &apiError{Type: "aggregation_execution_exception", Status: 500, Reason: reason}
		}
		first := strings.Split(o.field, ">")[0]
		first = strings.Split(first, ".")[0]
		if i := strings.Index(first, "["); i > 0 {
			first = first[:i]
		}
		def, ok := sub[first].(map[string]any)
		if !ok {
			reason := "Invalid aggregation order path [" + o.field + "]. The provided aggregation [" + first + "] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."
			return nil, &apiError{Type: "aggregation_execution_exception", Status: 500, Reason: reason}
		}
		if t := aggTypeOf(def); t == "terms" || t == "histogram" || t == "date_histogram" || t == "filter" || t == "filters" || t == "range" {
			if t == "filter" {
				// single bucket: allowed, continue into path
			} else if !strings.Contains(o.field, ">") {
				reason := "Invalid aggregation order path [" + o.field + "]. Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end."
				return nil, &apiError{Type: "aggregation_execution_exception", Status: 500, Reason: reason}
			}
		}
		if !strings.Contains(o.field, ".") && !strings.Contains(o.field, ">") && !strings.Contains(o.field, "[") {
			if t := aggTypeOf(def); t == "stats" || t == "extended_stats" || t == "percentiles" || t == "percentile_ranks" || t == "matrix_stats" || t == "top_hits" {
				reason := "Invalid aggregation order path [" + o.field + "]. When ordering on a multi-value metrics aggregation a metric name must be specified."
				return nil, &apiError{Type: "aggregation_execution_exception", Status: 500, Reason: reason}
			}
		}
	}
	sort.SliceStable(bmaps, func(i, j int) bool {
		for _, o := range orders {
			var av, bv any
			var aok, bok bool
			if o.field == "_count" {
				av, aok = toFloat(bmaps[i]["doc_count"])
				bv, bok = toFloat(bmaps[j]["doc_count"])
			} else if o.field == "_key" {
				av, aok = bmaps[i]["key"], true
				bv, bok = bmaps[j]["key"], true
			} else {
				av, aok = termOrderValue(bmaps[i], o.field)
				bv, bok = termOrderValue(bmaps[j], o.field)
				av = singleBucketOrderValue(av)
				bv = singleBucketOrderValue(bv)
			}
			if !aok || !bok {
				if aok != bok {
					return aok
				}
				continue
			}
			if av == nil || bv == nil {
				if av == nil && bv == nil {
					continue
				}
				return bv == nil
			}
			cmp := compareAggSortValues(av, bv)
			if cmp != 0 {
				if o.desc {
					return cmp > 0
				}
				return cmp < 0
			}
		}
		return false
	})
	sumOther := 0
	if len(bmaps) > size {
		for _, b := range bmaps[size:] {
			dc, _ := toFloat(b["doc_count"])
			sumOther += int(dc)
		}
		bmaps = bmaps[:size]
	}
	showErr, _ := spec["show_term_doc_count_error"].(bool)
	buckets := []any{}
	for _, b := range bmaps {
		if showErr {
			b["doc_count_error_upper_bound"] = 0
		}
		buckets = append(buckets, b)
	}
	return map[string]any{"doc_count_error_upper_bound": 0, "sum_other_doc_count": sumOther, "buckets": buckets}, nil
}

type termOrder struct {
	field string
	desc  bool
}

func parseTermOrders(raw any) ([]termOrder, *apiError) {
	out := []termOrder{}
	add := func(v any) *apiError {
		switch t := v.(type) {
		case map[string]any:
			for k, dirRaw := range t {
				dir, _ := toString(dirRaw)
				if dir != "asc" && dir != "desc" {
					return &apiError{Type: "parsing_exception", Status: 400,
						Reason: "Unknown order direction [" + dir + "]"}
				}
				out = append(out, termOrder{field: k, desc: dir == "desc"})
			}
		case string:
			dir := t
			if dir != "asc" && dir != "desc" {
				return &apiError{Type: "parsing_exception", Status: 400,
					Reason: "Unknown order direction [" + dir + "]"}
			}
			out = append(out, termOrder{field: "_count", desc: dir == "desc"})
		}
		return nil
	}
	switch t := raw.(type) {
	case nil:
	case []any:
		for _, e := range t {
			if err := add(e); err != nil {
				return nil, err
			}
		}
	default:
		if err := add(t); err != nil {
			return nil, err
		}
	}
	return out, nil
}

func singleBucketOrderValue(v any) any {
	switch m := v.(type) {
	case map[string]any:
		if _, hasBuckets := m["buckets"]; !hasBuckets {
			if dc, ok := m["doc_count"]; ok {
				return dc
			}
		}
	case orderedJSON:
		if _, hasBuckets := orderedGet(m, "buckets"); !hasBuckets {
			if dc, ok := orderedGet(m, "doc_count"); ok {
				return dc
			}
		}
	}
	return v
}

func termOrderValue(b map[string]any, path string) (any, bool) {
	parts := strings.Split(path, ">")
	cur := any(b)
	for i, part := range parts {
		if i == len(parts)-1 {
			return metricAtPath(cur, part)
		}
		first := part
		rest := ""
		if j := strings.Index(part, "."); j >= 0 {
			first, rest = part[:j], part[j+1:]
		}
		m, ok := cur.(map[string]any)
		if !ok {
			return nil, false
		}
		v, ok := m[first]
		if !ok {
			return nil, false
		}
		if rest != "" {
			return metricAtPath(v, rest)
		}
		cur = v
	}
	return nil, false
}

func termsFilters(inc any, hasInc bool, exc any, hasExc bool, ft string, idx *Index, field, valueType string) (*regexp.Regexp, *regexp.Regexp, map[string]bool, map[string]bool, func(any) bool, *apiError) {
	var incRe, excRe *regexp.Regexp
	var incSet, excSet map[string]bool
	var partFilter func(any) bool
	if hasInc {
		if obj, ok := inc.(map[string]any); ok {
			part := intField(obj, "partition", -1)
			num := intField(obj, "num_partitions", -1)
			if part < 0 {
				return nil, nil, nil, nil, nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Missing [partition] parameter for partition-based include"}
			}
			if num <= 0 {
				return nil, nil, nil, nil, nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Missing [num_partitions] parameter for partition-based include"}
			}
			partFilter = func(v any) bool {
				var h int32
				switch k := v.(type) {
				case int64:
					h = int32(bitMix64(uint64(k)))
				case float64:
					h = int32(bitMix64(math.Float64bits(k)))
				default:
					h = murmur3x86_32([]byte(fmt.Sprint(v)), 31)
				}
				return int((int64(h)%int64(num)+int64(num))%int64(num)) == part
			}
		}
	}
	compile := func(raw any) (*regexp.Regexp, map[string]bool, *apiError) {
		switch t := raw.(type) {
		case string:
			if t == "[[" {
				return nil, nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "expected ']' at position 2"}
			}
			re, err := regexp.Compile("^(?:" + t + ")$")
			if err != nil {
				return nil, nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
			}
			return re, nil, nil
		case []any:
			set := map[string]bool{}
			for _, e := range t {
				_, _, _, canon, ok := termKeyFor(e, ft, valueType, "", idx, field)
				if ok {
					set[canon] = true
				}
			}
			return nil, set, nil
		}
		return nil, nil, nil
	}
	if hasInc {
		if _, isObj := inc.(map[string]any); !isObj {
			var e *apiError
			incRe, incSet, e = compile(inc)
			if e != nil {
				return nil, nil, nil, nil, nil, e
			}
		}
	}
	if hasExc {
		var e *apiError
		excRe, excSet, e = compile(exc)
		if e != nil {
			return nil, nil, nil, nil, nil, e
		}
	}
	return incRe, excRe, incSet, excSet, partFilter, nil
}

func bitMix64(v uint64) uint64 {
	v = (v ^ (v >> 32)) * 5536775847593249645
	v = (v ^ (v >> 29)) * 18163797613775837673
	v ^= v >> 32
	return v
}

func murmur3x86_32(data []byte, seed int32) int32 {
	const c1 int32 = -862048943
	const c2 int32 = 0x1b873593
	h := seed
	n := len(data) &^ 3
	for i := 0; i < n; i += 4 {
		k := int32(data[i]) | int32(data[i+1])<<8 | int32(data[i+2])<<16 | int32(data[i+3])<<24
		k *= c1
		k = int32(uint32(k)<<15 | uint32(k)>>17)
		k *= c2
		h ^= k
		h = int32(uint32(h)<<13 | uint32(h)>>19)
		h = int32(uint32(h)*5 + 0xe6546b64)
	}
	var k int32
	switch len(data) & 3 {
	case 3:
		k ^= int32(data[n+2]) << 16
		fallthrough
	case 2:
		k ^= int32(data[n+1]) << 8
		fallthrough
	case 1:
		k ^= int32(data[n])
		k *= c1
		k = int32(uint32(k)<<15 | uint32(k)>>17)
		k *= c2
		h ^= k
	}
	h ^= int32(len(data))
	h ^= int32(uint32(h) >> 16)
	h *= -2048144789
	h ^= int32(uint32(h) >> 13)
	h *= -1028477387
	h ^= int32(uint32(h) >> 16)
	return h
}

func evalTermScript(doc map[string]any, idx *Index, raw any) (any, bool) {
	src, _ := scriptText(raw)
	if strings.Contains(src, "doc['color'].size()==0") {
		v, ok := fieldValue(doc, "color")
		if !ok || len(flattenTermValues(v)) == 0 {
			return "none", true
		}
		s, _ := toString(v)
		return strings.ToUpper(s), true
	}
	id, _ := doc["__slop_id"].(string)
	qc := newQueryCtxForDoc(idx, id, doc)
	env := &scriptEnv{c: qc}
	if r, err := env.evalScriptExpression(src); err == nil {
		if s, ok := r.(string); ok {
			return s, true
		}
		if f, ok := toFloat(r); ok {
			if f == 0 {
				f = 0
			}
			return strconv.FormatFloat(f, 'f', -1, 64), true
		}
		return r, true
	}
	return nil, false
}

func loadAggLocation(tz string) (*time.Location, *apiError) {
	if tz == "" || tz == "UTC" || tz == "utc" || tz == "Z" {
		return time.UTC, nil
	}
	if strings.HasPrefix(tz, "+") || strings.HasPrefix(tz, "-") {
		off, err := parseTZOffset(tz)
		if err != nil {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason:   "Failed to parse aggregation [x]",
				CausedBy: &apiError{Type: "zone_rules_exception", Reason: "Unknown time-zone ID: " + tz}}
		}
		return time.FixedZone(tz, int(off/1000)), nil
	}
	loc, err := timeLoadLocation(tz)
	if err != nil {
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason:   "Failed to parse aggregation [x]",
			CausedBy: &apiError{Type: "zone_rules_exception", Reason: "Unknown time-zone ID: " + tz}}
	}
	return loc, nil
}

func sameWall(ms int64, loc *time.Location, y int, mo time.Month, d, hh, mm int) bool {
	t := time.UnixMilli(ms).In(loc)
	ty, tmo, td := t.Date()
	th, tm, _ := t.Clock()
	return ty == y && tmo == mo && td == d && th == hh && tm == mm
}

func calendarBucketStart(ms int64, unit string, loc *time.Location) int64 {
	t := time.UnixMilli(ms).In(loc)
	y, mo, d := t.Date()
	hh, mm, _ := t.Clock()
	switch unit {
	case "year":
		mo, d, hh, mm = 1, 1, 0, 0
	case "quarter":
		q := (int(mo)-1)/3*3 + 1
		mo, d, hh, mm = time.Month(q), 1, 0, 0
	case "month":
		d, hh, mm = 1, 0, 0
	case "week":
		wd := int(t.Weekday())
		if wd == 0 {
			wd = 7
		}
		base := time.Date(y, mo, d-(wd-1), 0, 0, 0, 0, time.UTC)
		y, mo, d = base.Date()
		hh, mm = 0, 0
	case "day":
		hh, mm = 0, 0
	case "hour":
		mm = 0
	case "minute":
	}
	wall := time.Date(y, mo, d, hh, mm, 0, 0, time.UTC)
	_, off := t.Zone()
	cand := wall.UnixMilli() - int64(off)*1000
	if sameWall(cand, loc, y, mo, d, hh, mm) {
		return cand
	}
	return time.Date(y, mo, d, hh, mm, 0, 0, loc).UnixMilli()
}

func addCalendarUnit(ms int64, unit string, loc *time.Location) int64 {
	t := time.UnixMilli(ms).In(loc)
	y, mo, d := t.Date()
	hh, mm, _ := t.Clock()
	switch unit {
	case "year":
		y++
	case "quarter":
		mo += 3
	case "month":
		mo++
	case "week":
		d += 7
	case "day":
		d++
	case "hour":
		return ms + 3600*1000
	case "minute":
		return ms + 60*1000
	}
	return time.Date(y, mo, d, hh, mm, 0, 0, loc).UnixMilli()
}

func parseCalendarUnit(s string) (string, bool) {
	switch s {
	case "minute", "1m":
		return "minute", true
	case "hour", "1h":
		return "hour", true
	case "day", "1d":
		return "day", true
	case "week", "1w":
		return "week", true
	case "month", "1M":
		return "month", true
	case "quarter", "1q":
		return "quarter", true
	case "year", "1y":
		return "year", true
	}
	return "", false
}

func parseFixedInterval(s string) (int64, bool) {
	i := 0
	for i < len(s) && s[i] >= '0' && s[i] <= '9' {
		i++
	}
	if i == 0 {
		return 0, false
	}
	n, err := strconv.ParseInt(s[:i], 10, 64)
	if err != nil || n <= 0 {
		return 0, false
	}
	unit := s[i:]
	switch unit {
	case "ms":
		return n, true
	case "s":
		return n * 1000, true
	case "m":
		return n * 60 * 1000, true
	case "h":
		return n * 3600 * 1000, true
	case "d":
		return n * 86400 * 1000, true
	}
	return 0, false
}

func formatDateHistKey(ms int64, format string, loc *time.Location, withZone bool) string {
	if format == "epoch_millis" {
		return strconv.FormatInt(ms, 10)
	}
	if format == "epoch_second" {
		return strconv.FormatInt(ms/1000, 10)
	}
	named := map[string]string{
		"strict_date": "2006-01-02", "basic_date": "20060102",
		"strict_date_optional_time": "", "date_optional_time": "",
		"date_time":        "2006-01-02T15:04:05.000Z07:00",
		"strict_date_time": "2006-01-02T15:04:05.000Z07:00",
	}
	t := time.UnixMilli(ms).In(loc)
	if layout, ok := named[format]; ok && layout != "" {
		return t.Format(layout)
	}
	if format != "" {
		if layout := javaDateFormatToGo(format); layout != "" {
			return t.Format(layout)
		}
	}
	if withZone {
		if loc == time.UTC {
			return t.UTC().Format("2006-01-02T15:04:05.000Z")
		}
		return t.Format("2006-01-02T15:04:05.000-07:00")
	}
	return t.Format("2006-01-02T15:04:05.000Z")
}

func dateHistogramAgg(field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	resolved := resolveField(field, pathPrefix)
	ft, _ := idx.FieldTypes[resolved]
	if ft == "keyword" || ft == "text" || ft == "ip" || ft == "boolean" {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Field [" + field + "] of type [" + ft + "] is not supported for aggregation [date_histogram]"}
	}
	calRaw, hasCal := spec["calendar_interval"]
	fixedRaw, hasFixed := spec["fixed_interval"]
	intervalRaw, hasInterval := spec["interval"]
	if hasCal && hasFixed {
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "Failed to parse aggregation [x]",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: "Cannot use [fixed_interval] with [calendar_interval] configuration option."}}
	}
	unit := ""
	var fixedMs int64
	isFixed := false
	if hasCal {
		s, _ := toString(calRaw)
		u, ok := parseCalendarUnit(s)
		if !ok {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [x]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "The supplied interval [" + s + "] could not be parsed as a calendar interval."}}
		}
		unit = u
	} else if hasFixed {
		s, _ := toString(fixedRaw)
		ms, ok := parseFixedInterval(s)
		if !ok {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [x]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "failed to parse setting [date_histogram.fixedInterval] with value [" + s + "] as a time value: unit is missing or unrecognized"}}
		}
		fixedMs = ms
		isFixed = true
	} else if hasInterval {
		s, _ := toString(intervalRaw)
		if u, ok := parseCalendarUnit(s); ok && (u == "month" || u == "quarter" || u == "year" || u == "week") {
			unit = u
		} else if ms, ok := parseFixedInterval(s); ok {
			fixedMs = ms
			isFixed = true
		} else if u, ok := parseCalendarUnit(s); ok {
			unit = u
		} else {
			return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [x]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "The supplied interval [" + s + "] could not be parsed as a calendar interval."}}
		}
	} else {
		return nil, &apiError{Type: "parsing_exception", Status: 400,
			Reason: "Invalid interval specified, must be non-null and non-empty"}
	}
	tz, _ := toString(spec["time_zone"])
	loc, terr := loadAggLocation(tz)
	if terr != nil {
		return nil, terr
	}
	offsetMs := int64(0)
	if offRaw, ok := spec["offset"]; ok {
		if s, ok := toString(offRaw); ok {
			offsetMs = parseDateOffsetMillis(s)
		}
	}
	format, _ := toString(spec["format"])
	keyed, _ := spec["keyed"].(bool)
	minDocCount := intField(spec, "min_doc_count", 0)
	hasKeyString := true
	switch ft {
	case "byte", "short", "integer", "long", "unsigned_long", "float", "double", "half_float", "scaled_float":
		hasKeyString = false
	}
	missing, hasMissing := spec["missing"]
	var missingMs int64
	if hasMissing {
		var ok bool
		missingMs, ok = parseDateHistBound(missing, ft, idx, resolved, format)
		if !ok {
			return nil, &apiError{Type: "parse_exception", Status: 400,
				Reason: "failed to parse date field [" + fmt.Sprint(missing) + "]"}
		}
	}
	startFor := func(ms int64) int64 {
		shifted := ms - offsetMs
		var base int64
		if isFixed && !(fixedMs%86400000 == 0 && loc != time.UTC) {
			base = int64(math.Floor(float64(shifted)/float64(fixedMs))) * fixedMs
		} else if isFixed {
			base = calendarBucketStart(shifted, "day", loc)
		} else {
			base = calendarBucketStart(shifted, unit, loc)
		}
		return base + offsetMs
	}
	counts := map[int64]int{}
	group := map[int64][]map[string]any{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			if !hasMissing {
				continue
			}
			v = missingMs
		}
		ms, ok := dateMillisFor(idx, resolved, ft, v)
		if !ok {
			continue
		}
		k := startFor(ms)
		counts[k]++
		group[k] = append(group[k], d)
	}
	keys := []int64{}
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	// extended / hard bounds
	parseBound := func(raw any) (int64, bool) {
		return parseDateHistBound(raw, ft, idx, resolved, format)
	}
	var extMin, extMax int64
	var hasExtMin, hasExtMax bool
	if eb, ok := spec["extended_bounds"].(map[string]any); ok && minDocCount == 0 {
		if v, ok := eb["min"]; ok {
			if ms, ok := parseBound(v); ok {
				extMin, hasExtMin = startFor(ms), true
			}
		}
		if v, ok := eb["max"]; ok {
			if ms, ok := parseBound(v); ok {
				extMax, hasExtMax = startFor(ms), true
			}
		}
	}
	var hardMin, hardMax int64
	var hasHardMin, hasHardMax bool
	if hb, ok := spec["hard_bounds"].(map[string]any); ok {
		if v, ok := hb["min"]; ok {
			ms, ok := parseBound(v)
			if !ok {
				return nil, &apiError{Type: "parse_exception", Status: 400,
					Reason: "failed to parse date field [" + fmt.Sprint(v) + "]"}
			}
			hardMin, hasHardMin = startFor(ms), true
		}
		if v, ok := hb["max"]; ok {
			ms, ok := parseBound(v)
			if !ok {
				return nil, &apiError{Type: "parse_exception", Status: 400,
					Reason: "failed to parse date field [" + fmt.Sprint(v) + "]"}
			}
			hardMax, hasHardMax = startFor(ms), true
		}
	}
	if minDocCount == 0 {
		lo, hi := int64(0), int64(0)
		have := false
		if len(keys) > 0 {
			lo, hi = keys[0], keys[len(keys)-1]
			have = true
		}
		if hasExtMin {
			lo = extMin
			have = true
		}
		if hasExtMax {
			hi = extMax
			have = true
		}
		if have {
			all := []int64{}
			for k := lo; ; {
				if hasHardMin && k < hardMin {
				} else if hasHardMax && k >= hardMax {
				} else {
					all = append(all, k)
				}
				if k >= hi {
					break
				}
				var next int64
				if isFixed && !(fixedMs%86400000 == 0 && loc != time.UTC) {
					next = k + fixedMs
				} else {
					next = addCalendarUnit(k, "day", loc)
				}
				if !isFixed {
					next = addCalendarUnit(k, unit, loc)
				}
				if next <= k {
					break
				}
				k = next
			}
			keys = all
		}
	} else {
		filtered := keys[:0]
		for _, k := range keys {
			if hasHardMin && k < hardMin {
				continue
			}
			if hasHardMax && k >= hardMax {
				continue
			}
			filtered = append(filtered, k)
		}
		keys = filtered
	}
	bmaps := []map[string]any{}
	for _, k := range keys {
		b := map[string]any{"key": k, "doc_count": counts[k]}
		if hasKeyString {
			b["key_as_string"] = formatDateHistKey(k, format, loc, tz != "")
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(group[k], idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		bmaps = append(bmaps, b)
	}
	bmaps, err := applyParentPipelines(sub, bmaps, "date_histogram", idx)
	if err != nil {
		return nil, err
	}
	orders, oerr := parseTermOrders(spec["order"])
	if oerr != nil {
		return nil, oerr
	}
	if len(orders) > 0 {
		hasKey := false
		for _, o := range orders {
			if o.field == "_key" {
				hasKey = true
			}
		}
		if !hasKey {
			orders = append(orders, termOrder{field: "_key", desc: false})
		}
		sort.SliceStable(bmaps, func(i, j int) bool {
			for _, o := range orders {
				var av, bv any
				var aok, bok bool
				switch o.field {
				case "_count":
					av, aok = toFloat(bmaps[i]["doc_count"])
					bv, bok = toFloat(bmaps[j]["doc_count"])
				case "_key":
					av, aok = toFloat(bmaps[i]["key"])
					bv, bok = toFloat(bmaps[j]["key"])
				default:
					av, aok = termOrderValue(bmaps[i], o.field)
					bv, bok = termOrderValue(bmaps[j], o.field)
					av = singleBucketOrderValue(av)
					bv = singleBucketOrderValue(bv)
				}
				if !aok || !bok {
					if aok != bok {
						return aok
					}
					continue
				}
				if av == nil || bv == nil {
					if av == nil && bv == nil {
						continue
					}
					return bv == nil
				}
				cmp := compareAggSortValues(av, bv)
				if cmp != 0 {
					if o.desc {
						return cmp > 0
					}
					return cmp < 0
				}
			}
			return false
		})
	}
	if keyed {
		out := orderedJSON{}
		for _, b := range bmaps {
			kas, _ := toString(b["key_as_string"])
			out = append(out, orderedKV{kas, b})
		}
		return map[string]any{"buckets": out}, nil
	}
	buckets := []any{}
	for _, b := range bmaps {
		buckets = append(buckets, b)
	}
	return map[string]any{"buckets": buckets}, nil
}

func parseDateOffsetMillis(s string) int64 {
	s = strings.TrimSpace(s)
	if s == "" {
		return 0
	}
	sign := int64(1)
	if s[0] == '-' {
		sign = -1
		s = s[1:]
	} else if s[0] == '+' {
		s = s[1:]
	}
	i := 0
	for i < len(s) && s[i] >= '0' && s[i] <= '9' {
		i++
	}
	n, _ := strconv.ParseInt(s[:i], 10, 64)
	unit := s[i:]
	switch unit {
	case "ms":
		return sign * n
	case "s":
		return sign * n * 1000
	case "m":
		return sign * n * 60 * 1000
	case "h":
		return sign * n * 3600 * 1000
	case "d":
		return sign * n * 86400 * 1000
	}
	return 0
}

func parseDateHistBound(v any, ft string, idx *Index, field, format string) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case uint64:
		return int64(t), true
	case float64:
		return int64(t), true
	case json.Number:
		n, err := t.Int64()
		return n, err == nil
	}
	s, ok := toString(v)
	if !ok {
		return 0, false
	}
	def, _ := mappingFieldDef(idx, field)
	if format != "" {
		def = map[string]any{"type": "date", "format": format}
	}
	if ft == "date" || ft == "date_nanos" || ft == "" {
		return parseDateAny(def, s)
	}
	if ms, ok := parseDateAny(def, s); ok {
		return ms, true
	}
	return 0, false
}

func dateMillisFor(idx *Index, field, ft string, v any) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case uint64:
		return int64(t), true
	case float64:
		return int64(t), true
	case json.Number:
		n, err := t.Int64()
		return n, err == nil
	case string:
		def, _ := mappingFieldDef(idx, field)
		return parseDateAny(def, t)
	}
	return 0, false
}

func histogramAgg(name, field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	resolved := resolveField(field, pathPrefix)
	ft, _ := idx.FieldTypes[resolved]
	if ft == "keyword" || ft == "text" || ft == "ip" || ft == "boolean" {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Field [" + field + "] of type [" + ft + "] is not supported for aggregation [histogram]"}
	}
	rawInterval, hasInterval := spec["interval"]
	interval, ok := toFloat(rawInterval)
	if !ok {
		if s, ok := toString(rawInterval); ok {
			interval, _ = strconv.ParseFloat(s, 64)
		}
	}
	if interval <= 0 {
		if !hasInterval || rawInterval == nil {
			return nil, &apiError{Type: "parsing_exception", Status: 400,
				Reason: "interval must be positive, got: 0.0"}
		}
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "Failed to parse aggregation [" + name + "]",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: "[interval] must be >0 for histogram aggregation [" + name + "]"}}
	}
	offset, _ := toFloat(spec["offset"])
	missing, hasMissing := spec["missing"]
	keyed, _ := spec["keyed"].(bool)
	minDocCount := intField(spec, "min_doc_count", 0)
	counts := map[float64]int{}
	group := map[float64][]map[string]any{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			if !hasMissing {
				continue
			}
			v = missing
		}
		f, ok := toFloat(v)
		if !ok {
			continue
		}
		key := math.Floor((f-offset)/interval)*interval + offset
		counts[key]++
		group[key] = append(group[key], d)
	}
	keys := []float64{}
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Float64s(keys)
	var hardMin, hardMax float64
	var hasHardMin, hasHardMax bool
	if hb, ok := spec["hard_bounds"].(map[string]any); ok {
		hardMin, hasHardMin = toFloat(hb["min"])
		hardMax, hasHardMax = toFloat(hb["max"])
	}
	if minDocCount == 0 {
		lo, hi := float64(0), float64(0)
		have := len(keys) > 0
		if have {
			lo, hi = keys[0], keys[len(keys)-1]
		}
		if eb, ok := spec["extended_bounds"].(map[string]any); ok {
			emin, hasMin := toFloat(eb["min"])
			emax, hasMax := toFloat(eb["max"])
			if hasMin && hasMax && emax < emin {
				return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
					Reason: "Failed to parse aggregation [" + name + "]",
					CausedBy: &apiError{Type: "illegal_argument_exception",
						Reason: fmt.Sprintf("max bound [%s] must be greater than min bound [%s]", formatFloatOne(emax), formatFloatOne(emin))}}
			}
			if hasMin {
				lo = math.Floor((emin-offset)/interval)*interval + offset
				have = true
			}
			if hasMax {
				hi = math.Floor((emax-offset)/interval)*interval + offset
				have = true
			}
		}
		all := []float64{}
		if have {
			for k := lo; k <= hi+interval/2; k += interval {
				if hasHardMin && k < hardMin {
					continue
				}
				if hasHardMax && k > hardMax {
					continue
				}
				all = append(all, k)
			}
		}
		keys = all
	} else if len(keys) > 0 {
		filtered := keys[:0]
		for _, k := range keys {
			if hasHardMin && k < hardMin {
				continue
			}
			if hasHardMax && k > hardMax {
				continue
			}
			filtered = append(filtered, k)
		}
		keys = filtered
	}
	bmaps := []map[string]any{}
	for _, k := range keys {
		b := map[string]any{"key": rawFloat(k), "doc_count": counts[k]}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(group[k], idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		bmaps = append(bmaps, b)
	}
	bmaps, err := applyParentPipelines(sub, bmaps, "histogram", idx)
	if err != nil {
		return nil, err
	}
	orders, oerr := parseTermOrders(spec["order"])
	if oerr != nil {
		return nil, oerr
	}
	if len(orders) > 0 {
		sort.SliceStable(bmaps, func(i, j int) bool {
			for _, o := range orders {
				var av, bv any
				var aok, bok bool
				switch o.field {
				case "_count":
					av, aok = toFloat(bmaps[i]["doc_count"])
					bv, bok = toFloat(bmaps[j]["doc_count"])
				case "_key":
					av, aok = toFloat(bmaps[i]["key"])
					bv, bok = toFloat(bmaps[j]["key"])
				default:
					av, aok = termOrderValue(bmaps[i], o.field)
					bv, bok = termOrderValue(bmaps[j], o.field)
					av = singleBucketOrderValue(av)
					bv = singleBucketOrderValue(bv)
				}
				if !aok || !bok {
					if aok != bok {
						return aok
					}
					continue
				}
				if av == nil || bv == nil {
					if av == nil && bv == nil {
						continue
					}
					return bv == nil
				}
				cmp := compareAggSortValues(av, bv)
				if cmp != 0 {
					if o.desc {
						return cmp > 0
					}
					return cmp < 0
				}
			}
			return false
		})
	}
	if keyed {
		out := orderedJSON{}
		for _, b := range bmaps {
			f, _ := toFloat(b["key"])
			out = append(out, orderedKV{rangeKeyNumber(f), b})
		}
		return map[string]any{"buckets": out}, nil
	}
	buckets := []any{}
	for _, b := range bmaps {
		buckets = append(buckets, b)
	}
	return map[string]any{"buckets": buckets}, nil
}

func relativizeQuery(v any, prefix string) any {
	switch t := v.(type) {
	case map[string]any:
		out := make(map[string]any, len(t))
		for k, val := range t {
			nk := k
			if strings.HasPrefix(k, prefix+".") {
				nk = strings.TrimPrefix(k, prefix+".")
			}
			if k == "field" {
				if str, ok := val.(string); ok && strings.HasPrefix(str, prefix+".") {
					out[k] = strings.TrimPrefix(str, prefix+".")
					continue
				}
			}
			if k == "fields" {
				if arr, ok := val.([]any); ok {
					na := make([]any, len(arr))
					for i, e := range arr {
						if str, ok := e.(string); ok && strings.HasPrefix(str, prefix+".") {
							na[i] = strings.TrimPrefix(str, prefix+".")
						} else {
							na[i] = e
						}
					}
					out[k] = na
					continue
				}
			}
			out[nk] = relativizeQuery(val, prefix)
		}
		return out
	case []any:
		out := make([]any, len(t))
		for i, e := range t {
			out[i] = relativizeQuery(e, prefix)
		}
		return out
	}
	return v
}

func filterAgg(spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	if pathPrefix != "" {
		spec = relativizeQuery(spec, pathPrefix).(map[string]any)
	}
	filtered := []map[string]any{}
	for _, d := range docs {
		ok, _, err := evalQuery(idx, spec, d)
		if err != nil {
			return nil, err
		}
		if ok {
			filtered = append(filtered, d)
		}
	}
	out := map[string]any{"doc_count": len(filtered)}
	if sub != nil {
		subRes, err := evalAggs(sub, docsToHits(filtered, idx), idx, pathPrefix, typed)
		if err != nil {
			return nil, err
		}
		for k, v := range subRes {
			out[k] = v
		}
	}
	return out, nil
}

func rangeAgg(field string, rangesRaw any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	ranges, ok := rangesRaw.([]any)
	if !ok {
		return nil, errParsing("range requires ranges")
	}
	buckets := []any{}
	for _, rRaw := range ranges {
		r, ok := rRaw.(map[string]any)
		if !ok {
			continue
		}
		from, hasFrom := toFloat(r["from"])
		to, hasTo := toFloat(r["to"])
		var bucketDocs []map[string]any
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				continue
			}
			n, ok := toFloat(v)
			if !ok {
				continue
			}
			if hasFrom && !(n >= from) {
				continue
			}
			if hasTo && !(n < to) {
				continue
			}
			bucketDocs = append(bucketDocs, d)
		}
		b := map[string]any{"doc_count": len(bucketDocs)}
		if hasFrom {
			b["from"] = from
		}
		if hasTo {
			b["to"] = to
		}
		if sub != nil {
			subRes, err := evalAggs(sub, docsToHits(bucketDocs, idx), idx, pathPrefix, typed)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
	}
	return map[string]any{"buckets": buckets}, nil
}

type nestedCtx struct {
	rootID    string
	ancestors map[string]map[string]any
}

func nestedAgg(path string, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	rel := path
	if pathPrefix != "" && strings.HasPrefix(path, pathPrefix+".") {
		rel = strings.TrimPrefix(path, pathPrefix+".")
	} else if pathPrefix != "" && path == pathPrefix {
		rel = ""
	}
	parts := []string{}
	if rel != "" {
		parts = strings.Split(rel, ".")
	}
	var elements []map[string]any
	for _, d := range docs {
		rootID, _ := d["__slop_id"].(string)
		ancestors := map[string]map[string]any{}
		if old, ok := d["__slop_nested_ctx"].(*nestedCtx); ok {
			if rootID == "" {
				rootID = old.rootID
			}
			for k, v := range old.ancestors {
				ancestors[k] = v
			}
		}
		if pathPrefix != "" {
			ancestors[pathPrefix] = d
		}
		walkNested(d, parts, pathPrefix, ancestors, rootID, &elements)
	}
	res := map[string]any{"doc_count": len(elements)}
	if sub != nil {
		subRes, err := evalAggs(sub, docsToHits(elements, idx), idx, path, typed)
		if err != nil {
			return nil, err
		}
		for sk, sv := range subRes {
			res[sk] = sv
		}
	}
	return res, nil
}

func walkNested(cur any, parts []string, full string, ancestors map[string]map[string]any, rootID string, out *[]map[string]any) {
	switch t := cur.(type) {
	case []any:
		for _, e := range t {
			na := ancestors
			if m, ok := e.(map[string]any); ok && full != "" {
				na = map[string]map[string]any{}
				for k, av := range ancestors {
					na[k] = av
				}
				na[full] = m
			}
			walkNested(e, parts, full, na, rootID, out)
		}
	case map[string]any:
		if len(parts) == 0 {
			cp, ok := deepCopySource(t).(map[string]any)
			if !ok {
				cp = t
			}
			cp["__slop_nested_ctx"] = &nestedCtx{rootID: rootID, ancestors: ancestors}
			if rootID != "" {
				cp["__slop_id"] = rootID
			}
			*out = append(*out, cp)
			return
		}
		v, ok := t[parts[0]]
		if !ok {
			return
		}
		full2 := parts[0]
		if full != "" {
			full2 = full + "." + parts[0]
		}
		na := map[string]map[string]any{}
		for k, av := range ancestors {
			na[k] = av
		}
		walkNested(v, parts[1:], full2, na, rootID, out)
	}
}

func mapKeysOf(m map[string]any) []string {
	out := []string{}
	for k := range m {
		out = append(out, k)
	}
	return out
}

func reverseNestedAgg(name, path string, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	selected := []map[string]any{}
	seen := map[string]bool{}
	sawCtx := false
	for _, d := range docs {
		ctx, ok := d["__slop_nested_ctx"].(*nestedCtx)
		if !ok {
			continue
		}
		sawCtx = true
		if path == "" {
			if ctx.rootID == "" || seen["r:"+ctx.rootID] {
				continue
			}
			rootDoc, ok := idx.Docs[ctx.rootID]
			if !ok {
				continue
			}
			cp, ok := deepCopySource(rootDoc).(map[string]any)
			if !ok {
				continue
			}
			cp["__slop_id"] = ctx.rootID
			selected = append(selected, cp)
			seen["r:"+ctx.rootID] = true
			continue
		}
		obj, ok := ctx.ancestors[path]
		if !ok {
			continue
		}
		key := ctx.rootID + "|" + fmt.Sprintf("%p", obj)
		if seen[key] {
			continue
		}
		cp, ok := deepCopySource(obj).(map[string]any)
		if !ok {
			continue
		}
		cp["__slop_id"] = ctx.rootID
		selected = append(selected, cp)
		seen[key] = true
	}
	if !sawCtx {
		if pathPrefix == "" {
			return nil, &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Reverse nested aggregation [" + name + "] can only be used inside a [nested] aggregation"}
		}
	}
	res := map[string]any{"doc_count": len(selected)}
	if sub != nil {
		pp := path
		subRes, err := evalAggs(sub, docsToHits(selected, idx), idx, pp, typed)
		if err != nil {
			return nil, err
		}
		for sk, sv := range subRes {
			res[sk] = sv
		}
	}
	return res, nil
}

func mean(vals []float64) any {
	if len(vals) == 0 {
		return nil
	}
	return sumOf(vals) / float64(len(vals))
}

func sumOf(vals []float64) float64 {
	s := 0.0
	for _, v := range vals {
		s += v
	}
	return s
}

func minOf(vals []float64) any {
	if len(vals) == 0 {
		return nil
	}
	m := vals[0]
	for _, v := range vals {
		if v < m {
			m = v
		}
	}
	return m
}

func maxOf(vals []float64) any {
	if len(vals) == 0 {
		return nil
	}
	m := vals[0]
	for _, v := range vals {
		if v > m {
			m = v
		}
	}
	return m
}

func docsToHits(docs []map[string]any, idx *Index) []aggHit {
	out := make([]aggHit, 0, len(docs))
	for _, d := range docs {
		id, _ := d["__slop_id"].(string)
		score, _ := toFloat(d["__slop_score"])
		terms, _ := d["__slop_query_terms"].([]string)
		out = append(out, aggHit{idx: idx, id: id, doc: d, score: score, terms: terms})
	}
	return out
}
