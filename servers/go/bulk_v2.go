package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
)

type bulkAction struct {
	line       int
	op         string
	meta       map[string]any
	indexName  string
	id         string
	idGiven    bool
	source     map[string]any
	sourceErr  *apiError
	updateBody map[string]any
	updateErr  *apiError
}

func (s *Server) handleBulkV2(w http.ResponseWriter, r *http.Request, pathIndex string) {
	raw, err := io.ReadAll(r.Body)
	if err != nil {
		writeError(w, errBad("failed to read bulk body"))
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(raw) == 0 {
		writeError(w, &apiError{Type: "parse_exception", Status: 400, Reason: "request body is required"})
		return
	}
	if !strings.HasSuffix(string(raw), "\n") {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "The bulk request must be terminated by a newline [\\n]"})
		return
	}
	lines := strings.Split(string(raw), "\n")
	actions := []bulkAction{}
	for i := 0; i < len(lines); i++ {
		line := strings.TrimSuffix(lines[i], "\r")
		if strings.TrimSpace(line) == "" {
			continue
		}
		lineNo := i + 1
		trimmed := strings.TrimSpace(line)
		if trimmed[0] != '{' {
			found := "VALUE_STRING"
			switch trimmed[0] {
			case '[':
				found = "START_ARRAY"
			case ']':
				found = "END_ARRAY"
			default:
				if trimmed[0] == '-' || (trimmed[0] >= '0' && trimmed[0] <= '9') {
					found = "VALUE_NUMBER"
				}
			}
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("Malformed action/metadata line [%d], expected START_OBJECT but found [%s]", lineNo, found)})
			return
		}
		op, meta, perr := parseBulkActionLine(trimmed)
		if perr != nil {
			perr.Reason = fmt.Sprintf("Malformed action/metadata line [%d], %s", lineNo, perr.Reason)
			writeError(w, perr)
			return
		}
		switch op {
		case "index", "create", "delete", "update":
		default:
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: fmt.Sprintf("Malformed action/metadata line [%d], expected one of [create, delete, index, update] but found [%s]", lineNo, op)})
			return
		}
		allowed := map[string]bool{"_index": true, "_id": true, "routing": true, "version": true,
			"version_type": true, "if_seq_no": true, "if_primary_term": true, "pipeline": true,
			"require_alias": true, "retry_on_conflict": true, "_source": true}
		for k := range meta {
			if !allowed[k] {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: fmt.Sprintf("Action/metadata line [%d] contains an unknown parameter [%s]", lineNo, k)})
				return
			}
		}
		if v, ok := meta["_source"]; ok {
			switch v.(type) {
			case []any:
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "expected a simple value for field [_source] but found [START_ARRAY]"})
				return
			case map[string]any:
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "expected a simple value for field [_source] but found [START_OBJECT]"})
				return
			}
		}
		idxName := pathIndex
		if v, ok := meta["_index"].(string); ok && v != "" {
			idxName = v
		}
		if idxName == "" {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: index is missing;"})
			return
		}
		id := ""
		idGiven := false
		if v, ok := meta["_id"]; ok {
			idGiven = true
			id, _ = valueToScriptString(v)
		}
		if (op == "delete" || op == "update") && !idGiven {
			writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: id is missing;"})
			return
		}
		a := bulkAction{line: lineNo, op: op, meta: meta, indexName: idxName, id: id, idGiven: idGiven}
		if op != "delete" {
			if i+1 >= len(lines) || strings.TrimSpace(strings.TrimSuffix(lines[i+1], "\r")) == "" {
				writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
					Reason: "Validation Failed: 1: no requests added;"})
				return
			}
			i++
			srcLine := strings.TrimSpace(strings.TrimSuffix(lines[i], "\r"))
			var srcAny any
			if err := unmarshalJSON([]byte(srcLine), &srcAny); err != nil {
				a.sourceErr = &apiError{Type: "mapper_parsing_exception", Status: 400,
					Reason:   "Failed to parse request body",
					CausedBy: &apiError{Type: "json_parse_exception", Reason: err.Error()}}
			} else {
				src, ok := srcAny.(map[string]any)
				if !ok {
					a.sourceErr = &apiError{Type: "mapper_parsing_exception", Status: 400,
						Reason: "Failed to parse request body"}
				} else if op == "update" {
					a.updateBody = src
					allowedUp := map[string]bool{"doc": true, "upsert": true, "script": true,
						"scripted_upsert": true, "doc_as_upsert": true, "detect_noop": true,
						"_source": true, "retry_on_conflict": true}
					for k := range src {
						if !allowedUp[k] {
							writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
								Reason: "[UpdateRequest] unknown field [" + k + "]"})
							return
						}
					}
					if _, ok := src["doc"].(map[string]any); !ok {
						if _, has := src["doc"]; has {
							a.updateErr = &apiError{Type: "mapper_parsing_exception", Status: 400,
								Reason: "[doc] must be an object"}
						}
					}
				} else {
					a.source = src
				}
			}
		}
		actions = append(actions, a)
	}
	if len(actions) == 0 {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: no requests added;"})
		return
	}
	rv, _ := refreshValue(r)
	requireAliasURL := queryBool(r, "require_alias")
	urlRouting := r.URL.Query().Get("routing")
	urlPipeline := r.URL.Query().Get("pipeline")
	urlSource := r.URL.Query().Get("_source")
	hasErrors := false
	usedPipeline := urlPipeline != ""
	for _, a := range actions {
		if metaString(a.meta, "pipeline") != "" {
			usedPipeline = true
		}
	}
	items := make([]any, 0, len(actions))
	for _, a := range actions {
		item, isErr := s.runBulkAction(a, rv, requireAliasURL, urlRouting, urlPipeline, urlSource)
		if isErr {
			hasErrors = true
		}
		items = append(items, item)
	}
	resp := orderedJSON{{"took", int64(1)}, {"errors", hasErrors}, {"items", items}}
	if usedPipeline {
		resp = append(resp, orderedKV{"ingest_took", int64(0)})
	}
	writeJSON(w, 200, resp)
}

func parseBulkActionLine(line string) (string, map[string]any, *apiError) {
	dec := json.NewDecoder(strings.NewReader(line))
	dec.UseNumber()
	tok, err := dec.Token()
	if err != nil {
		return "", nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	if d, ok := tok.(json.Delim); !ok || d != '{' {
		return "", nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "expected START_OBJECT"}
	}
	kt, err := dec.Token()
	if err != nil {
		return "", nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "expected action name"}
	}
	op, _ := kt.(string)
	var all map[string]any
	if err := unmarshalJSON([]byte(line), &all); err != nil {
		return "", nil, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: err.Error()}
	}
	meta, _ := all[op].(map[string]any)
	if meta == nil {
		meta = map[string]any{}
	}
	return op, meta, nil
}

func (s *Server) bulkResolve(name string, requireAlias bool) (*Index, *apiError) {
	if requireAlias {
		if _, concrete := s.indices[name]; concrete {
			return nil, &apiError{Type: "index_not_found_exception", Status: 404,
				Reason: "no such index [" + name + "] and [require_alias] request flag is [true] and is not an alias",
				Fields: map[string]any{"index": name, "resource.type": "index_or_alias", "resource.id": name, "index_uuid": "_na_"}}
		}
		idx, err := s.resolveWriteTarget(name)
		if err != nil {
			if err.Type == "index_not_found_exception" {
				err.Reason = "no such index [" + name + "] and [require_alias] request flag is [true] and is not an alias"
			}
			return nil, err
		}
		return idx, nil
	}
	return s.resolveWriteTargetV2(name)
}

func metaString(meta map[string]any, key string) string {
	if v, ok := meta[key]; ok {
		s, _ := valueToScriptString(v)
		return s
	}
	return ""
}

func metaBool(meta map[string]any, key string) bool {
	b, _ := meta[key].(bool)
	return b
}

func metaInt(meta map[string]any, key string) (int64, bool) {
	v, ok := meta[key]
	if !ok {
		return 0, false
	}
	if n, ok := integerOf(v); ok {
		return n, true
	}
	if f, ok := toFloat(v); ok {
		return int64(f), true
	}
	return 0, false
}

func bulkVersion(idx *Index, id string, meta map[string]any, exists bool) (int64, *apiError) {
	old := currentVersion(idx, id)
	v, has := metaInt(meta, "version")
	vt := metaString(meta, "version_type")
	if !has {
		if vt == "external" || vt == "external_gte" {
			return 0, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: illegal version value [null];"}
		}
		if vt != "" && vt != "internal" {
			return 0, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "No version type match [" + vt + "]"}
		}
		return old + 1, nil
	}
	switch vt {
	case "internal":
		return 0, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead;"}
	case "external":
		if v < 0 {
			return 0, &apiError{Type: "action_request_validation_exception", Status: 400,
				Reason: "Validation Failed: 1: illegal version value [" + strconv.FormatInt(v, 10) + "];"}
		}
		if exists && old >= v {
			return 0, versionConflict(idx, id, fmt.Sprintf("[%s]: version conflict, current version [%d] is higher or equal to the one provided [%d]", id, old, v))
		}
		return v, nil
	case "external_gte":
		if exists && old > v {
			return 0, versionConflict(idx, id, fmt.Sprintf("[%s]: version conflict, current version [%d] is higher than the one provided [%d]", id, old, v))
		}
		return v, nil
	default:
		return 0, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "No version type match [" + vt + "]"}
	}
}

func bulkIfSeq(idx *Index, id string, meta map[string]any) *apiError {
	seq, hasSeq := metaInt(meta, "if_seq_no")
	term, hasTerm := metaInt(meta, "if_primary_term")
	if !hasSeq && !hasTerm {
		return nil
	}
	if hasSeq != hasTerm {
		return &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: ifSeqNo and ifPrimaryTerm must be set together;"}
	}
	curSeq, ok := idx.SeqNos[id]
	if !ok {
		curSeq = -2
	}
	curTerm := idx.PrimaryTerms[id]
	if curTerm == 0 {
		curTerm = 1
	}
	if !ok || curSeq != seq || curTerm != term {
		return versionConflict(idx, id, fmt.Sprintf("[%s]: version conflict, required seqNo [%d], primary term [%d]. current document has seqNo [%d] and primary term [%d]", id, seq, term, curSeq, curTerm))
	}
	return nil
}

func bulkItem(op, index, id string, version, seq, term int64, result string, status int, forced bool) orderedJSON {
	it := orderedJSON{{"_index", index}, {"_id", id}, {"_version", version}, {"result", result}}
	if forced {
		it = append(it, orderedKV{"forced_refresh", true})
	}
	it = append(it,
		orderedKV{"_shards", map[string]any{"total": 1, "successful": 1, "failed": 0}},
		orderedKV{"_seq_no", seq}, orderedKV{"_primary_term", term}, orderedKV{"status", status})
	return it
}

func bulkErrItem(op, index string, id any, e *apiError) orderedJSON {
	return orderedJSON{{op, orderedJSON{
		{"_index", index}, {"_id", id}, {"status", e.Status}, {"error", bulkCause(e)},
	}}}
}

func bulkCause(e *apiError) map[string]any {
	m := map[string]any{"type": e.Type, "reason": e.Reason}
	for k, v := range e.Fields {
		m[k] = v
	}
	if e.CausedBy != nil {
		m["caused_by"] = bulkCause(e.CausedBy)
	}
	return m
}

func bulkOKItem(op string, body orderedJSON) orderedJSON {
	return orderedJSON{{op, body}}
}

func (s *Server) runBulkAction(a bulkAction, rv string, requireAliasURL bool, urlRouting, urlPipeline, urlSource string) (any, bool) {
	if a.sourceErr != nil {
		return bulkErrItem(a.op, a.indexName, a.id, a.sourceErr), true
	}
	if a.updateErr != nil {
		return bulkErrItem("update", a.indexName, a.id, a.updateErr), true
	}
	if a.idGiven && a.id == "" && a.op != "delete" && a.op != "update" {
		return bulkErrItem(a.op, a.indexName, "", &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "if _id is specified it must not be empty"}), true
	}
	requireAlias := metaBool(a.meta, "require_alias") || requireAliasURL
	idx, apiErr := s.bulkResolve(a.indexName, requireAlias)
	if apiErr != nil {
		var id any = a.id
		if apiErr.Type == "invalid_index_name_exception" {
			id = nil
		}
		return bulkErrItem(a.op, a.indexName, id, apiErr), true
	}
	if idx.Closed {
		return bulkErrItem(a.op, idx.Name, a.id, errIndexClosed(idx)), true
	}
	if berr := writeBlockError(idx); berr != nil {
		return bulkErrItem(a.op, idx.Name, a.id, berr), true
	}
	ensureDocState(idx)
	routing := metaString(a.meta, "routing")
	if routing == "" {
		routing = urlRouting
	}
	if routing == "" {
		routing = aliasRoutingFor(idx, a.indexName)
	}
	if mappingRoutingRequired(idx) && routing == "" {
		return bulkErrItem(a.op, idx.Name, a.id, routingMissing(idx, a.id)), true
	}
	id := a.id
	if id == "" && a.op != "delete" && a.op != "update" {
		id = randomID20(s)
	}
	switch a.op {
	case "delete":
		exists := false
		if _, ok := idx.Docs[id]; ok && routingMatches(idx, id, routing) {
			exists = true
		}
		if err := bulkIfSeq(idx, id, a.meta); err != nil {
			return bulkErrItem("delete", idx.Name, id, err), true
		}
		ver, err := bulkVersion(idx, id, a.meta, exists)
		if err != nil {
			return bulkErrItem("delete", idx.Name, id, err), true
		}
		seq := nextSeq(idx)
		idx.SeqNos[id] = seq
		idx.PrimaryTerms[id] = 1
		idx.Versions[id] = ver
		if !exists {
			return bulkOKItem("delete", bulkItem("delete", idx.Name, id, ver, seq, 1, "not_found", 404, false)), false
		}
		delete(idx.Docs, id)
		delete(idx.Pending, id)
		delete(idx.RawDocs, id)
		idx.removeDocValues(id)
		idx.DeleteTotal++
		return bulkOKItem("delete", bulkItem("delete", idx.Name, id, ver, seq, 1, "deleted", 200, false)), false
	case "update":
		return s.runBulkUpdate(idx, a, id, routing, rv, urlSource)
	default:
		return s.runBulkIndex(idx, a, id, routing, rv, urlPipeline)
	}
}

func (s *Server) runBulkIndex(idx *Index, a bulkAction, id, routing, rv, urlPipeline string) (any, bool) {
	_, exists := idx.Docs[id]
	if a.op == "create" && exists {
		return bulkErrItem("create", idx.Name, id, versionConflict(idx, id,
			fmt.Sprintf("[%s]: version conflict, document already exists (current version [%d])", id, currentVersion(idx, id)))), true
	}
	if err := bulkIfSeq(idx, id, a.meta); err != nil {
		return bulkErrItem(a.op, idx.Name, id, err), true
	}
	ver, err := bulkVersion(idx, id, a.meta, exists)
	if err != nil {
		return bulkErrItem(a.op, idx.Name, id, err), true
	}
	pipeline := metaString(a.meta, "pipeline")
	if pipeline == "" {
		pipeline = urlPipeline
	}
	if pipeline != "" && pipeline != "_none" {
		if _, ok := s.ingestPipelines[pipeline]; !ok {
			return bulkErrItem(a.op, idx.Name, id, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "pipeline with id [" + pipeline + "] does not exist"}), true
		}
	}
	params := map[string]any{}
	if pipeline != "" {
		params["pipeline"] = pipeline
	}
	src, apiErr := s.applyPipelines(idx, deepCopyMap(a.source), params)
	if apiErr != nil {
		if apiErr.Type == "drop_processor_exception" {
			return map[string]any{a.op: map[string]any{"_index": idx.Name, "_id": id, "_version": -3,
				"result": "noop", "status": 200, "_shards": map[string]any{"total": 0, "successful": 0, "failed": 0}}}, false
		}
		return bulkErrItem(a.op, idx.Name, id, apiErr), true
	}
	props := ensureProperties(idx.Mappings)
	if derr := mappingShapeConflict(props, src); derr != nil {
		return bulkErrItem(a.op, idx.Name, id, derr), true
	}
	if derr := checkDynamicStrict(idx, props, src); derr != nil {
		return bulkErrItem(a.op, idx.Name, id, derr), true
	}
	mergeMappingForIndex(idx, props, src)
	idx.FieldTypes = map[string]string{}
	buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
	if derr := idx.computeDocValues(id, src); derr != nil {
		return bulkErrItem(a.op, idx.Name, id, derr), true
	}
	if !exists {
		idx.Order = append(idx.Order, id)
	}
	idx.Docs[id] = src
	delete(idx.RawDocs, id)
	idx.Versions[id] = ver
	seq := nextSeq(idx)
	idx.SeqNos[id] = seq
	idx.PrimaryTerms[id] = 1
	idx.IndexTotal++
	if routing != "" {
		idx.Routing[id] = routing
	} else {
		delete(idx.Routing, id)
	}
	if rv == "true" || rv == "wait_for" {
		delete(idx.Pending, id)
	} else {
		idx.Pending[id] = src
	}
	result := "created"
	status := 201
	if exists {
		result = "updated"
		status = 200
	}
	return bulkOKItem(a.op, bulkItem(a.op, idx.Name, id, ver, seq, 1, result, status, rv == "true")), false
}

func (s *Server) runBulkUpdate(idx *Index, a bulkAction, id, routing, rv, urlSource string) (any, bool) {
	body := a.updateBody
	docPatch, _ := body["doc"].(map[string]any)
	script, hasScript := body["script"]
	upsert, hasUpsert := body["upsert"].(map[string]any)
	docAsUpsert, _ := body["doc_as_upsert"].(bool)
	scriptedUpsert, _ := body["scripted_upsert"].(bool)
	detectNoop := true
	if b, ok := body["detect_noop"].(bool); ok {
		detectNoop = b
	}
	old, exists := idx.Docs[id]
	if exists && !routingMatches(idx, id, routing) {
		exists = false
	}
	if !exists {
		if !hasUpsert && !docAsUpsert {
			return bulkErrItem("update", idx.Name, id, &apiError{Type: "document_missing_exception", Status: 404,
				Reason: "[" + id + "]: document missing", Fields: map[string]any{"index": idx.Name, "shard": "0"}}), true
		}
		var src map[string]any
		if docAsUpsert {
			src = deepCopyMap(docPatch)
		} else {
			src = deepCopyMap(upsert)
		}
		op := "index"
		if hasScript && scriptedUpsert {
			res, serr := runUpdateScript(script, src, idx.Name, id, 0)
			if serr != nil {
				return bulkErrItem("update", idx.Name, id, serr), true
			}
			op, src = res.op, res.source
		}
		if op == "none" || op == "noop" {
			it := bulkItem("update", idx.Name, id, -1, 0, 1, "noop", 200, false)
			return bulkOKItem("update", it), false
		}
		props := ensureProperties(idx.Mappings)
		if derr := mappingShapeConflict(props, src); derr != nil {
			return bulkErrItem("update", idx.Name, id, derr), true
		}
		mergeMappingForIndex(idx, props, src)
		idx.FieldTypes = map[string]string{}
		buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
		if derr := idx.computeDocValues(id, src); derr != nil {
			return bulkErrItem("update", idx.Name, id, derr), true
		}
		idx.Order = append(idx.Order, id)
		idx.Docs[id] = src
		delete(idx.RawDocs, id)
		idx.Versions[id] = 1
		seq := nextSeq(idx)
		idx.SeqNos[id] = seq
		idx.PrimaryTerms[id] = 1
		idx.IndexTotal++
		if routing != "" {
			idx.Routing[id] = routing
		}
		if rv == "true" || rv == "wait_for" {
			delete(idx.Pending, id)
		} else {
			idx.Pending[id] = src
		}
		it := bulkItem("update", idx.Name, id, 1, seq, 1, "created", 201, rv == "true")
		if get, ok := bulkGetObject(idx, id, src, seq, a.meta, body, urlSource); ok {
			it = append(it, orderedKV{"get", get})
		}
		return bulkOKItem("update", it), false
	}
	newDoc := deepCopyMap(old)
	op := "index"
	if hasScript {
		res, serr := runUpdateScript(script, newDoc, idx.Name, id, currentVersion(idx, id))
		if serr != nil {
			return bulkErrItem("update", idx.Name, id, serr), true
		}
		op, newDoc = res.op, res.source
	} else {
		deepMergeDoc(newDoc, docPatch)
	}
	if op == "delete" {
		ver := currentVersion(idx, id) + 1
		seq := nextSeq(idx)
		delete(idx.Docs, id)
		delete(idx.Pending, id)
		delete(idx.RawDocs, id)
		idx.removeDocValues(id)
		idx.DeleteTotal++
		idx.Versions[id] = ver
		idx.SeqNos[id] = seq
		return bulkOKItem("update", bulkItem("update", idx.Name, id, ver, seq, 1, "deleted", 200, false)), false
	}
	if op == "none" || op == "noop" || (detectNoop && reflectDeepEqual(old, newDoc)) {
		it := bulkItem("update", idx.Name, id, currentVersion(idx, id), idx.SeqNos[id], 1, "noop", 200, false)
		if get, ok := bulkGetObject(idx, id, old, idx.SeqNos[id], a.meta, body, urlSource); ok {
			it = append(it, orderedKV{"get", get})
		}
		return bulkOKItem("update", it), false
	}
	props := ensureProperties(idx.Mappings)
	if derr := mappingShapeConflict(props, newDoc); derr != nil {
		return bulkErrItem("update", idx.Name, id, derr), true
	}
	mergeMappingForIndex(idx, props, newDoc)
	idx.FieldTypes = map[string]string{}
	buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
	if derr := idx.computeDocValues(id, newDoc); derr != nil {
		return bulkErrItem("update", idx.Name, id, derr), true
	}
	ver := currentVersion(idx, id) + 1
	seq := nextSeq(idx)
	idx.Docs[id] = newDoc
	delete(idx.RawDocs, id)
	idx.Versions[id] = ver
	idx.SeqNos[id] = seq
	idx.PrimaryTerms[id] = 1
	idx.IndexTotal++
	if rv == "true" || rv == "wait_for" {
		delete(idx.Pending, id)
	} else {
		idx.Pending[id] = newDoc
	}
	it := bulkItem("update", idx.Name, id, ver, seq, 1, "updated", 200, rv == "true")
	if get, ok := bulkGetObject(idx, id, newDoc, seq, a.meta, body, urlSource); ok {
		it = append(it, orderedKV{"get", get})
	}
	return bulkOKItem("update", it), false
}

func bulkGetObject(idx *Index, id string, src map[string]any, seq int64, meta, body map[string]any, urlSource string) (orderedJSON, bool) {
	requested := false
	if v, ok := meta["_source"]; ok {
		if b, isBool := v.(bool); isBool && b {
			requested = true
		}
	}
	if v, ok := body["_source"]; ok {
		if b, isBool := v.(bool); isBool && b {
			requested = true
		}
	}
	if urlSource != "" {
		requested = true
	}
	if !requested {
		return nil, false
	}
	includes, excludes := []string(nil), []string(nil)
	if urlSource != "" {
		includes = splitSourceList(urlSource)
	}
	out := orderedJSON{{"_seq_no", seq}, {"_primary_term", int64(1)}, {"found", true}}
	if mappingSourceEnabled(idx) {
		out = append(out, orderedKV{"_source", filterSourceDoc(src, includes, excludes)})
	}
	return out, true
}

func reflectDeepEqual(a, b any) bool {
	ab, err1 := json.Marshal(a)
	bb, err2 := json.Marshal(b)
	return err1 == nil && err2 == nil && string(ab) == string(bb)
}
