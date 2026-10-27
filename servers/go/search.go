package main

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"math"
	"net/http"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"
)

type multiHit struct {
	idx   *Index
	id    string
	doc   map[string]any
	score float64
}

func checkDocValuesLoadable(idx *Index, field string) *apiError {
	def, ok := mappingFieldDef(idx, field)
	if !ok {
		return nil
	}
	ftype, _ := def["type"].(string)
	if dv, ok := def["doc_values"].(bool); ok && !dv {
		reason := "Can't load fielddata on [" + field + "] because fielddata is unsupported on fields of type [" + ftype + "]. Use doc values instead."
		return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
			CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	if ftype == "text" {
		if fd, ok := def["fielddata"].(bool); !ok || !fd {
			reason := "Fielddata is disabled on text fields by default. Set fielddata=true on [" + field + "] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
			return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
				CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
		}
	}
	return nil
}

type fieldSpec struct {
	name   string
	format string
}

func parseFieldSpecs(raw any) []fieldSpec {
	var out []fieldSpec
	list, ok := raw.([]any)
	if !ok {
		return nil
	}
	for _, e := range list {
		switch t := e.(type) {
		case string:
			out = append(out, fieldSpec{name: t})
		case map[string]any:
			name, _ := toString(t["field"])
			if name == "" {
				continue
			}
			format, _ := toString(t["format"])
			out = append(out, fieldSpec{name: name, format: format})
		}
	}
	return out
}

var textLikeTypes = map[string]bool{
	"text": true, "match_only_text": true, "search_as_you_type": true,
	"completion": true, "annotated_text": true,
}

// docValuesOutput returns the values to render for docvalue_fields / fields.
func docValuesOutput(idx *Index, id, field string, spec fieldSpec, fallbackSource bool) ([]any, bool, *apiError) {
	def, hasDef := mappingFieldDef(idx, field)
	if !hasDef {
		return nil, false, nil
	}
	ftype := ""
	hasDV := true
	if hasDef {
		ftype, _ = def["type"].(string)
		if dv, ok := def["doc_values"].(bool); ok && !dv {
			hasDV = false
		}
	}
	if spec.format != "" && spec.format != "use_field_mapping" {
		switch ftype {
		case "integer", "long", "short", "byte", "unsigned_long":
			reason := "Field [" + field + "] of type [" + ftype + "] doesn't support formats."
			return nil, false, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
				CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
		}
	}
	var vals []any
	if dv := idx.DocValues[id]; dv != nil {
		vals = dv[field]
	}
	formatted := func(list []any) []any {
		out := make([]any, 0, len(list))
		for _, v := range list {
			out = append(out, formatDocValue(idx, field, ftype, v, spec.format))
		}
		return out
	}
	if textLikeTypes[ftype] || !hasDV {
		if !fallbackSource {
			if textLikeTypes[ftype] {
				reason := "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [" + field + "] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
				return nil, false, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
					RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
					CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
			}
			if !hasDV && len(vals) > 0 {
				reason := "Can't load fielddata on [" + field + "] because fielddata is unsupported on fields of type [" + ftype + "]. Use doc values instead."
				return nil, false, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
					RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason},
					CausedBy:  &apiError{Type: "illegal_argument_exception", Reason: reason}}
			}
			return nil, false, nil
		}
		// fall back to _source values
		if v, ok := fieldValue(idx.Docs[id], field); ok {
			list := flattenAny(v)
			return formatted(list), true, nil
		}
		return nil, false, nil
	}
	if len(vals) == 0 {
		return nil, false, nil
	}
	res := formatted(vals)
	if ftype == "geo_point" && spec.format == "" {
		for i, v := range res {
			if m, ok := v.(map[string]any); ok {
				lat, _ := toFloat(m["lat"])
				lon, _ := toFloat(m["lon"])
				if fallbackSource {
					res[i] = map[string]any{"type": "Point", "coordinates": []any{lon, lat}}
				} else {
					res[i] = strconv.FormatFloat(lat, 'f', 1, 64) + ", " + strconv.FormatFloat(lon, 'f', 1, 64)
				}
			}
		}
	}
	return res, true, nil
}

func flattenAny(v any) []any {
	switch t := v.(type) {
	case []any:
		out := []any{}
		for _, e := range t {
			out = append(out, flattenAny(e)...)
		}
		return out
	case nil:
		return nil
	default:
		return []any{v}
	}
}

func formatDocValue(idx *Index, field, ftype string, v any, format string) any {
	if ftype == "geo_point" && format == "wkt" {
		if m, ok := v.(map[string]any); ok {
			lat, _ := toFloat(m["lat"])
			lon, _ := toFloat(m["lon"])
			return "POINT (" + strconv.FormatFloat(lon, 'f', 1, 64) + " " + strconv.FormatFloat(lat, 'f', 1, 64) + ")"
		}
	}
	switch ftype {
	case "date", "date_nanos":
		var ms int64
		switch t := v.(type) {
		case int64:
			ms = t
		case uint64:
			ms = int64(t)
		default:
			if n, ok := integerOf(v); ok {
				ms = n
			} else {
				return v
			}
		}
		if ftype == "date_nanos" {
			ms = ms / int64(1e6)
		}
		switch format {
		case "":
			return time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")
		case "strict_date_optional_time", "date_optional_time":
			return time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")
		case "epoch_millis":
			return strconv.FormatInt(ms, 10)
		case "epoch_second":
			return strconv.FormatInt(ms/1000, 10)
		default:
			if layout := javaDateFormatToGo(format); layout != "" {
				return time.UnixMilli(ms).UTC().Format(layout)
			}
			return strconv.FormatInt(ms, 10)
		}
	}
	if format != "" && strings.Contains(format, ".") {
		if n, ok := toFloat(v); ok {
			dec := len(format) - strings.Index(format, ".") - 1
			if dec < 0 {
				dec = 0
			}
			return strconv.FormatFloat(n, 'f', dec, 64)
		}
	}
	return v
}

func (h multiHit) sortValue(field string, desc bool, mode string) (any, bool) {
	switch field {
	case "_score":
		return h.score, true
	case "_id":
		return h.id, true
	}
	if def, ok := mappingFieldDef(h.idx, field); ok {
		if t, _ := def["type"].(string); t == "alias" {
			field = resolveAliasField(h.idx, field)
		}
	}
	vals := h.idx.DocValues[h.id]
	if vals == nil {
		return nil, false
	}
	list, ok := vals[field]
	if !ok || len(list) == 0 {
		return nil, false
	}
	if len(list) == 1 {
		return list[0], true
	}
	return reduceSortValues(list, desc, mode), true
}

func compareSortValues(a, b any) int {
	if as, ok := a.(string); ok {
		bs, _ := toString(b)
		if _, bok := b.(string); bok {
			return strings.Compare(as, bs)
		}
		return strings.Compare(as, bs)
	}
	if af, ok := toFloat(a); ok {
		if bf, ok := toFloat(b); ok {
			switch {
			case af < bf:
				return -1
			case af > bf:
				return 1
			}
			return 0
		}
	}
	return compareValues(a, b)
}

var searchBodyKeys = map[string]bool{
	"query": true, "post_filter": true, "sort": true, "from": true, "size": true,
	"_source": true, "aggs": true, "aggregations": true, "highlight": true,
	"track_scores": true, "docvalue_fields": true, "fields": true,
	"script_fields": true, "suggest": true, "search_pipeline": true,
	"timeout": true, "terminate_after": true, "min_score": true, "profile": true,
	"explain": true, "version": true, "seq_no_primary_term": true,
	"stored_fields": true, "collapse": true, "slice": true, "pit": true,
	"search_after": true, "runtime_mappings": true, "indices_boost": true,
	"stats": true, "track_total_hits": true, "rescore": true, "ext": true,
	"scroll": true, "allow_no_indices": true, "allow_partial_search_results": true,
	"ignore_unavailable": true, "expand_wildcards": true, "batched_reduce_size": true,
	"ccs_minimize_roundtrips": true, "max_concurrent_shard_requests": true,
	"pre_filter_shard_size": true, "request_cache": true, "routing": true,
	"preference": true, "search_type": true, "typed_keys": true,
	"rest_total_hits_as_int": true, "source": true, "seq_no_primary_term ": true,
}

func searchValueType(v any) string {
	switch v.(type) {
	case nil:
		return "VALUE_NULL"
	case bool:
		return "VALUE_BOOLEAN"
	case json.Number, float64, int, int64:
		return "VALUE_NUMBER"
	case string:
		return "VALUE_STRING"
	case []any:
		return "START_ARRAY"
	case map[string]any:
		return "START_OBJECT"
	}
	return "VALUE_NULL"
}

func validateSearchBody(body map[string]any) *apiError {
	for k, v := range body {
		if !searchBodyKeys[k] {
			return &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Unknown key for a " + searchValueType(v) + " in [" + k + "]."}
		}
	}
	if q, ok := body["query"]; ok {
		if q == nil {
			return &apiError{Type: "parsing_exception", Status: 400,
				Reason: "Unknown key for a VALUE_NULL in [query]."}
		}
		if qm, ok := q.(map[string]any); ok && len(qm) == 0 {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "query malformed, empty clause found at [1:9]"}
		}
	}
	return nil
}

var searchURLParams = map[string]bool{
	"q": true, "df": true, "analyzer": true, "analyze_wildcard": true,
	"default_operator": true, "lenient": true, "quote_analyzer": true,
	"quote_field_suffix": true, "fuzzy_transpositions": true,
	"fuzzy_max_expansions": true, "fuzzy_prefix_length": true, "rewrite": true,
	"minimum_should_match": true, "boost": true, "sort": true, "size": true,
	"from": true, "_source": true, "_source_includes": true, "_source_excludes": true,
	"source": true, "source_content_type": true, "source_includes": true, "source_excludes": true,
	"include_named_queries_score": true,
	"timeout":                     true, "terminate_after": true, "track_total_hits": true,
	"search_type": true, "routing": true, "preference": true, "request_cache": true,
	"allow_no_indices": true, "ignore_unavailable": true, "expand_wildcards": true,
	"allow_partial_search_results": true, "typed_keys": true,
	"rest_total_hits_as_int": true, "search_pipeline": true, "verbose_pipeline": true,
	"scroll": true, "version": true, "seq_no_primary_term": true, "explain": true,
	"stored_fields": true, "docvalue_fields": true, "fields": true, "stats": true,
	"batched_reduce_size": true, "max_concurrent_shard_requests": true,
	"pre_filter_shard_size": true, "ccs_minimize_roundtrips": true,
	"pretty": true, "human": true, "error_trace": true, "filter_path": true,
}

func validateSearchURLParams(r *http.Request) *apiError {
	for k := range r.URL.Query() {
		if !searchURLParams[k] {
			return &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "request [" + r.URL.Path + "] contains unrecognized parameter: [" + k + "]"}
		}
	}
	return nil
}

func (s *Server) handleSearch(w http.ResponseWriter, r *http.Request, name string) {
	if _, has := r.URL.Query()["source"]; has && r.URL.Query().Get("source_content_type") == "" {
		writeError(w, &apiError{Type: "illegal_state_exception", Status: 500,
			Reason: "source and source_content_type parameters are required"})
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	pitID := ""
	var pitCtx *pitContext
	if rawPit, has := body["pit"]; has && rawPit != nil {
		pm, _ := rawPit.(map[string]any)
		pitID, _ = pm["id"].(string)
		if name != "" {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: [indices] cannot be used with point in time;"})
			return
		}
		if !strings.HasPrefix(pitID, "slopensearch-pit-") {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "invalid id: [" + pitID + "]"})
			return
		}
		ctx, ok := s.getPit(pitID)
		if !ok {
			reason := "No search context found for id [" + pitID + "]"
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 404,
				Reason: reason, RootCause: &apiError{Type: "search_context_missing_exception", Reason: reason}})
			return
		}
		pitCtx = ctx
	}
	if apiErr := validateSearchBody(body); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if apiErr := validateSearchURLParams(r); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{closedError: true})
	var indices []*Index
	if pitCtx != nil {
		indices = []*Index{pitCtx.idx}
	} else {
		var apiErr2 *apiError
		indices, apiErr2 = s.resolveSpec2(name, o)
		if apiErr2 != nil {
			writeError(w, apiErr2)
			return
		}
	}

	pipe, pipeErr := s.resolveSearchPipeline(r, body, indices)
	if pipeErr != nil {
		writeError(w, pipeErr)
		return
	}
	var pipeState *searchPipeState
	var verboseResults []any
	verbosePipeline := queryBool(r, "verbose_pipeline")
	if pipe != nil {
		var perr *apiError
		pipeState, verboseResults, perr = applySearchRequestProcessors(body, pipe, verbosePipeline)
		if perr != nil {
			writeError(w, perr)
			return
		}
	}

	if st := r.URL.Query().Get("search_type"); st != "" {
		switch st {
		case "query_then_fetch", "dfs_query_then_fetch":
		case "query_and_fetch", "fetch_by_query":
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Unsupported search type [" + st + "]"})
			return
		default:
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "No search type for [" + st + "]"})
			return
		}
	}
	if rc := r.URL.Query().Get("request_cache"); rc != "" && rc != "true" && rc != "false" {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "Failed to parse value [" + rc + "] as only [true] or [false] are allowed."})
		return
	}
	for _, spec := range []struct {
		key string
		min int
		msg string
	}{
		{"batched_reduce_size", 2, "batchedReduceSize must be >= 2"},
		{"pre_filter_shard_size", 1, "preFilterShardSize must be >= 1"},
		{"max_concurrent_shard_requests", 1, "maxConcurrentShardRequests must be >= 1"},
	} {
		if v := r.URL.Query().Get(spec.key); v != "" {
			n, err := strconv.Atoi(v)
			if err != nil || n < spec.min {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: spec.msg})
				return
			}
		}
	}
	for _, tv := range []string{r.URL.Query().Get("timeout")} {
		if tv == "" {
			continue
		}
		if !validTimeoutString(tv) {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse [" + tv + "]"})
			return
		}
	}
	if bt, ok := body["timeout"].(string); ok && bt != "" && !validTimeoutString(bt) {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to parse [" + bt + "]"})
		return
	}
	query, _ := body["query"].(map[string]any)
	if vals, has := r.URL.Query()["q"]; has {
		var qerr *apiError
		query, qerr = parseQParamOpts(vals[0], r.URL.Query().Get("df"), r.URL.Query().Get("default_operator"))
		if qerr != nil {
			writeError(w, qerr)
			return
		}
	}
	if query == nil {
		query = map[string]any{"match_all": map[string]any{}}
	}

	if body["size"] == nil {
		if _, exists := body["size"]; exists {
			writeError(w, errParsing("Unknown key for a VALUE_NULL in [size]."))
			return
		}
	}
	if body["from"] == nil {
		if _, exists := body["from"]; exists {
			writeError(w, errParsing("Unknown key for a VALUE_NULL in [from]."))
			return
		}
	}
	from := intField(body, "from", 0)
	size := intField(body, "size", 10)
	if v := r.URL.Query().Get("from"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Failed to parse int parameter [from] with value [" + v + "]"})
			return
		}
		from = n
	}
	if v := r.URL.Query().Get("size"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "Failed to parse int parameter [size] with value [" + v + "]"})
			return
		}
		size = n
	}
	if from < 0 {
		writeError(w, errBad(fmt.Sprintf("[from] parameter cannot be negative, found [%d]", from)))
		return
	}
	if size < 0 {
		writeError(w, errBad(fmt.Sprintf("[size] parameter cannot be negative, found [%d]", size)))
		return
	}
	scrollParam := r.URL.Query().Get("scroll")
	scrollID := ""
	restInt := r.URL.Query().Get("rest_total_hits_as_int") == "true"
	if scrollParam != "" {
		if verr := validateScrollParams(r, body, from, size); verr != nil {
			writeError(w, verr)
			return
		}
	}
	sliceID, sliceMax := -1, 0
	if rawSlice, ok := body["slice"].(map[string]any); ok {
		if scrollParam == "" && body["pit"] == nil {
			reason := "`slice` cannot be used outside of a scroll context or PIT context"
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 500,
				Reason: reason, Phase: "query",
				RootCause: &apiError{Type: "search_exception", Reason: reason}})
			return
		}
		if f, ok := toFloat(rawSlice["id"]); ok {
			sliceID = int(f)
		}
		if f, ok := toFloat(rawSlice["max"]); ok {
			sliceMax = int(f)
		}
		if sliceMax <= 1 {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason:   "max must be greater than 1",
				CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "max must be greater than 1"}})
			return
		}
		if sliceID >= sliceMax {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason:   "max must be greater than id",
				CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "max must be greater than id"}})
			return
		}
	}
	for _, idx := range indices {
		window := 10000
		if v, ok := findSetting(idx.Settings, "max_result_window"); ok {
			if n, ok := settingsValueInt(v); ok {
				window = n
			}
		}
		if from+size > window {
			reason := fmt.Sprintf("Result window is too large, from + size must be less than or equal to: [%d] but was [%d]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.", window, from+size)
			if r.URL.Query().Get("scroll") != "" {
				reason = fmt.Sprintf("Batch size is too large, size must be less than or equal to: [%d] but was [%d].", window, size)
			}
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: reason, Phase: "query", RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
			return
		}
	}

	boostMap := map[*Index]float64{}
	if rawBoost, ok := body["indices_boost"]; ok && rawBoost != nil {
		var entries []any
		if arr, isArr := rawBoost.([]any); isArr {
			entries = arr
		} else {
			entries = []any{rawBoost}
		}
		for _, ent := range entries {
			obj, _ := ent.(map[string]any)
			for pat, val := range obj {
				f, _ := toFloat(val)
				boosted, berr := s.resolveSpec2(pat, o)
				if berr != nil {
					writeError(w, berr)
					return
				}
				for _, bi := range boosted {
					boostMap[bi] = f
				}
			}
		}
	}
	var matched []multiHit
	shards := 0
	sf := newShardFilter(r, indices)
	dfsSearch := r.URL.Query().Get("search_type") == "dfs_query_then_fetch"
	if pitCtx != nil {
		shards = pitCtx.shards
		for _, h := range pitCtx.hits {
			qc := newQueryCtxForDoc(h.idx, h.id, h.doc)
			ok, score, err := qc.eval(query)
			if err != nil {
				writeError(w, err)
				return
			}
			if ok {
				if f, has := boostMap[h.idx]; has {
					score *= f
				}
				matched = append(matched, multiHit{idx: h.idx, id: h.id, doc: h.doc, score: score})
			}
		}
	} else {
		for _, idx := range indices {
			if err := readBlockError(idx); err != nil {
				writeError(w, err)
				return
			}
			q := query
			if f := searchAliasFilter(idx, name); f != nil {
				q = map[string]any{"bool": map[string]any{"must": []any{query, f}}}
			}
			if sf != nil {
				shards += len(sf.allowed[idx])
			} else {
				shards += idx.numShards()
			}
			for _, id := range idx.Order {
				if !sf.allows(idx, id) {
					continue
				}
				doc, live := idx.Docs[id]
				if !live {
					continue
				}
				if _, pending := idx.Pending[id]; pending {
					continue
				}
				qc := newQueryCtxForDoc(idx, id, doc)
				ok, score, err := qc.eval(q)
				if err != nil {
					writeError(w, err)
					return
				}
				if ok {
					if f, has := boostMap[idx]; has {
						score *= f
					}
					score *= searchShardMultiplier(idx, id, q, dfsSearch)
					matched = append(matched, multiHit{idx: idx, id: id, doc: doc, score: score})
				}
			}
		}
	}
	if ms, ok := body["min_score"]; ok {
		if f, ok := toFloat(ms); ok {
			kept := matched[:0]
			for _, h := range matched {
				if h.score >= f {
					kept = append(kept, h)
				}
			}
			matched = kept
		}
	}
	aggMatched := append([]multiHit{}, matched...)
	if pf, ok := body["post_filter"].(map[string]any); ok && len(pf) > 0 {
		kept := matched[:0]
		for _, h := range matched {
			qc := newQueryCtxForDoc(h.idx, h.id, h.doc)
			ok, _, err := qc.eval(pf)
			if err != nil {
				writeError(w, err)
				return
			}
			if ok {
				kept = append(kept, h)
			}
		}
		matched = kept
	}
	total := 0

	searchSortSpecs, sortErr := parseSortSpecs(body["sort"], r.URL.Query().Get("sort"))
	if sortErr != nil {
		writeError(w, sortErr)
		return
	}
	hasScoreSort := false
	allScoreSort := len(searchSortSpecs) > 0
	for _, sp := range searchSortSpecs {
		if sp.field == "_score" {
			hasScoreSort = true
		} else {
			allScoreSort = false
		}
	}
	sortedByField := len(searchSortSpecs) > 0 && !allScoreSort
	for _, sp := range searchSortSpecs {
		if sp.field == "_shard_doc" && pitCtx == nil {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: _shard_doc is only supported with point-in-time (PIT). Add a PIT or remove _shard_doc.;"})
			return
		}
	}
	if after, ok := body["search_after"].([]any); ok {
		if len(searchSortSpecs) == 0 {
			reason := "Sort must contain at least one field."
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: reason, Phase: "query",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
			return
		}
		if len(after) != len(searchSortSpecs) {
			reason := fmt.Sprintf("search_after has %d value(s) but sort has %d.", len(after), len(searchSortSpecs))
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: reason, Phase: "query",
				RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
			return
		}
		if r.URL.Query().Get("scroll") != "" {
			reason := "`search_after` cannot be used in a scroll context."
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 500,
				Reason: reason, Phase: "query",
				RootCause: &apiError{Type: "search_exception", Reason: reason}})
			return
		}
		if len(indices) > 0 {
			for i, sp := range searchSortSpecs {
				def, ok := mappingFieldDef(indices[0], sp.field)
				if !ok {
					continue
				}
				ft, _ := def["type"].(string)
				if isNumericType(ft) && ft != "date" && ft != "date_nanos" {
					if _, conv := toFloat(after[i]); !conv {
						reason := "Failed to parse search_after value for field [" + sp.field + "]."
						writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
							Reason: reason, Phase: "query",
							RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
						return
					}
				}
			}
		}
		if from != 0 {
			reason := "`from` parameter must be set to 0 when `search_after` is used."
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 500,
				Reason: reason, Phase: "query",
				RootCause: &apiError{Type: "search_exception", Reason: reason}})
			return
		}
	}
	failedShards := 0
	partialReason := ""
	var shardFailures []any
	if sortedByField {
		for _, sp := range searchSortSpecs {
			if sp.field == "_score" || sp.field == "_doc" || sp.field == "_id" || sp.field == "_index" || sp.field == "_script" || sp.field == "_geo_distance" || sp.field == "_shard_doc" {
				continue
			}
			anyMapped := false
			for _, idx := range indices {
				if _, ok := mappingFieldDef(idx, sp.field); ok {
					anyMapped = true
					break
				}
			}
			if !anyMapped && sp.unmapped == "" {
				reason := "No mapping found for [" + sp.field + "] in order to sort on"
				writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
					Reason: reason, Phase: "query",
					RootCause: &apiError{Type: "query_shard_exception", Reason: reason}})
				return
			}
			if !anyMapped || sp.unmapped != "" {
				continue
			}
			for _, idx := range indices {
				if _, ok := mappingFieldDef(idx, sp.field); ok {
					continue
				}
				failedShards++
				partialReason = "No mapping found for [" + sp.field + "] in order to sort on"
				shardFailures = append(shardFailures, map[string]any{
					"index":  idx.Name,
					"shard":  0,
					"reason": map[string]any{"type": "query_shard_exception", "reason": "No mapping found for [" + sp.field + "] in order to sort on"},
				})
				kept := matched[:0]
				for _, h := range matched {
					if h.idx != idx {
						kept = append(kept, h)
					}
				}
				matched = kept
			}
		}
	}
	if failedShards > 0 {
		allowPartial := true
		if v, ok := body["allow_partial_search_results"].(bool); ok {
			allowPartial = v
		}
		if vals, has := r.URL.Query()["allow_partial_search_results"]; has {
			allowPartial = vals[0] != "false"
		}
		if !allowPartial {
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
				Reason: partialReason, Phase: "query",
				RootCause: &apiError{Type: "query_shard_exception", Reason: partialReason}})
			return
		}
	}
	if sliceMax > 0 {
		kept := matched[:0]
		for _, h := range matched {
			n, err := strconv.Atoi(h.id)
			if err != nil {
				n = int(hashString(h.id))
			}
			if n%sliceMax == sliceID {
				kept = append(kept, h)
			}
		}
		matched = kept
	}
	terminateAfter := 0
	if n, ok := toFloat(body["terminate_after"]); ok {
		terminateAfter = int(n)
	}
	if v := r.URL.Query().Get("terminate_after"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			terminateAfter = n
		}
	}
	terminatedEarly := false
	if terminateAfter > 0 && len(matched) > terminateAfter {
		matched = matched[:terminateAfter]
		terminatedEarly = true
	}
	total = len(matched)
	if after, ok := body["search_after"].([]any); ok && len(after) > 0 && len(searchSortSpecs) > 0 {
		kept := matched[:0]
		for _, h := range matched {
			if hitAfterSearchAfter(h, searchSortSpecs, after) {
				kept = append(kept, h)
			}
		}
		matched = kept
	}
	if rerr := applyRescore(matched, body); rerr != nil {
		writeError(w, rerr)
		return
	}
	if len(searchSortSpecs) == 0 {
		sort.SliceStable(matched, func(i, j int) bool {
			return matched[i].score > matched[j].score
		})
	}
	if sortedByField {
		for _, sp := range searchSortSpecs {
			if sp.field == "_score" || sp.field == "_doc" || sp.field == "_id" || sp.field == "_index" || sp.field == "_script" || sp.field == "_shard_doc" {
				continue
			}
			for _, idx := range indices {
				if sp.field == "_geo_distance" {
					def, ok := mappingFieldDef(idx, sp.geoField)
					t, _ := def["type"].(string)
					if (!ok || t != "geo_point") && !sp.geoIgnore {
						reason := "failed to find mapper for [" + sp.geoField + "] for geo distance based sort"
						writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
							Reason: reason, Phase: "query",
							RootCause: &apiError{Type: "query_shard_exception", Reason: reason}})
						return
					}
					continue
				}
				if sp.nestedPath != "" {
					def, ok := mappingFieldDef(idx, sp.nestedPath)
					t, _ := def["type"].(string)
					if !ok || t != "nested" {
						reason := "[nested] failed to find nested object under path [" + sp.nestedPath + "]"
						writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
							Reason: reason, Phase: "query",
							RootCause: &apiError{Type: "query_shard_exception", Reason: reason}})
						return
					}
				}
				if sp.mode == "avg" || sp.mode == "sum" || sp.mode == "median" {
					if def, ok := mappingFieldDef(idx, sp.field); ok {
						t, _ := def["type"].(string)
						if !isNumericType(t) && t != "date" {
							reason := "we only support AVG, MEDIAN and SUM on number based fields"
							writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
								Reason: reason, Phase: "query",
								RootCause: &apiError{Type: "query_shard_exception", Reason: reason}})
							return
						}
					}
				}
				if def, ok := mappingFieldDef(idx, sp.field); ok {
					t, _ := def["type"].(string)
					fd, _ := def["fielddata"].(bool)
					if (t == "text" || t == "match_only_text") && !fd {
						reason := "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [" + sp.field + "] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
						writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400,
							Reason: reason, Phase: "query",
							RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}})
						return
					}
				}
				if err := checkDocValuesLoadable(idx, sp.field); err != nil {
					writeError(w, err)
					return
				}
			}
		}
		sort.SliceStable(matched, func(i, j int) bool {
			for _, sp := range searchSortSpecs {
				c := compareSortSpec(matched[i], matched[j], sp)
				if c != 0 {
					if sp.desc {
						return c > 0
					}
					return c < 0
				}
			}
			return false
		})
	}
	trackScores, _ := body["track_scores"].(bool)
	collapseFields := map[string]map[string]any{}
	collapseInner := map[string]map[string]any{}
	if body["collapse"] != nil {
		var cerr *apiError
		matched, collapseFields, collapseInner, cerr = applyCollapse(matched, body)
		if cerr != nil {
			writeError(w, cerr)
			return
		}
	}

	if from > len(matched) {
		from = len(matched)
	}
	end := from + size
	if end > len(matched) {
		end = len(matched)
	}
	page := matched[from:end]
	if scrollParam != "" {
		sid := s.storeScroll(matched, end, size, total, restInt)
		defer func() { _ = sid }()
		scrollID = sid
	}

	srcIncludes, srcExcludes, omitSource, _ := sourceFilterFromBody(body)
	qIncludes, qExcludes, qOmit, qHas := sourceFilterFromQuery(r.URL.Query())
	if qHas || qOmit {
		srcIncludes, srcExcludes, omitSource = qIncludes, qExcludes, qOmit
	}
	if _, hasSF := body["script_fields"].(map[string]any); hasSF {
		if _, hasSrc := body["_source"]; !hasSrc && r.URL.Query().Get("_source") == "" {
			omitSource = true
		}
	}
	storedRaw, hasStored := body["stored_fields"]
	storedURL := r.URL.Query().Get("stored_fields")
	if storedURL != "" {
		storedRaw, hasStored = storedURL, true
	}
	storedNone := false
	storedNames := []string{}
	if hasStored {
		items := []any{}
		switch sv := storedRaw.(type) {
		case string:
			for _, part := range strings.Split(sv, ",") {
				items = append(items, strings.TrimSpace(part))
			}
		case []any:
			items = sv
		}
		for _, item := range items {
			name, _ := toString(item)
			if name == "_none_" {
				storedNone = true
			} else if name != "" {
				storedNames = append(storedNames, name)
			}
		}
		omitSource = true
	}
	if storedNone {
		srcTrue := false
		if v, ok := body["_source"].(bool); ok && v {
			srcTrue = true
		}
		if qv := r.URL.Query().Get("_source"); qv == "true" {
			srcTrue = true
		}
		if srcTrue {
			writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 500, Reason: "all shards failed",
				RootCause: &apiError{Type: "search_exception", Reason: "[stored_fields] cannot be disabled if [_source] is requested"}})
			return
		}
	}
	hlCfg, hlParseErr := parseHighlightConfig(body, query)
	if hlParseErr != nil {
		writeError(w, hlParseErr)
		return
	}
	if list, ok := body["fields"].([]any); ok {
		for _, e := range list {
			if m, ok := e.(map[string]any); ok {
				if _, has := m["include_unmapped"]; has {
					reason := "[docvalues_field] unknown field [include_unmapped]"
					writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400, Reason: reason,
						RootCause: &apiError{Type: "parsing_exception", Reason: reason}})
					return
				}
			}
		}
	}
	dvFields := parseFieldSpecs(body["docvalue_fields"])
	paramFields := parseFieldSpecs(body["fields"])
	if v := r.URL.Query().Get("docvalue_fields"); v != "" {
		items := []any{}
		for _, part := range strings.Split(v, ",") {
			items = append(items, strings.TrimSpace(part))
		}
		dvFields = parseFieldSpecs(items)
	}
	if v := r.URL.Query().Get("fields"); v != "" {
		items := []any{}
		for _, part := range strings.Split(v, ",") {
			items = append(items, strings.TrimSpace(part))
		}
		paramFields = parseFieldSpecs(items)
	}

	versionFlag := false
	if v, ok := body["version"].(bool); ok {
		versionFlag = v
	}
	if vals, has := r.URL.Query()["version"]; has {
		versionFlag = vals[0] == "true"
	}
	explainFlag := false
	if v, ok := body["explain"].(bool); ok {
		explainFlag = v
	}
	if r.URL.Query().Get("explain") == "true" {
		explainFlag = true
	}
	seqNoTermFlag := false
	if v, ok := body["seq_no_primary_term"].(bool); ok {
		seqNoTermFlag = v
	}
	if vals, has := r.URL.Query()["seq_no_primary_term"]; has {
		seqNoTermFlag = vals[0] == "true"
	}
	omitScores := sortedByField && !hasScoreSort && !trackScores
	maxScoreAllowed := !sortedByField || trackScores || (len(searchSortSpecs) > 0 && searchSortSpecs[0].field == "_score" && searchSortSpecs[0].desc)
	var maxScore any
	if maxScoreAllowed && len(indices) == 0 {
		maxScore = 0.0
	} else if maxScoreAllowed && len(page) > 0 {
		m := page[0].score
		for _, h := range page[1:] {
			if h.score > m {
				m = h.score
			}
		}
		maxScore = scoreJSON(m)
	}
	trackTotal := true
	trackTotalVal := 10000
	trackExplicit := false
	if tv, has := body["track_total_hits"]; has {
		switch t := tv.(type) {
		case bool:
			trackTotal = t
			if t {
				trackTotalVal = -1
			}
		case float64:
			n := int(t)
			if n < 0 {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: fmt.Sprintf("value must be >= 0, got %d", n)})
				return
			}
			trackTotalVal = n
			trackExplicit = true
		case json.Number:
			n64, err := t.Int64()
			if err != nil {
				writeError(w, &apiError{Type: "number_format_exception", Status: 400,
					Reason: "For input string: \"" + t.String() + "\""})
				return
			}
			n := int(n64)
			if n < 0 {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: fmt.Sprintf("value must be >= 0, got %d", n)})
				return
			}
			trackTotalVal = n
			trackExplicit = true
		case string:
			n, err := strconv.Atoi(t)
			if err != nil {
				writeError(w, &apiError{Type: "number_format_exception", Status: 400,
					Reason: "For input string: \"" + t + "\""})
				return
			}
			if n < 0 {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: fmt.Sprintf("value must be >= 0, got %d", n)})
				return
			}
			trackTotalVal = n
			trackExplicit = true
		}
	}
	if vals, has := r.URL.Query()["track_total_hits"]; has {
		v := vals[0]
		if v == "false" {
			trackTotal = false
		} else if v != "true" {
			n, err := strconv.Atoi(v)
			if err != nil {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "Failed to parse int parameter [track_total_hits] with value [" + v + "]"})
				return
			}
			if n < 0 {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: fmt.Sprintf("value must be >= 0, got %d", n)})
				return
			}
			trackTotalVal = n
			trackExplicit = true
		}
	}
	restInt = r.URL.Query().Get("rest_total_hits_as_int") == "true"
	if restInt && trackExplicit && trackTotalVal >= 0 && trackTotalVal < total {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: fmt.Sprintf("[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got %d", trackTotalVal)})
		return
	}
	totalObj := map[string]any{"value": total, "relation": "eq"}
	if trackTotalVal >= 0 && trackTotalVal < total {
		totalObj = map[string]any{"value": trackTotalVal, "relation": "gte"}
	}
	var totalOut any = totalObj
	if restInt {
		if trackExplicit {
			totalOut = totalObj["value"]
		} else {
			totalOut = total
		}
	}
	hits := make([]any, 0, len(page))
	for _, h := range page {
		src := filterSourceDoc(h.doc, srcIncludes, srcExcludes)
		var scoreVal any = h.score
		if omitScores {
			scoreVal = nil
		} else {
			scoreVal = scoreJSON(h.score)
		}
		entry := map[string]any{
			"_index": h.idx.Name, "_id": h.id, "_score": scoreVal,
		}
		if rv, ok := h.idx.Routing[h.id]; ok && rv != "" {
			entry["_routing"] = rv
		}
		if versionFlag {
			entry["_version"] = currentVersion(h.idx, h.id)
		}
		if seqNoTermFlag {
			entry["_seq_no"] = h.idx.SeqNos[h.id]
			entry["_primary_term"] = int64(1)
		}
		if explainFlag {
			entry["_shard"] = "[" + h.idx.Name + "][0]"
			entry["_node"] = "slopensearch-node-1"
			field, term := explainLabel(query)
			qc := newQueryCtxForDoc(h.idx, h.id, h.doc)
			exp := qc.bm25Explain(field, term, h.score)
			exp["value"] = scoreJSON(h.score)
			entry["_explanation"] = exp
		}
		if sortedByField && len(searchSortSpecs) > 0 && !allScoreSort {
			sortVals := make([]any, 0, len(searchSortSpecs))
			for _, sp := range searchSortSpecs {
				sortVals = append(sortVals, sortOutputValue(h, sp))
			}
			entry["sort"] = sortVals
		}
		if !omitSource {
			entry["_source"] = src
		}
		if storedNone {
			delete(entry, "_id")
		}
		if hlCfg != nil {
			if hlErr := hlCfg.validate(h.idx); hlErr != nil {
				writeError(w, hlErr)
				return
			}
			if hl := hlCfg.render(h.idx, h.doc); len(hl) > 0 {
				entry["highlight"] = hl
			}
		}
		if cf, ok := collapseFields[h.id]; ok {
			if existing, ok := entry["fields"].(map[string]any); ok {
				for k, v := range cf {
					existing[k] = v
				}
			} else {
				entry["fields"] = cf
			}
		}
		if ci, ok := collapseInner[h.id]; ok {
			entry["inner_hits"] = ci
		}
		if len(dvFields) > 0 || len(paramFields) > 0 || len(storedNames) > 0 {
			out := map[string]any{}
			collect := func(specs []fieldSpec, fallback bool) *apiError {
				for _, spec := range specs {
					names := []string{spec.name}
					if strings.ContainsAny(spec.name, "*?") {
						names = nil
						re := wildcardRegex(spec.name)
						for _, f := range mappedLeafFields(h.idx) {
							if re.MatchString(f) {
								names = append(names, f)
							}
						}
						sort.Strings(names)
					}
					for _, name := range names {
						if _, done := out[name]; done {
							continue
						}
						vals, found, derr := docValuesOutput(h.idx, h.id, name, spec, fallback)
						if derr != nil {
							return derr
						}
						if found {
							out[name] = vals
						}
					}
				}
				return nil
			}
			if derr := collect(dvFields, false); derr != nil {
				writeError(w, derr)
				return
			}
			if derr := collect(paramFields, true); derr != nil {
				writeError(w, derr)
				return
			}
			for _, name := range storedNames {
				def, ok := mappingFieldDef(h.idx, name)
				if !ok {
					continue
				}
				stored, _ := def["store"].(bool)
				if !stored {
					continue
				}
				if v, ok := fieldValue(h.doc, name); ok {
					out[name] = []any{v}
				}
			}
			if len(out) > 0 {
				entry["fields"] = out
			}
		}
		if sf, ok := body["script_fields"].(map[string]any); ok && len(sf) > 0 {
			got, serr := evalScriptFields(h, sf)
			if serr != nil {
				writeError(w, serr)
				return
			}
			if existing, ok := entry["fields"].(map[string]any); ok {
				for k, v := range got {
					existing[k] = v
				}
			} else {
				entry["fields"] = got
			}
		}
		named := map[string]float64{}
		qcNamed := newQueryCtxForDoc(h.idx, h.id, h.doc)
		collectNamedQueries(query, qcNamed, named)
		if pf, ok := body["post_filter"].(map[string]any); ok {
			collectNamedQueries(pf, qcNamed, named)
		}
		if len(named) > 0 {
			if r.URL.Query().Get("include_named_queries_score") == "true" {
				scored := map[string]any{}
				for n, sc := range named {
					scored[n] = scoreJSON(sc)
				}
				entry["matched_queries"] = scored
			} else {
				names := make([]string, 0, len(named))
				for n := range named {
					names = append(names, n)
				}
				entry["matched_queries"] = names
			}
		}
		hits = append(hits, entry)
	}

	resp := map[string]any{
		"took":      1,
		"timed_out": false,
		"_shards":   shardStatusObject(shards, failedShards, shardFailures),
		"hits": map[string]any{
			"total":     totalOut,
			"max_score": maxScore,
			"hits":      hits,
		},
	}

	var aggIdx *Index
	if len(indices) > 0 {
		aggIdx = indices[0]
	}
	if aggIdx != nil {
		for _, key := range []string{"aggregations", "aggs"} {
			if aggs, ok := body[key].(map[string]any); ok {
				terms := queryHighlightTerms(body["query"])
				aggHits := make([]aggHit, 0, len(aggMatched))
				for _, h := range aggMatched {
					aggHits = append(aggHits, aggHit{idx: h.idx, id: h.id, doc: h.doc, score: h.score, terms: terms})
				}
				typed := r.URL.Query().Get("typed_keys") != "" && r.URL.Query().Get("typed_keys") != "false"
				out, err := evalAggs(aggs, aggHits, aggIdx, "", typed)
				if err != nil {
					writeError(w, err)
					return
				}
				resp["aggregations"] = out
				break
			}
		}
		if suggest, ok := body["suggest"].(map[string]any); ok {
			typed := r.URL.Query().Get("typed_keys") != "" && r.URL.Query().Get("typed_keys") != "false"
			resp["suggest"] = evalSuggest(suggest, aggIdx, typed)
		}
	}

	if terminateAfter > 0 {
		resp["terminated_early"] = terminatedEarly
	}
	profileFlag := false
	if v, ok := body["profile"].(bool); ok {
		profileFlag = v
	}
	if profileFlag && len(indices) > 0 {
		resp["profile"] = buildProfile(query, body, indices[0].Name)
	}
	if !trackTotal {
		if hm, ok := resp["hits"].(map[string]any); ok {
			if restInt {
				hm["total"] = -1
			} else {
				delete(hm, "total")
			}
		}
	}
	if pipe != nil {
		if perr := applySearchResponseProcessors(resp, pipe, pipeState); perr != nil {
			writeError(w, perr)
			return
		}
		if verbosePipeline {
			resp["processor_results"] = verboseResults
		}
	}
	if scrollID != "" {
		resp["_scroll_id"] = scrollID
	}
	if pitID != "" {
		ordered := orderedJSON{{k: "pit_id", v: pitID}}
		for k, v := range resp {
			ordered = append(ordered, orderedKV{k: k, v: v})
		}
		writeJSON(w, 200, ordered)
		return
	}

	if terminateAfter > 0 {
		ordered := orderedJSON{{k: "took", v: resp["took"]}, {k: "timed_out", v: resp["timed_out"]}, {k: "terminated_early", v: terminatedEarly}}
		for k, v := range resp {
			if k == "took" || k == "timed_out" || k == "terminated_early" {
				continue
			}
			ordered = append(ordered, orderedKV{k: k, v: v})
		}
		writeJSON(w, 200, ordered)
		return
	}
	writeJSON(w, 200, resp)
}

func validTimeoutString(s string) bool {
	if s == "" {
		return false
	}
	i := 0
	for i < len(s) && s[i] >= '0' && s[i] <= '9' {
		i++
	}
	if i == 0 {
		return false
	}
	switch s[i:] {
	case "ms", "s", "m", "h", "d":
		return true
	}
	return false
}

func buildProfile(query map[string]any, body map[string]any, idxName string) map[string]any {
	q := profileQuery(query)
	aggs := []any{}
	if am, ok := body["aggs"].(map[string]any); ok {
		for name := range am {
			aggs = append(aggs, map[string]any{"description": name, "time_in_nanos": int64(1),
				"breakdown": map[string]any{"build_aggregation": int64(1), "collect": int64(0)}})
		}
	}
	return map[string]any{"shards": []any{map[string]any{
		"id": "[" + idxName + "][0]",
		"searches": []any{map[string]any{
			"query":        []any{q},
			"rewrite_time": int64(0),
			"collector":    []any{map[string]any{"name": "SimpleTopScoreDocCollector", "reason": "search_top_hits", "time_in_nanos": int64(1)}},
		}},
		"aggregations": aggs,
		"fetch":        []any{map[string]any{"type": "fetch", "description": "fetch"}},
	}}}
}

func profileQuery(query map[string]any) map[string]any {
	base := map[string]any{"time_in_nanos": int64(1), "breakdown": map[string]any{
		"create_weight": int64(1), "build_scorer": int64(1), "next_doc": int64(1), "score": int64(1), "advance": int64(1), "match": int64(1),
	}}
	if m, ok := query["match"].(map[string]any); ok {
		for field, raw := range m {
			text, _ := queryText(raw)
			term := ""
			if toks := analyze(text); len(toks) > 0 {
				term = toks[0]
			}
			base["type"] = "TermQuery"
			base["description"] = field + ":" + term
			return base
		}
	}
	if m, ok := query["term"].(map[string]any); ok {
		for field, raw := range m {
			val := raw
			if vm, ok := raw.(map[string]any); ok {
				val = vm["value"]
			}
			s, _ := toString(val)
			base["type"] = "TermQuery"
			base["description"] = field + ":" + s
			return base
		}
	}
	if bm, ok := query["bool"].(map[string]any); ok {
		parts := []string{}
		children := []any{}
		for _, item := range asQueryList(bm["must"]) {
			if cm, ok := item.(map[string]any); ok {
				ch := profileQuery(cm)
				if _, isTerm := cm["term"]; isTerm {
					parts = append(parts, "+ConstantScore("+fmt.Sprint(ch["description"])+")")
					ch["type"] = "ConstantScoreQuery"
				} else {
					parts = append(parts, "+"+fmt.Sprint(ch["description"]))
				}
				children = append(children, ch)
			}
		}
		for _, item := range asQueryList(bm["filter"]) {
			if cm, ok := item.(map[string]any); ok {
				ch := profileQuery(cm)
				parts = append(parts, "+ConstantScore("+fmt.Sprint(ch["description"])+")")
				ch["type"] = "ConstantScoreQuery"
				children = append(children, ch)
			}
		}
		base["type"] = "BooleanQuery"
		base["description"] = strings.Join(parts, " ")
		base["children"] = children
		return base
	}
	if _, ok := query["match_all"]; ok {
		base["type"] = "MatchAllDocsQuery"
		base["description"] = "match_all"
		return base
	}
	base["type"] = "TermQuery"
	base["description"] = ""
	return base
}

func multiDocs(hits []multiHit) []map[string]any {
	out := make([]map[string]any, 0, len(hits))
	for _, h := range hits {
		out = append(out, h.doc)
	}
	return out
}

type hit struct {
	id    string
	doc   map[string]any
	score float64
}

func matchedDocs(matched []hit) []map[string]any {
	out := make([]map[string]any, 0, len(matched))
	for _, m := range matched {
		out = append(out, m.doc)
	}
	return out
}

func intField(body map[string]any, key string, def int) int {
	if v, ok := body[key]; ok {
		if f, ok := toFloat(v); ok {
			return int(f)
		}
	}
	return def
}

type searchSortSpec struct {
	field        string
	desc         bool
	missing      any
	hasMissing   bool
	mode         string
	unmapped     string
	format       string
	nestedPath   string
	nestedFilter map[string]any
	numericType  string
	scriptSource string
	scriptParams any
	scriptType   string
	geoField     string
	geoPoint     any
	geoUnit      string
	geoMode      string
	geoIgnore    bool
}

func parseSortSpecs(bodySort any, uriSort string) ([]searchSortSpec, *apiError) {
	var out []searchSortSpec
	add := func(raw any, uri bool) *apiError {
		switch t := raw.(type) {
		case string:
			if t == "" {
				return nil
			}
			out = append(out, searchSortSpec{field: t})
		case map[string]any:
			for field, v := range t {
				sp := searchSortSpec{field: field}
				switch ov := v.(type) {
				case string:
					if !validSortOrder(ov) {
						if uri {
							return nil
						}
						return &apiError{Type: "illegal_argument_exception", Status: 400,
							Reason: "No enum constant org.opensearch.search.sort.SortOrder." + strings.ToUpper(ov)}
					}
					sp.desc = strings.EqualFold(ov, "desc")
				case map[string]any:
					if o, ok := ov["order"].(string); ok {
						if !validSortOrder(o) {
							if uri {
								return nil
							}
							return &apiError{Type: "illegal_argument_exception", Status: 400,
								Reason: "No enum constant org.opensearch.search.sort.SortOrder." + strings.ToUpper(o)}
						}
						sp.desc = strings.EqualFold(o, "desc")
					}
					if m, ok := ov["missing"]; ok {
						sp.missing = m
						sp.hasMissing = true
					}
					if m, ok := ov["mode"].(string); ok {
						switch m {
						case "min", "max", "avg", "sum", "median":
							sp.mode = m
						default:
							return &apiError{Type: "x_content_parse_exception", Status: 400,
								Reason: "Failed to parse sort mode [" + m + "]",
								CausedBy: &apiError{Type: "illegal_argument_exception",
									Reason: "Unknown SortMode [" + m + "]"}}
						}
					}
					if m, ok := ov["unmapped_type"].(string); ok {
						sp.unmapped = m
					}
					if _, ok := ov["format"]; ok {
						return &apiError{Type: "x_content_parse_exception", Status: 400,
							Reason: "Failed to parse sort [field_sort] unknown field [format]"}
					}
					if nt, ok := ov["numeric_type"].(string); ok {
						sp.numericType = nt
					}
					if st, ok := ov["type"].(string); ok {
						sp.scriptType = st
					}
					if sp.field == "_geo_distance" {
						for k, v := range ov {
							switch k {
							case "order", "unit", "mode", "distance_type", "validation_method", "ignore_unmapped", "nested", "nested_path":
							default:
								sp.geoField = k
								sp.geoPoint = v
							}
						}
						if u, ok := ov["unit"].(string); ok {
							sp.geoUnit = u
						}
						if m, ok := ov["mode"].(string); ok {
							sp.geoMode = m
						}
						if b, ok := ov["ignore_unmapped"].(bool); ok {
							sp.geoIgnore = b
						}
						if sp.geoUnit != "" {
							if _, ok := geoUnitMeters(sp.geoUnit); !ok {
								return &apiError{Type: "illegal_argument_exception", Status: 400,
									Reason: "No distance unit match [" + sp.geoUnit + "]"}
							}
						}
					}
					switch sc := ov["script"].(type) {
					case string:
						sp.scriptSource = sc
					case map[string]any:
						if src, ok := sc["source"].(string); ok {
							sp.scriptSource = src
						} else if src, ok := sc["inline"].(string); ok {
							sp.scriptSource = src
						}
						if pr, ok := sc["params"]; ok {
							sp.scriptParams = pr
						}
					}
					if np, ok := ov["nested_path"].(string); ok {
						sp.nestedPath = np
					}
					if nm, ok := ov["nested"].(map[string]any); ok {
						if np, ok := nm["path"].(string); ok {
							sp.nestedPath = np
						}
						if f, ok := nm["filter"].(map[string]any); ok {
							sp.nestedFilter = f
						}
					}
					if m, ok := ov["format"].(string); ok {
						sp.format = m
					}
				}
				out = append(out, sp)
			}
		}
		return nil
	}
	switch t := bodySort.(type) {
	case []any:
		for _, raw := range t {
			if err := add(raw, false); err != nil {
				return nil, err
			}
		}
	case string, map[string]any:
		if err := add(t, false); err != nil {
			return nil, err
		}
	}
	if uriSort != "" {
		valid := true
		var uriSpecs []searchSortSpec
		for _, part := range strings.Split(uriSort, ",") {
			part = strings.TrimSpace(part)
			if part == "" {
				continue
			}
			field := part
			desc := false
			if at := strings.LastIndex(part, ":"); at >= 0 {
				field = part[:at]
				ord := part[at+1:]
				if ord != "asc" && ord != "desc" {
					valid = false
					break
				}
				desc = ord == "desc"
			}
			uriSpecs = append(uriSpecs, searchSortSpec{field: field, desc: desc})
		}
		if valid {
			out = append(out, uriSpecs...)
		}
	}
	return out, nil
}

func validSortOrder(s string) bool {
	return s == "asc" || s == "desc" || s == "ASC" || s == "DESC"
}

func searchSortSpecValue(h multiHit, sp searchSortSpec) (any, bool, bool) {
	v, ok, missing := searchSortSpecValueRaw(h, sp)
	if ok && !missing && sp.numericType != "" {
		v = convertSortNumeric(v, sp.numericType)
	}
	return v, ok, missing
}

func convertSortNumeric(v any, numericType string) any {
	f, ok := toFloat(v)
	if !ok {
		return v
	}
	switch numericType {
	case "long":
		return int64(f)
	case "double":
		return f
	case "date_nanos":
		return int64(f) * 1000000
	}
	return v
}

func searchSortSpecValueRaw(h multiHit, sp searchSortSpec) (any, bool, bool) {
	switch sp.field {
	case "_score":
		return h.score, true, false
	case "_id":
		return h.id, true, false
	case "_index":
		return h.idx.Name, true, false
	case "_script":
		return evalSearchSortScript(sp.scriptSource, sp.scriptParams, h.doc), true, false
	case "_geo_distance":
		return geoSortDistance(h, sp), true, false
	case "_shard_doc":
		pos := 0
		for i, id := range h.idx.Order {
			if id == h.id {
				pos = i
				break
			}
		}
		ns := h.idx.numShards()
		if ns < 1 {
			ns = 1
		}
		shard := int64(hashString(h.id) % uint32(ns))
		return shard*1000000000 + int64(pos), true, false
	case "_doc":
		pos := 0
		for i, id := range h.idx.Order {
			if id == h.id {
				pos = i
				break
			}
		}
		return int64(pos), true, false
	}
	if fieldUnderNested(h.idx, sp.field) && sp.nestedPath == "" {
		return nil, false, true
	}
	if sp.nestedPath != "" {
		vals := nestedSortValues(h, sp)
		if len(vals) == 0 {
			return nil, false, true
		}
		if len(vals) == 1 {
			return vals[0], true, false
		}
		return reduceSortValues(vals, sp.desc, sp.mode), true, false
	}
	if sp.unmapped != "" {
		if _, mapped := mappingFieldDef(h.idx, sp.field); !mapped {
			switch sp.unmapped {
			case "long":
				return int64(9223372036854775807), true, false
			case "integer", "short", "byte":
				return int64(2147483647), true, false
			case "double", "float", "half_float", "scaled_float":
				return math.MaxFloat64, true, false
			}
		}
	}
	v, ok := h.sortValue(sp.field, sp.desc, sp.mode)
	if ok {
		return v, true, false
	}
	if sp.hasMissing {
		switch m := sp.missing.(type) {
		case string:
			if m == "_first" || m == "_last" {
				return nil, false, true
			}
		}
		return sp.missing, true, false
	}
	return nil, false, true
}

func compareSortSpec(a, b multiHit, sp searchSortSpec) int {
	av, aok, amissing := searchSortSpecValue(a, sp)
	bv, bok, bmissing := searchSortSpecValue(b, sp)
	if amissing || bmissing {
		if amissing && bmissing {
			return 0
		}
		first := false
		if sp.hasMissing {
			if m, ok := sp.missing.(string); ok && m == "_first" {
				first = true
			}
		}
		if amissing {
			if first {
				if sp.desc {
					return 1
				}
				return -1
			}
			if sp.desc {
				return -1
			}
			return 1
		}
		if first {
			if sp.desc {
				return -1
			}
			return 1
		}
		if sp.desc {
			return 1
		}
		return -1
	}
	if !aok && !bok {
		return 0
	}
	if !aok {
		return 1
	}
	if !bok {
		return -1
	}
	return compareSortValues(av, bv)
}

func sortOutputValue(h multiHit, sp searchSortSpec) any {
	v, ok, missing := searchSortSpecValue(h, sp)
	if f, isF := v.(float64); isF && math.IsInf(f, 0) {
		if f > 0 {
			return "Infinity"
		}
		return "-Infinity"
	}
	if missing {
		numeric := false
		if def, ok := mappingFieldDef(h.idx, sp.field); ok {
			t, _ := def["type"].(string)
			numeric = isNumericType(t)
		}
		if numeric && !sp.hasMissing {
			switch sp.numericType {
			case "long", "date_nanos":
				if sp.desc {
					return int64(-9223372036854775808)
				}
				return int64(9223372036854775807)
			case "integer", "short", "byte":
				if sp.desc {
					return int64(-2147483648)
				}
				return int64(2147483647)
			}
			if def, ok := mappingFieldDef(h.idx, sp.field); ok {
				t, _ := def["type"].(string)
				if t == "integer" || t == "long" || t == "short" || t == "byte" {
					if sp.desc {
						return int64(-2147483648)
					}
					return int64(2147483647)
				}
			}
		}
		if !numeric {
			if sp.hasMissing {
				if m, ok := sp.missing.(string); ok && (m == "_first" || m == "_last") {
					return nil
				}
				return sp.missing
			}
			return nil
		}
		if sp.hasMissing {
			if m, ok := sp.missing.(string); ok && (m == "_first" || m == "_last") {
				if m == "_first" {
					return "-Infinity"
				}
				if sp.desc {
					return "-Infinity"
				}
				return "Infinity"
			}
			return sp.missing
		}
		if sp.desc {
			return "-Infinity"
		}
		return "Infinity"
	}
	if ok {
		return v
	}
	return nil
}

func parseSort(spec any) (string, bool) {
	switch t := spec.(type) {
	case string:
		return t, false
	case map[string]any:
		for k, v := range t {
			if k == "_score" {
				continue
			}
			dir, _ := toString(v)
			return k, dir == "desc"
		}
	}
	return "", false
}

func compareValues(a, b any) int {
	af, aok := toFloat(a)
	bf, bok := toFloat(b)
	if aok && bok {
		switch {
		case af < bf:
			return -1
		case af > bf:
			return 1
		default:
			return 0
		}
	}
	as, _ := toString(a)
	bs, _ := toString(b)
	return strings.Compare(as, bs)
}

func fieldType(idx *Index, field string) string {
	if t, ok := idx.FieldTypes[field]; ok {
		return t
	}
	return "text"
}

func isNumericType(t string) bool {
	switch t {
	case "integer", "long", "short", "byte", "float", "double", "half_float", "scaled_float", "date":
		return true
	}
	return false
}

func docStrings(v any) []string {
	switch t := v.(type) {
	case []any:
		out := []string{}
		for _, e := range t {
			if s, ok := toString(e); ok {
				out = append(out, s)
			}
		}
		return out
	case nil:
		return nil
	default:
		if s, ok := toString(t); ok {
			return []string{s}
		}
	}
	return nil
}

var knownClauses = map[string]bool{
	"match_all": true, "match": true, "term": true, "terms": true, "range": true,
	"bool": true, "match_phrase": true, "multi_match": true, "wildcard": true,
	"fuzzy": true, "exists": true, "prefix": true,
}

func validateQuery(idx *Index, query map[string]any) *apiError {
	for clause, raw := range query {
		if !knownClauses[clause] {
			return errParsing("unknown query [" + clause + "]")
		}
		switch clause {
		case "range":
			m, ok := raw.(map[string]any)
			if !ok {
				return errParsing("bad range")
			}
			field, condRaw, ok := singleEntry(m)
			if !ok {
				return errParsing("empty range")
			}
			ft := fieldType(idx, field)
			if cond, ok := condRaw.(map[string]any); ok {
				for _, bound := range cond {
					if ft == "date" {
						if _, ok := parseDate(bound); !ok {
							return errParsing("failed to parse date [" + field + "]")
						}
					} else if isNumericType(ft) {
						if _, ok := toFloat(bound); !ok {
							return errParsing("failed to parse number [" + field + "]")
						}
					}
				}
			}
		case "bool":
			if bm, ok := raw.(map[string]any); ok {
				for _, key := range []string{"must", "should", "filter", "must_not"} {
					for _, q := range asQueryList(bm[key]) {
						if qm, ok := q.(map[string]any); ok {
							if err := validateQuery(idx, qm); err != nil {
								return err
							}
						}
					}
				}
			}
		}
	}
	return nil
}

func evalQuery(idx *Index, query map[string]any, doc map[string]any) (bool, float64, *apiError) {
	if len(query) == 0 {
		return true, 1, nil
	}
	for clause, raw := range query {
		switch clause {
		case "match_all":
			return true, 1, nil
		case "match_none":
			return false, 0, nil
		case "match":
			return evalMatch(idx, raw, doc)
		case "term":
			return evalTerm(idx, raw, doc, 0)
		case "terms":
			return evalTerms(idx, raw, doc)
		case "range":
			return evalRange(idx, raw, doc)
		case "bool":
			return evalBool(idx, raw, doc)
		case "match_phrase":
			return evalMatchPhrase(idx, raw, doc)
		case "multi_match":
			return evalMultiMatch(idx, raw, doc)
		case "wildcard":
			return evalWildcard(idx, raw, doc)
		case "fuzzy":
			return evalFuzzy(idx, raw, doc)
		case "exists":
			return evalExists(raw, doc)
		case "prefix":
			return evalPrefix(idx, raw, doc)
		default:
			return false, 0, errParsing("unknown query [" + clause + "]")
		}
	}
	return false, 0, errParsing("empty query")
}

func singleEntry(m map[string]any) (string, any, bool) {
	for k, v := range m {
		return k, v, true
	}
	return "", nil, false
}

func queryText(raw any) (string, any) {
	if m, ok := raw.(map[string]any); ok {
		if q, ok := m["query"]; ok {
			s, _ := toString(q)
			return s, m
		}
	}
	s, _ := toString(raw)
	return s, nil
}

func evalMatch(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty match")
	}
	text, opts := queryText(val)
	operator := ""
	if om, ok := opts.(map[string]any); ok {
		if o, ok := om["operator"].(string); ok {
			operator = o
		}
	}
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	if isNumericType(ft) || ft == "keyword" || ft == "boolean" {
		for _, s := range docStrings(v) {
			if s == text {
				return true, 1, nil
			}
		}
		return false, 0, nil
	}
	qTokens := analyze(text)
	var docTokens []string
	for _, s := range docStrings(v) {
		docTokens = append(docTokens, analyze(s)...)
	}
	score := 0.0
	for _, qt := range qTokens {
		for _, dt := range docTokens {
			if qt == dt {
				score++
				break
			}
		}
	}
	if operator == "and" {
		return len(qTokens) > 0 && score == float64(len(qTokens)), score, nil
	}
	return score > 0, score, nil
}

func evalPrefix(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty prefix")
	}
	prefix := ""
	if m, ok := val.(map[string]any); ok {
		prefix, _ = toString(m["value"])
	} else {
		prefix, _ = toString(val)
	}
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if strings.HasPrefix(c, prefix) {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalTerm(idx *Index, raw any, doc map[string]any, fuzz int) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty term")
	}
	if m, ok := val.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			val = v
		}
		if f, ok := m["fuzziness"]; ok {
			if n, ok := toFloat(f); ok {
				fuzz = int(n)
			}
		}
	}
	term, _ := toString(val)
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if c == term {
				return true, 1, nil
			}
			if fuzz > 0 && levenshtein(c, term) <= fuzz {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalFuzzy(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, _ := singleEntry(raw.(map[string]any))
	fuzz := autoFuzziness(field)
	if m, ok := val.(map[string]any); ok {
		if v, ok := m["value"]; ok {
			text, _ := toString(v)
			fuzz = autoFuzziness(text)
		}
	}
	return evalTerm(idx, raw, doc, fuzz)
}

func evalTerms(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty terms")
	}
	list, ok := val.([]any)
	if !ok {
		return false, 0, errParsing("terms expects array")
	}
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	ft := fieldType(idx, field)
	for _, item := range list {
		want, _ := toString(item)
		for _, s := range docStrings(v) {
			candidates := []string{s}
			if ft == "text" {
				candidates = analyze(s)
			}
			for _, c := range candidates {
				if c == want {
					return true, 1, nil
				}
			}
		}
	}
	return false, 0, nil
}

func compareRange(dv float64, cond map[string]any, field string, parse func(any) (float64, bool)) (bool, float64, *apiError) {
	for op, bound := range cond {
		bv, ok := parse(bound)
		if !ok {
			return false, 0, errParsing("failed to parse [" + field + "]")
		}
		switch op {
		case "gt":
			if !(dv > bv) {
				return false, 0, nil
			}
		case "gte":
			if !(dv >= bv) {
				return false, 0, nil
			}
		case "lt":
			if !(dv < bv) {
				return false, 0, nil
			}
		case "lte":
			if !(dv <= bv) {
				return false, 0, nil
			}
		}
	}
	return true, 1, nil
}

func evalRange(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad range")
	}
	field, condRaw, ok := singleEntry(m)
	if !ok {
		return false, 0, errParsing("empty range")
	}
	cond, ok := condRaw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad range condition")
	}
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	ft := fieldType(idx, field)
	if ft == "date" {
		dv, ok := parseDate(v)
		if !ok {
			return false, 0, nil
		}
		return compareRange(dv, cond, field, parseDate)
	}
	if isNumericType(ft) {
		dv, ok := toFloat(v)
		if !ok {
			return false, 0, nil
		}
		return compareRange(dv, cond, field, toFloat)
	}
	ds, _ := toString(v)
	for op, bound := range cond {
		bs, _ := toString(bound)
		c := strings.Compare(ds, bs)
		switch op {
		case "gt":
			if !(c > 0) {
				return false, 0, nil
			}
		case "gte":
			if !(c >= 0) {
				return false, 0, nil
			}
		case "lt":
			if !(c < 0) {
				return false, 0, nil
			}
		case "lte":
			if !(c <= 0) {
				return false, 0, nil
			}
		}
	}
	return true, 1, nil
}

func asQueryList(v any) []any {
	switch t := v.(type) {
	case []any:
		return t
	case map[string]any:
		return []any{t}
	}
	return nil
}

func evalBool(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad bool")
	}
	score := 0.0
	must := asQueryList(m["must"])
	filter := asQueryList(m["filter"])
	should := asQueryList(m["should"])
	mustNot := asQueryList(m["must_not"])

	for _, q := range append(append([]any{}, must...), filter...) {
		ok, sc, err := evalQuery(idx, q.(map[string]any), doc)
		if err != nil {
			return false, 0, err
		}
		if !ok {
			return false, 0, nil
		}
		score += sc
	}
	if len(should) > 0 {
		anyMatched := false
		for _, q := range should {
			ok, sc, err := evalQuery(idx, q.(map[string]any), doc)
			if err != nil {
				return false, 0, err
			}
			if ok {
				anyMatched = true
				score += sc
			}
		}
		if !anyMatched && len(must) == 0 && len(filter) == 0 {
			return false, 0, nil
		}
	}
	for _, q := range mustNot {
		ok, _, err := evalQuery(idx, q.(map[string]any), doc)
		if err != nil {
			return false, 0, err
		}
		if ok {
			return false, 0, nil
		}
	}
	if len(must) == 0 && len(filter) == 0 && len(should) == 0 {
		return true, 1, nil
	}
	return true, score, nil
}

func evalMatchPhrase(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty match_phrase")
	}
	text, _ := queryText(val)
	qTokens := analyze(text)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		dTokens := analyze(s)
		if containsSubsequence(dTokens, qTokens) {
			return true, float64(len(qTokens)), nil
		}
	}
	return false, 0, nil
}

func containsSubsequence(hay, needle []string) bool {
	if len(needle) == 0 {
		return false
	}
	for i := 0; i+len(needle) <= len(hay); i++ {
		ok := true
		for j := range needle {
			if hay[i+j] != needle[j] {
				ok = false
				break
			}
		}
		if ok {
			return true
		}
	}
	return false
}

func evalMultiMatch(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad multi_match")
	}
	q, _ := toString(m["query"])
	fields, _ := m["fields"].([]any)
	total := 0.0
	for _, f := range fields {
		fs, _ := toString(f)
		ok, sc, err := evalMatch(idx, map[string]any{fs: q}, doc)
		if err != nil {
			return false, 0, err
		}
		if ok {
			total += sc
		}
	}
	return total > 0, total, nil
}

var wildcardCache = map[string]*regexp.Regexp{}

func wildcardRegex(pattern string) *regexp.Regexp {
	if re, ok := wildcardCache[pattern]; ok {
		return re
	}
	var b strings.Builder
	b.WriteString("^")
	for _, ch := range pattern {
		switch ch {
		case '*':
			b.WriteString(".*")
		case '?':
			b.WriteString(".")
		default:
			b.WriteString(regexp.QuoteMeta(string(ch)))
		}
	}
	b.WriteString("$")
	re := regexp.MustCompile(b.String())
	wildcardCache[pattern] = re
	return re
}

func evalWildcard(idx *Index, raw any, doc map[string]any) (bool, float64, *apiError) {
	field, val, ok := singleEntry(raw.(map[string]any))
	if !ok {
		return false, 0, errParsing("empty wildcard")
	}
	m, ok := val.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad wildcard body")
	}
	pattern, _ := toString(m["value"])
	re := wildcardRegex(pattern)
	ft := fieldType(idx, field)
	v, present := fieldValue(doc, field)
	if !present {
		return false, 0, nil
	}
	for _, s := range docStrings(v) {
		candidates := []string{s}
		if ft == "text" {
			candidates = analyze(s)
		}
		for _, c := range candidates {
			if re.MatchString(c) {
				return true, 1, nil
			}
		}
	}
	return false, 0, nil
}

func evalExists(raw any, doc map[string]any) (bool, float64, *apiError) {
	m, ok := raw.(map[string]any)
	if !ok {
		return false, 0, errParsing("bad exists")
	}
	field, _ := toString(m["field"])
	_, present := fieldValue(doc, field)
	return present, 1, nil
}

func filterSource(doc map[string]any, spec any) map[string]any {
	var includes []string
	switch t := spec.(type) {
	case string:
		includes = []string{t}
	case []any:
		for _, e := range t {
			if s, ok := toString(e); ok {
				includes = append(includes, s)
			}
		}
	case map[string]any:
		if inc, ok := t["includes"].([]any); ok {
			for _, e := range inc {
				if s, ok := toString(e); ok {
					includes = append(includes, s)
				}
			}
		}
	}
	if len(includes) == 0 {
		return doc
	}
	out := map[string]any{}
	for _, f := range includes {
		if v, ok := doc[f]; ok {
			out[f] = v
		}
	}
	return out
}

func highlightFieldsOf(body map[string]any) []string {
	hl, ok := body["highlight"].(map[string]any)
	if !ok {
		return nil
	}
	fields, ok := hl["fields"].(map[string]any)
	if !ok {
		return nil
	}
	out := []string{}
	for f := range fields {
		out = append(out, f)
	}
	sort.Strings(out)
	return out
}

func highlightFragment(text string, tokens map[string]bool) string {
	fields := strings.Fields(text)
	for i, w := range fields {
		clean := strings.ToLower(strings.Trim(w, ".,!?"))
		if tokens[clean] {
			fields[i] = "<em>" + w + "</em>"
		}
	}
	return strings.Join(fields, " ")
}

func collectTokens(query map[string]any) map[string]bool {
	out := map[string]bool{}
	var walk func(m map[string]any)
	walk = func(m map[string]any) {
		for clause, raw := range m {
			switch clause {
			case "match", "match_phrase":
				if mm, ok := raw.(map[string]any); ok {
					_, val, _ := singleEntry(mm)
					text, _ := queryText(val)
					for _, t := range analyze(text) {
						out[t] = true
					}
				}
			case "multi_match":
				if mm, ok := raw.(map[string]any); ok {
					text, _ := toString(mm["query"])
					for _, t := range analyze(text) {
						out[t] = true
					}
				}
			case "bool":
				if bm, ok := raw.(map[string]any); ok {
					for _, key := range []string{"must", "should", "filter"} {
						for _, q := range asQueryList(bm[key]) {
							if qm, ok := q.(map[string]any); ok {
								walk(qm)
							}
						}
					}
				}
			}
		}
	}
	walk(query)
	return out
}

func queryHighlightTerms(q any) []string {
	out := []string{}
	var walk func(v any)
	walk = func(v any) {
		switch t := v.(type) {
		case string:
			for _, w := range strings.Fields(t) {
				out = append(out, w)
			}
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
	walk(q)
	return out
}

func highlightText(text string, terms []string) string {
	for _, term := range terms {
		if term == "" {
			continue
		}
		lower := strings.ToLower(text)
		lt := strings.ToLower(term)
		idx := 0
		for {
			i := strings.Index(lower[idx:], lt)
			if i < 0 {
				break
			}
			i += idx
			text = text[:i] + "<em>" + text[i:i+len(term)] + "</em>" + text[i+len(term):]
			lower = strings.ToLower(text)
			idx = i + len(term) + 9
		}
	}
	return text
}

func shardStatusObject(total, failed int, failures []any) map[string]any {
	out := map[string]any{"total": total, "successful": total - failed, "skipped": 0, "failed": failed}
	if len(failures) > 0 {
		out["failures"] = failures
	}
	return out
}

func reduceSortValues(list []any, desc bool, mode string) any {
	sorted := append([]any{}, list...)
	sort.SliceStable(sorted, func(i, j int) bool {
		return compareSortValues(sorted[i], sorted[j]) < 0
	})
	switch mode {
	case "min":
		return sorted[0]
	case "max":
		return sorted[len(sorted)-1]
	case "avg", "sum", "median":
		floats := make([]float64, 0, len(sorted))
		for _, v := range sorted {
			if f, ok := toFloat(v); ok {
				floats = append(floats, f)
			}
		}
		if len(floats) == 0 {
			return sorted[0]
		}
		sort.Float64s(floats)
		switch mode {
		case "avg":
			sum := 0.0
			for _, f := range floats {
				sum += f
			}
			return math.Round(sum / float64(len(floats)))
		case "sum":
			sum := 0.0
			for _, f := range floats {
				sum += f
			}
			return sum
		default:
			mid := len(floats) / 2
			if len(floats)%2 == 0 {
				return math.Round((floats[mid-1] + floats[mid]) / 2)
			}
			return floats[mid]
		}
	}
	if desc {
		return sorted[len(sorted)-1]
	}
	return sorted[0]
}

func nestedSortValues(h multiHit, sp searchSortSpec) []any {
	pathParts := strings.Split(sp.nestedPath, ".")
	fieldParts := strings.Split(sp.field, ".")
	rel := fieldParts
	if len(pathParts) <= len(fieldParts) {
		rel = fieldParts[len(pathParts):]
	}
	var elems []any
	cur := any(h.doc)
	for _, part := range pathParts {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil
		}
		cur = m[part]
	}
	switch t := cur.(type) {
	case []any:
		elems = t
	case map[string]any:
		elems = []any{t}
	default:
		return nil
	}
	out := []any{}
	for _, elem := range elems {
		em, ok := elem.(map[string]any)
		if !ok {
			continue
		}
		if sp.nestedFilter != nil {
			fake := map[string]any{sp.nestedPath: []any{em}}
			qc := newQueryCtxForDoc(h.idx, h.id+"__nested_sort", fake)
			ok, _, err := qc.eval(sp.nestedFilter)
			if err != nil || !ok {
				continue
			}
		}
		var v any = em
		for _, part := range rel {
			m, ok := v.(map[string]any)
			if !ok {
				v = nil
				break
			}
			v = m[part]
		}
		if v != nil {
			out = append(out, v)
		}
	}
	return out
}

func scoreJSON(v float64) any {
	if v == math.Trunc(v) {
		return json.Number(strconv.FormatFloat(v, 'f', 1, 64))
	}
	return v
}

var scriptDocRefRe = regexp.MustCompile(`doc\['([^']+)'\]\.value`)
var scriptParamRefRe = regexp.MustCompile(`params\.([A-Za-z0-9_]+)`)

func evalSearchSortScript(src string, params any, doc map[string]any) any {
	expr := scriptDocRefRe.ReplaceAllStringFunc(src, func(m string) string {
		sub := scriptDocRefRe.FindStringSubmatch(m)
		v := docNestedValue(doc, sub[1])
		return scriptLiteral(v)
	})
	pm, _ := params.(map[string]any)
	expr = scriptParamRefRe.ReplaceAllStringFunc(expr, func(m string) string {
		sub := scriptParamRefRe.FindStringSubmatch(m)
		return scriptLiteral(pm[sub[1]])
	})
	node, err := parser.ParseExpr(expr)
	if err != nil {
		return nil
	}
	v, err := evalSortScriptNode(node)
	if err != nil {
		return nil
	}
	return v
}

func docNestedValue(doc map[string]any, field string) any {
	var cur any = doc
	for _, part := range strings.Split(field, ".") {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil
		}
		cur = m[part]
	}
	if list, ok := cur.([]any); ok {
		if len(list) == 0 {
			return nil
		}
		return list[0]
	}
	return cur
}

func scriptLiteral(v any) string {
	switch t := v.(type) {
	case nil:
		return "0"
	case string:
		return strconv.Quote(t)
	case bool:
		return strconv.FormatBool(t)
	case json.Number:
		return t.String()
	}
	if f, ok := toFloat(v); ok {
		return strconv.FormatFloat(f, 'f', -1, 64)
	}
	return "0"
}

func evalSortScriptNode(node ast.Expr) (any, error) {
	switch n := node.(type) {
	case *ast.ParenExpr:
		return evalSortScriptNode(n.X)
	case *ast.BasicLit:
		switch n.Kind {
		case token.INT, token.FLOAT:
			return strconv.ParseFloat(n.Value, 64)
		case token.STRING:
			return strconv.Unquote(n.Value)
		}
	case *ast.UnaryExpr:
		v, err := evalSortScriptNode(n.X)
		if err != nil {
			return nil, err
		}
		f, _ := toFloat(v)
		if n.Op == token.SUB {
			return -f, nil
		}
		return f, nil
	case *ast.BinaryExpr:
		lv, err := evalSortScriptNode(n.X)
		if err != nil {
			return nil, err
		}
		rv, err := evalSortScriptNode(n.Y)
		if err != nil {
			return nil, err
		}
		lf, _ := toFloat(lv)
		rf, _ := toFloat(rv)
		switch n.Op {
		case token.ADD:
			return lf + rf, nil
		case token.SUB:
			return lf - rf, nil
		case token.MUL:
			return lf * rf, nil
		case token.QUO:
			return lf / rf, nil
		}
	case *ast.CallExpr:
		if sel, ok := n.Fun.(*ast.SelectorExpr); ok && sel.Sel.Name == "abs" && len(n.Args) == 1 {
			v, err := evalSortScriptNode(n.Args[0])
			if err != nil {
				return nil, err
			}
			f, _ := toFloat(v)
			return math.Abs(f), nil
		}
	}
	return nil, fmt.Errorf("unsupported script")
}

func geoUnitMeters(unit string) (float64, bool) {
	switch unit {
	case "m", "meters":
		return 1, true
	case "km", "kilometers":
		return 1000, true
	case "mi", "miles":
		return 1609.344, true
	case "yd", "yards":
		return 0.9144, true
	case "ft", "feet":
		return 0.3048, true
	case "in", "inch":
		return 0.0254, true
	case "cm", "centimeters":
		return 0.01, true
	case "mm", "millimeters":
		return 0.001, true
	case "nmi", "NM", "nauticalmiles":
		return 1852, true
	}
	return 0, false
}

func parseGeoPoint(v any) (float64, float64, bool) {
	switch t := v.(type) {
	case map[string]any:
		lat, ok1 := toFloat(t["lat"])
		lon, ok2 := toFloat(t["lon"])
		return lat, lon, ok1 && ok2
	case []any:
		if len(t) == 2 {
			lon, ok1 := toFloat(t[0])
			lat, ok2 := toFloat(t[1])
			return lat, lon, ok1 && ok2
		}
	case string:
		parts := strings.Split(t, ",")
		if len(parts) == 2 {
			lat, err1 := strconv.ParseFloat(strings.TrimSpace(parts[0]), 64)
			lon, err2 := strconv.ParseFloat(strings.TrimSpace(parts[1]), 64)
			return lat, lon, err1 == nil && err2 == nil
		}
	}
	return 0, 0, false
}

func geoDistanceMeters(lat1, lon1, lat2, lon2 float64) float64 {
	const r = 6371000.0
	rad := math.Pi / 180
	dLat := (lat2 - lat1) * rad
	dLon := (lon2 - lon1) * rad
	a := math.Sin(dLat/2)*math.Sin(dLat/2) +
		math.Cos(lat1*rad)*math.Cos(lat2*rad)*math.Sin(dLon/2)*math.Sin(dLon/2)
	return 2 * r * math.Atan2(math.Sqrt(a), math.Sqrt(1-a))
}

func parseGeoOrigins(v any) [][2]float64 {
	if arr, isArr := v.([]any); isArr {
		if len(arr) == 2 {
			if _, ok1 := toFloat(arr[0]); ok1 {
				if _, ok2 := toFloat(arr[1]); ok2 {
					if lat, lon, ok := parseGeoPoint(arr); ok {
						return [][2]float64{{lat, lon}}
					}
				}
			}
		}
		var out [][2]float64
		for _, item := range arr {
			if lat, lon, ok := parseGeoPoint(item); ok {
				out = append(out, [2]float64{lat, lon})
			}
		}
		return out
	}
	if lat, lon, ok := parseGeoPoint(v); ok {
		return [][2]float64{{lat, lon}}
	}
	return nil
}

func geoSortDistance(h multiHit, sp searchSortSpec) any {
	origins := parseGeoOrigins(sp.geoPoint)
	if len(origins) == 0 {
		return math.Inf(1)
	}
	raw := docNestedValue(h.doc, sp.geoField)
	if raw == nil {
		return math.Inf(1)
	}
	var candidates []any
	if arr, isArr := raw.([]any); isArr {
		candidates = arr
	} else {
		candidates = []any{raw}
	}
	mult, _ := geoUnitMeters(sp.geoUnit)
	if mult == 0 {
		mult = 1
	}
	best := math.Inf(1)
	sum := 0.0
	count := 0
	for _, c := range candidates {
		lat, lon, pok := parseGeoPoint(c)
		if !pok {
			continue
		}
		for _, o := range origins {
			d := geoDistanceMeters(o[0], o[1], lat, lon) / mult
			switch sp.geoMode {
			case "max":
				if math.IsInf(best, 1) || d > best {
					best = d
				}
			case "avg":
				sum += d
				count++
				best = sum / float64(count)
			default:
				if d < best {
					best = d
				}
			}
		}
	}
	if math.IsInf(best, 1) {
		return math.Inf(1)
	}
	return best
}

func hitAfterSearchAfter(h multiHit, specs []searchSortSpec, after []any) bool {
	for i, sp := range specs {
		if i >= len(after) {
			break
		}
		av, _, _ := searchSortSpecValue(h, sp)
		cmp := compareSearchAfterValue(h, sp, av, after[i])
		if cmp == 0 {
			continue
		}
		if sp.desc {
			return cmp < 0
		}
		return cmp > 0
	}
	return false
}

func compareSearchAfterValue(h multiHit, sp searchSortSpec, av, bv any) int {
	def, _ := mappingFieldDef(h.idx, sp.field)
	ft, _ := def["type"].(string)
	if ft == "date" || ft == "date_nanos" {
		am, ok1 := dateMillisFor(h.idx, sp.field, ft, av)
		bm, ok2 := dateMillisFor(h.idx, sp.field, ft, bv)
		if ok1 && ok2 {
			switch {
			case am < bm:
				return -1
			case am > bm:
				return 1
			}
			return 0
		}
	}
	if sp.numericType == "long" || sp.numericType == "date_nanos" {
		af, _ := toFloat(av)
		bf, _ := toFloat(bv)
		switch {
		case af < bf:
			return -1
		case af > bf:
			return 1
		}
		return 0
	}
	return compareSortValues(av, bv)
}

func hashString(s string) uint32 {
	var h uint32 = 2166136261
	for i := 0; i < len(s); i++ {
		h ^= uint32(s[i])
		h *= 16777619
	}
	return h
}

func collectNamedQueries(q map[string]any, qc *queryCtx, out map[string]float64) {
	walkNamedClause(q, qc, out)
}

func walkNamedClause(cm map[string]any, qc *queryCtx, out map[string]float64) {
	if cm == nil {
		return
	}
	name, _ := cm["_name"].(string)
	for k, v := range cm {
		switch k {
		case "_name", "boost":
			continue
		case "bool":
			bm, ok := v.(map[string]any)
			if !ok {
				continue
			}
			for _, key := range []string{"must", "should", "filter", "must_not"} {
				for _, item := range asQueryList(bm[key]) {
					if child, ok := item.(map[string]any); ok {
						walkNamedClause(child, qc, out)
					}
				}
			}
		default:
			nm := name
			if opt, ok := v.(map[string]any); ok {
				if n, ok := opt["_name"].(string); ok {
					nm = n
				}
			}
			if fm, ok := v.(map[string]any); ok {
				for _, fv := range fm {
					if opt, ok := fv.(map[string]any); ok {
						if n, ok := opt["_name"].(string); ok {
							nm = n
						}
					}
				}
			}
			if nm != "" {
				evalMap := cm
				if _, has := cm["_name"]; has {
					evalMap = map[string]any{}
					for ck, cv := range cm {
						if ck != "_name" {
							evalMap[ck] = cv
						}
					}
				}
				matched, score, err := qc.eval(evalMap)
				if err == nil && matched {
					out[nm] = score
				}
			}
		}
	}
}

func mappedLeafFields(idx *Index) []string {
	out := []string{}
	var walk func(prefix string, props map[string]any)
	walk = func(prefix string, props map[string]any) {
		for name, raw := range props {
			def, ok := raw.(map[string]any)
			if !ok {
				continue
			}
			path := name
			if prefix != "" {
				path = prefix + "." + name
			}
			if sub, ok := def["properties"].(map[string]any); ok {
				walk(path, sub)
				continue
			}
			out = append(out, path)
			if fields, ok := def["fields"].(map[string]any); ok {
				for sub := range fields {
					out = append(out, path+"."+sub)
				}
			}
		}
	}
	props, _ := idx.Mappings["properties"].(map[string]any)
	walk("", props)
	return out
}

func explainDescription(query map[string]any) string {
	if q, ok := query["match"].(map[string]any); ok {
		for field, raw := range q {
			text, _ := queryText(raw)
			term := ""
			if toks := analyze(text); len(toks) > 0 {
				term = toks[0]
			}
			return "weight(" + field + ":" + term + " in 0) [PerFieldSimilarity], result of:"
		}
	}
	if q, ok := query["match_phrase"].(map[string]any); ok {
		for field, raw := range q {
			text, _ := queryText(raw)
			term := ""
			if toks := analyze(text); len(toks) > 0 {
				term = toks[0]
			}
			return "weight(" + field + ":" + term + " in 0) [PerFieldSimilarity], result of:"
		}
	}
	for k := range query {
		if k == "match_all" {
			return "match_all"
		}
	}
	return "sum of:"
}

func applyCollapse(matched []multiHit, body map[string]any) ([]multiHit, map[string]map[string]any, map[string]map[string]any, *apiError) {
	collapse, ok := body["collapse"].(map[string]any)
	if !ok {
		return matched, nil, nil, nil
	}
	if _, has := body["rescore"]; has {
		return nil, nil, nil, &apiError{Type: "search_phase_execution_exception", Status: 500, Reason: "all shards failed",
			RootCause: &apiError{Type: "search_exception", Reason: "cannot use `collapse` in conjunction with `rescore`"}}
	}
	field, _ := collapse["field"].(string)
	var def map[string]any
	found := false
	for _, h := range matched {
		if d, ok := mappingFieldDef(h.idx, field); ok {
			def = d
			found = true
			break
		}
	}
	if !found {
		reason := "no mapping found for `" + field + "` in order to collapse on"
		return nil, nil, nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	ftype, _ := def["type"].(string)
	if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
		reason := "unknown type for collapse field `" + field + "`, only keywords and numbers are accepted"
		return nil, nil, nil, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: reason}}
	}
	for _, h := range matched {
		if vals := h.idx.DocValues[h.id]; vals != nil && len(vals[field]) > 1 {
			reason := "the collapse field must be single valued"
			return nil, nil, nil, &apiError{Type: "search_phase_execution_exception", Status: 500, Reason: "all shards failed",
				RootCause: &apiError{Type: "illegal_state_exception", Reason: reason},
				CausedBy:  &apiError{Type: "illegal_state_exception", Reason: reason}}
		}
	}
	groups := map[string][]multiHit{}
	order := []string{}
	for _, h := range matched {
		key := ""
		vals := h.idx.DocValues[h.id]
		if vals != nil && len(vals[field]) > 0 {
			s, _ := valueScalarString(vals[field][0])
			key = s
		} else {
			key = "\x00missing:" + h.idx.Name + ":" + h.id
		}
		if _, exists := groups[key]; !exists {
			order = append(order, key)
		}
		groups[key] = append(groups[key], h)
	}
	collapsed := make([]multiHit, 0, len(order))
	fieldsByID := map[string]map[string]any{}
	for _, key := range order {
		g := groups[key]
		if len(g) == 0 {
			continue
		}
		h := g[0]
		collapsed = append(collapsed, h)
		vals := h.idx.DocValues[h.id]
		if vals != nil && len(vals[field]) > 0 {
			fieldsByID[h.id] = map[string]any{field: []any{vals[field][0]}}
		}
	}
	innerByID := map[string]map[string]any{}
	innerRaw := collapse["inner_hits"]
	if innerRaw != nil {
		specs := []map[string]any{}
		switch t := innerRaw.(type) {
		case map[string]any:
			specs = append(specs, t)
		case []any:
			for _, e := range t {
				if m, ok := e.(map[string]any); ok {
					specs = append(specs, m)
				}
			}
		}
		for _, spec := range specs {
			name, _ := spec["name"].(string)
			if name == "" {
				return nil, nil, nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "Field name cannot be null"}
			}
			size := 3
			if n, ok := toFloat(spec["size"]); ok {
				size = int(n)
			}
			srcFalse := false
			if v, ok := spec["_source"].(bool); ok && !v {
				srcFalse = true
			}
			var specs2 []searchSortSpec
			if rawSort, has := spec["sort"]; has {
				var serr *apiError
				specs2, serr = parseSortSpecs(rawSort, "")
				if serr != nil {
					return nil, nil, nil, serr
				}
			}
			for _, key := range order {
				outer := groups[key][0]
				g := append([]multiHit{}, groups[key]...)
				if len(specs2) > 0 {
					sort.SliceStable(g, func(i, j int) bool {
						for _, sp := range specs2 {
							c := compareSortSpec(g[i], g[j], sp)
							if c != 0 {
								if sp.desc {
									return c > 0
								}
								return c < 0
							}
						}
						return false
					})
				}
				if len(g) > size {
					g = g[:size]
				}
				innerHits := []any{}
				for _, ih := range g {
					e := map[string]any{"_index": ih.idx.Name, "_id": ih.id, "_score": nil}
					if len(specs2) > 0 {
						sv := []any{}
						for _, sp := range specs2 {
							sv = append(sv, sortOutputValue(ih, sp))
						}
						e["sort"] = sv
					}
					if !srcFalse {
						e["_source"] = ih.doc
					}
					innerHits = append(innerHits, e)
				}
				obj := map[string]any{"hits": map[string]any{
					"total":     map[string]any{"value": len(groups[key]), "relation": "eq"},
					"max_score": nil,
					"hits":      innerHits,
				}}
				if innerByID[outer.id] == nil {
					innerByID[outer.id] = map[string]any{}
				}
				innerByID[outer.id][name] = obj
			}
		}
	}
	return collapsed, fieldsByID, innerByID, nil
}

func applyRescore(matched []multiHit, body map[string]any) *apiError {
	raw, has := body["rescore"]
	if !has || raw == nil {
		return nil
	}
	if s, ok := body["sort"].([]any); ok && len(s) > 0 {
		return &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: "all shards failed",
			RootCause: &apiError{Type: "illegal_argument_exception", Reason: "Cannot use [sort] option in conjunction with [rescore]."}}
	}
	specs := []map[string]any{}
	switch t := raw.(type) {
	case map[string]any:
		specs = append(specs, t)
	case []any:
		for _, e := range t {
			if m, ok := e.(map[string]any); ok {
				specs = append(specs, m)
			}
		}
	}
	for _, spec := range specs {
		window := 10
		if n, ok := toFloat(spec["window_size"]); ok {
			window = int(n)
		}
		qm, _ := spec["query"].(map[string]any)
		if qm == nil {
			continue
		}
		rq, _ := qm["rescore_query"].(map[string]any)
		if rq == nil {
			continue
		}
		mode := "total"
		if m, ok := qm["score_mode"].(string); ok && m != "" {
			mode = m
		}
		qw := 1.0
		if v, ok := toFloat(qm["query_weight"]); ok {
			qw = v
		}
		rw := 1.0
		if v, ok := toFloat(qm["rescore_query_weight"]); ok {
			rw = v
		}
		order := make([]int, len(matched))
		for i := range order {
			order[i] = i
		}
		sort.SliceStable(order, func(a, b int) bool {
			return matched[order[a]].score > matched[order[b]].score
		})
		if window < len(order) {
			order = order[:window]
		}
		for _, i := range order {
			h := &matched[i]
			qc := newQueryCtxForDoc(h.idx, h.id, h.doc)
			ok, rs, err := qc.eval(rq)
			if err != nil {
				return err
			}
			base := h.score * qw
			if !ok || rs == 0 {
				h.score = base
				continue
			}
			weighted := rs * rw
			switch mode {
			case "multiply":
				h.score = base * weighted
			case "avg":
				h.score = (base + weighted) / 2
			case "max":
				if weighted > base {
					h.score = weighted
				} else {
					h.score = base
				}
			case "min":
				if weighted < base {
					h.score = weighted
				} else {
					h.score = base
				}
			default:
				h.score = base + weighted
			}
		}
	}
	return nil
}
