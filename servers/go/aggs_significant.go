package main

import (
	"math"
	"sort"
	"strings"
	"unicode"
)

type sigTerm struct {
	key   any
	canon string
	count int
	docs  []map[string]any
}

func sigTermBuckets(docs []map[string]any, field string, idx *Index, pathPrefix, valueType, format string) (map[string]*sigTerm, *apiError) {
	resolved := resolveField(field, pathPrefix)
	ft, _ := idx.FieldTypes[resolved]
	out := map[string]*sigTerm{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		seen := map[string]bool{}
		for _, e := range flattenTermValues(v) {
			k, kas, hasKas, canon, ok := termKeyFor(e, ft, valueType, format, idx, resolved)
			if !ok {
				continue
			}
			if seen[canon] {
				continue
			}
			seen[canon] = true
			t := out[canon]
			if t == nil {
				t = &sigTerm{key: k, canon: canon}
				if hasKas {
					t.key = kas
				}
				out[canon] = t
			}
			t.count++
			t.docs = append(t.docs, d)
		}
	}
	return out, nil
}

func sigTextBuckets(docs []map[string]any, field string, idx *Index, pathPrefix string, dedupe bool) map[string]*sigTerm {
	out := map[string]*sigTerm{}
	for _, d := range docs {
		v, ok := aggFieldValue(d, field, pathPrefix)
		if !ok {
			continue
		}
		seen := map[string]bool{}
		for _, e := range flattenTermValues(v) {
			s, ok := toString(e)
			if !ok {
				continue
			}
			words := strings.FieldsFunc(strings.ToLower(s), func(r rune) bool {
				return !unicode.IsLetter(r) && !unicode.IsDigit(r)
			})
			for _, w := range words {
				if w == "" {
					continue
				}
				if dedupe && seen[w] {
					continue
				}
				seen[w] = true
				t := out[w]
				if t == nil {
					t = &sigTerm{key: w, canon: w}
					out[w] = t
				}
				t.count++
				t.docs = append(t.docs, d)
			}
		}
	}
	return out
}

func sigScore(heuristic string, fg, bg int, fgTotal, bgTotal int) float64 {
	if fgTotal == 0 || fg == 0 {
		return 0
	}
	f := float64(fg) / float64(fgTotal)
	var b float64
	if bgTotal > 0 {
		if bg == 0 {
			b = 1 / float64(bgTotal)
		} else {
			b = float64(bg) / float64(bgTotal)
		}
	}
	switch heuristic {
	case "percentage":
		if bg == 0 {
			return 0
		}
		return float64(fg) / float64(bg)
	case "chi_square":
		if bgTotal == 0 {
			return 0
		}
		N := float64(bgTotal)
		n11 := float64(fg)
		n01 := float64(fgTotal - fg)
		n10 := float64(bg - fg)
		n00 := float64(bgTotal - bg - (fgTotal - fg))
		den := (n11 + n01) * (n10 + n00) * (n11 + n10) * (n01 + n00)
		if den == 0 {
			return 0
		}
		d := n11*n00 - n01*n10
		if d <= 0 {
			return 0
		}
		return N * d * d / den
	case "gnd":
		if bg == 0 || bgTotal == 0 || fg == 0 {
			return 0
		}
		n1 := float64(bg)
		n_1 := float64(fgTotal)
		n11 := float64(fg)
		N := float64(bgTotal)
		if n1 <= 0 || n_1 <= 0 || n11 <= 0 || N <= 0 {
			return 0
		}
		mx := math.Max(math.Log(n1), math.Log(n_1))
		mn := math.Min(math.Log(n1), math.Log(n_1))
		den := math.Log(N) - mn
		if den == 0 {
			return 0
		}
		return math.Exp(-(mx - math.Log(n11)) / den)
	case "mutual_information":
		if bgTotal == 0 {
			return 0
		}
		N := float64(bgTotal)
		n11 := float64(fg)
		n01 := float64(fgTotal - fg)
		n10 := float64(bg - fg)
		n00 := float64(bgTotal - bg - (fgTotal - fg))
		n1_ := float64(bg)
		n_1 := float64(fgTotal)
		n0_ := float64(bgTotal - bg)
		n_0 := float64(bgTotal - fgTotal)
		if n11/n_1 < n10/n_0 {
			return 0
		}
		term := func(nxy, nx_, n_y float64) float64 {
			nxx := math.Abs(N * nxy)
			nyy := math.Abs(nx_ * n_y)
			p := math.Abs(nxy / N)
			if nxx < 1e-7 && p < 1e-7 {
				return 0
			}
			if nxx == 0 || nyy == 0 {
				return 0
			}
			return p * math.Log(nxx/nyy)
		}
		return (term(n00, n0_, n_0) + term(n01, n0_, n_1) + term(n10, n1_, n_0) + term(n11, n1_, n_1)) / math.Log(2)
	}
	if b == 0 {
		return 0
	}
	return (f - b) * (f / b)
}

func significantAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool, text bool) (any, *apiError) {
	field, _ := toString(spec["field"])
	resolved := resolveField(field, pathPrefix)
	ft, _ := idx.FieldTypes[resolved]
	valueType, _ := toString(spec["value_type"])
	format, _ := toString(spec["format"])
	minDoc := intField(spec, "min_doc_count", 3)
	if minDoc < 1 {
		minDoc = 1
	}
	size := intField(spec, "size", 10)
	heuristic, _ := toString(spec["background_filter"])
	_ = heuristic
	heur := "jlh"
	for _, h := range []string{"jlh", "chi_square", "gnd", "mutual_information", "percentage"} {
		if _, ok := spec[h]; ok {
			heur = h
			break
		}
	}
	all := allIndexDocs(idx)
	background := all
	if bf, ok := spec["background_filter"].(map[string]any); ok {
		background = []map[string]any{}
		for _, d := range all {
			match, _, err := evalQuery(idx, bf, d)
			if err != nil {
				return nil, err
			}
			if match {
				background = append(background, d)
			}
		}
	}
	var fgTerms map[string]*sigTerm
	var bgTerms map[string]*sigTerm
	if text {
		dedupe, _ := spec["filter_duplicate_text"].(bool)
		fgTerms = sigTextBuckets(docs, field, idx, pathPrefix, dedupe)
		bgTerms = sigTextBuckets(background, field, idx, pathPrefix, dedupe)
	} else {
		var err *apiError
		fgTerms, err = sigTermBuckets(docs, field, idx, pathPrefix, valueType, format)
		if err != nil {
			return nil, err
		}
		bgTerms, err = sigTermBuckets(background, field, idx, pathPrefix, valueType, format)
		if err != nil {
			return nil, err
		}
	}
	type scored struct {
		t     *sigTerm
		score float64
		bg    int
	}
	list := []scored{}
	for _, t := range fgTerms {
		if t.count < minDoc {
			continue
		}
		bg := 0
		if bt := bgTerms[t.canon]; bt != nil {
			bg = bt.count
		}
		score := sigScore(heur, t.count, bg, len(docs), len(background))
		if (heur == "jlh" || heur == "chi_square" || heur == "mutual_information") && score <= 0 {
			continue
		}
		list = append(list, scored{t, score, bg})
	}
	sort.SliceStable(list, func(i, j int) bool {
		if list[i].score != list[j].score {
			return list[i].score > list[j].score
		}
		return list[i].t.canon < list[j].t.canon
	})
	if size > 0 && len(list) > size {
		list = list[:size]
	}
	buckets := []any{}
	for _, s := range list {
		b := map[string]any{"key": s.t.key, "doc_count": s.t.count, "bg_count": s.bg, "score": s.score}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(s.t.docs, idx), idx, pathPrefix, typed, false)
			if err != nil {
				return nil, err
			}
			for sk, sv := range subRes {
				b[sk] = sv
			}
		}
		buckets = append(buckets, b)
	}
	if ft == "" {
		return map[string]any{"buckets": buckets}, nil
	}
	return map[string]any{"doc_count": len(docs), "bg_count": len(background), "buckets": buckets}, nil
}

func rareTermsAgg(name string, spec map[string]any, docs []map[string]any, idx *Index, pathPrefix string, sub map[string]any, typed bool) (any, *apiError) {
	maxDoc := intField(spec, "max_doc_count", 1)
	if maxDoc > 100 {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "max_doc_count must be less than or equal to 100",
			CausedBy: &apiError{Type: "illegal_argument_exception",
				Reason: "[max_doc_count] must be smallerthan 100in [" + name + "]"}}
	}
	if maxDoc < 1 {
		maxDoc = 1
	}
	field, _ := toString(spec["field"])
	valueType, _ := toString(spec["value_type"])
	format, _ := toString(spec["format"])
	terms, err := sigTermBuckets(docs, field, idx, pathPrefix, valueType, format)
	if err != nil {
		return nil, err
	}
	include := map[string]bool{}
	if raw, ok := spec["include"].([]any); ok {
		resolved := resolveField(field, pathPrefix)
		ft, _ := idx.FieldTypes[resolved]
		for _, v := range raw {
			_, _, _, canon, ok := termKeyFor(v, ft, valueType, format, idx, resolved)
			if ok {
				include[canon] = true
			}
		}
	}
	list := []*sigTerm{}
	for canon, t := range terms {
		if t.count > maxDoc {
			continue
		}
		if len(include) > 0 && !include[canon] {
			continue
		}
		list = append(list, t)
	}
	sort.SliceStable(list, func(i, j int) bool {
		if list[i].count != list[j].count {
			return list[i].count < list[j].count
		}
		return list[i].canon < list[j].canon
	})
	size := intField(spec, "size", 10)
	if size > 0 && len(list) > size {
		list = list[:size]
	}
	buckets := []any{}
	for _, t := range list {
		b := map[string]any{"key": t.key, "doc_count": t.count}
		if sub != nil {
			subRes, err := evalAggsMode(sub, docsToHits(t.docs, idx), idx, pathPrefix, typed, false)
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
