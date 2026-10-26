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
    pub scope_path: Option<String>,
    pub scope_obj: Option<Value>,
}

impl<'a> QueryCtx<'a> {
    pub fn new(idx: &'a Index, id: &str, doc: &'a Value) -> Self {
        QueryCtx {
            idx,
            id: id.to_string(),
            doc,
            vals: None,
            scope_path: None,
            scope_obj: None,
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
            scope_path: None,
            scope_obj: None,
        }
    }

    fn raw_values(&self) -> Option<&Map<String, Value>> {
        if let Some(v) = &self.vals {
            return Some(v);
        }
        self.idx.doc_values.get(&self.id)
    }

    fn nested_allowed(&self, resolved: &str) -> bool {
        let np = match nested_ancestor(self.idx, resolved) {
            Some(p) => p,
            None => return true,
        };
        match &self.scope_path {
            Some(sp) => np == *sp,
            None => mapping_field_def(self.idx, &np)
                .map(|d| {
                    d.get("include_in_parent").and_then(|v| v.as_bool()) == Some(true)
                        || d.get("include_in_root").and_then(|v| v.as_bool()) == Some(true)
                })
                .unwrap_or(false),
        }
    }

    pub fn values(&self, field: &str) -> (Vec<Value>, bool) {
        let resolved = resolve_alias_field(self.idx, field);
        if resolved == "_id" {
            return (vec![Value::String(self.id.clone())], true);
        }
        if !self.nested_allowed(&resolved) {
            return (Vec::new(), false);
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
        if !self.nested_allowed(&resolved) {
            return false;
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
    if let Some(d) = idx
        .mappings
        .get("derived")
        .and_then(|m| m.get(field))
        .and_then(|d| d.as_object())
    {
        return Some(d);
    }
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

pub fn nested_ancestor(idx: &Index, field: &str) -> Option<String> {
    let parts: Vec<&str> = field.split('.').collect();
    for i in (1..=parts.len()).rev() {
        let prefix = parts[..i].join(".");
        if let Some(def) = mapping_field_def(idx, &prefix) {
            if def.get("type").and_then(|t| t.as_str()) == Some("nested") {
                return Some(prefix);
            }
        }
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

fn shard_query_error(root_type: &str, reason: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
    let cause = ApiError::new(root_type, reason, 400);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e
}

fn known_query_type(k: &str) -> bool {
    matches!(
        k,
        "match_all" | "match_none" | "term" | "terms" | "ids" | "range" | "exists"
            | "prefix" | "wildcard" | "regexp" | "fuzzy" | "match" | "match_phrase"
            | "match_phrase_prefix" | "match_bool_prefix" | "multi_match" | "bool"
            | "constant_score" | "dis_max" | "boosting" | "nested" | "has_child"
            | "has_parent" | "parent_id" | "script" | "script_score" | "function_score"
            | "query_string" | "simple_query_string" | "combined_fields" | "more_like_this"
            | "rank_feature" | "distance_feature" | "wrapper" | "percolate" | "terms_set"
            | "span_term" | "span_near" | "span_or" | "span_first" | "span_not"
            | "span_containing" | "span_within" | "span_multi" | "intervals"
            | "random_score" | "knn" | "geo_distance" | "geo_bounding_box" | "geo_polygon"
            | "geo_shape"
    )
}

fn query_leaf_field(body: &Value) -> Option<(String, Value)> {
    let m = body.as_object()?;
    for (k, v) in m {
        if k != "boost" && k != "_name" {
            let val = v
                .as_object()
                .and_then(|o| o.get("value").cloned())
                .unwrap_or_else(|| v.clone());
            return Some((k.clone(), val));
        }
    }
    None
}

pub fn validate_query(idx: &Index, q: &Map<String, Value>) -> Result<(), ApiError> {
    for (key, body) in q {
        if !known_query_type(key) {
            let mut e = ApiError::parsing(&format!("unknown query [{}]", key));
            e.caused_by = Some(Box::new(ApiError::new(
                "named_object_not_found_exception",
                &format!("[{}] unknown query", key),
                400,
            )));
            return Err(e);
        }
        match key.as_str() {
            "term" => {
                if let Some((field, val)) = query_leaf_field(body) {
                    let def = mapping_field_def(idx, &field).cloned().unwrap_or_default();
                    let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if def.get("index").and_then(|v| v.as_bool()) == Some(false) {
                        let dv = def.get("doc_values").and_then(|v| v.as_bool());
                        let reason = if dv == Some(false) {
                            format!("failed to create query: Cannot search on field [{}] since it is both not indexed, and does not have doc_values enabled.", field)
                        } else if ftype == "text" {
                            format!("failed to create query: Cannot search on field [{}] since it is not indexed.", field)
                        } else {
                            String::new()
                        };
                        if !reason.is_empty() {
                            return Err(crate::eval_extra::shard_error(
                                400,
                                "query_shard_exception",
                                &reason,
                            ));
                        }
                    }
                    match ftype {
                        "integer" | "long" | "short" | "byte" | "unsigned_long"
                        | "float" | "double" | "half_float" | "scaled_float" => {
                            let bad = match &val {
                                Value::String(s) => s.parse::<f64>().is_err(),
                                Value::Number(_) => false,
                                Value::Bool(_) => true,
                                _ => true,
                            };
                            if bad {
                                return Err(typed_value_error(ftype, &val));
                            }
                        }
                        "date" | "date_nanos" => {
                            if let Ok(()) = parse_query_date(&def, &val, "", false).map(|_| ()) {
                            } else {
                                return Err(typed_value_error(ftype, &val));
                            }
                        }
                        "boolean" => {
                            let ctx = IndexCtx::query_ctx();
                            if ctx.parseBooleanValue("_value", &val).is_err() {
                                return Err(typed_value_error(ftype, &val));
                            }
                        }
                        "ip" => {
                            if let Some(s) = value_scalar_string(&val) {
                                let bad = if s.contains('/') {
                                    crate::fields::canonical_ip(&s).is_err()
                                } else {
                                    s.parse::<std::net::IpAddr>().is_err()
                                };
                                if bad {
                                    return Err(typed_value_error(ftype, &val));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "match" | "match_phrase" | "match_phrase_prefix" | "match_bool_prefix" => {
                if let Some((field, _)) = query_leaf_field(body) {
                    let def = mapping_field_def(idx, &field).cloned().unwrap_or_default();
                    let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if def.get("index").and_then(|v| v.as_bool()) == Some(false) {
                        let dv = def.get("doc_values").and_then(|v| v.as_bool());
                        let reason = if dv == Some(false) {
                            format!("failed to create query: Cannot search on field [{}] since it is both not indexed, and does not have doc_values enabled.", field)
                        } else if ftype == "text" {
                            format!("failed to create query: Cannot search on field [{}] since it is not indexed.", field)
                        } else {
                            String::new()
                        };
                        if !reason.is_empty() {
                            return Err(crate::eval_extra::shard_error(
                                400,
                                "query_shard_exception",
                                &reason,
                            ));
                        }
                    }
                }
            }
            "range" => {
                if let Some(m) = body.as_object() {
                    for (field, spec) in m {
                        if field == "boost" || field == "_name" || field == "relation" {
                            continue;
                        }
                        let def = mapping_field_def(idx, field).cloned().unwrap_or_default();
                        let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let numeric = matches!(
                            ftype,
                            "integer" | "long" | "short" | "byte" | "unsigned_long"
                                | "float" | "double" | "half_float" | "scaled_float"
                        );
                        if numeric {
                            if let Some(bounds) = spec.as_object() {
                                for (k, v) in bounds {
                                    if matches!(k.as_str(), "gte" | "gt" | "lte" | "lt") {
                                        let bad = match v {
                                            Value::String(s) => s.parse::<f64>().is_err(),
                                            Value::Number(_) => false,
                                            Value::Bool(_) => true,
                                            Value::Null => false,
                                            _ => true,
                                        };
                                        if bad {
                                            return Err(typed_value_error(ftype, v));
                                        }
                                    }
                                }
                            }
                        }
                        break;
                    }
                }
            }
            "bool" => {
                if let Some(bm) = body.as_object() {
                    for k in ["must", "should", "filter", "must_not"] {
                        if let Some(cl) = bm.get(k) {
                            let items: Vec<&Value> = match cl {
                                Value::Array(a) => a.iter().collect(),
                                other => vec![other],
                            };
                            for item in items {
                                if let Some(sub) = item.as_object() {
                                    validate_query(idx, sub)?;
                                }
                            }
                        }
                    }
                }
            }
            "constant_score" => {
                if let Some(f) = body.get("filter").and_then(|v| v.as_object()) {
                    validate_query(idx, f)?;
                }
            }
            "function_score" => {
                if let Some(f) = body.get("query").and_then(|v| v.as_object()) {
                    validate_query(idx, f)?;
                }
            }
            "dis_max" => {
                if let Some(arr) = body.get("queries").and_then(|v| v.as_array()) {
                    for item in arr {
                        if let Some(sub) = item.as_object() {
                            validate_query(idx, sub)?;
                        }
                    }
                }
            }
            "nested" | "has_child" | "has_parent" => {
                if let Some(f) = body.get("query").and_then(|v| v.as_object()) {
                    validate_query(idx, f)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn typed_value_error(ftype: &str, v: &Value) -> ApiError {
    let text = value_to_string(v).unwrap_or_else(|| preview_value(v));
    match ftype {
        "date" | "date_nanos" | "date_range" => shard_query_error(
            "parse_exception",
            &format!("failed to parse date field [{}]", text),
        ),
        "boolean" => shard_query_error(
            "query_shard_exception",
            &format!(
                "failed to create query: Can't parse boolean value [{}], expected [true] or [false]",
                text
            ),
        ),
        "ip" | "ip_range" => {
            let reason = if text.contains('/') {
                match crate::fields::canonical_ip(&text) {
                    Err(ones) if ones > 0 => format!(
                        "failed to create query: Illegal prefix length [{}] in [{}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
                        ones, text
                    ),
                    _ => format!(
                        "failed to create query: '{}' is not an IP string literal.",
                        text
                    ),
                }
            } else {
                format!("failed to create query: '{}' is not an IP string literal.", text)
            };
            shard_query_error("query_shard_exception", &reason)
        }
        _ => shard_query_error(
            "query_shard_exception",
            &format!("For input string: \"{}\"", text),
        ),
    }
}

fn multiterm_type_error(qt: &str, field: &str, ftype: &str) -> ApiError {
    shard_query_error(
        "query_shard_exception",
        &format!(
            "Can only use {} queries on keyword and text fields - not on [{}] which is of type [{}]",
            qt, field, ftype
        ),
    )
}

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

pub fn ip_to_16(s: &str) -> Option<[u8; 16]> {
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
    round_up: bool,
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
        let use_math = typ == "date" || s.contains("||") || s.starts_with("now");
        if use_math {
            if let Some(ms) = parse_date_math(def, s, tz, round_up) {
                return Ok(Value::Number(Number::from(if typ == "date_nanos" {
                    ms.saturating_mul(1_000_000)
                } else {
                    ms
                })));
            }
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

fn days_from_civil_pub(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

fn date_floor(ms: i64, unit: char) -> i64 {
    match unit {
        'y' => {
            let (y, _, _) = civil_from_days(ms.div_euclid(86_400_000));
            days_from_civil_pub(y, 1, 1) * 86_400_000
        }
        'M' => {
            let (y, m, _) = civil_from_days(ms.div_euclid(86_400_000));
            days_from_civil_pub(y, m, 1) * 86_400_000
        }
        'w' => {
            let day = ms.div_euclid(86_400_000);
            let weekday = (day + 4).rem_euclid(7);
            (day - weekday) * 86_400_000
        }
        'd' => ms.div_euclid(86_400_000) * 86_400_000,
        'h' => ms.div_euclid(3_600_000) * 3_600_000,
        'm' => ms.div_euclid(60_000) * 60_000,
        's' => ms.div_euclid(1000) * 1000,
        _ => ms,
    }
}

fn date_unit_ms(unit: char) -> Option<i64> {
    match unit {
        'y' => Some(365 * 86_400_000),
        'M' => Some(30 * 86_400_000),
        'w' => Some(7 * 86_400_000),
        'd' => Some(86_400_000),
        'h' => Some(3_600_000),
        'm' => Some(60_000),
        's' => Some(1000),
        _ => None,
    }
}

fn add_date_math(ms: i64, n: i64, unit: char) -> i64 {
    match unit {
        'y' | 'M' => {
            let (mut y, mut mo, d) = civil_from_days(ms.div_euclid(86_400_000));
            let rem = ms.rem_euclid(86_400_000);
            if unit == 'y' {
                y += n;
            } else {
                mo += n;
                while mo > 12 {
                    mo -= 12;
                    y += 1;
                }
                while mo < 1 {
                    mo += 12;
                    y -= 1;
                }
            }
            days_from_civil_pub(y, mo, d) * 86_400_000 + rem
        }
        _ => {
            let mult = date_unit_ms(unit).unwrap_or(0);
            ms + n * mult
        }
    }
}

fn parse_date_math(def: &Map<String, Value>, raw: &str, tz: &str, round_up: bool) -> Option<i64> {
    let (base, math) = if let Some(i) = raw.find("||") {
        (&raw[..i], Some(&raw[i + 2..]))
    } else if raw.starts_with("now") && raw.len() > 3 {
        ("now", Some(&raw[3..]))
    } else {
        (raw, None)
    };
    let date_only = !base.contains('T') && !base.contains(' ')
        && !math.map(|m| m.contains('/')).unwrap_or(false);
    let explicit_offset = base.ends_with('Z')
        || base.rfind(['+', '-']).map(|i| i > 10).unwrap_or(false);
    let mut ms = if base == "now" {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64
    } else {
        crate::fields::parse_date_any(def, base)?
    };
    if let Some(math) = math {
        let mut rest = math;
        while !rest.is_empty() {
            let mut chars = rest.char_indices();
            let sign = match chars.next() {
                Some((_, '+')) => 1i64,
                Some((_, '-')) => -1i64,
                Some((_, '/')) => {
                    let unit = rest.chars().nth(1)?;
                    ms = date_floor(ms, unit);
                    if round_up {
                        if let Some(step) = date_unit_ms(unit) {
                            if matches!(unit, 'y' | 'M') {
                                let (y, mo, _) = civil_from_days(ms.div_euclid(86_400_000));
                                let next = if unit == 'y' {
                                    days_from_civil_pub(y + 1, 1, 1)
                                } else if mo == 12 {
                                    days_from_civil_pub(y + 1, 1, 1)
                                } else {
                                    days_from_civil_pub(y, mo + 1, 1)
                                };
                                ms = next * 86_400_000 - 1;
                            } else {
                                ms += step - 1;
                            }
                        }
                    }
                    rest = &rest[2..];
                    continue;
                }
                _ => return None,
            };
            let mut end = 1usize;
            for (i, c) in rest.char_indices().skip(1) {
                if c.is_ascii_digit() {
                    end = i + 1;
                } else {
                    break;
                }
            }
            if end == 1 {
                return None;
            }
            let n: i64 = rest[1..end].parse().ok()?;
            let unit = rest[end..].chars().next()?;
            ms = add_date_math(ms, sign * n, unit);
            rest = &rest[end + unit.len_utf8()..];
        }
    }
    if date_only && round_up {
        ms += 86_400_000 - 1;
    }
    if !tz.is_empty() && !explicit_offset {
        if let Ok(offset) = parse_tz_offset(tz) {
            ms -= offset;
        }
    }
    Some(ms)
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
        if let (Some(x), Some(y)) = (ip_to_16(sa), ip_to_16(sb)) {
            return match x.cmp(&y) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
        }
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
    lower_inclusive: bool,
    upper_inclusive: bool,
    stored: &Map<String, Value>,
) -> bool {
    let (mut s_lower, mut s_has_lower, mut s_lower_inc) = match stored.get("gte") {
        Some(l) => (l.clone(), true, true),
        None => match stored.get("gt") {
            Some(l) => (l.clone(), true, false),
            None => (Value::Null, false, true),
        },
    };
    let (mut s_upper, mut s_has_upper, mut s_upper_inc) = match stored.get("lte") {
        Some(u) => (u.clone(), true, true),
        None => match stored.get("lt") {
            Some(u) => (u.clone(), true, false),
            None => (Value::Null, false, true),
        },
    };
    if !s_has_lower {
        s_lower = Value::Null;
    }
    if !s_has_upper {
        s_upper = Value::Null;
    }
    let _ = &mut s_lower;
    let _ = &mut s_upper;
    let _ = &mut s_lower_inc;
    let _ = &mut s_upper_inc;
    let low_cmp = if has_lower && s_has_lower {
        Some(compare_any(&s_lower, q_lower))
    } else {
        None
    };
    let up_cmp = if has_upper && s_has_upper {
        Some(compare_any(&s_upper, q_upper))
    } else {
        None
    };
    let intersects = {
        let (lower_val, lower_inc) = if has_lower && s_has_lower {
            match compare_any(&s_lower, q_lower) {
                c if c > 0 => (s_lower.clone(), s_lower_inc),
                c if c < 0 => (q_lower.clone(), lower_inclusive),
                _ => (q_lower.clone(), lower_inclusive && s_lower_inc),
            }
        } else if has_lower {
            (q_lower.clone(), lower_inclusive)
        } else if s_has_lower {
            (s_lower.clone(), s_lower_inc)
        } else {
            (Value::Null, true)
        };
        let (upper_val, upper_inc) = if has_upper && s_has_upper {
            match compare_any(&s_upper, q_upper) {
                c if c < 0 => (s_upper.clone(), s_upper_inc),
                c if c > 0 => (q_upper.clone(), upper_inclusive),
                _ => (q_upper.clone(), upper_inclusive && s_upper_inc),
            }
        } else if has_upper {
            (q_upper.clone(), upper_inclusive)
        } else if s_has_upper {
            (s_upper.clone(), s_upper_inc)
        } else {
            (Value::Null, true)
        };
        let has_l = has_lower || s_has_lower;
        let has_u = has_upper || s_has_upper;
        if has_l && has_u {
            match compare_any(&lower_val, &upper_val) {
                c if c > 0 => false,
                c if c == 0 => lower_inc && upper_inc,
                _ => true,
            }
        } else {
            true
        }
    };
    match relation.to_uppercase().as_str() {
        "" | "INTERSECTS" => intersects,
        "CONTAINS" => {
            if (!has_lower && s_has_lower) || (!has_upper && s_has_upper) {
                return false;
            }
            if let Some(c) = low_cmp {
                if c > 0 || (c == 0 && !s_lower_inc && lower_inclusive) {
                    return false;
                }
            }
            if let Some(c) = up_cmp {
                if c < 0 || (c == 0 && !s_upper_inc && upper_inclusive) {
                    return false;
                }
            }
            true
        }
        "WITHIN" => {
            if (has_lower && !s_has_lower) || (has_upper && !s_has_upper) {
                return false;
            }
            if let Some(c) = low_cmp {
                if c < 0 || (c == 0 && !lower_inclusive && s_lower_inc) {
                    return false;
                }
            }
            if let Some(c) = up_cmp {
                if c > 0 || (c == 0 && !upper_inclusive && s_upper_inc) {
                    return false;
                }
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
        let term_ftype = mapping_field_def(self.idx, field)
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if term_ftype == "binary" {
            return Err(shard_query_error(
                "query_shard_exception",
                "Binary fields do not support searching",
            ));
        }
        let mut val = raw.clone();
        let mut fuzz = 0usize;
        let mut case_insensitive = false;
        if let Some(m) = raw.as_object() {
            for k in m.keys() {
                if !matches!(
                    k.as_str(),
                    "value" | "boost" | "case_insensitive" | "fuzziness" | "_name"
                ) {
                    return Err(ApiError::parsing(&format!(
                        "[term] query does not support [{}]",
                        k
                    )));
                }
            }
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(f) = m.get("fuzziness") {
                if let Some(n) = fuzziness_value(f, &preview_value(&val)) {
                    fuzz = n;
                }
            }
            if let Some(ci) = m.get("case_insensitive") {
                match ci.as_bool() {
                    Some(b) => case_insensitive = b,
                    None => {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            &format!(
                                "Failed to parse value [{}] as only [true] or [false] are allowed.",
                                preview_value(ci)
                            ),
                            400,
                        ))
                    }
                }
            }
        }
        let mut boost = boost;
        if let Some(m) = raw.as_object() {
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
        }
        val = crate::fields::normalize_query_value(self.idx, field, &val);
        if let Some((base, name)) = field.split_once('#') {
            let bdef = mapping_field_def(self.idx, base).cloned().unwrap_or_default();
            if bdef.get("type").and_then(|t| t.as_str()) == Some("join") {
                let want = value_scalar_string(&val).unwrap_or_default();
                if self.id == want {
                    let (jname, _) = crate::eval_extra::join_parts(self.doc.get(base));
                    if jname == name {
                        return Ok((true, boost));
                    }
                }
                if self.idx.routing.get(&self.id).map(|r| r.as_str()) == Some(want.as_str()) {
                    let (jname, _) = crate::eval_extra::join_parts(self.doc.get(base));
                    if jname != name {
                        return Ok((true, boost));
                    }
                }
                return Ok((false, 0.0));
            }
        }
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(ftype.as_str(), "integer" | "long" | "short" | "byte" | "unsigned_long") {
            if let Some(f) = val.as_f64() {
                if f.fract() != 0.0 {
                    return Ok((false, 0.0));
                }
            }
        }
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
                let want = if case_insensitive {
                    term.to_lowercase()
                } else {
                    term.clone()
                };
                for v in &vals {
                    let s = value_scalar_string(v).unwrap_or_default();
                    for tok in self.analyze_field(field, None, &s) {
                        if tok == want {
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
                let want = if case_insensitive {
                    term.to_lowercase()
                } else {
                    term.clone()
                };
                for v in &vals {
                    if let Some(s) = value_scalar_string(v) {
                        let have = if case_insensitive {
                            s.to_lowercase()
                        } else {
                            s
                        };
                        if have == want {
                            return Ok((true, boost));
                        }
                    }
                }
                Ok((false, 0.0))
            }
            "completion" => Ok((false, 0.0)),
            "rank_feature" | "rank_features" => {
                return Err(shard_query_error(
                    "illegal_argument_exception",
                    "failed to create query: Queries on [rank_feature] fields are not supported",
                ))
            }
            "token_count" => {
                let parsed = parse_term_value(&ftype, &def, &val)
                    .map_err(|_| typed_value_error(&ftype, &val))?;
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
                    let reason = if term.contains('/') {
                        match crate::fields::canonical_ip(&term) {
                            Err(ones) if ones > 0 => format!(
                                "failed to create query: Illegal prefix length [{}] in [{}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
                                ones, term
                            ),
                            _ => format!(
                                "failed to create query: '{}' is not an IP string literal.",
                                term
                            ),
                        }
                    } else {
                        format!("failed to create query: '{}' is not an IP string literal.", term)
                    };
                    return Err(shard_query_error("query_shard_exception", &reason));
                }
                for v in &vals {
                    let s = value_to_string(v).unwrap_or_default();
                    if ip_contains(&s, &term) {
                        return Ok((true, boost));
                    }
                }
                Ok((false, 0.0))
            }
            "date" | "date_nanos" => {
                if let Value::String(sv) = &val {
                    if !sv.contains('T') && !sv.contains(' ') {
                        let start = parse_query_date(&def, &val, "", false)?;
                        let end = parse_query_date(&def, &val, "", true)?;
                        for v in &vals {
                            if numeric_less(v, &start) >= 0 && numeric_less(v, &end) <= 0 {
                                return Ok((true, boost));
                            }
                        }
                        return Ok((false, 0.0));
                    }
                }
                let parsed = parse_term_value(&ftype, &def, &val)
                    .map_err(|_| typed_value_error(&ftype, &val))?;
                for v in &vals {
                    for p in &parsed {
                        if numeric_equal(v, p) {
                            return Ok((true, boost));
                        }
                    }
                }
                Ok((false, 0.0))
            }
            "boolean" => {
                if matches!(&val, Value::String(s) if s.is_empty()) {
                    return Err(typed_value_error(&ftype, &val));
                }
                let parsed = match parse_term_value(&ftype, &def, &val) {
                    Ok(p) => p,
                    Err(_) => return Err(typed_value_error(&ftype, &val)),
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
                let parsed = parse_term_value(&ftype, &def, &val)
                    .map_err(|_| typed_value_error(&ftype, &val))?;
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
        if raw.contains_key("case_insensitive") {
            return Err(ApiError::parsing(
                "[terms] query does not support [case_insensitive]",
            ));
        }
        if let Some(b) = raw.get("boost").and_then(to_f64) {
            boost *= b;
        }
        let list = match list_raw.as_array() {
            Some(l) => l,
            None => {
                if list_raw.get("index").is_some() && list_raw.get("id").is_some() {
                    return Ok((false, 0.0));
                }
                return Err(ApiError::parsing(&format!(
                    "[terms] query does not support [{}]",
                    field
                )));
            }
        };
        if let Some(v) = find_setting(&self.idx.settings, "max_terms_count") {
            if let Some(max) = settings_value_int(v) {
                if list.len() as i64 > max {
                    return Err(shard_query_error(
                        "query_shard_exception",
                        &format!(
                            "The number of terms [{}] used in the Terms Query request has exceeded the allowed maximum of [{}]",
                            list.len(),
                            max
                        ),
                    ));
                }
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
        for item in list {
            let mut matched = false;
            if item.is_object() {
                return Err(ApiError::parsing("[terms] query does not support map values"));
            }
            if matches!(
                ftype.as_str(),
                "integer" | "long" | "short" | "byte" | "unsigned_long"
            ) {
                if let Some(f) = item.as_f64() {
                    if f.fract() != 0.0 {
                        continue;
                    }
                }
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
                    let norm_item = crate::fields::normalize_query_value(self.idx, field, item);
                    let term = value_scalar_string(&norm_item).unwrap_or_default();
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
        let mut def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        if let Some(f) = cond.get("format").and_then(|v| v.as_str()) {
            def.insert("format".to_string(), Value::String(f.to_string()));
        }
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if ftype == "wildcard" {
            return Err(shard_query_error(
                "query_shard_exception",
                "failed to create query: TODO",
            ));
        }
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
        let parse_bound = |v: &Value, round_up: bool| -> Result<Value, ApiError> {
            let res = match ftype.as_str() {
                "date" | "date_nanos" | "date_range" => {
                    parse_query_date(&def, v, &time_zone, round_up)
                }
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
                        if v.is_number() {
                            Ok(v.clone())
                        } else {
                            ctx.parseIntValue(field, base, &def, v)
                        }
                    } else if matches!(base, "float" | "double" | "half_float" | "scaled_float") {
                        let raw = match v {
                            Value::Number(n) => n.as_f64(),
                            Value::String(s) => s.trim().parse::<f64>().ok(),
                            _ => None,
                        };
                        match raw {
                            Some(f) if base == "float" => Ok(Value::Number(
                                Number::from_f64(f as f32 as f64).unwrap(),
                            )),
                            Some(f) => Ok(Value::Number(
                                Number::from_f64(f).unwrap_or_else(|| Number::from(0)),
                            )),
                            None => Err(typed_value_error(&ftype, v)),
                        }
                    } else {
                        ctx.parseFloatValue(field, base, &def, v)
                    }
                }
                _ => Ok(v.clone()),
            };
            res.map_err(|_| typed_value_error(&ftype, v))
        };
        let lower = cond
            .get("gte")
            .filter(|v| !v.is_null())
            .map(|v| (v.clone(), true))
            .or_else(|| {
                cond.get("gt")
                    .filter(|v| !v.is_null())
                    .map(|v| (v.clone(), false))
            })
            .or_else(|| {
                cond.get("from").filter(|v| !v.is_null()).map(|v| {
                    (
                        v.clone(),
                        cond.get("include_lower")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(true),
                    )
                })
            });
        let upper = cond
            .get("lte")
            .filter(|v| !v.is_null())
            .map(|v| (v.clone(), true))
            .or_else(|| {
                cond.get("lt")
                    .filter(|v| !v.is_null())
                    .map(|v| (v.clone(), false))
            })
            .or_else(|| {
                cond.get("to").filter(|v| !v.is_null()).map(|v| {
                    (
                        v.clone(),
                        cond.get("include_upper")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(true),
                    )
                })
            });
        let q_lower = match &lower {
            Some((v, inclusive)) => Some(parse_bound(v, !inclusive)?),
            None => None,
        };
        let q_upper = match &upper {
            Some((v, inclusive)) => Some(parse_bound(v, *inclusive)?),
            None => None,
        };
        let is_range_field = matches!(
            ftype.as_str(),
            "integer_range" | "long_range" | "float_range" | "double_range" | "date_range"
                | "ip_range"
        );
        if is_range_field {
            for v in &vals {
                if let Value::Object(rng) = v {
                    let mut stored = rng.clone();
                    if ftype == "date_range" {
                        for key in ["gte", "gt", "lte", "lt"] {
                            if let Some(b) = rng.get(key).cloned() {
                                if let Ok(parsed) =
                                    parse_query_date(&def, &b, "", false)
                                {
                                    stored.insert(key.to_string(), parsed);
                                }
                            }
                        }
                    }
                    if range_relation(
                        &relation,
                        q_lower.as_ref().unwrap_or(&Value::Null),
                        q_upper.as_ref().unwrap_or(&Value::Null),
                        q_lower.is_some(),
                        q_upper.is_some(),
                        lower.as_ref().map(|(_, i)| *i).unwrap_or(true),
                        upper.as_ref().map(|(_, i)| *i).unwrap_or(true),
                        &stored,
                    ) {
                        return Ok((true, boost));
                    }
                }
            }
            return Ok((false, 0.0));
        }
        for v in &vals {
            if let Value::Object(rng) = v {
                if let Some(q) = &q_lower {
                    if !in_range_field(rng, q) {
                        continue;
                    }
                }
                if let Some(q) = &q_upper {
                    if !in_range_field(rng, q) {
                        continue;
                    }
                }
                return Ok((true, boost));
            }
            let candidates: Vec<Value> = if ftype == "text" || ftype == "match_only_text" {
                analyze(&value_to_string(v).unwrap_or_default())
                    .into_iter()
                    .map(Value::String)
                    .collect()
            } else {
                vec![v.clone()]
            };
            for cand in &candidates {
                let mut ok = true;
                if let Some((_, inclusive)) = &lower {
                    let bv = q_lower.as_ref().unwrap();
                    let cmp = self.range_cmp(&ftype, cand, bv);
                    if (*inclusive && cmp < 0) || (!*inclusive && cmp <= 0) {
                        ok = false;
                    }
                }
                if ok {
                    if let Some((_, inclusive)) = &upper {
                        let bv = q_upper.as_ref().unwrap();
                        let cmp = self.range_cmp(&ftype, cand, bv);
                        if (*inclusive && cmp > 0) || (!*inclusive && cmp >= 0) {
                            ok = false;
                        }
                    }
                }
                if ok {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    fn range_cmp(&self, ftype: &str, v: &Value, bv: &Value) -> i32 {
        if ftype == "ip" {
            let vs = value_to_string(v).unwrap_or_default();
            let bs = value_to_string(bv).unwrap_or_default();
            compare_ip_literal(&vs, &bs)
        } else if ftype == "text"
            || ftype == "keyword"
            || ftype == "match_only_text"
            || ftype == "wildcard"
            || ftype.is_empty()
        {
            let vs = value_to_string(v).unwrap_or_default();
            let bs = value_to_string(bv).unwrap_or_default();
            match vs.cmp(&bs) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        } else {
            numeric_less(v, bv)
        }
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
        let derived_field = self
            .idx
            .mappings
            .get("derived")
            .and_then(|d| d.as_object())
            .map(|d| d.contains_key(&field))
            .unwrap_or(false);
        if let Some(def) = mapping_field_def(self.idx, &field) {
            if derived_field || def.get("type").and_then(|v| v.as_str()) == Some("derived") {
                return Err(shard_query_error(
                    "illegal_argument_exception",
                    &format!(
                        "failed to create query: Field [{}] of type [derived] does not support exist queries",
                        field
                    ),
                ));
            }
            if def.get("index").and_then(|v| v.as_bool()) == Some(false)
                && def.get("doc_values").and_then(|v| v.as_bool()) == Some(false)
            {
                return Ok((false, 0.0));
            }
            if def.get("type").and_then(|v| v.as_str()) == Some("binary")
                && def.get("doc_values").and_then(|v| v.as_bool()) != Some(true)
            {
                return Ok((false, 0.0));
            }
        }
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
        let mut case_insensitive = false;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value") {
                val = v.clone();
            }
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
            if let Some(ci) = m.get("case_insensitive").and_then(|v| v.as_bool()) {
                case_insensitive = ci;
            }
        }
        let nval = crate::fields::normalize_query_value(self.idx, field, &val);
        let prefix = match value_scalar_string(&nval) {
            Some(p) => {
                if case_insensitive {
                    p.to_lowercase()
                } else {
                    p
                }
            }
            None => return Ok((false, 0.0)),
        };
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !ftype.is_empty()
            && ftype != "text"
            && ftype != "keyword"
            && ftype != "constant_keyword"
            && ftype != "wildcard"
            && ftype != "match_only_text"
            && ftype != "search_as_you_type"
            && ftype != "flat_object"
        {
            return Err(multiterm_type_error("prefix", field, &ftype));
        }
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            if ftype == "text" || ftype == "match_only_text" || ftype == "search_as_you_type" {
                for tok in analyze(&s) {
                    let have = if case_insensitive {
                        tok.to_lowercase()
                    } else {
                        tok
                    };
                    if have.starts_with(&prefix) {
                        return Ok((true, boost));
                    }
                }
            } else {
                let have = if case_insensitive {
                    s.to_lowercase()
                } else {
                    s
                };
                if have.starts_with(&prefix) {
                    return Ok((true, boost));
                }
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
        let mut case_insensitive = false;
        if let Some(m) = raw.as_object() {
            if let Some(v) = m.get("value").or_else(|| m.get("wildcard")) {
                val = v.clone();
            }
            if let Some(b) = m.get("boost").and_then(to_f64) {
                boost *= b;
            }
            if let Some(ci) = m.get("case_insensitive").and_then(|v| v.as_bool()) {
                case_insensitive = ci;
            }
        }
        let nval = crate::fields::normalize_query_value(self.idx, field, &val);
        let pattern = match value_scalar_string(&nval) {
            Some(p) => {
                if case_insensitive {
                    p.to_lowercase()
                } else {
                    p
                }
            }
            None => return Ok((false, 0.0)),
        };
        let re = wildcard_regex(&pattern);
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !ftype.is_empty()
            && ftype != "text"
            && ftype != "keyword"
            && ftype != "constant_keyword"
            && ftype != "wildcard"
            && ftype != "match_only_text"
            && ftype != "search_as_you_type"
            && ftype != "flat_object"
        {
            return Err(multiterm_type_error("wildcard", field, &ftype));
        }
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
                let have = if case_insensitive {
                    cand.to_lowercase()
                } else {
                    cand
                };
                if re.is_match(&have) {
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
        let mut flags = "ALL".to_string();
        let mut case_insensitive = false;
        if let Some(m) = raw.as_object() {
            if let Some(f) = m.get("flags").and_then(|v| v.as_str()) {
                flags = f.to_uppercase();
            }
            if let Some(ci) = m.get("case_insensitive").and_then(|v| v.as_bool()) {
                case_insensitive = ci;
            }
        }
        let mut pattern = match value_scalar_string(&val) {
            Some(p) => p,
            None => return Ok((false, 0.0)),
        };
        if case_insensitive {
            pattern = pattern.to_lowercase();
        }
        let complement = flags.contains("COMPLEMENT")
            && pattern.starts_with("~(")
            && pattern.ends_with(')');
        if complement {
            pattern = pattern[2..pattern.len() - 1].to_string();
        }
        let use_at = flags.contains("ANYSTRING") || (flags.contains("ALL") && !flags.contains("NONE"));
        let use_interval = flags.contains("INTERVAL") || (flags.contains("ALL") && !flags.contains("NONE"));
        if use_at {
            pattern = replace_unescaped_at(&pattern);
        }
        if use_interval {
            pattern = expand_intervals(&pattern);
        }
        let parts: Vec<&str> = if flags.contains("INTERSECTION") {
            pattern.split('&').collect()
        } else {
            vec![pattern.as_str()]
        };
        let mut regexes = Vec::new();
        for part in parts {
            let whole = format!("^(?:{})$", part);
            match Regex::new(&whole) {
                Ok(r) => regexes.push(r),
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
            }
        }
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !ftype.is_empty()
            && ftype != "text"
            && ftype != "keyword"
            && ftype != "constant_keyword"
            && ftype != "wildcard"
            && ftype != "match_only_text"
            && ftype != "search_as_you_type"
            && ftype != "flat_object"
        {
            return Err(multiterm_type_error("regexp", field, &ftype));
        }
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
                let have = if case_insensitive {
                    cand.to_lowercase()
                } else {
                    cand
                };
                let mut hit = regexes.iter().all(|re| re.is_match(&have));
                if complement {
                    hit = !hit;
                }
                if hit {
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
        let owned;
        let m = if let Some(m) = raw.as_object() {
            m
        } else {
            let mut mm = Map::new();
            mm.insert("value".to_string(), raw.clone());
            owned = mm;
            &owned
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
        let transpositions = m
            .get("transpositions")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let def = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if ftype == "wildcard" {
            return Err(shard_query_error(
                "query_shard_exception",
                &format!(
                    "failed to create query: Can only use fuzzy queries on keyword and text fields - not on [{}] which is of type [wildcard]",
                    field
                ),
            ));
        }
        if !ftype.is_empty()
            && ftype != "text"
            && ftype != "keyword"
            && ftype != "constant_keyword"
            && ftype != "match_only_text"
            && ftype != "search_as_you_type"
            && ftype != "flat_object"
        {
            return Err(multiterm_type_error("fuzzy", field, &ftype));
        }
        let (vals, present) = self.values(field);
        if !present {
            return Ok((false, 0.0));
        }
        let mut allowed: Option<std::collections::HashSet<String>> = None;
        if max_expansions > 0 {
            let mut all_terms: Vec<String> = Vec::new();
            for id in &self.idx.order {
                if let Some(dv) = self.idx.doc_values.get(id) {
                    if let Some(raw) = dv.get(&resolve_alias_field(self.idx, field)) {
                        for v in array_values(raw) {
                            let s = value_scalar_string(&v).unwrap_or_default();
                            if ftype == "text" || ftype == "match_only_text" {
                                all_terms.extend(analyze(&s));
                            } else {
                                all_terms.push(s);
                            }
                        }
                    }
                }
            }
            all_terms.sort();
            all_terms.dedup();
            let chosen: std::collections::HashSet<String> = all_terms
                .into_iter()
                .filter(|cand| {
                    cand != &term
                        && cand.len() > prefix_length
                        && term.len() > prefix_length
                        && cand.as_bytes()[..prefix_length] == term.as_bytes()[..prefix_length]
                        && fuzzy_token_match(&term, cand, fuzz, prefix_length, transpositions)
                })
                .take(max_expansions)
                .collect();
            allowed = Some(chosen);
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
                let is_match = if let Some(ref set) = allowed {
                    set.contains(&cand)
                } else {
                    fuzzy_token_match(&term, &cand, fuzz, prefix_length, transpositions)
                };
                if is_match {
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
        let mut toks = analyze_with(&a, text);
        if a == "english" {
            toks = toks.into_iter().map(|t| english_stem(&t)).collect();
        }
        if self.analyzer_folds(&a) {
            toks = toks.into_iter().map(|t| asciifold(&t)).collect();
        }
        toks
    }

    fn analyzer_folds(&self, analyzer: &str) -> bool {
        let key = format!("analysis.analyzer.{}.filter", analyzer);
        match find_setting(&self.idx.settings, &key) {
            Some(Value::Array(a)) => a
                .iter()
                .any(|f| f.as_str() == Some("asciifolding")),
            Some(Value::String(s)) => s.contains("asciifolding"),
            _ => false,
        }
    }

    fn synonym_match(
        &self,
        field: &str,
        qtokens: &[String],
        auto_phrase: bool,
    ) -> Option<(bool, f64)> {
        let groups = synonym_groups(self.idx, field);
        if groups.is_empty() {
            return None;
        }
        let mut matched: Vec<&Vec<Vec<String>>> = Vec::new();
        for g in &groups {
            if g.iter().any(|alt| alt.as_slice() == qtokens) {
                matched.push(g);
            }
        }
        if matched.is_empty() {
            return None;
        }
        let mut doc_tokens: Vec<String> = Vec::new();
        let mut doc_pos: Vec<(String, usize)> = Vec::new();
        for v in self.must_values(field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            doc_tokens.extend(self.analyze_field(field, None, &s));
            doc_pos.extend(analyze_positions("", &s));
        }
        if auto_phrase {
            for g in matched {
                for alt in g {
                    let ok = if alt.len() == 1 {
                        doc_tokens.iter().any(|t| t == &alt[0])
                    } else {
                        let q: Vec<(String, usize)> = alt
                            .iter()
                            .enumerate()
                            .map(|(i, t)| (t.clone(), i))
                            .collect();
                        phrase_positions_match(&q, &doc_pos, 0)
                    };
                    if ok {
                        return Some((true, 1.0));
                    }
                }
            }
        } else {
            let mut any: Vec<String> = Vec::new();
            for g in matched {
                for alt in g {
                    any.extend(alt.iter().cloned());
                }
            }
            if doc_tokens.iter().any(|t| any.contains(t)) {
                return Some((true, 1.0));
            }
        }
        Some((false, 0.0))
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
            let all = opts
                .and_then(|m| m.get("zero_terms_query"))
                .and_then(|v| v.as_str())
                .map(|v| v.eq_ignore_ascii_case("all"))
                .unwrap_or(false);
            let an = self.analyzer_name(field, opts);
            if all && an == "stop" && analyze_positions(&an, &text).is_empty() {
                return Ok((true, boost));
            }
            return Ok((false, 0.0));
        }
        if is_eval_numeric_type(&ftype)
            || ftype == "boolean"
            || ftype == "date"
            || ftype == "date_nanos"
            || ftype == "ip"
        {
            let lenient = opts
                .and_then(|m| m.get("lenient"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if opts.and_then(|m| m.get("fuzziness")).is_some() {
                let reason = format!(
                    "Can only use fuzzy queries on keyword and text fields - not on [{}] which is of type [{}]",
                    field, ftype
                );
                let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
                e.root_cause = Some(Box::new(ApiError::new(
                    "query_shard_exception",
                    &reason,
                    400,
                )));
                return Err(e);
            }
            let parsed = match parse_term_value(&ftype, &def, &Value::String(text.clone())) {
                Ok(p) => p,
                Err(_) => {
                    if lenient {
                        return Ok((false, 0.0));
                    }
                    let (root_type, reason) = if ftype == "date" || ftype == "date_nanos" {
                        (
                            "parse_exception",
                            format!("failed to parse date field [{}]", text),
                        )
                    } else if ftype == "boolean" {
                        (
                            "query_shard_exception",
                            format!("Can't parse boolean value [{}]", text),
                        )
                    } else {
                        (
                            "query_shard_exception",
                            format!("number_format_exception: For input string: \"{}\"", text),
                        )
                    };
                    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
                    e.root_cause = Some(Box::new(ApiError::new(root_type, &reason, 400)));
                    return Err(e);
                }
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
            || (ftype.is_empty() && crate::fields::flat_object_root(self.idx, field).is_some())
        {
            let text = value_scalar_string(&crate::fields::normalize_query_value(
                self.idx,
                field,
                &Value::String(text.clone()),
            ))
            .unwrap_or_default();
            let split = def
                .get("split_queries_on_whitespace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if split {
                let tokens: Vec<String> = text
                    .split_whitespace()
                    .map(|t| t.to_string())
                    .collect();
                if tokens.is_empty() {
                    return Ok((false, 0.0));
                }
                let required = if operator == "and" { tokens.len() } else { 1 };
                let mut matched = 0usize;
                for tok in &tokens {
                    let norm = value_scalar_string(&crate::fields::normalize_query_value(
                        self.idx,
                        field,
                        &Value::String(tok.clone()),
                    ))
                    .unwrap_or_default();
                    if vals
                        .iter()
                        .any(|v| value_scalar_string(v).unwrap_or_default() == norm)
                    {
                        matched += 1;
                    }
                }
                if matched >= required {
                    return Ok((true, boost));
                }
                return Ok((false, 0.0));
            }
            for v in &vals {
                let s = value_scalar_string(v).unwrap_or_default();
                if s == text {
                    return Ok((true, boost));
                }
            }
            return Ok((false, 0.0));
        }
        let qan = self.analyzer_name(field, opts);
        let qtokens = if qan == "stop" {
            analyze_positions(&qan, &text)
                .into_iter()
                .map(|(t, _)| t)
                .collect::<Vec<_>>()
        } else {
            self.analyze_field(field, opts, &text)
        };
        if qtokens.is_empty() {
            let all = opts
                .and_then(|m| m.get("zero_terms_query"))
                .and_then(|v| v.as_str())
                .map(|v| v.eq_ignore_ascii_case("all"))
                .unwrap_or(false);
            if all {
                return Ok((true, boost));
            }
            return Ok((false, 0.0));
        }
        let auto_syn = opts
            .and_then(|m| m.get("auto_generate_synonyms_phrase_query"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if let Some(r) = self.synonym_match(field, &qtokens, auto_syn) {
            return Ok(r);
        }
        let fuzz_spec = opts.and_then(|m| m.get("fuzziness"));
        let mut fuzz = Fuzz::None;
        if let Some(spec) = fuzz_spec {
            match parse_fuzziness(spec) {
                Ok(n) => fuzz = n,
                Err(bad) => {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Invalid fuzziness value: {}", bad),
                        400,
                    ));
                }
            }
        }
        let prefix_length = opts
            .and_then(|m| m.get("prefix_length"))
            .and_then(|v| to_f64(v))
            .unwrap_or(0.0) as usize;
        let transpositions = opts
            .and_then(|m| m.get("fuzzy_transpositions"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut required = 1usize;
        if qtokens.len() > 1 {
            if let Some(spec) = opts.and_then(|m| m.get("minimum_should_match")) {
                match minimum_should_match_checked(spec, qtokens.len()) {
                    Ok(n) => required = n,
                    Err(bad) => {
                        let reason = format!("failed to create query: For input string: \"{}\"", bad);
                        let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
                        e.root_cause = Some(Box::new(ApiError::new(
                            "query_shard_exception",
                            &reason,
                            400,
                        )));
                        return Err(e);
                    }
                }
            } else if operator == "and" {
                required = qtokens.len();
            }
        }
        let dan = self.analyzer_name(field, None);
        let doc_tokens = |s: &str| -> Vec<String> {
            if dan == "stop" {
                analyze_positions(&dan, s)
                    .into_iter()
                    .map(|(t, _)| t)
                    .collect()
            } else {
                self.analyze_field(field, None, s)
            }
        };
        let mut cur_tokens: Vec<String> = Vec::new();
        for v in &vals {
            let s = value_scalar_string(v).unwrap_or_default();
            cur_tokens.extend(doc_tokens(&s));
        }
        let dl = cur_tokens.len() as f64;
        let mut total_docs = 0usize;
        let mut total_len = 0usize;
        let mut dfs = vec![0usize; qtokens.len()];
        for id in &self.idx.order {
            let mut toks: Vec<String> = Vec::new();
            if let Some(dv) = self.idx.doc_values.get(id) {
                if let Some(raw) = dv.get(&resolve_alias_field(self.idx, field)) {
                    for v in array_values(raw) {
                        let s = value_scalar_string(&v).unwrap_or_default();
                        toks.extend(doc_tokens(&s));
                    }
                }
            }
            if toks.is_empty() {
                continue;
            }
            total_docs += 1;
            total_len += toks.len();
            for (qi, qt) in qtokens.iter().enumerate() {
                if toks.iter().any(|t| t == qt) {
                    dfs[qi] += 1;
                }
            }
        }
        let avgdl = if total_docs > 0 {
            total_len as f64 / total_docs as f64
        } else {
            1.0
        };
        let docs_only = def.get("index_options").and_then(|v| v.as_str()) == Some("docs");
        let norms_off = def.get("norms").and_then(|v| v.as_bool()) == Some(false);
        let mut matched = 0usize;
        let mut score = 0.0;
        for (qi, qt) in qtokens.iter().enumerate() {
            let edits = fuzz.edits_for(qt);
            let mut tf = cur_tokens
                .iter()
                .filter(|tok| {
                    *tok == qt
                        || (edits > 0
                            && fuzzy_token_match(qt, tok, edits, prefix_length, transpositions))
                })
                .count() as f64;
            if docs_only && tf > 0.0 {
                tf = 1.0;
            }
            if tf > 0.0 {
                matched += 1;
                let n = total_docs as f64;
                let df = dfs[qi] as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let norm = if norms_off { 1.0 } else { 1.0 - 0.75 + 0.75 * dl / avgdl };
                let tf_part = tf / (tf + 1.2 * norm);
                score += idf * tf_part;
            }
        }
        if matched >= required {
            if ftype == "match_only_text" {
                return Ok((true, boost));
            }
            return Ok((true, boost * score.max(1e-9)));
        }
        Ok((false, 0.0))
    }

    pub fn eval_match_phrase(
        &self,
        field: &str,
        raw: &Value,
        boost: f64,
        slop: f64,
    ) -> Result<(bool, f64), ApiError> {
        let pdef = mapping_field_def(self.idx, field).cloned().unwrap_or_default();
        let io = pdef.get("index_options").and_then(|v| v.as_str()).unwrap_or("");
        if io == "docs" || io == "freqs" {
            let reason = format!(
                "failed to create query: field:[{}] was indexed without position data; cannot run PhraseQuery",
                field
            );
            return Err(shard_query_error("query_shard_exception", &reason));
        }
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
            if def
                .get("split_queries_on_whitespace")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Err(shard_query_error(
                    "query_shard_exception",
                    &format!(
                        "failed to create query: field:[{}] was indexed without position data; cannot run PhraseQuery",
                        field
                    ),
                ));
            }
            for v in self.must_values(field) {
                let s = value_scalar_string(&v).unwrap_or_default();
                if s == text {
                    return Ok((true, boost));
                }
            }
            return Ok((false, 0.0));
        }
        let qan = self.analyzer_name(field, opts);
        let qtokens = analyze_positions(&qan, &text);
        if qtokens.is_empty() {
            let all = opts
                .and_then(|m| m.get("zero_terms_query"))
                .and_then(|v| v.as_str())
                .map(|v| v.eq_ignore_ascii_case("all"))
                .unwrap_or(false);
            if all {
                return Ok((true, boost));
            }
            return Ok((false, 0.0));
        }
        if let Some(r) = self.synonym_match(
            field,
            &qtokens.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            true,
        ) {
            return Ok(r);
        }
        let dan = self.analyzer_name(field, None);
        let gap = mapping_field_def(self.idx, field)
            .and_then(|d| d.get("position_increment_gap"))
            .and_then(|v| v.as_i64())
            .unwrap_or(100) as usize;
        let mut dtokens: Vec<(String, usize)> = Vec::new();
        let mut base = 0usize;
        for v in self.must_values(field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            let part = analyze_positions(&dan, &s);
            let mut max_pos = 0usize;
            for (tok, pos) in part {
                dtokens.push((tok, pos + base));
                max_pos = max_pos.max(pos + base);
            }
            base = max_pos + gap + 1;
        }
        if dtokens.is_empty() {
            return Ok((false, 0.0));
        }
        if phrase_positions_match(&qtokens, &dtokens, slop.max(0.0) as usize) {
            let norm = 1.0 + 1.0 / (1.0 + dtokens.len() as f64);
            return Ok((true, boost * norm * qtokens.len() as f64));
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
        let mut fuzz = Fuzz::None;
        if let Some(spec) = opts.and_then(|m| m.get("fuzziness")) {
            match parse_fuzziness(spec) {
                Ok(f) => fuzz = f,
                Err(bad) => {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Invalid fuzziness value: {}", bad),
                        400,
                    ));
                }
            }
        }
        let prefix_length = opts
            .and_then(|m| m.get("prefix_length"))
            .and_then(|v| to_f64(v))
            .unwrap_or(0.0) as usize;
        let transpositions = opts
            .and_then(|m| m.get("fuzzy_transpositions"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let qtokens = self.analyze_field(field, opts, &text);
        if qtokens.is_empty() {
            return Ok((false, 0.0));
        }
        let mut required = if operator == "and" { qtokens.len() } else { 1 };
        if let Some(spec) = opts.and_then(|m| m.get("minimum_should_match")) {
            match minimum_should_match_checked(spec, qtokens.len()) {
                Ok(n) => required = n,
                Err(bad) => {
                    let reason = format!("failed to create query: For input string: \"{}\"", bad);
                    let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
                    e.root_cause = Some(Box::new(ApiError::new(
                        "query_shard_exception",
                        &reason,
                        400,
                    )));
                    return Err(e);
                }
            }
        }
        let mut matched = 0usize;
        for (i, tok) in qtokens.iter().enumerate() {
            let ok = if i == qtokens.len() - 1 {
                self.eval_prefix(field, &Value::String(tok.clone()), boost)?.0
            } else {
                let edits = fuzz.edits_for(tok);
                if edits > 0 {
                    self.field_token_fuzzy(field, tok, edits, prefix_length, transpositions)
                } else {
                    self.eval_term(field, &Value::String(tok.clone()), boost)?.0
                }
            };
            if ok {
                matched += 1;
            }
        }
        if matched >= required {
            return Ok((true, boost));
        }
        Ok((false, 0.0))
    }

    fn field_token_fuzzy(
        &self,
        field: &str,
        token: &str,
        edits: usize,
        prefix_length: usize,
        transpositions: bool,
    ) -> bool {
        for v in self.must_values(field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            for tok in self.analyze_field(field, None, &s) {
                if tok == token || fuzzy_token_match(token, &tok, edits, prefix_length, transpositions) {
                    return true;
                }
            }
        }
        false
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
            if !must_not.is_empty() {
                return Ok((true, 0.0));
            }
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
        if queries.is_empty() {
            return Err(ApiError::parsing(
                "query malformed, must start with start_object",
            ));
        }
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

    // A query object holds a single clause; only the first entry is evaluated.
    #[allow(clippy::never_loop)]
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
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("bad wildcard body")),
                    };
                    return self.eval_wildcard(&field, &val, 1.0);
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
                    if bm.len() > 1 {
                        let names: Vec<&str> = bm.keys().map(|k| k.as_str()).collect();
                        return Err(ApiError::parsing(&format!(
                            "[match] query doesn't support multiple fields, found [{}] and [{}]",
                            names[0], names[1]
                        )));
                    }
                    let (field, val) = match single_entry(bm) {
                        Some((k, v)) => (k.clone(), v.clone()),
                        None => return Err(ApiError::parsing("[match] query malformed")),
                    };
                    validate_match_query(&field, &val)?;
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
                    let (_, popts) = query_text(&val);
                    let slop = popts
                        .and_then(|m| m.get("slop"))
                        .and_then(|v| to_f64(v))
                        .unwrap_or(0.0);
                    if slop < 0.0 {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            "No negative slop allowed.",
                            400,
                        ));
                    }
                    return self.eval_match_phrase(&field, &val, 1.0, slop);
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
                    let (text, popts) = query_text(&val);
                    let slop = popts
                        .and_then(|m| m.get("slop"))
                        .and_then(|v| to_f64(v))
                        .unwrap_or(0.0);
                    if slop < 0.0 {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            "No negative slop allowed.",
                            400,
                        ));
                    }
                    let max_exp = popts
                        .and_then(|m| m.get("max_expansions"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(50)
                        .max(0) as usize;
                    let qan = self.analyzer_name(&field, popts);
                    let qtokens = analyze_positions(&qan, &text);
                    if qtokens.is_empty() {
                        return Ok((false, 0.0));
                    }
                    let prefix = qtokens.last().map(|(t, _)| t.clone()).unwrap_or_default();
                    let dan = self.analyzer_name(&field, None);
                    let resolved = resolve_alias_field(self.idx, &field);
                    let mut allowed: Vec<String> = Vec::new();
                    for dv in self.idx.doc_values.values() {
                        if let Some(vals) = dv.get(&resolved) {
                            for v in array_values(vals) {
                                let s = value_scalar_string(&v).unwrap_or_default();
                                for (tok, _) in analyze_positions(&dan, &s) {
                                    if tok.starts_with(&prefix) && !allowed.contains(&tok) {
                                        allowed.push(tok);
                                    }
                                }
                            }
                        }
                    }
                    allowed.sort();
                    allowed.truncate(max_exp);
                    let mut doc_tokens: Vec<Vec<(String, usize)>> = Vec::new();
                    for v in self.must_values(&field) {
                        let s = value_scalar_string(&v).unwrap_or_default();
                        doc_tokens.push(analyze_positions(&dan, &s));
                    }
                    for part in doc_tokens {
                        if phrase_positions_match_prefix(&qtokens, &part, slop.max(0.0) as usize, &allowed) {
                            return Ok((true, qtokens.len() as f64));
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
                        let nf = match bm.get("negative_boost").and_then(to_f64) {
                            Some(v) if v > 0.0 => v,
                            _ => {
                                return Err(ApiError::parsing(
                                    "[boosting] query requires 'negative_boost' to be set to be a positive value",
                                ))
                            }
                        };
                        let (neg, _) = self.eval(neg_raw)?;
                        if neg {
                            sc *= nf;
                        }
                    }
                    return Ok((true, sc));
                }
                "nested" => return self.eval_nested(body),
                "has_child" => return self.eval_has_child(body),
                "has_parent" => return self.eval_has_parent(body),
                "parent_id" => return self.eval_parent_id(body),
                "geo_distance" => return self.eval_geo_distance(body),
                "geo_bounding_box" => return self.eval_geo_bounding_box(body),
                "geo_polygon" => return self.eval_geo_polygon(body),
                "geo_shape" => return self.eval_geo_shape(body),
                "script" => return self.eval_script_query(body),
                "script_score" => return self.eval_script_score(body),
                "function_score" => return self.eval_function_score(body),
                "query_string" => return self.eval_query_string(body, false),
                "simple_query_string" => return self.eval_simple_query_string(body),
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
                other => {
                    let mut e = ApiError::parsing(&format!("unknown query [{}]", other));
                    e.caused_by = Some(Box::new(ApiError::new(
                        "named_object_not_found_exception",
                        &format!("[{}] unknown query", other),
                        400,
                    )));
                    return Err(e);
                }
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

fn validate_match_query(field: &str, raw: &Value) -> Result<(), ApiError> {
    let _ = field;
    if raw.is_null() {
        return Err(ApiError::parsing("No text specified for text query"));
    }
    if let Some(m) = raw.as_object() {
        if !m.contains_key("query") {
            return Err(ApiError::parsing("No text specified for text query"));
        }
        const ALLOWED: &[&str] = &[
            "query",
            "operator",
            "analyzer",
            "boost",
            "minimum_should_match",
            "fuzziness",
            "prefix_length",
            "max_expansions",
            "fuzzy_transpositions",
            "zero_terms_query",
            "lenient",
            "cutoff_frequency",
            "auto_generate_synonyms_phrase_query",
            "fuzzy_rewrite",
            "flags",
            "_name",
        ];
        for k in m.keys() {
            if !ALLOWED.contains(&k.as_str()) {
                return Err(ApiError::parsing(&format!(
                    "[match] query does not support [{}]",
                    k
                )));
            }
        }
        if let Some(op) = m.get("operator").and_then(|o| o.as_str()) {
            let up = op.to_uppercase();
            if up != "AND" && up != "OR" {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "No enum constant org.opensearch.index.query.Operator.{}",
                        up
                    ),
                    400,
                ));
            }
        }
        if let Some(z) = m.get("zero_terms_query").and_then(|o| o.as_str()) {
            if !z.eq_ignore_ascii_case("none") && !z.eq_ignore_ascii_case("all") {
                return Err(ApiError::parsing(&format!(
                    "Unsupported zero_terms_query value [{}]",
                    z
                )));
            }
        }
        if let Some(a) = m.get("analyzer").and_then(|o| o.as_str()) {
            let known = matches!(
                a.to_lowercase().as_str(),
                "standard" | "simple" | "whitespace" | "stop" | "keyword" | "lowercase"
                    | "english" | "pattern" | "fingerprint" | "arabic" | "armenian"
                    | "basque" | "bengali" | "brazilian" | "bulgarian" | "catalan"
                    | "cjk" | "czech" | "danish" | "dutch" | "estonian" | "finnish"
                    | "french" | "galician" | "german" | "greek" | "hindi" | "hungarian"
                    | "indonesian" | "irish" | "italian" | "latvian" | "lithuanian"
                    | "norwegian" | "persian" | "portuguese" | "romanian" | "russian"
                    | "sorani" | "spanish" | "swedish" | "turkish" | "thai"
            );
            if !known {
                let reason = format!("[match] analyzer [{}] not found", a);
                let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
                e.root_cause = Some(Box::new(ApiError::new(
                    "query_shard_exception",
                    &reason,
                    400,
                )));
                return Err(e);
            }
        }
    } else if raw.is_array() {
        return Err(ApiError::new(
            "illegal_state_exception",
            "failed to build match query",
            500,
        ));
    }
    Ok(())
}

fn parse_synonym_rules(v: &Value) -> Vec<Vec<Vec<String>>> {
    let mut out = Vec::new();
    let mut push = |line: &str| {
        let mut alts = Vec::new();
        for part in line.split(',') {
            let toks: Vec<String> = part
                .split_whitespace()
                .map(|t| t.to_lowercase())
                .collect();
            if !toks.is_empty() {
                alts.push(toks);
            }
        }
        if !alts.is_empty() {
            out.push(alts);
        }
    };
    match v {
        Value::Array(a) => {
            for e in a {
                if let Some(s) = e.as_str() {
                    push(s);
                }
            }
        }
        Value::String(s) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                if parsed.is_array() {
                    return parse_synonym_rules(&parsed);
                }
            }
            for line in s.lines() {
                if !line.trim().is_empty() && !line.trim_start().starts_with('#') {
                    push(line);
                }
            }
        }
        _ => {}
    }
    out
}

fn synonym_groups(idx: &Index, field: &str) -> Vec<Vec<Vec<String>>> {
    let analyzer = {
        let mut name = String::new();
        if let Some(def) = mapping_field_def(idx, field) {
            if let Some(a) = def.get("search_analyzer").and_then(|a| a.as_str()) {
                name = a.to_string();
            }
            if name.is_empty() {
                if let Some(a) = def.get("analyzer").and_then(|a| a.as_str()) {
                    name = a.to_string();
                }
            }
        }
        name
    };
    if analyzer.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let prefix = format!(".analysis.filter.");
    for (k, v) in &idx.settings {
        if k.contains(&prefix) && k.ends_with(".synonyms") {
            out.extend(parse_synonym_rules(v));
        }
    }
    out
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

#[derive(Clone, Copy)]
enum Fuzz {
    None,
    Auto(usize, usize),
    Fixed(usize),
}

impl Fuzz {
    fn edits_for(self, token: &str) -> usize {
        let n = token.chars().count();
        match self {
            Fuzz::None => 0,
            Fuzz::Fixed(v) => v,
            Fuzz::Auto(low, high) => {
                if n < low {
                    0
                } else if n < high {
                    1
                } else {
                    2
                }
            }
        }
    }
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in",
    "into", "is", "it", "no", "not", "of", "on", "or", "such", "that", "the",
    "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

fn tokenize_positions(text: &str) -> Vec<String> {
    analyze(text)
}

fn analyze_positions(analyzer: &str, text: &str) -> Vec<(String, usize)> {
    if analyzer == "keyword" {
        return vec![(text.to_string(), 0)];
    }
    if analyzer == "whitespace" {
        return text
            .split_whitespace()
            .enumerate()
            .map(|(i, s)| (s.to_string(), i))
            .collect();
    }
    if analyzer == "lowercase" {
        return vec![(simple_lower(text), 0)];
    }
    let mut out = Vec::new();
    for (i, tok) in analyze(text).into_iter().enumerate() {
        if analyzer == "stop" && STOP_WORDS.contains(&tok.as_str()) {
            continue;
        }
        out.push((tok, i));
    }
    out
}

fn phrase_positions_match(q: &[(String, usize)], d: &[(String, usize)], slop: usize) -> bool {
    if q.is_empty() {
        return false;
    }
    let mut prev: Vec<(usize, usize)> = Vec::new();
    for (di, (tok, pos)) in d.iter().enumerate() {
        if tok == &q[0].0 {
            prev.push((di, 0));
        }
    }
    if prev.is_empty() {
        return false;
    }
    for qi in 1..q.len() {
        let mut cur: Vec<(usize, usize)> = Vec::new();
        for (di, (tok, pos)) in d.iter().enumerate() {
            if tok != &q[qi].0 {
                continue;
            }
            let mut best = usize::MAX;
            for &(pdi, pcost) in &prev {
                if pdi == di {
                    continue;
                }
                let actual_gap = *pos as isize - d[pdi].1 as isize;
                let expected_gap = q[qi].1 as isize - q[qi - 1].1 as isize;
                let cost = pcost + actual_gap.abs_diff(expected_gap);
                best = best.min(cost);
            }
            if best != usize::MAX {
                cur.push((di, best));
            }
        }
        if cur.is_empty() {
            return false;
        }
        prev = cur;
    }
    prev.iter().any(|&(_, c)| c <= slop)
}

fn phrase_positions_match_prefix(
    q: &[(String, usize)],
    d: &[(String, usize)],
    slop: usize,
    allowed_last: &[String],
) -> bool {
    if q.is_empty() {
        return false;
    }
    let last = q.len() - 1;
    let tok_ok = |qi: usize, tok: &str| -> bool {
        if qi == last {
            allowed_last.iter().any(|a| a == tok)
        } else {
            tok == q[qi].0
        }
    };
    let mut prev: Vec<(usize, usize)> = Vec::new();
    for (di, (tok, _)) in d.iter().enumerate() {
        if tok_ok(0, tok) {
            prev.push((di, 0));
        }
    }
    if prev.is_empty() {
        return false;
    }
    for qi in 1..q.len() {
        let mut cur: Vec<(usize, usize)> = Vec::new();
        for (di, (tok, pos)) in d.iter().enumerate() {
            if !tok_ok(qi, tok) {
                continue;
            }
            let mut best = usize::MAX;
            for &(pdi, pcost) in &prev {
                if pdi == di {
                    continue;
                }
                let actual_gap = *pos as isize - d[pdi].1 as isize;
                let expected_gap = q[qi].1 as isize - q[qi - 1].1 as isize;
                let cost = pcost + actual_gap.abs_diff(expected_gap);
                best = best.min(cost);
            }
            if best != usize::MAX {
                cur.push((di, best));
            }
        }
        if cur.is_empty() {
            return false;
        }
        prev = cur;
    }
    prev.iter().any(|&(_, c)| c <= slop)
}

fn parse_fuzziness(spec: &Value) -> Result<Fuzz, String> {
    match spec {
        Value::Number(n) => Ok(Fuzz::Fixed(
            (n.as_i64().unwrap_or(0).max(0) as usize).min(2),
        )),
        Value::String(s) => {
            let s = s.trim();
            if let Some(rest) = s.strip_prefix("AUTO") {
                if rest.is_empty() {
                    return Ok(Fuzz::Auto(3, 6));
                }
                if let Some(args) = rest.strip_prefix(':') {
                    let mut parts = args.split(',');
                    let low = parts.next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(3);
                    let high = parts.next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(6);
                    return Ok(Fuzz::Auto(low, high));
                }
                return Err(s.to_string());
            }
            s.parse::<usize>()
                .map(|n| Fuzz::Fixed(n.min(2)))
                .map_err(|_| s.to_string())
        }
        _ => Ok(Fuzz::None),
    }
}

fn fuzzy_token_match(q: &str, cand: &str, max_edits: usize, prefix_length: usize, transpositions: bool) -> bool {
    let qc: Vec<char> = q.chars().collect();
    let cc: Vec<char> = cand.chars().collect();
    let prefix = prefix_length.min(qc.len()).min(cc.len());
    if qc[..prefix] != cc[..prefix] {
        return false;
    }
    let limit = max_edits;
    if limit == 0 {
        return false;
    }
    if qc[prefix..] == cc[prefix..] {
        return true;
    }
    damerau_within(&qc[prefix..], &cc[prefix..], limit, transpositions)
}

fn damerau_within(a: &[char], b: &[char], limit: usize, transpositions: bool) -> bool {
    let n = a.len();
    let m = b.len();
    if n.abs_diff(m) > limit {
        return false;
    }
    let mut prev2: Vec<usize> = vec![0; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if transpositions && i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > limit {
            return false;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m] <= limit
}

pub fn minimum_should_match(spec: &Value, clause_count: usize, _matched: usize) -> usize {
    minimum_should_match_checked(spec, clause_count).unwrap_or(1)
}

fn msm_one(rule: &str, clause_count: usize) -> Result<i64, String> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Ok(1);
    }
    if let Some(pct) = rule.strip_suffix('%') {
        let neg = pct.starts_with('-');
        let pct = pct.trim_start_matches('-');
        let p: f64 = pct.parse().map_err(|_| pct.to_string())?;
        let v = (p / 100.0 * clause_count as f64) as i64;
        if neg {
            let req = clause_count as i64 - v;
            return Ok(if req <= 0 { 1 } else { req });
        }
        return Ok(v);
    }
    if let Ok(n) = rule.parse::<i64>() {
        if n < 0 {
            let req = clause_count as i64 + n;
            return Ok(if req <= 0 { 1 } else { req });
        }
        return Ok(n);
    }
    Err(rule.to_string())
}

pub fn minimum_should_match_checked(spec: &Value, clause_count: usize) -> Result<usize, String> {
    let text = match spec {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return Ok(1),
    };
    let mut conditional: Option<&str> = None;
    for part in text.split_whitespace() {
        if let Some(lt) = part.find('<') {
            let (cond, rest) = part.split_at(lt);
            let rest = &rest[1..];
            let cond: i64 = cond.parse().map_err(|_| cond.to_string())?;
            if clause_count as i64 > cond {
                conditional = Some(rest);
            }
        } else {
            conditional = Some(part);
            break;
        }
    }
    let rule = match conditional {
        Some(r) => r,
        None => return Ok(clause_count),
    };
    let required = msm_one(rule, clause_count)?;
    Ok(required.max(1) as usize)
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

fn replace_unescaped_at(pattern: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in pattern.chars() {
        if esc {
            out.push(c);
            esc = false;
            continue;
        }
        match c {
            '\\' => {
                out.push(c);
                esc = true;
            }
            '@' => out.push_str(".*"),
            other => out.push(other),
        }
    }
    out
}

fn expand_intervals(pattern: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = pattern[i + 1..].find('>') {
                let inner = &pattern[i + 1..i + 1 + end];
                if let Some((a, b)) = inner.split_once('-') {
                    if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                        if a <= b && b - a <= 1000 {
                            out.push('(');
                            for n in a..=b {
                                if n > a {
                                    out.push('|');
                                }
                                out.push_str(&n.to_string());
                            }
                            out.push(')');
                            i += end + 2;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn wildcard_regex(pattern: &str) -> Regex {
    let mut b = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    b.push_str(&regex::escape(&next.to_string()));
                } else {
                    b.push_str(&regex::escape("\\"));
                }
            }
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
