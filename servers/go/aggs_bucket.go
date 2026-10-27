package main

import (
	"fmt"
	"math"
	"math/big"
	"net"
	"reflect"
	"sort"
	"strconv"
	"strings"
	"time"
)

func allIndexDocs(idx *Index) []map[string]any {
	out := []map[string]any{}
	for _, id := range idx.Order {
		doc, ok := idx.Docs[id]
		if !ok {
			continue
		}
		d := deepCopySource(doc).(map[string]any)
		d["__slop_id"] = id
		out = append(out, d)
	}
	return out
}

func globalAgg(sub map[string]any, idx *Index, pathPrefix string, typed bool) (any, *apiError) {
	docs := allIndexDocs(idx)
	out := map[string]any{"doc_count": len(docs)}
	if sub != nil {
		subRes, err := evalAggsMode(sub, docsToHits(docs, idx), idx, pathPrefix, typed, false)
		if err != nil {
			return nil, err
		}
		for k, v := range subRes {
			out[k] = v
		}
	}
	return out, nil
}

func globalSubError(name string, sub map[string]any) *apiError {
	for gname, raw := range sub {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		if _, ok := def["global"]; ok {
			return &apiError{Type: "aggregation_execution_exception", Status: 500,
				Reason: "Aggregation [" + name + "] cannot have a global sub-aggregation [" + gname + "]. Global aggregations can only be defined as top level aggregations"}
		}
	}
	return nil
}

func missingAgg(field string, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	resolved := resolveField(field, pathPrefix)
	nestedParent := false
	parts := strings.Split(resolved, ".")
	for i := 1; i < len(parts); i++ {
		parent := strings.Join(parts[:i], ".")
		def, ok := mappingFieldDef(idx, parent)
		if ok {
			if t, _ := def["type"].(string); t == "nested" {
				nestedParent = true
				break
			}
		}
	}
	var bucket []map[string]any
	for _, d := range docs {
		if nestedParent {
			bucket = append(bucket, d)
			continue
		}
		v, ok := fieldValue(d, resolved)
		if !ok || isEmptyAggValue(v) {
			bucket = append(bucket, d)
		}
	}
	out := map[string]any{"doc_count": len(bucket)}
	if sub != nil {
		subRes, err := evalAggsMode(sub, docsToHits(bucket, idx), idx, pathPrefix, typed, false)
		if err != nil {
			return nil, err
		}
		for k, v := range subRes {
			out[k] = v
		}
	}
	return out, nil
}

func isEmptyAggValue(v any) bool {
	switch t := v.(type) {
	case nil:
		return true
	case []any:
		return len(t) == 0
	}
	return false
}

func rangeAggSpec(name, field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	rawRanges, hasRanges := spec["ranges"]
	ranges, ok := rawRanges.([]any)
	if !hasRanges || !ok || len(ranges) == 0 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No [ranges] specified for the [" + name + "] aggregation"}
	}
	missing, hasMissing := spec["missing"]
	format, _ := toString(spec["format"])
	keyed, _ := spec["keyed"].(bool)
	type rng struct {
		from, to float64
		hasFrom  bool
		hasTo    bool
		key      string
		custom   bool
		fromStr  string
		toStr    string
		bucket   map[string]any
	}
	parsed := make([]*rng, 0, len(ranges))
	for _, rr := range ranges {
		r, ok := rr.(map[string]any)
		if !ok {
			continue
		}
		x := &rng{}
		if v, ok := toFloat(r["from"]); ok {
			x.from, x.hasFrom = v, true
		}
		if v, ok := toFloat(r["to"]); ok {
			x.to, x.hasTo = v, true
		}
		if k, ok := toString(r["key"]); ok {
			x.key, x.custom = k, true
		}
		parsed = append(parsed, x)
	}
	sort.SliceStable(parsed, func(i, j int) bool {
		a, b := parsed[i], parsed[j]
		if a.hasFrom != b.hasFrom {
			return !a.hasFrom
		}
		if a.hasFrom && a.from != b.from {
			return a.from < b.from
		}
		if a.hasTo != b.hasTo {
			return a.hasTo
		}
		if a.hasTo && a.to != b.to {
			return a.to < b.to
		}
		return false
	})
	buckets := []any{}
	keyedBuckets := orderedJSON{}
	for _, x := range parsed {
		var bucketDocs []map[string]any
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				if !hasMissing {
					continue
				}
				v = missing
			}
			n, ok := toFloat(v)
			if !ok {
				continue
			}
			if x.hasFrom && !(n >= x.from) {
				continue
			}
			if x.hasTo && !(n < x.to) {
				continue
			}
			bucketDocs = append(bucketDocs, d)
		}
		b := map[string]any{"doc_count": len(bucketDocs)}
		fromS, toS := "*", "*"
		if x.hasFrom {
			b["from"] = rawFloat(x.from)
			fromS = rangeKeyNumber(x.from)
			if format != "" {
				x.fromStr = fmtMetricNumber(x.from, format)
				b["from_as_string"] = x.fromStr
			}
		}
		if x.hasTo {
			b["to"] = rawFloat(x.to)
			toS = rangeKeyNumber(x.to)
			if format != "" {
				x.toStr = fmtMetricNumber(x.to, format)
				b["to_as_string"] = x.toStr
			}
		}
		key := fromS + "-" + toS
		if x.custom {
			key = x.key
		}
		if !keyed {
			b["key"] = key
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(bucketDocs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		if keyed {
			keyedBuckets = append(keyedBuckets, orderedKV{key, b})
		} else {
			buckets = append(buckets, b)
		}
	}
	if keyed {
		return map[string]any{"buckets": keyedBuckets}, nil
	}
	return map[string]any{"buckets": buckets}, nil
}

func rangeKeyNumber(f float64) string {
	s := strconv.FormatFloat(f, 'f', -1, 64)
	if !strings.Contains(s, ".") {
		s += ".0"
	}
	return s
}

func parseRangeDateValue(v any, def map[string]any, format, tz string, now time.Time) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case uint64:
		return int64(t), true
	case float64:
		return int64(t), true
	}
	s, ok := toString(v)
	if !ok {
		return 0, false
	}
	dateMath := ""
	if i := strings.Index(s, "||"); i >= 0 {
		dateMath = s[i+2:]
		s = s[:i]
	}
	base := now
	if s == "now" {
	} else if s != "" {
		tmp := def
		if format != "" {
			tmp = map[string]any{"type": "date", "format": format}
		}
		if ms, ok := parseDateAny(tmp, s); ok {
			if tz != "" {
				if off, err := parseTZOffset(tz); err == nil {
					ms -= off
				}
			}
			base = time.UnixMilli(ms).UTC()
		} else {
			return 0, false
		}
	}
	if dateMath != "" {
		base = applyDateMath(base, dateMath)
	}
	return base.UnixMilli(), true
}

func applyDateMath(t time.Time, mathExpr string) time.Time {
	for _, part := range strings.Split(mathExpr, "/") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		if part == "M" {
			t = time.Date(t.Year(), t.Month(), 1, 0, 0, 0, 0, t.Location())
			continue
		}
		if part == "d" {
			t = time.Date(t.Year(), t.Month(), t.Day(), 0, 0, 0, 0, t.Location())
			continue
		}
		if part == "H" {
			t = t.Truncate(time.Hour)
			continue
		}
		if part == "m" {
			t = t.Truncate(time.Minute)
			continue
		}
		if part == "s" {
			t = t.Truncate(time.Second)
			continue
		}
		if len(part) < 2 {
			continue
		}
		sign := 1
		if part[0] == '-' {
			sign = -1
		} else if part[0] != '+' {
			continue
		}
		i := 1
		for i < len(part) && part[i] >= '0' && part[i] <= '9' {
			i++
		}
		n, _ := strconv.Atoi(part[1:i])
		unit := part[i:]
		switch unit {
		case "y":
			t = t.AddDate(sign*n, 0, 0)
		case "M":
			t = t.AddDate(0, sign*n, 0)
		case "w":
			t = t.AddDate(0, 0, sign*n*7)
		case "d":
			t = t.AddDate(0, 0, sign*n)
		case "h", "H":
			t = t.Add(time.Duration(sign*n) * time.Hour)
		case "m":
			t = t.Add(time.Duration(sign*n) * time.Minute)
		case "s":
			t = t.Add(time.Duration(sign*n) * time.Second)
		}
	}
	return t
}

func dateRangeAgg(name, field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	rawRanges, hasRanges := spec["ranges"]
	ranges, ok := rawRanges.([]any)
	if !hasRanges || !ok || len(ranges) == 0 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No [ranges] specified for the [" + name + "] aggregation"}
	}
	resolved := resolveField(field, pathPrefix)
	def, _ := mappingFieldDef(idx, resolved)
	ft, _ := def["type"].(string)
	if ft == "" {
		ft = "date"
	}
	format, _ := toString(spec["format"])
	tz, _ := toString(spec["time_zone"])
	keyed, _ := spec["keyed"].(bool)
	missing, hasMissing := spec["missing"]
	now := time.Now().UTC()
	type rng struct {
		from, to     int64
		hasFrom      bool
		hasTo        bool
		key          string
		custom       bool
		fromS, toS   string
		fromMs, toMs int64
	}
	parsed := []*rng{}
	for _, rr := range ranges {
		r, ok := rr.(map[string]any)
		if !ok {
			continue
		}
		x := &rng{}
		if v, ok := r["from"]; ok {
			if ms, ok := parseRangeDateValue(v, def, format, tz, now); ok {
				x.from, x.hasFrom = ms, true
			}
		}
		if v, ok := r["to"]; ok {
			if ms, ok := parseRangeDateValue(v, def, format, tz, now); ok {
				x.to, x.hasTo = ms, true
			}
		}
		if k, ok := toString(r["key"]); ok {
			x.key, x.custom = k, true
		}
		parsed = append(parsed, x)
	}
	sort.SliceStable(parsed, func(i, j int) bool {
		a, b := parsed[i], parsed[j]
		if a.hasFrom != b.hasFrom {
			return !a.hasFrom
		}
		if a.hasFrom && a.from != b.from {
			return a.from < b.from
		}
		if a.hasTo != b.hasTo {
			return a.hasTo
		}
		return false
	})
	fmtDate := func(ms int64) string {
		loc := time.UTC
		if tz != "" {
			if off, err := parseTZOffset(tz); err == nil {
				loc = time.FixedZone(tz, int(off/1000))
			}
		}
		t := time.UnixMilli(ms).In(loc)
		if format != "" {
			if layout := javaDateFormatToGo(format); layout != "" {
				return t.Format(layout)
			}
		}
		if tz != "" && tz != "UTC" && tz != "Z" && tz != "+00:00" {
			return t.Format("2006-01-02T15:04:05.000-07:00")
		}
		return t.UTC().Format("2006-01-02T15:04:05.000Z")
	}
	buckets := []any{}
	keyedBuckets := orderedJSON{}
	for _, x := range parsed {
		var bucketDocs []map[string]any
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				if !hasMissing {
					continue
				}
				v = missing
			}
			ms, ok := dateMillisFor(idx, resolved, ft, v)
			if !ok {
				continue
			}
			if x.hasFrom && ms < x.from {
				continue
			}
			if x.hasTo && ms >= x.to {
				continue
			}
			bucketDocs = append(bucketDocs, d)
		}
		b := map[string]any{"doc_count": len(bucketDocs)}
		fromS, toS := "*", "*"
		if x.hasFrom {
			x.fromS = fmtDate(x.from)
			b["from"] = rawFloat(float64(x.from))
			b["from_as_string"] = x.fromS
			fromS = x.fromS
		}
		if x.hasTo {
			x.toS = fmtDate(x.to)
			b["to"] = rawFloat(float64(x.to))
			b["to_as_string"] = x.toS
			toS = x.toS
		}
		key := fromS + "-" + toS
		if x.custom {
			key = x.key
		}
		if !keyed {
			b["key"] = key
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(bucketDocs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
		if keyed {
			keyedBuckets = append(keyedBuckets, orderedKV{key, b})
		}
	}
	if keyed {
		return map[string]any{"buckets": keyedBuckets}, nil
	}
	return map[string]any{"buckets": buckets}, nil
}

func ipToBytes(s string) net.IP {
	ip := net.ParseIP(strings.TrimSpace(s))
	if ip == nil {
		return nil
	}
	if v4 := ip.To4(); v4 != nil {
		return v4
	}
	return ip.To16()
}

func ipLess(a, b net.IP) bool {
	aa, bb := a.To16(), b.To16()
	if aa == nil || bb == nil {
		return string(a) < string(b)
	}
	return string(aa) < string(bb)
}

func ipRangeAgg(name, field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	rawRanges, hasRanges := spec["ranges"]
	ranges, ok := rawRanges.([]any)
	if !hasRanges || !ok || len(ranges) == 0 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No [ranges] specified for the [" + name + "] aggregation"}
	}
	keyed, _ := spec["keyed"].(bool)
	type rng struct {
		from, to   net.IP
		hasFrom    bool
		hasTo      bool
		key        string
		custom     bool
		fromS, toS string
	}
	parsed := []*rng{}
	for _, rr := range ranges {
		r, ok := rr.(map[string]any)
		if !ok {
			continue
		}
		x := &rng{}
		if mask, ok := toString(r["mask"]); ok {
			_, ipnet, err := net.ParseCIDR(mask)
			if err != nil {
				prefix := ""
				if i := strings.LastIndex(mask, "/"); i >= 0 {
					prefix = mask[i+1:]
				}
				return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
					Reason: "Failed to parse aggregation [" + name + "]",
					CausedBy: &apiError{Type: "illegal_argument_exception",
						Reason: "Illegal prefix length [" + prefix + "] in [" + mask + "]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"}}
			}
			from := ipnet.IP
			bits := 32
			if from.To4() == nil {
				bits = 128
			}
			ones, _ := ipnet.Mask.Size()
			to := nextNetwork(from, bits, ones)
			x.from, x.hasFrom = from, true
			x.to, x.hasTo = to, true
			x.key = mask
			x.custom = true
			if k, ok := toString(r["key"]); ok {
				x.key = k
			}
		} else {
			if v, ok := toString(r["from"]); ok {
				ip := ipToBytes(v)
				if ip != nil {
					x.from, x.hasFrom = ip, true
					x.fromS = ip.String()
				}
			}
			if v, ok := toString(r["to"]); ok {
				ip := ipToBytes(v)
				if ip != nil {
					x.to, x.hasTo = ip, true
					x.toS = ip.String()
				}
			}
			if k, ok := toString(r["key"]); ok {
				x.key, x.custom = k, true
			}
		}
		parsed = append(parsed, x)
	}
	sort.SliceStable(parsed, func(i, j int) bool {
		a, b := parsed[i], parsed[j]
		if a.hasFrom != b.hasFrom {
			return !a.hasFrom
		}
		if a.hasFrom && !a.from.Equal(b.from) {
			return ipLess(a.from, b.from)
		}
		if a.hasTo != b.hasTo {
			return a.hasTo
		}
		return false
	})
	buckets := []any{}
	keyedBuckets := orderedJSON{}
	for _, x := range parsed {
		var bucketDocs []map[string]any
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				continue
			}
			s, _ := toString(v)
			ip := ipToBytes(s)
			if ip == nil {
				continue
			}
			if x.hasFrom && ipLess(ip, x.from) {
				continue
			}
			if x.hasTo && !ipLess(ip, x.to) {
				continue
			}
			bucketDocs = append(bucketDocs, d)
		}
		b := map[string]any{"doc_count": len(bucketDocs)}
		fromS, toS := "*", "*"
		if x.hasFrom {
			fromS = x.from.String()
			b["from"] = fromS
		}
		if x.hasTo {
			toS = x.to.String()
			b["to"] = toS
		}
		key := fromS + "-" + toS
		if x.custom {
			key = x.key
		}
		if !keyed {
			b["key"] = key
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(bucketDocs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
		if keyed {
			keyedBuckets = append(keyedBuckets, orderedKV{key, b})
		}
	}
	if keyed {
		return map[string]any{"buckets": keyedBuckets}, nil
	}
	return map[string]any{"buckets": buckets}, nil
}

func nextNetwork(ip net.IP, bits, ones int) net.IP {
	v4 := ip.To4()
	if v4 != nil {
		ip = v4
	} else {
		ip = ip.To16()
	}
	n := new(big.Int).SetBytes(ip)
	step := new(big.Int).Lsh(big.NewInt(1), uint(bits-ones))
	n.Add(n, step)
	b := n.Bytes()
	if len(b) < len(ip) {
		pad := make([]byte, len(ip)-len(b))
		b = append(pad, b...)
	}
	return net.IP(b)
}

func filtersAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	if _, has := spec["keyed"]; has {
		return nil, &apiError{Type: "parsing_exception", Status: 400,
			Reason: "Unknown key for a VALUE_BOOLEAN in [" + name + "]: [keyed]."}
	}
	raw, ok := spec["filters"]
	if !ok {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[filters] cannot be empty."}
	}
	otherKey, hasOtherKey := toString(spec["other_bucket_key"])
	other, _ := spec["other_bucket"].(bool)
	if hasOtherKey {
		other = true
	}
	buildBucket := func(filter map[string]any, bdocs []map[string]any) (map[string]any, *apiError) {
		b := map[string]any{"doc_count": len(bdocs)}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(bdocs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for k, v := range subRes {
				b[k] = v
			}
		}
		return b, nil
	}
	matchedAny := map[int]bool{}
	var otherDocs []map[string]any
	switch t := raw.(type) {
	case map[string]any:
		if len(t) == 0 {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[filters] cannot be empty."}
		}
		names := make([]string, 0, len(t))
		for k := range t {
			names = append(names, k)
		}
		sort.Strings(names)
		buckets := map[string]any{}
		for _, key := range names {
			filter, _ := t[key].(map[string]any)
			bdocs := []map[string]any{}
			for _, d := range docs {
				ok, _, err := evalQuery(idx, filter, d)
				if err != nil {
					return nil, err
				}
				if ok {
					bdocs = append(bdocs, d)
				}
			}
			b, err := buildBucket(filter, bdocs)
			if err != nil {
				return nil, err
			}
			buckets[key] = b
		}
		if other {
			otherDocs = unmatchedDocs(docs, t, idx)
			if len(otherDocs) > 0 || true {
				b, err := buildBucket(nil, otherDocs)
				if err != nil {
					return nil, err
				}
				ok := "_other_"
				if hasOtherKey {
					ok = otherKey
				}
				buckets[ok] = b
			}
		}
		return map[string]any{"buckets": buckets}, nil
	case []any:
		if len(t) == 0 {
			return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[filters] cannot be empty."}
		}
		buckets := []any{}
		for i, rawFilter := range t {
			filter, _ := rawFilter.(map[string]any)
			bdocs := []map[string]any{}
			for _, d := range docs {
				ok, _, err := evalQuery(idx, filter, d)
				if err != nil {
					return nil, err
				}
				if ok {
					bdocs = append(bdocs, d)
					matchedAny[i] = true
				}
			}
			b, err := buildBucket(filter, bdocs)
			if err != nil {
				return nil, err
			}
			buckets = append(buckets, b)
		}
		if other {
			for _, d := range docs {
				matched := false
				for i, rawFilter := range t {
					filter, _ := rawFilter.(map[string]any)
					ok, _, _ := evalQuery(idx, filter, d)
					if ok {
						matched = true
						_ = i
						break
					}
				}
				if !matched {
					otherDocs = append(otherDocs, d)
				}
			}
			b, err := buildBucket(nil, otherDocs)
			if err != nil {
				return nil, err
			}
			buckets = append(buckets, b)
		}
		return map[string]any{"buckets": buckets}, nil
	}
	return nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "[filters] cannot be empty."}
}

func unmatchedDocs(docs []map[string]any, filters map[string]any, idx *Index) []map[string]any {
	out := []map[string]any{}
	for _, d := range docs {
		matched := false
		for _, raw := range filters {
			filter, _ := raw.(map[string]any)
			ok, _, _ := evalQuery(idx, filter, d)
			if ok {
				matched = true
				break
			}
		}
		if !matched {
			out = append(out, d)
		}
	}
	return out
}

func multiTermsAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	terms, ok := spec["terms"].([]any)
	if !ok || len(terms) < 2 {
		n := 0
		if ok {
			n = len(terms)
		}
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "Failed to parse aggregation [" + name + "]",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: fmt.Sprintf("multi term aggregation must has at least 2 terms. Found [%d] in [%s] Use terms aggregation for single term aggregation", n, name)}}
	}
	type fieldInfo struct {
		name     string
		resolved string
		ft       string
		missing  any
		hasMiss  bool
	}
	infos := make([]fieldInfo, 0, len(terms))
	for _, raw := range terms {
		m, _ := raw.(map[string]any)
		f, _ := toString(m["field"])
		resolved := resolveField(f, pathPrefix)
		ft, _ := idx.FieldTypes[resolved]
		mi, hm := m["missing"]
		infos = append(infos, fieldInfo{name: f, resolved: resolved, ft: ft, missing: mi, hasMiss: hm})
	}
	size := intField(spec, "size", 10)
	type mtBucket struct {
		key   []any
		kas   string
		docs  []map[string]any
		canon string
	}
	groups := map[string]*mtBucket{}
	orderKeys := []string{}
	for _, d := range docs {
		combos := [][]any{{}}
		for _, fi := range infos {
			v, ok := fieldValue(d, fi.resolved)
			var vals []any
			if ok {
				vals = flattenTermValues(v)
			}
			if len(vals) == 0 {
				if fi.hasMiss {
					vals = []any{fi.missing}
				} else {
					combos = nil
					break
				}
			}
			next := [][]any{}
			for _, c := range combos {
				for _, v := range vals {
					n := append(append([]any{}, c...), v)
					next = append(next, n)
				}
			}
			combos = next
		}
		for _, combo := range combos {
			keys := make([]any, len(combo))
			kasParts := make([]string, len(combo))
			for i, v := range combo {
				fi := infos[i]
				k, kas, hasKas, _, ok := termKeyFor(v, fi.ft, "", "", idx, fi.resolved)
				if !ok {
					k = v
				}
				if fi.ft == "date" || fi.ft == "date_nanos" {
					keys[i] = kas
					kasParts[i] = kas
				} else {
					keys[i] = k
					if hasKas {
						kasParts[i] = kas
					} else {
						s, _ := toString(k)
						kasParts[i] = s
					}
				}
			}
			canon := ""
			for _, k := range keys {
				canon += canonicalTermKey(k) + "|"
			}
			tb := groups[canon]
			if tb == nil {
				tb = &mtBucket{key: keys, kas: strings.Join(kasParts, "|"), canon: canon}
				groups[canon] = tb
				orderKeys = append(orderKeys, canon)
			}
			tb.docs = append(tb.docs, d)
		}
	}
	bmaps := []map[string]any{}
	for _, canon := range orderKeys {
		tb := groups[canon]
		b := map[string]any{"key": tb.key, "key_as_string": tb.kas, "doc_count": len(tb.docs)}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(tb.docs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for k, v := range subRes {
				b[k] = v
			}
		}
		bmaps = append(bmaps, b)
	}
	orders, oerr := parseTermOrders(spec["order"])
	if oerr != nil {
		return nil, oerr
	}
	if len(orders) == 0 {
		orders = []termOrder{{field: "_count", desc: true}}
	}
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
				av, aok = multiKeyString(bmaps[i]["key"]), true
				bv, bok = multiKeyString(bmaps[j]["key"]), true
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
	sumOther := 0
	if len(bmaps) > size {
		for _, b := range bmaps[size:] {
			dc, _ := toFloat(b["doc_count"])
			sumOther += int(dc)
		}
		bmaps = bmaps[:size]
	}
	buckets := []any{}
	for _, b := range bmaps {
		buckets = append(buckets, b)
	}
	return map[string]any{"doc_count_error_upper_bound": 0, "sum_other_doc_count": sumOther, "buckets": buckets}, nil
}

func multiKeyString(v any) string {
	arr, _ := v.([]any)
	parts := make([]string, len(arr))
	for i, e := range arr {
		s, _ := toString(e)
		parts[i] = s
	}
	return strings.Join(parts, "|")
}

func fmtIP(ip net.IP) string {
	return ip.String()
}

var _ = math.Floor

type compositeSource struct {
	name          string
	kind          string
	field         string
	missingBucket bool
	desc          bool
	script        any
	ft            string
	interval      float64
	calUnit       string
	fixedMs       int64
	isFixed       bool
	loc           *time.Location
	format        string
	tz            string
	offsetMs      int64
}

func compositeAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	rawSources, _ := spec["sources"].([]any)
	if len(rawSources) == 0 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Composite [sources] cannot be null or empty."}
	}
	srcs := []*compositeSource{}
	for _, raw := range rawSources {
		m, ok := raw.(map[string]any)
		if !ok || len(m) == 0 {
			continue
		}
		for srcName, rawDef := range m {
			def, _ := rawDef.(map[string]any)
			src := &compositeSource{name: srcName, loc: time.UTC}
			for kind, rawSpec := range def {
				ss, _ := rawSpec.(map[string]any)
				switch kind {
				case "terms":
					src.kind = "terms"
					src.field, _ = toString(ss["field"])
					src.script = ss["script"]
					src.missingBucket, _ = ss["missing_bucket"].(bool)
					if o, _ := toString(ss["order"]); o == "desc" {
						src.desc = true
					}
				case "histogram":
					src.kind = "histogram"
					src.field, _ = toString(ss["field"])
					src.interval, _ = toFloat(ss["interval"])
					if src.interval <= 0 {
						src.interval = 1
					}
					src.missingBucket, _ = ss["missing_bucket"].(bool)
					if o, _ := toString(ss["order"]); o == "desc" {
						src.desc = true
					}
					src.offsetMs = int64(0)
					if off, ok := toFloat(ss["offset"]); ok {
						src.offsetMs = int64(off)
					}
				case "date_histogram":
					src.kind = "date_histogram"
					src.field, _ = toString(ss["field"])
					src.format, _ = toString(ss["format"])
					src.tz, _ = toString(ss["time_zone"])
					loc, terr := loadAggLocation(src.tz)
					if terr != nil {
						return nil, terr
					}
					src.loc = loc
					if off, ok := toString(ss["offset"]); ok {
						src.offsetMs = parseDateOffsetMillis(off)
					}
					if v, ok := ss["calendar_interval"]; ok {
						s, _ := toString(v)
						u, ok := parseCalendarUnit(s)
						if !ok {
							return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
								Reason: "Failed to parse aggregation [" + name + "]",
								CausedBy: &apiError{Type: "illegal_argument_exception",
									Reason: "The supplied interval [" + s + "] could not be parsed as a calendar interval."}}
						}
						src.calUnit = u
					} else if v, ok := ss["fixed_interval"]; ok {
						s, _ := toString(v)
						ms, ok := parseFixedInterval(s)
						if !ok {
							return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
								Reason: "Failed to parse aggregation [" + name + "]",
								CausedBy: &apiError{Type: "illegal_argument_exception",
									Reason: "failed to parse setting [date_histogram.fixedInterval] with value [" + s + "] as a time value: unit is missing or unrecognized"}}
						}
						src.fixedMs, src.isFixed = ms, true
					} else if v, ok := ss["interval"]; ok {
						s, _ := toString(v)
						if u, ok := parseCalendarUnit(s); ok {
							src.calUnit = u
						} else if ms, ok := parseFixedInterval(s); ok {
							src.fixedMs, src.isFixed = ms, true
						}
					}
					src.missingBucket, _ = ss["missing_bucket"].(bool)
					if o, _ := toString(ss["order"]); o == "desc" {
						src.desc = true
					}
				}
			}
			if src.kind == "" {
				continue
			}
			src.ft = idx.FieldTypes[resolveField(src.field, pathPrefix)]
			srcs = append(srcs, src)
		}
	}
	if len(srcs) == 0 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Composite [sources] cannot be null or empty."}
	}
	afterRaw, hasAfter := spec["after"].(map[string]any)
	if hasAfter {
		for _, src := range srcs {
			if _, ok := afterRaw[src.name]; !ok {
				return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Missing value for [after." + src.name + "]"}
			}
		}
	}
	type combo struct {
		vals []any
		docs []map[string]any
	}
	groups := map[string]*combo{}
	for _, d := range docs {
		lists := make([][]any, len(srcs))
		ok := true
		for i, src := range srcs {
			vals, has := compositeValues(src, d, idx, pathPrefix)
			if !has {
				ok = false
				break
			}
			lists[i] = vals
		}
		if !ok {
			continue
		}
		var rec func(i int, vals []any)
		rec = func(i int, vals []any) {
			if i == len(lists) {
				ck := compositeCanon(vals)
				g := groups[ck]
				if g == nil {
					cp := make([]any, len(vals))
					copy(cp, vals)
					g = &combo{vals: cp}
					groups[ck] = g
				}
				g.docs = append(g.docs, d)
				return
			}
			seen := map[string]bool{}
			for _, v := range lists[i] {
				ck := compositeCanon([]any{v})
				if seen[ck] {
					continue
				}
				seen[ck] = true
				rec(i+1, append(vals, v))
			}
		}
		rec(0, nil)
	}
	combos := make([]*combo, 0, len(groups))
	for _, g := range groups {
		combos = append(combos, g)
	}
	sort.SliceStable(combos, func(i, j int) bool {
		for k, src := range srcs {
			cmp := compositeCompare(combos[i].vals[k], combos[j].vals[k])
			if cmp != 0 {
				if src.desc {
					return cmp > 0
				}
				return cmp < 0
			}
		}
		return false
	})
	if hasAfter {
		afterVals := make([]any, len(srcs))
		for i, src := range srcs {
			afterVals[i] = afterRaw[src.name]
		}
		filtered := combos[:0]
		for _, c := range combos {
			if compositeAfter(c.vals, afterVals, srcs) {
				filtered = append(filtered, c)
			}
		}
		combos = filtered
	}
	size := intField(spec, "size", 10)
	if size < 0 {
		size = 0
	}
	if len(combos) > size {
		combos = combos[:size]
	}
	buckets := []any{}
	for _, c := range combos {
		key := orderedJSON{}
		for i, src := range srcs {
			v := c.vals[i]
			if src.kind == "histogram" {
				if f, ok := toFloat(v); ok {
					v = rawFloat(f)
				}
			}
			key = append(key, orderedKV{src.name, v})
		}
		b := map[string]any{"key": key, "doc_count": len(c.docs)}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(c.docs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
	}
	out := map[string]any{"buckets": buckets}
	if len(buckets) > 0 {
		last := buckets[len(buckets)-1].(map[string]any)
		out["after_key"] = last["key"]
	}
	return out, nil
}

func compositeValues(src *compositeSource, doc map[string]any, idx *Index, pathPrefix string) ([]any, bool) {
	if src.kind == "terms" && src.script != nil {
		v, ok := evalTermScript(doc, idx, src.script)
		if !ok {
			if src.missingBucket {
				return []any{nil}, true
			}
			return nil, false
		}
		return []any{v}, true
	}
	resolved := resolveField(src.field, pathPrefix)
	v, ok := aggFieldValue(doc, src.field, pathPrefix)
	if !ok || isEmptyAggValue(v) {
		if src.missingBucket {
			return []any{nil}, true
		}
		return nil, false
	}
	switch src.kind {
	case "terms":
		flat := flattenTermValues(v)
		if len(flat) == 0 {
			if src.missingBucket {
				return []any{nil}, true
			}
			return nil, false
		}
		out := []any{}
		for _, e := range flat {
			switch src.ft {
			case "byte", "short", "integer", "long", "unsigned_long":
				if f, ok := toFloat(e); ok {
					out = append(out, int64(f))
				} else {
					out = append(out, fmt.Sprint(e))
				}
			case "float", "double", "half_float", "scaled_float":
				if f, ok := toFloat(e); ok {
					out = append(out, f)
				} else {
					out = append(out, fmt.Sprint(e))
				}
			case "boolean":
				if b, ok := e.(bool); ok {
					out = append(out, b)
				} else if f, ok := toFloat(e); ok {
					out = append(out, f != 0)
				}
			default:
				s, _ := toString(e)
				out = append(out, s)
			}
		}
		return out, true
	case "histogram":
		f, ok := toFloat(v)
		if !ok {
			if src.missingBucket {
				return []any{nil}, true
			}
			return nil, false
		}
		k := math.Floor((f-float64(src.offsetMs))/src.interval)*src.interval + float64(src.offsetMs)
		return []any{k}, true
	case "date_histogram":
		ms, ok := dateMillisFor(idx, resolved, src.ft, v)
		if !ok {
			if src.missingBucket {
				return []any{nil}, true
			}
			return nil, false
		}
		shifted := ms - src.offsetMs
		var k int64
		if src.isFixed && !(src.fixedMs%86400000 == 0 && src.loc != time.UTC) {
			k = int64(math.Floor(float64(shifted)/float64(src.fixedMs))) * src.fixedMs
		} else if src.isFixed {
			k = calendarBucketStart(shifted, "day", src.loc)
		} else {
			k = calendarBucketStart(shifted, src.calUnit, src.loc)
		}
		k += src.offsetMs
		if src.format != "" {
			return []any{formatDateHistKey(k, src.format, src.loc, src.tz != "")}, true
		}
		return []any{k}, true
	}
	return nil, false
}

func compositeCanon(vals []any) string {
	var b strings.Builder
	for _, v := range vals {
		switch t := v.(type) {
		case nil:
			b.WriteString("n:")
		case string:
			b.WriteString("s:")
			b.WriteString(t)
		case bool:
			if t {
				b.WriteString("b:1")
			} else {
				b.WriteString("b:0")
			}
		case int64:
			b.WriteString("i:")
			b.WriteString(strconv.FormatInt(t, 10))
		default:
			if f, ok := toFloat(v); ok {
				b.WriteString("f:")
				b.WriteString(strconv.FormatFloat(f, 'g', -1, 64))
			} else {
				b.WriteString("x:")
				b.WriteString(fmt.Sprint(v))
			}
		}
		b.WriteByte(0)
	}
	return b.String()
}

func compositeCompare(a, b any) int {
	if a == nil || b == nil {
		if a == nil && b == nil {
			return 0
		}
		if a == nil {
			return -1
		}
		return 1
	}
	if as, ok := a.(string); ok {
		bs, _ := b.(string)
		if as < bs {
			return -1
		}
		if as > bs {
			return 1
		}
		return 0
	}
	if ab, ok := a.(bool); ok {
		bb, _ := b.(bool)
		if ab == bb {
			return 0
		}
		if !ab {
			return -1
		}
		return 1
	}
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
	as, bs := fmt.Sprint(a), fmt.Sprint(b)
	if as < bs {
		return -1
	}
	if as > bs {
		return 1
	}
	return 0
}

func compositeAfter(vals, after []any, srcs []*compositeSource) bool {
	for i := range srcs {
		cmp := compositeCompare(vals[i], after[i])
		if cmp != 0 {
			if srcs[i].desc {
				return cmp < 0
			}
			return cmp > 0
		}
	}
	return false
}

func geohashEncode(lat, lon float64, precision int) string {
	const base32 = "0123456789bcdefghjkmnpqrstuvwxyz"
	latLo, latHi := -90.0, 90.0
	lonLo, lonHi := -180.0, 180.0
	out := []byte{}
	bit, ch := 0, 0
	even := true
	for len(out) < precision {
		if even {
			mid := (lonLo + lonHi) / 2
			if lon >= mid {
				ch = ch<<1 | 1
				lonLo = mid
			} else {
				ch <<= 1
				lonHi = mid
			}
		} else {
			mid := (latLo + latHi) / 2
			if lat >= mid {
				ch = ch<<1 | 1
				latLo = mid
			} else {
				ch <<= 1
				latHi = mid
			}
		}
		even = !even
		bit++
		if bit == 5 {
			out = append(out, base32[ch])
			bit, ch = 0, 0
		}
	}
	return string(out)
}

func geotileKey(lat, lon float64, precision int) string {
	n := math.Pow(2, float64(precision))
	x := int(math.Floor((lon + 180) / 360 * n))
	rad := lat * math.Pi / 180
	y := int(math.Floor((1 - math.Log(math.Tan(rad)+1/math.Cos(rad))/math.Pi) / 2 * n))
	if x < 0 {
		x = 0
	}
	if y < 0 {
		y = 0
	}
	if x >= int(n) {
		x = int(n) - 1
	}
	if y >= int(n) {
		y = int(n) - 1
	}
	return fmt.Sprintf("%d/%d/%d", precision, x, y)
}

func geoHashGridAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool, tile bool) (any, *apiError) {
	field, _ := toString(spec["field"])
	precision := intField(spec, "precision", 5)
	maxP := 12
	minP := 1
	aggName := "geohash aggregation"
	if tile {
		maxP = 29
		minP = 0
		aggName = "geotile_grid"
	}
	if precision < minP || precision > maxP {
		return nil, &apiError{Type: "x_content_parse_exception", Status: 400,
			Reason: "Failed to parse aggregation [" + name + "]",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: fmt.Sprintf("Invalid %s precision of %d. Must be between %d and %d.", aggName, precision, minP, maxP)}}
	}
	counts := map[string]int{}
	group := map[string][]map[string]any{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		lat, lon, ok := geoPointOf(v)
		if !ok {
			continue
		}
		var key string
		if tile {
			key = geotileKey(lat, lon, precision)
		} else {
			if precision == 0 {
				continue
			}
			key = geohashEncode(lat, lon, precision)
		}
		counts[key]++
		group[key] = append(group[key], d)
	}
	keys := []string{}
	for k := range counts {
		keys = append(keys, k)
	}
	sort.SliceStable(keys, func(i, j int) bool {
		if counts[keys[i]] != counts[keys[j]] {
			return counts[keys[i]] > counts[keys[j]]
		}
		return keys[i] > keys[j]
	})
	size := intField(spec, "size", 10)
	if size > 0 && len(keys) > size {
		keys = keys[:size]
	}
	buckets := []any{}
	for _, k := range keys {
		b := map[string]any{"key": k, "doc_count": counts[k]}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(group[k], idx), idx, pathPrefix, typed, false)
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

func geoDistanceAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	field, _ := toString(spec["field"])
	olat, olon, ok := geoPointOf(spec["origin"])
	if !ok {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "failed to parse geo_distance point"}
	}
	unit, _ := toString(spec["unit"])
	unitM := unitToMeters(unit)
	if unitM == 0 {
		unitM = 1
	}
	rawRanges, _ := spec["ranges"].([]any)
	keyed, _ := spec["keyed"].(bool)
	type gr struct {
		from, to                     float64
		hasFrom, hasTo, explicitFrom bool
		key                          string
	}
	parsed := []*gr{}
	for _, rr := range rawRanges {
		r, _ := rr.(map[string]any)
		x := &gr{}
		if v, ok := toFloat(r["from"]); ok {
			x.from, x.hasFrom, x.explicitFrom = v, true, true
		} else {
			x.from, x.hasFrom = 0, true
		}
		if v, ok := toFloat(r["to"]); ok {
			x.to, x.hasTo = v, true
		}
		if k, ok := toString(r["key"]); ok {
			x.key = k
		}
		parsed = append(parsed, x)
	}
	sort.SliceStable(parsed, func(i, j int) bool {
		if parsed[i].from != parsed[j].from {
			return parsed[i].from < parsed[j].from
		}
		return parsed[i].to < parsed[j].to
	})
	buckets := []any{}
	keyedBuckets := orderedJSON{}
	for _, x := range parsed {
		var bdocs []map[string]any
		for _, d := range docs {
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				continue
			}
			lat, lon, ok := geoPointOf(v)
			if !ok {
				continue
			}
			dist := haversineMeters(olat, olon, lat, lon) / unitM
			if dist < x.from {
				continue
			}
			if x.hasTo && dist >= x.to {
				continue
			}
			bdocs = append(bdocs, d)
		}
		fromS, toS := "*", "*"
		if x.explicitFrom {
			fromS = formatFloatOne(x.from)
		}
		if x.hasTo {
			toS = formatFloatOne(x.to)
		}
		key := fromS + "-" + toS
		if x.key != "" {
			key = x.key
		}
		b := map[string]any{"from": rawFloat(x.from), "doc_count": len(bdocs)}
		if x.hasTo {
			b["to"] = rawFloat(x.to)
		}
		if !keyed {
			b["key"] = key
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(bdocs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		if keyed {
			keyedBuckets = append(keyedBuckets, orderedKV{key, b})
		} else {
			buckets = append(buckets, b)
		}
	}
	if keyed {
		return map[string]any{"buckets": keyedBuckets}, nil
	}
	return map[string]any{"buckets": buckets}, nil
}

func adjacencyMatrixAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	raw, ok := spec["filters"].(map[string]any)
	if !ok || len(raw) == 0 {
		return nil, &apiError{Type: "aggregation_execution_exception", Status: 500,
			Reason: "[" + name + "] is missing : filters parameter"}
	}
	sep, _ := toString(spec["separator"])
	if sep == "" {
		sep = "&"
	}
	keys := []string{}
	for k := range raw {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	matches := map[string][]map[string]any{}
	for _, k := range keys {
		q, _ := raw[k].(map[string]any)
		for _, d := range docs {
			ok, _, err := evalQuery(idx, q, d)
			if err != nil {
				return nil, err
			}
			if ok {
				matches[k] = append(matches[k], d)
			}
		}
	}
	type abucket struct {
		key  string
		docs []map[string]any
	}
	abs := []abucket{}
	for _, k := range keys {
		if len(matches[k]) > 0 {
			abs = append(abs, abucket{k, matches[k]})
		}
	}
	for i := 0; i < len(keys); i++ {
		for j := i + 1; j < len(keys); j++ {
			var both []map[string]any
			for _, d := range matches[keys[i]] {
				for _, e := range matches[keys[j]] {
					if reflect.ValueOf(d).Pointer() == reflect.ValueOf(e).Pointer() {
						both = append(both, d)
						break
					}
				}
			}
			if len(both) > 0 {
				abs = append(abs, abucket{keys[i] + sep + keys[j], both})
			}
		}
	}
	sort.SliceStable(abs, func(i, j int) bool { return abs[i].key < abs[j].key })
	buckets := []any{}
	for _, ab := range abs {
		b := map[string]any{"key": ab.key, "doc_count": len(ab.docs)}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(ab.docs, idx), idx, pathPrefix, typed, false)
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

func samplerAgg(spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool, diversified bool) (any, *apiError) {
	if sub == nil {
		return nil, &apiError{Type: "aggregation_execution_exception", Status: 500,
			Reason: "Sampler aggregation must be used with child aggregations."}
	}
	shardSize := intField(spec, "shard_size", 100)
	if shardSize <= 0 {
		shardSize = 100
	}
	sorted := make([]map[string]any, len(docs))
	copy(sorted, docs)
	sort.SliceStable(sorted, func(i, j int) bool {
		si, _ := toFloat(sorted[i]["__slop_score"])
		sj, _ := toFloat(sorted[j]["__slop_score"])
		return si > sj
	})
	selected := []map[string]any{}
	if !diversified {
		if len(sorted) > shardSize {
			sorted = sorted[:shardSize]
		}
		selected = sorted
	} else {
		field, _ := toString(spec["field"])
		maxPer := intField(spec, "max_docs_per_value", 1)
		if maxPer <= 0 {
			maxPer = 1
		}
		counts := map[string]int{}
		for _, d := range sorted {
			if len(selected) >= shardSize {
				break
			}
			v, ok := aggFieldValue(d, field, pathPrefix)
			if !ok {
				continue
			}
			vals := flattenTermValues(v)
			if len(vals) == 0 {
				continue
			}
			key, _ := toString(vals[0])
			if counts[key] >= maxPer {
				continue
			}
			counts[key]++
			selected = append(selected, d)
		}
	}
	res := map[string]any{"doc_count": len(selected)}
	subRes, err := evalAggsMode(sub, docsToHits(selected, idx), idx, pathPrefix, typed, false)
	if err != nil {
		return nil, err
	}
	for sk, sv := range subRes {
		res[sk] = sv
	}
	return res, nil
}

type autoInterval struct {
	label   string
	fixedMs int64
	unit    string
	months  int
}

var autoIntervals = []autoInterval{
	{label: "1s", fixedMs: 1000},
	{label: "5s", fixedMs: 5000},
	{label: "10s", fixedMs: 10000},
	{label: "30s", fixedMs: 30000},
	{label: "1m", fixedMs: 60000},
	{label: "5m", fixedMs: 300000},
	{label: "10m", fixedMs: 600000},
	{label: "30m", fixedMs: 1800000},
	{label: "1h", fixedMs: 3600000},
	{label: "3h", fixedMs: 10800000},
	{label: "12h", fixedMs: 43200000},
	{label: "1d", unit: "day"},
	{label: "7d", unit: "week"},
	{label: "1M", months: 1},
	{label: "3M", months: 3},
	{label: "1y", months: 12},
	{label: "2y", months: 24},
	{label: "5y", months: 60},
	{label: "10y", months: 120},
	{label: "20y", months: 240},
	{label: "50y", months: 600},
	{label: "100y", months: 1200},
}

func autoStart(ms int64, iv autoInterval, loc *time.Location) int64 {
	if iv.fixedMs > 0 {
		return int64(math.Floor(float64(ms)/float64(iv.fixedMs))) * iv.fixedMs
	}
	if iv.months > 0 {
		t := time.UnixMilli(ms).In(loc)
		y, mo, _ := t.Date()
		if iv.months >= 12 {
			mo = time.January
		}
		return time.Date(y, mo, 1, 0, 0, 0, 0, loc).UnixMilli()
	}
	return calendarBucketStart(ms, iv.unit, loc)
}

func autoNext(ms int64, iv autoInterval, loc *time.Location) int64 {
	if iv.fixedMs > 0 {
		return ms + iv.fixedMs
	}
	if iv.months > 0 {
		t := time.UnixMilli(ms).In(loc)
		y, mo, _ := t.Date()
		total := y*12 + int(mo) - 1 + iv.months
		return time.Date(total/12, time.Month(total%12+1), 1, 0, 0, 0, 0, loc).UnixMilli()
	}
	return addCalendarUnit(ms, iv.unit, loc)
}

func autoBucketStart(ms int64, iv autoInterval, loc *time.Location, origin int64) int64 {
	if iv.fixedMs > 0 {
		return int64(math.Floor(float64(ms)/float64(iv.fixedMs))) * iv.fixedMs
	}
	if iv.months > 0 {
		t := time.UnixMilli(ms).In(loc)
		o := time.UnixMilli(origin).In(loc)
		diff := (t.Year()-o.Year())*12 + int(t.Month()) - int(o.Month())
		if diff < 0 {
			diff = 0
		}
		n := diff / iv.months
		k := origin
		for i := 0; i < n; i++ {
			k = autoNext(k, iv, loc)
		}
		return k
	}
	k := origin
	for {
		nk := autoNext(k, iv, loc)
		if nk <= k || nk > ms {
			return k
		}
		k = nk
	}
}

func autoDateHistogramAgg(field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	target := intField(spec, "buckets", 10)
	if target <= 0 {
		target = 10
	}
	resolved := resolveField(field, pathPrefix)
	ft, _ := idx.FieldTypes[resolved]
	format, _ := toString(spec["format"])
	tz, _ := toString(spec["time_zone"])
	loc, terr := loadAggLocation(tz)
	if terr != nil {
		return nil, terr
	}
	vals := []int64{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		if ms, ok := dateMillisFor(idx, resolved, ft, v); ok {
			vals = append(vals, ms)
		}
	}
	if len(vals) == 0 {
		return map[string]any{"buckets": []any{}, "interval": "1s"}, nil
	}
	minV, maxV := vals[0], vals[0]
	for _, v := range vals {
		if v < minV {
			minV = v
		}
		if v > maxV {
			maxV = v
		}
	}
	chosen := autoIntervals[len(autoIntervals)-1]
	for _, iv := range autoIntervals {
		lo := autoStart(minV, iv, loc)
		hi := autoBucketStart(maxV, iv, loc, lo)
		count := 0
		for k := lo; k <= hi && count <= target; k = autoNext(k, iv, loc) {
			count++
			if count > target {
				break
			}
		}
		if count <= target {
			chosen = iv
			break
		}
	}
	lo := autoStart(minV, chosen, loc)
	hi := autoBucketStart(maxV, chosen, loc, lo)
	counts := map[int64]int{}
	group := map[int64][]map[string]any{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		ms, ok := dateMillisFor(idx, resolved, ft, v)
		if !ok {
			continue
		}
		k := autoBucketStart(ms, chosen, loc, lo)
		counts[k]++
		group[k] = append(group[k], d)
	}
	buckets := []any{}
	for k := lo; ; k = autoNext(k, chosen, loc) {
		b := map[string]any{"key": k, "doc_count": counts[k]}
		if ft != "integer" && ft != "long" && ft != "double" && ft != "float" {
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
		buckets = append(buckets, b)
		if k >= hi {
			break
		}
		nk := autoNext(k, chosen, loc)
		if nk <= k {
			break
		}
	}
	return map[string]any{"buckets": buckets, "interval": chosen.label}, nil
}

func variableWidthHistogramAgg(field string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	target := intField(spec, "buckets", 10)
	if target <= 0 {
		target = 10
	}
	type vb struct {
		min, max float64
		count    int
		sum      float64
		docs     []map[string]any
	}
	byVal := map[float64]*vb{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		for _, e := range flattenTermValues(v) {
			f, ok := toFloat(e)
			if !ok {
				continue
			}
			b := byVal[f]
			if b == nil {
				b = &vb{min: f, max: f}
				byVal[f] = b
			}
			b.count++
			b.sum += f
			b.docs = append(b.docs, d)
		}
	}
	vals := []float64{}
	for f := range byVal {
		vals = append(vals, f)
	}
	sort.Float64s(vals)
	bs := []*vb{}
	for _, f := range vals {
		bs = append(bs, byVal[f])
	}
	for len(bs) > target {
		best := 0
		bestD := math.Inf(1)
		for i := 0; i+1 < len(bs); i++ {
			a, b := bs[i], bs[i+1]
			d := (b.sum / float64(b.count)) - (a.sum / float64(a.count))
			if d < bestD {
				bestD = d
				best = i
			}
		}
		a, b := bs[best], bs[best+1]
		m := &vb{min: a.min, max: b.max, count: a.count + b.count, sum: a.sum + b.sum}
		m.docs = append(m.docs, a.docs...)
		m.docs = append(m.docs, b.docs...)
		bs = append(bs[:best], append([]*vb{m}, bs[best+2:]...)...)
	}
	buckets := []any{}
	for _, b := range bs {
		bb := map[string]any{
			"min": rawFloat(b.min), "key": rawFloat(b.sum / float64(b.count)),
			"max": rawFloat(b.max), "doc_count": b.count,
		}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(b.docs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				bb[sk] = sv
			}
		}
		buckets = append(buckets, bb)
	}
	return map[string]any{"buckets": buckets}, nil
}

func copyAggDoc(doc map[string]any, id string) map[string]any {
	cp := make(map[string]any, len(doc)+2)
	for k, v := range doc {
		cp[k] = v
	}
	cp["__slop_id"] = id
	cp["__slop_score"] = 0.0
	return cp
}

func childrenAgg(typ string, docs []map[string]any, idx *Index, sub map[string]any, typed bool) (any, *apiError) {
	field, _, hasJoin := findJoinField(idx)
	if !hasJoin {
		return map[string]any{"doc_count": 0}, nil
	}
	parents := map[string]bool{}
	for _, d := range docs {
		if id, _ := d["__slop_id"].(string); id != "" {
			parents[id] = true
		}
	}
	kids := []map[string]any{}
	for _, id := range idx.Order {
		doc := idx.Docs[id]
		if doc == nil {
			continue
		}
		name, parent := joinParts(doc[field])
		if name != typ || parent == "" || !parents[parent] {
			continue
		}
		kids = append(kids, copyAggDoc(doc, id))
	}
	res := map[string]any{"doc_count": len(kids)}
	if sub != nil {
		subRes, err := evalAggsMode(sub, docsToHits(kids, idx), idx, "", typed, false)
		if err != nil {
			return nil, err
		}
		for sk, sv := range subRes {
			res[sk] = sv
		}
	}
	return res, nil
}

func parentAgg(typ string, docs []map[string]any, idx *Index, sub map[string]any, typed bool) (any, *apiError) {
	field, _, hasJoin := findJoinField(idx)
	if !hasJoin {
		return map[string]any{"doc_count": 0}, nil
	}
	seen := map[string]bool{}
	parents := []map[string]any{}
	for _, d := range docs {
		id, _ := d["__slop_id"].(string)
		if id == "" {
			continue
		}
		src := idx.Docs[id]
		if src == nil {
			continue
		}
		name, parent := joinParts(src[field])
		if name != typ {
			continue
		}
		if parent == "" {
			parent = idx.Routing[id]
		}
		if parent == "" || seen[parent] {
			continue
		}
		pdoc := idx.Docs[parent]
		if pdoc == nil {
			continue
		}
		seen[parent] = true
		parents = append(parents, copyAggDoc(pdoc, parent))
	}
	res := map[string]any{"doc_count": len(parents)}
	if sub != nil {
		subRes, err := evalAggsMode(sub, docsToHits(parents, idx), idx, "", typed, false)
		if err != nil {
			return nil, err
		}
		for sk, sv := range subRes {
			res[sk] = sv
		}
	}
	return res, nil
}
