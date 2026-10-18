package main

import (
	"sort"
	"strings"
)

func collectFieldTerms(idx *Index, field string) (map[string]int, []string) {
	freq := map[string]int{}
	order := []string{}
	for _, id := range idx.Order {
		v, ok := fieldValue(idx.Docs[id], field)
		if !ok {
			continue
		}
		for _, s := range docStrings(v) {
			for _, tok := range analyze(s) {
				if _, seen := freq[tok]; !seen {
					order = append(order, tok)
				}
				freq[tok]++
			}
		}
	}
	return freq, order
}

func evalSuggest(suggest map[string]any, idx *Index) map[string]any {
	out := map[string]any{}
	for name, raw := range suggest {
		spec, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		if t, ok := spec["term"].(map[string]any); ok {
			out[name] = []any{termSuggester(spec, t, idx)}
		} else if t, ok := spec["phrase"].(map[string]any); ok {
			out[name] = []any{phraseSuggester(spec, t, idx)}
		} else if t, ok := spec["completion"].(map[string]any); ok {
			out[name] = []any{completionSuggester(spec, t, idx)}
		}
	}
	return out
}

func suggestText(spec map[string]any) string {
	if v, ok := spec["text"]; ok {
		s, _ := toString(v)
		return s
	}
	if v, ok := spec["prefix"]; ok {
		s, _ := toString(v)
		return s
	}
	return ""
}

func termSuggester(spec, body map[string]any, idx *Index) map[string]any {
	field, _ := toString(body["field"])
	input := suggestText(spec)
	freq, order := collectFieldTerms(idx, field)

	options := []any{}
	seen := map[string]bool{}
	for _, tok := range analyze(input) {
		fuzz := autoFuzziness(tok)
		type cand struct {
			text  string
			score float64
		}
		var cands []cand
		for _, c := range order {
			d := levenshtein(tok, c)
			if d == 0 || d > fuzz {
				continue
			}
			ml := max(len([]rune(tok)), len([]rune(c)))
			cands = append(cands, cand{text: c, score: 1 - float64(d)/float64(ml)})
		}
		sort.Slice(cands, func(i, j int) bool { return cands[i].score > cands[j].score })
		for _, c := range cands {
			if seen[c.text] {
				continue
			}
			seen[c.text] = true
			options = append(options, map[string]any{"text": c.text, "score": c.score, "freq": freq[c.text]})
		}
	}
	return map[string]any{"text": input, "offset": 0, "length": len(input), "options": options}
}

func phraseSuggester(spec, body map[string]any, idx *Index) map[string]any {
	field, _ := toString(body["field"])
	input := suggestText(spec)
	_, order := collectFieldTerms(idx, field)

	tokens := analyze(input)
	corrected := make([]string, 0, len(tokens))
	changed := false
	for _, tok := range tokens {
		best := tok
		bestDist := 1 << 30
		for _, c := range order {
			d := levenshtein(tok, c)
			if d == 0 {
				best = tok
				bestDist = 0
				break
			}
			if d <= 2 && d < bestDist {
				bestDist = d
				best = c
			}
		}
		if best != tok {
			changed = true
		}
		corrected = append(corrected, best)
	}

	options := []any{}
	if changed {
		phrase := strings.Join(corrected, " ")
		options = append(options, map[string]any{"text": phrase, "score": 0.9})
	}
	return map[string]any{"text": input, "offset": 0, "length": len(input), "options": options}
}

func completionInputs(v any) [][2]any {
	var out [][2]any
	add := func(input string, weight float64) {
		out = append(out, [2]any{input, weight})
	}
	switch t := v.(type) {
	case string:
		add(t, 0)
	case map[string]any:
		w, _ := toFloat(t["weight"])
		addAll := func(in any) {
			switch iv := in.(type) {
			case string:
				add(iv, w)
			case []any:
				for _, e := range iv {
					if s, ok := toString(e); ok {
						add(s, w)
					}
				}
			}
		}
		if in, ok := t["input"]; ok {
			addAll(in)
		}
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				add(s, 0)
				continue
			}
			if m, ok := e.(map[string]any); ok {
				w, _ := toFloat(m["weight"])
				if in, ok := m["input"]; ok {
					switch iv := in.(type) {
					case string:
						add(iv, w)
					case []any:
						for _, x := range iv {
							if s, ok := toString(x); ok {
								add(s, w)
							}
						}
					}
				}
			}
		}
	}
	return out
}

func completionSuggester(spec, body map[string]any, idx *Index) map[string]any {
	field, _ := toString(body["field"])
	prefix := suggestText(spec)
	lowerPrefix := strings.ToLower(prefix)

	type entry struct {
		text   string
		weight float64
		id     string
	}
	var entries []entry
	for _, id := range idx.Order {
		v, ok := fieldValue(idx.Docs[id], field)
		if !ok {
			continue
		}
		for _, pair := range completionInputs(v) {
			text, _ := pair[0].(string)
			weight, _ := pair[1].(float64)
			if strings.HasPrefix(strings.ToLower(text), lowerPrefix) {
				entries = append(entries, entry{text: text, weight: weight, id: id})
			}
		}
	}
	sort.SliceStable(entries, func(i, j int) bool { return entries[i].weight > entries[j].weight })

	options := []any{}
	for _, e := range entries {
		options = append(options, map[string]any{"text": e.text, "_score": e.weight, "_id": e.id})
	}
	return map[string]any{"text": prefix, "offset": 0, "length": len(prefix), "options": options}
}
