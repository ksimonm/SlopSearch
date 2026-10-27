package main

import (
	"bytes"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"strings"
)

func templateLookup(params map[string]any, name string) (any, bool) {
	cur := any(params)
	for _, part := range strings.Split(name, ".") {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil, false
		}
		cur, ok = m[part]
		if !ok {
			return nil, false
		}
	}
	return cur, true
}

func templateTruthy(v any) bool {
	switch t := v.(type) {
	case nil:
		return false
	case bool:
		return t
	case string:
		return t != ""
	case []any:
		return len(t) > 0
	case map[string]any:
		return len(t) > 0
	case json.Number:
		f, _ := t.Float64()
		return f != 0
	case float64:
		return t != 0
	}
	return true
}

func templateString(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case json.Number:
		return t.String()
	case float64:
		if t == float64(int64(t)) {
			return strconv.FormatInt(int64(t), 10)
		}
		return strconv.FormatFloat(t, 'f', -1, 64)
	case bool:
		if t {
			return "true"
		}
		return "false"
	}
	b, _ := json.Marshal(v)
	return string(b)
}

func templateEscape(s string) string {
	b, _ := json.Marshal(s)
	if len(b) >= 2 {
		return string(b[1 : len(b)-1])
	}
	return s
}

func findSectionEnd(tpl, name string, start int) int {
	depth := 1
	for i := start; i < len(tpl); {
		j := strings.Index(tpl[i:], "{{")
		if j < 0 {
			return -1
		}
		j += i
		k := strings.Index(tpl[j:], "}}")
		if k < 0 {
			return -1
		}
		tag := strings.TrimSpace(tpl[j+2 : j+k])
		if tag == "#"+name || tag == "^"+name {
			depth++
		} else if tag == "/"+name {
			depth--
			if depth == 0 {
				return j
			}
		}
		i = j + k + 2
	}
	return -1
}

func renderMustache(tpl string, params map[string]any, jsonMode bool) (string, error) {
	var b strings.Builder
	inString := false
	for i := 0; i < len(tpl); {
		if strings.HasPrefix(tpl[i:], "{{") {
			j := strings.Index(tpl[i:], "}}")
			if j < 0 {
				return "", errParsing("unclosed mustache tag")
			}
			tag := strings.TrimSpace(tpl[i+2 : i+j])
			after := i + j + 2
			if strings.HasPrefix(tag, "#") || strings.HasPrefix(tag, "^") {
				inv := strings.HasPrefix(tag, "^")
				name := strings.TrimSpace(tag[1:])
				if !inv && (name == "toJson" || name == "join" || name == "url") {
					end := findSectionEnd(tpl, name, after)
					if end < 0 {
						return "", errParsing("unclosed function [" + name + "]")
					}
					inner := strings.TrimSpace(tpl[after:end])
					v, _ := templateLookup(params, inner)
					var out string
					switch name {
					case "toJson":
						b, _ := json.Marshal(v)
						out = string(b)
					case "join":
						items := []string{}
						if list, ok := v.([]any); ok {
							for _, e := range list {
								items = append(items, templateString(e))
							}
						}
						out = strings.Join(items, ",")
					case "url":
						rendered, rerr := renderMustache(inner, params, false)
						if rerr != nil {
							return "", rerr
						}
						out = url.QueryEscape(rendered)
					}
					if jsonMode && inString {
						out = templateEscape(out)
					}
					b.WriteString(out)
					i = end + len("{{/"+name+"}}")
					continue
				}
				end := findSectionEnd(tpl, name, after)
				if end < 0 {
					return "", errParsing("unclosed section [" + name + "]")
				}
				inner := tpl[after:end]
				v, ok := templateLookup(params, name)
				include := ok && templateTruthy(v)
				if inv {
					include = !include
				}
				if include {
					rendered, err := renderMustache(inner, params, jsonMode)
					if err != nil {
						return "", err
					}
					b.WriteString(rendered)
				}
				closeLen := len("{{/" + name + "}}")
				i = end + closeLen
				continue
			}
			v, ok := templateLookup(params, tag)
			if !ok {
				v = ""
			}
			out := templateString(v)
			if jsonMode && inString {
				out = templateEscape(out)
			}
			b.WriteString(out)
			i = after
			continue
		}
		ch := tpl[i]
		if jsonMode && ch == '"' && (i == 0 || tpl[i-1] != '\\') {
			inString = !inString
		}
		b.WriteByte(ch)
		i++
	}
	return b.String(), nil
}

func renderMustacheValue(v any, params map[string]any) (any, error) {
	switch t := v.(type) {
	case string:
		return renderMustache(t, params, false)
	case map[string]any:
		out := map[string]any{}
		for k, e := range t {
			r, err := renderMustacheValue(e, params)
			if err != nil {
				return nil, err
			}
			out[k] = r
		}
		return out, nil
	case []any:
		out := make([]any, 0, len(t))
		for _, e := range t {
			r, err := renderMustacheValue(e, params)
			if err != nil {
				return nil, err
			}
			out = append(out, r)
		}
		return out, nil
	}
	return v, nil
}

func renderTemplateSource(source any, params map[string]any) (map[string]any, *apiError) {
	var rendered any
	var err error
	switch t := source.(type) {
	case map[string]any:
		rendered, err = renderMustacheValue(t, params)
	case string:
		var text string
		text, err = renderMustache(t, params, true)
		if err == nil {
			var m map[string]any
			if uerr := json.Unmarshal([]byte(text), &m); uerr != nil {
				err = uerr
			} else {
				rendered = m
			}
		}
	default:
		err = errParsing("template source must be a string or object")
	}
	if err != nil {
		reason := "Failed to parse template"
		if ae, ok := err.(*apiError); ok {
			reason = ae.Reason
		}
		return nil, &apiError{Type: "script_exception", Status: 400, Reason: reason,
			Fields:    map[string]any{"lang": "mustache"},
			CausedBy:  &apiError{Type: "mustache_exception", Reason: reason},
			RootCause: &apiError{Type: "script_exception", Reason: reason, Fields: map[string]any{"lang": "mustache"}}}
	}
	m, ok := rendered.(map[string]any)
	if !ok {
		return nil, &apiError{Type: "script_exception", Status: 400, Reason: "template output is not an object",
			Fields: map[string]any{"lang": "mustache"}}
	}
	return m, nil
}

func (s *Server) templateSourceFor(id string) (any, *apiError) {
	s.scriptsMu.RLock()
	sc, ok := s.scripts[id]
	s.scriptsMu.RUnlock()
	if !ok {
		return nil, &apiError{Type: "resource_not_found_exception", Status: 404,
			Reason: "unable to find script [" + id + "] in cluster state"}
	}
	return sc["source"], nil
}

func (s *Server) renderTemplateBody(body map[string]any) (map[string]any, *apiError) {
	source := body["source"]
	if id, _ := body["id"].(string); id != "" {
		src, apiErr := s.templateSourceFor(id)
		if apiErr != nil {
			return nil, apiErr
		}
		source = src
	}
	params, _ := body["params"].(map[string]any)
	if params == nil {
		params = map[string]any{}
	}
	out, apiErr := renderTemplateSource(source, params)
	if apiErr != nil {
		return nil, apiErr
	}
	for k, v := range body {
		if k == "source" || k == "id" || k == "params" {
			continue
		}
		out[k] = v
	}
	return out, nil
}

func (s *Server) executeSearchRaw(r *http.Request, name string, searchBody map[string]any) (int, []byte) {
	bodyBytes, _ := json.Marshal(searchBody)
	target := "/" + name + "/_search"
	if name == "" {
		target = "/_search"
	}
	req := httptest.NewRequest(http.MethodPost, target+"?"+r.URL.RawQuery, bytes.NewReader(bodyBytes))
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()
	s.handleSearch(rec, req, name)
	return rec.Code, rec.Body.Bytes()
}

func (s *Server) executeTemplateSearch(r *http.Request, name string, tplBody map[string]any) (int, []byte, *apiError) {
	searchBody, apiErr := s.renderTemplateBody(tplBody)
	if apiErr != nil {
		return 0, nil, apiErr
	}
	code, out := s.executeSearchRaw(r, name, searchBody)
	return code, out, nil
}

func (s *Server) handleSearchTemplate(w http.ResponseWriter, r *http.Request, name string) {
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	code, out, apiErr := s.executeTemplateSearch(r, name, body)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_, _ = w.Write(out)
}

func (s *Server) handleRenderTemplate(w http.ResponseWriter, r *http.Request, id string) {
	if r.Method != http.MethodPost && r.Method != http.MethodGet {
		methodNotAllowed(w, r, "POST", "GET")
		return
	}
	body, apiErr := readJSON(r)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if id != "" {
		body["id"] = id
	}
	out, apiErr := s.renderTemplateBody(body)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	writeJSON(w, 200, map[string]any{"template_output": out})
}

func parseMsearchPairs(raw []byte) ([][2]map[string]any, *apiError) {
	if len(raw) == 0 || raw[len(raw)-1] != '\n' {
		return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "The msearch request must be terminated by a newline [\n]"}
	}
	lines := strings.Split(string(raw), "\n")
	lines = lines[:len(lines)-1]
	pairs := [][2]map[string]any{}
	for i := 0; i+1 < len(lines); i += 2 {
		var header, body map[string]any
		if err := json.Unmarshal([]byte(lines[i]), &header); err != nil {
			return nil, errParsing("Failed to parse request body")
		}
		if err := json.Unmarshal([]byte(lines[i+1]), &body); err != nil {
			return nil, errParsing("Failed to parse request body")
		}
		for k := range header {
			if !msearchHeaderKeys[k] {
				return nil, &apiError{Type: "illegal_argument_exception", Status: 400,
					Reason: "key [" + k + "] is not supported in the metadata section"}
			}
		}
		pairs = append(pairs, [2]map[string]any{header, body})
	}
	if len(pairs) == 0 {
		return nil, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: no requests added;"}
	}
	return pairs, nil
}

func (s *Server) handleMsearchTemplate(w http.ResponseWriter, r *http.Request, pathIndex string) {
	raw, err := ioReadAll(r)
	if err != nil {
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400, Reason: "failed to read request body"})
		return
	}
	pairs, apiErr := parseMsearchPairs(raw)
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	responses := []any{}
	for _, pr := range pairs {
		idx := pathIndex
		if iv, ok := pr[0]["index"]; ok {
			switch t := iv.(type) {
			case string:
				idx = t
			case []any:
				parts := []string{}
				for _, e := range t {
					if sv, ok := toString(e); ok && sv != "" {
						parts = append(parts, sv)
					}
				}
				idx = strings.Join(parts, ",")
			}
		}
		code, out, apiErr := s.executeTemplateSearch(r, idx, pr[1])
		var item map[string]any
		if apiErr != nil {
			item = errorJSON(apiErr)
			code = apiErr.Status
		} else {
			_ = json.Unmarshal(out, &item)
			if item == nil {
				item = map[string]any{}
			}
		}
		item["status"] = code
		responses = append(responses, item)
	}
	writeJSON(w, 200, map[string]any{"took": 1, "responses": responses})
}

func ioReadAll(r *http.Request) ([]byte, error) {
	var b bytes.Buffer
	_, err := b.ReadFrom(r.Body)
	return b.Bytes(), err
}

func errorJSON(e *apiError) map[string]any {
	status := e.Status
	if status == 0 {
		status = 400
	}
	body := map[string]any{"error": map[string]any{"type": e.Type, "reason": e.Reason}, "status": status}
	if e.CausedBy != nil {
		body["error"].(map[string]any)["caused_by"] = map[string]any{"type": e.CausedBy.Type, "reason": e.CausedBy.Reason}
	}
	if e.Fields != nil {
		body["error"].(map[string]any)["lang"] = e.Fields["lang"]
	}
	return body
}
