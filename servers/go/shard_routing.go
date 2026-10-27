package main

import (
	"net/http"
	"strconv"
	"strings"
)

type shardFilter struct {
	active  bool
	allowed map[*Index]map[int]bool
	total   int
}

func newShardFilter(r *http.Request, indices []*Index) *shardFilter {
	routing := r.URL.Query().Get("routing")
	pref := r.URL.Query().Get("preference")
	f := &shardFilter{active: false, allowed: map[*Index]map[int]bool{}}
	if routing != "" {
		f.active = true
		for _, idx := range indices {
			set := map[int]bool{}
			for _, part := range strings.Split(routing, ",") {
				if part == "" {
					continue
				}
				set[shardOf(part, idx.numShards())] = true
			}
			f.allowed[idx] = set
		}
	} else if strings.HasPrefix(pref, "_shards:") {
		f.active = true
		spec := strings.TrimPrefix(pref, "_shards:")
		for _, idx := range indices {
			set := map[int]bool{}
			for _, part := range strings.Split(spec, ",") {
				n, err := strconv.Atoi(strings.TrimSpace(part))
				if err == nil && n >= 0 && n < idx.numShards() {
					set[n] = true
				}
			}
			f.allowed[idx] = set
		}
	}
	if !f.active {
		return nil
	}
	for _, idx := range indices {
		f.total += len(f.allowed[idx])
	}
	return f
}

func (f *shardFilter) allowsShard(idx *Index, shard int) bool {
	if f == nil {
		return true
	}
	return f.allowed[idx][shard]
}

func (f *shardFilter) allows(idx *Index, id string) bool {
	if f == nil {
		return true
	}
	routing := idx.Routing[id]
	if routing == "" {
		routing = id
	}
	return f.allowsShard(idx, shardOf(routing, idx.numShards()))
}

func searchShardMultiplier(idx *Index, id string, query map[string]any, dfs bool) float64 {
	if idx.numShards() <= 1 {
		return 1
	}
	field, text := "", ""
	if m, ok := query["match"].(map[string]any); ok {
		for f, raw := range m {
			field = f
			text, _ = queryText(raw)
		}
	} else if m, ok := query["match_phrase"].(map[string]any); ok {
		for f, raw := range m {
			field = f
			text, _ = queryText(raw)
		}
	}
	if field == "" {
		return 1
	}
	tokens := analyze(text)
	if len(tokens) == 0 {
		return 1
	}
	docShard := shardOf(func() string {
		if rv := idx.Routing[id]; rv != "" {
			return rv
		}
		return id
	}(), idx.numShards())
	n, df := 0, 0
	for _, did := range idx.Order {
		doc, live := idx.Docs[did]
		if !live {
			continue
		}
		ds := shardOf(func() string {
			if rv := idx.Routing[did]; rv != "" {
				return rv
			}
			return did
		}(), idx.numShards())
		if !dfs && ds != docShard {
			continue
		}
		n++
		qc := newQueryCtxForDoc(idx, did, doc)
		vals, present := qc.values(field)
		if !present {
			continue
		}
		found := false
		for _, v := range vals {
			s, _ := valueScalarString(v)
			for _, tok := range qc.analyzeField(field, nil, s) {
				for _, want := range tokens {
					if tok == want {
						found = true
					}
				}
			}
		}
		if found {
			df++
		}
	}
	if n == 0 || df == 0 {
		return 1
	}
	return float64(n) / float64(df)
}
