package main

import (
	"net/http"
	"strconv"
	"strings"
)

type reindexFailure struct {
	index string
	id    string
	typ   string
	rex   string
	code  int
}

func (f reindexFailure) json(destUUID string) map[string]any {
	return map[string]any{
		"index": f.index,
		"id":    f.id,
		"cause": map[string]any{
			"type":       f.typ,
			"reason":     f.rex,
			"index":      f.index,
			"shard":      "0",
			"index_uuid": destUUID,
		},
		"status": f.code,
	}
}

func (s *Server) handleReindex(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		methodNotAllowed(w, r, "POST")
		return
	}
	body, apiErr2 := readJSON(r)
	if apiErr2 != nil {
		writeError(w, apiErr2)
		return
	}
	for k := range body {
		switch k {
		case "source", "dest", "script", "max_docs", "conflicts", "wait_for_completion",
			"refresh", "timeout", "scroll", "size", "slices":
		default:
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[1:0] [reindex] unknown field [" + k + "]"})
			return
		}
	}
	srcRaw, _ := body["source"].(map[string]any)
	destRaw, _ := body["dest"].(map[string]any)
	if destRaw == nil {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: index must be specified;"})
		return
	}
	destName, _ := destRaw["index"].(string)
	if destName == "" {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: index must be specified;"})
		return
	}
	if srcRaw == nil || srcRaw["index"] == nil {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: use _all if you really want to copy from all existing indexes;"})
		return
	}
	if remote, ok := srcRaw["remote"].(map[string]any); ok {
		host, _ := remote["host"].(string)
		host = strings.TrimPrefix(host, "http://")
		host = strings.TrimPrefix(host, "https://")
		writeError(w, &apiError{Type: "illegal_argument_exception", Status: 400,
			Reason: "[" + host + "] not allowlisted in reindex.remote.allowlist"})
		return
	}
	var srcSpecs []string
	switch t := srcRaw["index"].(type) {
	case string:
		srcSpecs = []string{t}
	case []any:
		for _, v := range t {
			if s, ok := v.(string); ok {
				srcSpecs = append(srcSpecs, s)
			}
		}
	}
	srcSpec := strings.Join(srcSpecs, ",")
	if srcSpec == "" {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: use _all if you really want to copy from all existing indexes;"})
		return
	}
	if srcSpec == destName {
		writeError(w, &apiError{Type: "action_request_validation_exception", Status: 400,
			Reason: "Validation Failed: 1: reindex cannot write into an index its reading from [" + srcSpec + "];"})
		return
	}
	maxDocs := -1
	if v, ok := body["max_docs"]; ok && v != nil {
		n, isInt := settingsValueInt(v)
		if !isInt {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[1:0] [reindex] failed to parse field [max_docs]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "[max_docs] should be >= [slices]"}})
			return
		}
		if n < 0 {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[1:0] [reindex] failed to parse field [max_docs]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "[max_docs] parameter cannot be negative, found [" + strconv.Itoa(n) + "]"}})
			return
		}
		if n < 1 {
			writeError(w, &apiError{Type: "x_content_parse_exception", Status: 400,
				Reason: "[1:0] [reindex] failed to parse field [max_docs]",
				CausedBy: &apiError{Type: "illegal_argument_exception",
					Reason: "[max_docs] should be >= [slices]"}})
			return
		}
		maxDocs = n
	}
	conflictsProceed := false
	if c, ok := body["conflicts"].(string); ok && c == "proceed" {
		conflictsProceed = true
	}
	size := 1000
	if v, ok := srcRaw["size"]; ok {
		if n, isInt := settingsValueInt(v); isInt && n > 0 {
			size = n
		}
	}
	opType, _ := destRaw["op_type"].(string)
	versionType, _ := destRaw["version_type"].(string)
	var scriptSrc string
	if sc, ok := body["script"].(map[string]any); ok {
		scriptSrc, _ = sc["source"].(string)
	}
	var query map[string]any
	if q, ok := srcRaw["query"].(map[string]any); ok {
		query = q
	}
	var sourceInclude []string
	if si, ok := srcRaw["_source"].([]any); ok {
		for _, f := range si {
			if fs, ok := f.(string); ok {
				sourceInclude = append(sourceInclude, fs)
			}
		}
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	allowNo := false
	srcIndices, apiErr := s.resolveSpec2(srcSpec, resolveOpts2{allowNoIndices: &allowNo})
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	type item struct {
		idx *Index
		id  string
		doc map[string]any
	}
	var items []item
	for _, sidx := range srcIndices {
		for _, id := range sidx.Order {
			doc := sidx.Docs[id]
			if query != nil {
				qc := newQueryCtxForDoc(sidx, id, doc)
				ok, _, qerr := qc.eval(query)
				if qerr != nil {
					writeError(w, qerr)
					return
				}
				if !ok {
					continue
				}
			}
			items = append(items, item{idx: sidx, id: id, doc: doc})
		}
	}
	if maxDocs >= 0 && len(items) > maxDocs {
		items = items[:maxDocs]
	}
	total := len(items)
	batches := 0
	if total > 0 {
		batches = (total + size - 1) / size
	}

	created, updated, deleted, noops, versionConflicts := 0, 0, 0, 0, 0
	failures := []any{}
	destIdx := s.indices[destName]
	destUUID := "_na_"
	if destIdx != nil {
		destUUID = destIdx.UUID
	}
	destNameErr := validateIndexName(destName)
	for _, it := range items {
		doc := deepCopyJSON(it.doc).(map[string]any)
		id := it.id
		if scriptSrc != "" {
			op := ""
			runReindexScript(scriptSrc, doc, &id, &op)
			if op == "noop" {
				noops++
				continue
			}
			if op == "delete" {
				deleted++
				continue
			}
		}
		if destIdx == nil {
			if destNameErr != nil {
				failures = append(failures, reindexFailure{index: destName, id: it.id,
					typ: destNameErr.Type, rex: destNameErr.Reason, code: destNameErr.Status}.json("_na_"))
				continue
			}
			newIdx, apiErr3 := s.createIndexInternal(destName, map[string]any{}, false)
			if apiErr3 != nil {
				failures = append(failures, reindexFailure{index: destName, id: it.id,
					typ: apiErr3.Type, rex: apiErr3.Reason, code: apiErr3.Status}.json("_na_"))
				continue
			}
			destIdx = newIdx
			destUUID = destIdx.UUID
		}
		existing := false
		if _, has := destIdx.Docs[id]; has {
			existing = true
		}
		if existing && (opType == "create" || versionType == "external" || versionType == "external_gte") {
			versionConflicts++
			if !conflictsProceed {
				reason := ""
				if opType == "create" {
					reason = "[" + id + "]: version conflict, document already exists (current version [1])"
				} else {
					reason = "[" + id + "]: version conflict, current version [1] is higher or equal to the one provided [1]"
				}
				failures = append(failures, reindexFailure{index: destName, id: it.id,
					typ: "version_conflict_engine_exception", rex: reason, code: 409}.json(destUUID))
			}
			continue
		}
		if sourceInclude != nil {
			filtered := map[string]any{}
			for _, f := range sourceInclude {
				if v, ok := doc[f]; ok {
					filtered[f] = v
				}
			}
			doc = filtered
		}
		if existing {
			updated++
		} else {
			created++
			destIdx.Order = append(destIdx.Order, id)
		}
		props := ensureProperties(destIdx.Mappings)
		mergeMapping(props, doc)
		destIdx.FieldTypes = map[string]string{}
		buildFieldTypes(destIdx.Mappings, "", destIdx.FieldTypes)
		if derr := destIdx.computeDocValues(id, doc); derr != nil {
			if existing {
				updated--
			} else {
				created--
				if len(destIdx.Order) > 0 {
					destIdx.Order = destIdx.Order[:len(destIdx.Order)-1]
				}
			}
			failures = append(failures, reindexFailure{index: destName, id: id,
				typ: derr.Type, rex: derr.Reason, code: derr.Status}.json(destUUID))
			continue
		}
		destIdx.Docs[id] = doc
		destIdx.IndexTotal++
	}
	if destIdx != nil {
		s.bumpState()
	}
	status := 200
	if versionConflicts > 0 && !conflictsProceed {
		status = 409
	} else if len(failures) > 0 {
		status = 400
	}
	writeJSON(w, status, map[string]any{
		"took":                   1,
		"timed_out":              false,
		"total":                  total,
		"updated":                updated,
		"created":                created,
		"deleted":                deleted,
		"batches":                batches,
		"version_conflicts":      versionConflicts,
		"noops":                  noops,
		"retries":                map[string]any{"bulk": 0, "search": 0},
		"throttled_millis":       0,
		"requests_per_second":    -1.0,
		"throttled_until_millis": 0,
		"failures":               failures,
	})
}

// ---------------------------------------------------------------- reindex script

func splitReindexStatements(s string) []string {
	var out []string
	depth := 0
	start := 0
	for i := 0; i < len(s); i++ {
		switch s[i] {
		case '{':
			depth++
		case '}':
			depth--
		case ';':
			if depth == 0 {
				st := strings.TrimSpace(s[start:i])
				if st != "" {
					out = append(out, st)
				}
				start = i + 1
			}
		}
	}
	st := strings.TrimSpace(s[start:])
	if st != "" {
		out = append(out, st)
	}
	return out
}

func matchCloseBrace(s string, open int) int {
	depth := 0
	for i := open; i < len(s); i++ {
		switch s[i] {
		case '{':
			depth++
		case '}':
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return len(s) - 1
}

func matchParen(s string, open int) int {
	depth := 0
	quote := byte(0)
	for i := open; i < len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '\'', '"':
			quote = c
		case '(':
			depth++
		case ')':
			depth--
			if depth == 0 {
				return i
			}
		}
	}
	return -1
}

func reindexSourceGet(doc map[string]any, ref string) (any, bool) {
	ref = strings.TrimSpace(ref)
	ref = strings.TrimPrefix(ref, "ctx._source.")
	ref = strings.TrimPrefix(ref, "ctx._source")
	ref = strings.Trim(ref, "[]'\"")
	if ref == "" {
		return nil, false
	}
	return fieldValue(doc, ref)
}

func reindexLiteral(tok string) (any, bool) {
	tok = strings.TrimSpace(tok)
	if len(tok) >= 2 && (tok[0] == '\'' || tok[0] == '"') && tok[len(tok)-1] == tok[0] {
		return tok[1 : len(tok)-1], true
	}
	if f, err := strconv.ParseFloat(tok, 64); err == nil {
		return f, true
	}
	return nil, false
}

func splitTopLevelExpr(expr string, op byte) []string {
	var out []string
	depth := 0
	quote := byte(0)
	start := 0
	for i := 0; i < len(expr); i++ {
		c := expr[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		switch c {
		case '\'', '"':
			quote = c
		case '(', '[':
			depth++
		case ')', ']':
			depth--
		default:
			if c == op && depth == 0 {
				out = append(out, expr[start:i])
				start = i + 1
			}
		}
	}
	out = append(out, expr[start:])
	return out
}

func evalReindexExpr(expr string, doc map[string]any, id string) any {
	expr = strings.TrimSpace(expr)
	if expr == "ctx._id" || expr == "ctx.id" {
		return id
	}
	if v, ok := reindexLiteral(expr); ok {
		return v
	}
	if v, ok := reindexSourceGet(doc, expr); ok && strings.HasPrefix(strings.TrimSpace(expr), "ctx._source") {
		return v
	}
	parts := splitTopLevelExpr(expr, '+')
	if len(parts) > 1 {
		out := ""
		for _, p := range parts {
			v := evalReindexExpr(p, doc, id)
			out += settingsValueString(v)
		}
		return out
	}
	for _, opCh := range []byte{'*', '/', '-', '+'} {
		sub := splitTopLevelExpr(expr, opCh)
		if len(sub) == 2 {
			lv := evalReindexExpr(sub[0], doc, id)
			rv := evalReindexExpr(sub[1], doc, id)
			lf, lok := toFloat(lv)
			rf, rok := toFloat(rv)
			if !lok || !rok {
				if opCh == '+' {
					return settingsValueString(lv) + settingsValueString(rv)
				}
				return lv
			}
			switch opCh {
			case '*':
				return lf * rf
			case '/':
				if rf != 0 {
					return lf / rf
				}
				return lf
			case '-':
				return lf - rf
			case '+':
				return lf + rf
			}
		}
	}
	return expr
}

func evalReindexCond(cond string, doc map[string]any) bool {
	for _, op := range []string{"==", "!=", ">=", "<=", ">", "<"} {
		i := indexOutsideQuotes(cond, op)
		if i < 0 {
			continue
		}
		lhs := strings.TrimSpace(cond[:i])
		rhs := strings.TrimSpace(cond[i+len(op):])
		lv, lok := reindexSourceGet(doc, lhs)
		rv, rok := reindexLiteral(rhs)
		if !rok {
			rv, rok = reindexSourceGet(doc, rhs)
		}
		if !lok || !rok {
			return false
		}
		ls, lsok := lv.(string)
		rs, rsok := rv.(string)
		if lsok && rsok {
			switch op {
			case "==":
				return ls == rs
			case "!=":
				return ls != rs
			}
			return false
		}
		lf, _ := toFloat(lv)
		rf, _ := toFloat(rv)
		switch op {
		case "==":
			return lf == rf
		case "!=":
			return lf != rf
		case ">":
			return lf > rf
		case "<":
			return lf < rf
		case ">=":
			return lf >= rf
		case "<=":
			return lf <= rf
		}
	}
	return false
}

func indexOutsideQuotes(s, sub string) int {
	quote := byte(0)
	for i := 0; i+len(sub) <= len(s); i++ {
		c := s[i]
		if quote != 0 {
			if c == quote {
				quote = 0
			}
			continue
		}
		if c == '\'' || c == '"' {
			quote = c
			continue
		}
		if strings.HasPrefix(s[i:], sub) {
			return i
		}
	}
	return -1
}

// runReindexScript applies the (small) Painless subset used by reindex tests.
func runReindexScript(code string, doc map[string]any, id *string, op *string) {
	for _, st := range splitReindexStatements(code) {
		execReindexStmt(st, doc, id, op)
	}
}

func execReindexStmt(st string, doc map[string]any, id *string, op *string) {
	if strings.HasPrefix(st, "if") {
		open := strings.Index(st, "(")
		if open < 0 {
			return
		}
		close := matchParen(st, open)
		if close < 0 {
			return
		}
		cond := st[open+1 : close]
		rest := strings.TrimSpace(st[close+1:])
		body := ""
		remainder := ""
		if strings.HasPrefix(rest, "{") {
			end := matchCloseBrace(rest, 0)
			body = rest[1:end]
			remainder = strings.TrimSpace(rest[end+1:])
		} else {
			body = rest
		}
		if evalReindexCond(cond, doc) {
			runReindexScript(body, doc, id, op)
		}
		if remainder != "" {
			runReindexScript(remainder, doc, id, op)
		}
		return
	}
	eq := indexOutsideQuotes(st, "=")
	if eq < 0 {
		return
	}
	lhs := strings.TrimSpace(st[:eq])
	rhs := strings.TrimSpace(st[eq+1:])
	lhs = strings.TrimPrefix(lhs, "ctx.")
	switch lhs {
	case "op":
		if v, ok := reindexLiteral(rhs); ok {
			*op = settingsValueString(v)
		}
	case "_id":
		v := evalReindexExpr(rhs, doc, *id)
		*id = settingsValueString(v)
	default:
		if strings.HasPrefix(lhs, "_source.") || strings.HasPrefix(lhs, "_source[") {
			field := strings.TrimPrefix(lhs, "_source.")
			field = strings.Trim(field, "[]'\"")
			doc[field] = evalReindexExpr(rhs, doc, *id)
		}
	}
}
