package main

import (
	"fmt"
	"sort"
	"strconv"
	"strings"
)

func aggTypeOf(def map[string]any) string {
	for k := range def {
		switch k {
		case "aggs", "aggregations", "meta":
			continue
		}
		return k
	}
	return ""
}

func isParentPipelineType(t string) bool {
	switch t {
	case "derivative", "cumulative_sum", "moving_fn", "moving_avg", "serial_diff",
		"bucket_script", "bucket_selector", "bucket_sort":
		return true
	}
	return false
}

func isSiblingPipelineType(t string) bool {
	switch t {
	case "avg_bucket", "sum_bucket", "min_bucket", "max_bucket", "stats_bucket",
		"extended_stats_bucket", "percentiles_bucket":
		return true
	}
	return false
}

func isPipelineType(t string) bool {
	return isParentPipelineType(t) || isSiblingPipelineType(t)
}

func withoutPipelines(sub map[string]any) map[string]any {
	if sub == nil {
		return nil
	}
	out := map[string]any{}
	for k, v := range sub {
		def, _ := v.(map[string]any)
		if def != nil && isPipelineType(aggTypeOf(def)) {
			continue
		}
		out[k] = v
	}
	return out
}

func applyParentPipelines(sub map[string]any, buckets []map[string]any, kind string, idx *Index) ([]map[string]any, *apiError) {
	if sub == nil {
		return buckets, nil
	}
	pipes := []struct {
		name string
		def  map[string]any
	}{}
	for name, raw := range sub {
		def, ok := raw.(map[string]any)
		if !ok || !isParentPipelineType(aggTypeOf(def)) {
			continue
		}
		pipes = append(pipes, struct {
			name string
			def  map[string]any
		}{name, def})
	}
	if len(pipes) == 0 {
		return buckets, nil
	}
	pipeNames := map[string]bool{}
	for _, p := range pipes {
		pipeNames[p.name] = true
	}
	for _, p := range pipes {
		t := aggTypeOf(p.def)
		if kind == "terms" && t != "bucket_script" && t != "bucket_selector" && t != "bucket_sort" {
			return nil, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: fmt.Sprintf("Validation Failed: 1: %s aggregation [%s] must have a histogram, date_histogram or auto_date_histogram as parent;", t, p.name)}
		}
		body, _ := p.def[t].(map[string]any)
		for _, path := range bucketPathsOf(body) {
			first := strings.Split(strings.Split(path, ">")[0], ".")[0]
			if i := strings.Index(first, "["); i > 0 {
				first = first[:i]
			}
			if first == "_count" || first == "_bucket_count" || first == "_key" || pipeNames[first] {
				continue
			}
			found := false
			for _, b := range buckets {
				if _, ok := b[first]; ok {
					found = true
					break
				}
			}
			if !found {
				return nil, &apiError{Type: "action_request_validation_exception", Status: 400,
					Reason: "Validation Failed: 1: No aggregation found for path [" + path + "];"}
			}
		}
	}
	applied := map[string]bool{}
	for _, p := range pipes {
		if pipelineAlreadyApplied(buckets, p.name) {
			applied[p.name] = true
		}
	}
	for pass := 0; pass < len(pipes)+2; pass++ {
		progress := false
		for _, p := range pipes {
			t := aggTypeOf(p.def)
			body, _ := p.def[t].(map[string]any)
			if body == nil {
				continue
			}
			if applied[p.name] {
				continue
			}
			if !pipelineDepsReady(body, buckets, applied) {
				continue
			}
			var err *apiError
			switch t {
			case "derivative":
				err = applyDerivative(p.name, body, buckets, kind)
			case "cumulative_sum":
				err = applyCumulativeSum(p.name, body, buckets)
			case "serial_diff":
				err = applySerialDiff(p.name, body, buckets)
			case "moving_fn":
				err = applyMovingFn(p.name, body, buckets)
			case "moving_avg":
				err = applyMovingAvg(p.name, body, buckets)
			case "bucket_script":
				err = applyBucketScript(p.name, body, buckets)
			case "bucket_selector":
				buckets, err = applyBucketSelector(p.name, body, buckets)
			case "bucket_sort":
				buckets, err = applyBucketSort(p.name, body, buckets)
			default:
				continue
			}
			if err != nil {
				return nil, err
			}
			applied[p.name] = true
			progress = true
		}
		if !progress {
			break
		}
	}
	return buckets, nil
}

func pipelineAlreadyApplied(buckets []map[string]any, name string) bool {
	for _, b := range buckets {
		if _, ok := b[name]; ok {
			return true
		}
	}
	return false
}

func pipelineDepsReady(body map[string]any, buckets []map[string]any, applied map[string]bool) bool {
	paths := bucketPathsOf(body)
	for _, path := range paths {
		first := strings.Split(strings.Split(path, ">")[0], ".")[0]
		if i := strings.Index(first, "["); i > 0 {
			first = first[:i]
		}
		if first == "_count" || first == "_bucket_count" || first == "_key" || applied[first] {
			continue
		}
		for _, b := range buckets {
			if _, ok := b[first]; !ok {
				return false
			}
		}
	}
	return true
}

func bucketPathsOf(body map[string]any) []string {
	out := []string{}
	if s, ok := body["buckets_path"].(string); ok {
		out = append(out, s)
	}
	if m, ok := body["buckets_path"].(map[string]any); ok {
		for _, v := range m {
			if s, ok := toString(v); ok {
				out = append(out, s)
			}
		}
	}
	return out
}

func gapPolicyOf(body map[string]any) (string, *apiError) {
	gp, _ := toString(body["gap_policy"])
	if gp == "" {
		return "skip", nil
	}
	if gp != "skip" && gp != "insert_zeros" {
		return "", &apiError{Type: "parsing_exception", Status: 400,
			Reason: fmt.Sprintf("Invalid gap policy: [%s], accepted values: [insert_zeros, skip]", gp)}
	}
	return gp, nil
}

func metricAtPath(node any, path string) (any, bool) {
	path = strings.TrimSpace(path)
	if path == "" {
		return node, true
	}
	if path == "_count" {
		if m, ok := node.(map[string]any); ok {
			if v, ok := m["doc_count"]; ok {
				return v, true
			}
		}
	}
	if path == "_bucket_count" {
		if m, ok := node.(map[string]any); ok {
			if bs, ok := m["buckets"].([]any); ok {
				return len(bs), true
			}
		}
	}
	if i := strings.Index(path, "["); i > 0 && strings.HasSuffix(path, "]") {
		key := path[i+1 : len(path)-1]
		base := path[:i]
		v, ok := metricAtPath(node, base)
		if !ok {
			return nil, false
		}
		if m, ok := v.(map[string]any); ok {
			if vals, ok := m["values"]; ok {
				return metricMapKey(vals, key)
			}
		}
		if o, ok := v.(orderedJSON); ok {
			if vals, ok := orderedGet(o, "values"); ok {
				return metricMapKey(vals, key)
			}
			return orderedMapKey(o, key)
		}
		return nil, false
	}
	first := path
	rest := ""
	if i := strings.Index(path, "."); i >= 0 {
		first, rest = path[:i], path[i+1:]
	}
	switch m := node.(type) {
	case map[string]any:
		if v, ok := m[first]; ok {
			if rest == "" {
				return metricScalar(v)
			}
			return metricAtPath(v, rest)
		}
	case orderedJSON:
		if v, ok := orderedGet(m, first); ok {
			if rest == "" {
				return metricScalar(v)
			}
			return metricAtPath(v, rest)
		}
	}
	if m, ok := node.(map[string]any); ok && rest != "" {
		if vals, ok := m["values"]; ok {
			if f, err := strconv.ParseFloat(rest, 64); err == nil {
				if x, ok := metricMapKey(vals, percentileKey(f)); ok {
					return metricScalar(x)
				}
			}
			if x, ok := metricMapKey(vals, rest); ok {
				return metricScalar(x)
			}
		}
	}
	if o, ok := node.(orderedJSON); ok && rest != "" {
		if vals, ok := orderedGet(o, "values"); ok {
			if f, err := strconv.ParseFloat(rest, 64); err == nil {
				if x, ok := metricMapKey(vals, percentileKey(f)); ok {
					return metricScalar(x)
				}
			}
			if x, ok := metricMapKey(vals, rest); ok {
				return metricScalar(x)
			}
		}
	}
	return nil, false
}

func metricScalar(v any) (any, bool) {
	switch m := v.(type) {
	case map[string]any:
		if val, ok := m["value"]; ok {
			return val, true
		}
		return v, true
	case orderedJSON:
		if val, ok := orderedGet(m, "value"); ok {
			return val, true
		}
		return v, true
	default:
		return v, true
	}
}

func orderedGet(o orderedJSON, key string) (any, bool) {
	for _, kv := range o {
		if kv.k == key {
			return kv.v, true
		}
	}
	return nil, false
}

func orderedMapKey(v any, key string) (any, bool) {
	switch m := v.(type) {
	case orderedJSON:
		if x, ok := orderedGet(m, key); ok {
			return x, true
		}
	case map[string]any:
		if x, ok := m[key]; ok {
			return x, true
		}
	}
	return nil, false
}

func metricMapKey(vals any, key string) (any, bool) {
	switch m := vals.(type) {
	case orderedJSON:
		return orderedGet(m, key)
	case map[string]any:
		if v, ok := m[key]; ok {
			return v, true
		}
	}
	return nil, false
}

func parentMetricValue(bucket map[string]any, path, gap string) (any, bool) {
	if path == "_count" || path == "_bucket_count" {
		v, ok := metricAtPath(bucket, path)
		return v, ok
	}
	dc, _ := toFloat(bucket["doc_count"])
	v, ok := metricAtPath(bucket, path)
	if !ok || v == nil {
		if gap == "insert_zeros" {
			return 0.0, true
		}
		return nil, false
	}
	if dc == 0 {
		if gap == "insert_zeros" {
			return 0.0, true
		}
		return nil, false
	}
	return v, true
}

func pipelineValue(out any) any {
	return out
}

func applyDerivative(name string, body map[string]any, buckets []map[string]any, kind string) *apiError {
	path, _ := toString(body["buckets_path"])
	if path == "" {
		return &apiError{Type: "parsing_exception", Status: 400, Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	gap, err := gapPolicyOf(body)
	if err != nil {
		return err
	}
	unit, _ := toString(body["unit"])
	unitMs := durationMillis(unit)
	if unit != "" && unitMs == 0 {
		return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "failed to parse [" + unit + "]",
			CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "failed to parse [" + unit + "]"}}
	}
	format, _ := toString(body["format"])
	var prev any
	prevOK := false
	for i, b := range buckets {
		cur, curOK := parentMetricValue(b, path, gap)
		if i == 0 {
			prev, prevOK = cur, curOK
			continue
		}
		if i == 1 {
			first := strings.Split(strings.Split(path, ">")[0], ".")[0]
			if j := strings.Index(first, "["); j > 0 {
				first = first[:j]
			}
			if _, exists := buckets[0][first]; !exists && first != "_count" && first != "_bucket_count" {
				prev, prevOK = cur, curOK
				continue
			}
			if !prevOK {
				prev, prevOK = cur, curOK
				continue
			}
		}
		if gap == "insert_zeros" {
			if !curOK {
				cur, curOK = 0.0, true
			}
			if !prevOK {
				prev, prevOK = 0.0, true
			}
		}
		if !curOK || !prevOK {
			nilOut := orderedJSON{{"value", nil}}
			if unit != "" {
				nilOut = append(nilOut, orderedKV{"normalized_value", nil})
			}
			b[name] = nilOut
			prev, prevOK = cur, curOK
			continue
		}
		cf, _ := toFloat(cur)
		pf, _ := toFloat(prev)
		val := cf - pf
		out := orderedJSON{{"value", rawFloat(val)}}
		if unit != "" {
			norm := val
			if kind == "date_histogram" {
				curKey, _ := toFloat(b["key"])
				prevKey, _ := toFloat(buckets[i-1]["key"])
				days := (curKey - prevKey) / 86400000
				if days > 0 {
					norm = val / days
				}
			} else {
				norm = val * unitMs / 20
			}
			out = append(out, orderedKV{"normalized_value", rawFloat(norm)})
		}
		if format != "" {
			out = append(out, orderedKV{"value_as_string", fmtMetricNumber(val, format)})
		}
		b[name] = out
		prev, prevOK = cur, curOK
	}
	return nil
}

func durationMillis(unit string) float64 {
	if unit == "" {
		return 0
	}
	u := strings.TrimSpace(unit)
	mult := 1.0
	for _, suffix := range []struct {
		s string
		m float64
	}{{"millis", 1}, {"ms", 1}, {"seconds", 1000}, {"second", 1000}, {"s", 1000},
		{"minutes", 60000}, {"minute", 60000}, {"m", 60000}, {"hours", 3600000}, {"hour", 3600000}, {"h", 3600000},
		{"days", 86400000}, {"day", 86400000}, {"d", 86400000}} {
		if strings.HasSuffix(u, suffix.s) {
			u = strings.TrimSuffix(u, suffix.s)
			mult = suffix.m
			break
		}
	}
	n := 1.0
	if u != "" {
		var err error
		n, err = strconv.ParseFloat(u, 64)
		if err != nil {
			return 0
		}
	}
	return n * mult
}

func applyCumulativeSum(name string, body map[string]any, buckets []map[string]any) *apiError {
	if _, ok := body["gap_policy"]; ok {
		return &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "[cumulative_sum] unknown field [gap_policy]"}
	}
	path, _ := toString(body["buckets_path"])
	if path == "" {
		return &apiError{Type: "parsing_exception", Status: 400, Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	format, _ := toString(body["format"])
	total := 0.0
	for _, b := range buckets {
		v, ok := parentMetricValue(b, path, "skip")
		if ok {
			f, _ := toFloat(v)
			total += f
		}
		out := orderedJSON{{"value", rawFloat(total)}}
		if format != "" {
			out = append(out, orderedKV{"value_as_string", fmtMetricNumber(total, format)})
		}
		b[name] = out
	}
	return nil
}

func applySerialDiff(name string, body map[string]any, buckets []map[string]any) *apiError {
	path, _ := toString(body["buckets_path"])
	if path == "" {
		return &apiError{Type: "parsing_exception", Status: 400, Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	lag := intField(body, "lag", 1)
	if lag == 0 {
		return &apiError{Type: "parsing_exception", Status: 400,
			Reason: "Lag must be a positive, non-zero integer.  Value supplied was0 in [" + name + "]: [lag]."}
	}
	gap, err := gapPolicyOf(body)
	if err != nil {
		return err
	}
	format, _ := toString(body["format"])
	for i := range buckets {
		if i < lag {
			continue
		}
		cur, curOK := parentMetricValue(buckets[i], path, gap)
		base, baseOK := parentMetricValue(buckets[i-lag], path, gap)
		if !curOK || !baseOK {
			if gap == "insert_zeros" {
				cur, curOK = 0.0, true
				base, baseOK = 0.0, true
			}
		}
		if !curOK || !baseOK {
			continue
		}
		cf, _ := toFloat(cur)
		bf, _ := toFloat(base)
		val := cf - bf
		out := orderedJSON{{"value", rawFloat(val)}}
		if format != "" {
			out = append(out, orderedKV{"value_as_string", fmtMetricNumber(val, format)})
		}
		buckets[i][name] = out
	}
	return nil
}

func applyBucketScript(name string, body map[string]any, buckets []map[string]any) *apiError {
	paths, _ := body["buckets_path"].(map[string]any)
	if len(paths) == 0 {
		return &apiError{Type: "parsing_exception", Status: 400, Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	gap, err := gapPolicyOf(body)
	if err != nil {
		return err
	}
	format, _ := toString(body["format"])
	src, params := scriptText(body["script"])
	for _, b := range buckets {
		pvals := map[string]any{}
		missing := false
		for k, raw := range paths {
			path, _ := toString(raw)
			v, ok := parentMetricValue(b, path, gap)
			if !ok {
				missing = true
				break
			}
			f, _ := toFloat(v)
			pvals[k] = f
		}
		if missing {
			continue
		}
		env := &scriptEnv{params: pvals}
		if len(params) > 0 {
			for k, v := range params {
				pvals[k] = v
			}
		}
		r, e := env.evalScriptExpression(src)
		if e != nil {
			continue
		}
		if r == nil {
			continue
		}
		if _, ok := toFloat(r); !ok {
			reason := "Cannot cast from [java.lang.String] to [java.lang.Number]."
			return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
				CausedBy: &apiError{Type: "script_exception", Reason: reason,
					CausedBy: &apiError{Type: "illegal_argument_exception", Reason: reason}}}
		}
		f, _ := toFloat(r)
		out := orderedJSON{{"value", rawFloat(f)}}
		if format != "" {
			out = append(out, orderedKV{"value_as_string", fmtMetricNumber(f, format)})
		}
		b[name] = out
	}
	return nil
}

func applyBucketSelector(name string, body map[string]any, buckets []map[string]any) ([]map[string]any, *apiError) {
	paths, _ := body["buckets_path"].(map[string]any)
	if len(paths) == 0 {
		return buckets, &apiError{Type: "parsing_exception", Status: 400, Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	gap, err := gapPolicyOf(body)
	if err != nil {
		return buckets, err
	}
	src, _ := scriptText(body["script"])
	kept := []map[string]any{}
	for _, b := range buckets {
		pvals := map[string]any{}
		missing := false
		for k, raw := range paths {
			path, _ := toString(raw)
			v, ok := parentMetricValue(b, path, gap)
			if !ok {
				missing = true
				break
			}
			f, _ := toFloat(v)
			pvals[k] = f
		}
		if missing {
			continue
		}
		env := &scriptEnv{params: pvals}
		r, e := env.evalScriptExpression(src)
		if e != nil {
			continue
		}
		bv, ok := r.(bool)
		if !ok {
			reason := "Cannot cast from [int] to [boolean]."
			return buckets, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
				CausedBy: &apiError{Type: "script_exception", Reason: reason,
					CausedBy: &apiError{Type: "illegal_argument_exception", Reason: reason}}}
		}
		if bv {
			kept = append(kept, b)
		}
	}
	return kept, nil
}

func applyBucketSort(name string, body map[string]any, buckets []map[string]any) ([]map[string]any, *apiError) {
	if _, ok := body["sort"]; !ok {
		if _, ok := body["size"]; !ok {
			if _, ok := body["from"]; !ok {
				return buckets, &apiError{Type: "action_request_validation_exception", Status: 400,
					Reason: "Validation Failed: 1: [" + name + "] is configured to perform nothing. Please set either of [sort, size, from] to use bucket_sort;"}
			}
		}
	}
	size := -1
	if v, ok := body["size"]; ok {
		f, _ := toFloat(v)
		if f < 0 {
			return buckets, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [" + name + "]",
				CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "size must be positive",
					CausedBy: &apiError{Type: "illegal_argument_exception", Reason: fmt.Sprintf("[size] must be a positive integer: [%d]", int(f))}}}
		}
		size = int(f)
	}
	from := 0
	if v, ok := body["from"]; ok {
		f, _ := toFloat(v)
		if f < 0 {
			return buckets, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "Failed to parse aggregation [" + name + "]",
				CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "from must be non-negative",
					CausedBy: &apiError{Type: "illegal_argument_exception", Reason: fmt.Sprintf("[from] must be a non-negative integer: [%d]", int(f))}}}
		}
		from = int(f)
	}
	sorts := parseSortList(body["sort"])
	if len(sorts) > 0 {
		for _, s := range sorts {
			if s.field == "_count" || s.field == "_key" {
				continue
			}
			found, multi := false, false
			for _, b := range buckets {
				if v, ok := metricAtPath(b, s.field); ok {
					found = true
					if isMultiValue(v) {
						multi = true
					}
					break
				}
			}
			if !found {
				return buckets, &apiError{Type: "action_request_validation_exception", Status: 400,
					Reason: "Validation Failed: 1: No aggregation found for path [" + s.field + "];"}
			}
			if multi {
				reason := "buckets_path must reference either a number value or a single value numeric metric aggregation, but [" + s.field + "] contains multiple values. Please specify which to use."
				return buckets, &apiError{Type: "search_phase_execution_exception", Status: 500, Reason: reason,
					CausedBy: &apiError{Type: "aggregation_execution_exception", Reason: reason}}
			}
		}
		sort.SliceStable(buckets, func(i, j int) bool {
			for _, s := range sorts {
				av, aok := bucketSortVal(buckets[i], s.field)
				bv, bok := bucketSortVal(buckets[j], s.field)
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
	if from > len(buckets) {
		from = len(buckets)
	}
	end := len(buckets)
	if size >= 0 && from+size < end {
		end = from + size
	}
	return buckets[from:end], nil
}

func bucketHasMetric(buckets []map[string]any, name string) bool {
	for _, b := range buckets {
		if v, ok := b[name]; ok {
			if _, isMap := v.(map[string]any); isMap {
				return true
			}
		}
	}
	return false
}

func bucketSortVal(b map[string]any, field string) (any, bool) {
	if field == "_count" {
		return b["doc_count"], true
	}
	if field == "_key" {
		return b["key"], true
	}
	v, ok := metricAtPath(b, field)
	if !ok || v == nil {
		return nil, false
	}
	if m, ok := v.(map[string]any); ok {
		if len(m) > 1 {
			return nil, false
		}
	}
	return v, true
}

func lastPathSeg(path string) string {
	if i := strings.LastIndex(path, ">"); i >= 0 {
		path = path[i+1:]
	}
	if i := strings.LastIndex(path, "."); i >= 0 {
		path = path[i+1:]
	}
	if i := strings.Index(path, "["); i > 0 {
		path = path[:i]
	}
	return path
}

func metricContainerValue(v any, key string) (any, bool) {
	switch m := v.(type) {
	case map[string]any:
		x, ok := m[key]
		return x, ok
	case orderedJSON:
		return orderedGet(m, key)
	}
	return nil, false
}

func isMultiValue(v any) bool {
	switch m := v.(type) {
	case map[string]any:
		if _, ok := m["value"]; ok {
			return false
		}
		return len(m) > 1
	case orderedJSON:
		if _, ok := orderedGet(m, "value"); ok {
			return false
		}
		return len(m) > 1
	}
	return false
}

func evalSiblingPipeline(name string, body map[string]any, typ string, out map[string]any) (any, *apiError) {
	path, _ := toString(body["buckets_path"])
	if path == "" {
		return nil, &apiError{Type: "parsing_exception", Status: 400,
			Reason: "Missing required field [buckets_path] for aggregation [" + name + "]"}
	}
	gap, err := gapPolicyOf(body)
	if err != nil {
		return nil, err
	}
	vals, keys, err := siblingPathValues(out, path, gap)
	if err != nil {
		return nil, err
	}
	format, _ := toString(body["format"])
	switch typ {
	case "avg_bucket":
		if len(vals) == 0 {
			return orderedJSON{{"value", nil}}, nil
		}
		v := meanF(vals)
		o := orderedJSON{{"value", rawFloat(v)}}
		if format != "" {
			o = append(o, orderedKV{"value_as_string", fmtMetricNumber(v, format)})
		}
		return o, nil
	case "sum_bucket":
		v := sumOf(vals)
		o := orderedJSON{{"value", rawFloat(v)}}
		if format != "" {
			o = append(o, orderedKV{"value_as_string", fmtMetricNumber(v, format)})
		}
		return o, nil
	case "min_bucket", "max_bucket":
		if len(vals) == 0 {
			return orderedJSON{{"value", nil}, {"keys", []any{}}}, nil
		}
		best := vals[0]
		bestKeys := []any{}
		for i, v := range vals {
			better := false
			if typ == "min_bucket" {
				better = v < best
			} else {
				better = v > best
			}
			if better {
				best = v
				bestKeys = []any{}
			}
			if v == best {
				bestKeys = append(bestKeys, keys[i])
			}
		}
		o := orderedJSON{{"value", rawFloat(best)}, {"keys", bestKeys}}
		if format != "" {
			o = append(o, orderedKV{"value_as_string", fmtMetricNumber(best, format)})
		}
		return o, nil
	case "stats_bucket":
		if len(vals) == 0 {
			return orderedJSON{{"count", 0}, {"min", nil}, {"max", nil}, {"avg", nil}, {"sum", rawFloat(0)}}, nil
		}
		return orderedJSON{
			{"count", len(vals)}, {"min", rawFloat(minF(vals))}, {"max", rawFloat(maxF(vals))},
			{"avg", rawFloat(meanF(vals))}, {"sum", rawFloat(sumOf(vals))},
		}, nil
	case "extended_stats_bucket":
		return siblingExtendedStats(body, vals)
	case "percentiles_bucket":
		return siblingPercentiles(body, vals)
	}
	return map[string]any{}, nil
}

func siblingPathValues(out map[string]any, path, gap string) ([]float64, []any, *apiError) {
	segs := splitBucketPath(path, out)
	if len(segs) == 0 {
		return nil, nil, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: No aggregation found for path [" + path + "];"}
	}
	first := segs[0]
	node, ok := out[first]
	if !ok {
		return nil, nil, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: No aggregation found for path [" + path + "];"}
	}
	vals := []float64{}
	keys := []any{}
	missingAgg := ""
	var collectErr *apiError
	var walk func(node any, segs []string, key any) bool
	walk = func(node any, segs []string, key any) bool {
		if len(segs) == 0 {
			if m, ok := node.(map[string]any); ok {
				if _, isBucket := m["buckets"]; isBucket {
					collectErr = &apiError{Type: "search_phase_execution_exception", Status: 500,
						Reason: "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [Bucket] at aggregation [" + first + "]",
						CausedBy: &apiError{Type: "aggregation_execution_exception",
							Reason: "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [Bucket] at aggregation [" + first + "]"}}
					return false
				}
			}
			v, ok := metricScalar(node)
			if !ok {
				return false
			}
			if v == nil {
				return true
			}
			if isMultiValue(v) {
				collectErr = &apiError{Type: "search_phase_execution_exception", Status: 500,
					Reason: "buckets_path must reference either a number value or a single value numeric metric aggregation, but [" + lastPathSeg(path) + "] contains multiple values. Please specify which to use.",
					CausedBy: &apiError{Type: "aggregation_execution_exception",
						Reason: "buckets_path must reference either a number value or a single value numeric metric aggregation, but [" + lastPathSeg(path) + "] contains multiple values. Please specify which to use."}}
				return false
			}
			f, ok := toFloat(v)
			if !ok {
				return true
			}
			vals = append(vals, f)
			keys = append(keys, key)
			return true
		}
		if m, ok := node.(map[string]any); ok {
			if bs, ok := m["buckets"].([]any); ok {
				_ = bs
				for _, rawB := range m["buckets"].([]any) {
					b, _ := rawB.(map[string]any)
					if b == nil {
						continue
					}
					dc, _ := toFloat(b["doc_count"])
					seg := segs[0]
					if seg == "_count" || seg == "_bucket_count" {
						if seg == "_count" {
							vals = append(vals, dc)
						} else {
							if sub, ok := b["buckets"].([]any); ok {
								vals = append(vals, float64(len(sub)))
							}
						}
						keys = append(keys, bucketKey(b))
						continue
					}
					if dc == 0 {
						if gap == "insert_zeros" {
							vals = append(vals, 0)
							keys = append(keys, bucketKey(b))
							continue
						}
						continue
					}
					if !walk(b, segs, bucketKey(b)) {
						return false
					}
				}
				return true
			}
			seg := segs[0]
			if seg == "_count" {
				if dc, ok := m["doc_count"]; ok {
					f, _ := toFloat(dc)
					vals = append(vals, f)
					keys = append(keys, key)
					return true
				}
			}
			if i := strings.Index(seg, "."); i > 0 {
				head, rest := seg[:i], seg[i+1:]
				if child, ok := m[head]; ok {
					return walk(child, append([]string{rest}, segs[1:]...), key)
				}
			}
			if i := strings.Index(seg, "["); i > 0 && strings.HasSuffix(seg, "]") {
				head, k := seg[:i], seg[i+1:len(seg)-1]
				if child, ok := m[head]; ok {
					if vals, ok := metricContainerValue(child, "values"); ok {
						if x, ok := metricMapKey(vals, k); ok {
							return walk(x, segs[1:], key)
						}
					}
				}
			}
			if valsMap, ok := m["values"]; ok {
				if x, ok := metricMapKey(valsMap, seg); ok {
					return walk(x, segs[1:], key)
				}
				if f, err := strconv.ParseFloat(seg, 64); err == nil {
					if x, ok := metricMapKey(valsMap, percentileKey(f)); ok {
						return walk(x, segs[1:], key)
					}
				}
			}
			child, ok := resolveChild(m, seg)
			if !ok {
				missingAgg = seg
				return false
			}
			return walk(child, segs[1:], key)
		}
		if o, ok := node.(orderedJSON); ok {
			seg := segs[0]
			if vals, ok := orderedGet(o, "values"); ok {
				if x, ok := metricMapKey(vals, seg); ok {
					return walk(x, segs[1:], key)
				}
				if f, err := strconv.ParseFloat(seg, 64); err == nil {
					if x, ok := metricMapKey(vals, percentileKey(f)); ok {
						return walk(x, segs[1:], key)
					}
				}
			}
			child, ok := orderedGet(o, seg)
			if !ok {
				if _, hasCount := orderedGet(o, "count"); hasCount {
					if _, hasSum := orderedGet(o, "sum"); hasSum {
						collectErr = &apiError{Type: "search_phase_execution_exception", Status: 400,
							Reason: "No enum constant org.opensearch.search.aggregations.metrics.InternalStats.Metrics." + seg,
							CausedBy: &apiError{Type: "illegal_argument_exception",
								Reason: "No enum constant org.opensearch.search.aggregations.metrics.InternalStats.Metrics." + seg}}
						return false
					}
				}
				missingAgg = seg
				return false
			}
			return walk(child, segs[1:], key)
		}
		return false
	}
	if !walk(node, segs[1:], nil) {
		if collectErr != nil {
			return nil, nil, collectErr
		}
		if missingAgg != "" {
			return nil, nil, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: No aggregation [" + missingAgg + "] found for path [" + path + "];"}
		}
	}
	if collectErr != nil {
		return nil, nil, collectErr
	}
	return vals, keys, nil
}

func splitBucketPath(path string, out map[string]any) []string {
	if strings.Contains(path, ">") {
		return strings.Split(path, ">")
	}
	first := path
	for k := range out {
		if strings.HasPrefix(path, k+".") {
			if len(k) > len(first) || first == path {
				first = k
			}
		}
	}
	if first != path {
		return []string{first, strings.TrimPrefix(path, first+".")}
	}
	return []string{path}
}

func resolveChild(m map[string]any, seg string) (any, bool) {
	if v, ok := m[seg]; ok {
		return v, true
	}
	if i := strings.Index(seg, "."); i > 0 {
		head, rest := seg[:i], seg[i+1:]
		if v, ok := m[head]; ok {
			if rest == "_count" {
				if mm, ok := v.(map[string]any); ok {
					if dc, ok := mm["doc_count"]; ok {
						return dc, true
					}
				}
			}
			if mm, ok := v.(map[string]any); ok {
				if vals, ok := mm["values"]; ok {
					if x, ok := metricMapKey(vals, rest); ok {
						return x, true
					}
					if f, err := strconv.ParseFloat(rest, 64); err == nil {
						if x, ok := metricMapKey(vals, percentileKey(f)); ok {
							return x, true
						}
					}
				}
			}
			return v, true
		}
	}
	if i := strings.Index(seg, "["); i > 0 && strings.HasSuffix(seg, "]") {
		head := seg[:i]
		key := seg[i+1 : len(seg)-1]
		if v, ok := m[head]; ok {
			if mm, ok := v.(map[string]any); ok {
				if vals, ok := mm["values"]; ok {
					return metricMapKey(vals, key)
				}
			}
		}
	}
	return nil, false
}

func bucketKey(b map[string]any) any {
	if s, ok := b["key_as_string"].(string); ok {
		return s
	}
	if k, ok := b["key"]; ok {
		switch t := k.(type) {
		case rawFloat:
			return strconv.FormatFloat(float64(t), 'f', 1, 64)
		case float64:
			return strconv.FormatFloat(t, 'f', 1, 64)
		}
		return k
	}
	return nil
}

func siblingExtendedStats(body map[string]any, vals []float64) (any, *apiError) {
	n := len(vals)
	if n == 0 {
		return orderedJSON{
			{"count", 0}, {"min", nil}, {"max", nil}, {"avg", nil}, {"sum", rawFloat(0)},
			{"sum_of_squares", nil}, {"variance", nil}, {"variance_population", nil},
			{"variance_sampling", nil}, {"std_deviation", nil}, {"std_deviation_population", nil},
			{"std_deviation_sampling", nil},
			{"std_deviation_bounds", orderedJSON{{"upper", nil}, {"lower", nil}, {"upper_population", nil},
				{"lower_population", nil}, {"upper_sampling", nil}, {"lower_sampling", nil}}},
		}, nil
	}
	sum := sumOf(vals)
	avg := sum / float64(n)
	sumSq := 0.0
	for _, v := range vals {
		sumSq += v * v
	}
	varPop := sumSq/float64(n) - avg*avg
	if varPop < 0 {
		varPop = 0
	}
	var varSample any = nil
	var stdSample any = nil
	if n > 1 {
		vs := (sumSq - float64(n)*avg*avg) / float64(n-1)
		varSample = rawFloat(vs)
		stdSample = rawFloat(mathSqrt(vs))
	}
	stdPop := mathSqrt(varPop)
	sigma := 2.0
	if v, ok := toFloat(body["sigma"]); ok {
		sigma = v
	}
	return orderedJSON{
		{"count", n}, {"min", rawFloat(minF(vals))}, {"max", rawFloat(maxF(vals))},
		{"avg", rawFloat(avg)}, {"sum", rawFloat(sum)}, {"sum_of_squares", rawFloat(sumSq)},
		{"variance", rawFloat(varPop)}, {"variance_population", rawFloat(varPop)},
		{"variance_sampling", varSample}, {"std_deviation", rawFloat(stdPop)},
		{"std_deviation_population", rawFloat(stdPop)}, {"std_deviation_sampling", stdSample},
		{"std_deviation_bounds", orderedJSON{
			{"upper", rawFloat(avg + sigma*stdPop)}, {"lower", rawFloat(avg - sigma*stdPop)},
			{"upper_population", rawFloat(avg + sigma*stdPop)}, {"lower_population", rawFloat(avg - sigma*stdPop)},
			{"upper_sampling", anyOrNil(stdSample, func(s float64) float64 { return avg + sigma*s })},
			{"lower_sampling", anyOrNil(stdSample, func(s float64) float64 { return avg - sigma*s })},
		}},
	}, nil
}

func mathSqrt(f float64) float64 {
	if f <= 0 {
		return 0
	}
	x := f
	for i := 0; i < 40; i++ {
		x = (x + f/x) / 2
	}
	return x
}

func siblingPercentiles(body map[string]any, vals []float64) (any, *apiError) {
	rawPercents, has := body["percents"]
	percents := []float64{1, 5, 25, 50, 75, 95, 99}
	if has {
		arr, _ := rawPercents.([]any)
		percents = []float64{}
		for _, e := range arr {
			if f, ok := toFloat(e); ok {
				percents = append(percents, f)
			}
		}
	}
	keyed := true
	if k, ok := body["keyed"].(bool); ok {
		keyed = k
	}
	if keyed {
		sorted := append([]float64(nil), percents...)
		sort.Float64s(sorted)
		out := orderedJSON{}
		for _, p := range sorted {
			var v any
			if len(vals) > 0 {
				v = rawFloat(tdigestQuantile(vals, p/100))
			}
			out = append(out, orderedKV{percentileKey(p), v})
		}
		return orderedJSON{{"values", out}}, nil
	}
	items := []any{}
	for _, p := range percents {
		var v any
		if len(vals) > 0 {
			v = rawFloat(tdigestQuantile(vals, p/100))
		}
		items = append(items, orderedJSON{{"key", rawFloat(p)}, {"value", v}})
	}
	return orderedJSON{{"values", items}}, nil
}

func applyMovingFn(name string, body map[string]any, buckets []map[string]any) *apiError {
	scriptRaw, hasScript := body["script"]
	if !hasScript {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Required [script]"}
	}
	if _, ok := body["window"]; !ok {
		return &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Required [window]"}
	}
	window := intField(body, "window", 0)
	if window <= 0 {
		inner := &apiError{Type: "illegal_argument_exception", Reason: "[window] must be a positive, non-zero integer."}
		return &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason:   "Failed to build [moving_fn] after last required field arrived",
			CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "window must be positive", CausedBy: inner}}
	}
	shift := intField(body, "shift", 0)
	gap, err := gapPolicyOf(body)
	if err != nil {
		return err
	}
	path, _ := toString(body["buckets_path"])
	src, params := scriptText(scriptRaw)
	for i, b := range buckets {
		dc, _ := toFloat(b["doc_count"])
		if dc == 0 && gap == "skip" {
			continue
		}
		vals := []float64{}
		if gap == "skip" {
			if shift > 0 {
				if i+shift > len(buckets) {
					b[name] = orderedJSON{{"value", nil}}
					continue
				}
				for j := 0; j <= i; j++ {
					if d, _ := toFloat(buckets[j]["doc_count"]); d == 0 {
						continue
					}
					if v, ok := metricAtPath(buckets[j], path); ok {
						if f, ok := toFloat(v); ok {
							vals = append(vals, f)
						}
					}
				}
			} else {
				for j := 0; j < i; j++ {
					if d, _ := toFloat(buckets[j]["doc_count"]); d == 0 {
						continue
					}
					if v, ok := metricAtPath(buckets[j], path); ok {
						if f, ok := toFloat(v); ok {
							vals = append(vals, f)
						}
					}
				}
			}
			if len(vals) > window {
				vals = vals[len(vals)-window:]
			}
		} else {
			end := i + shift - 1
			start := end - window + 1
			if end >= len(buckets) {
				b[name] = orderedJSON{{"value", nil}}
				continue
			}
			if start < 0 {
				start = 0
			}
			for j := start; j <= end && j < len(buckets); j++ {
				d, _ := toFloat(buckets[j]["doc_count"])
				if d == 0 {
					vals = append(vals, 0)
					continue
				}
				if v, ok := metricAtPath(buckets[j], path); ok {
					if f, ok := toFloat(v); ok {
						vals = append(vals, f)
					}
				}
			}
		}
		res, ok := movingFnResult(src, params, vals)
		if !ok {
			b[name] = orderedJSON{{"value", nil}}
			continue
		}
		if res == nil {
			b[name] = orderedJSON{{"value", nil}}
		} else {
			b[name] = orderedJSON{{"value", rawFloat(*res)}}
		}
	}
	return nil
}

func movingFnResult(src string, params map[string]any, vals []float64) (*float64, bool) {
	src = strings.ReplaceAll(src, " ", "")
	switch {
	case strings.Contains(src, "values.length"):
		v := float64(len(vals))
		return &v, true
	case strings.Contains(src, "MovingFunctions.stdDev(values,MovingFunctions.unweightedAvg(values))"):
		if len(vals) == 0 {
			return nil, true
		}
		avg := meanF(vals)
		sum := 0.0
		for _, v := range vals {
			d := v - avg
			sum += d * d
		}
		v := mathSqrt(sum / float64(len(vals)))
		return &v, true
	case strings.Contains(src, "MovingFunctions.linearWeightedAvg(values)"):
		if len(vals) == 0 {
			return nil, true
		}
		num, den := 0.0, 1.0
		for i, v := range vals {
			w := float64(i + 1)
			num += v * w
			den += w
		}
		v := num / den
		return &v, true
	case strings.Contains(src, "MovingFunctions.ewma(values"):
		if len(vals) == 0 {
			return nil, true
		}
		alpha := 0.5
		if a, ok := toFloat(params["alpha"]); ok {
			alpha = a
		} else if i := strings.Index(src, "ewma(values,"); i >= 0 {
			rest := strings.TrimPrefix(src[i:], "ewma(values,")
			if j := strings.Index(rest, ")"); j >= 0 {
				if a, err := strconv.ParseFloat(rest[:j], 64); err == nil {
					alpha = a
				}
			}
		}
		v := vals[0]
		for _, x := range vals[1:] {
			v = alpha*x + (1-alpha)*v
		}
		return &v, true
	case strings.Contains(src, "MovingFunctions.unweightedAvg(values)"):
		if len(vals) == 0 {
			return nil, true
		}
		v := meanF(vals)
		if k, ok := toFloat(params["k"]); ok {
			v *= k
		}
		return &v, true
	case strings.Contains(src, "MovingFunctions.max(values)"):
		if len(vals) == 0 {
			return nil, true
		}
		v := maxF(vals)
		return &v, true
	case strings.Contains(src, "MovingFunctions.min(values)"):
		if len(vals) == 0 {
			return nil, true
		}
		v := minF(vals)
		return &v, true
	case strings.Contains(src, "MovingFunctions.sum(values)"):
		v := sumOf(vals)
		if k, ok := toFloat(params["k"]); ok {
			v *= k
		}
		return &v, true
	}
	if f, err := strconv.ParseFloat(src, 64); err == nil {
		return &f, true
	}
	return nil, false
}

func applyMovingAvg(name string, body map[string]any, buckets []map[string]any) *apiError {
	path, _ := toString(body["buckets_path"])
	model, _ := toString(body["model"])
	if model == "" {
		model = "simple"
	}
	window := intField(body, "window", 5)
	gap, err := gapPolicyOf(body)
	if err != nil {
		return err
	}
	settings, _ := body["settings"].(map[string]any)
	alpha := 0.5
	if a, ok := toFloat(settings["alpha"]); ok {
		alpha = a
	}
	hist := []float64{}
	pos := []float64{}
	for i, b := range buckets {
		dc, _ := toFloat(b["doc_count"])
		if i == 0 {
			if dc > 0 {
				if v, ok := metricAtPath(b, path); ok {
					if f, ok := toFloat(v); ok {
						hist = append(hist, f)
						pos = append(pos, f)
					}
				}
			}
			continue
		}
		if dc == 0 && gap == "skip" {
			continue
		}
		var v float64
		switch model {
		case "linear":
			if len(hist) == 0 {
				b[name] = orderedJSON{{"value", nil}}
				break
			}
			src := hist
			if len(src) > window {
				src = src[len(src)-window:]
			}
			num, den := 0.0, 1.0
			for j, x := range src {
				w := float64(j + 1)
				num += x * w
				den += w
			}
			v = num / den
			b[name] = orderedJSON{{"value", rawFloat(v)}}
		case "ewma":
			if len(hist) == 0 {
				b[name] = orderedJSON{{"value", nil}}
				break
			}
			start := 0
			if len(hist) > window {
				start = len(hist) - window
			}
			v = hist[start]
			for _, x := range hist[start+1:] {
				v = alpha*x + (1-alpha)*v
			}
			b[name] = orderedJSON{{"value", rawFloat(v)}}
		default:
			src := hist
			if gap == "insert_zeros" {
				src = pos
			}
			if len(src) == 0 {
				b[name] = orderedJSON{{"value", nil}}
				break
			}
			if len(src) > window {
				src = src[len(src)-window:]
			}
			v = meanF(src)
			b[name] = orderedJSON{{"value", rawFloat(v)}}
		}
		if dc == 0 {
			pos = append(pos, 0)
		} else if v, ok := metricAtPath(b, path); ok {
			if f, ok := toFloat(v); ok {
				pos = append(pos, f)
				hist = append(hist, f)
			}
		}
	}
	return nil
}
