use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub use crate::errors::ApiError;
use crate::resp::format_number;

pub const VERSION_CREATED: &str = "137307827";
pub const CLUSTER_NAME: &str = "opensearch";
pub const CLUSTER_UUID: &str = "slopsearch-cluster-uuid";
pub const NODE_ID: &str = "slopsearch-node-id";
pub const NODE_NAME: &str = "slopsearch-node";
pub const NODE_VERSION: &str = "3.9.0";
pub const LUCENE_VERSION: &str = "10.5.1";

#[derive(Clone)]
pub struct Index {
    pub name: String,
    pub uuid: String,
    pub creation_date: i64,
    pub mappings: Value,
    pub settings: Map<String, Value>,
    pub docs: HashMap<String, Value>,
    pub order: Vec<String>,
    pub field_types: HashMap<String, String>,
    pub closed: bool,
    pub aliases: Map<String, Value>,
    pub mapping_version: i64,
    pub pending: HashMap<String, Value>,
    pub routing: HashMap<String, String>,
    pub seg_gen: i64,
    pub deletes: i64,
    pub data_stream: String,
    pub verified: bool,
    pub index_total: i64,
    pub delete_total: i64,
    pub force_merged: bool,
    pub doc_values: HashMap<String, Map<String, Value>>,
    pub doc_ignored: HashMap<String, Vec<String>>,
    pub versions: HashMap<String, i64>,
    pub seq_nos: HashMap<String, i64>,
    pub raw_sources: HashMap<String, Vec<u8>>,
}

impl Index {
    /// Minimal index used where OpenSearch only needs the mapping machinery
    /// (for example validating an alias filter query).
    pub fn skeleton(name: &str) -> Self {
        Index {
            name: name.to_string(),
            uuid: String::new(),
            creation_date: 0,
            mappings: Value::Object(Map::new()),
            settings: Map::new(),
            docs: HashMap::new(),
            order: Vec::new(),
            field_types: HashMap::new(),
            closed: false,
            aliases: Map::new(),
            mapping_version: 0,
            pending: HashMap::new(),
            routing: HashMap::new(),
            seg_gen: 0,
            deletes: 0,
            data_stream: String::new(),
            verified: false,
            index_total: 0,
            delete_total: 0,
            force_merged: false,
            doc_values: HashMap::new(),
            doc_ignored: HashMap::new(),
            versions: HashMap::new(),
            seq_nos: HashMap::new(),
            raw_sources: HashMap::new(),
        }
    }

    pub fn is_hidden(&self) -> bool {
        match find_setting(&self.settings, "hidden") {
            Some(v) => settings_value_string(v) == "true",
            None => false,
        }
    }

    pub fn block_on(&self, name: &str) -> bool {
        match find_setting(&self.settings, &format!("blocks.{}", name)) {
            Some(v) => settings_value_string(v) == "true",
            None => false,
        }
    }

    pub fn refresh_disabled(&self) -> bool {
        match find_setting(&self.settings, "refresh_interval") {
            Some(v) => settings_value_string(v) == "-1",
            None => false,
        }
    }

    pub fn num_shards(&self) -> i64 {
        setting_int(&self.settings, "number_of_shards").unwrap_or(1)
    }

    pub fn num_replicas(&self) -> i64 {
        setting_int(&self.settings, "number_of_replicas").unwrap_or(1)
    }

    pub fn store_bytes(&self) -> i64 {
        if self.docs.is_empty() {
            return 0;
        }
        self.docs.len() as i64 * 64 + 64
    }

    pub fn ensure_field_types(&mut self) {
        let mut fresh = HashMap::new();
        build_field_types(&self.mappings, "", &mut fresh);
        self.field_types = fresh;
    }
}

#[derive(Clone)]
pub struct DataStreamInfo {
    pub name: String,
    pub backing_indices: Vec<String>,
    pub generation: i64,
    pub timestamp_field: String,
    pub template: String,
}

pub struct Store {
    pub indices: HashMap<String, Index>,
    pub counter: i64,
    pub state_version: i64,
    pub cluster_persistent: Map<String, Value>,
    pub cluster_transient: Map<String, Value>,
    pub legacy_templates: HashMap<String, Map<String, Value>>,
    pub composable_templates: HashMap<String, Map<String, Value>>,
    pub component_templates: HashMap<String, Map<String, Value>>,
    pub data_streams: HashMap<String, DataStreamInfo>,
    pub pipelines: HashMap<String, Map<String, Value>>,
    pub search_pipelines: HashMap<String, Map<String, Value>>,
    pub scripts: HashMap<String, Map<String, Value>>,
    pub scrolls: HashMap<String, crate::search::ScrollState>,
    pub scroll_counter: u64,
    pub pits: HashMap<String, crate::search::PitState>,
    pub pit_counter: u64,
}

impl Store {
    pub fn new() -> Self {
        Store {
            indices: HashMap::new(),
            counter: 0,
            state_version: 1,
            cluster_persistent: Map::new(),
            cluster_transient: Map::new(),
            legacy_templates: HashMap::new(),
            composable_templates: HashMap::new(),
            component_templates: HashMap::new(),
            data_streams: HashMap::new(),
            pipelines: HashMap::new(),
            search_pipelines: HashMap::new(),
            scripts: HashMap::new(),
            scrolls: HashMap::new(),
            scroll_counter: 0,
            pits: HashMap::new(),
            pit_counter: 0,
        }
    }

    pub fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{:020x}", self.counter)
    }

    pub fn bump_state(&mut self) {
        self.state_version += 1;
    }
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn random_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);
    let mut bytes = [0u8; 16];
    let mut x = STATE.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed);
    for chunk in bytes.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (x >> (i * 8)) as u8;
        }
    }
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn settings_value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                format_number(f)
            } else {
                n.to_string()
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

pub fn settings_value_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                let f = n.as_f64()?;
                if f == f.trunc() {
                    Some(f as i64)
                } else {
                    None
                }
            }
        }
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

pub fn setting_int(settings: &Map<String, Value>, key: &str) -> Option<i64> {
    let v = find_setting(settings, key)?;
    to_f64(v).map(|f| f as i64)
}

pub fn find_setting<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(v) = settings.get(&format!("index.{}", key)) {
        return Some(v);
    }
    if let Some(v) = settings.get(key) {
        return Some(v);
    }
    if let Some(Value::Object(m)) = settings.get("index") {
        if let Some(v) = m.get(key) {
            return Some(v);
        }
    }
    None
}

pub fn stringify_scalar(v: &Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::String(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Value::String(u.to_string())
            } else if let Some(f) = n.as_f64() {
                Value::String(format_number(f))
            } else {
                v.clone()
            }
        }
        Value::Bool(b) => Value::String(b.to_string()),
        other => other.clone(),
    }
}

pub(crate) fn is_cjk(c: char) -> bool {
    let c = c as u32;
    (0x3040..=0x30FF).contains(&c)
        || (0x31F0..=0x31FF).contains(&c)
        || (0x3400..=0x4DBF).contains(&c)
        || (0x4E00..=0x9FFF).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
        || (0xFF66..=0xFF9D).contains(&c)
}

/// Unicode simple lowercase mapping, matching Go's strings.ToLower: no
/// multi-char special cases (İ stays single 'i') and no final-sigma rule.
pub fn simple_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c.to_lowercase().next() {
            Some(l) => out.push(l),
            None => out.push(c),
        }
    }
    out
}

pub fn analyze(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<String>| {
        if !buf.is_empty() {
            out.push(simple_lower(buf));
            buf.clear();
        }
    };
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if is_han(c) {
            flush(&mut buf, &mut out);
            out.push(simple_lower(&c.to_string()));
            i += 1;
        } else if is_cjk(c) {
            if !buf.is_empty() && !buf.chars().all(is_kana) {
                flush(&mut buf, &mut out);
            }
            buf.push(c);
            i += 1;
        } else if c.is_alphanumeric() {
            buf.push(c);
            i += 1;
        } else if (c == '.' || c == '\'')
            && i > 0
            && i + 1 < chars.len()
            && chars[i - 1].is_alphanumeric()
            && chars[i + 1].is_alphanumeric()
        {
            buf.push(c);
            i += 1;
        } else {
            flush(&mut buf, &mut out);
            i += 1;
        }
    }
    flush(&mut buf, &mut out);
    out
}

pub fn is_han(c: char) -> bool {
    let c = c as u32;
    (0x3400..=0x4DBF).contains(&c)
        || (0x4E00..=0x9FFF).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
}

fn is_kana(c: char) -> bool {
    let c = c as u32;
    (0x3040..=0x30FF).contains(&c)
        || (0x31F0..=0x31FF).contains(&c)
        || (0xFF66..=0xFF9D).contains(&c)
}

pub fn english_stem(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if len > 5 && s.ends_with("ing") {
        let mut base: Vec<char> = chars[..len - 3].to_vec();
        let n = base.len();
        if n >= 2 && base[n - 1] == base[n - 2] && !matches!(base[n - 1], 'a' | 'e' | 'i' | 'o' | 'u') {
            base.pop();
        }
        return base.into_iter().collect();
    }
    if len > 4 && s.ends_with("ies") {
        let mut base: Vec<char> = chars[..len - 3].to_vec();
        base.push('y');
        return base.into_iter().collect();
    }
    if len > 4 && s.ends_with("es") {
        return chars[..len - 2].iter().collect();
    }
    if len > 3 && s.ends_with('s') && !s.ends_with("ss") {
        return chars[..len - 1].iter().collect();
    }
    s.to_string()
}

pub fn asciifold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ø' => out.push('o'),
            'đ' | 'ð' => out.push('d'),
            'ł' => out.push('l'),
            'þ' => out.push_str("th"),
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => out.push('e'),
            'í' | 'ì' | 'î' | 'ï' | 'ī' | 'į' => out.push('i'),
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ō' | 'ő' => out.push('o'),
            'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'š' => out.push('s'),
            'ž' | 'ź' | 'ż' => out.push('z'),
            'ý' | 'ÿ' => out.push('y'),
            'ř' => out.push('r'),
            'ť' => out.push('t'),
            'ď' => out.push('d'),
            'ğ' => out.push('g'),
            _ => out.push(c),
        }
    }
    out
}

pub fn analyze_with(analyzer: &str, text: &str) -> Vec<String> {
    match analyzer {
        "keyword" => vec![text.to_string()],
        "whitespace" => text.split_whitespace().map(|s| s.to_string()).collect(),
        "lowercase" => vec![simple_lower(text)],
        _ => analyze(text),
    }
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

pub fn auto_fuzziness(term: &str) -> usize {
    match term.chars().count() {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}

pub fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else if let Some(f) = n.as_f64() {
                Some(format_number(f))
            } else {
                Some(n.to_string())
            }
        }
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn doc_strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(value_to_string).collect(),
        Value::Null => vec![],
        other => value_to_string(other).into_iter().collect(),
    }
}

pub fn valid_type(t: &str) -> bool {
    matches!(
        t,
        "text" | "keyword" | "integer" | "long" | "short" | "byte" | "float" | "double"
            | "half_float" | "scaled_float" | "unsigned_long" | "date" | "date_nanos"
            | "boolean" | "object" | "nested" | "completion" | "geo_point" | "geo_shape"
            | "ip" | "binary" | "wildcard" | "constant_keyword" | "flat_object"
            | "token_count" | "search_as_you_type" | "rank_feature" | "rank_features"
            | "match_only_text" | "percolator" | "join" | "alias" | "version"
            | "integer_range" | "float_range" | "long_range" | "double_range"
            | "date_range" | "ip_range" | "xy_point" | "knn_vector"
    )
}

pub fn build_field_types(mappings: &Value, prefix: &str, out: &mut HashMap<String, String>) {
    let props = match mappings.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return,
    };
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}.{}", prefix, name)
        };
        if let Some(t) = def.get("type").and_then(|v| v.as_str()) {
            out.insert(path.clone(), t.to_string());
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            for (fname, fraw) in fields {
                if let Some(ft) = fraw.get("type").and_then(|v| v.as_str()) {
                    out.insert(format!("{}.{}", path, fname), ft.to_string());
                }
            }
        }
        build_field_types(&Value::Object(def.clone()), &path, out);
    }
}

pub fn validate_mapping(mappings: &Value) -> Result<(), String> {
    if let Some(props) = mappings.get("properties").and_then(|p| p.as_object()) {
        return validate_props(props);
    }
    Ok(())
}

fn validate_props(props: &Map<String, Value>) -> Result<(), String> {
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        if let Some(t) = def.get("type").and_then(|v| v.as_str()) {
            if !valid_type(t) {
                return Err(format!(
                    "No handler for type [{}] declared on field [{}]",
                    t, name
                ));
            }
        }
        if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
            validate_props(sub)?;
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            validate_props(fields)?;
        }
    }
    Ok(())
}

pub fn validate_doc(props: &Map<String, Value>, src: &Map<String, Value>) -> Result<(), String> {
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        let val = match src.get(name) {
            Some(v) => v,
            None => continue,
        };
        let t = def.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
            | "scaled_float" => {
                if to_f64(val).is_none() {
                    return Err(format!("failed to parse field [{}] of type [{}]", name, t));
                }
            }
            "boolean" => match val {
                Value::Bool(_) => {}
                Value::String(s) if s == "true" || s == "false" => {}
                _ => return Err(format!("failed to parse field [{}] of type [boolean]", name)),
            },
            "nested" | "object" => {
                if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
                    match val {
                        Value::Array(arr) => {
                            for el in arr {
                                if let Some(m) = el.as_object() {
                                    validate_doc(sub, m)?;
                                }
                            }
                        }
                        Value::Object(m) => validate_doc(sub, m)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn value_at_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    for part in path.split('.') {
        cur = cur.as_object()?.get(part)?;
    }
    Some(cur)
}

pub fn field_value<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = value_at_path(doc, path) {
        return Some(v);
    }
    let parts: Vec<&str> = path.split('.').collect();
    for i in (1..parts.len()).rev() {
        if let Some(v) = value_at_path(doc, &parts[..i].join(".")) {
            return Some(v);
        }
    }
    None
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn parse_date_string(s: &str) -> Option<f64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y = s[0..4].parse::<i64>().ok()?;
    let mo = s[5..7].parse::<i64>().ok()?;
    let d = s[8..10].parse::<i64>().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (mut hh, mut mi, mut ss, mut ms) = (0i64, 0i64, 0i64, 0i64);
    if bytes.len() > 10 {
        if bytes[10] != b'T' && bytes[10] != b' ' {
            return None;
        }
        let rest = &s[11..];
        if rest.len() < 8 {
            return None;
        }
        if rest.as_bytes()[2] != b':' || rest.as_bytes()[5] != b':' {
            return None;
        }
        hh = rest[0..2].parse::<i64>().ok()?;
        mi = rest[3..5].parse::<i64>().ok()?;
        ss = rest[6..8].parse::<i64>().ok()?;
        if rest.len() > 8 && rest.as_bytes()[8] == b'.' {
            let frac = &rest[9..];
            let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                let mut v: i64 = digits.parse().ok()?;
                for _ in digits.len()..3 {
                    v *= 10;
                }
                for _ in 3..digits.len() {
                    v /= 10;
                }
                ms = v;
            }
        }
    }
    let days = days_from_civil(y, mo, d);
    Some(((days * 86400 + hh * 3600 + mi * 60 + ss) * 1000 + ms) as f64)
}

pub fn parse_date(v: &Value) -> Option<f64> {
    if let Some(f) = to_f64(v) {
        return Some(f);
    }
    parse_date_string(v.as_str()?)
}

pub fn infer_type(v: &Value) -> Option<Value> {
    match v {
        Value::Bool(_) => Some(json!({"type": "boolean"})),
        Value::Number(_) => {
            let f = v.as_f64()?;
            if f.fract() == 0.0 {
                Some(json!({"type": "long"}))
            } else {
                Some(json!({"type": "float"}))
            }
        }
        Value::String(s) => {
            if parse_date_string(s).is_some() {
                Some(json!({"type": "date"}))
            } else {
                Some(json!({
                    "type": "text",
                    "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}
                }))
            }
        }
        Value::Array(a) => a
            .iter()
            .find_map(|e| if e.is_null() { None } else { infer_type(e) }),
        Value::Object(m) => {
            let mut props = Map::new();
            for (k, val) in m {
                if let Some(t) = infer_type(val) {
                    props.insert(k.clone(), t);
                }
            }
            Some(json!({"properties": Value::Object(props)}))
        }
        _ => None,
    }
}

pub fn ensure_properties(mappings: &mut Value) -> &mut Map<String, Value> {
    let obj = mappings.as_object_mut().unwrap();
    if !obj.get("properties").map(|p| p.is_object()).unwrap_or(false) {
        obj.insert("properties".to_string(), Value::Object(Map::new()));
    }
    obj.get_mut("properties").unwrap().as_object_mut().unwrap()
}

pub fn merge_mapping(props: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        if k.starts_with('_') {
            continue;
        }
        if !props.contains_key(k) {
            if let Some(t) = infer_type(v) {
                props.insert(k.clone(), t);
            }
            continue;
        }
        let sub = props
            .get(k)
            .and_then(|p| p.get("properties"))
            .and_then(|p| p.as_object())
            .cloned();
        if let Some(mut sub) = sub {
            match v {
                Value::Object(m) => merge_mapping(&mut sub, m),
                Value::Array(a) => {
                    for e in a {
                        if let Some(m) = e.as_object() {
                            merge_mapping(&mut sub, m);
                        }
                    }
                }
                _ => {}
            }
            if let Some(obj) = props.get_mut(k).and_then(|p| p.as_object_mut()) {
                obj.insert("properties".to_string(), Value::Object(sub));
            }
        }
    }
}
