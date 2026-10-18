package main

import (
	"bytes"
	"fmt"
	"net/http"
	"sort"
	"strconv"
	"strings"
	"time"
)

const catRootListing = `=^.^=
/_cat/allocation
/_cat/segment_replication
/_cat/segment_replication/{index}
/_cat/shards
/_cat/shards/{index}
/_cat/cluster_manager
/_cat/nodes
/_cat/tasks
/_cat/indices
/_cat/indices/{index}
/_cat/segments
/_cat/segments/{index}
/_cat/count
/_cat/count/{index}
/_cat/recovery
/_cat/recovery/{index}
/_cat/health
/_cat/pending_tasks
/_cat/aliases
/_cat/aliases/{alias}
/_cat/thread_pool
/_cat/thread_pool/{thread_pools}
/_cat/plugins
/_cat/fielddata
/_cat/fielddata/{fields}
/_cat/nodeattrs
/_cat/repositories
/_cat/snapshots/{repository}
/_cat/templates
/_cat/pit_segments
/_cat/pit_segments/{pit_id}
`

type catCol struct {
	key       string
	aliases   []string
	desc      string
	numeric   bool
	noDefault bool
}

type catRow struct {
	display []string
	values  map[string]any
}

var catBaseParams = map[string]bool{
	"v": true, "help": true, "h": true, "s": true, "format": true, "pretty": true,
	"bytes": true, "local": true, "cluster_manager_timeout": true, "master_timeout": true,
	"expand_wildcards": true, "ignore_unavailable": true, "ts": true, "full_id": true,
	"label": true, "index": true, "actions": true, "detailed": true, "group_by": true,
	"node_id": true, "pri": true, "name": true, "health": true, "include_unloaded_segments": true,
	"error_trace": true, "filter_path": true, "source": true,
}

func (s *Server) handleCat(w http.ResponseWriter, r *http.Request, rest []string) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		methodNotAllowed(w, r, "GET")
		return
	}
	if len(rest) == 0 {
		w.Header().Set("Content-Type", "text/plain; charset=UTF-8")
		w.WriteHeader(200)
		w.Write([]byte(catRootListing))
		return
	}
	endpoint := rest[0]
	spec := ""
	if len(rest) >= 2 {
		spec = rest[1]
	}
	if len(rest) > 2 {
		writeNoHandler(w, r)
		return
	}
	known := map[string]bool{}
	for k, v := range catBaseParams {
		known[k] = v
	}
	switch endpoint {
	case "indices":
		known["creation_date"] = true
	case "health":
		known["epoch"] = true
		known["timestamp"] = true
	case "thread_pool":
		known["size"] = true
	case "shards":
	case "nodes":
	case "cluster_manager", "master":
	case "aliases":
	case "count":
	case "segments":
	case "allocation":
	case "recovery":
	case "templates":
	case "nodeattrs":
	case "plugins":
	case "pending_tasks":
	case "repositories":
	case "fielddata":
	case "tasks":
	default:
		writeNoHandler(w, r)
		return
	}
	if v, ok := r.URL.Query()["v"]; ok {
		val := ""
		if len(v) > 0 {
			val = v[0]
		}
		if val != "" && val != "true" && val != "false" {
			writeError(w, &apiError{Type: "options_parse_exception",
				Reason: "Failed to parse value [" + val + "] as only [true] or [false] are allowed.", Status: 400})
			return
		}
	}
	if paramBool(r, "help") {
		if spec != "" {
			writeError(w, errBad("request ["+r.URL.Path+"] contains unrecognized parameter: [index]"))
			return
		}
		s.writeCatHelp(w, r, endpoint)
		return
	}
	for name := range r.URL.Query() {
		if !known[name] {
			writeError(w, errBad("request ["+r.URL.Path+"] contains unrecognized parameter: ["+name+"]"))
			return
		}
	}
	var cols []catCol
	var rows []catRow
	var apiErr *apiError
	switch endpoint {
	case "indices":
		cols, rows, apiErr = s.catIndices(r, spec, endpoint)
	case "health":
		cols, rows = s.catHealth(r)
	case "count":
		cols, rows, apiErr = s.catCount(r, spec)
	case "aliases":
		cols, rows = s.catAliases(r, spec)
	case "shards":
		cols, rows, apiErr = s.catShards(r, spec)
	case "nodes":
		cols, rows = s.catNodes(r)
	case "cluster_manager", "master":
		cols, rows = s.catClusterManager(r)
	case "nodeattrs":
		cols, rows = s.catNodeAttrs(r)
	case "plugins":
		cols, rows = catPlugins(r)
	case "pending_tasks":
		cols, rows = catPendingTasks(r)
	case "thread_pool":
		cols, rows = catThreadPool(r, spec)
	case "repositories":
		cols, rows = catRepositories(r)
	case "fielddata":
		cols, rows = catFielddata(r)
	case "allocation":
		cols, rows, apiErr = s.catAllocation(r)
	case "recovery":
		cols, rows, apiErr = s.catRecovery(r, spec)
	case "segments":
		cols, rows, apiErr = s.catSegments(r, spec)
	case "tasks":
		cols, rows = s.catTasks(r)
	case "templates":
		cols, rows = s.catTemplates(r, spec)
	}
	if apiErr != nil {
		writeError(w, apiErr)
		return
	}
	if endpoint == "master" {
		w.Header().Set("Warning", `299 OpenSearch-3.9.0-00000000 "[GET /_cat/master] is deprecated! Use [GET /_cat/cluster_manager] instead."`)
	}
	if endpoint == "health" && strings.ToLower(r.URL.Query().Get("ts")) == "false" {
		cols = cols[2:]
	}
	if apiErr := filterCatHealth(r, endpoint, &rows); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	requested := resolveCatColumns(r.URL.Query().Get("h"), cols)
	if apiErr := sortCatRows(r.URL.Query().Get("s"), requested, cols, &rows); apiErr != nil {
		writeError(w, apiErr)
		return
	}
	renderCat(w, r, requested, rows, paramBool(r, "v"))
}

func filterCatHealth(r *http.Request, endpoint string, rows *[]catRow) *apiError {
	if endpoint != "indices" {
		return nil
	}
	health := r.URL.Query().Get("health")
	if health == "" {
		return nil
	}
	if health != "green" && health != "yellow" && health != "red" {
		return &apiError{Type: "illegal_argument_exception", Reason: "unknown cluster health status [" + health + "]", Status: 400}
	}
	out := []catRow{}
	for _, row := range *rows {
		if row.values["health"] == health {
			out = append(out, row)
		}
	}
	*rows = out
	return nil
}

type catRequestedCol struct {
	key     string
	display string
	numeric bool
}

func resolveCatColumns(h string, cols []catCol) []catRequestedCol {
	if h == "" {
		out := make([]catRequestedCol, 0, len(cols))
		for _, c := range cols {
			if c.noDefault {
				continue
			}
			out = append(out, catRequestedCol{key: c.key, display: c.key, numeric: c.numeric})
		}
		return out
	}
	out := []catRequestedCol{}
	for _, token := range strings.Split(h, ",") {
		token = strings.TrimSpace(token)
		if token == "" {
			continue
		}
		if strings.ContainsAny(token, "*?") {
			for _, c := range cols {
				if globMatch(token, c.key) {
					out = append(out, catRequestedCol{key: c.key, display: c.key, numeric: c.numeric})
				}
			}
			continue
		}
		found := false
		for _, c := range cols {
			if c.key == token {
				out = append(out, catRequestedCol{key: c.key, display: token, numeric: c.numeric})
				found = true
				break
			}
			for _, a := range c.aliases {
				if a == token {
					out = append(out, catRequestedCol{key: c.key, display: token, numeric: c.numeric})
					found = true
					break
				}
			}
			if found {
				break
			}
		}
	}
	return out
}

func catAliasToKey(token string, cols []catCol) string {
	for _, c := range cols {
		if c.key == token {
			return token
		}
		for _, a := range c.aliases {
			if a == token {
				return c.key
			}
		}
	}
	return ""
}

func sortCatRows(s string, requested []catRequestedCol, cols []catCol, rows *[]catRow) *apiError {
	if s == "" {
		return nil
	}
	type sortKey struct {
		key  string
		desc bool
	}
	keys := []sortKey{}
	for _, token := range strings.Split(s, ",") {
		token = strings.TrimSpace(token)
		if token == "" {
			continue
		}
		key := token
		desc := false
		if i := strings.LastIndex(token, ":"); i >= 0 {
			dir := token[i+1:]
			if dir != "asc" && dir != "desc" {
				return &apiError{Type: "unsupported_operation_exception",
					Reason: "Unable to sort by unknown sort key `" + token + "`", Status: 500}
			}
			desc = dir == "desc"
			key = token[:i]
		}
		canonical := catAliasToKey(key, cols)
		if canonical == "" {
			return &apiError{Type: "unsupported_operation_exception",
				Reason: "Unable to sort by unknown sort key `" + token + "`", Status: 500}
		}
		keys = append(keys, sortKey{key: canonical, desc: desc})
	}
	numericCols := map[string]bool{}
	for _, c := range cols {
		numericCols[c.key] = c.numeric
	}
	list := *rows
	sort.SliceStable(list, func(i, j int) bool {
		for _, k := range keys {
			a, b := list[i].values[k.key], list[j].values[k.key]
			if a == nil && b == nil {
				continue
			}
			if a == nil {
				return k.desc
			}
			if b == nil {
				return !k.desc
			}
			var less bool
			if numericCols[k.key] {
				af, aok := toFloat(a)
				bf, bok := toFloat(b)
				if aok && bok {
					less = af < bf
				} else {
					less = fmt.Sprintf("%v", a) < fmt.Sprintf("%v", b)
				}
			} else {
				less = fmt.Sprintf("%v", a) < fmt.Sprintf("%v", b)
			}
			if less {
				return !k.desc
			}
			if fmt.Sprintf("%v", a) != fmt.Sprintf("%v", b) {
				return k.desc
			}
		}
		return false
	})
	*rows = list
	return nil
}

func renderCat(w http.ResponseWriter, r *http.Request, cols []catRequestedCol, rows []catRow, vFlag bool) {
	format := strings.ToLower(r.URL.Query().Get("format"))
	pretty := paramBool(r, "pretty")
	switch format {
	case "json":
		renderCatJSON(w, cols, rows, pretty)
	case "yaml":
		w.Header().Set("Content-Type", "application/yaml")
		renderCatYAML(w, cols, rows)
	default:
		w.Header().Set("Content-Type", "text/plain; charset=UTF-8")
		renderCatText(w, cols, rows, vFlag)
	}
}

func renderCatText(w http.ResponseWriter, cols []catRequestedCol, rows []catRow, vFlag bool) {
	widths := make([]int, len(cols))
	for i, c := range cols {
		if vFlag {
			widths[i] = len(c.display)
		}
	}
	for _, row := range rows {
		for i, c := range cols {
			v := cellString(row.values[c.key])
			if len(v) > widths[i] {
				widths[i] = len(v)
			}
		}
	}
	lines := []string{}
	if vFlag {
		parts := make([]string, len(cols))
		for i, c := range cols {
			if c.numeric {
				parts[i] = padLeft(c.display, widths[i])
			} else {
				parts[i] = padRight(c.display, widths[i])
			}
		}
		lines = append(lines, strings.TrimRight(strings.Join(parts, " "), " "))
	}
	for _, row := range rows {
		parts := make([]string, len(cols))
		for i, c := range cols {
			v := cellString(row.values[c.key])
			if c.numeric {
				parts[i] = padLeft(v, widths[i])
			} else {
				parts[i] = padRight(v, widths[i])
			}
		}
		lines = append(lines, strings.TrimRight(strings.Join(parts, " "), " "))
	}
	if len(lines) == 0 {
		return
	}
	w.Write([]byte(strings.Join(lines, "\n") + "\n"))
}

func padRight(s string, n int) string {
	for len(s) < n {
		s += " "
	}
	return s
}

func padLeft(s string, n int) string {
	for len(s) < n {
		s = " " + s
	}
	return s
}

func cellString(v any) string {
	if v == nil {
		return ""
	}
	return fmt.Sprintf("%v", v)
}

func renderCatJSON(w http.ResponseWriter, cols []catRequestedCol, rows []catRow, pretty bool) {
	w.Header().Set("Content-Type", "application/json; charset=UTF-8")
	var buf bytes.Buffer
	writeCatJSON(&buf, cols, rows, pretty)
	cw, ok := w.(*captureWriter)
	if ok {
		cw.ordered = true
	}
	w.WriteHeader(200)
	w.Write(buf.Bytes())
}

func writeCatJSON(buf *bytes.Buffer, cols []catRequestedCol, rows []catRow, pretty bool) {
	if pretty {
		buf.WriteString("[\n")
		for ri, row := range rows {
			buf.WriteString("  ")
			writeCatJSONObject(buf, cols, row, 1, true)
			if ri < len(rows)-1 {
				buf.WriteString(",")
			}
			buf.WriteString("\n")
		}
		buf.WriteString("]\n")
		return
	}
	buf.WriteString("[")
	for ri, row := range rows {
		if ri > 0 {
			buf.WriteString(",")
		}
		writeCatJSONObject(buf, cols, row, 0, false)
	}
	buf.WriteString("]")
}

func writeCatJSONObject(buf *bytes.Buffer, cols []catRequestedCol, row catRow, indent int, pretty bool) {
	buf.WriteString("{")
	for i, c := range cols {
		if i > 0 {
			buf.WriteString(",")
		}
		if pretty {
			buf.WriteString("\n")
			for j := 0; j < indent+1; j++ {
				buf.WriteString("  ")
			}
		}
		buf.WriteString(encodeString(c.display))
		if pretty {
			buf.WriteString(" : ")
		} else {
			buf.WriteString(":")
		}
		v := row.values[c.key]
		if v == nil {
			buf.WriteString("null")
		} else {
			buf.WriteString(encodeString(fmt.Sprintf("%v", v)))
		}
	}
	if pretty && len(cols) > 0 {
		buf.WriteString("\n")
		for j := 0; j < indent; j++ {
			buf.WriteString("  ")
		}
	}
	buf.WriteString("}")
}

func renderCatYAML(w http.ResponseWriter, cols []catRequestedCol, rows []catRow) {
	var buf bytes.Buffer
	buf.WriteString("---\n")
	for _, row := range rows {
		for i, c := range cols {
			prefix := "  "
			if i == 0 {
				prefix = "- "
			}
			buf.WriteString(prefix + c.display + ": ")
			v := row.values[c.key]
			if v == nil {
				buf.WriteString("null")
			} else {
				buf.WriteString(encodeString(fmt.Sprintf("%v", v)))
			}
			buf.WriteString("\n")
		}
	}
	w.Write(buf.Bytes())
}

// ------------------------------------------------------------------- help

func (s *Server) writeCatHelp(w http.ResponseWriter, r *http.Request, endpoint string) {
	w.Header().Set("Content-Type", "text/plain; charset=UTF-8")
	var cols []catCol
	switch endpoint {
	case "indices":
		all := catIndicesColumns()
		cols = append(append([]catCol{}, all[:8]...), all[10], all[11], all[8], all[9])
	case "health":
		cols = catHealthColumns()
	case "aliases":
		cols = catAliasesColumns()
	case "shards":
		cols = catShardsColumns()
	default:
		cols = []catCol{{key: "help", aliases: []string{}, desc: endpoint + " help"}}
	}
	var buf bytes.Buffer
	for _, c := range cols {
		alias := strings.Join(c.aliases, ",")
		if alias == "" {
			alias = c.key
		}
		buf.WriteString(padRight(c.key, 22) + " | " + padRight(alias, 22) + " | " + c.desc + "\n")
	}
	w.Write(buf.Bytes())
}

// ---------------------------------------------------------------- indices

func catIndicesColumns() []catCol {
	return []catCol{
		{key: "health", aliases: []string{"h"}, desc: "current health status"},
		{key: "status", aliases: []string{"s"}, desc: "open/close status"},
		{key: "index", aliases: []string{"i", "idx"}, desc: "index name"},
		{key: "uuid", aliases: []string{"id"}, desc: "index uuid"},
		{key: "pri", aliases: []string{"p", "shardsPrimary"}, desc: "number of primary shards", numeric: true},
		{key: "rep", aliases: []string{"r", "shardsReplica"}, desc: "number of replica shards", numeric: true},
		{key: "docs.count", aliases: []string{"dc", "docsCount"}, desc: "available docs", numeric: true},
		{key: "docs.deleted", aliases: []string{"dd", "docsDeleted"}, desc: "deleted docs", numeric: true},
		{key: "store.size", aliases: []string{"ss", "storeSize"}, desc: "store size of primaries & replicas", numeric: true},
		{key: "pri.store.size", aliases: []string{"pss", "priStoreSize"}, desc: "store size of primaries", numeric: true},
		{key: "creation.date", aliases: []string{"cd"}, desc: "creation date", numeric: true, noDefault: true},
		{key: "creation.date.string", aliases: []string{"cds"}, desc: "creation date string", noDefault: true},
	}
}

func (s *Server) catIndices(r *http.Request, spec, endpoint string) ([]catCol, []catRow, *apiError) {
	all := catIndicesColumns()
	expandRaw := r.URL.Query().Get("expand_wildcards")
	expand := map[string]bool{"open": true, "closed": true}
	if expandRaw != "" {
		parsed, apiErr := parseExpandWildcards(expandRaw, nil)
		if apiErr != nil {
			return nil, nil, apiErr
		}
		expand = parsed
		expand["closed"] = parsed["closed"]
	}
	o := resolveOpts2{expand: expand}
	o.ignoreUnavailable = paramBool(r, "ignore_unavailable")
	indices, apiErr := s.resolveSpec2(spec, o)
	if apiErr != nil {
		return nil, nil, apiErr
	}
	rows := []catRow{}
	for _, idx := range indices {
		h := computeIndexHealth(idx)
		row := catRow{values: map[string]any{}}
		row.values["health"] = h.Status
		status := "open"
		if idx.Closed {
			status = "close"
		}
		row.values["status"] = status
		row.values["index"] = idx.Name
		row.values["uuid"] = idx.UUID
		row.values["pri"] = strconv.Itoa(idx.numShards())
		row.values["rep"] = strconv.Itoa(idx.numReplicas())
		if idx.Closed {
			row.values["docs.count"] = nil
			row.values["docs.deleted"] = nil
			row.values["store.size"] = nil
			row.values["pri.store.size"] = nil
		} else {
			row.values["docs.count"] = strconv.Itoa(len(idx.Docs))
			row.values["docs.deleted"] = "0"
			row.values["store.size"] = formatCatBytes(idx.storeBytes(), r.URL.Query().Get("bytes"))
			row.values["pri.store.size"] = formatCatBytes(idx.storeBytes(), r.URL.Query().Get("bytes"))
		}
		row.values["creation.date"] = strconv.FormatInt(idx.CreationDate, 10)
		row.values["creation.date.string"] = time.UnixMilli(idx.CreationDate).UTC().Format("2006-01-02T15:04:05.000Z")
		rows = append(rows, row)
	}
	if _, ok := r.URL.Query()["h"]; !ok {
		sort.SliceStable(rows, func(i, j int) bool {
			return fmt.Sprintf("%v", rows[i].values["index"]) < fmt.Sprintf("%v", rows[j].values["index"])
		})
	}
	return all, rows, nil
}

func (idx *Index) storeBytes() int64 {
	if len(idx.Docs) == 0 {
		return 0
	}
	return int64(len(idx.Docs))*64 + 64
}

func formatCatBytes(n int64, unit string) string {
	switch unit {
	case "b":
		return strconv.FormatInt(n, 10)
	case "kb":
		return strconv.FormatInt(n/1024, 10)
	case "mb":
		return strconv.FormatInt(n/(1024*1024), 10)
	case "gb":
		return strconv.FormatInt(n/(1024*1024*1024), 10)
	}
	if n < 1024 {
		return strconv.FormatInt(n, 10) + "b"
	}
	if n < 1024*1024 {
		return trimFloat(float64(n)/1024) + "kb"
	}
	if n < 1024*1024*1024 {
		return trimFloat(float64(n)/(1024*1024)) + "mb"
	}
	return trimFloat(float64(n)/(1024*1024*1024)) + "gb"
}

func trimFloat(f float64) string {
	s := strconv.FormatFloat(f, 'f', 1, 64)
	s = strings.TrimSuffix(s, ".0")
	return s
}

// ----------------------------------------------------------------- health

func catHealthColumns() []catCol {
	return []catCol{
		{key: "epoch", aliases: []string{"e"}, desc: "seconds since 1970", numeric: true},
		{key: "timestamp", aliases: []string{"ts"}, desc: "time of day"},
		{key: "cluster", aliases: []string{"cl"}, desc: "cluster name"},
		{key: "status", aliases: []string{"st"}, desc: "health status"},
		{key: "node.total", aliases: []string{"nt"}, desc: "total number of nodes", numeric: true},
		{key: "node.data", aliases: []string{"nd"}, desc: "number of data nodes", numeric: true},
		{key: "discovered_cluster_manager", aliases: []string{"dcm"}, desc: "cluster manager node discovered"},
		{key: "shards", aliases: []string{"t", "sh", "total"}, desc: "total number of shards", numeric: true},
		{key: "pri", aliases: []string{"p"}, desc: "number of primary shards", numeric: true},
		{key: "relo", aliases: []string{"r"}, desc: "number of relocating nodes", numeric: true},
		{key: "init", aliases: []string{"i"}, desc: "number of initializing nodes", numeric: true},
		{key: "unassign", aliases: []string{"u"}, desc: "number of unassigned shards", numeric: true},
		{key: "pending_tasks", aliases: []string{"pt"}, desc: "number of pending tasks", numeric: true},
		{key: "max_task_wait_time", aliases: []string{"mtwt"}, desc: "max time of longest tasks"},
		{key: "active_shards_percent", aliases: []string{"asp"}, desc: "percent of active shards"},
	}
}

func (s *Server) catHealth(r *http.Request) ([]catCol, []catRow) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, _ := s.resolveIndices("*", resolveOpts{wildcardClosed: true})
	status := "green"
	pri, active, unassigned := 0, 0, 0
	for _, idx := range indices {
		h := computeIndexHealth(idx)
		status = worstStatus(status, h.Status)
		pri += h.ActivePrimary
		active += h.Active
		unassigned += h.Unassigned
	}
	total := active + unassigned
	pct := 100.0
	if total > 0 {
		pct = float64(active) / float64(total) * 100
	}
	now := time.Now()
	row := catRow{values: map[string]any{}}
	row.values["epoch"] = strconv.FormatInt(now.Unix(), 10)
	row.values["timestamp"] = now.Format("15:04:05")
	row.values["cluster"] = clusterName
	row.values["status"] = status
	row.values["node.total"] = "1"
	row.values["node.data"] = "1"
	row.values["discovered_cluster_manager"] = "true"
	row.values["shards"] = strconv.Itoa(total)
	row.values["pri"] = strconv.Itoa(pri)
	row.values["relo"] = "0"
	row.values["init"] = "0"
	row.values["unassign"] = strconv.Itoa(unassigned)
	row.values["pending_tasks"] = "0"
	row.values["max_task_wait_time"] = "-"
	row.values["active_shards_percent"] = strconv.FormatFloat(pct, 'f', 1, 64) + "%"
	return catHealthColumns(), []catRow{row}
}

// ------------------------------------------------------------------ count

func (s *Server) catCount(r *http.Request, spec string) ([]catCol, []catRow, *apiError) {
	cols := []catCol{
		{key: "epoch", aliases: []string{"e"}, desc: "seconds since 1970", numeric: true},
		{key: "timestamp", aliases: []string{"ts"}, desc: "time of day"},
		{key: "count", aliases: []string{"c"}, desc: "document count", numeric: true},
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, apiErr := s.resolveIndices(spec, resolveOpts{missing404: true, namedClosed: true})
	if apiErr != nil {
		return nil, nil, apiErr
	}
	count := 0
	for _, idx := range indices {
		if idx.Closed {
			return nil, nil, &apiError{Type: "index_closed_exception", Reason: "closed", Status: 400}
		}
	}
	for _, idx := range indices {
		count += len(idx.Docs)
	}
	if spec != "" && !strings.ContainsAny(spec, "*?") {
		for _, idx := range indices {
			for aliasName, def := range idx.Aliases {
				if aliasName == spec {
					filtered := 0
					for _, doc := range idx.Docs {
						if matchesAliasFilter(doc, def) {
							filtered++
						}
					}
					if len(def) > 0 {
						count = filtered
					}
				}
			}
		}
	}
	now := time.Now()
	row := catRow{values: map[string]any{
		"epoch":     strconv.FormatInt(now.Unix(), 10),
		"timestamp": now.Format("15:04:05"),
		"count":     strconv.Itoa(count),
	}}
	return cols, []catRow{row}, nil
}

func matchesAliasFilter(doc map[string]any, def map[string]any) bool {
	filter, ok := def["filter"].(map[string]any)
	if !ok {
		return true
	}
	term, ok := filter["term"].(map[string]any)
	if !ok {
		return true
	}
	for field, want := range term {
		got, ok := doc[field]
		if !ok {
			return false
		}
		if fmt.Sprintf("%v", got) != fmt.Sprintf("%v", want) {
			return false
		}
	}
	return true
}

// ---------------------------------------------------------------- aliases

func catAliasesColumns() []catCol {
	return []catCol{
		{key: "alias", aliases: []string{"a"}, desc: "alias name"},
		{key: "index", aliases: []string{"i", "idx"}, desc: "index alias points to"},
		{key: "filter", aliases: []string{"f", "fi"}, desc: "filter"},
		{key: "routing.index", aliases: []string{"ri", "routingIndex"}, desc: "index routing"},
		{key: "routing.search", aliases: []string{"rs", "routingSearch"}, desc: "search routing"},
		{key: "is_write_index", aliases: []string{"w", "isWriteIndex"}, desc: "write index"},
	}
}

func (s *Server) catAliases(r *http.Request, spec string) ([]catCol, []catRow) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rows := []catRow{}
	if spec == "" {
		spec = "*"
	}
	for _, idx := range s.indices {
		for alias, def := range idx.Aliases {
			if !globMatch(spec, alias) {
				continue
			}
			row := catRow{values: map[string]any{}}
			row.values["alias"] = alias
			row.values["index"] = idx.Name
			if _, ok := def["filter"]; ok {
				row.values["filter"] = "*"
			} else {
				row.values["filter"] = "-"
			}
			if v, ok := def["index_routing"]; ok {
				row.values["routing.index"] = fmt.Sprintf("%v", v)
			} else {
				row.values["routing.index"] = "-"
			}
			if v, ok := def["search_routing"]; ok {
				row.values["routing.search"] = fmt.Sprintf("%v", v)
			} else {
				row.values["routing.search"] = "-"
			}
			if v, ok := def["is_write_index"]; ok {
				row.values["is_write_index"] = fmt.Sprintf("%v", v)
			} else {
				row.values["is_write_index"] = "-"
			}
			rows = append(rows, row)
		}
	}
	return catAliasesColumns(), rows
}

// ----------------------------------------------------------------- shards

func catShardsColumns() []catCol {
	return []catCol{
		{key: "index", aliases: []string{"i", "idx"}, desc: "index name"},
		{key: "shard", aliases: []string{"s", "sh"}, desc: "shard name", numeric: true},
		{key: "prirep", aliases: []string{"p", "pr", "primaryOrReplica"}, desc: "primary or replica"},
		{key: "state", aliases: []string{"st"}, desc: "shard state"},
		{key: "docs", aliases: []string{"d", "dc", "docsCount"}, desc: "number of docs in shard", numeric: true},
		{key: "store", aliases: []string{"sto", "storeSize"}, desc: "store size of shard", numeric: true},
		{key: "ip", aliases: []string{"p", "publishAddress"}, desc: "ip of node"},
		{key: "node", aliases: []string{"n", "nodeName"}, desc: "name of node"},
		{key: "unassigned.reason", aliases: []string{"ur"}, desc: "unassigned reason", noDefault: true},
	}
}

func (s *Server) catShards(r *http.Request, spec string) ([]catCol, []catRow, *apiError) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, apiErr := s.resolveIndices(spec, resolveOpts{missing404: !paramBool(r, "ignore_unavailable")})
	if apiErr != nil {
		return nil, nil, apiErr
	}
	for _, idx := range indices {
		if idx.Closed {
			return nil, nil, &apiError{Type: "index_closed_exception", Reason: "closed (" + idx.Name + ")", Status: 400}
		}
	}
	rows := []catRow{}
	for _, idx := range indices {
		perShard := distributeDocs(len(idx.Docs), idx.numShards())
		for i := 0; i < idx.numShards(); i++ {
			row := catRow{values: map[string]any{}}
			row.values["index"] = idx.Name
			row.values["shard"] = strconv.Itoa(i)
			row.values["prirep"] = "p"
			row.values["state"] = "STARTED"
			row.values["docs"] = strconv.Itoa(perShard[i])
			row.values["store"] = formatCatBytes(int64(perShard[i])*64, r.URL.Query().Get("bytes"))
			row.values["ip"] = nodeHost
			row.values["node"] = nodeName
			row.values["unassigned.reason"] = nil
			rows = append(rows, row)
			for rp := 0; rp < idx.numReplicas(); rp++ {
				replica := catRow{values: map[string]any{}}
				replica.values["index"] = idx.Name
				replica.values["shard"] = strconv.Itoa(i)
				replica.values["prirep"] = "r"
				replica.values["state"] = "UNASSIGNED"
				replica.values["docs"] = nil
				replica.values["store"] = nil
				replica.values["ip"] = nil
				replica.values["node"] = nil
				replica.values["unassigned.reason"] = "INDEX_CREATED"
				rows = append(rows, replica)
			}
		}
	}
	if _, ok := r.URL.Query()["s"]; !ok {
		sort.SliceStable(rows, func(i, j int) bool {
			if rows[i].values["index"] != rows[j].values["index"] {
				return fmt.Sprintf("%v", rows[i].values["index"]) < fmt.Sprintf("%v", rows[j].values["index"])
			}
			if rows[i].values["shard"] != rows[j].values["shard"] {
				return fmt.Sprintf("%v", rows[i].values["shard"]) < fmt.Sprintf("%v", rows[j].values["shard"])
			}
			return rows[i].values["prirep"] == "p"
		})
	}
	return catShardsColumns(), rows, nil
}

func distributeDocs(total, shards int) []int {
	out := make([]int, shards)
	for i := 0; i < total; i++ {
		out[i%shards]++
	}
	return out
}

// ------------------------------------------------------------------ nodes

func catNodesColumns() []catCol {
	return []catCol{
		{key: "ip", aliases: []string{"i"}, desc: "ip address", numeric: true},
		{key: "heap.percent", aliases: []string{"hp", "heapPercent"}, desc: "heap percent", numeric: true},
		{key: "ram.percent", aliases: []string{"rp", "ramPercent"}, desc: "ram percent", numeric: true},
		{key: "cpu", aliases: []string{"c"}, desc: "cpu", numeric: true},
		{key: "load_1m", aliases: []string{"l"}, desc: "load 1m", numeric: true},
		{key: "load_5m", aliases: []string{"l"}, desc: "load 5m", numeric: true},
		{key: "load_15m", aliases: []string{"l"}, desc: "load 15m", numeric: true},
		{key: "node.role", aliases: []string{"r", "role", "nodeRole"}, desc: "node role"},
		{key: "node.roles", aliases: []string{"r", "roles", "nodeRoles"}, desc: "node roles"},
		{key: "cluster_manager", aliases: []string{"m", "clusterManager"}, desc: "cluster manager node"},
		{key: "master", aliases: []string{"m", "master"}, desc: "master node", noDefault: true},
		{key: "name", aliases: []string{"n"}, desc: "node name"},
		{key: "id", aliases: []string{"id"}, desc: "node id", noDefault: true},
		{key: "version", aliases: []string{"v"}, desc: "node version", noDefault: true},
	}
}

func (s *Server) catNodes(r *http.Request) ([]catCol, []catRow) {
	fullID := paramBool(r, "full_id")
	row := catRow{values: map[string]any{}}
	row.values["ip"] = nodeHost
	row.values["heap.percent"] = "12"
	row.values["ram.percent"] = "40"
	row.values["cpu"] = "5"
	row.values["load_1m"] = "0.50"
	row.values["load_5m"] = "0.40"
	row.values["load_15m"] = "0.30"
	row.values["node.role"] = nodeShortRoles()
	row.values["node.roles"] = "cluster_manager,data,ingest,remote_cluster_client"
	row.values["cluster_manager"] = "*"
	row.values["master"] = "*"
	row.values["name"] = nodeName
	if fullID {
		row.values["id"] = nodeID
	} else {
		id := nodeID
		if len(id) > 4 {
			id = id[:4]
		}
		row.values["id"] = id
	}
	row.values["version"] = nodeVersion
	return catNodesColumns(), []catRow{row}
}

func (s *Server) catClusterManager(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "id", aliases: []string{"nodeId"}, desc: "node id"},
		{key: "host", aliases: []string{"h"}, desc: "host name"},
		{key: "ip", aliases: []string{"i"}, desc: "ip address"},
		{key: "node", aliases: []string{"n"}, desc: "node name"},
	}
	row := catRow{values: map[string]any{
		"id": nodeID, "host": nodeHost, "ip": nodeHost, "node": nodeName,
	}}
	return cols, []catRow{row}
}

func (s *Server) catNodeAttrs(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "node", aliases: []string{"n"}, desc: "node name"},
		{key: "host", aliases: []string{"h"}, desc: "host name"},
		{key: "ip", aliases: []string{"i"}, desc: "ip address"},
		{key: "attr", aliases: []string{"a"}, desc: "attribute name"},
		{key: "value", aliases: []string{"v"}, desc: "attribute value"},
	}
	row := catRow{values: map[string]any{
		"node": nodeName, "host": nodeHost, "ip": nodeHost,
		"attr": "shard_indexing_pressure_enabled", "value": "true",
	}}
	return cols, []catRow{row}
}

func catPlugins(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "name", aliases: []string{"n"}, desc: "node name"},
		{key: "component", aliases: []string{"c"}, desc: "component"},
		{key: "version", aliases: []string{"v"}, desc: "version"},
	}
	return cols, nil
}

func catPendingTasks(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "insertOrder", aliases: []string{"o"}, desc: "insert order", numeric: true},
		{key: "timeInQueue", aliases: []string{"t"}, desc: "time in queue"},
		{key: "priority", aliases: []string{"p"}, desc: "priority"},
		{key: "source", aliases: []string{"s"}, desc: "source"},
	}
	return cols, nil
}

func catThreadPool(r *http.Request, spec string) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "node_name", aliases: []string{"nn"}, desc: "node name"},
		{key: "name", aliases: []string{"n"}, desc: "thread pool name"},
		{key: "active", aliases: []string{"a"}, desc: "active threads", numeric: true},
		{key: "queue", aliases: []string{"q"}, desc: "queue size", numeric: true},
		{key: "rejected", aliases: []string{"r"}, desc: "rejected threads", numeric: true},
		{key: "type", aliases: []string{"t"}, desc: "pool type", noDefault: true},
		{key: "queue_size", aliases: []string{"qs"}, desc: "queue size limit", numeric: true, noDefault: true},
	}
	pools := nodeThreadPools()
	rows := []catRow{}
	for _, name := range sortedThreadPoolNames() {
		if spec != "" {
			matched := false
			for _, pat := range strings.Split(spec, ",") {
				if globMatch(pat, name) {
					matched = true
				}
			}
			if !matched {
				continue
			}
		}
		pool, _ := pools[name].(map[string]any)
		row := catRow{values: map[string]any{
			"node_name": nodeName,
			"name":      name,
			"active":    "0",
			"queue":     "0",
			"rejected":  "0",
		}}
		if pool != nil {
			row.values["type"] = fmt.Sprintf("%v", pool["type"])
			row.values["queue_size"] = fmt.Sprintf("%v", pool["queue_size"])
		}
		rows = append(rows, row)
	}
	return cols, rows
}

func catRepositories(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "id", aliases: []string{"i"}, desc: "repository id"},
		{key: "type", aliases: []string{"t"}, desc: "repository type"},
	}
	return cols, nil
}

func catFielddata(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "id", aliases: []string{"i"}, desc: "node id"},
		{key: "host", aliases: []string{"h"}, desc: "host name"},
		{key: "ip", aliases: []string{"ip"}, desc: "ip address"},
		{key: "node", aliases: []string{"n"}, desc: "node name"},
		{key: "field", aliases: []string{"f"}, desc: "field name"},
		{key: "size", aliases: []string{"s"}, desc: "field size", numeric: true},
	}
	return cols, nil
}

func (s *Server) catAllocation(r *http.Request) ([]catCol, []catRow, *apiError) {
	cols := []catCol{
		{key: "shards", aliases: []string{"s"}, desc: "number of shards", numeric: true},
		{key: "disk.indices", aliases: []string{"di"}, desc: "disk used by indices", numeric: true},
		{key: "disk.used", aliases: []string{"du"}, desc: "disk used", numeric: true},
		{key: "disk.avail", aliases: []string{"da"}, desc: "disk available", numeric: true},
		{key: "disk.total", aliases: []string{"dt"}, desc: "disk total", numeric: true},
		{key: "disk.percent", aliases: []string{"dp"}, desc: "disk percent", numeric: true},
		{key: "host", aliases: []string{"h"}, desc: "host name"},
		{key: "ip", aliases: []string{"ip"}, desc: "ip address"},
		{key: "node", aliases: []string{"n"}, desc: "node name"},
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, _ := s.resolveIndices("*", resolveOpts{wildcardClosed: true})
	shards, assignedBytes, unassigned := 0, int64(0), 0
	for _, idx := range indices {
		if idx.Closed {
			continue
		}
		h := computeIndexHealth(idx)
		shards += h.Active
		unassigned += h.Unassigned
		assignedBytes += idx.storeBytes()
	}
	bytesParam := r.URL.Query().Get("bytes")
	var unusedAPIErr *apiError
	_ = unusedAPIErr
	diskTotal := int64(500107862016)
	row := catRow{values: map[string]any{
		"shards":       strconv.Itoa(shards),
		"disk.indices": formatCatBytes(assignedBytes, bytesParam),
		"disk.used":    formatCatBytes(int64(1024*1024*1024), bytesParam),
		"disk.avail":   formatCatBytes(diskTotal-int64(1024*1024*1024), bytesParam),
		"disk.total":   formatCatBytes(diskTotal, bytesParam),
		"disk.percent": "1",
		"host":         nodeHost,
		"ip":           nodeHost,
		"node":         nodeName,
	}}
	rows := []catRow{row}
	if unassigned > 0 {
		un := catRow{values: map[string]any{
			"shards":       strconv.Itoa(unassigned),
			"disk.indices": nil,
			"disk.used":    nil,
			"disk.avail":   nil,
			"disk.total":   nil,
			"disk.percent": nil,
			"host":         nil,
			"ip":           nil,
			"node":         "UNASSIGNED",
		}}
		rows = append(rows, un)
	}
	return cols, rows, nil
}

func (s *Server) catRecovery(r *http.Request, spec string) ([]catCol, []catRow, *apiError) {
	cols := []catCol{
		{key: "index", aliases: []string{"i", "idx"}, desc: "index name"},
		{key: "shard", aliases: []string{"s", "sh"}, desc: "shard name", numeric: true},
		{key: "start_time", aliases: []string{"st"}, desc: "start time"},
		{key: "time", aliases: []string{"t"}, desc: "recovery time"},
		{key: "type", aliases: []string{"ty"}, desc: "recovery type"},
		{key: "stage", aliases: []string{"sg"}, desc: "recovery stage"},
		{key: "source_host", aliases: []string{"shost"}, desc: "source host"},
		{key: "source_node", aliases: []string{"snode"}, desc: "source node"},
		{key: "target_host", aliases: []string{"thost"}, desc: "target host"},
		{key: "target_node", aliases: []string{"tnode"}, desc: "target node"},
		{key: "repository", aliases: []string{"rep"}, desc: "repository"},
		{key: "snapshot", aliases: []string{"snap"}, desc: "snapshot"},
		{key: "files", aliases: []string{"f"}, desc: "files", numeric: true},
		{key: "files_recovered", aliases: []string{"fr"}, desc: "files recovered", numeric: true},
		{key: "files_percent", aliases: []string{"fp"}, desc: "files percent"},
		{key: "files_total", aliases: []string{"ft"}, desc: "files total", numeric: true},
		{key: "bytes", aliases: []string{"b"}, desc: "bytes", numeric: true},
		{key: "bytes_recovered", aliases: []string{"br"}, desc: "bytes recovered", numeric: true},
		{key: "bytes_percent", aliases: []string{"bp"}, desc: "bytes percent"},
		{key: "bytes_total", aliases: []string{"bt"}, desc: "bytes total", numeric: true},
		{key: "translog_ops", aliases: []string{"to"}, desc: "translog ops", numeric: true},
		{key: "translog_ops_recovered", aliases: []string{"tor"}, desc: "translog ops recovered", numeric: true},
		{key: "translog_ops_percent", aliases: []string{"top"}, desc: "translog ops percent"},
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, apiErr := s.resolveIndices(spec, resolveOpts{missing404: true})
	if apiErr != nil {
		return nil, nil, apiErr
	}
	rows := []catRow{}
	for _, idx := range indices {
		if idx.Closed {
			continue
		}
		for i := 0; i < idx.numShards(); i++ {
			row := catRow{values: map[string]any{
				"index": idx.Name, "shard": strconv.Itoa(i),
				"start_time": "2026-01-01T00:00:00.000Z", "time": "0ms",
				"type": "empty_store", "stage": "done",
				"source_host": "-", "source_node": "-",
				"target_host": nodeHost, "target_node": nodeName,
				"repository": "-", "snapshot": "-",
				"files": "0", "files_recovered": "0", "files_percent": "100.0%", "files_total": "0",
				"bytes": "0", "bytes_recovered": "0", "bytes_percent": "100.0%", "bytes_total": "0",
				"translog_ops": "0", "translog_ops_recovered": "0", "translog_ops_percent": "100.0%",
			}}
			rows = append(rows, row)
		}
	}
	return cols, rows, nil
}

func (s *Server) catSegments(r *http.Request, spec string) ([]catCol, []catRow, *apiError) {
	cols := []catCol{
		{key: "index", aliases: []string{"i", "idx"}, desc: "index name"},
		{key: "shard", aliases: []string{"s", "sh"}, desc: "shard name", numeric: true},
		{key: "prirep", aliases: []string{"p", "pr", "primaryOrReplica"}, desc: "primary or replica"},
		{key: "ip", aliases: []string{"ip"}, desc: "ip address"},
		{key: "segment", aliases: []string{"seg"}, desc: "segment name"},
		{key: "generation", aliases: []string{"g", "gen"}, desc: "segment generation", numeric: true},
		{key: "docs.count", aliases: []string{"dc", "docsCount"}, desc: "number of docs", numeric: true},
		{key: "docs.deleted", aliases: []string{"dd", "docsDeleted"}, desc: "deleted docs", numeric: true},
		{key: "size", aliases: []string{"si"}, desc: "segment size", numeric: true},
		{key: "size.memory", aliases: []string{"sm"}, desc: "segment memory", numeric: true},
		{key: "committed", aliases: []string{"ic", "isCommitted"}, desc: "committed"},
		{key: "searchable", aliases: []string{"is", "isSearchable"}, desc: "searchable"},
		{key: "version", aliases: []string{"v"}, desc: "version"},
		{key: "compound", aliases: []string{"c"}, desc: "compound"},
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	indices, apiErr := s.resolveIndices(spec, resolveOpts{missing404: true})
	if apiErr != nil {
		return nil, nil, apiErr
	}
	rows := []catRow{}
	for _, idx := range indices {
		if idx.Closed {
			continue
		}
		perShard := distributeDocs(len(idx.Docs), idx.numShards())
		for i := 0; i < idx.numShards(); i++ {
			if perShard[i] == 0 {
				continue
			}
			row := catRow{values: map[string]any{
				"index": idx.Name, "shard": strconv.Itoa(i), "prirep": "p", "ip": nodeHost,
				"segment": fmt.Sprintf("_%d", i), "generation": "0",
				"docs.count": strconv.Itoa(perShard[i]), "docs.deleted": "0",
				"size":        formatCatBytes(int64(perShard[i])*64, r.URL.Query().Get("bytes")),
				"size.memory": "0b", "committed": "true", "searchable": "true",
				"version": luceneVersion, "compound": "true",
			}}
			rows = append(rows, row)
		}
	}
	return cols, rows, nil
}

func (s *Server) catTasks(r *http.Request) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "action", aliases: []string{"ac"}, desc: "action"},
		{key: "task_id", aliases: []string{"ti"}, desc: "task id"},
		{key: "parent_task_id", aliases: []string{"pti"}, desc: "parent task id"},
		{key: "type", aliases: []string{"ty"}, desc: "task type"},
		{key: "start_time", aliases: []string{"st"}, desc: "start time"},
		{key: "timestamp", aliases: []string{"ts"}, desc: "timestamp"},
		{key: "running_time", aliases: []string{"rt"}, desc: "running time"},
		{key: "ip", aliases: []string{"i"}, desc: "ip"},
		{key: "node", aliases: []string{"n"}, desc: "node name"},
	}
	rows := []catRow{}
	for _, t := range s.syntheticTasks() {
		if !taskActionMatches(t, r.URL.Query().Get("actions")) {
			continue
		}
		row := catRow{values: map[string]any{
			"action":       t["action"],
			"task_id":      fmt.Sprintf("%v:%v", t["node"], t["id"]),
			"type":         t["type"],
			"start_time":   time.UnixMilli(nowMillis()).UTC().Format("15:04:05"),
			"timestamp":    strconv.FormatInt(nowMillis(), 10),
			"running_time": "1ms",
			"ip":           nodeHost,
			"node":         nodeName,
		}}
		if pid, ok := t["parent_task_id"]; ok {
			row.values["parent_task_id"] = pid
		} else {
			row.values["parent_task_id"] = nil
		}
		rows = append(rows, row)
	}
	return cols, rows
}

func (s *Server) catTemplates(r *http.Request, spec string) ([]catCol, []catRow) {
	cols := []catCol{
		{key: "name", aliases: []string{"n"}, desc: "template name"},
		{key: "index_patterns", aliases: []string{"ip"}, desc: "index patterns"},
		{key: "order", aliases: []string{"o"}, desc: "order", numeric: true},
		{key: "version", aliases: []string{"v"}, desc: "version", numeric: true},
		{key: "composed_of", aliases: []string{"co"}, desc: "composed of"},
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	rows := []catRow{}
	match := func(name string) bool {
		if spec == "" || spec == "*" {
			return true
		}
		for _, pat := range strings.Split(spec, ",") {
			if globMatch(pat, name) {
				return true
			}
		}
		return false
	}
	for name, tpl := range s.legacyTemplates {
		if !match(name) {
			continue
		}
		patterns := tpl["index_patterns"]
		row := catRow{values: map[string]any{
			"name":           name,
			"index_patterns": formatPatterns(patterns),
			"order":          fmt.Sprintf("%v", intOrDefault(tpl["order"], 0)),
			"version":        intOrNil(tpl["version"]),
			"composed_of":    "",
		}}
		rows = append(rows, row)
	}
	for name, tpl := range s.composableTemplates {
		if !match(name) {
			continue
		}
		patterns := tpl["index_patterns"]
		composed := []string{}
		if raw, ok := tpl["composed_of"].([]any); ok {
			for _, c := range raw {
				composed = append(composed, fmt.Sprintf("%v", c))
			}
		}
		row := catRow{values: map[string]any{
			"name":           name,
			"index_patterns": formatPatterns(patterns),
			"order":          fmt.Sprintf("%v", intOrDefault(tpl["priority"], 0)),
			"version":        intOrNil(tpl["version"]),
			"composed_of":    "[" + strings.Join(composed, ", ") + "]",
		}}
		rows = append(rows, row)
	}
	return cols, rows
}

func intOrDefault(v any, def int) int {
	if v == nil {
		return def
	}
	if f, ok := toFloat(v); ok {
		return int(f)
	}
	return def
}

func intOrNil(v any) any {
	if v == nil {
		return nil
	}
	if f, ok := toFloat(v); ok {
		return strconv.Itoa(int(f))
	}
	return nil
}

func formatPatterns(v any) string {
	items := []string{}
	switch t := v.(type) {
	case []any:
		for _, p := range t {
			items = append(items, fmt.Sprintf("%v", p))
		}
	case string:
		items = append(items, t)
	}
	return "[" + strings.Join(items, ", ") + "]"
}
