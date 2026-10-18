// Query evaluation against indexed (doc values) field values.
// Port of servers/go/eval.go.

use crate::fields::{preview_value, value_scalar_string, IndexCtx};
use crate::resp::format_number;
use crate::store::*;
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;

pub struct QueryCtx<'a> {
    pub idx: &'a Index,
    pub id: String,
    pub doc: &'a Value,
    pub vals: Option<Map<String, Value>>,
}

impl<'a> QueryCtx<'a> {
    pub fn new(idx: &'a Index, id: &str, doc: &'a Value) -> Self {
        QueryCtx {
            idx,
            id: id.to_string(),
            doc,
            vals: None,
        }
    }

    pub fn with_values(
        idx: &'a Index,
        id: &str,
        doc: &'a Value,
        vals: Map<String, Value>,
    ) -> Self {
        QueryCtx {
            idx,
            id: id.to_string(),
            doc,
            vals: Some(vals),
        }
    }

    fn raw_values(&self) -> Option<&Map<String, Value>> {
        if let Some(v) = &self.vals {
            return Some(v);
        }
        self.idx.doc_values.get(&self.id)
    }

    pub fn values(&self, field: &str) -> (Vec<Value>, bool) {
        let resolved = resolve_alias_field(self.idx, field);
        if resolved == "_id" {
            return (vec![Value::String(self.id.clone())], true);
        }
        if let Some(dv) = self.raw_values() {
            if let Some(vals) = dv.get(&resolved) {
                let vals = array_values(vals);
                return (vals.clone(), !vals.is_empty());
            }
            if resolved != field {
                if let Some(vals) = dv.get(field) {
                    let vals = array_values(vals);
                    return (vals.clone(), !vals.is_empty());
                }
            }
        }
        (Vec::new(), false)
    }

    pub fn has_field(&self, field: &str) -> bool {
        let resolved = resolve_alias_field(self.idx, field);
        if resolved == "_id" {
            return true;
        }
        if let Some(dv) = self.raw_values() {
            if let Some(vals) = dv.get(&resolved) {
                if !array_values(vals).is_empty() {
                    return true;
                }
            }
            let prefix = format!("{}.", resolved);
            for path in dv.keys() {
                if path.starts_with(&prefix) {
                    return true;
                }
            }
        }
        false
    }

    pub fn values_map(&self) -> Option<&Map<String, Value>> {
        self.raw_values()
    }
}

pub fn array_values(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    }
}

pub fn resolve_alias_field(idx: &Index, field: &str) -> String {
    let mut field = field.to_string();
    for _ in 0..10 {
        let def = match mapping_field_def(idx, &field) {
            Some(d) => d,
            None => return field,
        };
        if def.get("type").and_then(|t| t.as_str()) == Some("alias") {
            let path = def.get("path").and_then(|p| p.as_str()).unwrap_or("");
            if path.is_empty() || path == field {
                return field;
            }
            field = path.to_string();
            continue;
        }
        return field;
    }
    field
}

pub fn mapping_field_def<'a>(idx: &'a Index, field: &str) -> Option<&'a Map<String, Value>> {
    let props = idx.mappings.get("properties")?.as_object()?;
    let parts: Vec<&str> = field.split('.').collect();
    let mut cur = props;
    for i in 0..parts.len() {
        if i + 1 < parts.len() {
            let joined = parts[i..].join(".");
            if let Some(def) = cur.get(&joined).and_then(|d| d.as_object()) {
                return Some(def);
            }
        }
        let def = cur.get(parts[i])?.as_object()?;
        if i == parts.len() - 1 {
            return Some(def);
        }
        let rest = parts[i + 1..].join(".");
        if let Some(sub) = def.get("fields").and_then(|f| f.as_object()) {
            if let Some(sd) = sub.get(&rest).and_then(|d| d.as_object()) {
                return Some(sd);
            }
        }
        cur = def.get("properties")?.as_object()?;
    }
    None
}

pub fn object_paths(idx: &Index) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(prefix: &str, props: &Map<String, Value>, out: &mut Vec<String>) {
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
            if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
                out.push(path.clone());
                walk(&path, sub, out);
            }
        }
    }
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        walk("", props, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// typed comparisons
// ---------------------------------------------------------------------------

fn num_eq(a: &Value, b: &Value) -> bool {
    if let (Value::Number(na), Value::Number(nb)) = (a, b) {
        if let (Some(x), Some(y)) = (na.as_i64(), nb.as_i64()) {
            return x == y;
        }
        if let (Some(x), Some(y)) = (na.as_u64(), nb.as_u64()) {
            return x == y;
        }
        if let (Some(x), Some(y)) = (na.as_i64(), nb.as_u64()) {
            return x >= 0 && x as u64 == y;
        }
        if let (Some(x), Some(y)) = (na.as_u64(), nb.as_i64()) {
            return y >= 0 && x == y as u64;
        }
        return na.as_f64().unwrap_or(0.0) == nb.as_f64().unwrap_or(0.0);
    }
    false
}

pub fn integer_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Some(u as i64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn numeric_equal(a: &Value, b: &Value) -> bool {
    if a.is_number() && b.is_number() {
        return num_eq(a, b);
    }
    match (to_f64(a), to_f64(b)) {
        (Some(af), Some(bf)) => af == bf,
        _ => false,
    }
}

fn cmp_ord(a: &Value, b: &Value) -> Option<i32> {
    if let (Value::Number(na), Value::Number(nb)) = (a, b) {
        if na.as_i64().is_some() && nb.as_i64().is_some() {
            let x = na.as_i64().unwrap_or(0);
            let y = nb.as_i64().unwrap_or(0);
            return Some(if x < y { -1 } else if x > y { 1 } else { 0 });
        }
        if na.as_u64().is_some() && nb.as_u64().is_some() {
            let x = na.as_u64().unwrap_or(0);
            let y = nb.as_u64().unwrap_or(0);
            return Some(if x < y { -1 } else if x > y { 1 } else { 0 });
        }
        if let (Some(x), Some(y)) = (na.as_i64(), nb.as_u64()) {
            if x < 0 {
                return Some(-1);
            }
            let x = x as u64;
            return Some(if x < y { -1 } else if x > y { 1 } else { 0 });
        }
        if let (Some(x), Some(y)) = (na.as_u64(), nb.as_i64()) {
            if y < 0 {
                return Some(1);
            }
            let y = y as u64;
            return Some(if x < y { -1 } else if x > y { 1 } else { 0 });
        }
        let af = na.as_f64().unwrap_or(0.0);
        let bf = nb.as_f64().unwrap_or(0.0);
        return Some(if af < bf { -1 } else if af > bf { 1 } else { 0 });
    }
    match (to_f64(a), to_f64(b)) {
        (Some(af), Some(bf)) => Some(if af < bf {
            -1
        } else if af > bf {
            1
        } else {
            0
        }),
        _ => None,
    }
}

pub fn numeric_less(a: &Value, b: &Value) -> i32 {
    if let Some(c) = cmp_ord(a, b) {
        return c;
    }
    let as_ = value_to_string(a).unwrap_or_default();
    let bs = value_to_string(b).unwrap_or_default();
    match as_.cmp(&bs) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

// ---------------------------------------------------------------------------
// term value parsing
// ---------------------------------------------------------------------------

pub fn parse_term_value(
    ftype: &str,
    def: &Map<String, Value>,
    v: &Value,
) -> Result<Vec<Value>, ApiError> {
    match ftype {
        "integer" | "long" | "short" | "byte" | "unsigned_long" => {
            let ctx = IndexCtx::query_ctx();
            match ctx.parseIntValue("_value", ftype, def, v) {
                Ok(parsed) => Ok(vec![parsed]),
                Err(_) => Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!("failed to parse value [{}] as {}", preview_value(v), ftype),
                    400,
                )),
            }
        }
        "float" | "double" | "half_float" | "scaled_float" => {
            let ctx = IndexCtx::query_ctx();
            match ctx.parseFloatValue("_value", ftype, def, v) {
                Ok(parsed) => Ok(vec![parsed]),
                Err(_) => Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!("failed to parse value [{}] as {}", preview_value(v), ftype),
                    400,
                )),
            }
        }
        "boolean" => {
            let ctx = IndexCtx::query_ctx();
            match ctx.parseBooleanValue("_value", v) {
                Ok(parsed) => Ok(vec![parsed]),
                Err(e) => Err(e),
            }
        }
        "date" | "date_nanos" => {
            let ctx = IndexCtx::query_ctx();
            match ctx.parseDateValue("_value", ftype, def, v) {
                Ok(parsed) => Ok(vec![parsed]),
                Err(e) => Err(e),
            }
        }
        "ip" => {
            let ctx = IndexCtx::query_ctx();
            match ctx.parseIPValue("_value", v) {
                Ok(parsed) => Ok(vec![parsed]),
                Err(e) => Err(e),
            }
        }
        "integer_range" | "float_range" | "long_range" | "double_range" | "date_range"
        | "ip_range" => {
            let ctx = IndexCtx::query_ctx();
            let base = ftype.trim_end_matches("_range");
            match base {
                "date" => Ok(vec![ctx.parseDateValue("_value", "date", def, v)?]),
                "ip" => Ok(vec![ctx.parseIPValue("_value", v)?]),
                "integer" | "long" => Ok(vec![ctx.parseIntValue("_value", base, def, v)?]),
                "float" | "double" => Ok(vec![ctx.parseFloatValue("_value", base, def, v)?]),
                _ => Ok(vec![v.clone()]),
            }
        }
        _ => {
            let s = value_scalar_string(v).unwrap_or_else(|| preview_value(v));
            Ok(vec![Value::String(s)])
        }
    }
}

// ---------------------------------------------------------------------------
// ip helpers
// ---------------------------------------------------------------------------

pub fn ip_to_bytes(s: &str) -> Option<Vec<u8>> {
    if let Ok(addr) = s.parse::<std::net::IpAddr>() {
        match addr {
            std::net::IpAddr::V4(v4) => Some(v4.octets().to_vec()),
            std::net::IpAddr::V6(v6) => Some(v6.octets().to_vec()),
        }
    } else {
        None
    }
}

fn ip_to_16(s: &str) -> Option<[u8; 16]> {
    let addr = s.parse::<std::net::IpAddr>().ok()?;
    match addr {
        std::net::IpAddr::V4(v4) => {
            let mut out = [0u8; 16];
            out[10] = 0xff;
            out[11] = 0xff;
            out[12..].copy_from_slice(&v4.octets());
            Some(out)
        }
        std::net::IpAddr::V6(v6) => Some(v6.octets()),
    }
}

pub fn ip_contains(host: &str, needle: &str) -> bool {
    if needle.contains('/') {
        let mut parts = needle.splitn(2, '/');
        let net = parts.next().unwrap_or("");
        let len: u32 = match parts.next().and_then(|l| l.parse().ok()) {
            Some(l) => l,
            None => return false,
        };
        let host_bytes = match ip_to_bytes(host) {
            Some(b) => b,
            None => return false,
        };
        let net_bytes = match ip_to_bytes(net) {
            Some(b) => b,
            None => return false,
        };
        if host_bytes.len() != net_bytes.len() {
            return false;
        }
        let total = (host_bytes.len() * 8) as u32;
        if len > total {
            return false;
        }
        let mut remaining = len;
        for (h, n) in host_bytes.iter().zip(net_bytes.iter()) {
            if remaining == 0 {
                return true;
            }
            let bits = remaining.min(8) as u8;
            let mask = if bits == 0 {
                0u8
            } else {
                (!0u8) << (8 - bits)
            };
            if h & mask != n & mask {
                return false;
            }
            remaining = remaining.saturating_sub(8);
        }
        return true;
    }
    match canonical_ip(needle) {
        Ok(c) => host == c,
        Err(_) => false,
    }
}

pub fn canonical_ip(s: &str) -> Result<String, i32> {
    crate::fields::canonical_ip(s)
}

fn compare_ip_literal(a: &str, b: &str) -> i32 {
    if b.contains('/') {
        if ip_contains(a, b) {
            return 0;
        }
        return 1;
    }
    match (ip_to_16(a), ip_to_16(b)) {
        (Some(ab), Some(bb)) => match ab.cmp(&bb) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
        _ => match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
    }
}

fn parse_tz_offset(tz: &str) -> Result<i64, ()> {
    let tz = tz.trim();
    if tz == "UTC" || tz == "utc" || tz == "Z" || tz.is_empty() {
        return Ok(0);
    }
    if tz.starts_with('+') || tz.starts_with('-') {
        let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
        let body = &tz[1..];
        let parts: Vec<&str> = body.split(':').collect();
        let hours: i64 = parts[0].parse().map_err(|_| ())?;
        let mut mins: i64 = 0;
        if parts.len() > 1 {
            mins = parts[1].parse().unwrap_or(0);
        }
        return Ok(sign * (hours * 3600 + mins * 60) * 1000);
    }
    Err(())
}

pub fn parse_query_date(
    def: &Map<String, Value>,
    v: &Value,
    tz: &str,
) -> Result<Value, ApiError> {
    let ctx = IndexCtx::query_ctx();
    let typ = if def.get("type").and_then(|t| t.as_str()) == Some("date_nanos") {
        "date_nanos"
    } else {
        "date"
    };
    let mut scalar = v.clone();
    if let Some(m) = v.as_object() {
        if let Some(mv) = m.get("value") {
            scalar = mv.clone();
        }
    }
    if let Value::String(s) = &scalar {
        let mut s = s.clone();
        if let Some(i) = s.find("||") {
            s = s[..i].to_string();
        }
        if let Some(abs) = crate::fields::parse_date_any(def, &s) {
            let mut abs = abs;
            if !tz.is_empty() {
                if let Ok(offset) = parse_tz_offset(tz) {
                    abs -= offset;
                }
            }
            return Ok(Value::Number(Number::from(abs)));
        }
    }
    match ctx.parseDateValue("_value", typ, def, &scalar) {
        Ok(parsed) => Ok(parsed),
        Err(e) => Err(ApiError::new(
            "search_phase_execution_exception",
            &format!("failed to parse date field [{}]", preview_value(&scalar)),
            400,
        )
        .caused_by(e)),
    }
}

// ---------------------------------------------------------------------------
// range helpers
// ---------------------------------------------------------------------------

fn in_range_field(rng: &Map<String, Value>, v: &Value) -> bool {
    let (mut lower, mut has_lower) = match rng.get("gte") {
        Some(l) => (l.clone(), true),
        None => (Value::Null, false),
    };
    let (mut upper, mut has_upper) = match rng.get("lte") {
        Some(u) => (u.clone(), true),
        None => (Value::Null, false),
    };
    if !has_lower {
        if let Some(val) = rng.get("gt") {
            lower = val.clone();
            has_lower = true;
        }
    }
    if !has_upper {
        if let Some(val) = rng.get("lt") {
            upper = val.clone();
            has_upper = true;
        }
    }
    if has_lower {
        let op = if rng.contains_key("gt") { "gt" } else { "gte" };
        let c = compare_any(&lower, v);
        if op == "gte" && c > 0 {
            return false;
        }
        if op == "gt" && c >= 0 {
            return false;
        }
    }
    if has_upper {
        let op = if rng.contains_key("lt") { "lt" } else { "lte" };
        let c = compare_any(&upper, v);
        if op == "lte" && c < 0 {
            return false;
        }
        if op == "lt" && c <= 0 {
            return false;
        }
    }
    true
}

pub fn compare_any(a: &Value, b: &Value) -> i32 {
    if let (Value::String(sa), Value::String(sb)) = (a, b) {
        return match sa.cmp(sb) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    if let Value::String(sa) = a {
        let bs = preview_value(b);
        return match sa.as_str().cmp(bs.as_str()) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    numeric_less(a, b)
}

fn range_relation(
    relation: &str,
    q_lower: &Value,
    q_upper: &Value,
    has_lower: bool,
    has_upper: bool,
    stored: &Map<String, Value>,
) -> bool {
    let (mut s_lower, mut s_has_lower) = match stored.get("gte") {
        Some(l) => (l.clone(), true),
        None => (Value::Null, false),
    };
    let (mut s_upper, mut s_has_upper) = match stored.get("lte") {
        Some(u) => (u.clone(), true),
        None => (Value::Null, false),
    };
    if !s_has_lower {
        if let Some(v) = stored.get("gt") {
            s_lower = v.clone();
            s_has_lower = true;
        }
    }
    if !s_has_upper {
        if let Some(v) = stored.get("lt") {
            s_upper = v.clone();
            s_has_upper = true;
        }
    }
    let mut intersects = true;
    if has_upper && s_has_lower && compare_any(&s_lower, q_upper) > 0 {
        intersects = false;
    }
    if has_lower && s_has_upper && compare_any(&s_upper, q_lower) < 0 {
        intersects = false;
    }
    match relation.to_uppercase().as_str() {
        "" | "INTERSECTS" => intersects,
        "CONTAINS" => {
            if (!has_lower && s_has_lower) || (!has_upper && s_has_upper) {
                return false;
            }
            if has_lower && s_has_lower && compare_any(q_lower, &s_lower) > 0 {
                return false;
            }
            if has_upper && s_has_upper && compare_any(q_upper, &s_upper) < 0 {
                return false;
            }
            true
        }
        "WITHIN" => {
            if (has_lower && !s_has_lower) || (has_upper && !s_has_upper) {
                return false;
            }
            if has_lower && s_has_lower && compare_any(&s_lower, q_lower) > 0 {
                return false;
            }
            if has_upper && s_has_upper && compare_any(&s_upper, q_upper) < 0 {
                return false;
            }
            true
        }
        _ => intersects,
    }
}

// ---------------------------------------------------------------------------
// term-level queries
// ---------------------------------------------------------------------------

impl<'a> QueryCtx<'a> {
    pub fn eval_term(&self, field: &str, raw: &Value, boost: f64) -> Result<(bool, f64), ApiError> {
        if field == "_routing" || field == "_id" {
            let want = raw
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| raw.get("value").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            let have = if field == "_id" {
                Some(self.id.clone())
            } else {
                self.idx.routing.get(&self.id).cloned()
            };
            return Ok(match have {
                Some(h) if h == want => (true, boost),
                _ => (false, 0.0),
            });
        }
        let mut val = raw.clone();
        let mut fuzz = 0usize;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(f) = m.get("fuzziness") {
                if let Some(n) = fuzziness_value(f, &preview_value(&val)) {
                    fuzz = n;
                }
            }
        }
        let mut boost = boost;
        if let Some(m) = raw.as_object() {
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
        }
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        match ftype.as_str() {
            "" => {
                let term = value_scalar_string(&val).unwrap_or_default();
                for v in &vals {
                    if v.is_null() {
                        continue;
                    }
                    let vs = value_scalar_string(v).unwrap_or_default();
                    if vs == term {
                        return Ok((true, boost));
                    }
                }
                Ok((false, 0.0))
            }
            "text" | "match_only_text" | "search_as_you_type" => {
                let term = match value_scalar_string(&val) {
                    Some(t) => t,
                    None => return Err(ApiError::parsing("failed to parse term value")),
                };
                for v in &vals {
                    let s = value_scalar_string(v).unwrap_or_default();
                    for tok in analyze(&s) {
                        if tok == term {
                            return Ok((true, boost));
                        }
                    }
                }
                Ok((false, 0.0))
            }
            "keyword" | "constant_keyword" | "wildcard" | "flat_object" | "binary" => {
                let term = match value_scalar_string(&val) {
                    Some(t) => t,
                    None => return Ok((false, 0.0)),
                };
                for v in &vals {
                    if let Some(s) = value_scalar_string(v) {
                        if s == term {
                            return Ok((true, boost));
                        }
                    }
                }
                Ok((false, 0.0))
            }
            "completion" => Err(ApiError::new(
                "search_phase_execution_exception",
                "failed to create query: Field [completion] of type [completion] is not supported for term queries",
                400,
            )
            .caused_by(ApiError::new(
                "illegal_argument_exception",
                "Field [completion] of type [completion] is not supported for term queries",
                400,
            ))),
            "token_count" | "rank_feature" | "rank_features" => {
                let parsed = parse_term_value(&ftype, &def, &val)?;
                for v in &vals {
                    if numeric_equal(v, &parsed[0]) {
                        return Ok((true, boost));
                    }
                }
                Ok((false, 0.0))
            }
            "geo_point" | "geo_shape" => Err(ApiError::parsing("failed to parse term value")),
            "join" => {
                let term = value_scalar_string(&val).unwrap_or_default();
                for v in &vals {
                    match v {
                        Value::String(s) => {
                            if *s == term {
                                return Ok((true, boost));
                            }
                        }
                        Value::Object(m) => {
                            if m.get("name").and_then(|n| n.as_str()) == Some(term.as_str()) {
                                return Ok((true, boost));
                            }
                        }
                        _ => {}
                    }
                }
                Ok((false, 0.0))
            }
            "ip_range" => {
                if let Some(s) = value_scalar_string(&val) {
                    if s.contains('/') {
                        let reason = format!("failed to create query: '{}' is not an IP string literal.", s);
                        let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
                        e.root_cause = Some(Box::new(ApiError::new("illegal_argument_exception", &reason, 400)));
                        return Err(e);
                    }
                }
                self.term_range(&def, &ftype, &val, &vals, boost)
            }
            "integer_range" | "float_range" | "long_range" | "double_range" | "date_range" => {
                self.term_range(&def, &ftype, &val, &vals, boost)
            }
            "ip" => {
                let term = match value_scalar_string(&val) {
                    Some(t) => t,
                    None => return Err(ApiError::parsing("failed to parse ip")),
                };
                let bad = if term.contains('/') {
                    crate::fields::canonical_ip(&term).is_err()
                } else {
                    term.parse::<std::net::IpAddr>().is_err()
                };
                if bad {
                    let reason = format!("failed to create query: '{}' is not an IP string literal.", term);
                    let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
                    e.root_cause = Some(Box::new(ApiError::new("illegal_argument_exception", &reason, 400)));
                    return Err(e);
                }
                for v in &vals {
                    let s = value_to_string(v).unwrap_or_default();
                    if ip_contains(&s, &term) {
                        return Ok((true, boost));
                    }
                }
                Ok((false, 0.0))
            }
            "boolean" => {
                let parsed = match parse_term_value(&ftype, &def, &val) {
                    Ok(p) => p,
                    Err(_) => {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            &format!(
                                "failed to parse value [{}] as a boolean",
                                preview_value(&val)
                            ),
                            400,
                        ))
                    }
                };
                let want = parsed[0].as_bool().unwrap_or(false);
                for v in &vals {
                    if let Value::Bool(b) = v {
                        if *b == want {
                            return Ok((true, boost));
                        }
                    }
                }
                Ok((false, 0.0))
            }
            _ => {
                let parsed = parse_term_value(&ftype, &def, &val)?;
                for v in &vals {
                    for p in &parsed {
                        if numeric_equal(v, p) {
                            return Ok((true, boost));
                        }
                        if fuzz > 0 {
                            let vs = value_scalar_string(v).unwrap_or_default();
                            let ps = value_scalar_string(p).unwrap_or_default();
                            if levenshtein(&vs, &ps) <= fuzz {
                                return Ok((true, boost));
                            }
                        }
                    }
                }
                Ok((false, 0.0))
            }
        }
    }

    fn term_range(
        &self,
        def: &Map<String, Value>,
        ftype: &str,
        val: &Value,
        vals: &[Value],
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let parsed = parse_term_value(ftype, def, val)?;
        for v in vals {
            if let Value::Object(rng) = v {
                if in_range_field(rng, &parsed[0]) {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_terms(
        &self,
        field: &str,
        raw: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut boost = boost;
        let list_raw = match raw.get(field) {
            Some(l) => l.clone(),
            None => {
                return Err(ApiError::parsing(&format!(
                    "[{}] query requires a field term to contain a value",
                    field
                )))
            }
        };
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let list = match list_raw.as_array() {
            Some(l) => l,
            None => {
                return Err(ApiError::parsing(&format!(
                    "[terms] query does not support [{}] as field value, must be an array",
                    preview_value(&list_raw)
                )))
            }
        };
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        for item in list {
            let mut matched = false;
            if item.is_object() {
                return Err(ApiError::parsing("[terms] query does not support map values"));
            }
            match ftype.as_str() {
                "text" | "match_only_text" | "search_as_you_type" => {
                    let term = value_scalar_string(item).unwrap_or_default();
                    for v in &vals {
                        let s = value_scalar_string(v).unwrap_or_default();
                        for tok in analyze(&s) {
                            if tok == term {
                                matched = true;
                                break;
                            }
                        }
                        if matched {
                            break;
                        }
                    }
                }
                "keyword" | "constant_keyword" | "wildcard" | "flat_object" | "binary" | "" => {
                    let term = value_scalar_string(item).unwrap_or_default();
                    for v in &vals {
                        if value_scalar_string(v).unwrap_or_default() == term {
                            matched = true;
                            break;
                        }
                    }
                }
                _ => {
                    let parsed = parse_term_value(&ftype, &def, item)?;
                    for v in &vals {
                        if let Value::Object(rng) = v {
                            if in_range_field(rng, &parsed[0]) {
                                matched = true;
                                break;
                            }
                            continue;
                        }
                        if let Value::String(s) = v {
                            if ftype == "ip" {
                                let ps = value_scalar_string(&parsed[0]).unwrap_or_default();
                                if ip_contains(s, &ps) {
                                    matched = true;
                                    break;
                                }
                                continue;
                            }
                        }
                        if numeric_equal(v, &parsed[0]) {
                            matched = true;
                            break;
                        }
                    }
                }
            }
            if matched {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_ids(
        &self,
        raw: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut boost = boost;
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let raw_vals = match raw.get("values") {
            Some(v) => v,
            None => return Err(ApiError::parsing("[ids] requires a 'values' element")),
        };
        let list: Vec<Value> = match raw_vals {
            Value::Array(a) => a.clone(),
            Value::String(s) => vec![Value::String(s.clone())],
            Value::Number(n) => vec![Value::String(n.to_string())],
            _ => return Err(ApiError::parsing("[ids] 'values' must be an array")),
        };
        for item in &list {
            let s = value_scalar_string(item).unwrap_or_default();
            if s == self.id {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // range query
    // -----------------------------------------------------------------------

    pub fn eval_range(
        &self,
        field: &str,
        cond: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        let mut boost = boost;
        let relation = cond
            .get("relation")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let time_zone = cond
            .get("time_zone")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(b) = cond.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let parse_bound = |v: &Value| -> Result<Value, ApiError> {
            match ftype.as_str() {
                "date" | "date_nanos" | "date_range" => parse_query_date(&def, v, &time_zone),
                "ip" | "ip_range" => {
                    if v.is_object() {
                        return Ok(v.clone());
                    }
                    let ctx = IndexCtx::query_ctx();
                    ctx.parseIPValue(field, v)
                }
                "integer" | "long" | "short" | "byte" | "unsigned_long" | "integer_range"
                | "long_range" | "float" | "double" | "half_float" | "scaled_float"
                | "float_range" | "double_range" | "token_count" => {
                    let base = ftype.trim_end_matches("_range");
                    let ctx = IndexCtx::query_ctx();
                    if is_integer_kind(base) {
                        ctx.parseIntValue(field, base, &def, v)
                    } else {
                        ctx.parseFloatValue(field, base, &def, v)
                    }
                }
                _ => Ok(v.clone()),
            }
        };
        let is_range_field = matches!(
            ftype.as_str(),
            "integer_range" | "long_range" | "float_range" | "double_range" | "date_range"
                | "ip_range"
        );
        if is_range_field {
            let (mut lower, mut has_lower) = match cond.get("gte") {
                Some(l) => (l.clone(), true),
                None => (Value::Null, false),
            };
            if !has_lower {
                if let Some(l) = cond.get("gt") {
                    lower = l.clone();
                    has_lower = true;
                }
            }
            let (mut upper, mut has_upper) = match cond.get("lte") {
                Some(u) => (u.clone(), true),
                None => (Value::Null, false),
            };
            if !has_upper {
                if let Some(u) = cond.get("lt") {
                    upper = u.clone();
                    has_upper = true;
                }
            }
            let mut q_lower = Value::Null;
            let mut q_upper = Value::Null;
            if has_lower {
                q_lower = parse_bound(&lower)?;
            }
            if has_upper {
                q_upper = parse_bound(&upper)?;
            }
            for v in &vals {
                if let Value::Object(rng) = v {
                    if range_relation(&relation, &q_lower, &q_upper, has_lower, has_upper, rng) {
                        return Ok((true, boost));
                    }
                }
            }
            return Ok((false, 0.0));
        }
        for v in &vals {
            let mut ok = true;
            let mut cond_items: Vec<(&String, &Value)> = cond.iter().collect();
            cond_items.sort_by(|a, b| a.0.cmp(b.0));
            for (op, bound) in cond_items {
                match op.as_str() {
                    "boost" | "relation" | "time_zone" | "format" | "_name" => continue,
                    _ => {}
                }
                let bv = parse_bound(bound)?;
                let cmp = if ftype == "ip" {
                    let vs = value_to_string(v).unwrap_or_default();
                    let bs = value_to_string(&bv).unwrap_or_default();
                    compare_ip_literal(&vs, &bs)
                } else if ftype == "text"
                    || ftype == "keyword"
                    || ftype == "match_only_text"
                    || ftype == "wildcard"
                    || ftype.is_empty()
                {
                    let vs = value_to_string(v).unwrap_or_default();
                    let bs = value_to_string(&bv).unwrap_or_default();
                    match vs.cmp(&bs) {
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    }
                } else {
                    if let Value::Object(rng) = v {
                        if !in_range_field(rng, &bv) {
                            ok = false;
                        }
                        continue;
                    }
                    numeric_less(v, &bv)
                };
                match op.as_str() {
                    "gt" => {
                        if cmp <= 0 {
                            ok = false;
                        }
                    }
                    "gte" => {
                        if cmp < 0 {
                            ok = false;
                        }
                    }
                    "lt" => {
                        if cmp >= 0 {
                            ok = false;
                        }
                    }
                    "lte" => {
                        if cmp > 0 {
                            ok = false;
                        }
                    }
                    _ => {}
                }
                if !ok {
                    break;
                }
            }
            if ok {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // exists / prefix / wildcard / regexp / fuzzy
    // -----------------------------------------------------------------------

    pub fn eval_exists(&self, raw: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        let field = raw
            .get("field")
            .and_then(value_to_string)
            .unwrap_or_default();
        if field.is_empty() {
            return Err(ApiError::parsing("[exists] query requires 'field'"));
        }
        let boost = raw.get("boost").and_then(to_f64).unwrap_or(1.0);
        if field.contains('*') || field.contains('?') {
            let re = wildcard_regex(&field);
            if let Some(dv) = self.raw_values() {
                for (path, vals) in dv {
                    if re.is_match(path) && !array_values(vals).is_empty() {
                        return Ok((true, boost));
                    }
                }
            }
            for path in object_paths(self.idx) {
                if re.is_match(&path) && self.has_field(&path) {
                    return Ok((true, boost));
                }
            }
            return Ok((false, 0.0));
        }
        if self.has_field(&field) {
            return Ok((true, boost));
        }
        Ok((false, 0.0))
    }

    pub fn eval_prefix(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut val = raw.clone();
        let mut boost = boost;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
        }
        let prefix = match value_scalar_string(&val) {
            Some(p) => p,
            None => return Ok((false, 0.0)),
        };
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
                for tok in analyze(&s) {
                    if tok.starts_with(&prefix) {
                        return Ok((true, boost));
                    }
                }
            } else if s.starts_with(&prefix) {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_wildcard(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut val = raw.clone();
        let mut boost = boost;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
        }
        let pattern = match value_scalar_string(&val) {
            Some(p) => p.to_lowercase(),
            None => return Ok((false, 0.0)),
        };
        let re = wildcard_regex(&pattern);
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            let candidates: Vec<String> =
                if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
                    analyze(&s)
                } else {
                    vec![s]
                };
            for cand in candidates {
                if re.is_match(&cand.to_lowercase()) {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_regexp(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut val = raw.clone();
        let mut boost = boost;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
            if let Some(states) = m.get("max_determinized_states").and_then(to_f64) {
                if states <= 10.0 {
                    let reason = "failed to create query: [regexp] parsing failed. would require more than 10 effort";
                    let mut e = ApiError::new("search_phase_execution_exception", reason, 400);
                    e.root_cause = Some(Box::new(ApiError::new("query_shard_exception", reason, 400)));
                    return Err(e);
                }
            }
        }
        let pattern = match value_scalar_string(&val) {
            Some(p) => p,
            None => return Ok((false, 0.0)),
        };
        let whole = format!("^(?:{})$", pattern);
        let re = match Regex::new(&whole) {
            Ok(r) => r,
            Err(_) => {
                let msg = "unexpected end-of-string";
                let mut e = ApiError::new(
                    "search_phase_execution_exception",
                    &format!("failed to create query: {}", msg),
                    400,
                );
                e.root_cause = Some(Box::new(ApiError::new("query_shard_exception", msg, 400)));
                return Err(e);
            }
        };
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            let candidates: Vec<String> = if ftype == "text" || ftype == "match_only_text" {
                analyze(&s)
            } else {
                vec![s]
            };
            for cand in candidates {
                if re.is_match(&cand) {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_fuzzy(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let m = match raw.as_object() {
            Some(m) => m,
            None => return Err(ApiError::parsing("[fuzzy] query requires an object")),
        };
        let val = match m.get("value") {
            Some(v) => v,
            None => return Err(ApiError::parsing("[fuzzy] query requires 'value'")),
        };
        let mut boost = boost;
        if let Some(b) = m.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let term = value_scalar_string(val).unwrap_or_default();
        let mut fuzz = auto_fuzziness(&term);
        if let Some(f) = m.get("fuzziness") {
            if let Some(n) = fuzziness_value(f, &term) {
                fuzz = n;
            }
        }
        let prefix_length = m.get("prefix_length").and_then(to_f64).unwrap_or(0.0) as usize;
        let max_expansions = m.get("max_expansions").and_then(to_f64).unwrap_or(0.0) as usize;
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        let mut matched = 0usize;
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            let candidates: Vec<String> = if ftype == "text" || ftype == "match_only_text" {
                analyze(&s)
            } else {
                vec![s]
            };
            for cand in candidates {
                if cand == term {
                    return Ok((true, boost * 2.0));
                }
                if cand.len() <= prefix_length || term.len() <= prefix_length {
                    continue;
                }
                if cand.as_bytes()[..prefix_length] != term.as_bytes()[..prefix_length] {
                    continue;
                }
                if levenshtein(&cand, &term) <= fuzz {
                    if max_expansions > 0 && matched >= max_expansions {
                        continue;
                    }
                    matched += 1;
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // full text
    // -----------------------------------------------------------------------

    pub fn field_tokens(&self, field: &str) -> (Vec<String>, bool) {
        let (vals, present) = self.values(field);
        if !present {
            return (Vec::new(), false);
        }
        let mut out = Vec::new();
        for v in &vals {
            if let Some(s) = value_scalar_string(v) {
                out.extend(analyze(&s));
            }
        }
        (out, true)
    }

    pub fn field_strings(&self, field: &str) -> (Vec<String>, bool) {
        let (vals, present) = self.values(field);
        if !present {
            return (Vec::new(), false);
        }
        let mut out = Vec::new();
        for v in &vals {
            if let Some(s) = value_scalar_string(v) {
                out.push(s);
            }
        }
        (out, true)
    }

    pub fn analyzer_name(&self, field: &str, opts: Option<&Map<String, Value>>) -> String {
        if let Some(m) = opts {
            if let Some(a) = m.get("analyzer").and_then(|a| a.as_str()) {
                if !a.is_empty() {
                    return a.to_string();
                }
            }
        }
        if let Some(def) = mapping_field_def(self.idx, field) {
            if let Some(a) = def.get("search_analyzer").and_then(|a| a.as_str()) {
                if !a.is_empty() {
                    return a.to_string();
                }
            }
            if let Some(a) = def.get("analyzer").and_then(|a| a.as_str()) {
                if !a.is_empty() {
                    return a.to_string();
                }
            }
        }
        String::new()
    }

    pub fn analyze_field(&self, field: &str, opts: Option<&Map<String, Value>>, text: &str) -> Vec<String> {
        let a = self.analyzer_name(field, opts);
        if a.is_empty() {
            return analyze(text);
        }
        analyze_with(&a, text)
    }

    pub fn eval_match(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let (text, opts) = query_text(raw);
        let mut boost = boost;
        if let Some(m) = opts {
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
        }
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let operator = opts
            .and_then(|m| m.get("operator"))
            .and_then(|o| o.as_str())
            .unwrap_or("or")
            .to_lowercase();
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        if is_eval_numeric_type(&ftype)
            || ftype == "boolean"
            || ftype == "date"
            || ftype == "date_nanos"
            || ftype == "ip"
        {
            let parsed = match parse_term_value(&ftype, &def, &Value::String(text.clone())) {
                Ok(p) => p,
                Err(_) => return Ok((false, 0.0)),
            };
            for v in &vals {
                for p in &parsed {
                    if ftype == "ip" {
                        let s = value_to_string(v).unwrap_or_default();
                        let ps = value_to_string(p).unwrap_or_default();
                        if ip_contains(&s, &ps) {
                            return Ok((true, boost));
                        }
                        continue;
                    }
                    if numeric_equal(v, p) {
                        return Ok((true, boost));
                    }
                }
            }
            return Ok((false, 0.0));
        }
        if ftype == "keyword"
            || ftype == "constant_keyword"
            || ftype == "wildcard"
            || ftype == "flat_object"
        {
            for v in &vals {
                let s = value_scalar_string(v).unwrap_or_default();
                if s == text {
                    return Ok((true, boost));
                }
            }
            return Ok((false, 0.0));
        }
        let qtokens = self.analyze_field(field, opts, &text);
        if qtokens.is_empty() {
            return Ok((false, 0.0));
        }
        let mut doc_len = 0usize;
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            doc_len += self.analyze_field(field, None, &s).len();
        }
        let norm = 1.0 + 1.0 / (1.0 + doc_len as f64);
        let mut score = 0.0;
        let mut matched_all = true;
        for qt in &qtokens {
            let mut found = false;
            for v in &vals {
                let s = value_scalar_string(v).unwrap_or_default();
                for tok in self.analyze_field(field, None, &s) {
                    if &tok == qt {
                        found = true;
                        break;
                    }
                }
                if found {
                    break;
                }
            }
            if found {
                score += norm;
            } else {
                matched_all = false;
            }
        }
        if operator == "and" {
            if matched_all {
                return Ok((true, boost * norm * qtokens.len() as f64));
            }
            return Ok((false, 0.0));
        }
        if score > 0.0 {
            return Ok((true, boost * score));
        }
        Ok((false, 0.0))
    }

    pub fn eval_match_phrase(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
        _slop: f64,
    ) -> Result<(bool, f64), ApiError> {
        let (text, opts) = query_text(raw);
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if ftype == "boolean"
            || is_eval_numeric_type(&ftype)
            || ftype == "date"
            || ftype == "date_nanos"
            || ftype == "ip"
        {
            return self.eval_match(field, raw, boost);
        }
        if ftype == "keyword"
            || ftype == "constant_keyword"
            || ftype == "wildcard"
            || ftype == "flat_object"
        {
            for v in self.must_values(field) {
                let s = value_scalar_string(&v).unwrap_or_default();
                if s == text {
                    return Ok((true, boost));
                }
            }
            return Ok((false, 0.0));
        }
        let qtokens = self.analyze_field(field, opts, &text);
        if qtokens.is_empty() {
            return Ok((false, 0.0));
        }
        let mut doc_len = 0usize;
        for v in self.must_values(field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            let dtokens = self.analyze_field(field, None, &s);
            doc_len += dtokens.len();
            if contains_subsequence(&dtokens, &qtokens) {
                let norm = 1.0 + 1.0 / (1.0 + doc_len as f64);
                return Ok((true, boost * norm * qtokens.len() as f64));
            }
        }
        Ok((false, 0.0))
    }

    pub fn must_values(&self, field: &str) -> Vec<Value> {
        let (vals, _) = self.values(field);
        vals
    }

    pub fn eval_match_bool_prefix(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let (text, opts) = query_text(raw);
        let operator = opts
            .and_then(|m| m.get("operator"))
            .and_then(|o| o.as_str())
            .unwrap_or("or")
            .to_lowercase();
        let qtokens = self.analyze_field(field, opts, &text);
        if qtokens.is_empty() {
            return Ok((false, 0.0));
        }
        if operator == "and" {
            for (i, tok) in qtokens.iter().enumerate() {
                let ok = if i == qtokens.len() - 1 {
                    self.eval_prefix(field, &Value::String(tok.clone()), boost)?.0
                } else {
                    self.eval_term(field, &Value::String(tok.clone()), boost)?.0
                };
                if !ok {
                    return Ok((false, 0.0));
                }
            }
            return Ok((true, boost));
        }
        for (i, tok) in qtokens.iter().enumerate() {
            let ok = if i == qtokens.len() - 1 {
                self.eval_prefix(field, &Value::String(tok.clone()), boost)?.0
            } else {
                self.eval_term(field, &Value::String(tok.clone()), boost)?.0
            };
            if ok {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // bool / compound
    // -----------------------------------------------------------------------

    pub fn eval_bool(
        &self,
        raw: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        for k in raw.keys() {
            match k.as_str() {
                "must" | "should" | "filter" | "must_not" | "minimum_should_match" | "boost"
                | "adjust_pure_negative" | "_name" => {}
                _ => {
                    return Err(ApiError::new(
                        "x_content_parse_exception",
                        &format!(
                            "[bool] unknown field [{}] did you mean any of [must, mustNot]?",
                            k
                        ),
                        400,
                    ))
                }
            }
        }
        let mut boost = boost;
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let must = as_query_list(raw.get("must"));
        let filter = as_query_list(raw.get("filter"));
        let should = as_query_list(raw.get("should"));
        let must_not = as_query_list(raw.get("must_not"));
        let mut score = 0.0;
        for list in [&must, &filter] {
            for q in list {
                let qm = match q.as_object() {
                    Some(m) => m,
                    None => return Err(ApiError::parsing("malformed bool clause")),
                };
                let (matched, sc) = self.eval(qm)?;
                if !matched {
                    return Ok((false, 0.0));
                }
                score += sc;
            }
        }
        if !filter.is_empty() {
            let mut fscore = 0.0;
            for q in &filter {
                if let Some(qm) = q.as_object() {
                    let (_, sc) = self.eval(qm)?;
                    fscore += sc;
                }
            }
            score -= fscore;
        }
        let mut should_matched = 0usize;
        for q in &should {
            let qm = match q.as_object() {
                Some(m) => m,
                None => continue,
            };
            let (matched, sc) = self.eval(qm)?;
            if matched {
                should_matched += 1;
                score += sc;
            }
        }
        let has_positives = !must.is_empty() || !filter.is_empty();
        let msm = if let Some(spec) = raw.get("minimum_should_match") {
            minimum_should_match(spec, should.len(), should_matched)
        } else if !has_positives {
            1
        } else {
            0
        };
        if !should.is_empty() && should_matched < msm {
            return Ok((false, 0.0));
        }
        let adjust = raw
            .get("adjust_pure_negative")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if must.is_empty() && filter.is_empty() && should.is_empty() && !must_not.is_empty() {
            if !adjust {
                return Ok((false, 0.0));
            }
        }
        for q in &must_not {
            let qm = match q.as_object() {
                Some(m) => m,
                None => continue,
            };
            let (matched, _) = self.eval(qm)?;
            if matched {
                return Ok((false, 0.0));
            }
        }
        if must.is_empty() && filter.is_empty() && should.is_empty() {
            return Ok((true, boost));
        }
        Ok((true, boost * score))
    }

    pub fn eval_constant_score(
        &self,
        raw: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let filter = match raw.get("filter").and_then(|f| f.as_object()) {
            Some(f) => f,
            None => return Err(ApiError::parsing("[constant_score] requires a 'filter' element")),
        };
        let mut boost = boost;
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let (matched, _) = self.eval(filter)?;
        if matched {
            return Ok((true, boost));
        }
        Ok((false, 0.0))
    }

    pub fn eval_dis_max(
        &self,
        raw: &Map<String, Value>,
        boost: f64,
    ) -> Result<(bool, f64), ApiError> {
        let mut boost = boost;
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let tie = raw.get("tie_breaker").and_then(to_f64).unwrap_or(0.0);
        let queries = as_query_list(raw.get("queries"));
        let mut best = 0.0;
        let mut sum = 0.0;
        let mut found = false;
        for q in &queries {
            let qm = match q.as_object() {
                Some(m) => m,
                None => continue,
            };
            let (matched, sc) = self.eval(qm)?;
            if matched {
                found = true;
                if sc > best {
                    best = sc;
                }
                sum += sc;
            }
        }
        if !found {
            return Ok((false, 0.0));
        }
        Ok((true, boost * (best + tie * (sum - best))))
    }

    pub fn eval(&self, raw: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        if raw.is_empty() {
            return Ok((true, 1.0));
        }
        for (clause, body) in raw {
            match clause.as_str() {
                "match_all" => {
                    let mut boost = 1.0;
                    if let Some(m) = body.as_object() {
                        if let Some(b) = m.get("boost").and_then(to_f64) {
                            boost = b;
                        }
                    }
                    return Ok((true, boost));
                }
                "match_none" => return Ok((false, 0.0)),
                "term" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[term] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[term] query malformed")),
                    };
                    return self.eval_term(&field, &val, 1.0);
                }
                "terms" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[terms] query malformed")),
                    };
                    let mut field = String::new();
                    for k in bm.keys() {
                        if k != "boost" && k != "_name" {
                            field = k.clone();
                            break;
                        }
                    }
                    if field.is_empty() {
                        return Err(ApiError::parsing("[terms] query malformed"));
                    }
                    return self.eval_terms(&field, bm, 1.0);
                }
                "ids" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[ids] query malformed")),
                    };
                    return self.eval_ids(bm, 1.0);
                }
                "range" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[range] query malformed")),
                    };
                    let (field, cond_raw) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[range] query malformed")),
                    };
                    let cond = match cond_raw.as_object() {
                        Some(c) => c.clone(),
                        None => return Err(ApiError::parsing("[range] query malformed")),
                    };
                    return self.eval_range(&field, &cond, 1.0);
                }
                "exists" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[exists] query malformed")),
                    };
                    return self.eval_exists(bm);
                }
                "prefix" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[prefix] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[prefix] query malformed")),
                    };
                    return self.eval_prefix(&field, &val, 1.0);
                }
                "wildcard" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[wildcard] query malformed")),
                    };
                    if !bm.contains_key("value") {
                        if bm.len() == 1 {
                            if let Some((field, val)) = single_entry(bm) {
                                if !val.is_object() {
                                    let mut m2 = Map::new();
                                    m2.insert("value".to_string(), val.clone());
                                    return self.eval_wildcard(field, &Value::Object(m2), 1.0);
                                }
                            }
                        }
                        return Err(ApiError::parsing("bad wildcard body"));
                    }
                    return Err(ApiError::parsing("bad wildcard body"));
                }
                "regexp" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[regexp] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[regexp] query malformed")),
                    };
                    return self.eval_regexp(&field, &val, 1.0);
                }
                "fuzzy" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[fuzzy] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[fuzzy] query malformed")),
                    };
                    return self.eval_fuzzy(&field, &val, 1.0);
                }
                "match" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[match] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[match] query malformed")),
                    };
                    return self.eval_match(&field, &val, 1.0);
                }
                "match_phrase" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[match_phrase] query malformed")),
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[match_phrase] query malformed")),
                    };
                    return self.eval_match_phrase(&field, &val, 1.0, 0.0);
                }
                "match_phrase_prefix" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => {
                            return Err(ApiError::parsing("[match_phrase_prefix] query malformed"))
                        }
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => {
                            return Err(ApiError::parsing("[match_phrase_prefix] query malformed"))
                        }
                    };
                    let (text, _) = query_text(&val);
                    let qtokens = analyze(&text);
                    if qtokens.is_empty() {
                        return Ok((false, 0.0));
                    }
                    for v in self.must_values(&field) {
                        let s = value_scalar_string(&v).unwrap_or_default();
                        let dtokens = analyze(&s);
                        if dtokens.len() < qtokens.len() {
                            continue;
                        }
                        for i in 0..=(dtokens.len() - qtokens.len()) {
                            let mut ok = true;
                            for (j, qt) in qtokens.iter().enumerate() {
                                if j == qtokens.len() - 1 {
                                    if !dtokens[i + j].starts_with(qt) {
                                        ok = false;
                                    }
                                } else if &dtokens[i + j] != qt {
                                    ok = false;
                                }
                            }
                            if ok {
                                return Ok((true, qtokens.len() as f64));
                            }
                        }
                    }
                    return Ok((false, 0.0));
                }
                "match_bool_prefix" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => {
                            return Err(ApiError::parsing("[match_bool_prefix] query malformed"))
                        }
                    };
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => {
                            return Err(ApiError::parsing("[match_bool_prefix] query malformed"))
                        }
                    };
                    return self.eval_match_bool_prefix(&field, &val, 1.0);
                }
                "multi_match" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[multi_match] query malformed")),
                    };
                    return self.eval_multi_match(bm);
                }
                "bool" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[bool] query malformed")),
                    };
                    return self.eval_bool(bm, 1.0);
                }
                "constant_score" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[constant_score] query malformed")),
                    };
                    return self.eval_constant_score(bm, 1.0);
                }
                "dis_max" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[dis_max] query malformed")),
                    };
                    return self.eval_dis_max(bm, 1.0);
                }
                "boosting" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[boosting] query malformed")),
                    };
                    let positive = match bm.get("positive").and_then(|p| p.as_object()) {
                        Some(p) => p,
                        None => return Err(ApiError::parsing("[boosting] requires 'positive'")),
                    };
                    let (matched, mut sc) = self.eval(positive)?;
                    if !matched {
                        return Ok((false, 0.0));
                    }
                    if let Some(neg_raw) = bm.get("negative").and_then(|n| n.as_object()) {
                        let (neg, _) = self.eval(neg_raw)?;
                        if neg {
                            let mut nf = 0.5;
                            if let Some(v) = bm.get("negative_boost").and_then(to_f64) {
                                nf = v;
                            }
                            sc *= nf;
                        }
                    }
                    return Ok((true, sc));
                }
                "nested" => return self.eval_nested(body),
                "has_child" => return self.eval_has_child(body),
                "has_parent" => return self.eval_has_parent(body),
                "geo_distance" => return self.eval_geo_distance(body),
                "geo_bounding_box" => return self.eval_geo_bounding_box(body),
                "geo_shape" => return self.eval_geo_shape(body),
                "script" => return self.eval_script_query(body),
                "script_score" => return self.eval_script_score(body),
                "function_score" => return self.eval_function_score(body),
                "query_string" => return self.eval_query_string(body, false),
                "simple_query_string" => return self.eval_query_string(body, true),
                "combined_fields" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[combined_fields] query malformed")),
                    };
                    return self.eval_combined_fields(bm);
                }
                "more_like_this" => {
                    let bm = match body.as_object() {
                        Some(m) => m,
                        None => return Err(ApiError::parsing("[more_like_this] query malformed")),
                    };
                    return self.eval_more_like_this(bm);
                }
                "rank_feature" => return self.eval_rank_feature(body),
                "distance_feature" => return self.eval_distance_feature(body),
                "wrapper" => return self.eval_wrapper(body),
                "pinned" => return self.eval_pinned(body),
                "percolate" => return self.eval_percolate(body),
                "terms_set" => return self.eval_terms_set(body),
                "span_term" => return self.eval_span_term(body),
                "span_near" => return self.eval_span_near(body),
                "span_or" => return self.eval_span_or(body),
                "span_first" => return self.eval_span_first(body),
                "span_not" => return self.eval_span_not(body),
                "span_containing" => return self.eval_span_containing(body),
                "span_within" => return self.eval_span_within(body),
                "span_multi" => return self.eval_span_multi(body),
                "intervals" => return self.eval_intervals(body),
                "random_score" => return Ok((true, 1.0)),
                "knn" => return self.eval_knn(body),
                other => return Err(ApiError::parsing(&format!("unknown query [{}]", other))),
            }
        }
        Err(ApiError::parsing("empty query"))
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

pub fn single_entry(m: &Map<String, Value>) -> Option<(&String, &Value)> {
    m.iter().next()
}

pub fn query_text(raw: &Value) -> (String, Option<&Map<String, Value>>) {
    if let Some(m) = raw.as_object() {
        if let Some(q) = m.get("query") {
            return (value_to_string(q).unwrap_or_default(), Some(m));
        }
    }
    (value_to_string(raw).unwrap_or_default(), None)
}

pub fn as_query_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(obj @ Value::Object(_)) => vec![obj.clone()],
        _ => Vec::new(),
    }
}

pub fn is_eval_numeric_type(t: &str) -> bool {
    matches!(
        t,
        "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
            | "scaled_float" | "date"
    )
}

pub fn fuzziness_value(v: &Value, term: &str) -> Option<usize> {
    if let Value::String(t) = v {
        if t.starts_with("AUTO") {
            if t == "AUTO" {
                return Some(auto_fuzziness(term));
            }
            if let Some(rest) = t.strip_prefix("AUTO:") {
                let parts: Vec<&str> = rest.split(',').collect();
                let low: i64 = parts[0].trim().parse().unwrap_or(0);
                let high: i64 = if parts.len() > 1 {
                    parts[1].trim().parse().unwrap_or(0)
                } else {
                    0
                };
                let n = term.chars().count() as i64;
                if n < low {
                    return Some(0);
                }
                if n >= high {
                    return Some(2);
                }
                return Some(1);
            }
        }
    }
    to_f64(v).map(|n| n.max(0.0) as usize)
}

pub fn minimum_should_match(spec: &Value, clause_count: usize, _matched: usize) -> usize {
    let mut required: i64 = 1;
    match spec {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                required = i;
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if let Some(pct) = s.strip_suffix('%') {
                let neg = pct.starts_with('-');
                let pct = pct.trim_start_matches('-');
                let p: f64 = pct.parse().unwrap_or(0.0);
                let v = (p / 100.0 * clause_count as f64) as i64;
                if neg {
                    required = clause_count as i64 - v;
                } else {
                    required = v;
                }
            } else if let Ok(n) = s.parse::<i64>() {
                if n < 0 {
                    required = clause_count as i64 + n;
                } else {
                    required = n;
                }
            }
        }
        _ => {}
    }
    if required < 0 {
        required = 0;
    }
    if required > clause_count as i64 {
        required = clause_count as i64;
    }
    required as usize
}

pub fn contains_subsequence(hay: &[String], needle: &[String]) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.len() > hay.len() {
        return false;
    }
    for i in 0..=(hay.len() - needle.len()) {
        if hay[i..i + needle.len()] == *needle {
            return true;
        }
    }
    false
}

pub fn wildcard_regex(pattern: &str) -> Regex {
    let mut b = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => b.push_str(".*"),
            '?' => b.push('.'),
            other => b.push_str(&regex::escape(&other.to_string())),
        }
    }
    b.push('$');
    Regex::new(&b).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

pub fn valid_cidr(s: &str) -> bool {
    let mut parts = s.splitn(2, '/');
    let net = parts.next().unwrap_or("");
    let len: u32 = match parts.next().and_then(|l| l.parse().ok()) {
        Some(l) => l,
        None => return false,
    };
    let bytes = match ip_to_bytes(net) {
        Some(b) => b,
        None => return false,
    };
    len <= (bytes.len() * 8) as u32
}

pub fn value_float(v: &Value) -> f64 {
    to_f64(v).unwrap_or(0.0)
}

pub fn debug_number(v: &Value) -> String {
    match v {
        Value::Number(n) => format_number(n.as_f64().unwrap_or(0.0)),
        other => preview_value(other),
    }
}

pub fn is_integer_kind(t: &str) -> bool {
    matches!(
        t,
        "byte" | "short" | "integer" | "long" | "unsigned_long" | "integer_range" | "long_range"
    )
}

pub fn btree_to_map(values: BTreeMap<String, Vec<Value>>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in values {
        out.insert(k, Value::Array(v));
    }
    out
}
