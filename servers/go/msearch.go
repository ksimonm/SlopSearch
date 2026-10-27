package main

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"strings"
)

var msearchHeaderKeys = map[string]bool{
	"index": true, "search_type": true, "routing": true, "preference": true,
	"ignore_unavailable": true, "allow_no_indices": true, "expand_wildcards": true,
	"request_cache": true, "ignore_throttled": true, "ccs_minimize_roundtrips": true,
}

func (s *Server) handleMsearch(w http.ResponseWriter, r *http.Request, pathIndex string) {
	raw, err := io.ReadAll(r.Body)
	if err != nil {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to read request body"})
		return
	}
	if len(raw) == 0 || (len(raw) > 0 && raw[len(raw)-1] != '\n') {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "The msearch request must be terminated by a newline [\n]"})
		return
	}
	lines := strings.Split(string(raw), "\n")
	if len(lines) > 0 && lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	type pair struct {
		header map[string]any
		body   map[string]any
	}
	pairs := []pair{}
	if len(lines) > 0 && lines[0] == "" {
		for _, ln := range lines[1:] {
			if strings.TrimSpace(ln) == "" {
				continue
			}
			var m map[string]any
			if err := json.Unmarshal([]byte(ln), &m); err != nil {
				writeError(w, errParsing("Failed to parse request body"))
				return
			}
			for k := range m {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "key [" + k + "] is not supported in the metadata section"})
				return
			}
		}
	}
	for i := 0; i+1 < len(lines); i += 2 {
		var header map[string]any
		if err := json.Unmarshal([]byte(lines[i]), &header); err != nil {
			writeError(w, errParsing("Failed to parse request body"))
			return
		}
		for k := range header {
			if !msearchHeaderKeys[k] {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "key [" + k + "] is not supported in the metadata section"})
				return
			}
		}
		var body map[string]any
		if err := json.Unmarshal([]byte(lines[i+1]), &body); err != nil {
			writeError(w, errParsing("Failed to parse request body"))
			return
		}
		if f, ok := body["from"]; ok {
			if n, ok := toFloat(f); ok && n < 0 {
				writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "[from] parameter cannot be negative, found [" + strconv.FormatInt(int64(n), 10) + "]"})
				return
			}
		}
		pairs = append(pairs, pair{header: header, body: body})
	}
	if len(pairs) == 0 {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: no requests added;"})
		return
	}
	responses := []any{}
	for _, pr := range pairs {
		idx := pathIndex
		if iv, ok := pr.header["index"]; ok {
			switch t := iv.(type) {
			case string:
				idx = t
			case []any:
				parts := []string{}
				for _, e := range t {
					s, _ := toString(e)
					if s != "" {
						parts = append(parts, s)
					}
				}
				idx = strings.Join(parts, ",")
			}
		}
		q := url.Values{}
		for _, key := range []string{"rest_total_hits_as_int", "typed_keys", "routing", "preference", "ignore_unavailable"} {
			if v := r.URL.Query().Get(key); v != "" {
				q.Set(key, v)
			}
		}
		if iv, ok := pr.header["ignore_unavailable"].(bool); ok && iv {
			q.Set("ignore_unavailable", "true")
		}
		for _, key := range []string{"routing", "preference", "search_type"} {
			if v, ok := pr.header[key].(string); ok && v != "" {
				q.Set(key, v)
			}
		}
		bodyBytes, _ := json.Marshal(pr.body)
		target := "/" + idx + "/_search"
		if idx == "" {
			target = "/_search"
		}
		req := httptest.NewRequest(http.MethodPost, target+"?"+q.Encode(), bytes.NewReader(bodyBytes))
		req.Header.Set("Content-Type", "application/json")
		rec := httptest.NewRecorder()
		s.handleSearch(rec, req, idx)
		var item map[string]any
		_ = json.Unmarshal(rec.Body.Bytes(), &item)
		if rec.Code == 400 {
			if em, ok := item["error"].(map[string]any); ok {
				if typ, _ := em["type"].(string); typ == "parsing_exception" || typ == "x_content_parse_exception" {
					writeJSON(w, rec.Code, item)
					return
				}
			}
		}
		if item == nil {
			item = map[string]any{}
		}
		item["status"] = rec.Code
		responses = append(responses, item)
	}
	writeJSON(w, 200, map[string]any{"took": 1, "responses": responses})
}
