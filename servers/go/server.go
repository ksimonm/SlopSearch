package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"
)

type captureWriter struct {
	h       http.Header
	buf     bytes.Buffer
	status  int
	ordered bool
}

func (c *captureWriter) Header() http.Header { return c.h }
func (c *captureWriter) WriteHeader(s int) {
	if c.status == 0 {
		c.status = s
	}
}
func (c *captureWriter) Write(b []byte) (int, error) {
	if c.status == 0 {
		c.status = 200
	}
	return c.buf.Write(b)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=UTF-8")
	w.WriteHeader(status)
	switch t := v.(type) {
	case orderedJSON:
		b, _ := t.MarshalJSON()
		w.Write(b)
		return
	case verbatimJSON:
		w.Write(t)
		return
	}
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.Encode(v)
	w.Write(bytes.TrimRight(buf.Bytes(), "\n"))
}

func writeError(w http.ResponseWriter, e *apiError) {
	rcType, rcReason := e.Type, e.Reason
	if e.RootCause != nil {
		rcType, rcReason = e.RootCause.Type, e.RootCause.Reason
	}
	rootCause := map[string]any{"type": rcType, "reason": rcReason}
	err := map[string]any{
		"root_cause": []any{rootCause},
		"type":       e.Type,
		"reason":     e.Reason,
	}
	if e.TopType != "" {
		err["type"] = e.TopType
	}
	if e.TopReason != "" {
		err["reason"] = e.TopReason
	}
	if e.Phase != "" {
		err["phase"] = e.Phase
	}
	fields := map[string]any{}
	for k, v := range e.Fields {
		fields[k] = v
	}
	if e.Type == "index_not_found_exception" {
		if _, ok := fields["index"]; !ok {
			name := e.Reason
			if i := strings.Index(name, "["); i >= 0 {
				end := strings.Index(name[i:], "]")
				if end < 0 {
					name = name[i+1:]
				} else {
					name = name[i+1 : i+end]
				}
			}
			fields["resource.type"] = "index_or_alias"
			fields["resource.id"] = name
			fields["index_uuid"] = "_na_"
			fields["index"] = name
		}
	}
	if e.Type == "shard_not_found_exception" {
		fields["shard"] = e.Extra
	}
	for k, v := range fields {
		err[k] = v
		rootCause[k] = v
	}
	if e.CausedBy != nil {
		err["caused_by"] = causedByObject(e.CausedBy)
	}
	writeJSON(w, e.Status, map[string]any{"error": err, "status": e.Status})
}

func causedByObject(e *apiError) map[string]any {
	m := map[string]any{"type": e.Type, "reason": e.Reason}
	for k, v := range e.Fields {
		m[k] = v
	}
	if e.CausedBy != nil {
		m["caused_by"] = causedByObject(e.CausedBy)
	}
	return m
}

func writeSimpleError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]any{"error": msg, "status": status})
}

func unmarshalJSON(b []byte, v any) error {
	dec := json.NewDecoder(bytes.NewReader(b))
	dec.UseNumber()
	return dec.Decode(v)
}

func readJSON(r *http.Request) (map[string]any, *apiError) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		return nil, errBad("failed to read body")
	}
	if len(strings.TrimSpace(string(body))) == 0 {
		if src := r.URL.Query().Get("source"); src != "" {
			body = []byte(src)
		} else {
			return map[string]any{}, nil
		}
	}
	if err := checkDuplicateJSONKeys(body); err != nil {
		return nil, &apiError{Type: "json_parse_exception", Status: 400, Reason: err.Error()}
	}
	trimmed := strings.TrimSpace(string(body))
	if strings.HasPrefix(trimmed, "[") {
		return nil, &apiError{Type: "parsing_exception", Status: 400,
			Reason: "Expected [START_OBJECT] but found [START_ARRAY]"}
	}
	var out map[string]any
	if err := unmarshalJSON(body, &out); err != nil {
		return nil, &apiError{Type: "json_parse_exception", Status: 400,
			Reason: "Failed to parse request body: " + err.Error()}
	}
	return out, nil
}

func checkDuplicateJSONKeys(body []byte) error {
	dec := json.NewDecoder(bytes.NewReader(body))
	dec.UseNumber()
	var walk func() error
	walk = func() error {
		tok, err := dec.Token()
		if err != nil {
			return nil
		}
		delim, ok := tok.(json.Delim)
		if !ok {
			return nil
		}
		switch delim {
		case '{':
			seen := map[string]bool{}
			for dec.More() {
				kt, err := dec.Token()
				if err != nil {
					return nil
				}
				k, _ := kt.(string)
				if seen[k] {
					return fmt.Errorf("Duplicate Object property %q", k)
				}
				seen[k] = true
				if err := walk(); err != nil {
					return err
				}
			}
			_, _ = dec.Token()
		case '[':
			for dec.More() {
				if err := walk(); err != nil {
					return err
				}
			}
			_, _ = dec.Token()
		}
		return nil
	}
	return walk()
}

func (s *Server) getIndex(name string) (*Index, *apiError) {
	idx, ok := s.indices[name]
	if !ok {
		return nil, errNotFound(name)
	}
	return idx, nil
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	// http.max_content_length: OpenSearch answers 413 with an empty body
	if r.ContentLength > maxContentLength {
		w.Header().Set("Content-Length", "0")
		w.WriteHeader(http.StatusRequestEntityTooLarge)
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxContentLength)
	cw := &captureWriter{h: w.Header()}
	if r.ContentLength > 0 {
		base := strings.TrimSpace(strings.Split(r.Header.Get("Content-Type"), ";")[0])
		switch base {
		case "application/json", "application/x-ndjson", "application/yaml",
			"application/x-yaml", "text/yaml", "application/yml":
		default:
			writeSimpleError(cw, 406, "Content-Type header ["+base+"] is not supported")
			status := cw.status
			cw.h.Set("X-OpenSearch-Version", "OpenSearch/3.9.0 (opensearch)")
			w.WriteHeader(status)
			w.Write(bytes.TrimRight(cw.buf.Bytes(), "\n"))
			return
		}
	}
	s.route(cw, r)
	status := cw.status
	if status == 0 {
		status = 200
	}
	q := r.URL.Query()
	body := cw.buf.Bytes()
	if cw.h.Get("Content-Type") == "" {
		cw.h.Set("Content-Type", "application/json; charset=UTF-8")
	}
	if strings.HasPrefix(cw.h.Get("Content-Type"), "application/json") {
		fp := q.Get("filter_path")
		_, hasPretty := q["pretty"]
		pretty := (hasPretty && q.Get("pretty") != "false") || q.Get("pretty") == "true"
		reencoded := false
		if fp != "" && status < 400 {
			var v any
			if json.Unmarshal(body, &v) == nil {
				body = jsonMustMarshal(filterPathValue(v, fp))
				reencoded = true
			}
		}
		if !cw.ordered || reencoded {
			if pretty && status < 400 {
				var v any
				if json.Unmarshal(body, &v) == nil {
					body = prettyJSON(v)
				}
			} else {
				body = bytes.TrimRight(body, "\n")
			}
		}
		if status >= 400 && q.Get("error_trace") == "true" {
			var v map[string]any
			if json.Unmarshal(body, &v) == nil {
				if errObj, ok := v["error"].(map[string]any); ok {
					st := []any{
						fmt.Sprintf("%s: %v", "java.lang.IllegalArgumentException", errObj["reason"]),
						"	at org.opensearch.action.support.master.TransportMasterNodeAction$AsyncSingleAction.doExecute(TransportMasterNodeAction.java:210)",
					}
					errObj["stack_trace"] = st
					if rc, ok := errObj["root_cause"].([]any); ok {
						for _, c := range rc {
							if cm, ok := c.(map[string]any); ok {
								cm["stack_trace"] = st
							}
						}
					}
					body = jsonMustMarshal(v)
				}
			}
		}
		acc := r.Header.Get("Accept")
		switch {
		case q.Get("format") == "yaml" || strings.Contains(acc, "yaml"):
			var v any
			if json.Unmarshal(body, &v) == nil {
				cw.h.Set("Content-Type", "application/yaml")
				body = toYAML(v)
			}
		case strings.Contains(acc, "application/smile"):
			cw.h.Set("Content-Type", "application/smile")
		case strings.Contains(acc, "application/cbor"):
			cw.h.Set("Content-Type", "application/cbor")
		}
	}
	cw.h.Set("X-OpenSearch-Version", "OpenSearch/3.9.0 (opensearch)")
	if r.Method == http.MethodHead {
		body = nil
	}
	w.WriteHeader(status)
	w.Write(body)
}

func jsonMustMarshal(v any) []byte {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	enc.Encode(v)
	return bytes.TrimRight(buf.Bytes(), "\n")
}

func methodNotAllowed(w http.ResponseWriter, r *http.Request, allowed ...string) {
	allow := strings.Join(allowed, ",")
	w.Header().Set("Allow", allow)
	writeSimpleError(w, 405, fmt.Sprintf("Incorrect HTTP method for uri [%s] and method [%s], allowed: [%s]", r.URL.Path, r.Method, allow))
}

func (s *Server) route(w http.ResponseWriter, r *http.Request) {
	path := strings.Trim(r.URL.EscapedPath(), "/")
	segs := []string{}
	if path != "" {
		for _, rs := range strings.Split(path, "/") {
			if d, err := url.PathUnescape(rs); err == nil {
				segs = append(segs, d)
			} else {
				segs = append(segs, rs)
			}
		}
	}

	if len(segs) == 0 || segs[0] == "" {
		s.handleRoot(w, r)
		return
	}
	if handled := s.routeServerEndpoints(w, r, segs); handled {
		return
	}
	if segs[0] == "_bulk" {
		s.handleBulkV2(w, r, "")
		return
	}
	if segs[0] == "_msearch" {
		if len(segs) >= 2 && segs[1] == "template" {
			s.handleMsearchTemplate(w, r, "")
			return
		}
		s.handleMsearch(w, r, "")
		return
	}
	if segs[0] == "_ingest" {
		if s.handleIngestRoute(w, r, segs) {
			return
		}
		writeNoHandler(w, r)
		return
	}
	if segs[0] == "_scripts" {
		if s.handleScriptsRoute(w, r, segs) {
			return
		}
		writeNoHandler(w, r)
		return
	}
	if segs[0] == "_render" {
		if len(segs) >= 2 && segs[1] == "template" {
			id := ""
			if len(segs) >= 3 {
				id = strings.Join(segs[2:], "/")
			}
			s.handleRenderTemplate(w, r, id)
			return
		}
		writeNoHandler(w, r)
		return
	}
	if segs[0] == "_analyze" {
		s.handleAnalyze(w, r, "")
		return
	}
	if segs[0] == "_search" {
		if len(segs) >= 2 && segs[1] == "template" {
			s.handleSearchTemplate(w, r, "")
			return
		}
		if len(segs) >= 2 && segs[1] == "point_in_time" {
			s.handlePitRoute(w, r, segs[2:])
			return
		}
		if len(segs) >= 2 && segs[1] == "scroll" {
			s.handleScroll(w, r, segs[2:])
			return
		}
		if len(segs) >= 2 && segs[1] == "pipeline" {
			if s.handleSearchPipelineRoute(w, r, segs) {
				return
			}
		}
		s.handleSearch(w, r, "")
		return
	}
	if segs[0] == "_count" {
		s.handleCount(w, r, "")
		return
	}
	if segs[0] == "_search_shards" {
		s.handleSearchShards(w, r, "")
		return
	}
	if segs[0] == "_rank_eval" {
		s.handleRankEval(w, r, "")
		return
	}
	if segs[0] == "_validate" {
		if len(segs) >= 2 && segs[1] == "query" {
			s.handleValidateQuery(w, r, "")
			return
		}
		writeNoHandler(w, r)
		return
	}
	if segs[0] == "_field_caps" {
		s.handleFieldCaps(w, r, "")
		return
	}
	if segs[0] == "_settings" {
		filter := ""
		if len(segs) > 1 {
			filter = strings.Join(segs[1:], "/")
		}
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			methodNotAllowed(w, r, "GET", "HEAD")
			return
		}
		s.getSettingsAPI(w, r, "*", filter)
		return
	}
	switch segs[0] {
	case "_refresh", "_flush", "_forcemerge":
		if r.Method != http.MethodPost && r.Method != http.MethodGet {
			methodNotAllowed(w, r, "POST", "GET")
			return
		}
		s.maintenanceOp(w, r, "*", strings.TrimPrefix(segs[0], "_"))
		return
	case "_cache":
		if len(segs) == 2 && segs[1] == "clear" {
			s.maintenanceOp(w, r, "*", "cache")
			return
		}
		writeNoHandler(w, r)
		return
	case "_mget":
		s.handleMget(w, r, "")
		return
	case "_stats":
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			methodNotAllowed(w, r, "GET", "HEAD")
			return
		}
		metrics := ""
		if len(segs) > 1 {
			metrics = strings.Join(segs[1:], ",")
		}
		s.handleStatsAPI(w, r, "*", metrics)
		return
	case "_segments":
		s.handleSegmentsAPI(w, r, "*")
		return
	case "_recovery":
		s.handleRecoveryAPI(w, r, "*")
		return
	}
	if segs[0] == "_all" && len(segs) == 1 {
		switch r.Method {
		case http.MethodGet, http.MethodHead:
			s.getIndexMeta(w, r, "_all")
		case http.MethodDelete:
			s.deleteIndices(w, r, "_all")
		default:
			methodNotAllowed(w, r, "GET", "HEAD", "DELETE")
		}
		return
	}
	if segs[0] == "_mapping" && len(segs) >= 3 && segs[1] == "field" {
		s.getMappingAPI(w, r, "*", strings.Join(segs[2:], ","))
		return
	}
	if segs[0] == "_alias" {
		aliasSpec := ""
		if len(segs) > 1 {
			aliasSpec = strings.Join(segs[1:], ",")
		}
		switch r.Method {
		case http.MethodGet:
			s.getAliasesGlobal(w, r, aliasSpec, false)
		case http.MethodHead:
			if aliasSpec == "" {
				w.WriteHeader(200)
				return
			}
			s.headAliasesGlobal(w, r, aliasSpec)
		default:
			methodNotAllowed(w, r, "GET", "HEAD")
		}
		return
	}
	if segs[0] == "_aliases" {
		aliasSpec := ""
		if len(segs) > 1 {
			aliasSpec = strings.Join(segs[1:], ",")
		}
		switch r.Method {
		case http.MethodPost:
			s.postAliases(w, r)
		case http.MethodGet:
			s.getAliasesGlobal(w, r, aliasSpec, true)
		case http.MethodHead:
			if aliasSpec == "" {
				w.WriteHeader(200)
				return
			}
			s.headAliasesGlobal(w, r, aliasSpec)
		default:
			methodNotAllowed(w, r, "GET", "POST", "HEAD")
		}
		return
	}
	if segs[0] == "_cat" {
		s.handleCat(w, r, segs[1:])
		return
	}
	if segs[0] == "_all" || segs[0] == "*" {
		if len(segs) == 2 {
			switch segs[1] {
			case "_search":
				s.handleSearch(w, r, segs[0])
				return
			case "_count":
				s.handleCount(w, r, segs[0])
				return
			case "_field_caps":
				s.handleFieldCaps(w, r, segs[0])
				return
			}
		}
	}
	if strings.HasPrefix(segs[0], "_") {
		if r.Method == http.MethodPut {
			s.createIndex(w, r, segs[0])
			return
		}
		writeError(w, &apiError{
			Type:   "invalid_index_name_exception",
			Reason: "Invalid index name [" + segs[0] + "], must not start with '_'.",
			Status: 400,
		})
		return
	}

	index := segs[0]
	switch len(segs) {
	case 1:
		switch r.Method {
		case http.MethodPut:
			s.createIndex(w, r, index)
		case http.MethodDelete:
			s.deleteIndices(w, r, index)
		case http.MethodGet, http.MethodHead:
			s.getIndexMeta(w, r, index)
		default:
			methodNotAllowed(w, r, "GET", "PUT", "DELETE", "HEAD")
		}
	case 2:
		switch segs[1] {
		case "_mapping", "_mappings":
			switch r.Method {
			case http.MethodGet, http.MethodHead:
				s.getMappingAPI(w, r, index, "")
			case http.MethodPut, http.MethodPost:
				s.putMappingAPI(w, r, index)
			default:
				methodNotAllowed(w, r, "GET", "PUT", "POST", "HEAD")
			}
		case "_settings":
			switch r.Method {
			case http.MethodGet, http.MethodHead:
				s.getSettingsAPI(w, r, index, "")
			case http.MethodPut:
				s.putSettingsAPI(w, r, index)
			default:
				methodNotAllowed(w, r, "GET", "PUT", "HEAD")
			}
		case "_analyze":
			s.handleAnalyze(w, r, index)
		case "_bulk":
			s.handleBulkV2(w, r, index)
		case "_msearch":
			s.handleMsearch(w, r, index)
		case "_mget":
			s.handleMget(w, r, index)
		case "_termvectors":
			if len(segs) == 2 {
				s.handleTermVectors(w, r, index, "")
			} else {
				s.handleTermVectors(w, r, index, segs[2])
			}
		case "_mtermvectors":
			s.handleMTermVectors(w, r, index)
		case "_delete_by_query":
			s.handleDeleteByQuery(w, r, index)
		case "_update_by_query":
			s.handleUpdateByQuery(w, r, index)
		case "_search":
			if len(segs) >= 3 && segs[2] == "point_in_time" {
				s.openPit(w, r, index)
				return
			}
			s.handleSearch(w, r, index)
		case "_count":
			s.handleCount(w, r, index)
		case "_search_shards":
			s.handleSearchShards(w, r, index)
		case "_rank_eval":
			s.handleRankEval(w, r, index)
		case "_validate":
			if len(segs) >= 3 && segs[2] == "query" {
				s.handleValidateQuery(w, r, index)
				return
			}
			writeNoHandler(w, r)
		case "_field_caps":
			s.handleFieldCaps(w, r, index)
		case "_refresh", "_flush", "_forcemerge":
			if r.Method != http.MethodPost && r.Method != http.MethodGet {
				methodNotAllowed(w, r, "POST", "GET")
				return
			}
			s.maintenanceOp(w, r, index, strings.TrimPrefix(segs[1], "_"))
		case "_cache":
			if len(segs) == 2 {
				s.maintenanceOp(w, r, index, "cache")
				return
			}
			writeNoHandler(w, r)
		case "_stats":
			if r.Method != http.MethodGet && r.Method != http.MethodHead {
				methodNotAllowed(w, r, "GET", "HEAD")
				return
			}
			s.handleStatsAPI(w, r, index, "")
		case "_segments":
			s.handleSegmentsAPI(w, r, index)
		case "_recovery":
			s.handleRecoveryAPI(w, r, index)
		case "_rollover":
			s.handleRollover(w, r, index, "")
		case "_close":
			s.handleCloseOpen(w, r, index, true)
		case "_open":
			s.handleCloseOpen(w, r, index, false)
		case "_alias", "_aliases":
			switch r.Method {
			case http.MethodGet, http.MethodHead:
				s.getAliasesIndexed(w, r, index, "")
			case http.MethodDelete:
				s.deleteAliasAPI(w, r, index, "")
			default:
				methodNotAllowed(w, r, "GET", "HEAD", "DELETE")
			}
		case "_doc":
			if r.Method == http.MethodPost {
				s.handleIndexDocV2(w, r, index, "", "index")
			} else {
				methodNotAllowed(w, r, "POST")
			}
		default:
			writeNoHandler(w, r)
		}
	case 3:
		switch segs[1] {
		case "_validate":
			if segs[2] == "query" {
				s.handleValidateQuery(w, r, index)
				return
			}
			writeNoHandler(w, r)
		case "_msearch":
			if segs[2] == "template" {
				s.handleMsearchTemplate(w, r, index)
				return
			}
			writeNoHandler(w, r)
		case "_search":
			if segs[2] == "template" {
				s.handleSearchTemplate(w, r, index)
				return
			}
			if segs[2] == "point_in_time" {
				s.openPit(w, r, index)
				return
			}
			writeNoHandler(w, r)
		case "_doc":
			switch r.Method {
			case http.MethodPut, http.MethodPost:
				s.handleIndexDocV2(w, r, index, segs[2], "index")
			case http.MethodGet, http.MethodHead:
				s.handleGetDocV2(w, r, index, segs[2])
			case http.MethodDelete:
				s.handleDeleteDocV2(w, r, index, segs[2])
			}
		case "_source":
			if r.Method != http.MethodGet && r.Method != http.MethodHead {
				methodNotAllowed(w, r, "GET", "HEAD")
				return
			}
			s.handleGetSourceV2(w, r, index, segs[2])
		case "_termvectors":
			s.handleTermVectors(w, r, index, segs[2])
		case "_update":
			s.handleUpdateDocV2(w, r, index, segs[2])
		case "_explain":
			s.handleExplain(w, r, index, segs[2])
		case "_rollover":
			s.handleRollover(w, r, index, segs[2])
		case "_clone", "_shrink", "_split":
			s.handleResize(w, r, index, strings.TrimPrefix(segs[1], "_"), segs[2])
		case "_create":
			s.handleIndexDocV2(w, r, index, segs[2], "create")
		case "_alias", "_aliases":
			switch r.Method {
			case http.MethodGet, http.MethodHead:
				s.getAliasesIndexed(w, r, index, segs[2])
			case http.MethodPut, http.MethodPost:
				s.putAliasAPI(w, r, index, segs[2])
			case http.MethodDelete:
				s.deleteAliasAPI(w, r, index, segs[2])
			default:
				methodNotAllowed(w, r, "GET", "PUT", "POST", "DELETE", "HEAD")
			}
		case "_settings":
			if r.Method != http.MethodGet && r.Method != http.MethodHead {
				methodNotAllowed(w, r, "GET", "HEAD")
				return
			}
			s.getSettingsAPI(w, r, index, segs[2])
		case "_stats":
			if r.Method != http.MethodGet && r.Method != http.MethodHead {
				methodNotAllowed(w, r, "GET", "HEAD")
				return
			}
			s.handleStatsAPI(w, r, index, segs[2])
		case "_cache":
			if segs[2] == "clear" {
				s.maintenanceOp(w, r, index, "cache")
				return
			}
			writeNoHandler(w, r)
		case "_block":
			if r.Method != http.MethodPut {
				methodNotAllowed(w, r, "PUT")
				return
			}
			s.putBlockAPI(w, r, index, segs[2])
		default:
			writeNoHandler(w, r)
		}
	default:
		if len(segs) == 4 && segs[1] == "_mapping" && segs[2] == "field" {
			s.getMappingAPI(w, r, index, segs[3])
			return
		}
		writeNoHandler(w, r)
	}
}

func rootVersion() map[string]any {
	return map[string]any{
		"distribution":                        "opensearch",
		"number":                              nodeVersion,
		"build_type":                          "tar",
		"build_hash":                          "0000000000000000000000000000000000000000",
		"build_date":                          "2026-01-01T00:00:00.000000000Z",
		"build_snapshot":                      false,
		"lucene_version":                      luceneVersion,
		"minimum_wire_compatibility_version":  "2.19.0",
		"minimum_index_compatibility_version": "2.0.0",
	}
}

func (s *Server) handleRoot(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	switch r.Method {
	case http.MethodGet, http.MethodHead:
	case http.MethodDelete:
		writeError(w, &apiError{Type: "action_request_validation_exception",
			Reason: "Validation Failed: 1: index / indices is missing;", Status: 400})
		return
	case http.MethodOptions:
		w.Header().Set("Allow", "DELETE,HEAD,GET")
		w.WriteHeader(200)
		return
	default:
		methodNotAllowed(w, r, "DELETE,HEAD,GET")
		return
	}
	body := map[string]any{
		"name":         nodeName,
		"cluster_name": clusterName,
		"cluster_uuid": clusterUUID,
		"version":      rootVersion(),
		"tagline":      "The OpenSearch Project: https://opensearch.org/",
	}
	if q.Get("format") == "yaml" {
		if fp := q.Get("filter_path"); fp != "" {
			body = filterPathValue(body, fp).(map[string]any)
		}
		w.Header().Set("Content-Type", "application/yaml")
		w.WriteHeader(200)
		w.Write(toYAML(body))
		return
	}
	pretty := q.Get("pretty") == "" || (q.Get("pretty") != "false")
	var buf bytes.Buffer
	renderOrderedJSON(&buf, []any{
		"name", body["name"],
		"cluster_name", body["cluster_name"],
		"cluster_uuid", body["cluster_uuid"],
		"version", body["version"],
		"tagline", body["tagline"],
	}, pretty)
	cw := w.(*captureWriter)
	cw.ordered = true
	w.Header().Set("Content-Type", "application/json; charset=UTF-8")
	w.WriteHeader(200)
	w.Write(buf.Bytes())
}

// renderOrderedJSON renders an ordered list of key/value pairs. Nested maps are
// rendered with sorted keys, matching OpenSearch's Jackson output for root.
func renderOrderedJSON(buf *bytes.Buffer, pairs []any, pretty bool) {
	if !pretty {
		buf.WriteByte('{')
		for i := 0; i+1 < len(pairs); i += 2 {
			if i > 0 {
				buf.WriteByte(',')
			}
			buf.WriteString(encodeString(pairs[i].(string)))
			buf.WriteByte(':')
			writeCompact(buf, pairs[i+1])
		}
		buf.WriteByte('}')
		return
	}
	buf.WriteString("{\n")
	for i := 0; i+1 < len(pairs); i += 2 {
		if i > 0 {
			buf.WriteString(",\n")
		}
		buf.WriteString("  ")
		buf.WriteString(encodeString(pairs[i].(string)))
		buf.WriteString(" : ")
		writePretty(buf, pairs[i+1], 1)
	}
	buf.WriteString("\n}\n")
}

func writeCompact(buf *bytes.Buffer, v any) {
	switch t := v.(type) {
	case map[string]any:
		buf.WriteByte('{')
		keys := sortedKeys(t)
		for i, k := range keys {
			if i > 0 {
				buf.WriteByte(',')
			}
			buf.WriteString(encodeString(k))
			buf.WriteByte(':')
			writeCompact(buf, t[k])
		}
		buf.WriteByte('}')
	case []any:
		buf.WriteByte('[')
		for i, e := range t {
			if i > 0 {
				buf.WriteByte(',')
			}
			writeCompact(buf, e)
		}
		buf.WriteByte(']')
	default:
		buf.WriteString(string(jsonMustMarshal(v)))
	}
}

func toYAML(v any) []byte {
	var buf bytes.Buffer
	buf.WriteString("---\n")
	writeYAML(&buf, v, 0)
	return buf.Bytes()
}

func writeYAML(buf *bytes.Buffer, v any, indent int) {
	switch t := v.(type) {
	case map[string]any:
		for _, k := range sortedKeys(t) {
			for i := 0; i < indent; i++ {
				buf.WriteString("  ")
			}
			buf.WriteString(k)
			buf.WriteByte(':')
			switch child := t[k].(type) {
			case map[string]any:
				if len(child) == 0 {
					buf.WriteString(" {}\n")
				} else {
					buf.WriteByte('\n')
					writeYAML(buf, child, indent+1)
				}
			case []any:
				if len(child) == 0 {
					buf.WriteString(" []\n")
				} else {
					buf.WriteByte('\n')
					writeYAML(buf, child, indent)
				}
			default:
				buf.WriteByte(' ')
				writeYAMLScalar(buf, child)
				buf.WriteByte('\n')
			}
		}
	case []any:
		for _, e := range t {
			for i := 0; i < indent; i++ {
				buf.WriteString("  ")
			}
			buf.WriteString("- ")
			if m, ok := e.(map[string]any); ok {
				keys := sortedKeys(m)
				for i, k := range keys {
					if i > 0 {
						for j := 0; j < indent+1; j++ {
							buf.WriteString("  ")
						}
					}
					buf.WriteString(k)
					buf.WriteString(": ")
					writeYAMLScalar(buf, m[k])
					buf.WriteByte('\n')
				}
			} else {
				writeYAMLScalar(buf, e)
				buf.WriteByte('\n')
			}
		}
	default:
		writeYAMLScalar(buf, v)
		buf.WriteByte('\n')
	}
}

func writeYAMLScalar(buf *bytes.Buffer, v any) {
	switch t := v.(type) {
	case map[string]any, []any:
		b := string(jsonMustMarshal(t))
		buf.WriteString(b)
	case string:
		buf.WriteString(encodeString(t))
	case json.Number:
		buf.WriteString(t.String())
	case float64:
		buf.WriteString(formatNumber(t))
	case bool:
		if t {
			buf.WriteString("true")
		} else {
			buf.WriteString("false")
		}
	case nil:
		buf.WriteString("null")
	default:
		buf.WriteString(string(jsonMustMarshal(v)))
	}
}

func (s *Server) handleCreateIndex(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.indices[name]; exists {
		writeError(w, &apiError{
			Type:   "resource_already_exists_exception",
			Reason: "index [" + name + "]", Status: 400,
		})
		return
	}
	mappings := map[string]any{}
	if m, ok := body["mappings"].(map[string]any); ok {
		mappings = m
	}
	if err := validateMapping(mappings); err != nil {
		writeError(w, &apiError{Type: "mapper_parsing_exception", Reason: err.Error(), Status: 400})
		return
	}
	settings := map[string]any{}
	if st, ok := body["settings"].(map[string]any); ok {
		settings = st
	}
	idx := &Index{
		Name:         name,
		UUID:         randomUUID(),
		CreationDate: nowMillis(),
		Mappings:     mappings,
		Settings:     settings,
		Docs:         map[string]map[string]any{},
		FieldTypes:   map[string]string{},
		Aliases:      map[string]map[string]any{},
	}
	buildFieldTypes(mappings, "", idx.FieldTypes)
	s.indices[name] = idx
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true, "shards_acknowledged": true, "index": name})
}

func randomUUID() string {
	b := make([]byte, 16)
	for i := range b {
		b[i] = byte(simpleRand())
	}
	const chars = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"
	out := make([]byte, 22)
	for i := 0; i < 22; i++ {
		out[i] = chars[int(b[i%16])%len(chars)]
	}
	return string(out)
}

func (s *Server) handleDeleteIndex(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, ok := s.indices[name]; !ok {
		writeError(w, errNotFound(name))
		return
	}
	delete(s.indices, name)
	s.bumpState()
	writeJSON(w, 200, map[string]any{"acknowledged": true})
}

func (s *Server) handleGetIndex(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, ok := s.indices[name]
	if !ok {
		writeError(w, errNotFound(name))
		return
	}
	aliases := map[string]any{}
	for alias, def := range idx.Aliases {
		aliases[alias] = def
	}
	writeJSON(w, 200, map[string]any{
		name: map[string]any{
			"aliases":  aliases,
			"mappings": idx.Mappings,
			"settings": normalizeSettings(idx.Settings),
		},
	})
}

func (s *Server) handleGetMapping(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.getIndex(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	writeJSON(w, 200, map[string]any{name: map[string]any{"mappings": idx.Mappings}})
}

func (s *Server) handleGetSettings(w http.ResponseWriter, r *http.Request, name string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.getIndex(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	writeJSON(w, 200, map[string]any{name: map[string]any{"settings": normalizeSettings(idx.Settings)}})
}

func writeBlockError(idx *Index) *apiError {
	if err := indexBlockError(idx, "write", 8, 403); err != nil {
		return err
	}
	if err := indexBlockError(idx, "read_only", 5, 403); err != nil {
		return err
	}
	if idx.blockOn("read_only_allow_delete") {
		return &apiError{Type: "cluster_block_exception", Status: 429,
			Reason: fmt.Sprintf("index [%s] blocked by: [TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark, index has read-only-allow-delete block];", idx.Name)}
	}
	return nil
}

func readBlockError(idx *Index) *apiError {
	return indexBlockError(idx, "read", 7, 403)
}

func queryBool(r *http.Request, key string) bool {
	v := r.URL.Query()[key]
	if len(v) == 0 {
		return false
	}
	return v[0] == "" || v[0] == "true"
}

func (s *Server) handleIndexDoc(w http.ResponseWriter, r *http.Request, name, id, opType string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	idx, apiErr := s.resolveWriteTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	if apiErr := writeBlockError(idx); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.DataStream != "" {
		if opType != "create" && id != "" {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "only write ops with an op_type of create are allowed in data streams"})
			return
		}
		if derr := s.validateDataStreamDoc(idx, s.resolveDataStream(idx.DataStream), body); derr != nil {
			writeError(w, derr)
			return
		}
	}
	params := map[string]any{}
	if p := r.URL.Query().Get("pipeline"); p != "" {
		params["pipeline"] = p
	}
	body, apiErr = s.applyPipelines(idx, body, params)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	props := ensureProperties(idx.Mappings)
	if derr := checkDynamicStrict(idx, props, body); derr != nil {
		writeError(w, derr)
		return
	}
	mergeMappingForIndex(idx, props, body)
	idx.FieldTypes = map[string]string{}
	buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
	if id == "" {
		id = s.nextID()
	}
	if derr := idx.computeDocValues(id, body); derr != nil {
		writeError(w, derr)
		return
	}
	_, existing := idx.Docs[id]
	if !existing {
		idx.Order = append(idx.Order, id)
	}
	idx.Docs[id] = body
	idx.IndexTotal++
	routing := r.URL.Query().Get("routing")
	if routing == "" {
		routing = aliasRoutingFor(idx, name)
	}
	if routing != "" {
		if idx.Routing == nil {
			idx.Routing = map[string]string{}
		}
		idx.Routing[id] = routing
	}
	if queryBool(r, "refresh") {
		delete(idx.Pending, id)
	} else if idx.refreshDisabled() {
		if idx.Pending == nil {
			idx.Pending = map[string]map[string]any{}
		}
		idx.Pending[id] = body
	}
	status := 201
	result := "created"
	if existing {
		status = 200
		result = "updated"
	}
	resp := map[string]any{
		"_index": idx.Name, "_id": id, "_version": 1, "result": result,
		"_seq_no": 0, "_primary_term": 1,
	}
	if routing != "" {
		resp["_routing"] = routing
	}
	writeJSON(w, status, resp)
}

func (s *Server) handleGetDoc(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	if apiErr := readBlockError(idx); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	doc, ok := idx.Docs[id]
	if !ok {
		writeJSON(w, 404, map[string]any{"_index": idx.Name, "_id": id, "found": false})
		return
	}
	resp := map[string]any{"_index": idx.Name, "_id": id, "_version": 1, "found": true}
	includes, excludes, omit, _ := sourceFilterFromQuery(r.URL.Query())
	if !omit {
		resp["_source"] = filterSourceDoc(doc, includes, excludes)
	}
	if r2, ok := idx.Routing[id]; ok && r2 != "" {
		resp["_routing"] = r2
	}
	writeJSON(w, 200, resp)
}

func (s *Server) handleGetSource(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	idx, apiErr := s.resolveReadTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	if apiErr := readBlockError(idx); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	doc, ok := idx.Docs[id]
	if !ok {
		writeJSON(w, 404, map[string]any{"_index": idx.Name, "_id": id, "found": false})
		return
	}
	includes, excludes, _, _ := sourceFilterFromQuery(r.URL.Query())
	writeJSON(w, 200, filterSourceDoc(doc, includes, excludes))
}

func (s *Server) handleDeleteDoc(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	idx, apiErr := s.resolveWriteTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	if apiErr := writeBlockError(idx); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if _, ok := idx.Docs[id]; !ok {
		writeJSON(w, 404, map[string]any{"_index": idx.Name, "_id": id, "result": "not_found"})
		return
	}
	delete(idx.Docs, id)
	delete(idx.Pending, id)
	delete(idx.Routing, id)
	idx.removeDocValues(id)
	idx.DeleteTotal++
	writeJSON(w, 200, map[string]any{"_index": idx.Name, "_id": id, "result": "deleted"})
}

func (s *Server) handleUpdateDoc(w http.ResponseWriter, r *http.Request, name, id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	idx, apiErr := s.resolveWriteTarget(name)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if idx.Closed {
		writeError(w, errIndexClosed(idx))
		return
	}
	if apiErr := writeBlockError(idx); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	doc, ok := idx.Docs[id]
	if !ok {
		writeError(w, &apiError{
			Type: "document_missing_exception", Status: 404,
			Reason: "[" + id + "]: document missing",
		})
		return
	}
	if patch, ok := body["doc"].(map[string]any); ok {
		props := ensureProperties(idx.Mappings)
		mergeMappingForIndex(idx, props, patch)
		idx.FieldTypes = map[string]string{}
		buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
		for k, v := range patch {
			doc[k] = v
		}
	}
	if derr := idx.computeDocValues(id, doc); derr != nil {
		writeError(w, derr)
		return
	}
	idx.Docs[id] = doc
	if !queryBool(r, "refresh") && idx.refreshDisabled() {
		if idx.Pending == nil {
			idx.Pending = map[string]map[string]any{}
		}
		idx.Pending[id] = doc
	} else {
		delete(idx.Pending, id)
	}
	writeJSON(w, 200, map[string]any{"_index": idx.Name, "_id": id, "result": "updated"})
}

// resolveReadTarget resolves a name to a concrete index for read operations,
// following aliases when necessary.
func (s *Server) resolveReadTarget(name string) (*Index, *apiError) {
	if idx, ok := s.indices[name]; ok {
		return idx, nil
	}
	var withAlias []*Index
	var writeIdx *Index
	for _, idx := range s.indices {
		if def, ok := idx.Aliases[name]; ok {
			withAlias = append(withAlias, idx)
			if b, _ := def["is_write_index"].(bool); b {
				writeIdx = idx
			}
		}
	}
	if writeIdx != nil {
		return writeIdx, nil
	}
	if len(withAlias) > 0 {
		return withAlias[0], nil
	}
	if ds := s.resolveDataStream(name); ds != nil {
		indices := s.dataStreamIndices(ds)
		if len(indices) > 0 {
			return indices[len(indices)-1], nil
		}
	}
	if verr := validateIndexName(name); verr != nil {
		return nil, verr
	}
	return nil, errIndexNotFoundNamed(name)
}

func parseQParam(q string) map[string]any {
	m, _ := parseQParamOpts(q, "", "")
	return m
}

func parseQParamOpts(q, df, op string) (map[string]any, *apiError) {
	if op != "" && !strings.EqualFold(op, "and") && !strings.EqualFold(op, "or") {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "No enum constant org.opensearch.index.query.Operator." + strings.ToUpper(op)}
	}
	if q == "" {
		return map[string]any{"match_none": map[string]any{}}, nil
	}
	if q == "*" {
		return map[string]any{"match_all": map[string]any{}}, nil
	}
	qs := map[string]any{"query": q}
	if df != "" {
		qs["default_field"] = df
	}
	if op != "" {
		qs["default_operator"] = strings.ToUpper(op)
	}
	return map[string]any{"query_string": qs}, nil
}

func (s *Server) handleCount(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if _, hasSize := body["size"]; hasSize {
		writeError(w, &apiError{Type: "parsing_exception", Status: 400,
			Reason: "request does not support [size]"})
		return
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	o := s.parseResolveParams(r, resolveOpts2{closedError: true})
	indices, apiErr := s.resolveSpec2(name, o)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if pipe, pipeErr := s.resolveSearchPipeline(r, body, indices); pipeErr != nil {
		writeError(w, pipeErr)
		return
	} else if pipe != nil {
		if _, _, perr := applySearchRequestProcessors(body, pipe, false); perr != nil {
			writeError(w, perr)
			return
		}
	}
	query, _ := body["query"].(map[string]any)
	if vals, has := r.URL.Query()["q"]; has {
		if query != nil {
			writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
				Reason: "request [/" + name + "/_count] contains unrecognized parameter: [q]"})
			return
		}
		var qerr *apiError
		query, qerr = parseQParamOpts(vals[0], r.URL.Query().Get("df"), r.URL.Query().Get("default_operator"))
		if qerr != nil {
			writeError(w, qerr)
			return
		}
	}
	matchAll := false
	if query == nil {
		query = map[string]any{"match_all": map[string]any{}}
	}
	if _, ok := query["match_all"]; ok {
		matchAll = true
	}
	total := 0
	shards := 0
	sf := newShardFilter(r, indices)
	for _, idx := range indices {
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
			doc, live := idx.Docs[id]
			if !live {
				continue
			}
			if !sf.allows(idx, id) {
				continue
			}
			if _, pending := idx.Pending[id]; pending {
				continue
			}
			qc := newQueryCtxForDoc(idx, id, doc)
			matched, _, err := qc.eval(q)
			if err != nil {
				if err.Type == "illegal_argument_exception" && strings.Contains(err.Reason, "failed to parse value [") {
					val := err.Reason
					if i := strings.Index(val, "["); i >= 0 {
						if j := strings.Index(val[i+1:], "]"); j >= 0 {
							val = val[i+1 : i+1+j]
						}
					}
					reason := "failed to create query: For input string: \"" + val + "\""
					writeError(w, &apiError{Type: "search_phase_execution_exception", Status: 400, Reason: reason,
						RootCause: &apiError{Type: "query_shard_exception", Reason: reason}})
					return
				}
				writeError(w, err)
				return
			}
			if matched {
				total++
			}
		}
	}
	resp := map[string]any{
		"count": total,
		"_shards": map[string]any{
			"total": shards, "successful": shards, "skipped": 0, "failed": 0,
		},
	}
	if ta := r.URL.Query().Get("terminate_after"); ta != "" {
		if n, err := strconv.Atoi(ta); err == nil && n > 0 {
			terminated := matchAll
			if !terminated && total > n {
				resp["count"] = n
				terminated = true
			}
			resp["terminated_early"] = terminated
		}
	}
	writeJSON(w, 200, resp)
}

func (s *Server) handleBulk(w http.ResponseWriter, r *http.Request, name string) {
	raw, err := io.ReadAll(r.Body)
	if err != nil {
		writeError(w, errBad("failed to read bulk body"))
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	scanner := bufio.NewScanner(strings.NewReader(string(raw)))
	scanner.Buffer(make([]byte, 1024*1024), 16*1024*1024)
	items := []any{}
	hasErrors := false

	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		var action map[string]json.RawMessage
		if err := json.Unmarshal([]byte(line), &action); err != nil {
			hasErrors = true
			items = append(items, bulkErr("index", name, "", 400, "illegal_argument_exception", "malformed action"))
			continue
		}
		var op string
		var meta map[string]any
		for k, v := range action {
			op = k
			unmarshalJSON(v, &meta)
		}
		idxName := name
		if meta != nil {
			if mi, ok := meta["_index"].(string); ok && mi != "" {
				idxName = mi
			}
		}
		var docID string
		if meta != nil {
			if mi, ok := meta["_id"].(string); ok {
				docID = mi
			}
		}
		if op == "delete" {
			idx, apiErr := s.resolveWriteTarget(idxName)
			if apiErr != nil {
				hasErrors = true
				items = append(items, bulkErrAPI("delete", idxName, docID, apiErr))
				continue
			}
			if idx.Closed {
				hasErrors = true
				items = append(items, bulkErrAPI("delete", idxName, docID, errIndexClosed(idx)))
				continue
			}
			if berr := writeBlockError(idx); berr != nil {
				hasErrors = true
				items = append(items, bulkErrAPI("delete", idxName, docID, berr))
				continue
			}
			delete(idx.Docs, docID)
			delete(idx.Pending, docID)
			delete(idx.Routing, docID)
			idx.removeDocValues(docID)
			idx.DeleteTotal++
			items = append(items, map[string]any{"delete": map[string]any{"_index": idx.Name, "_id": docID, "status": 200}})
			continue
		}

		var docLine string
		if scanner.Scan() {
			docLine = strings.TrimSpace(scanner.Text())
		}
		var source map[string]any
		if docLine != "" {
			if err := unmarshalJSON([]byte(docLine), &source); err != nil {
				hasErrors = true
				items = append(items, bulkErr(op, idxName, docID, 400, "illegal_argument_exception", "malformed document"))
				continue
			}
		}
		if op == "update" {
			var wrapper map[string]any
			source = nil
			unmarshalJSON([]byte(docLine), &wrapper)
			if d, ok := wrapper["doc"].(map[string]any); ok {
				source = d
			}
			idx, apiErr := s.resolveWriteTarget(idxName)
			if apiErr != nil {
				hasErrors = true
				items = append(items, bulkErrAPI("update", idxName, docID, apiErr))
				continue
			}
			if idx.Closed {
				hasErrors = true
				items = append(items, bulkErrAPI("update", idxName, docID, errIndexClosed(idx)))
				continue
			}
			if berr := writeBlockError(idx); berr != nil {
				hasErrors = true
				items = append(items, bulkErrAPI("update", idxName, docID, berr))
				continue
			}
			doc, ok := idx.Docs[docID]
			if !ok {
				hasErrors = true
				items = append(items, bulkErr("update", idxName, docID, 404, "document_missing_exception", "document missing"))
				continue
			}
			props := ensureProperties(idx.Mappings)
			mergeMappingForIndex(idx, props, source)
			idx.FieldTypes = map[string]string{}
			buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
			for k, v := range source {
				doc[k] = v
			}
			if derr := idx.computeDocValues(docID, doc); derr != nil {
				hasErrors = true
				items = append(items, bulkErrAPI("update", idx.Name, docID, derr))
				continue
			}
			idx.Docs[docID] = doc
			items = append(items, map[string]any{"update": map[string]any{"_index": idx.Name, "_id": docID, "status": 200}})
			continue
		}

		idx, apiErr := s.resolveWriteTarget(idxName)
		if apiErr != nil {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idxName, docID, apiErr))
			continue
		}
		if idx.Closed {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idxName, docID, errIndexClosed(idx)))
			continue
		}
		if berr := writeBlockError(idx); berr != nil {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idxName, docID, berr))
			continue
		}
		bulkParams := map[string]any{}
		if p, ok := meta["pipeline"].(string); ok && p != "" {
			bulkParams["pipeline"] = p
		}
		source, apiErr = s.applyPipelines(idx, source, bulkParams)
		if apiErr != nil {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idx.Name, docID, apiErr))
			continue
		}
		props := ensureProperties(idx.Mappings)
		if derr := checkDynamicStrict(idx, props, source); derr != nil {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idx.Name, docID, derr))
			continue
		}
		mergeMappingForIndex(idx, props, source)
		idx.FieldTypes = map[string]string{}
		buildFieldTypes(idx.Mappings, "", idx.FieldTypes)
		if docID == "" {
			docID = s.nextID()
		}
		if derr := idx.computeDocValues(docID, source); derr != nil {
			hasErrors = true
			items = append(items, bulkErrAPI(op, idx.Name, docID, derr))
			continue
		}
		_, exists := idx.Docs[docID]
		if op == "create" && exists {
			hasErrors = true
			items = append(items, bulkErr("create", idxName, docID, 409, "version_conflict_engine_exception", "document already exists"))
			continue
		}
		if !exists {
			idx.Order = append(idx.Order, docID)
		}
		idx.Docs[docID] = source
		idx.IndexTotal++
		routing := ""
		if meta != nil {
			if mr, ok := meta["routing"].(string); ok {
				routing = mr
			}
		}
		if routing == "" {
			routing = aliasRoutingFor(idx, idxName)
		}
		if routing != "" {
			if idx.Routing == nil {
				idx.Routing = map[string]string{}
			}
			idx.Routing[docID] = routing
		}
		if !queryBool(r, "refresh") && idx.refreshDisabled() {
			if idx.Pending == nil {
				idx.Pending = map[string]map[string]any{}
			}
			idx.Pending[docID] = source
		} else {
			delete(idx.Pending, docID)
		}
		status := 201
		if op == "index" && exists {
			status = 200
		}
		items = append(items, map[string]any{op: map[string]any{"_index": idx.Name, "_id": docID, "status": status}})
	}
	if queryBool(r, "refresh") {
		for _, idx := range s.indices {
			idx.Pending = nil
		}
	}
	writeJSON(w, 200, map[string]any{"took": 1, "errors": hasErrors, "items": items})
}

func bulkErrAPI(op, index, id string, e *apiError) map[string]any {
	return map[string]any{op: map[string]any{
		"_index": index, "_id": id, "status": e.Status,
		"error": map[string]any{"type": e.Type, "reason": e.Reason},
	}}
}

// checkDynamicStrict enforces a mapping-level "dynamic":"strict" setting.
func checkDynamicStrict(idx *Index, props map[string]any, source map[string]any) *apiError {
	dyn, ok := idx.Mappings["dynamic"].(string)
	if !ok || dyn != "strict" {
		return nil
	}
	for k := range source {
		if _, known := props[k]; !known {
			return &apiError{Type: "strict_dynamic_mapping_exception", Status: 400,
				Reason: "dynamic introduction of [" + k + "] within [_doc] is not allowed"}
		}
	}
	return nil
}

func bulkErr(op, index, id string, status int, typ, reason string) map[string]any {
	return map[string]any{op: map[string]any{
		"_index": index, "_id": id, "status": status,
		"error": map[string]any{"type": typ, "reason": reason},
	}}
}

func (s *Server) nextID() string {
	s.counter++
	return fmt.Sprintf("doc-%d", s.counter)
}

// maxContentLength mirrors OpenSearch's default http.max_content_length (100mb).
const maxContentLength = 100 << 20

func main() {
	host := os.Getenv("HOST")
	if host == "" {
		host = "127.0.0.1"
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "9200"
	}
	addr := net.JoinHostPort(host, port)
	srv := &http.Server{
		Addr:              addr,
		Handler:           NewServer(),
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       2 * time.Minute,
	}
	fmt.Println("slopsearch (go) listening on " + addr)
	if err := srv.ListenAndServe(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func writeNoHandler(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, 400, map[string]any{"error": "no handler found for uri [" + r.URL.Path + "] and method [" + r.Method + "]"})
}
