package main

import (
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"
)

var byQueryParams = map[string]bool{
	"refresh": true, "conflicts": true, "max_docs": true, "slices": true,
	"q": true, "routing": true, "timeout": true, "wait_for_active_shards": true,
	"wait_for_completion": true, "requests_per_second": true, "scroll": true,
	"scroll_size": true, "search_timeout": true, "search_type": true,
	"error_trace": true, "human": true, "pretty": true, "filter_path": true,
	"ignore_unavailable": true, "allow_no_indices": true, "expand_wildcards": true,
	"pipeline": true,
}

func byQueryReport() orderedJSON {
	return orderedJSON{
		{"took", int64(0)}, {"timed_out", false}, {"total", int64(0)},
		{"deleted", int64(0)}, {"batches", int64(0)}, {"version_conflicts", int64(0)},
		{"noops", int64(0)}, {"retries", map[string]any{"bulk": 0, "search": 0}},
		{"throttled_millis", int64(0)}, {"requests_per_second", -1.0},
		{"throttled_until_millis", int64(0)}, {"failures", []any{}},
	}
}

func (s *Server) handleDeleteByQuery(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.runByQuery(w, r, name, true)
}

func (s *Server) handleUpdateByQuery(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.runByQuery(w, r, name, false)
}

func (s *Server) runByQuery(w http.ResponseWriter, r *http.Request, name string, isDelete bool) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	if err := unknownParams(r, byQueryParams); err != nil {
		writeError(w, err)
		return
	}
	conflicts := r.URL.Query().Get("conflicts")
	if conflicts != "" && conflicts != "proceed" && conflicts != "abort" {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: fmt.Sprintf("conflicts may only be \"proceed\" or \"abort\" but was [%s]", conflicts)})
		return
	}
	rv, rerr := refreshValue(r)
	if rerr != nil {
		writeError(w, rerr)
		return
	}
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	query, hasQuery := body["query"]
	if qp := r.URL.Query().Get("q"); qp != "" {
		query = parseQParam(qp)
		hasQuery = true
	}
	if !hasQuery {
		if isDelete {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: query is missing;"})
			return
		}
		query = map[string]any{"match_all": map[string]any{}}
	}
	qmap, ok := query.(map[string]any)
	if !ok {
		writeError(w, errParsing("[query] malformed"))
		return
	}
	maxDocs := int64(-1)
	if v, ok := body["max_docs"]; ok {
		if n, ok := toFloat(v); ok {
			maxDocs = int64(n)
		}
	}
	if v := r.URL.Query().Get("max_docs"); v != "" {
		if n, err := strconv.ParseInt(v, 10, 64); err == nil {
			maxDocs = n
		}
	}
	sliceCount := 0
	if v := r.URL.Query().Get("slices"); v != "" && v != "auto" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			sliceCount = n
		}
	}
	script, hasScript := body["script"]
	if hasScript && !isDelete {
		if err := validateByQueryScript(script); err != nil {
			writeError(w, err)
			return
		}
	}
	type sliceStat struct {
		total   int64
		deleted int64
		updated int64
		noops   int64
	}
	slices := make([]sliceStat, sliceCount)
	report := byQueryReport()
	total, deleted, updated, noops := int64(0), int64(0), int64(0), int64(0)
	var failures []any
	start := time.Now()
	for _, id := range append([]string(nil), idx.Order...) {
		if maxDocs >= 0 && total >= maxDocs {
			break
		}
		doc, live := idx.Docs[id]
		if !live {
			continue
		}
		if _, pending := idx.Pending[id]; pending {
			continue
		}
		qc := newQueryCtxForDoc(idx, id, doc)
		match, _, qerr := qc.eval(qmap)
		if qerr != nil {
			writeError(w, qerr)
			return
		}
		if !match {
			continue
		}
		total++
		si := -1
		if sliceCount > 0 {
			si = int((total - 1) % int64(sliceCount))
			slices[si].total++
		}
		if isDelete {
			base := currentVersion(idx, id)
			seq := nextSeq(idx)
			delete(idx.Docs, id)
			delete(idx.Pending, id)
			idx.removeDocValues(id)
			idx.DeleteTotal++
			idx.Versions[id] = base + 1
			idx.SeqNos[id] = seq
			deleted++
			if si >= 0 {
				slices[si].deleted++
			}
			continue
		}
		newDoc := deepCopyMap(doc)
		op := "index"
		if hasScript {
			res, serr := runUpdateScript(script, newDoc, idx.Name, id, currentVersion(idx, id))
			if serr != nil {
				if serr.CausedBy != nil && serr.CausedBy.Type == "script_exception" {
					writeError(w, &apiError{Type: "script_exception", Status: serr.Status,
						Reason: serr.CausedBy.Reason, CausedBy: serr.CausedBy.CausedBy})
					return
				}
				failures = append(failures, byQueryFailure(idx.Name, id, serr))
				continue
			}
			op = res.op
			newDoc = res.source
		}
		if op == "delete" {
			base := currentVersion(idx, id)
			seq := nextSeq(idx)
			delete(idx.Docs, id)
			delete(idx.Pending, id)
			idx.removeDocValues(id)
			idx.DeleteTotal++
			idx.Versions[id] = base + 1
			idx.SeqNos[id] = seq
			deleted++
			if si >= 0 {
				slices[si].deleted++
			}
			continue
		}
		pparams := map[string]any{}
		if pn := r.URL.Query().Get("pipeline"); pn != "" {
			pparams["pipeline"] = pn
		} else if pn, ok := body["pipeline"].(string); ok && pn != "" {
			pparams["pipeline"] = pn
		}
		newDoc, apiErr = s.applyPipelines(idx, newDoc, pparams)
		if apiErr != nil {
			failures = append(failures, byQueryFailure(idx.Name, id, apiErr))
			continue
		}
		if op == "none" || op == "noop" {
			noops++
			if si >= 0 {
				slices[si].noops++
			}
			continue
		}
		props := ensureProperties(idx.Mappings)
		if derr := mappingShapeConflict(props, newDoc); derr != nil {
			failures = append(failures, byQueryFailure(idx.Name, id, derr))
			continue
		}
		mergeMappingForIndex(idx, props, newDoc)
		idx.FieldTypes = map[string]string{}
		buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
		if derr := idx.computeDocValues(id, newDoc); derr != nil {
			failures = append(failures, byQueryFailure(idx.Name, id, derr))
			continue
		}
		seq := nextSeq(idx)
		idx.Docs[id] = newDoc
		idx.Versions[id] = currentVersion(idx, id) + 1
		idx.SeqNos[id] = seq
		idx.PrimaryTerms[id] = 1
		idx.IndexTotal++
		if rv == "true" || rv == "wait_for" {
			delete(idx.Pending, id)
		} else {
			idx.Pending[id] = newDoc
		}
		updated++
		if si >= 0 {
			slices[si].updated++
		}
	}
	if total > 0 {
		report[3].v = deleted
		report[4].v = int64(1)
	} else {
		report[3].v = deleted
		report[4].v = int64(0)
	}
	report[2].v = total
	report[6].v = noops
	if failures == nil {
		failures = []any{}
	}
	report[11].v = failures
	if !isDelete {
		report = append(report, orderedKV{"updated", updated})
	}
	report[0].v = time.Since(start).Milliseconds()
	if sliceCount > 0 {
		arr := make([]any, sliceCount)
		for i, st := range slices {
			obj := orderedJSON{{"slice_id", i}, {"total", st.total}}
			if isDelete {
				obj = append(obj, orderedKV{"deleted", st.deleted})
			} else {
				obj = append(obj, orderedKV{"updated", st.updated})
			}
			obj = append(obj, orderedKV{"noops", st.noops})
			if len(failures) > 0 {
				obj = append(obj, orderedKV{"failures", []any{}})
			}
			arr[i] = obj
		}
		report = append(report, orderedKV{"slices", arr})
	}
	status := 200
	if len(failures) > 0 {
		status = 400
	}
	writeJSON(w, status, report)
}

func validateByQueryScript(script any) *apiError {
	var src string
	switch t := script.(type) {
	case string:
		src = t
	case map[string]any:
		src, _ = t["source"].(string)
	}
	trimmed := strings.TrimSpace(src)
	if strings.HasSuffix(trimmed, "=") || strings.HasSuffix(trimmed, "+") && !strings.HasSuffix(trimmed, "++") ||
		strings.HasSuffix(trimmed, "-") && !strings.HasSuffix(trimmed, "--") {
		return &apiError{Type: "script_exception", Status: 400, Reason: "compile error",
			CausedBy: &apiError{Type: "illegal_argument_exception", Reason: "unexpected end of script"}}
	}
	return nil
}

func byQueryFailure(index, id string, e *apiError) map[string]any {
	return map[string]any{
		"index": index, "id": id, "status": e.Status, "cause": errorObject(e),
	}
}
