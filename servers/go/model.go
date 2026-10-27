package main

import (
	"encoding/json"
	"fmt"
	"math"
	"os"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
)

var validTypes = map[string]bool{
	"text": true, "keyword": true, "integer": true, "long": true, "short": true,
	"byte": true, "float": true, "double": true, "half_float": true,
	"scaled_float": true, "unsigned_long": true, "date": true, "date_nanos": true,
	"boolean": true, "object": true,
	"nested": true, "completion": true, "geo_point": true, "geo_shape": true,
	"ip": true, "binary": true, "wildcard": true, "constant_keyword": true,
	"flat_object": true, "token_count": true, "search_as_you_type": true,
	"rank_feature": true, "rank_features": true, "match_only_text": true,
	"percolator": true, "join": true, "alias": true, "version": true,
	"integer_range": true, "float_range": true, "long_range": true,
	"double_range": true, "date_range": true, "ip_range": true,
}

type Index struct {
	Name           string
	UUID           string
	CreationDate   int64
	Mappings       map[string]any
	Settings       map[string]any // canonical flat form, keys prefixed "index."
	Docs           map[string]map[string]any
	Order          []string
	FieldTypes     map[string]string
	Closed         bool
	Aliases        map[string]map[string]any
	MappingVersion int64
	Pending        map[string]map[string]any // indexed but not yet refreshed (search_interval=-1)
	Routing        map[string]string         // doc id -> routing value
	SegGen         int64
	Deletes        int64
	DataStream     string // non-empty when this index backs a data stream
	Verified       bool
	IndexTotal     int64
	DeleteTotal    int64
	ForceMerged    bool
	DocValues      map[string]map[string][]any // doc id -> field path -> indexed values
	DocIgnored     map[string][]string         // doc id -> ignored (malformed) fields
	RawDocs        map[string][]byte           // doc id -> original _source bytes
	Versions       map[string]int64
	SeqNos         map[string]int64
	PrimaryTerms   map[string]int64
	NextSeq        int64
}

const versionCreated = "137307827"

func (idx *Index) isHidden() bool {
	v, ok := findSetting(idx.Settings, "hidden")
	if !ok {
		return false
	}
	return settingsValueString(v) == "true"
}

func (idx *Index) blockOn(name string) bool {
	v, ok := findSetting(idx.Settings, "blocks."+name)
	if !ok {
		return false
	}
	return settingsValueString(v) == "true"
}

func (idx *Index) refreshDisabled() bool {
	v, ok := findSetting(idx.Settings, "refresh_interval")
	if !ok {
		return false
	}
	return settingsValueString(v) == "-1"
}

func settingsValueString(v any) string {
	switch t := v.(type) {
	case string:
		return t
	case json.Number:
		return t.String()
	case float64:
		return formatNumber(t)
	case bool:
		return strconv.FormatBool(t)
	case nil:
		return "null"
	default:
		return fmt.Sprintf("%v", v)
	}
}

func settingsValueInt(v any) (int, bool) {
	switch t := v.(type) {
	case float64:
		return int(t), t == float64(int(t))
	case json.Number:
		n, err := t.Int64()
		return int(n), err == nil
	case int64:
		return int(t), true
	case string:
		n, err := strconv.Atoi(strings.TrimSpace(t))
		if err != nil {
			return 0, false
		}
		return n, true
	}
	return 0, false
}

const (
	clusterName = "opensearch"
	clusterUUID = "slopsearch-cluster-uuid"
	nodeID      = "slopsearch-node-id"
	nodeName    = "slopsearch-node"
	nodeVersion = "3.9.0"
)

func (idx *Index) numShards() int {
	if v, ok := settingInt(idx.Settings, "number_of_shards"); ok {
		return v
	}
	return 1
}

func (idx *Index) numReplicas() int {
	if v, ok := settingInt(idx.Settings, "number_of_replicas"); ok {
		return v
	}
	return 1
}

func settingInt(settings map[string]any, key string) (int, bool) {
	v, ok := findSetting(settings, key)
	if !ok {
		return 0, false
	}
	f, ok := toFloat(v)
	if !ok {
		return 0, false
	}
	return int(f), true
}

func findSetting(settings map[string]any, key string) (any, bool) {
	if v, ok := settings["index."+key]; ok {
		return v, true
	}
	if v, ok := settings[key]; ok {
		return v, true
	}
	if m, ok := settings["index"].(map[string]any); ok {
		if v, ok := m[key]; ok {
			return v, true
		}
	}
	return nil, false
}

type apiError struct {
	Type      string
	Reason    string
	Status    int
	Extra     any
	Fields    map[string]any
	CausedBy  *apiError
	TopType   string
	TopReason string
	RootCause *apiError
	ForceTop  bool
	Phase     string
}

func (e *apiError) Error() string { return e.Type + ": " + e.Reason }

func errNotFound(index string) *apiError {
	return &apiError{Type: "index_not_found_exception", Reason: "no such index [" + index + "]", Status: 404}
}

func errBad(reason string) *apiError {
	return &apiError{Type: "illegal_argument_exception", Reason: reason, Status: 400}
}

func errParsing(reason string) *apiError {
	return &apiError{Type: "parsing_exception", Reason: reason, Status: 400}
}

type Server struct {
	mu                  sync.RWMutex
	indices             map[string]*Index
	counter             int64
	stateVersion        int64
	clusterPersistent   map[string]any
	clusterTransient    map[string]any
	legacyTemplates     map[string]map[string]any
	composableTemplates map[string]map[string]any
	componentTemplates  map[string]map[string]any
	dataStreams         map[string]*dataStreamInfo
	ingestPipelines     map[string]map[string]any
	searchPipelines     map[string]map[string]any
	scripts             map[string]map[string]any
	ingestMu            sync.RWMutex
	scriptsMu           sync.RWMutex
	scrolls             map[string]*scrollContext
	scrollMu            sync.Mutex
	scrollCounter       int64
	pits                map[string]*pitContext
	pitMu               sync.Mutex
	pitCounter          int64
}

type dataStreamInfo struct {
	Name           string
	BackingIndices []string
	Generation     int64
	TimestampField string
	Template       string
}

func NewServer() *Server {
	return &Server{
		indices:             map[string]*Index{},
		stateVersion:        1,
		clusterPersistent:   map[string]any{},
		clusterTransient:    map[string]any{},
		legacyTemplates:     map[string]map[string]any{},
		composableTemplates: map[string]map[string]any{},
		componentTemplates:  map[string]map[string]any{},
		dataStreams:         map[string]*dataStreamInfo{},
		ingestPipelines:     map[string]map[string]any{},
		searchPipelines:     map[string]map[string]any{},
		scripts:             map[string]map[string]any{},
		scrolls:             map[string]*scrollContext{},
		pits:                map[string]*pitContext{},
	}
}

func (s *Server) bumpState() { s.stateVersion++ }

var randState uint64 = 88172645463325252

func simpleRand() uint64 {
	randState ^= randState << 13
	randState ^= randState >> 7
	randState ^= randState << 17
	return randState
}

func isCJK(r rune) bool {
	switch {
	case r >= 0x3040 && r <= 0x30FF:
		return true
	case r >= 0x31F0 && r <= 0x31FF:
		return true
	case r >= 0x3400 && r <= 0x4DBF:
		return true
	case r >= 0x4E00 && r <= 0x9FFF:
		return true
	case r >= 0xF900 && r <= 0xFAFF:
		return true
	case r >= 0xFF66 && r <= 0xFF9D:
		return true
	}
	return false
}

func analyze(text string) []string {
	out := []string{}
	var buf []rune
	flush := func() {
		if len(buf) > 0 {
			out = append(out, strings.ToLower(string(buf)))
			buf = buf[:0]
		}
	}
	for _, r := range text {
		switch {
		case isCJK(r):
			flush()
			out = append(out, strings.ToLower(string(r)))
		case unicode.IsLetter(r) || unicode.IsDigit(r):
			buf = append(buf, r)
		default:
			flush()
		}
	}
	flush()
	return out
}

func analyzeWith(analyzer, text string) []string {
	switch analyzer {
	case "keyword":
		return []string{text}
	case "whitespace":
		return strings.Fields(text)
	case "lowercase":
		return []string{strings.ToLower(text)}
	default:
		return analyze(text)
	}
}

func levenshtein(a, b string) int {
	ar, br := []rune(a), []rune(b)
	prev := make([]int, len(br)+1)
	curr := make([]int, len(br)+1)
	for j := range prev {
		prev[j] = j
	}
	for i := 1; i <= len(ar); i++ {
		curr[0] = i
		for j := 1; j <= len(br); j++ {
			cost := 1
			if ar[i-1] == br[j-1] {
				cost = 0
			}
			curr[j] = min(prev[j]+1, min(curr[j-1]+1, prev[j-1]+cost))
		}
		prev, curr = curr, prev
	}
	return prev[len(br)]
}

func autoFuzziness(term string) int {
	n := len([]rune(term))
	switch {
	case n <= 2:
		return 0
	case n <= 5:
		return 1
	default:
		return 2
	}
}

func toFloat(v any) (float64, bool) {
	switch t := v.(type) {
	case float64:
		return t, true
	case float32:
		return float64(t), true
	case rawFloat:
		return float64(t), true
	case int:
		return float64(t), true
	case int64:
		return float64(t), true
	case uint64:
		return float64(t), true
	case json.Number:
		f, err := t.Float64()
		return f, err == nil
	case bool:
		if t {
			return 1, true
		}
		return 0, true
	case string:
		f, err := strconv.ParseFloat(strings.TrimSpace(t), 64)
		return f, err == nil
	case nil:
		return 0, false
	}
	return 0, false
}

func toString(v any) (string, bool) {
	switch t := v.(type) {
	case string:
		return t, true
	case float64:
		return strconv.FormatFloat(t, 'f', -1, 64), true
	case json.Number:
		return t.String(), true
	case int64:
		return strconv.FormatInt(t, 10), true
	case bool:
		return strconv.FormatBool(t), true
	}
	return "", false
}

func buildFieldTypes(mappings map[string]any, prefix string, out map[string]string) {
	props, ok := mappings["properties"].(map[string]any)
	if !ok {
		return
	}
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		path := name
		if prefix != "" {
			path = prefix + "." + name
		}
		if t, ok := def["type"].(string); ok {
			out[path] = t
		}
		if fields, ok := def["fields"].(map[string]any); ok {
			for fname, fraw := range fields {
				if fdef, ok := fraw.(map[string]any); ok {
					if ft, ok := fdef["type"].(string); ok {
						out[path+"."+fname] = ft
					}
				}
			}
		}
		buildFieldTypes(def, path, out)
	}
}

func validateMapping(mappings map[string]any) error {
	props, ok := mappings["properties"].(map[string]any)
	if !ok {
		return nil
	}
	return validateProps(props)
}

func validateProps(props map[string]any) error {
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		t, _ := def["type"].(string)
		if t != "" && !validTypes[t] {
			return fmt.Errorf("No handler for type [%s] declared on field [%s]", t, name)
		}
		if sub, ok := def["properties"].(map[string]any); ok {
			if err := validateProps(sub); err != nil {
				return err
			}
		}
		if fields, ok := def["fields"].(map[string]any); ok {
			if err := validateProps(fields); err != nil {
				return err
			}
		}
	}
	return nil
}

func validateDoc(props map[string]any, src map[string]any) error {
	for name, raw := range props {
		def, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		val, present := src[name]
		if !present {
			continue
		}
		t, _ := def["type"].(string)
		switch t {
		case "integer", "long", "short", "byte", "float", "double", "half_float", "scaled_float":
			if _, ok := toFloat(val); !ok {
				return fmt.Errorf("failed to parse field [%s] of type [%s]", name, t)
			}
		case "boolean":
			switch b := val.(type) {
			case bool:
			case string:
				if b != "true" && b != "false" {
					return fmt.Errorf("failed to parse field [%s] of type [boolean]", name)
				}
			default:
				return fmt.Errorf("failed to parse field [%s] of type [boolean]", name)
			}
		case "nested", "object":
			if sub, ok := def["properties"].(map[string]any); ok {
				switch v := val.(type) {
				case []any:
					for _, el := range v {
						if m, ok := el.(map[string]any); ok {
							if err := validateDoc(sub, m); err != nil {
								return err
							}
						}
					}
				case map[string]any:
					if err := validateDoc(sub, v); err != nil {
						return err
					}
				}
			}
		}
	}
	return nil
}

func normalizeSettings(settings map[string]any) map[string]any {
	index := map[string]any{}
	for k, v := range settings {
		if k == "index" {
			if m, ok := v.(map[string]any); ok {
				for ik, iv := range m {
					index[ik] = stringifyScalar(iv)
				}
			}
			continue
		}
		index[k] = stringifyScalar(v)
	}
	return map[string]any{"index": index}
}

func stringifyScalar(v any) any {
	switch t := v.(type) {
	case float64:
		return strconv.FormatFloat(t, 'f', -1, 64)
	case bool:
		return strconv.FormatBool(t)
	default:
		return v
	}
}

func valueAtPath(doc map[string]any, path string) (any, bool) {
	parts := strings.Split(path, ".")
	var cur any = doc
	for _, p := range parts {
		m, ok := cur.(map[string]any)
		if !ok {
			return nil, false
		}
		cur, ok = m[p]
		if !ok {
			return nil, false
		}
	}
	return cur, true
}

func fieldValue(doc map[string]any, path string) (any, bool) {
	if v, ok := valueAtPath(doc, path); ok {
		return v, true
	}
	parts := strings.Split(path, ".")
	for i := len(parts) - 1; i >= 1; i-- {
		if v, ok := valueAtPath(doc, strings.Join(parts[:i], ".")); ok {
			return v, true
		}
	}
	return nil, false
}

var datePattern = regexp.MustCompile(`^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?)?$`)

func parseDate(v any) (float64, bool) {
	if f, ok := toFloat(v); ok {
		return f, true
	}
	s, ok := v.(string)
	if !ok {
		return 0, false
	}
	s = strings.TrimSpace(s)
	for _, layout := range []string{
		time.RFC3339Nano,
		"2006-01-02T15:04:05.000Z07:00",
		"2006-01-02T15:04:05Z07:00",
		"2006-01-02T15:04:05",
		"2006-01-02 15:04:05",
		"2006-01-02",
	} {
		if t, err := time.Parse(layout, s); err == nil {
			return float64(t.UnixMilli()), true
		}
	}
	return 0, false
}

func inferType(v any) map[string]any {
	switch t := v.(type) {
	case bool:
		return map[string]any{"type": "boolean"}
	case float64:
		if t == math.Trunc(t) {
			return map[string]any{"type": "long"}
		}
		return map[string]any{"type": "float"}
	case json.Number:
		if strings.ContainsAny(t.String(), ".eE") {
			return map[string]any{"type": "float"}
		}
		return map[string]any{"type": "long"}
	case string:
		if datePattern.MatchString(t) {
			return map[string]any{"type": "date"}
		}
		return map[string]any{
			"type": "text",
			"fields": map[string]any{
				"keyword": map[string]any{"type": "keyword", "ignore_above": 256},
			},
		}
	case []any:
		for _, e := range t {
			if e != nil {
				return inferType(e)
			}
		}
		return nil
	case map[string]any:
		props := map[string]any{}
		for k, val := range t {
			if sub := inferType(val); sub != nil {
				props[k] = sub
			}
		}
		return map[string]any{"properties": props}
	}
	return nil
}

func ensureProperties(mappings map[string]any) map[string]any {
	props, ok := mappings["properties"].(map[string]any)
	if !ok {
		props = map[string]any{}
		mappings["properties"] = props
	}
	return props
}

func mergeMappingForIndex(idx *Index, props, src map[string]any) {
	switch d := idx.Mappings["dynamic"].(type) {
	case bool:
		if !d {
			return
		}
	case string:
		if d == "false" {
			return
		}
	}
	mergeMapping(props, src)
}

func mergeMapping(props map[string]any, src map[string]any) {
	for k, v := range src {
		if strings.HasPrefix(k, "_") {
			continue
		}
		existing, declared := props[k].(map[string]any)
		if !declared {
			if inferred := inferType(v); inferred != nil {
				props[k] = inferred
			}
			continue
		}
		if sub, ok := existing["properties"].(map[string]any); ok {
			switch t := v.(type) {
			case map[string]any:
				mergeMapping(sub, t)
			case []any:
				for _, e := range t {
					if m, ok := e.(map[string]any); ok {
						mergeMapping(sub, m)
					}
				}
			}
		}
	}
}

func ensureFieldTypes(idx *Index) {
	fresh := map[string]string{}
	buildFieldTypes(idx.Mappings, "", fresh)
	idx.FieldTypes = fresh
}

func httpPortNum() int {
	port := os.Getenv("PORT")
	if port == "" {
		port = "9200"
	}
	n, err := strconv.Atoi(port)
	if err != nil {
		return 9200
	}
	return n
}
