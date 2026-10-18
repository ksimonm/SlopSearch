package main

import (
	"sort"
	"strings"
	"time"
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
	idx *Index
	id  string
	doc map[string]any
}

func aggDocs(hits []aggHit) []map[string]any {
	out := make([]map[string]any, 0, len(hits))
	for _, h := range hits {
		out = append(out, h.doc)
	}
	return out
}

func evalAggs(aggs map[string]any, hits []aggHit, idx *Index, pathPrefix string) (map[string]any, *apiError) {
	docs := aggDocs(hits)
	out := map[string]any{}
	for name, raw := range aggs {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		res, err := evalOneAgg(def, docs, idx, pathPrefix)
		if err != nil {
			return nil, err
		}
		out[name] = res
	}
	return out, nil
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

func evalOneAgg(def map[string]any, docs []map[string]any, idx *Index, pathPrefix string) (any, *apiError) {
	if t, ok := def["terms"].(map[string]any); ok {
		field, _ := toString(t["field"])
		if fieldType(idx, resolveField(field, pathPrefix)) == "text" {
			return nil, &apiError{
				Type: "illegal_argument_exception", Status: 400,
				Reason: "Fielddata is disabled on text fields by default. Set fielddata=true on [" + field + "] in order to load field data by uninverting the inverted index.",
			}
		}
		return termsAgg(field, docs, idx, pathPrefix, subAggsOf(def), intField(t, "size", 10))
	}
	if t, ok := def["date_histogram"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return dateHistogramAgg(field, docs, idx, pathPrefix, subAggsOf(def))
	}
	if t, ok := def["range"].(map[string]any); ok {
		field, _ := toString(t["field"])
		return rangeAgg(field, t["ranges"], docs, idx, pathPrefix, subAggsOf(def))
	}
	if t, ok := def["nested"].(map[string]any); ok {
		path, _ := toString(t["path"])
		return nestedAgg(path, docs, idx, subAggsOf(def))
	}
	if t, ok := def["avg"].(map[string]any); ok {
		field, _ := toString(t["field"])
		vals := numericValues(docs, field, pathPrefix)
		return map[string]any{"value": mean(vals)}, nil
	}
	if t, ok := def["min"].(map[string]any); ok {
		field, _ := toString(t["field"])
		vals := numericValues(docs, field, pathPrefix)
		return map[string]any{"value": minOf(vals)}, nil
	}
	if t, ok := def["max"].(map[string]any); ok {
		field, _ := toString(t["field"])
		vals := numericValues(docs, field, pathPrefix)
		return map[string]any{"value": maxOf(vals)}, nil
	}
	if t, ok := def["sum"].(map[string]any); ok {
		field, _ := toString(t["field"])
		vals := numericValues(docs, field, pathPrefix)
		return map[string]any{"value": sumOf(vals)}, nil
	}
	if t, ok := def["stats"].(map[string]any); ok {
		field, _ := toString(t["field"])
		vals := numericValues(docs, field, pathPrefix)
		return map[string]any{
			"count": len(vals),
			"min":   minOf(vals),
			"max":   maxOf(vals),
			"avg":   mean(vals),
			"sum":   sumOf(vals),
		}, nil
	}
	if t, ok := def["cardinality"].(map[string]any); ok {
		field, _ := toString(t["field"])
		seen := map[string]bool{}
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				continue
			}
			for _, s := range docStrings(v) {
				seen[s] = true
			}
		}
		return map[string]any{"value": len(seen)}, nil
	}
	return map[string]any{}, nil
}

func termsAgg(field string, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, size int) (any, *apiError) {
	counts := map[string]int{}
	group := map[string][]map[string]any{}
	keys := []string{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		key, ok := toString(v)
		if !ok {
			continue
		}
		if _, seen := counts[key]; !seen {
			keys = append(keys, key)
		}
		counts[key]++
		group[key] = append(group[key], d)
	}
	sort.Slice(keys, func(i, j int) bool {
		if counts[keys[i]] != counts[keys[j]] {
			return counts[keys[i]] > counts[keys[j]]
		}
		return keys[i] < keys[j]
	})
	if size >= 0 && len(keys) > size {
		keys = keys[:size]
	}
	buckets := []any{}
	for _, k := range keys {
		b := map[string]any{"key": k, "doc_count": counts[k]}
		if sub != nil {
			subRes, err := evalAggs(sub, docsToHits(group[k], idx), idx, pathPrefix)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
	}
	return map[string]any{"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": buckets}, nil
}

func dateHistogramAgg(field string, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any) (any, *apiError) {
	counts := map[string]int{}
	group := map[string][]map[string]any{}
	keys := []string{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		s, ok := toString(v)
		if !ok {
			continue
		}
		parsed, err := time.Parse("2006-01-02", s[:min(10, len(s))])
		if err != nil {
			continue
		}
		key := parsed.Format("2006-01")
		if _, seen := counts[key]; !seen {
			keys = append(keys, key)
		}
		counts[key]++
		group[key] = append(group[key], d)
	}
	sort.Strings(keys)
	buckets := []any{}
	for _, k := range keys {
		parsed, _ := time.Parse("2006-01", k)
		b := map[string]any{
			"key_as_string": parsed.Format("2006-01-01T00:00:00.000Z"),
			"key":           parsed.UnixMilli(),
			"doc_count":     counts[k],
		}
		if sub != nil {
			subRes, err := evalAggs(sub, docsToHits(group[k], idx), idx, pathPrefix)
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

func rangeAgg(field string, rangesRaw any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any) (any, *apiError) {
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
			subRes, err := evalAggs(sub, docsToHits(bucketDocs, idx), idx, pathPrefix)
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

func nestedAgg(path string, docs []map[string]any, idx *Index, sub map[string]any) (any, *apiError) {
	var elements []map[string]any
	for _, d := range docs {
		v, ok := fieldValue(d, path)
		if !ok {
			continue
		}
		switch t := v.(type) {
		case []any:
			for _, e := range t {
				if m, ok := e.(map[string]any); ok {
					elements = append(elements, m)
				}
			}
		case map[string]any:
			elements = append(elements, t)
		}
	}
	res := map[string]any{"doc_count": len(elements)}
	if sub != nil {
		subRes, err := evalAggs(sub, docsToHits(elements, idx), idx, path)
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
		out = append(out, aggHit{idx: idx, id: "", doc: d})
	}
	return out
}
