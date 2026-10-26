use crate::eval::QueryCtx;
use crate::fields::canonical_ip;
use crate::resp::format_number;
use crate::search::{apply_java_date_format, civil_from_days, field_type, format_epoch_ms};
use crate::store::*;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

fn resolve_field<'a>(field: &'a str, prefix: &str) -> &'a str {
    if !prefix.is_empty() {
        let p = format!("{}.", prefix);
        if let Some(stripped) = field.strip_prefix(&p) {
            return stripped;
        }
    }
    field
}

fn map_str(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(value_to_string).unwrap_or_default()
}

fn agg_field_value<'a>(doc: &'a Value, field: &str, prefix: &str) -> Option<&'a Value> {
    field_value(doc, resolve_field(field, prefix))
}

fn values_of(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.iter().filter(|x| !x.is_null()).cloned().collect(),
        Some(Value::Null) | None => vec![],
        Some(x) => vec![x.clone()],
    }
}

fn doc_field_values(idx: &Index, doc: &Value, field: &str, prefix: &str) -> Vec<Value> {
    if let Some(n) = doc.get("__nest").and_then(|v| v.as_object()) {
        let p0 = n.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let elem = n.get("elem").cloned().unwrap_or(Value::Null);
        let rf = resolve_field(field, prefix);
        if let Some(rest) = rf.strip_prefix(&format!("{}.", p0)) {
            return values_of(agg_field_value(&elem, rest, ""));
        }
        if rf == p0 {
            return values_of(Some(&elem));
        }
        return vec![];
    }
    let base = resolve_field(field, prefix);
    let rf = crate::eval::resolve_alias_field(idx, &base);
    if is_under_nested_path(&rf) {
        return vec![];
    }
    if crate::eval::mapping_field_def(idx, &rf).is_none() {
        let dyn_off = match idx.mappings.get("dynamic") {
            Some(Value::Bool(false)) => true,
            Some(Value::String(s)) => s == "false" || s == "false_allow_templates",
            _ => false,
        };
        if dyn_off {
            return vec![];
        }
    }
    let mut vals = if rf != base {
        values_of(agg_field_value(doc, &rf, ""))
    } else {
        values_of(agg_field_value(doc, field, prefix))
    };
    if vals.is_empty() {
        let same_doc = |id: &str| {
            let Some(orig) = idx.docs.get(id) else {
                return false;
            };
            let mut d = doc.clone();
            if orig.get("_id").is_none() {
                if let Some(m) = d.as_object_mut() {
                    m.remove("_id");
                }
            }
            &d == orig
        };
        let by_id = doc
            .get("_id")
            .and_then(|v| v.as_str())
            .filter(|id| same_doc(id))
            .and_then(|id| idx.doc_values.get(id));
        let found = by_id.or_else(|| {
            idx.doc_values
                .iter()
                .find(|(id, _)| idx.docs.get(*id).map(|d| d == doc).unwrap_or(false))
                .map(|(_, dv)| dv)
        });
        if let Some(dv) = found {
            if let Some(raw) = dv.get(&rf) {
                vals = values_of(Some(raw));
            }
        }
    }
    if let Some(def) = crate::eval::mapping_field_def(idx, &rf) {
        if def.get("type").and_then(|v| v.as_str()) == Some("join") {
            vals = vals
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Value::String(s),
                    Value::Object(m) => Value::String(
                        m.get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    other => other,
                })
                .collect();
        }
        let ftype = def.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ftype == "boolean" {
            vals = vals
                .into_iter()
                .map(|v| match v {
                    Value::Bool(b) => Value::Bool(b),
                    Value::String(s) => Value::Bool(s == "true"),
                    Value::Number(n) => Value::Bool(n.as_i64().unwrap_or(0) != 0),
                    _ => Value::Bool(false),
                })
                .collect();
        }
        if let Some(ign) = doc.get("_ignored").and_then(|v| v.as_array()) {
            if ign.iter().any(|x| x.as_str() == Some(rf.as_str())) {
                vals.clear();
            }
        }
        if matches!(ftype, "keyword" | "constant_keyword" | "wildcard") {
            if let Some(limit) = def.get("ignore_above").and_then(|v| v.as_i64()) {
                if limit >= 0 {
                    vals.retain(|v| match v.as_str() {
                        Some(s) => s.chars().count() as i64 <= limit,
                        None => true,
                    });
                }
            }
        }
        if ftype == "ip" {
            vals = vals
                .into_iter()
                .map(|v| match v.as_str().and_then(|s| crate::fields::canonical_ip(s).ok()) {
                    Some(c) => Value::String(c),
                    None => v,
                })
                .collect();
        }
        if ftype == "scaled_float" {
            if let Some(scale) = def.get("scaling_factor").and_then(to_f64) {
                if scale != 0.0 {
                    vals = vals
                        .into_iter()
                        .map(|v| match to_f64(&v) {
                            Some(f) => serde_json::json!((f * scale).round() / scale),
                            None => v,
                        })
                        .collect();
                }
            }
        }
        if ftype == "text"
            && def.get("fielddata").and_then(|v| v.as_bool()) == Some(true)
        {
            vals = vals
                .into_iter()
                .filter_map(|v| v.as_str().map(crate::store::analyze))
                .flatten()
                .map(Value::String)
                .collect();
        }
        if let Some(norm) = def.get("normalizer").and_then(|v| v.as_str()) {
            if !norm.is_empty() {
                vals = vals
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => {
                            Value::String(crate::fields::apply_normalizer_idx(idx, norm, &s))
                        }
                        other => other,
                    })
                    .collect();
            }
        }
        if vals.is_empty() && ftype == "constant_keyword" {
            if let Some(cv) = def.get("value") {
                vals.push(cv.clone());
            }
        }
    }
    vals
}

fn numeric_values(idx: &Index, docs: &[&Value], field: &str, prefix: &str) -> Vec<f64> {
    let mut out = vec![];
    for d in docs {
        for v in doc_field_values(idx, d, field, prefix) {
            if let Some(f) = to_f64(&v) {
                out.push(f);
            }
        }
    }
    out
}

fn sub_aggs(def: &Map<String, Value>) -> Option<Map<String, Value>> {
    if let Some(sub) = def.get("aggs").and_then(|a| a.as_object()) {
        return Some(sub.clone());
    }
    def.get("aggregations")
        .and_then(|a| a.as_object())
        .cloned()
}

fn all_live_docs(idx: &Index) -> Vec<Value> {
    let mut out = vec![];
    for id in &idx.order {
        if let Some(d) = idx.docs.get(id) {
            let mut doc = d.clone();
            if let Value::Object(ref mut m) = doc {
                if !m.contains_key("_id") {
                    m.insert("_id".to_string(), json!(id.clone()));
                }
            }
            out.push(doc);
        }
    }
    out
}

fn nested_leaf_values(
    idx: &Index,
    p0: &str,
    elem: &Value,
    prefix: &str,
    out: &mut Map<String, Value>,
) {
    let def = if prefix.is_empty() {
        crate::eval::mapping_field_def(idx, p0).cloned()
    } else {
        crate::eval::mapping_field_def(idx, &format!("{}.{}", p0, prefix)).cloned()
    };
    let def = match def {
        Some(d) => d,
        None => return,
    };
    if let Some(props) = def.get("properties").and_then(|v| v.as_object()) {
        for (name, _) in props {
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}.{}", prefix, name)
            };
            nested_leaf_values(idx, p0, elem, &rel, out);
        }
        return;
    }
    let vals = values_of(field_value(elem, prefix));
    if vals.is_empty() {
        return;
    }
    out.insert(prefix.to_string(), Value::Array(vals.clone()));
    out.insert(format!("{}.{}", p0, prefix), Value::Array(vals));
}

fn doc_matches(idx: &Index, doc: &Value, query: &Map<String, Value>) -> bool {
    if let Some(n) = doc.get("__nest").and_then(|v| v.as_object()) {
        let p0 = n
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let elem = n.get("elem").cloned().unwrap_or(Value::Null);
        if elem.as_object().is_none() {
            return false;
        }
        let mut vm = Map::new();
        nested_leaf_values(idx, &p0, &elem, "", &mut vm);
        let mut ctx = QueryCtx::with_values(idx, "", &elem, vm);
        ctx.scope_path = Some(p0);
        ctx.scope_obj = Some(elem.clone());
        return ctx
            .eval(query)
            .map(|(ok, _)| ok)
            .unwrap_or(false);
    }
    let m = match doc.as_object() {
        Some(m) => m,
        None => return false,
    };
    let mut m2 = m.clone();
    m2.remove("_id");
    match crate::fields::doc_values_for(idx, "", &m2) {
        Ok((vals, _)) => {
            let mut vm = Map::new();
            for (k, v) in vals {
                vm.insert(k, Value::Array(v));
            }
            QueryCtx::with_values(idx, "", doc, vm)
                .eval(query)
                .map(|(ok, _)| ok)
                .unwrap_or(false)
        }
        Err(_) => false,
    }
}

fn json_num(f: f64) -> Value {
    match serde_json::Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None => Value::Null,
    }
}

fn fmt_bound(f: f64) -> String {
    if f == f.trunc() && f.is_finite() && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        format_number(f)
    }
}

fn is_mapped_field(idx: &Index, field: &str, prefix: &str) -> bool {
    idx.field_types.contains_key(resolve_field(field, prefix))
}

fn field_kind(idx: &Index, field: &str, prefix: &str) -> String {
    let resolved = resolve_field(field, prefix);
    field_type(idx, &crate::eval::resolve_alias_field(idx, &resolved))
}

#[derive(Clone, PartialEq)]
enum OrdKey {
    Num(f64),
    Str(String),
    Bool(bool),
    IpLow(u128),
    IpHigh(u128),
}

impl Eq for OrdKey {}

impl Ord for OrdKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (OrdKey::Num(a), OrdKey::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (OrdKey::Str(a), OrdKey::Str(b)) => a.cmp(b),
            (OrdKey::Bool(a), OrdKey::Bool(b)) => a.cmp(b),
            (OrdKey::IpLow(a), OrdKey::IpLow(b)) => a.cmp(b),
            (OrdKey::IpHigh(a), OrdKey::IpHigh(b)) => a.cmp(b),
            (OrdKey::IpLow(_), OrdKey::IpHigh(_)) => Ordering::Less,
            (OrdKey::IpHigh(_), OrdKey::IpLow(_)) => Ordering::Greater,
            _ => Ordering::Equal,
        }
    }
}

impl PartialOrd for OrdKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn ip_numeric(s: &str) -> Option<(u128, bool)> {
    let c = canonical_ip(s).ok()?;
    if c.contains(':') {
        let mut all: Vec<u16> = vec![];
        if let Some((head, tail)) = c.split_once("::") {
            for g in head.split(':') {
                if !g.is_empty() {
                    all.push(u16::from_str_radix(g, 16).ok()?);
                }
            }
            let mut tvec: Vec<u16> = vec![];
            for g in tail.split(':') {
                if !g.is_empty() {
                    tvec.push(u16::from_str_radix(g, 16).ok()?);
                }
            }
            while all.len() + tvec.len() < 8 {
                all.push(0);
            }
            all.extend(tvec);
        } else {
            for g in c.split(':') {
                all.push(u16::from_str_radix(g, 16).ok()?);
            }
        }
        if all.len() != 8 {
            return None;
        }
        let mut v: u128 = 0;
        for g in all {
            v = (v << 16) | g as u128;
        }
        Some((v, true))
    } else {
        let parts: Vec<&str> = c.split('.').collect();
        if parts.len() != 4 {
            return None;
        }
        let mut v: u128 = 0;
        for p in parts {
            let o: u128 = p.parse().ok()?;
            if o > 255 {
                return None;
            }
            v = (v << 8) | o;
        }
        Some((v, false))
    }
}

fn ord_key_of(idx: &Index, field: &str, prefix: &str, raw: &Value) -> OrdKey {
    let ft = field_kind(idx, field, prefix);
    match ft.as_str() {
        "integer" | "long" | "short" | "byte" | "unsigned_long" | "float" | "double"
        | "half_float" | "scaled_float" => match to_f64(raw) {
            Some(f) => OrdKey::Num(f),
            None => OrdKey::Num(0.0),
        },
        "boolean" => match raw {
            Value::Bool(b) => OrdKey::Bool(*b),
            Value::String(s) => OrdKey::Bool(s == "true"),
            Value::Number(n) => OrdKey::Bool(n.as_i64().unwrap_or(0) != 0),
            _ => OrdKey::Bool(false),
        },
        "date" | "date_nanos" => match date_ms_of(raw) {
            Some(ms) => OrdKey::Num(ms as f64),
            None => OrdKey::Num(0.0),
        },
        "ip" => match raw.as_str().and_then(ip_numeric) {
            Some((v, true)) => OrdKey::IpHigh(v),
            Some((v, false)) => OrdKey::IpLow(v),
            None => OrdKey::IpLow(0),
        },
        _ => OrdKey::Str(value_to_string(raw).unwrap_or_default()),
    }
}

fn term_key_and_string(
    idx: &Index,
    field: &str,
    prefix: &str,
    raw: &Value,
    format: &str,
) -> (Value, Option<String>) {
    let ft = field_kind(idx, field, prefix);
    match ft.as_str() {
        "integer" | "long" | "short" | "byte" | "unsigned_long" => match raw {
            Value::Number(n) => (Value::Number(n.clone()), None),
            Value::String(s) => match s.parse::<i64>() {
                Ok(i) => (json!(i), None),
                Err(_) => (raw.clone(), None),
            },
            _ => (raw.clone(), None),
        },
        "float" | "double" | "half_float" | "scaled_float" => match to_f64(raw) {
            Some(f) => (json_num(f), None),
            None => (raw.clone(), None),
        },
        "boolean" => {
            let b = match raw {
                Value::Bool(b) => *b,
                Value::String(s) => s == "true",
                Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                _ => false,
            };
            (
                json!(if b { 1 } else { 0 }),
                Some(if b { "true".to_string() } else { "false".to_string() }),
            )
        }
        "date" | "date_nanos" => match date_ms_of(raw) {
            Some(ms) => {
                let s = if format.is_empty() {
                    format_epoch_ms(ms)
                } else {
                    apply_java_date_format(format, ms).unwrap_or_else(|| format_epoch_ms(ms))
                };
                (json!(ms), Some(s))
            }
            None => (raw.clone(), None),
        },
        "ip" => match raw.as_str() {
            Some(s) => match canonical_ip(s) {
                Ok(c) => (json!(c), None),
                Err(_) => (raw.clone(), None),
            },
            None => (raw.clone(), None),
        },
        _ => (raw.clone(), None),
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn month_start_ms(y: i64, m: i64) -> i64 {
    days_from_civil(y, m, 1) * 86_400_000
}

fn add_months_ms(ms: i64, n: i64) -> i64 {
    let days = ms.div_euclid(86_400_000);
    let (y, m, _d) = civil_from_days(days);
    let idx = y * 12 + (m - 1) + n;
    let y2 = idx.div_euclid(12);
    let m2 = idx.rem_euclid(12) + 1;
    month_start_ms(y2, m2)
}

fn month_index(ms: i64) -> i64 {
    let (y, m, _d) = civil_from_days(ms.div_euclid(86_400_000));
    y * 12 + m - 1
}

fn date_ms_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f as i64),
        Value::String(s) => parse_date_flex(s),
        _ => None,
    }
}

fn parse_date_flex(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Some(ms) = parse_date_string(t) {
        return Some(ms as i64);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Some(f as i64);
    }
    let b = t.as_bytes();
    if b.len() == 4 && b.iter().all(|c| c.is_ascii_digit()) {
        let y: i64 = t.parse().ok()?;
        return Some(month_start_ms(y, 1));
    }
    if b.len() == 7 && b[4] == b'-' {
        let y: i64 = t[..4].parse().ok()?;
        let m: i64 = t[5..].parse().ok()?;
        if (1..=12).contains(&m) {
            return Some(month_start_ms(y, m));
        }
    }
    None
}

fn parse_with_format(fmt: &str, s: &str) -> Option<i64> {
    let f = fmt.replace("'T'", "T");
    if f.contains("HH") || f.contains("ss") || f.contains("dd") {
        return parse_date_string(s).map(|v| v as i64);
    }
    if f.contains("MM") {
        let b = s.as_bytes();
        if b.len() == 7 && b[4] == b'-' {
            let y: i64 = s[..4].parse().ok()?;
            let m: i64 = s[5..7].parse().ok()?;
            if (1..=12).contains(&m) {
                return Some(month_start_ms(y, m));
            }
        }
        return None;
    }
    if f.contains("yyyy") {
        if s.len() != 4 {
            return None;
        }
        let y: i64 = s.parse().ok()?;
        return Some(month_start_ms(y, 1));
    }
    None
}

fn parse_date_math(s: &str) -> Option<i64> {
    let t = s.trim();
    let (base_ms, math): (i64, &str) = if let Some(i) = t.find("||") {
        (parse_date_flex(&t[..i])?, &t[i + 2..])
    } else if let Some(rest) = t.strip_prefix("now") {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        (now, rest)
    } else {
        return parse_date_flex(t);
    };
    let (delta_part, round) = match math.find('/') {
        Some(i) => (&math[..i], Some(&math[i + 1..])),
        None => (math, None),
    };
    let mut ms = base_ms;
    if !delta_part.is_empty() {
        let sign = if delta_part.starts_with('-') { -1 } else { 1 };
        let d = delta_part.trim_start_matches(['+', '-']);
        let num: String = d.chars().take_while(|c| c.is_ascii_digit()).collect();
        if num.is_empty() {
            return None;
        }
        let unit = &d[num.len()..];
        let n: i64 = num.parse::<i64>().ok()? * (sign as i64);
        ms = match unit {
            "y" | "Y" => add_months_ms(ms, n * 12),
            "M" => add_months_ms(ms, n),
            "w" => ms + n * 604_800_000,
            "d" => ms + n * 86_400_000,
            "h" | "H" => ms + n * 3_600_000,
            "m" => ms + n * 60_000,
            "s" => ms + n * 1000,
            "ms" => ms + n,
            _ => return None,
        };
    }
    if let Some(r) = round {
        ms = match r {
            "y" => {
                let (y, _m, _d) = civil_from_days(ms.div_euclid(86_400_000));
                month_start_ms(y, 1)
            }
            "M" => {
                let (y, m, _d) = civil_from_days(ms.div_euclid(86_400_000));
                month_start_ms(y, m)
            }
            "d" => ms.div_euclid(86_400_000) * 86_400_000,
            "h" => ms.div_euclid(3_600_000) * 3_600_000,
            "m" => ms.div_euclid(60_000) * 60_000,
            "s" => ms.div_euclid(1000) * 1000,
            _ => ms,
        };
    }
    Some(ms)
}

fn date_step(iv: &str) -> Option<(Option<i64>, Option<&'static str>)> {
    Some(match iv {
        "second" | "1s" => (Some(1000), None),
        "minute" | "1m" => (Some(60_000), None),
        "hour" | "1h" => (Some(3_600_000), None),
        "day" | "1d" => (Some(86_400_000), None),
        "week" | "1w" => (None, Some("week")),
        "month" | "1M" => (None, Some("month")),
        "quarter" | "1q" => (None, Some("quarter")),
        "year" | "1y" => (None, Some("year")),
        _ => {
            let b = iv.as_bytes();
            if b.is_empty() || !b[0].is_ascii_digit() {
                return None;
            }
            let num: String = iv.chars().take_while(|c| c.is_ascii_digit()).collect();
            let unit = &iv[num.len()..];
            let n: i64 = num.parse().ok()?;
            let unit_ms = match unit {
                "ms" => 1,
                "s" => 1000,
                "m" => 60_000,
                "h" => 3_600_000,
                "d" => 86_400_000,
                "w" => 604_800_000,
                _ => return None,
            };
            (Some(n * unit_ms), None)
        }
    })
}

fn date_bucket_start(ms: i64, fixed: Option<i64>, cal: Option<&str>) -> i64 {
    if let Some(step) = fixed {
        return ms.div_euclid(step) * step;
    }
    let cal = cal.unwrap_or("day");
    let days = ms.div_euclid(86_400_000);
    let (y, m, _d) = civil_from_days(days);
    match cal {
        "year" => month_start_ms(y, 1),
        "quarter" => month_start_ms(y, ((m - 1) / 3) * 3 + 1),
        "month" => month_start_ms(y, m),
        "week" => {
            let dow = (days + 3).rem_euclid(7);
            (days - dow) * 86_400_000
        }
        "hour" => ms.div_euclid(3_600_000) * 3_600_000,
        "minute" => ms.div_euclid(60_000) * 60_000,
        "second" => ms.div_euclid(1000) * 1000,
        _ => days * 86_400_000,
    }
}

fn date_next_key(k: i64, fixed: Option<i64>, cal: Option<&str>) -> i64 {
    if let Some(step) = fixed {
        return k + step;
    }
    match cal.unwrap_or("day") {
        "year" => {
            let (y, _m, _d) = civil_from_days(k.div_euclid(86_400_000));
            month_start_ms(y + 1, 1)
        }
        "quarter" => add_months_ms(k, 3),
        "month" => add_months_ms(k, 1),
        "week" => k + 7 * 86_400_000,
        "hour" => k + 3_600_000,
        "minute" => k + 60_000,
        "second" => k + 1000,
        _ => k + 86_400_000,
    }
}

pub fn eval_aggs(
    aggs: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    eval_aggs_typed(aggs, docs, idx, prefix, false)
}

pub fn eval_aggs_root(
    aggs: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    typed: bool,
) -> Result<Value, ApiError> {
    for (name, raw) in aggs {
        if let Some(def) = raw.as_object() {
            if let Some(pt) = pipeline_type_of(def) {
                if is_parent_scoped_pipeline(pt) {
                    if matches!(pt, "bucket_script" | "bucket_selector" | "bucket_sort") {
                        return Err(validation_failed(&format!(
                            "{} aggregation [{}] must be declared inside of another aggregation;",
                            pt, name
                        )));
                    }
                    let path = def
                        .get(pt)
                        .and_then(|v| v.as_object())
                        .and_then(|m| m.get("buckets_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    return Err(validation_failed(&format!(
                        "No aggregation found for path [{}];",
                        path
                    )));
                }
            }
        }
    }
    eval_aggs_typed(aggs, docs, idx, "", typed)
}

const AGG_TYPES: &[&str] = &[
    "terms",
    "multi_terms",
    "rare_terms",
    "significant_terms",
    "significant_text",
    "date_histogram",
    "auto_date_histogram",
    "variable_width_histogram",
    "histogram",
    "range",
    "date_range",
    "ip_range",
    "filter",
    "filters",
    "missing",
    "global",
    "nested",
    "composite",
    "adjacency_matrix",
    "geohash_grid",
    "geotile_grid",
    "geo_distance",
    "sampler",
    "diversified_sampler",
    "reverse_nested",
    "children",
    "parent",
    "avg",
    "min",
    "max",
    "sum",
    "stats",
    "cardinality",
    "value_count",
    "percentiles",
    "percentile_ranks",
    "extended_stats",
    "weighted_avg",
    "top_hits",
    "scripted_metric",
    "matrix_stats",
    "median_absolute_deviation",
    "geo_bounds",
    "geo_centroid",
    "derivative",
    "cumulative_sum",
    "serial_diff",
    "moving_fn",
    "moving_avg",
    "bucket_script",
    "bucket_selector",
    "bucket_sort",
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
];

fn closest_agg_type(name: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for cand in AGG_TYPES {
        let d = levenshtein(name, cand);
        if d <= 2 {
            match best {
                Some((bd, _)) if bd <= d => {}
                _ => best = Some((d, cand)),
            }
        }
    }
    best.map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

pub fn eval_aggs_typed(
    aggs: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    refresh_nested_paths(idx);
    let mut out = Map::new();
    let mut sibling_specs: Vec<(String, &'static str, Map<String, Value>)> = Vec::new();
    for (name, raw) in aggs {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        if name.contains('[') || name.contains(']') || name.contains('>') {
            return Err(ApiError::new(
                "parsing_exception",
                &format!(
                    "Invalid aggregation name [{}]. Aggregation names can contain any character except '[', ']', and '>'",
                    name
                ),
                400,
            ));
        }
        let type_keys: Vec<String> = def
            .keys()
            .filter(|k| k.as_str() != "aggs" && k.as_str() != "aggregations" && k.as_str() != "meta")
            .cloned()
            .collect();
        if type_keys.is_empty() {
            return Err(ApiError::new(
                "parsing_exception",
                &format!("Missing definition for aggregation [{}]", name),
                400,
            ));
        }
        if type_keys.len() > 1 {
            return Err(ApiError::new(
                "parsing_exception",
                &format!(
                    "Found two aggregation type definitions in [{}]: [{}] and [{}]",
                    name, type_keys[0], type_keys[1]
                ),
                400,
            ));
        }
        if !AGG_TYPES.contains(&type_keys[0].as_str()) {
            let mut reason = format!("Unknown aggregation type [{}]", type_keys[0]);
            if let Some(s) = closest_agg_type(&type_keys[0]) {
                reason.push_str(&format!(" did you mean [{}]?", s));
            }
            let mut e = ApiError::new("parsing_exception", &reason, 400);
            e.caused_by = Some(Box::new(ApiError::new(
                "named_object_not_found_exception",
                &format!("[{}] unknown field [{}]", name, type_keys[0]),
                400,
            )));
            return Err(e);
        }
        if let Some(pt) = pipeline_type_of(def) {
            if is_parent_scoped_pipeline(pt) {
                continue;
            }
            let pt_cfg = def
                .get(pt)
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            sibling_specs.push((name.clone(), pt, pt_cfg));
            continue;
        }
        let mut res = eval_one(name, def, docs, idx, prefix, typed)?;
        if let Some(meta) = def.get("meta") {
            if let Value::Object(ref mut m) = res {
                m.insert("meta".to_string(), meta.clone());
            }
        }
        let key = if typed {
            match typed_prefix(def, idx, prefix) {
                Some(p) => format!("{}#{}", p, name),
                None => name.clone(),
            }
        } else {
            name.clone()
        };
        out.insert(key, res);
    }
    for (pname, pt, t) in sibling_specs {
        let res = eval_metric_pipeline(&pname, pt, &t, &out, aggs, idx, prefix, typed)?;
        out.insert(pname, res);
    }
    Ok(Value::Object(out))
}

fn terms_prefix(ft: &str) -> &'static str {
    match ft {
        "byte" | "short" | "integer" | "long" | "unsigned_long" | "date" | "date_nanos"
        | "boolean" => "lterms",
        "float" | "double" | "half_float" | "scaled_float" => "dterms",
        _ => "sterms",
    }
}

fn field_of(def: &Map<String, Value>, key: &str) -> String {
    def.get(key)
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("field"))
        .and_then(value_to_string)
        .unwrap_or_default()
}

fn typed_prefix(def: &Map<String, Value>, idx: &Index, prefix: &str) -> Option<String> {
    if def.contains_key("terms") {
        let ft = field_kind(idx, &field_of(def, "terms"), prefix);
        return Some(terms_prefix(&ft).to_string());
    }
    if def.contains_key("rare_terms") {
        let ft = field_kind(idx, &field_of(def, "rare_terms"), prefix);
        let p = match terms_prefix(&ft) {
            "lterms" => "lrareterms",
            "dterms" => "drareterms",
            _ => "srareterms",
        };
        return Some(p.to_string());
    }
    if def.contains_key("significant_terms") || def.contains_key("significant_text") {
        return Some("sigsterms".to_string());
    }
    for k in [
        "multi_terms",
        "histogram",
        "date_histogram",
        "auto_date_histogram",
        "variable_width_histogram",
        "range",
        "date_range",
        "ip_range",
        "filter",
        "filters",
        "missing",
        "global",
        "nested",
        "composite",
        "sampler",
        "diversified_sampler",
        "reverse_nested",
        "adjacency_matrix",
        "geohash_grid",
        "geotile_grid",
        "geo_distance",
        "avg",
        "min",
        "max",
        "sum",
        "stats",
        "cardinality",
        "value_count",
        "percentiles",
        "percentile_ranks",
        "extended_stats",
        "weighted_avg",
        "top_hits",
        "scripted_metric",
        "matrix_stats",
    ] {
        if def.contains_key(k) {
            let p = if k == "diversified_sampler" { "sampler" } else { k };
            return Some(p.to_string());
        }
    }
    None
}

fn eval_one(
    name: &str,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    if !def.contains_key("global") {
        check_global_in_sub(name, def)?;
    }
    if let Some(t) = def.get("terms").and_then(|v| v.as_object()) {
        return terms_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("multi_terms").and_then(|v| v.as_object()) {
        return multi_terms_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("rare_terms").and_then(|v| v.as_object()) {
        return rare_terms_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("significant_terms").and_then(|v| v.as_object()) {
        return significant_agg(t, def, docs, idx, prefix, typed, false);
    }
    if def.contains_key("significant_text") {
        let empty = Map::new();
        let t = def
            .get("significant_text")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty);
        return significant_agg(t, def, docs, idx, prefix, typed, true);
    }
    if let Some(t) = def.get("date_histogram").and_then(|v| v.as_object()) {
        return date_histogram(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("auto_date_histogram").and_then(|v| v.as_object()) {
        return auto_date_histogram(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("variable_width_histogram").and_then(|v| v.as_object()) {
        return variable_width_histogram(t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("histogram").and_then(|v| v.as_object()) {
        return histogram_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("range").and_then(|v| v.as_object()) {
        return range_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("date_range").and_then(|v| v.as_object()) {
        return date_range_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("ip_range").and_then(|v| v.as_object()) {
        return ip_range_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(q) = def.get("filter").and_then(|v| v.as_object()) {
        let bucket: Vec<&Value> = docs.iter().filter(|d| doc_matches(idx, d, q)).copied().collect();
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(bucket.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &bucket, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if let Some(t) = def.get("filters").and_then(|v| v.as_object()) {
        return filters_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("missing").and_then(|v| v.as_object()) {
        let field = map_str(t, "field");
        let bucket: Vec<&Value> = docs
            .iter()
            .filter(|d| doc_field_values(idx, d, &field, prefix).is_empty())
            .copied()
            .collect();
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(bucket.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &bucket, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if let Some(g) = def.get("global").and_then(|v| v.as_object()) {
        if !g.is_empty() {
            return Err(ApiError::new(
                "parsing_exception",
                "Unknown key for a VALUE_OBJECT in [global].",
                400,
            ));
        }
        let all = all_live_docs(idx);
        let refs: Vec<&Value> = all.iter().collect();
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(all.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if let Some(t) = def.get("nested").and_then(|v| v.as_object()) {
        let path = map_str(t, "path");
        return nested_agg(&path, docs, idx, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("children").and_then(|v| v.as_object()) {
        let ty = map_str(t, "type");
        let children = children_docs(idx, docs, &ty);
        let refs: Vec<&Value> = children.iter().collect();
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(children.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if let Some(t) = def.get("parent").and_then(|v| v.as_object()) {
        let ty = map_str(t, "type");
        let parents = parent_docs(idx, docs, &ty);
        let refs: Vec<&Value> = parents.iter().collect();
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(parents.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if def.get("reverse_nested").and_then(|v| v.as_object()).is_some() {
        return reverse_nested_agg(name, def, docs, idx, prefix, typed);
    }
    if def.get("sampler").and_then(|v| v.as_object()).is_some()
        || def.get("diversified_sampler").and_then(|v| v.as_object()).is_some()
    {
        if sub_aggs(def).is_none() {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                "Sampler aggregation must be used with child aggregations.",
                500,
            ));
        }
        let is_div = def.get("diversified_sampler").is_some();
        let t = if is_div {
            def.get("diversified_sampler").and_then(|v| v.as_object()).cloned().unwrap_or_default()
        } else {
            def.get("sampler").and_then(|v| v.as_object()).cloned().unwrap_or_default()
        };
        let size = t.get("shard_size").and_then(to_f64).map(|f| f as usize).unwrap_or(100);
        let max_docs = t
            .get("max_docs_per_value")
            .and_then(to_f64)
            .map(|f| f as usize)
            .unwrap_or(1);
        let dv_field = map_str(&t, "field");
        let chosen: Vec<&Value> = if is_div && !dv_field.is_empty() {
            let mut counts: HashMap<String, usize> = HashMap::new();
            let mut out: Vec<&Value> = vec![];
            for d in docs {
                if out.len() >= size {
                    break;
                }
                let mut picked = false;
                for v in doc_field_values(idx, d, &dv_field, prefix) {
                    let k = value_to_string(&v).unwrap_or_default();
                    let c = counts.entry(k).or_insert(0);
                    if *c < max_docs {
                        *c += 1;
                        picked = true;
                        break;
                    }
                }
                if picked {
                    out.push(*d);
                }
            }
            out
        } else {
            let take = docs.len().min(size);
            docs.iter().take(take).copied().collect()
        };
        let mut res = Map::new();
        res.insert("doc_count".to_string(), json!(chosen.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, &chosen, idx, prefix, typed)? {
                for (k, v) in m {
                    res.insert(k, v);
                }
            }
        }
        return Ok(Value::Object(res));
    }
    if let Some(t) = def.get("composite").and_then(|v| v.as_object()) {
        return composite_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("adjacency_matrix").and_then(|v| v.as_object()) {
        return adjacency_matrix_agg(name, t, def, docs, idx, prefix, typed);
    }
    if let Some(t) = def.get("geohash_grid").and_then(|v| v.as_object()) {
        return geohash_grid_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("geotile_grid").and_then(|v| v.as_object()) {
        return geotile_grid_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("geo_distance").and_then(|v| v.as_object()) {
        return geo_distance_agg(t, docs, idx, prefix, sub_aggs(def), typed);
    }
    if let Some(t) = def.get("geo_bounds").and_then(|v| v.as_object()) {
        let field = map_str(t, "field");
        let wrap = t
            .get("wrap_longitude")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;
        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut n = 0usize;
        for d in docs {
            if let Some(raw) = agg_field_value(d, &field, prefix) {
                for (lat, lon) in geo_points_of_value(raw) {
                    n += 1;
                    min_lat = min_lat.min(lat);
                    max_lat = max_lat.max(lat);
                    min_lon = min_lon.min(lon);
                    max_lon = max_lon.max(lon);
                }
            }
        }
        if n == 0 {
            return Ok(json!({}));
        }
        let (mut lmin, mut lmax) = (min_lon, max_lon);
        if wrap && (lmax - lmin) > 180.0 {
            lmin = f64::MAX;
            lmax = f64::MIN;
            for d in docs {
                if let Some(raw) = agg_field_value(d, &field, prefix) {
                    for (_, lon) in geo_points_of_value(raw) {
                        let lon = if lon < 0.0 { lon + 360.0 } else { lon };
                        lmin = lmin.min(lon);
                        lmax = lmax.max(lon);
                    }
                }
            }
        }
        let br_lon = if lmax > 180.0 { lmax - 360.0 } else { lmax };
        return Ok(json!({
            "bounds": {
                "top_left": {"lat": max_lat, "lon": lmin},
                "bottom_right": {"lat": min_lat, "lon": br_lon},
            }
        }));
    }
    if let Some(t) = def.get("geo_centroid").and_then(|v| v.as_object()) {
        let field = map_str(t, "field");
        let mut lat_sum = 0.0;
        let mut lon_sum = 0.0;
        let mut n = 0usize;
        for d in docs {
            if let Some(raw) = agg_field_value(d, &field, prefix) {
                for (lat, lon) in geo_points_of_value(raw) {
                    lat_sum += lat;
                    lon_sum += lon;
                    n += 1;
                }
            }
        }
        if n == 0 {
            return Ok(json!({"count": 0}));
        }
        return Ok(json!({
            "count": n,
            "location": {"lat": lat_sum / n as f64, "lon": lon_sum / n as f64},
        }));
    }
    if def.contains_key("derivative") {
        return Ok(json!({}));
    }
    for agg in [
        "avg",
        "min",
        "max",
        "sum",
        "stats",
        "value_count",
        "cardinality",
        "median_absolute_deviation",
    ] {
        if let Some(t) = def.get(agg).and_then(|v| v.as_object()) {
            return metric_agg(agg, name, t, docs, idx, prefix);
        }
    }
    if let Some(t) = def.get("extended_stats").and_then(|v| v.as_object()) {
        return extended_stats_agg(t, name, docs, idx, prefix);
    }
    if let Some(t) = def.get("weighted_avg").and_then(|v| v.as_object()) {
        return weighted_avg_agg(t, docs, idx, prefix);
    }
    if let Some(t) = def.get("top_hits").and_then(|v| v.as_object()) {
        return top_hits_agg(name, def, t, docs, idx);
    }
    if let Some(t) = def.get("scripted_metric").and_then(|v| v.as_object()) {
        return scripted_metric_agg(name, t, docs, idx);
    }
    if let Some(t) = def.get("matrix_stats").and_then(|v| v.as_object()) {
        return matrix_stats_agg(idx, t, docs, prefix);
    }
    if let Some(t) = def.get("percentiles").and_then(|v| v.as_object()) {
        return percentiles_agg(t, name, docs, idx, prefix);
    }
    if let Some(t) = def.get("percentile_ranks").and_then(|v| v.as_object()) {
        return percentile_ranks_agg(t, name, docs, idx, prefix);
    }
    Ok(json!({}))
}

const TERMS_ALLOWED: &[&str] = &[
    "field",
    "size",
    "shard_size",
    "min_doc_count",
    "missing",
    "include",
    "exclude",
    "order",
    "keyed",
    "collect_mode",
    "execution_hint",
    "value_type",
    "format",
    "script",
    "show_term_doc_count_error",
];

fn parse_order_clause(order: Option<&Value>) -> Result<Vec<(String, bool)>, ApiError> {
    let mut out = vec![];
    let entries: Vec<&Value> = match order {
        None => vec![],
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
    };
    for e in entries {
        if let Some(m) = e.as_object() {
            for (k, v) in m {
                let dir = v.as_str().unwrap_or("asc");
                if dir != "asc" && dir != "desc" {
                    return Err(ApiError::parsing(&format!(
                        "Unknown order direction [{}]",
                        dir
                    )));
                }
                out.push((k.clone(), dir == "desc"));
            }
        }
    }
    Ok(out)
}

fn regex_matches(pattern: &str, s: &str) -> Option<bool> {
    let full = format!("^(?:{})$", pattern);
    let re = match regex::Regex::new(&full) {
        Ok(r) => r,
        Err(_) => return None,
    };
    Some(re.is_match(s))
}

fn lucene_regex_pos(pattern: &str) -> usize {
    let mut stack: Vec<usize> = vec![];
    for (i, c) in pattern.chars().enumerate() {
        match c {
            '[' => stack.push(i),
            ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.last().map(|i| i + 1).unwrap_or(0)
}

fn mix64_stafford(z: i64) -> i64 {
    let mut z = z as u64;
    z = (z ^ (z >> 32)).wrapping_mul(0x4cd6944c5cc20b6d);
    z = (z ^ (z >> 29)).wrapping_mul(0xfc12c5b19d3259e9);
    (z ^ (z >> 32)) as i64
}

fn murmur3_h1(data: &[u8]) -> u64 {
    const MASK: u64 = u64::MAX;
    const C1: u64 = 0x87c37b91114253d5;
    const C2: u64 = 0x4cf5ad432745937f;
    let mut h1: u64 = 0;
    let mut h2: u64 = 0;
    let length = data.len();
    let nblocks = length / 16;
    for i in 0..nblocks {
        let mut k1 = u64::from_le_bytes(data[i * 16..i * 16 + 8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(data[i * 16 + 8..i * 16 + 16].try_into().unwrap());
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
    }
    let tail = &data[nblocks * 16..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    let tl = tail.len();
    for i in 0..tl.min(8) {
        k1 ^= (tail[i] as u64) << (i * 8);
    }
    for i in 8..tl {
        k2 ^= (tail[i] as u64) << ((i - 8) * 8);
    }
    if tl > 8 {
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if tl > 0 {
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= length as u64;
    h2 ^= length as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51afd7ed558ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ceb9fe1a85ec53);
        k ^= k >> 33;
        k
    }
    h1 = fmix(h1);
    h2 = fmix(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    let _ = MASK;
    h1
}

pub fn contains_sampler(v: &Value) -> bool {
    match v {
        Value::Object(m) => {
            if m.contains_key("sampler") || m.contains_key("diversified_sampler") {
                return true;
            }
            m.values().any(contains_sampler)
        }
        Value::Array(a) => a.iter().any(contains_sampler),
        _ => false,
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    if a.is_number() || b.is_number() {
        if let (Some(x), Some(y)) = (to_f64(a), to_f64(b)) {
            return (x - y).abs() < 1e-12;
        }
    }
    a == b || value_to_string(a) == value_to_string(b)
}

fn value_matches_include_exclude(
    raw: &Value,
    key: &Value,
    include: Option<&Value>,
    exclude: Option<&Value>,
) -> bool {
    let s = value_to_string(raw).unwrap_or_default();
    let ks = value_to_string(key).unwrap_or_default();
    let check = |spec: &Value| -> bool {
        match spec {
            Value::Array(a) => a.iter().any(|e| value_eq(e, key)),
            Value::String(p) => regex_matches(p, &s).unwrap_or(ks == *p),
            Value::Object(m) => {
                if let Some(part) = m.get("partition").and_then(to_f64) {
                    let n = m.get("num_partitions").and_then(to_f64).unwrap_or(1.0) as u64;
                    if n == 0 {
                        return true;
                    }
                    if let Some(x) = to_f64(raw) {
                        let v = x as i64;
                        let h = mix64_stafford(v);
                        let ni = n as i64;
                        let m = ((h % ni) + ni) % ni;
                        return m as f64 == part;
                    }
                    let h = murmur3_h1(s.as_bytes());
                    return (h % n) as f64 == part;
                }
                true
            }
            v => value_eq(v, key),
        }
    };
    if let Some(inc) = include {
        if !check(inc) {
            return false;
        }
    }
    if let Some(exc) = exclude {
        if check(exc) {
            return false;
        }
    }
    true
}

fn key_string(v: &Value) -> String {
    match v {
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Null => "null".to_string(),
        other => value_to_string(other).unwrap_or_default(),
    }
}

struct TermBucket {
    raw: Value,
    key: Value,
    key_as_string: Option<String>,
    ord: OrdKey,
    count: usize,
    docs: Vec<Value>,
}

fn check_global_in_sub(name: &str, def: &Map<String, Value>) -> Result<(), ApiError> {
    if let Some(sub) = sub_aggs(def) {
        for (gname, gdef) in &sub {
            if let Some(gm) = gdef.as_object() {
                if gm.contains_key("global") {
                    return Err(ApiError::new(
                        "aggregation_execution_exception",
                        &format!(
                            "Aggregation [{}] cannot have a global sub-aggregation [{}]. Global aggregations can only be defined as top level aggregations",
                            name, gname
                        ),
                        500,
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn agg_join_field(idx: &Index) -> Option<String> {
    let props = idx.mappings.get("properties")?.as_object()?;
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        if def.get("type").and_then(|t| t.as_str()) == Some("join") {
            return Some(name.clone());
        }
    }
    None
}

fn agg_join_name(v: Option<&Value>) -> (String, String) {
    match v {
        Some(Value::String(s)) => (s.clone(), String::new()),
        Some(Value::Object(m)) => (
            m.get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("parent")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    }
}

fn join_relation_children(idx: &Index, field: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = vec![];
    let props = match idx.mappings.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return pairs,
    };
    let rel = props
        .get(field)
        .and_then(|d| d.as_object())
        .and_then(|d| d.get("relations"))
        .cloned();
    fn walk(parent: &str, v: &Value, out: &mut Vec<(String, String)>) {
        match v {
            Value::String(s) => out.push((parent.to_string(), s.clone())),
            Value::Array(a) => {
                for x in a {
                    walk(parent, x, out);
                }
            }
            Value::Object(m) => {
                for (k, x) in m {
                    walk(k, x, out);
                }
            }
            _ => {}
        }
    }
    if let Some(Value::Object(m)) = rel {
        for (k, v) in &m {
            walk(k, v, &mut pairs);
        }
    }
    pairs
}

fn children_docs(idx: &Index, docs: &[&Value], ty: &str) -> Vec<Value> {
    let field = match agg_join_field(idx) {
        Some(f) => f,
        None => return vec![],
    };
    if ty.is_empty() {
        return vec![];
    }
    let mut parent_ids: HashSet<String> = HashSet::new();
    for d in docs {
        if let Some(id) = d.get("_id").and_then(|v| v.as_str()) {
            parent_ids.insert(id.to_string());
        }
    }
    let mut out = vec![];
    for id in &idx.order {
        if let Some(d) = idx.docs.get(id) {
            let (name, parent) = agg_join_name(d.get(&field));
            if name == ty && !parent.is_empty() && parent_ids.contains(&parent) {
                out.push(d.clone());
            }
        }
    }
    out
}

fn parent_docs(idx: &Index, docs: &[&Value], ty: &str) -> Vec<Value> {
    let field = match agg_join_field(idx) {
        Some(f) => f,
        None => return vec![],
    };
    if ty.is_empty() {
        return vec![];
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = vec![];
    for d in docs {
        let (name, parent) = agg_join_name(d.get(&field));
        if name == ty && !parent.is_empty() && seen.insert(parent.clone()) {
            if let Some(pd) = idx.docs.get(&parent) {
                out.push(pd.clone());
            }
        }
    }
    out
}

thread_local! {
    pub static NESTED_PATHS: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
}

fn refresh_nested_paths(idx: &Index) {
    let mut paths: Vec<String> = vec![];
    if let Some(props) = idx
        .mappings
        .get("properties")
        .and_then(|v| v.as_object())
    {
        for (name, def) in props {
            collect_nested_paths(name, def, &mut paths);
        }
    }
    NESTED_PATHS.with(|np| {
        *np.borrow_mut() = paths;
    });
}

fn collect_nested_paths(prefix: &str, def: &Value, out: &mut Vec<String>) {
    let m = match def.as_object() {
        Some(m) => m,
        None => return,
    };
    if m.get("type").and_then(|v| v.as_str()) == Some("nested") {
        out.push(prefix.to_string());
        if let Some(props) = m.get("properties").and_then(|v| v.as_object()) {
            for (name, d2) in props {
                collect_nested_paths(&format!("{}.{}", prefix, name), d2, out);
            }
        }
    }
}

fn is_under_nested_path(field: &str) -> bool {
    NESTED_PATHS.with(|np| {
        for p in np.borrow().iter() {
            if field == p || field.starts_with(&format!("{}.", p)) {
                return true;
            }
        }
        false
    })
}

#[derive(Clone)]
pub struct NestElem {
    pub root: usize,
    pub objs: Vec<(usize, Value)>,
}

#[derive(Clone)]
pub struct NestFrame {
    pub path: String,
    pub elems: Vec<NestElem>,
    pub root_docs: Vec<Value>,
}

thread_local! {
    pub static NEST_STACK: std::cell::RefCell<Vec<NestFrame>> = std::cell::RefCell::new(Vec::new());
}

pub fn with_nest_frame<T>(frame: NestFrame, f: impl FnOnce() -> T) -> T {
    NEST_STACK.with(|s| s.borrow_mut().push(frame));
    let out = f();
    NEST_STACK.with(|s| {
        s.borrow_mut().pop();
    });
    out
}

fn reverse_nested_agg(
    name: &str,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let inside = NEST_STACK.with(|st| !st.borrow().is_empty())
        || docs
            .iter()
            .any(|d| d.get("__nest").and_then(|v| v.as_object()).is_some());
    if !inside {
        return Err(ApiError::new(
            "search_phase_execution_exception",
            &format!(
                "Reverse nested aggregation [{}] can only be used inside a [nested] aggregation",
                name
            ),
            400,
        ));
    }
    let path = def
        .get("reverse_nested")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let mut targets: Vec<Value> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for d in docs {
        let n = match d.get("__nest").and_then(|v| v.as_object()) {
            Some(n) => n,
            None => continue,
        };
        let target = if path.is_empty() {
            n.get("root").cloned()
        } else {
            n.get("anc")
                .and_then(|a| a.as_object())
                .and_then(|a| a.get(&path))
                .cloned()
        };
        let t = match target {
            Some(t) if !t.is_null() => t,
            _ => continue,
        };
        let key = t
            .get("_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| serde_json::to_string(&t).unwrap_or_default());
        if seen.insert(key) {
            if path.is_empty() {
                targets.push(t);
            } else {
                targets.push(json!({"__nest": {
                    "path": path,
                    "elem": t,
                    "root": t,
                    "anc": {},
                }}));
            }
        }
    }
    let mut body = Map::new();
    body.insert("doc_count".to_string(), json!(targets.len()));
    if let Some(sub) = sub_aggs(def) {
        let refs: Vec<&Value> = targets.iter().collect();
        if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
            for (sk, sv) in m {
                body.insert(sk, sv);
            }
        }
    }
    Ok(Value::Object(body))
}

fn agg_script_source(t: &Map<String, Value>) -> Option<String> {
    let s = t.get("script")?;
    if let Some(m) = s.as_object() {
        m.get("source")
            .or_else(|| m.get("inline"))
            .and_then(|v| v.as_str())
            .map(|x| x.to_string())
    } else {
        s.as_str().map(|x| x.to_string())
    }
}

fn agg_script_params(t: &Map<String, Value>) -> Map<String, Value> {
    t.get("script")
        .and_then(|s| s.as_object())
        .and_then(|m| m.get("params"))
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default()
}

fn run_agg_script(
    idx: &Index,
    doc: &Value,
    src: &str,
    base: &Map<String, Value>,
    value: Option<&Value>,
) -> Result<Value, ApiError> {
    let vm = match doc.as_object() {
        Some(dm) => {
            let mut dm2 = dm.clone();
            dm2.remove("_id");
            match crate::fields::doc_values_for(idx, "", &dm2) {
            Ok((vals, _)) => {
                let mut vm = Map::new();
                for (k, v) in vals {
                    vm.insert(k, Value::Array(v));
                }
                vm
            }
            Err(_) => Map::new(),
            }
        }
        None => Map::new(),
    };
    let ctx = QueryCtx::with_values(idx, "", doc, vm);
    let mut params = base.clone();
    if let Some(v) = value {
        params.insert("_value".to_string(), v.clone());
    }
    let env = crate::painless::ScriptEnv { c: &ctx, params };
    env.eval_script_expression(src).map_err(|e| {
        ApiError::new("illegal_argument_exception", &e, 400)
    })
}

fn analyze_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn sig_heuristic(t: &Map<String, Value>) -> (String, bool, bool) {
    for k in ["jlh", "percentage", "chi_square", "mutual_information", "gnd"] {
        if let Some(v) = t.get(k) {
            let mut inc_neg = false;
            let mut bg_superset = true;
            if let Some(o) = v.as_object() {
                if let Some(b) = o.get("include_negatives").and_then(|v| v.as_bool()) {
                    inc_neg = b;
                }
                if let Some(b) = o.get("background_is_superset").and_then(|v| v.as_bool()) {
                    bg_superset = b;
                }
            }
            return (k.to_string(), inc_neg, bg_superset);
        }
    }
    ("jlh".to_string(), false, true)
}

fn sig_score(kind: &str, inc_neg: bool, bg_superset: bool, sf: f64, ss: f64, gf: f64, gs: f64) -> f64 {
    let (n00, n01, n10, n11, n0_, n1_, n_0, n_1, n) = if bg_superset {
        (
            gs - gf - (ss - sf),
            ss - sf,
            gf - sf,
            sf,
            gs - gf,
            gf,
            gs - ss,
            ss,
            gs,
        )
    } else {
        (
            gs - gf,
            ss - sf,
            gf,
            sf,
            gs - gf + ss - sf,
            gf + sf,
            gs,
            ss,
            gs + ss,
        )
    };
    match kind {
        "percentage" => {
            if gf == 0.0 {
                0.0
            } else {
                sf / gf
            }
        }
        "chi_square" => {
            if !inc_neg && n11 / n_1 < n10 / n_0 {
                return f64::NEG_INFINITY;
            }
            n * (n11 * n00 - n01 * n10).powi(2) / (n_1 * n1_ * n0_ * n_0)
        }
        "mutual_information" => {
            let mi_term = |nxy: f64, nx_: f64, n_y: f64, nn: f64| -> f64 {
                let num = (nn * nxy).abs();
                let den = (nx_ * n_y).abs();
                let fac = (nxy / nn).abs();
                if num < 1e-7 && fac < 1e-7 {
                    0.0
                } else {
                    fac * (num / den).ln()
                }
            };
            let mut score = (mi_term(n00, n0_, n_0, n)
                + mi_term(n01, n0_, n_1, n)
                + mi_term(n10, n1_, n_0, n)
                + mi_term(n11, n1_, n_1, n))
                / std::f64::consts::LN_2;
            if score.is_nan() {
                score = f64::NEG_INFINITY;
            }
            if !inc_neg && n11 / n_1 < n10 / n_0 {
                score = f64::NEG_INFINITY;
            }
            score
        }
        "gnd" => {
            let fx = n1_;
            let fy = n_1;
            let fxy = n11;
            if fxy == 0.0 {
                return 0.0;
            }
            if fx == fy && fx == fxy {
                return 1.0;
            }
            let sc = (fx.ln().max(fy.ln()) - fxy.ln()) / (n.ln() - fx.ln().min(fy.ln()));
            (-sc).exp()
        }
        _ => {
            if ss == 0.0 || gs == 0.0 {
                return 0.0;
            }
            let gf2 = if gf == 0.0 { 1.0 } else { gf };
            let f = sf / ss;
            let b = gf2 / gs;
            let d = f - b;
            if d <= 0.0 {
                0.0
            } else {
                d * (f / b)
            }
        }
    }
}

fn search_phase_illegal(reason: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
    e.root_cause = Some(Box::new(ApiError::new(
        "illegal_argument_exception",
        reason,
        400,
    )));
    e.caused_by = Some(Box::new(ApiError::new(
        "illegal_argument_exception",
        reason,
        400,
    )));
    e
}

fn metric_num_type_check(idx: &Index, field: &str, prefix: &str, agg: &str) -> Result<(), ApiError> {
    if field.is_empty() {
        return Ok(());
    }
    let resolved = crate::eval::resolve_alias_field(idx, &resolve_field(field, prefix));
    let ft = match idx.field_types.get(&resolved) {
        Some(f) => f.clone(),
        None => return Ok(()),
    };
    const NUMERIC: &[&str] = &[
        "integer",
        "long",
        "short",
        "byte",
        "unsigned_long",
        "double",
        "float",
        "half_float",
        "scaled_float",
        "date",
        "date_nanos",
        "boolean",
        "token_count",
        "rank_feature",
        "rank_features",
    ];
    if NUMERIC.contains(&ft.as_str()) {
        return Ok(());
    }
    if ft == "text" || ft == "match_only_text" || ft == "search_as_you_type" || ft == "wildcard" {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!(
                "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                field
            ),
            400,
        ));
    }
    Err(search_phase_illegal(&format!(
        "Field [{}] of type [{}] is not supported for aggregation [{}]",
        field, ft, agg
    )))
}

fn java_numeric_format(v: f64, pattern: &str) -> String {
    let (int_part, dec_part) = match pattern.find('.') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => (pattern, ""),
    };
    let min_int = int_part.chars().filter(|c| *c == '0').count();
    let dec_digits = dec_part.chars().filter(|c| *c == '0' || *c == '#').count();
    let neg = v < 0.0;
    let mut out = format!("{:.*}", dec_digits, v.abs());
    if min_int > 0 {
        let (ip, dp) = match out.find('.') {
            Some(i) => (out[..i].to_string(), out[i..].to_string()),
            None => (out.clone(), String::new()),
        };
        let padded = format!("{}{}", "0".repeat(min_int.saturating_sub(ip.len())), ip);
        out = format!("{}{}", padded, dp);
    }
    if neg {
        format!("-{}", out)
    } else {
        out
    }
}

fn metric_as_string(v: f64, is_date: bool, fmt: &str) -> Option<String> {
    if is_date {
        let ms = v as i64;
        Some(if fmt.is_empty() {
            format_epoch_ms(ms)
        } else {
            dh_format(fmt, ms)
        })
    } else if !fmt.is_empty() {
        Some(java_numeric_format(v, fmt))
    } else {
        None
    }
}

fn stats_json(vals: &[f64], is_date: bool, fmt: &str) -> Value {
    if vals.is_empty() {
        return json!({
            "count": 0,
            "min": Value::Null,
            "max": Value::Null,
            "avg": Value::Null,
            "sum": 0.0,
        });
    }
    let mut m = Map::new();
    m.insert("count".to_string(), json!(vals.len()));
    m.insert("min".to_string(), json_num(min_of(vals)));
    m.insert("max".to_string(), json_num(max_of(vals)));
    m.insert("avg".to_string(), json_num(mean(vals)));
    m.insert("sum".to_string(), json_num(sum_of(vals)));
    for (k, ask) in [
        ("min", "min_as_string"),
        ("max", "max_as_string"),
        ("avg", "avg_as_string"),
        ("sum", "sum_as_string"),
    ] {
        let fv = m.get(k).and_then(|v| v.as_f64());
        if let Some(fv) = fv {
            if let Some(s) = metric_as_string(fv, is_date, fmt) {
                m.insert(ask.to_string(), json!(s));
            }
        }
    }
    Value::Object(m)
}

fn metric_agg(
    agg: &str,
    name: &str,
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    let allowed: &[&str] = match agg {
        "cardinality" => &[
            "field",
            "script",
            "missing",
            "format",
            "precision_threshold",
            "value_type",
            "execution_hint",
        ],
        _ => &["field", "script", "missing", "format", "value_type"],
    };
    for k in t.keys() {
        if k == "meta" {
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(ApiError::new(
                "x_content_parse_exception",
                &format!("[{}] unknown field [{}]", agg, k),
                400,
            ));
        }
    }
    let field = map_str(t, "field");
    let script = agg_script_source(t);
    if field.is_empty() && script.is_none() {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified. ",
            400,
        ));
    }
    let numeric_metric = matches!(
        agg,
        "avg" | "min" | "max" | "sum" | "stats" | "median_absolute_deviation"
    );
    if !field.is_empty() && numeric_metric {
        metric_num_type_check(idx, &field, prefix, agg)?;
    }
    if agg == "cardinality" {
        if let Some(p) = t.get("precision_threshold").and_then(to_f64) {
            if p < 0.0 {
                let cause = ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "[precisionThreshold] must be greater than or equal to 0. Found [{}] in [{}]",
                        p as i64, name
                    ),
                    400,
                );
                return Err(ApiError::new(
                    "x_content_parse_exception",
                    "Failed to build [cardinality] after last required field arrived",
                    400,
                )
                .caused_by(cause));
            }
        }
    }
    let script_params = agg_script_params(t);
    let missing = t.get("missing").cloned();
    let mut raws: Vec<Value> = vec![];
    match &script {
        Some(src) if field.is_empty() => {
            for d in docs {
                let r = run_agg_script(idx, d, src, &script_params, None)?;
                if !r.is_null() {
                    raws.push(r);
                }
            }
        }
        sopt => {
            for d in docs {
                let vals = doc_field_values(idx, d, &field, prefix);
                if vals.is_empty() {
                    if let Some(m) = &missing {
                        raws.push(m.clone());
                    }
                    continue;
                }
                for v in vals {
                    match sopt {
                        Some(src) => {
                            let r = run_agg_script(idx, d, src, &script_params, Some(&v))?;
                            if !r.is_null() {
                                raws.push(r);
                            }
                        }
                        None => raws.push(v),
                    }
                }
            }
        }
    }
    if agg == "value_count" {
        return Ok(json!({"value": raws.len()}));
    }
    if agg == "cardinality" {
        let mut seen = HashSet::new();
        for v in &raws {
            match v {
                Value::Array(_) | Value::Object(_) => {
                    seen.insert(serde_json::to_string(v).unwrap_or_default());
                }
                _ => {
                    for s in doc_strings(v) {
                        seen.insert(s);
                    }
                }
            }
        }
        let distinct = seen.len();
        let approx = match t.get("precision_threshold").and_then(to_f64) {
            Some(th) if (distinct as f64) > th && distinct > 16 => distinct - 1,
            _ => distinct,
        };
        return Ok(json!({"value": approx}));
    }
    if agg == "median_absolute_deviation" {
        let mut vals: Vec<f64> = raws.iter().filter_map(to_f64).collect();
        if vals.is_empty() {
            return Ok(json!({"value": Value::Null}));
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = vals[vals.len() / 2];
        let mut devs: Vec<f64> = vals.iter().map(|v| (v - med).abs()).collect();
        devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        return Ok(json!({"value": json_num(devs[devs.len() / 2])}));
    }
    let is_date = field_is_date(idx, &field, prefix);
    let vals: Vec<f64> = raws
        .iter()
        .filter_map(|v| {
            to_f64(v).or_else(|| {
                if is_date {
                    date_ms_of(v).map(|ms| ms as f64)
                } else {
                    None
                }
            })
        })
        .collect();
    let fmt = map_str(t, "format");
    if agg == "stats" {
        return Ok(stats_json(&vals, is_date, &fmt));
    }
    let v: Value = match agg {
        "sum" => json_num(sum_of(&vals)),
        _ if vals.is_empty() => Value::Null,
        "avg" => json_num(mean(&vals)),
        "min" => json_num(min_of(&vals)),
        "max" => json_num(max_of(&vals)),
        _ => Value::Null,
    };
    let mut out = Map::new();
    let as_string = v
        .as_f64()
        .and_then(|fv| metric_as_string(fv, is_date, &fmt));
    out.insert("value".to_string(), v);
    if let Some(s) = as_string {
        out.insert("value_as_string".to_string(), json!(s));
    }
    Ok(Value::Object(out))
}

fn field_is_date(idx: &Index, field: &str, prefix: &str) -> bool {
    matches!(
        idx.field_types
            .get(resolve_field(field, prefix))
            .map(|s| s.as_str()),
        Some("date") | Some("date_nanos")
    )
}

fn java_double_string(f: f64) -> String {
    if f == f.trunc() && f.is_finite() && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        format!("{}", f)
    }
}

fn collect_metric_values(
    idx: &Index,
    docs: &[&Value],
    field: &str,
    prefix: &str,
    script: &Option<String>,
    script_params: &Map<String, Value>,
    is_date: bool,
) -> Result<Vec<f64>, ApiError> {
    let mut out = vec![];
    match script {
        Some(src) if field.is_empty() => {
            for d in docs {
                let r = run_agg_script(idx, d, src, script_params, None)?;
                if !r.is_null() {
                    if let Some(f) = to_f64(&r) {
                        out.push(f);
                    }
                }
            }
        }
        sopt => {
            for d in docs {
                for v in doc_field_values(idx, d, field, prefix) {
                    match sopt {
                        Some(src) => {
                            let r = run_agg_script(idx, d, src, script_params, Some(&v))?;
                            if !r.is_null() {
                                if let Some(f) = to_f64(&r) {
                                    out.push(f);
                                }
                            }
                        }
                        None => {
                            let f = to_f64(&v).or_else(|| {
                                if is_date {
                                    date_ms_of(&v).map(|ms| ms as f64)
                                } else {
                                    None
                                }
                            });
                            if let Some(f) = f {
                                out.push(f);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn search_phase_agg_exec(reason: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 500);
    e.root_cause = Some(Box::new(ApiError::new(
        "aggregation_execution_exception",
        reason,
        500,
    )));
    e.caused_by = Some(Box::new(ApiError::new(
        "aggregation_execution_exception",
        reason,
        500,
    )));
    e
}

fn wavg_numeric(v: &Value, is_date: bool) -> Option<f64> {
    to_f64(v).or_else(|| {
        if is_date {
            date_ms_of(v).map(|ms| ms as f64)
        } else {
            None
        }
    })
}

fn mvs_values_for_doc(
    cfg: &Map<String, Value>,
    d: &Value,
    idx: &Index,
    prefix: &str,
    is_date: bool,
) -> Result<Vec<f64>, ApiError> {
    let field = map_str(cfg, "field");
    let script = agg_script_source(cfg);
    let missing = cfg.get("missing");
    let mut out = vec![];
    match &script {
        Some(src) if field.is_empty() => {
            let params = agg_script_params(cfg);
            let r = run_agg_script(idx, d, src, &params, None)?;
            if !r.is_null() {
                if let Some(f) = to_f64(&r) {
                    out.push(f);
                }
            }
        }
        _ => {
            let raw = doc_field_values(idx, d, &field, prefix);
            if raw.is_empty() {
                if let Some(m) = missing {
                    if let Some(f) = wavg_numeric(m, is_date) {
                        out.push(f);
                    }
                }
            } else {
                for v in raw {
                    if let Some(f) = wavg_numeric(&v, is_date) {
                        out.push(f);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn weighted_avg_type_check(idx: &Index, field: &str, prefix: &str) -> Result<(), ApiError> {
    if field.is_empty() {
        return Ok(());
    }
    let resolved = crate::eval::resolve_alias_field(idx, &resolve_field(field, prefix));
    let ft = match idx.field_types.get(&resolved) {
        Some(f) => f.clone(),
        None => return Ok(()),
    };
    const OK: &[&str] = &[
        "integer",
        "long",
        "short",
        "byte",
        "unsigned_long",
        "double",
        "float",
        "half_float",
        "scaled_float",
        "date",
        "date_nanos",
    ];
    if OK.contains(&ft.as_str()) {
        return Ok(());
    }
    Err(search_phase_illegal(&format!(
        "Expected numeric type on field [{}], but got [{}]",
        field, ft
    )))
}

fn weighted_avg_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    let value_cfg = match t.get("value").and_then(|v| v.as_object()) {
        Some(v) => v.clone(),
        None => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                "Could not find field name [value] in multiValuesSource",
                400,
            ))
        }
    };
    let weight_cfg = match t.get("weight").and_then(|v| v.as_object()) {
        Some(v) => v.clone(),
        None => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                "Could not find field name [weight] in multiValuesSource",
                400,
            ))
        }
    };
    let value_field = map_str(&value_cfg, "field");
    let weight_field = map_str(&weight_cfg, "field");
    weighted_avg_type_check(idx, &value_field, prefix)?;
    weighted_avg_type_check(idx, &weight_field, prefix)?;
    let value_is_date = !value_field.is_empty() && field_is_date(idx, &value_field, prefix);
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for d in docs {
        let weights = mvs_values_for_doc(&weight_cfg, d, idx, prefix, false)?;
        if weights.is_empty() {
            continue;
        }
        if weights.len() > 1 {
            return Err(search_phase_agg_exec(
                "Encountered more than one weight for a single document. Consider using a script to compute the weight at a higher level.",
            ));
        }
        let w = weights[0];
        let vals = mvs_values_for_doc(&value_cfg, d, idx, prefix, value_is_date)?;
        for v in vals {
            num += v * w;
            den += w;
        }
    }
    if den == 0.0 {
        return Ok(json!({"value": Value::Null}));
    }
    let avg = num / den;
    let mut m = Map::new();
    m.insert("value".to_string(), json_num(avg));
    let fmt = map_str(t, "format");
    if !fmt.is_empty() && avg.is_finite() {
        m.insert(
            "value_as_string".to_string(),
            json!(java_numeric_format(avg, &fmt)),
        );
    }
    Ok(Value::Object(m))
}

thread_local! {
    pub static AGG_SCORES: std::cell::RefCell<HashMap<(String, String), f64>> =
        std::cell::RefCell::new(HashMap::new());
    pub static AGG_HL_TOKENS: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

pub fn set_agg_search_context(scores: HashMap<(String, String), f64>, tokens: HashSet<String>) {
    AGG_SCORES.with(|s| *s.borrow_mut() = scores);
    AGG_HL_TOKENS.with(|t| *t.borrow_mut() = tokens);
}

fn th_parse_sorts(t: &Map<String, Value>) -> Vec<(String, bool)> {
    let mut out = vec![];
    let list: Vec<Value> = match t.get("sort") {
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
        None => return out,
    };
    for e in list {
        match e {
            Value::String(s) => out.push((s, false)),
            Value::Object(m) => {
                for (k, v) in m {
                    let desc = match &v {
                        Value::String(s) => s == "desc",
                        Value::Object(o) => o
                            .get("order")
                            .and_then(|x| x.as_str())
                            .map(|s| s == "desc")
                            .unwrap_or(false),
                        _ => false,
                    };
                    out.push((k, desc));
                }
            }
            _ => {}
        }
    }
    out
}

fn th_sort_value(idx: &Index, doc: &Value, id: &str, field: &str, desc: bool) -> Option<Value> {
    if field == "_id" {
        return doc.get("_id").cloned();
    }
    let f = field.to_string();
    let mut is_date = false;
    if let Some(def) = crate::eval::mapping_field_def(idx, &f) {
        let ft = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
        is_date = ft == "date" || ft == "date_nanos";
    }
    let raw = idx.doc_values.get(id)?.get(&f)?;
    let mut list = crate::eval::array_values(raw);
    if list.is_empty() {
        return None;
    }
    list.sort_by(cmp_vals_asc);
    let chosen = if desc {
        list.pop()
    } else {
        list.into_iter().next()
    }?;
    if is_date {
        if let Some(ms) = date_ms_of(&chosen) {
            return Some(json!(ms));
        }
    }
    Some(chosen)
}

fn th_cmp_opts(a: &Option<Value>, b: &Option<Value>, desc: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let c = cmp_vals_asc(x, y);
            if desc {
                c.reverse()
            } else {
                c
            }
        }
    }
}

fn top_hits_source(doc: &Value, spec: Option<&Value>) -> Value {
    let full = {
        let mut m = doc.as_object().cloned().unwrap_or_default();
        m.remove("_id");
        m
    };
    let mut includes: Vec<String> = vec![];
    let mut excludes: Vec<String> = vec![];
    match spec {
        None | Some(Value::Bool(_)) => return Value::Object(full),
        Some(Value::String(s)) => includes.push(s.clone()),
        Some(Value::Array(a)) => {
            for e in a {
                if let Some(s) = value_to_string(e) {
                    includes.push(s);
                }
            }
        }
        Some(Value::Object(m)) => {
            for (key, target) in [("includes", &mut includes), ("excludes", &mut excludes)] {
                match m.get(key) {
                    Some(Value::String(s)) => target.push(s.clone()),
                    Some(Value::Array(a)) => {
                        for e in a {
                            if let Some(s) = value_to_string(e) {
                                target.push(s);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => return Value::Object(full),
    }
    let mut out = Map::new();
    if includes.is_empty() {
        out = full.clone();
    } else {
        for f in &includes {
            if let Some(v) = full.get(f) {
                out.insert(f.clone(), v.clone());
            }
        }
    }
    for f in &excludes {
        out.remove(f);
    }
    Value::Object(out)
}

fn parse_dv_names(raw: Option<&Value>) -> Vec<String> {
    let mut out = vec![];
    if let Some(arr) = raw.and_then(|v| v.as_array()) {
        for e in arr {
            match e {
                Value::String(s) => out.push(s.clone()),
                Value::Object(m) => {
                    if let Some(s) = m.get("field").and_then(|v| v.as_str()) {
                        out.push(s.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn top_hits_agg(
    name: &str,
    def: &Map<String, Value>,
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
) -> Result<Value, ApiError> {
    if sub_aggs(def).is_some() {
        return Err(ApiError::new(
            "aggregation_initialization_exception",
            &format!(
                "Aggregator [{}] of type [top_hits] cannot accept sub-aggregations",
                name
            ),
            500,
        ));
    }
    let from = t
        .get("from")
        .and_then(to_f64)
        .map(|f| if f > 0.0 { f as usize } else { 0 })
        .unwrap_or(0);
    let size = t
        .get("size")
        .and_then(to_f64)
        .map(|f| if f > 0.0 { f as usize } else { 0 })
        .unwrap_or(3);
    let window = setting_int(&idx.settings, "max_inner_result_window").unwrap_or(100) as usize;
    if from + size > window {
        return Err(search_phase_illegal(&format!(
            "Top hits result window is too large, the top hits aggregator [{}]'s from + size must be less than or equal to: [{}] but was [{}]. This limit can be set by changing the [index.max_inner_result_window] index level setting.",
            name,
            window,
            from + size
        )));
    }
    let sorts = th_parse_sorts(t);
    let track_scores = t
        .get("track_scores")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let score_sort = sorts.iter().all(|(f, _)| f == "_score");
    let omit_scores = !score_sort && !track_scores;
    let source_raw = t.get("_source");
    let stored_non_empty = match t.get("stored_fields") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    };
    let omit_source = stored_non_empty || matches!(source_raw, Some(Value::Bool(false)));

    struct Cand<'a> {
        doc: &'a Value,
        id: String,
        score: f64,
        sort_vals: Vec<Option<Value>>,
    }
    let mut cands: Vec<Cand> = vec![];
    for d in docs {
        let id = d
            .get("_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let score = AGG_SCORES
            .with(|s| s.borrow().get(&(idx.name.clone(), id.clone())).copied())
            .unwrap_or(1.0);
        let sort_vals: Vec<Option<Value>> = sorts
            .iter()
            .map(|(f, desc)| th_sort_value(idx, d, &id, f, *desc))
            .collect();
        cands.push(Cand {
            doc: d,
            id,
            score,
            sort_vals,
        });
    }
    if sorts.is_empty() {
        cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    } else {
        cands.sort_by(|a, b| {
            let mut c = Ordering::Equal;
            for (i, (_f, desc)) in sorts.iter().enumerate() {
                c = th_cmp_opts(&a.sort_vals[i], &b.sort_vals[i], *desc);
                if c != Ordering::Equal {
                    break;
                }
            }
            c
        });
    }
    let total = cands.len();
    let max_score = if !omit_scores && !cands.is_empty() {
        Some(cands.iter().map(|c| c.score).fold(f64::NEG_INFINITY, f64::max))
    } else {
        None
    };
    let end = (from + size).min(total);
    let page: Vec<&Cand> = if from >= total {
        vec![]
    } else {
        cands[from..end].iter().collect()
    };
    let hl_fields: Vec<String> = t
        .get("highlight")
        .and_then(|h| h.as_object())
        .and_then(|h| h.get("fields"))
        .and_then(|f| f.as_object())
        .map(|m| {
            let mut v: Vec<String> = m.keys().cloned().collect();
            v.sort();
            v
        })
        .unwrap_or_default();
    let dv_fields = parse_dv_names(t.get("docvalue_fields"));
    let script_fields = t
        .get("script_fields")
        .and_then(|s| s.as_object())
        .cloned()
        .unwrap_or_default();
    let mut hits = vec![];
    for c in &page {
        let mut entry = Map::new();
        entry.insert("_index".to_string(), json!(idx.name));
        if !stored_non_empty {
            entry.insert("_id".to_string(), json!(c.id));
        }
        entry.insert(
            "_score".to_string(),
            if omit_scores {
                Value::Null
            } else {
                json!(c.score)
            },
        );
        if !omit_source {
            entry.insert(
                "_source".to_string(),
                top_hits_source(c.doc, source_raw),
            );
        }
        if !sorts.is_empty() {
            let sv: Vec<Value> = c
                .sort_vals
                .iter()
                .map(|v| v.clone().unwrap_or(Value::Null))
                .collect();
            entry.insert("sort".to_string(), Value::Array(sv));
        }
        if t.get("version").and_then(|v| v.as_bool()).unwrap_or(false) {
            entry.insert("_version".to_string(), json!(1));
        }
        if t
            .get("seq_no_primary_term")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let seq = idx
                .order
                .iter()
                .position(|x| x == &c.id)
                .map(|p| p as i64)
                .unwrap_or(0);
            entry.insert("_seq_no".to_string(), json!(seq));
            entry.insert("_primary_term".to_string(), json!(1));
        }
        let mut fields: Map<String, Value> = Map::new();
        for f in &dv_fields {
            if let Some(vals) = idx
                .doc_values
                .get(&c.id)
                .and_then(|dv| dv.get(f))
                .map(crate::eval::array_values)
            {
                if !vals.is_empty() {
                    fields.insert(f.clone(), Value::Array(vals));
                }
            }
        }
        for (fname, spec) in &script_fields {
            if let Some(sm) = spec.as_object() {
                if let Some(src) = agg_script_source(sm) {
                    let params = agg_script_params(sm);
                    let v = run_agg_script(idx, c.doc, &src, &params, None)?;
                    fields.insert(fname.clone(), Value::Array(vec![v]));
                }
            }
        }
        if !fields.is_empty() {
            entry.insert("fields".to_string(), Value::Object(fields));
        }
        if !hl_fields.is_empty() {
            let tokens: HashSet<String> = AGG_HL_TOKENS.with(|t| t.borrow().clone());
            let mut hl = Map::new();
            for f in &hl_fields {
                if let Some(v) = field_value(c.doc, f) {
                    if let Some(s) = value_to_string(v) {
                        hl.insert(
                            f.clone(),
                            json!([crate::search::highlight_fragment(&s, &tokens)]),
                        );
                    }
                }
            }
            if !hl.is_empty() {
                entry.insert("highlight".to_string(), Value::Object(hl));
            }
        }
        hits.push(Value::Object(entry));
    }
    Ok(json!({
        "hits": {
            "total": {"value": total, "relation": "eq"},
            "max_score": match max_score { Some(m) => json!(m), None => Value::Null },
            "hits": hits,
        }
    }))
}

fn doc_script_vm(idx: &Index, doc: &Value) -> Map<String, Value> {
    let dm2 = match doc.as_object() {
        Some(dm) => {
            let mut d = dm.clone();
            d.remove("_id");
            d
        }
        None => return Map::new(),
    };
    match crate::fields::doc_values_for(idx, "", &dm2) {
        Ok((vals, _)) => {
            let mut vm = Map::new();
            for (k, v) in vals {
                vm.insert(k, Value::Array(v));
            }
            vm
        }
        Err(_) => Map::new(),
    }
}

fn scripted_metric_script(t: &Map<String, Value>, key: &str) -> Option<String> {
    t.get(key).and_then(|v| {
        if let Some(s) = v.as_str() {
            Some(s.to_string())
        } else {
            v.as_object()
                .and_then(|m| m.get("source"))
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        }
    })
}

fn scripted_metric_agg(
    name: &str,
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
) -> Result<Value, ApiError> {
    let map_src = match scripted_metric_script(t, "map_script") {
        Some(s) => s,
        None => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                "Required [map_script]",
                400,
            ))
        }
    };
    let combine_src = match scripted_metric_script(t, "combine_script") {
        Some(s) => s,
        None => {
            return Err(search_phase_illegal(&format!(
                "[combineScript] must not be null: [{}]",
                name
            )))
        }
    };
    let reduce_src = match scripted_metric_script(t, "reduce_script") {
        Some(s) => s,
        None => {
            return Err(search_phase_illegal(&format!(
                "[reduceScript] must not be null: [{}]",
                name
            )))
        }
    };
    let init_src = scripted_metric_script(t, "init_script");
    let params: Map<String, Value> = t
        .get("params")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    let script_err = |e: String| -> ApiError {
        ApiError::new("search_phase_execution_exception", &e, 500)
    };
    let empty_doc = Value::Null;
    let mut locals: Map<String, Value> = Map::new();
    locals.insert("state".to_string(), Value::Object(Map::new()));
    if let Some(src) = &init_src {
        let ctx = QueryCtx::with_values(idx, "", &empty_doc, Map::new());
        let env = crate::painless::ScriptEnv {
            c: &ctx,
            params: params.clone(),
        };
        crate::painless::exec_statements(src, &mut locals, &env).map_err(script_err)?;
    }
    for d in docs {
        let vm = doc_script_vm(idx, d);
        let ctx = QueryCtx::with_values(idx, "", d, vm);
        let env = crate::painless::ScriptEnv {
            c: &ctx,
            params: params.clone(),
        };
        crate::painless::exec_statements(&map_src, &mut locals, &env).map_err(script_err)?;
    }
    let combined = {
        let ctx = QueryCtx::with_values(idx, "", &empty_doc, Map::new());
        let env = crate::painless::ScriptEnv {
            c: &ctx,
            params: params.clone(),
        };
        crate::painless::exec_statements(&combine_src, &mut locals, &env).map_err(script_err)?
    };
    locals.insert("states".to_string(), Value::Array(vec![combined]));
    let result = {
        let ctx = QueryCtx::with_values(idx, "", &empty_doc, Map::new());
        let env = crate::painless::ScriptEnv {
            c: &ctx,
            params,
        };
        crate::painless::exec_statements(&reduce_src, &mut locals, &env).map_err(script_err)?
    };
    Ok(json!({"value": result}))
}

fn matrix_stats_agg(
    idx: &Index,
    t: &Map<String, Value>,
    docs: &[&Value],
    prefix: &str,
) -> Result<Value, ApiError> {
    let fields: Vec<String> = t
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let k = fields.len();
    let mut cols: Vec<Vec<f64>> = vec![Vec::new(); k];
    'docs: for d in docs {
        let mut vals: Vec<f64> = Vec::with_capacity(k);
        for f in &fields {
            let raw = doc_field_values(idx, d, f, prefix);
            let mut num = None;
            for v in &raw {
                if let Some(x) = to_f64(v) {
                    num = Some(x);
                    break;
                }
            }
            match num {
                Some(x) => vals.push(x),
                None => continue 'docs,
            }
        }
        for (i, v) in vals.into_iter().enumerate() {
            cols[i].push(v);
        }
    }
    let n = cols.first().map(|c| c.len()).unwrap_or(0) as f64;
    let means: Vec<f64> = cols
        .iter()
        .map(|c| {
            if c.is_empty() {
                0.0
            } else {
                c.iter().sum::<f64>() / n
            }
        })
        .collect();
    let m2s: Vec<f64> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| c.iter().map(|x| (x - means[i]).powi(2)).sum())
        .collect();
    let m3s: Vec<f64> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| c.iter().map(|x| (x - means[i]).powi(3)).sum())
        .collect();
    let m4s: Vec<f64> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| c.iter().map(|x| (x - means[i]).powi(4)).sum())
        .collect();
    let mut covs = vec![vec![0.0f64; k]; k];
    for i in 0..k {
        for j in i..k {
            let mut s = 0.0;
            for r in 0..cols[i].len() {
                s += (cols[i][r] - means[i]) * (cols[j][r] - means[j]);
            }
            covs[i][j] = s;
            covs[j][i] = s;
        }
    }
    let variance = |i: usize| if n > 1.0 { m2s[i] / (n - 1.0) } else { 0.0 };
    let covariance = |i: usize, j: usize| {
        if n > 1.0 {
            covs[i][j] / (n - 1.0)
        } else {
            0.0
        }
    };
    let mut out_fields = Vec::new();
    for i in 0..k {
        let var = variance(i);
        let skew = if m2s[i] > 0.0 {
            n.sqrt() * m3s[i] / m2s[i].powf(1.5)
        } else {
            0.0
        };
        let kurt = if m2s[i] > 0.0 {
            n * m4s[i] / (m2s[i] * m2s[i])
        } else {
            0.0
        };
        let mut cov = Map::new();
        let mut corr = Map::new();
        for j in 0..k {
            let c = covariance(i, j);
            cov.insert(fields[j].clone(), json_num(c));
            let r = if i == j {
                1.0
            } else if n > 1.0 && var > 0.0 && variance(j) > 0.0 {
                c / (var * variance(j)).sqrt()
            } else {
                0.0
            };
            corr.insert(fields[j].clone(), json_num(r));
        }
        out_fields.push(json!({
            "name": fields[i],
            "count": n as i64,
            "mean": json_num(means[i]),
            "variance": json_num(var),
            "skewness": json_num(skew),
            "kurtosis": json_num(kurt),
            "covariance": Value::Object(cov),
            "correlation": Value::Object(corr),
        }));
    }
    Ok(json!({
        "doc_count": n as i64,
        "fields": out_fields,
    }))
}

fn hdr_digits(t: &Map<String, Value>) -> u32 {
    t.get("hdr")
        .and_then(|h| h.as_object())
        .and_then(|h| h.get("number_of_significant_value_digits"))
        .and_then(to_f64)
        .unwrap_or(3.0)
        .clamp(0.0, 5.0) as u32
}

fn percentiles_fail(cause: ApiError) -> ApiError {
    ApiError::new(
        "x_content_parse_exception",
        "Failed to build [percentiles] after last required field arrived",
        400,
    )
    .caused_by(cause)
}

fn percentiles_method_fail(agg: &str, method: &str, cause: ApiError) -> ApiError {
    let inner = ApiError::new(
        "x_content_parse_exception",
        &format!("Failed to build [{}] after last required field arrived", method),
        400,
    )
    .caused_by(cause);
    ApiError::new(
        "x_content_parse_exception",
        &format!("Failed to build [{}] after last required field arrived", agg),
        400,
    )
    .caused_by(inner)
}

fn percentiles_agg(
    t: &Map<String, Value>,
    name: &str,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    let allowed = [
        "field", "script", "percents", "keyed", "tdigest", "hdr", "format", "missing",
    ];
    for k in t.keys() {
        if k == "meta" {
            continue;
        }
        if !allowed.contains(&k.as_str()) {
            return Err(ApiError::new(
                "x_content_parse_exception",
                &format!("[percentiles] unknown field [{}]", k),
                400,
            ));
        }
    }
    if t.contains_key("hdr") && t.contains_key("tdigest") {
        return Err(percentiles_fail(ApiError::new(
            "illegal_argument_exception",
            "Only one percentiles method should be declared.",
            400,
        )));
    }
    if let Some(td) = t.get("tdigest").and_then(|v| v.as_object()) {
        if let Some(c) = td.get("compression").and_then(to_f64) {
            if c < 0.0 {
                let cause = ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "[compression] must be greater than or equal to 0. Found [{}]",
                        fmt_bound(c)
                    ),
                    400,
                );
                return Err(percentiles_method_fail("percentiles", "tdigest", cause));
            }
        }
    }
    if let Some(hd) = t.get("hdr").and_then(|v| v.as_object()) {
        if let Some(d) = hd
            .get("number_of_significant_value_digits")
            .and_then(to_f64)
        {
            if !(0.0..=5.0).contains(&d) {
                let cause = ApiError::new(
                    "illegal_argument_exception",
                    "[numberOfSignificantValueDigits] must be between 0 and 5",
                    400,
                );
                return Err(percentiles_method_fail("percentiles", "hdr", cause));
            }
        }
    }
    let field = map_str(t, "field");
    let script = agg_script_source(t);
    if field.is_empty() && script.is_none() {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified. ",
            400,
        ));
    }
    if !field.is_empty() {
        metric_num_type_check(idx, &field, prefix, "percentiles")?;
    }
    let mut percents: Vec<f64> = match t.get("percents") {
        Some(Value::Array(a)) => a.iter().filter_map(to_f64).collect(),
        _ => vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0],
    };
    if percents.is_empty() {
        return Err(percentiles_fail(ApiError::new(
            "illegal_argument_exception",
            &format!("[percents] must not be empty: [{}]", name),
            400,
        )));
    }
    for p in &percents {
        if !(0.0..=100.0).contains(p) {
            return Err(percentiles_fail(ApiError::new(
                "illegal_argument_exception",
                &format!("percent must be in [0,100], got [{}]: [{}]", fmt_bound(*p), name),
                400,
            )));
        }
    }
    percents.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    percents.dedup();
    let is_date = field_is_date(idx, &field, prefix);
    let script_params = agg_script_params(t);
    let mut vals = collect_metric_values(idx, docs, &field, prefix, &script, &script_params, is_date)?;
    let hdr = if t.contains_key("hdr") {
        let mut h = crate::hdr::HdrHistogram::new(hdr_digits(t));
        for v in &vals {
            h.record_value(*v);
        }
        Some(h)
    } else {
        None
    };
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(true);
    let fmt = map_str(t, "format");
    let mut out: Vec<(f64, Value, Option<String>)> = vec![];
    for p in &percents {
        if n == 0 {
            out.push((*p, Value::Null, None));
            continue;
        }
        let f = match &hdr {
            Some(h) => h.value_at_percentile(*p),
            None => {
                let mut i = ((p / 100.0) * n as f64).floor() as usize;
                if i >= n {
                    i = n - 1;
                }
                vals[i]
            }
        };
        out.push((*p, json_num(f), metric_as_string(f, is_date, &fmt)));
    }
    if keyed {
        let mut m = Map::new();
        for (p, v, s) in &out {
            let k = java_double_string(*p);
            m.insert(k.clone(), v.clone());
            if let Some(s) = s {
                m.insert(format!("{}_as_string", k), json!(s));
            }
        }
        Ok(json!({"values": m}))
    } else {
        let arr: Vec<Value> = out
            .iter()
            .map(|(p, v, _)| json!({"key": json_num(*p), "value": v.clone()}))
            .collect();
        Ok(json!({"values": arr}))
    }
}

fn percentile_ranks_agg(
    t: &Map<String, Value>,
    name: &str,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    let fail = |cause: ApiError| -> ApiError {
        ApiError::new(
            "x_content_parse_exception",
            "Failed to build [percentile_ranks] after last required field arrived",
            400,
        )
        .caused_by(cause)
    };
    let mut values: Vec<f64> = match t.get("values") {
        Some(Value::Array(a)) => a.iter().filter_map(to_f64).collect(),
        _ => {
            return Err(fail(ApiError::new(
                "illegal_argument_exception",
                &format!("[values] must not be null: [{}]", name),
                400,
            )))
        }
    };
    if values.is_empty() {
        return Err(fail(ApiError::new(
            "illegal_argument_exception",
            &format!("[values] must not be an empty array: [{}]", name),
            400,
        )));
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values.dedup();
    let field = map_str(t, "field");
    let script = agg_script_source(t);
    if field.is_empty() && script.is_none() {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified. ",
            400,
        ));
    }
    if !field.is_empty() {
        metric_num_type_check(idx, &field, prefix, "percentile_ranks")?;
    }
    let is_date = field_is_date(idx, &field, prefix);
    let script_params = agg_script_params(t);
    let mut vals = collect_metric_values(idx, docs, &field, prefix, &script, &script_params, is_date)?;
    let hdr = if t.contains_key("hdr") {
        let mut h = crate::hdr::HdrHistogram::new(hdr_digits(t));
        for v in &vals {
            h.record_value(*v);
        }
        Some(h)
    } else {
        None
    };
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut out: Vec<(f64, Value)> = vec![];
    for v in &values {
        if n == 0 {
            out.push((*v, Value::Null));
            continue;
        }
        let rank = match &hdr {
            Some(h) => h.percentile_at_or_below_value(*v),
            None => {
                let less = vals.iter().filter(|x| **x < *v).count() as f64;
                let eq = vals.iter().filter(|x| **x == *v).count() as f64;
                100.0 * (less + 0.5 * eq) / n as f64
            }
        };
        out.push((*v, json_num(rank)));
    }
    if keyed {
        let mut m = Map::new();
        for (v, r) in &out {
            m.insert(java_double_string(*v), r.clone());
        }
        Ok(json!({"values": m}))
    } else {
        let arr: Vec<Value> = out
            .iter()
            .map(|(v, r)| json!({"key": json_num(*v), "value": r.clone()}))
            .collect();
        Ok(json!({"values": arr}))
    }
}

fn extended_stats_agg(
    t: &Map<String, Value>,
    name: &str,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let sigma = match t.get("sigma") {
        Some(v) => {
            let sc = to_f64(v).unwrap_or(2.0);
            if sc < 0.0 {
                let cause = ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "[sigma] must be greater than or equal to 0. Found [{}] in [{}]",
                        fmt_bound(sc),
                        name
                    ),
                    400,
                );
                return Err(ApiError::new(
                    "x_content_parse_exception",
                    "Failed to build [extended_stats] after last required field arrived",
                    400,
                )
                .caused_by(cause));
            }
            sc
        }
        None => 2.0,
    };
    metric_num_type_check(idx, &field, prefix, "extended_stats")?;
    let vals = numeric_values(idx, docs, &field, prefix);
    if vals.is_empty() {
        return Ok(json!({
            "count": 0,
            "min": Value::Null,
            "max": Value::Null,
            "avg": Value::Null,
            "sum": 0.0,
            "sum_of_squares": Value::Null,
            "variance": Value::Null,
            "variance_population": Value::Null,
            "variance_sampling": Value::Null,
            "std_deviation": Value::Null,
            "std_deviation_population": Value::Null,
            "std_deviation_sampling": Value::Null,
            "std_deviation_bounds": {
                "upper": Value::Null,
                "lower": Value::Null,
                "upper_population": Value::Null,
                "lower_population": Value::Null,
                "upper_sampling": Value::Null,
                "lower_sampling": Value::Null,
            },
        }));
    }
    let n = vals.len() as f64;
    let sum = sum_of(&vals);
    let sos: f64 = vals.iter().map(|x| x * x).sum();
    let avg = sum / n;
    let var_pop = sos / n - avg * avg;
    let var_samp = if n > 1.0 {
        (sos - sum * sum / n) / (n - 1.0)
    } else {
        0.0
    };
    let sd_pop = var_pop.max(0.0).sqrt();
    let sd_samp = var_samp.max(0.0).sqrt();
    Ok(json!({
        "count": vals.len(),
        "min": json_num(min_of(&vals)),
        "max": json_num(max_of(&vals)),
        "avg": json_num(avg),
        "sum": json_num(sum),
        "sum_of_squares": json_num(sos),
        "variance": json_num(var_pop),
        "variance_population": json_num(var_pop),
        "variance_sampling": json_num(var_samp),
        "std_deviation": json_num(sd_pop),
        "std_deviation_population": json_num(sd_pop),
        "std_deviation_sampling": json_num(sd_samp),
        "std_deviation_bounds": {
            "upper": json_num(avg + sigma * sd_pop),
            "lower": json_num(avg - sigma * sd_pop),
            "upper_population": json_num(avg + sigma * sd_pop),
            "lower_population": json_num(avg - sigma * sd_pop),
            "upper_sampling": json_num(avg + sigma * sd_samp),
            "lower_sampling": json_num(avg - sigma * sd_samp),
        },
    }))
}


const PIPELINE_TYPES: &[&str] = &[
    "derivative",
    "cumulative_sum",
    "serial_diff",
    "moving_fn",
    "moving_avg",
    "bucket_script",
    "bucket_selector",
    "bucket_sort",
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
];

fn pipeline_type_of(def: &Map<String, Value>) -> Option<&'static str> {
    for k in PIPELINE_TYPES {
        if def.contains_key(*k) {
            return Some(k);
        }
    }
    None
}

fn is_parent_scoped_pipeline(t: &str) -> bool {
    matches!(
        t,
        "derivative"
            | "cumulative_sum"
            | "serial_diff"
            | "moving_fn"
            | "moving_avg"
            | "bucket_script"
            | "bucket_selector"
            | "bucket_sort"
    )
}

fn validation_failed(reason: &str) -> ApiError {
    ApiError::new(
        "action_request_validation_exception",
        &format!("Validation Failed: 1: {}", reason),
        400,
    )
}

#[derive(Clone, Copy, PartialEq)]
enum PipeGap {
    InsertZeros,
    Skip,
}

fn parse_gap_policy(t: &Map<String, Value>) -> Result<PipeGap, ApiError> {
    match t.get("gap_policy").and_then(|v| v.as_str()) {
        Some("skip") | None => Ok(PipeGap::Skip),
        Some("insert_zeros") => Ok(PipeGap::InsertZeros),
        Some(other) => Err(ApiError::new(
            "parsing_exception",
            &format!(
                "Invalid gap policy: [{}], accepted values: [insert_zeros, skip]",
                other
            ),
            400,
        )),
    }
}

fn apply_gap_opt(v: Option<f64>, gap: PipeGap) -> Option<f64> {
    match (v, gap) {
        (Some(x), PipeGap::InsertZeros) if !x.is_finite() => Some(0.0),
        (v, _) => v,
    }
}

fn required_buckets_path<'a>(t: &'a Map<String, Value>, name: &str) -> Result<&'a str, ApiError> {
    t.get("buckets_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ApiError::new(
                "parsing_exception",
                &format!(
                    "Missing required field [buckets_path] for aggregation [{}]",
                    name
                ),
                400,
            )
        })
}

fn parse_path_tokens(path: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for ch in path.chars() {
        match ch {
            '[' => {
                depth += 1;
                cur.push(ch);
            }
            ']' => {
                depth -= 1;
                cur.push(ch);
            }
            '>' | '.' if depth == 0 => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn split_token(tok: &str) -> (String, Option<String>) {
    if let Some(i) = tok.find('[') {
        let name = tok[..i].to_string();
        let sel = tok[i + 1..].trim_end_matches(']').to_string();
        return (name, Some(sel));
    }
    (tok.to_string(), None)
}

fn agg_path_error(reason: String) -> ApiError {
    ApiError::new("search_phase_execution_exception", "all shards failed", 500)
        .caused_by(ApiError::new("aggregation_execution_exception", &reason, 500))
}

fn path_illegal_error(msg: &str) -> ApiError {
    ApiError::new("search_phase_execution_exception", "all shards failed", 400)
        .caused_by(ApiError::new("illegal_argument_exception", msg, 400))
}

fn script_cast_error(from: &str, to: &str) -> ApiError {
    ApiError::new("search_phase_execution_exception", "all shards failed", 400).caused_by(
        ApiError::new("script_exception", "runtime error", 400).caused_by(ApiError::new(
            "illegal_argument_exception",
            &format!("Cannot cast from [{}] to [{}].", from, to),
            400,
        )),
    )
}

fn multi_value_error(name: &str) -> ApiError {
    agg_path_error(format!(
        "buckets_path must reference either a number value or a single value numeric metric aggregation, but [{}] contains multiple values. Please specify which to use.",
        name
    ))
}

fn bucket_value_error(name: &str) -> ApiError {
    agg_path_error(format!(
        "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [Bucket] at aggregation [{}]",
        name
    ))
}

fn stats_key_error(key: &str) -> ApiError {
    path_illegal_error(&format!(
        "No enum constant org.opensearch.search.aggregations.metrics.InternalStats.Metrics.{}",
        key
    ))
}

fn path_f64(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn final_metric_value(node: &Value, name: &str, dc: f64) -> Result<f64, ApiError> {
    if let Some(v) = node.get("value") {
        if dc == 0.0 && name != "_count" {
            return Ok(f64::NAN);
        }
        return Ok(path_f64(v));
    }
    if node.get("values").is_some() || node.get("count").is_some() {
        return Err(multi_value_error(name));
    }
    Err(bucket_value_error(name))
}

fn leaf_metric_value(
    node: &Value,
    name: &str,
    sel: Option<&str>,
    rest: &[String],
) -> Result<f64, ApiError> {
    let key = match (sel, rest.len()) {
        (Some(s), 0) => Some(s.to_string()),
        (None, 1) => Some(rest[0].clone()),
        _ => None,
    };
    if let Some(k) = key {
        if let Some(values) = node.get("values").and_then(|v| v.as_object()) {
            let kk = match k.parse::<f64>() {
                Ok(f) => java_double_string(f),
                Err(_) => k.clone(),
            };
            return match values.get(&kk) {
                Some(v) => Ok(path_f64(v)),
                None => Ok(f64::NAN),
            };
        }
        if node.get("count").is_some() {
            return match k.as_str() {
                "count" | "min" | "max" | "avg" | "sum" | "sum_of_squares" | "variance"
                | "variance_population" | "variance_sampling" | "std_deviation"
                | "std_deviation_population" | "std_deviation_sampling" => {
                    Ok(node.get(k.as_str()).map(path_f64).unwrap_or(f64::NAN))
                }
                other => Err(stats_key_error(other)),
            };
        }
    }
    Err(bucket_value_error(name))
}

fn resolve_parent_path(
    bucket: &Map<String, Value>,
    tokens: &[String],
    full: &str,
) -> Result<Option<f64>, ApiError> {
    if tokens.is_empty() {
        return Err(bucket_value_error(full));
    }
    let (head, sel) = split_token(&tokens[0]);
    let rest = &tokens[1..];
    let dc = bucket
        .get("doc_count")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if head == "_count" && rest.is_empty() && sel.is_none() {
        return Ok(Some(dc));
    }
    if head == "_bucket_count" && rest.is_empty() {
        return Ok(Some(
            bucket
                .get("buckets")
                .and_then(|v| v.as_array())
                .map(|b| b.len())
                .unwrap_or(0) as f64,
        ));
    }
    let node = match bucket.get(&head) {
        Some(n) => n.clone(),
        None => {
            if dc == 0.0 {
                return Ok(Some(f64::NAN));
            }
            return Ok(None);
        }
    };
    if rest.is_empty() && sel.is_none() {
        return final_metric_value(&node, &head, dc).map(Some);
    }
    if let Some(bs) = node.get("buckets").and_then(|v| v.as_array()) {
        if rest.len() == 1 && rest[0] == "_bucket_count" {
            return Ok(Some(bs.len() as f64));
        }
        return Err(bucket_value_error(&head));
    }
    if let Some(nm) = node.as_object() {
        if nm.get("doc_count").is_some() {
            return resolve_parent_path(nm, rest, full);
        }
    }
    leaf_metric_value(&node, &head, sel.as_deref(), rest).map(Some)
}

fn resolve_bucket_value_in(
    bucket: &Map<String, Value>,
    tokens: &[String],
    full: &str,
) -> Result<f64, ApiError> {
    resolve_parent_path(bucket, tokens, full).map(|v| v.unwrap_or(f64::NAN))
}

fn bucket_display_key(b: &Value) -> String {
    if let Some(s) = b.get("key_as_string").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    match b.get("key") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => java_double_string(n.as_f64().unwrap_or(0.0)),
        _ => String::new(),
    }
}

fn scope_walk(
    scope: &Map<String, Value>,
    tokens: &[String],
    full: &str,
) -> Result<Vec<(Value, f64, String)>, ApiError> {
    if tokens.is_empty() {
        return Err(bucket_value_error(full));
    }
    let (head, sel) = split_token(&tokens[0]);
    let rest = &tokens[1..];
    let node = match scope.get(&head) {
        Some(n) => n.clone(),
        None => {
            return Err(agg_path_error(format!(
                "Invalid aggregation path [{}]",
                full
            )))
        }
    };
    if let Some(bs) = node.get("buckets").and_then(|v| v.as_array()) {
        if rest.is_empty() && sel.is_none() {
            return Err(bucket_value_error(&head));
        }
        let mut out = Vec::new();
        for b in bs {
            let bm = b.as_object().cloned().unwrap_or_default();
            let mut toks: Vec<String> = Vec::new();
            if let Some(s) = &sel {
                toks.push(format!("{}[{}]", head, s));
            }
            toks.extend(rest.iter().cloned());
            let v = resolve_bucket_value_in(&bm, &toks, full)?;
            out.push((b.clone(), v, head.clone()));
        }
        return Ok(out);
    }
    if node.get("doc_count").is_some() {
        let bm = node.as_object().cloned().unwrap_or_default();
        let mut toks: Vec<String> = Vec::new();
        if let Some(s) = &sel {
            toks.push(format!("{}[{}]", head, s));
        }
        toks.extend(rest.iter().cloned());
        let v = resolve_bucket_value_in(&bm, &toks, full)?;
        return Ok(vec![(node, v, head)]);
    }
    let v = leaf_metric_value(&node, &head, sel.as_deref(), rest)?;
    Ok(vec![(node, v, head)])
}

fn def_is_metric_leaf(d: &Map<String, Value>) -> bool {
    for k in [
        "stats",
        "extended_stats",
        "percentiles",
        "percentile_ranks",
        "avg",
        "min",
        "max",
        "sum",
        "value_count",
        "cardinality",
        "median_absolute_deviation",
    ] {
        if d.contains_key(k) {
            return true;
        }
    }
    false
}

fn validate_sibling_path(defs: &Map<String, Value>, path: &str) -> Result<(), ApiError> {
    let tokens = parse_path_tokens(path);
    if tokens.is_empty() {
        return Err(validation_failed(&format!(
            "No aggregation found for path [{}];",
            path
        )));
    }
    let (first, _) = split_token(&tokens[0]);
    let mut cur: Option<Map<String, Value>> = defs
        .get(&first)
        .and_then(|v| v.as_object())
        .cloned();
    if cur.is_none() {
        return Err(validation_failed(&format!(
            "No aggregation found for path [{}];",
            path
        )));
    }
    let mut i = 1;
    while i < tokens.len() {
        let (tok, _) = split_token(&tokens[i]);
        if tok == "_count" || tok == "_bucket_count" {
            return Ok(());
        }
        let cur_map = cur.clone().unwrap_or_default();
        match sub_aggs(&cur_map) {
            Some(s) => match s.get(&tok).and_then(|v| v.as_object()).cloned() {
                Some(d) => {
                    cur = Some(d);
                }
                None => {
                    if def_is_metric_leaf(&cur_map) {
                        return Ok(());
                    }
                    return Err(validation_failed(&format!(
                        "No aggregation [{}] found for path [{}];",
                        tok, path
                    )));
                }
            },
            None => {
                if def_is_metric_leaf(&cur_map) {
                    return Ok(());
                }
                return Err(validation_failed(&format!(
                    "No aggregation [{}] found for path [{}];",
                    tok, path
                )));
            }
        }
        i += 1;
    }
    Ok(())
}

fn validate_bucket_path(
    sub: &Map<String, Value>,
    path: &str,
    pipeline_names: &[String],
) -> Result<(), ApiError> {
    let tokens = parse_path_tokens(path);
    if tokens.is_empty() {
        return Err(validation_failed(&format!(
            "No aggregation found for path [{}];",
            path
        )));
    }
    let (head, _) = split_token(&tokens[0]);
    if head == "_count" || head == "_bucket_count" {
        return Ok(());
    }
    if !sub.contains_key(&head) && !pipeline_names.iter().any(|n| *n == head) {
        return Err(validation_failed(&format!(
            "No aggregation found for path [{}];",
            path
        )));
    }
    Ok(())
}

fn pipeline_value_obj(v: f64, fmt: &str) -> Value {
    let mut m = Map::new();
    m.insert("value".to_string(), json_num(v));
    if !fmt.is_empty() && v.is_finite() {
        m.insert(
            "value_as_string".to_string(),
            json!(java_numeric_format(v, fmt)),
        );
    }
    Value::Object(m)
}

fn extended_stats_json(vals: &[f64], sigma: f64) -> Value {
    if vals.is_empty() {
        return json!({
            "count": 0,
            "min": Value::Null,
            "max": Value::Null,
            "avg": Value::Null,
            "sum": 0.0,
            "sum_of_squares": Value::Null,
            "variance": Value::Null,
            "variance_population": Value::Null,
            "variance_sampling": Value::Null,
            "std_deviation": Value::Null,
            "std_deviation_population": Value::Null,
            "std_deviation_sampling": Value::Null,
            "std_deviation_bounds": {
                "upper": Value::Null,
                "lower": Value::Null,
                "upper_population": Value::Null,
                "lower_population": Value::Null,
                "upper_sampling": Value::Null,
                "lower_sampling": Value::Null,
            },
        });
    }
    let n = vals.len() as f64;
    let sum = sum_of(vals);
    let sos: f64 = vals.iter().map(|x| x * x).sum();
    let avg = sum / n;
    let var_pop = sos / n - avg * avg;
    let var_samp = if n > 1.0 {
        (sos - sum * sum / n) / (n - 1.0)
    } else {
        0.0
    };
    let sd_pop = var_pop.max(0.0).sqrt();
    let sd_samp = var_samp.max(0.0).sqrt();
    json!({
        "count": vals.len(),
        "min": json_num(min_of(vals)),
        "max": json_num(max_of(vals)),
        "avg": json_num(avg),
        "sum": json_num(sum),
        "sum_of_squares": json_num(sos),
        "variance": json_num(var_pop),
        "variance_population": json_num(var_pop),
        "variance_sampling": json_num(var_samp),
        "std_deviation": json_num(sd_pop),
        "std_deviation_population": json_num(sd_pop),
        "std_deviation_sampling": json_num(sd_samp),
        "std_deviation_bounds": {
            "upper": json_num(avg + sigma * sd_pop),
            "lower": json_num(avg - sigma * sd_pop),
            "upper_population": json_num(avg + sigma * sd_pop),
            "lower_population": json_num(avg - sigma * sd_pop),
            "upper_sampling": json_num(avg + sigma * sd_samp),
            "lower_sampling": json_num(avg - sigma * sd_samp),
        },
    })
}

fn eval_metric_pipeline(
    name: &str,
    pt: &str,
    t: &Map<String, Value>,
    scope: &Map<String, Value>,
    defs: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let _ = (idx, prefix, typed);
    let path = required_buckets_path(t, name)?.to_string();
    let gap = parse_gap_policy(t)?;
    validate_sibling_path(defs, &path)?;
    let tokens = parse_path_tokens(&path);
    let resolved = scope_walk(scope, &tokens, &path)?;
    let fmt = map_str(t, "format");
    let mut kept: Vec<(Value, f64)> = Vec::new();
    for (b, v, _n) in resolved {
        if v.is_finite() {
            kept.push((b, v));
        } else if gap == PipeGap::InsertZeros {
            kept.push((b, 0.0));
        }
    }
    match pt {
        "avg_bucket" => {
            if kept.is_empty() {
                Ok(pipeline_value_obj(f64::NAN, &fmt))
            } else {
                let s: f64 = kept.iter().map(|(_, v)| *v).sum();
                Ok(pipeline_value_obj(s / kept.len() as f64, &fmt))
            }
        }
        "sum_bucket" => {
            let s: f64 = kept.iter().map(|(_, v)| *v).sum();
            Ok(pipeline_value_obj(s, &fmt))
        }
        "min_bucket" | "max_bucket" => {
            let mut best: Option<f64> = None;
            for (_, v) in &kept {
                best = Some(match best {
                    None => *v,
                    Some(cur) => {
                        if pt == "min_bucket" {
                            cur.min(*v)
                        } else {
                            cur.max(*v)
                        }
                    }
                });
            }
            let mut keys = Vec::new();
            if let Some(bv) = best {
                for (b, v) in &kept {
                    if *v == bv {
                        keys.push(Value::String(bucket_display_key(b)));
                    }
                }
            }
            let mut m = Map::new();
            match best {
                Some(bv) => {
                    m.insert("value".to_string(), json_num(bv));
                    if !fmt.is_empty() && bv.is_finite() {
                        m.insert(
                            "value_as_string".to_string(),
                            json!(java_numeric_format(bv, &fmt)),
                        );
                    }
                }
                None => {
                    m.insert("value".to_string(), Value::Null);
                }
            }
            m.insert("keys".to_string(), Value::Array(keys));
            Ok(Value::Object(m))
        }
        "stats_bucket" => {
            if kept.is_empty() {
                Ok(json!({
                    "count": 0,
                    "min": Value::Null,
                    "max": Value::Null,
                    "avg": Value::Null,
                    "sum": 0.0,
                }))
            } else {
                let vals: Vec<f64> = kept.iter().map(|(_, v)| *v).collect();
                Ok(json!({
                    "count": vals.len(),
                    "min": json_num(min_of(&vals)),
                    "max": json_num(max_of(&vals)),
                    "avg": json_num(mean(&vals)),
                    "sum": json_num(sum_of(&vals)),
                }))
            }
        }
        "extended_stats_bucket" => {
            let vals: Vec<f64> = kept.iter().map(|(_, v)| *v).collect();
            let sigma = t.get("sigma").and_then(|v| to_f64(v)).unwrap_or(2.0);
            Ok(extended_stats_json(&vals, sigma))
        }
        "percentiles_bucket" => {
            let percents: Vec<f64> = match t.get("percents") {
                Some(Value::Array(a)) => a.iter().filter_map(to_f64).collect(),
                _ => vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0],
            };
            let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(true);
            let mut sorted: Vec<f64> = kept.iter().map(|(_, v)| *v).collect();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
            let n = sorted.len();
            let pick = |p: f64| -> Value {
                if n == 0 {
                    return Value::Null;
                }
                let mut i = ((p / 100.0) * n as f64).floor() as usize;
                if i >= n {
                    i = n - 1;
                }
                json_num(sorted[i])
            };
            if keyed {
                let mut m = Map::new();
                for p in &percents {
                    m.insert(java_double_string(*p), pick(*p));
                }
                Ok(json!({"values": m}))
            } else {
                let arr: Vec<Value> = percents
                    .iter()
                    .map(|p| json!({"key": json_num(*p), "value": pick(*p)}))
                    .collect();
                Ok(json!({"values": arr}))
            }
        }
        _ => Ok(Value::Object(Map::new())),
    }
}

fn compute_derivative(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
    kind: &str,
) -> Result<(), ApiError> {
    let _ = (idx, prefix, kind);
    let path = required_buckets_path(t, name)?.to_string();
    let tokens = parse_path_tokens(&path);
    let gap = parse_gap_policy(t)?;
    let fmt = map_str(t, "format");
    let unit = match t.get("unit") {
        Some(Value::String(u)) => Some(parse_unit_ms(u)?),
        _ => None,
    };
    let mut last: Option<f64> = None;
    let mut last_key: Option<f64> = None;
    for b in buckets.iter_mut() {
        let (cur, this_key) = {
            let bm = match b.as_object_mut() {
                Some(m) => m,
                None => continue,
            };
            let this_key = bm.get("key").and_then(|v| v.as_f64());
            let cur = apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, gap);
            (cur, this_key)
        };
        if let (Some(lv), Some(cv)) = (last, cur) {
            let gradient = cv - lv;
            let mut m = Map::new();
            m.insert("value".to_string(), json_num(gradient));
            if !fmt.is_empty() && gradient.is_finite() {
                m.insert(
                    "value_as_string".to_string(),
                    json!(java_numeric_format(gradient, &fmt)),
                );
            }
            if let (Some(u), Some(k0), Some(k1)) = (unit, last_key, this_key) {
                let xdiff = (k1 - k0) / u;
                if xdiff != 0.0 {
                    m.insert("normalized_value".to_string(), json_num(gradient / xdiff));
                } else {
                    m.insert("normalized_value".to_string(), Value::Null);
                }
            }
            if let Some(bm) = b.as_object_mut() {
                bm.insert(name.to_string(), Value::Object(m));
            }
        }
        last = cur;
        if this_key.is_some() {
            last_key = this_key;
        }
    }
    Ok(())
}

fn compute_cumulative_sum(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
) -> Result<(), ApiError> {
    let path = required_buckets_path(t, name)?.to_string();
    let tokens = parse_path_tokens(&path);
    let fmt = map_str(t, "format");
    let mut sum = 0.0f64;
    for b in buckets.iter_mut() {
        let cur = {
            let bm = match b.as_object() {
                Some(m) => m,
                None => continue,
            };
            apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, PipeGap::InsertZeros)
        };
        if let Some(v) = cur {
            if v.is_finite() {
                sum += v;
            }
        }
        let mut m = Map::new();
        m.insert("value".to_string(), json_num(sum));
        if !fmt.is_empty() {
            m.insert(
                "value_as_string".to_string(),
                json!(java_numeric_format(sum, &fmt)),
            );
        }
        if let Some(bm) = b.as_object_mut() {
            bm.insert(name.to_string(), Value::Object(m));
        }
    }
    Ok(())
}

fn compute_serial_diff(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
) -> Result<(), ApiError> {
    let path = required_buckets_path(t, name)?.to_string();
    let tokens = parse_path_tokens(&path);
    let gap = parse_gap_policy(t)?;
    let fmt = map_str(t, "format");
    let lag = t.get("lag").and_then(|v| v.as_i64()).unwrap_or(1);
    if lag <= 0 {
        return Err(ApiError::new(
            "parsing_exception",
            &format!(
                "Lag must be a positive, non-zero integer.  Value supplied was{} in [{}]: [lag].",
                lag, name
            ),
            400,
        ));
    }
    let lag = lag as usize;
    let mut window: Vec<f64> = Vec::new();
    let mut counter = 0usize;
    for b in buckets.iter_mut() {
        let this = {
            let bm = match b.as_object() {
                Some(m) => m,
                None => continue,
            };
            let v = apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, gap);
            v.map(|x| if x.is_nan() { f64::NAN } else { x })
                .unwrap_or(f64::NAN)
        };
        counter += 1;
        let lag_val = if counter <= lag {
            f64::NAN
        } else {
            window.first().copied().unwrap_or(f64::NAN)
        };
        if !this.is_nan() && !lag_val.is_nan() {
            let mut m = Map::new();
            let diff = this - lag_val;
            m.insert("value".to_string(), json_num(diff));
            if !fmt.is_empty() && diff.is_finite() {
                m.insert(
                    "value_as_string".to_string(),
                    json!(java_numeric_format(diff, &fmt)),
                );
            }
            if let Some(bm) = b.as_object_mut() {
                bm.insert(name.to_string(), Value::Object(m));
            }
        }
        window.push(this);
        if window.len() > lag {
            window.remove(0);
        }
    }
    Ok(())
}

fn ma_sum(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum()
    }
}

fn ma_unweighted_avg(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn ma_min(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().copied().fold(f64::INFINITY, f64::min)
    }
}

fn ma_max(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    }
}

fn ma_linear_weighted(v: &[f64]) -> f64 {
    let mut avg = 0.0;
    let mut total = 1.0;
    let mut cur = 1.0;
    for x in v {
        avg += x * cur;
        total += cur;
        cur += 1.0;
    }
    if total == 1.0 {
        f64::NAN
    } else {
        avg / total
    }
}

fn ma_ewma(v: &[f64], alpha: f64) -> f64 {
    let mut avg = f64::NAN;
    let mut first = true;
    for x in v {
        if first {
            avg = *x;
            first = false;
        } else {
            avg = x * alpha + avg * (1.0 - alpha);
        }
    }
    avg
}

fn ma_stddev(v: &[f64], avg: f64) -> f64 {
    if avg.is_nan() || v.is_empty() {
        return f64::NAN;
    }
    let ss: f64 = v.iter().map(|x| (x - avg).powi(2)).sum();
    (ss / v.len() as f64).sqrt()
}

fn moving_binop(op: char, a: f64, b: f64) -> f64 {
    match op {
        '*' => a * b,
        '+' => a + b,
        '-' => a - b,
        '/' => a / b,
        _ => a,
    }
}

fn moving_fn_eval(src: &str, params: &Map<String, Value>, vals: &[f64]) -> f64 {
    let mut expr = src.trim();
    let mut modifier: Option<(char, f64)> = None;
    if let Some(idx) = expr.rfind("params.") {
        let before = expr[..idx].trim_end();
        if let Some(op) = before.chars().last() {
            if matches!(op, '*' | '+' | '-' | '/') {
                let pname = expr[idx + 7..].trim();
                if let Some(v) = params.get(pname).and_then(|v| v.as_f64()) {
                    let base = before[..before.len() - op.len_utf8()].trim_end();
                    modifier = Some((op, v));
                    expr = base;
                }
            }
        }
    }
    let base = if expr.ends_with(".length") {
        vals.len() as f64
    } else if let Some(i) = expr.find("MovingFunctions.") {
        let after = &expr[i + "MovingFunctions.".len()..];
        let fname: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let inner = match after.find('(') {
            Some(j) => {
                let end = after.rfind(')').unwrap_or(after.len());
                &after[j + 1..end.max(j + 1)]
            }
            None => "",
        };
        let args: Vec<&str> = if inner.is_empty() {
            Vec::new()
        } else {
            inner.split(',').collect()
        };
        match fname.as_str() {
            "sum" => ma_sum(vals),
            "unweightedAvg" => ma_unweighted_avg(vals),
            "min" => ma_min(vals),
            "max" => ma_max(vals),
            "linearWeightedAvg" => ma_linear_weighted(vals),
            "ewma" => {
                let alpha = args
                    .get(1)
                    .and_then(|a| a.trim().parse::<f64>().ok())
                    .unwrap_or(0.3);
                ma_ewma(vals, alpha)
            }
            "stdDev" => {
                let avg = match args.get(1) {
                    Some(a) => moving_fn_eval(a.trim(), params, vals),
                    None => f64::NAN,
                };
                ma_stddev(vals, avg)
            }
            _ => f64::NAN,
        }
    } else {
        expr.parse::<f64>().unwrap_or(f64::NAN)
    };
    match modifier {
        Some((op, m)) => moving_binop(op, base, m),
        None => base,
    }
}

fn compute_moving_fn(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
) -> Result<(), ApiError> {
    let _ = (idx, prefix);
    let path = required_buckets_path(t, name)?.to_string();
    let tokens = parse_path_tokens(&path);
    let gap = parse_gap_policy(t)?;
    let fmt = map_str(t, "format");
    let src = agg_script_source(t).unwrap_or_default();
    let params = agg_script_params(t);
    let window = t.get("window").and_then(|v| v.as_i64()).unwrap_or(0);
    let shift = t.get("shift").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut values: Vec<f64> = Vec::new();
    for b in buckets.iter() {
        if let Some(bm) = b.as_object() {
            if let Some(v) = apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, gap) {
                if v.is_finite() {
                    values.push(v);
                }
            }
        }
    }
    let mut index = 0i64;
    let n = values.len() as i64;
    for b in buckets.iter_mut() {
        let cur = {
            let bm = match b.as_object() {
                Some(m) => m,
                None => continue,
            };
            apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, gap)
        };
        if let Some(v) = cur {
            if v.is_finite() {
                let from = (index - window + shift).clamp(0, n) as usize;
                let to = (index + shift).clamp(0, n) as usize;
                let slice: &[f64] = if from <= to { &values[from..to] } else { &[] };
                let res = moving_fn_eval(&src, &params, slice);
                if let Some(bm) = b.as_object_mut() {
                    let mut m = Map::new();
                    m.insert("value".to_string(), json_num(res));
                    if !fmt.is_empty() && res.is_finite() {
                        m.insert(
                            "value_as_string".to_string(),
                            json!(java_numeric_format(res, &fmt)),
                        );
                    }
                    bm.insert(name.to_string(), Value::Object(m));
                }
                index += 1;
            }
        }
    }
    Ok(())
}

fn compute_moving_avg(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
) -> Result<(), ApiError> {
    let path = required_buckets_path(t, name)?.to_string();
    let tokens = parse_path_tokens(&path);
    let gap = parse_gap_policy(t)?;
    let fmt = map_str(t, "format");
    let window = t
        .get("window")
        .and_then(|v| v.as_i64())
        .map(|w| w.max(1) as usize)
        .unwrap_or(5);
    let model = t
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("simple");
    let alpha = t
        .get("settings")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("alpha"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.3);
    let mut queue: Vec<f64> = Vec::new();
    for b in buckets.iter_mut() {
        let this = {
            let bm = match b.as_object() {
                Some(m) => m,
                None => continue,
            };
            match apply_gap_opt(resolve_parent_path(bm, &tokens, &path)?, gap) {
                Some(v) if v.is_finite() => v,
                _ => continue,
            }
        };
        if !queue.is_empty() {
            let res = match model {
                "linear" => ma_linear_weighted(&queue),
                "ewma" => ma_ewma(&queue, alpha),
                _ => ma_unweighted_avg(&queue),
            };
            if let Some(bm) = b.as_object_mut() {
                let mut m = Map::new();
                m.insert("value".to_string(), json_num(res));
                if !fmt.is_empty() && res.is_finite() {
                    m.insert(
                        "value_as_string".to_string(),
                        json!(java_numeric_format(res, &fmt)),
                    );
                }
                bm.insert(name.to_string(), Value::Object(m));
            }
        }
        queue.push(this);
        if queue.len() > window {
            queue.remove(0);
        }
    }
    Ok(())
}

fn parse_unit_ms(u: &str) -> Result<f64, ApiError> {
    let digits: String = u.chars().take_while(|c| c.is_ascii_digit()).collect();
    let rest = &u[digits.len()..];
    let n: f64 = if digits.is_empty() {
        1.0
    } else {
        digits.parse().unwrap_or(1.0)
    };
    let ms = match rest {
        "ms" | "millis" | "milliseconds" => 1.0,
        "s" | "second" | "seconds" => 1000.0,
        "m" | "minute" | "minutes" => 60000.0,
        "h" | "hour" | "hours" => 3600000.0,
        "d" | "day" | "days" => 86400000.0,
        "w" | "week" | "weeks" => 604800000.0,
        "M" | "month" | "months" => 2592000000.0,
        "y" | "year" | "years" => 31536000000.0,
        _ => {
            let inner = ApiError::new(
                "illegal_argument_exception",
                &format!("failed to parse [{}]", u),
                400,
            );
            return Err(ApiError::new(
                "parsing_exception",
                &format!("failed to parse [{}]", u),
                400,
            )
            .caused_by(inner));
        }
    };
    Ok(n * ms)
}

fn bucket_script_vars(
    t: &Map<String, Value>,
    name: &str,
) -> Result<Vec<(String, String)>, ApiError> {
    match t.get("buckets_path") {
        Some(Value::Object(m)) => Ok(m
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()),
        _ => Err(ApiError::new(
            "parsing_exception",
            &format!(
                "Missing required field [buckets_path] for aggregation [{}]",
                name
            ),
            400,
        )),
    }
}

fn resolve_vars_for_bucket(
    bm: &Map<String, Value>,
    vars_map: &[(String, String)],
    gap: PipeGap,
    script_params: &Map<String, Value>,
) -> Result<Option<Map<String, Value>>, ApiError> {
    let mut vars = script_params.clone();
    for (var, p) in vars_map {
        let toks = parse_path_tokens(p);
        let v = apply_gap_opt(resolve_parent_path(bm, &toks, p)?, gap);
        match v {
            Some(x) if x.is_finite() => {
                vars.insert(var.clone(), json_num(x));
            }
            Some(_) => {
                if gap == PipeGap::Skip {
                    return Ok(None);
                }
                vars.insert(var.clone(), json_num(0.0));
            }
            None => {
                if gap == PipeGap::Skip {
                    return Ok(None);
                }
                vars.insert(var.clone(), Value::Null);
            }
        }
    }
    Ok(Some(vars))
}

fn compute_bucket_script(
    buckets: &mut [Value],
    name: &str,
    t: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
) -> Result<(), ApiError> {
    let _ = prefix;
    let gap = parse_gap_policy(t)?;
    let fmt = map_str(t, "format");
    let src = agg_script_source(t).unwrap_or_default();
    let script_params = agg_script_params(t);
    let vars_map = bucket_script_vars(t, name)?;
    for b in buckets.iter_mut() {
        let bm = match b.as_object() {
            Some(m) => m.clone(),
            None => continue,
        };
        let vars = match resolve_vars_for_bucket(&bm, &vars_map, gap, &script_params)? {
            Some(v) => v,
            None => continue,
        };
        let res = run_agg_script(idx, &Value::Null, &src, &vars, None)?;
        let f = match res {
            Value::Null => continue,
            Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
            Value::Bool(_) => return Err(script_cast_error("boolean", "java.lang.Number")),
            Value::String(_) => {
                return Err(script_cast_error("java.lang.String", "java.lang.Number"))
            }
            _ => return Err(script_cast_error("java.lang.Object", "java.lang.Number")),
        };
        let mut m = Map::new();
        m.insert("value".to_string(), json_num(f));
        if !fmt.is_empty() && f.is_finite() {
            m.insert(
                "value_as_string".to_string(),
                json!(java_numeric_format(f, &fmt)),
            );
        }
        if let Some(bm) = b.as_object_mut() {
            bm.insert(name.to_string(), Value::Object(m));
        }
    }
    Ok(())
}

fn apply_bucket_selector(
    buckets: &mut Vec<Value>,
    name: &str,
    t: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
) -> Result<(), ApiError> {
    let _ = prefix;
    let gap = parse_gap_policy(t)?;
    let src = agg_script_source(t).unwrap_or_default();
    let script_params = agg_script_params(t);
    let vars_map = bucket_script_vars(t, name)?;
    let mut kept: Vec<Value> = Vec::new();
    for b in buckets.drain(..) {
        let bm = match b.as_object() {
            Some(m) => m.clone(),
            None => {
                kept.push(b);
                continue;
            }
        };
        let vars = match resolve_vars_for_bucket(&bm, &vars_map, gap, &script_params)? {
            Some(v) => v,
            None => continue,
        };
        let res = run_agg_script(idx, &Value::Null, &src, &vars, None)?;
        match res {
            Value::Bool(true) => kept.push(b),
            Value::Bool(false) => {}
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                let tname = if f.fract() == 0.0 { "int" } else { "double" };
                return Err(script_cast_error(tname, "boolean"));
            }
            Value::String(_) => return Err(script_cast_error("java.lang.String", "boolean")),
            _ => return Err(script_cast_error("java.lang.Object", "boolean")),
        }
    }
    *buckets = kept;
    Ok(())
}

fn bucket_sort_parse_fail(reason: &str) -> ApiError {
    ApiError::new(
        "x_content_parse_exception",
        "Failed to build [bucket_sort] after last required field arrived",
        400,
    )
    .caused_by(
        ApiError::new(
            "x_content_parse_exception",
            "Failed to build [bucket_sort] after last required field arrived",
            400,
        )
        .caused_by(ApiError::new("illegal_argument_exception", reason, 400)),
    )
}

fn parse_bucket_sort_entries(t: &Map<String, Value>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let arr: Vec<Value> = match t.get("sort") {
        Some(Value::Array(a)) => a.clone(),
        Some(v) => vec![v.clone()],
        None => return out,
    };
    for e in arr {
        match e {
            Value::String(p) => out.push((p, false)),
            Value::Object(m) => {
                for (p, v) in m {
                    let desc = match &v {
                        Value::String(s) => s == "desc",
                        Value::Object(o) => o
                            .get("order")
                            .and_then(|x| x.as_str())
                            .map(|s| s == "desc")
                            .unwrap_or(false),
                        _ => false,
                    };
                    out.push((p, desc));
                }
            }
            _ => {}
        }
    }
    out
}

fn apply_bucket_sort(
    buckets: &mut Vec<Value>,
    name: &str,
    t: &Map<String, Value>,
    idx: &Index,
    prefix: &str,
) -> Result<(), ApiError> {
    let _ = (idx, prefix);
    if let Some(v) = t.get("from") {
        let f = v.as_i64().unwrap_or(0);
        if f < 0 {
            return Err(bucket_sort_parse_fail(&format!(
                "[from] must be a non-negative integer: [{}]",
                f
            )));
        }
    }
    if let Some(v) = t.get("size") {
        let s = v.as_i64().unwrap_or(0);
        if s < 1 {
            return Err(bucket_sort_parse_fail(&format!(
                "[size] must be a positive integer: [{}]",
                s
            )));
        }
    }
    let sorts = parse_bucket_sort_entries(t);
    let from = t.get("from").and_then(|v| v.as_i64()).unwrap_or(0) as usize;
    let size = t.get("size").and_then(|v| v.as_i64()).map(|s| s as usize);
    if sorts.is_empty() && size.is_none() && t.get("from").is_none() {
        return Err(validation_failed(&format!(
            "[{}] is configured to perform nothing. Please set either of [sort, size, from] to use bucket_sort;",
            name
        )));
    }
    let n = buckets.len();
    if from >= n {
        buckets.clear();
        return Ok(());
    }
    if sorts.is_empty() {
        let end = match size {
            Some(s) => (from + s).min(n),
            None => n,
        };
        *buckets = buckets[from..end].to_vec();
        return Ok(());
    }
    let gap = parse_gap_policy(t)?;
    #[derive(Clone)]
    enum SortVal {
        Num(f64),
        Str(String),
    }
    let mut items: Vec<(usize, Vec<Option<SortVal>>)> = Vec::new();
    for (i, b) in buckets.iter().enumerate() {
        let bm = match b.as_object() {
            Some(m) => m,
            None => continue,
        };
        let mut vals: Vec<Option<SortVal>> = Vec::new();
        let mut all_missing = true;
        for (p, _desc) in &sorts {
            let sv = if p == "_key" {
                match bm.get("key") {
                    Some(Value::Number(n)) => Some(SortVal::Num(n.as_f64().unwrap_or(f64::NAN))),
                    Some(Value::String(s)) => Some(SortVal::Str(s.clone())),
                    _ => None,
                }
            } else if p == "_count" {
                Some(SortVal::Num(
                    bm.get("doc_count").and_then(|v| v.as_f64()).unwrap_or(0.0),
                ))
            } else {
                let toks = parse_path_tokens(p);
                let v = match gap {
                    PipeGap::InsertZeros => {
                        apply_gap_opt(resolve_parent_path(bm, &toks, p)?, gap)
                            .map(|x| if x.is_nan() { 0.0 } else { x })
                    }
                    PipeGap::Skip => resolve_parent_path(bm, &toks, p)?
                        .filter(|x| !x.is_nan()),
                };
                v.map(SortVal::Num)
            };
            if sv.is_some() {
                all_missing = false;
            }
            vals.push(sv);
        }
        if all_missing {
            continue;
        }
        items.push((i, vals));
    }
    let cmp_sv = |a: &SortVal, b: &SortVal| -> Ordering {
        match (a, b) {
            (SortVal::Num(x), SortVal::Num(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
            (SortVal::Str(x), SortVal::Str(y)) => x.cmp(y),
            _ => Ordering::Equal,
        }
    };
    items.sort_by(|a, b| {
        for (k, (_p, desc)) in sorts.iter().enumerate() {
            let av = &a.1[k];
            let bv = &b.1[k];
            let c = match (av, bv) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) => {
                    let c = cmp_sv(x, y);
                    if *desc {
                        c.reverse()
                    } else {
                        c
                    }
                }
            };
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    });
    let limit = (from + size.unwrap_or(items.len())).min(items.len());
    let start = from.min(limit);
    let out: Vec<Value> = items[start..limit]
        .iter()
        .map(|(i, _)| buckets[*i].clone())
        .collect();
    *buckets = out;
    Ok(())
}

fn finish_bucket_pipelines(
    def: &Map<String, Value>,
    buckets: &mut Vec<Value>,
    kind: &str,
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<(), ApiError> {
    let sub = match sub_aggs(def) {
        Some(s) => s,
        None => return Ok(()),
    };
    let mut specs: Vec<(String, &'static str, Map<String, Value>)> = Vec::new();
    let pipeline_names: Vec<String> = sub
        .iter()
        .filter(|(_, v)| {
            v.as_object()
                .map(|d| pipeline_type_of(d).is_some())
                .unwrap_or(false)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for (pname, praw) in &sub {
        if let Some(pdef) = praw.as_object() {
            if let Some(pt) = pipeline_type_of(pdef) {
                if is_parent_scoped_pipeline(pt) {
                    let t = pdef
                        .get(pt)
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    specs.push((pname.clone(), pt, t));
                }
            }
        }
    }
    if specs.is_empty() {
        return Ok(());
    }
    let hist_like = matches!(kind, "histogram" | "date_histogram" | "auto_date_histogram");
    for (pname, pt, t) in &specs {
        if !hist_like
            && matches!(
                *pt,
                "derivative" | "cumulative_sum" | "serial_diff" | "moving_fn" | "moving_avg"
            )
        {
            return Err(validation_failed(&format!(
                "{} aggregation [{}] must have a histogram, date_histogram or auto_date_histogram as parent;",
                pt, pname
            )));
        }
        if *pt == "moving_fn" {
            if agg_script_source(t).is_none() {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    "Required [script]",
                    400,
                ));
            }
            match t.get("window").and_then(|v| v.as_i64()) {
                None => {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        "Required [window]",
                        400,
                    ))
                }
                Some(w) if w <= 0 => {
                    let cause = ApiError::new(
                        "illegal_argument_exception",
                        "[window] must be a positive, non-zero integer.",
                        400,
                    );
                    let mid = ApiError::new(
                        "x_content_parse_exception",
                        "Failed to build [window] after last required field arrived",
                        400,
                    )
                    .caused_by(cause);
                    return Err(ApiError::new(
                        "x_content_parse_exception",
                        "Failed to build [moving_fn] after last required field arrived",
                        400,
                    )
                    .caused_by(mid));
                }
                _ => {}
            }
        }
        if *pt == "serial_diff" {
            let lag = t.get("lag").and_then(|v| v.as_i64()).unwrap_or(1);
            if lag <= 0 {
                return Err(ApiError::new(
                    "parsing_exception",
                    &format!(
                        "Lag must be a positive, non-zero integer.  Value supplied was{} in [{}]: [lag].",
                        lag, pname
                    ),
                    400,
                ));
            }
        }
        if *pt == "cumulative_sum" {
            for k in t.keys() {
                if k != "buckets_path" && k != "format" && k != "meta" {
                    return Err(ApiError::new(
                        "x_content_parse_exception",
                        &format!("[cumulative_sum] unknown field [{}]", k),
                        400,
                    ));
                }
            }
        }
        if *pt == "bucket_sort" {
            for (p, _d) in parse_bucket_sort_entries(t) {
                if p == "_key" || p == "_count" || p == "_bucket_count" {
                    continue;
                }
                validate_bucket_path(&sub, &p, &pipeline_names)?;
            }
        } else {
            let paths: Vec<String> = match t.get("buckets_path") {
                Some(Value::String(p)) => vec![p.clone()],
                Some(Value::Object(m)) => m
                    .values()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => {
                    return Err(ApiError::new(
                        "parsing_exception",
                        &format!(
                            "Missing required field [buckets_path] for aggregation [{}]",
                            pname
                        ),
                        400,
                    ))
                }
            };
            for p in &paths {
                validate_bucket_path(&sub, p, &pipeline_names)?;
            }
        }
    }
    for (pname, pt, t) in &specs {
        match *pt {
            "derivative" => compute_derivative(buckets, pname, t, idx, prefix, kind)?,
            "cumulative_sum" => compute_cumulative_sum(buckets, pname, t)?,
            "serial_diff" => compute_serial_diff(buckets, pname, t)?,
            "moving_fn" => compute_moving_fn(buckets, pname, t, idx, prefix)?,
            "moving_avg" => compute_moving_avg(buckets, pname, t)?,
            "bucket_script" => compute_bucket_script(buckets, pname, t, idx, prefix)?,
            "bucket_selector" => apply_bucket_selector(buckets, pname, t, idx, prefix)?,
            "bucket_sort" => apply_bucket_sort(buckets, pname, t, idx, prefix)?,
            _ => {}
        }
    }
    let _ = typed;
    Ok(())
}

fn significant_agg(
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
    text: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    if field.is_empty() || !is_mapped_field(idx, &field, prefix) {
        return Ok(json!({"buckets": []}));
    }
    let bg_docs: Vec<Value> = match t.get("background_filter").and_then(|v| v.as_object()) {
        Some(q) => {
            let q = q.clone();
            all_live_docs(idx)
                .into_iter()
                .filter(|d| doc_matches(idx, d, &q))
                .collect()
        }
        None => all_live_docs(idx),
    };
    let (kind, inc_neg, bg_superset) = sig_heuristic(t);
    let size = t
        .get("size")
        .and_then(to_f64)
        .map(|f| f as usize)
        .unwrap_or(10);
    let min_doc_count = t
        .get("min_doc_count")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(3);
    let terms_of = |d: &Value| -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = vec![];
        let mut seen: HashSet<String> = HashSet::new();
        for v in doc_field_values(idx, d, &field, prefix) {
            if text {
                if let Some(s) = v.as_str() {
                    for tok in analyze_tokens(s) {
                        if seen.insert(tok.clone()) {
                            out.push((tok.clone(), Value::String(tok)));
                        }
                    }
                }
            } else {
                let k = value_to_string(&v).unwrap_or_default();
                if seen.insert(k.clone()) {
                    out.push((k, v));
                }
            }
        }
        out
    };
    let mut fg: HashMap<String, i64> = HashMap::new();
    let mut bg: HashMap<String, i64> = HashMap::new();
    let mut keys: HashMap<String, Value> = HashMap::new();
    let mut fg_doc_ids: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, d) in docs.iter().enumerate() {
        for (k, v) in terms_of(d) {
            keys.entry(k.clone()).or_insert(v);
            *fg.entry(k.clone()).or_insert(0) += 1;
            fg_doc_ids.entry(k).or_insert_with(Vec::new).push(i);
        }
    }
    for d in &bg_docs {
        for (k, _) in terms_of(d) {
            *bg.entry(k).or_insert(0) += 1;
        }
    }
    let ss = docs.len() as f64;
    let gs = bg_docs.len() as f64;
    let mut scored: Vec<(f64, String, i64, i64)> = vec![];
    for (k, f) in &fg {
        if *f < min_doc_count {
            continue;
        }
        let g = *bg.get(k).unwrap_or(&0);
        let s = sig_score(&kind, inc_neg, bg_superset, *f as f64, ss, g as f64, gs);
        if !(s > 0.0) {
            continue;
        }
        scored.push((s, k.clone(), *f, g));
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut out_buckets: Vec<Value> = vec![];
    for (s, k, f, g) in scored.into_iter().take(size) {
        let mut bm = Map::new();
        bm.insert(
            "key".to_string(),
            keys.get(&k).cloned().unwrap_or(Value::Null),
        );
        bm.insert("doc_count".to_string(), json!(f));
        bm.insert("score".to_string(), json!(s));
        bm.insert("bg_count".to_string(), json!(g));
        if let Some(sub) = sub_aggs(def) {
            let refs: Vec<&Value> = fg_doc_ids
                .get(&k)
                .map(|ids| ids.iter().map(|&i| docs[i]).collect())
                .unwrap_or_default();
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out_buckets.push(Value::Object(bm));
    }
    Ok(json!({
        "doc_count": docs.len(),
        "bg_count": bg_docs.len(),
        "buckets": out_buckets,
    }))
}

fn terms_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    for k in t.keys() {
        if !TERMS_ALLOWED.contains(&k.as_str()) {
            return Err(ApiError::new(
                "x_content_parse_exception",
                &format!("[terms] unknown field [{}]", k),
                400,
            ));
        }
    }
    let field = map_str(t, "field");
    if field == "_ignored" {
        let reason = "Fielddata is not supported on field [_ignored] of type [_ignored]";
        let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
        let cause = ApiError::new("illegal_argument_exception", reason, 400);
        e.root_cause = Some(Box::new(cause.clone()));
        e.caused_by = Some(Box::new(cause));
        return Err(e);
    }
    if !field.is_empty() {
        if let Some(def) = crate::eval::mapping_field_def(idx, &field) {
            if def.get("type").and_then(|v| v.as_str()) == Some("search_as_you_type") {
                let reason = format!(
                    "Fielddata is not supported on field [{}] of type [search_as_you_type]",
                    field
                );
                let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
                let cause = ApiError::new("illegal_argument_exception", &reason, 400);
                e.root_cause = Some(Box::new(cause.clone()));
                e.caused_by = Some(Box::new(cause));
                return Err(e);
            }
            if def.get("doc_values").and_then(|v| v.as_bool()) == Some(false) {
                let ftype = def.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let reason = format!(
                    "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
                    field, ftype
                );
                let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
                let cause = ApiError::new("illegal_argument_exception", &reason, 400);
                e.root_cause = Some(Box::new(cause.clone()));
                e.caused_by = Some(Box::new(cause));
                return Err(e);
            }
        }
    }
    if let Some(hint) = t.get("execution_hint").and_then(|v| v.as_str()) {
        if hint != "map" && hint != "global_ordinals" {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "Unknown `execution_hint`: [{}], expected any of [map, global_ordinals]",
                    hint
                ),
                400,
            ));
        }
    }
    if let Some(mode) = t.get("collect_mode").and_then(|v| v.as_str()) {
        if mode != "breadth_first" && mode != "depth_first" {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("no [collect_mode] found for value [{}]", mode),
                400,
            ));
        }
    }
    let field = map_str(t, "field");
    let resolved = crate::eval::resolve_alias_field(idx, &resolve_field(&field, prefix));
    if let Some(ft) = idx.field_types.get(&resolved) {
        let fd = crate::eval::mapping_field_def(idx, &resolved)
            .and_then(|d| d.get("fielddata"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if ft == "text" && !fd {
            return Err(search_phase_illegal(&format!(
                "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                field
            )));
        }
    }
    if let Some(ft) = idx.field_types.get(&resolved) {
        if ft.ends_with("_range") {
            return Err(search_phase_illegal(&format!(
                "Field [{}] of type [{}] is not supported for aggregation [terms]",
                field, ft
            )));
        }
    }
    let size = t.get("size").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    if size == 0 {
        let msg = format!("[size] must be greater than 0. Found [0] in [{}]", name);
        return Err(
            ApiError::new("illegal_argument_exception", &msg, 400)
                .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
        );
    }
    let min_doc_count = t
        .get("min_doc_count")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(1);
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let show_err = t
        .get("show_term_doc_count_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let format = map_str(t, "format");
    let missing = t.get("missing").cloned();
    let value_type = map_str(t, "value_type");
    let include = t.get("include").cloned();
    let exclude = t.get("exclude").cloned();
    for spec in [&include, &exclude] {
        if let Some(Value::String(p)) = spec {
            if !p.is_empty() && regex_matches(p, "").is_none() {
                let msg = format!("expected ']' at position {}", lucene_regex_pos(p));
                return Err(ApiError::new("illegal_argument_exception", &msg, 400));
            }
        }
    }

    let mut buckets: HashMap<String, TermBucket> = HashMap::new();
    let mut order_ids: Vec<String> = vec![];
    let make_bucket = |raw: &Value| -> TermBucket {
        let (key, kas) = if value_type == "long" {
            match to_f64(raw) {
                Some(f) => (json!(f as i64), None),
                None => (raw.clone(), None),
            }
        } else {
            term_key_and_string(idx, &field, prefix, raw, &format)
        };
        let ord = if value_type == "long" {
            OrdKey::Num(to_f64(&key).unwrap_or(0.0))
        } else {
            ord_key_of(idx, &field, prefix, &key)
        };
        TermBucket {
            raw: raw.clone(),
            key,
            key_as_string: kas,
            ord,
            count: 0,
            docs: vec![],
        }
    };
    let script_src = agg_script_source(t);
    let script_params = agg_script_params(t);
    let script_only = script_src.is_some() && field.is_empty();
    let value_script = script_src.is_some() && !field.is_empty();
    for d in docs {
        let join_handled: Option<(String, String)> = if script_src.is_none() {
            match agg_join_field(idx) {
                Some(jf) if field == jf => Some((jf.clone(), String::new())),
                Some(jf) if field.starts_with(&format!("{}#", jf)) => {
                    Some((jf.clone(), field[jf.len() + 1..].to_string()))
                }
                _ => None,
            }
        } else {
            None
        };
        let effective: Vec<Value> = if let Some((jf, virt)) = &join_handled {
            let (name, parent) = agg_join_name(d.get(jf));
            if virt.is_empty() {
                if name.is_empty() {
                    vec![]
                } else {
                    vec![Value::String(name)]
                }
            } else if name == *virt {
                d.get("_id").cloned().map(|v| vec![v]).unwrap_or_default()
            } else {
                let is_child = join_relation_children(idx, jf)
                    .iter()
                    .any(|(p, c)| p == virt && *c == name);
                if is_child {
                    let pid = d
                        .get("_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| idx.routing.get(id))
                        .cloned()
                        .or_else(|| {
                            if parent.is_empty() {
                                None
                            } else {
                                Some(parent.clone())
                            }
                        });
                    pid.map(|p| vec![Value::String(p)]).unwrap_or_default()
                } else {
                    vec![]
                }
            }
        } else if script_only {
            let r = run_agg_script(idx, d, script_src.as_deref().unwrap(), &script_params, None)?;
            match r {
                Value::Null => vec![],
                other => vec![Value::String(key_string(&other))],
            }
        } else {
            let vals = doc_field_values(idx, d, &field, prefix);
            let mut eff: Vec<Value> = vals;
            if eff.is_empty() {
                if let Some(m) = &missing {
                    eff.push(m.clone());
                }
            }
            if value_script {
                let src = script_src.as_deref().unwrap();
                let mut mapped: Vec<Value> = vec![];
                for v in eff {
                    let r = run_agg_script(idx, d, src, &script_params, Some(&v))?;
                    if !r.is_null() {
                        mapped.push(r);
                    }
                }
                mapped
            } else {
                eff
            }
        };
        for v in effective {
            let id = value_to_string(&v).unwrap_or_default();
            if !buckets.contains_key(&id) {
                let b = make_bucket(&v);
                order_ids.push(id.clone());
                buckets.insert(id.clone(), b);
            }
            let e = buckets.get_mut(&id).unwrap();
            e.count += 1;
            e.docs.push(Value::clone(d));
        }
    }
    if min_doc_count == 0 && is_mapped_field(idx, &field, prefix) {
        for d in all_live_docs(idx) {
            for v in doc_field_values(idx, &d, &field, prefix) {
                let id = value_to_string(&v).unwrap_or_default();
                if !buckets.contains_key(&id) {
                    let b = make_bucket(&v);
                    order_ids.push(id.clone());
                    buckets.insert(id, b);
                }
            }
        }
    }
    let orders = parse_order_clause(t.get("order"))?;
    let mut metric_vals: HashMap<(String, String), Option<f64>> = HashMap::new();
    for (path, _) in &orders {
        if path != "_key" && path != "_count" {
            for id in &order_ids {
                let b = &buckets[id];
                let refs: Vec<&Value> = b.docs.iter().collect();
                let v = resolve_order_value(path, def, &refs, idx, prefix, typed)?;
                metric_vals.insert((id.clone(), path.clone()), v);
            }
        }
    }
    order_ids.sort_by(|a, b| {
        let ba = &buckets[a];
        let bb = &buckets[b];
        for (path, desc) in &orders {
            let c = match path.as_str() {
                "_key" => ba.ord.cmp(&bb.ord),
                "_count" => ba.count.cmp(&bb.count),
                _ => {
                    let va = metric_vals.get(&(a.clone(), path.clone())).copied().flatten();
                    let vb = metric_vals.get(&(b.clone(), path.clone())).copied().flatten();
                    match (va, vb) {
                        (None, None) => Ordering::Equal,
                        (None, Some(_)) => Ordering::Less,
                        (Some(_), None) => Ordering::Greater,
                        (Some(x), Some(y)) => {
                            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
                        }
                    }
                }
            };
            let c = if *desc { c.reverse() } else { c };
            if c != Ordering::Equal {
                return c;
            }
        }
        bb.count
            .cmp(&ba.count)
            .then_with(|| ba.ord.cmp(&bb.ord))
    });
    let mut sum_other = 0usize;
    let mut out_buckets = vec![];
    let mut kept = 0usize;
    for id in &order_ids {
        let b = &buckets[id];
        if !value_matches_include_exclude(&b.raw, &b.key, include.as_ref(), exclude.as_ref()) {
            continue;
        }
        if (b.count as i64) < min_doc_count {
            continue;
        }
        if kept >= size {
            sum_other += b.count;
            continue;
        }
        kept += 1;
        let mut bm = Map::new();
        bm.insert("key".to_string(), b.key.clone());
        if let Some(kas) = &b.key_as_string {
            bm.insert("key_as_string".to_string(), json!(kas));
        }
        bm.insert("doc_count".to_string(), json!(b.count));
        if show_err {
            bm.insert("doc_count_error_upper_bound".to_string(), json!(0));
        }
        if let Some(sub) = sub_aggs(def) {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out_buckets.push(Value::Object(bm));
    }
    finish_bucket_pipelines(def, &mut out_buckets, "terms", idx, prefix, typed)?;
    if keyed {
        let mut obj = Map::new();
        for b in out_buckets {
            let k = b
                .get("key_as_string")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    b.get("key").and_then(value_to_string).unwrap_or_default()
                });
            obj.insert(k, b);
        }
        return Ok(json!({
            "doc_count_error_upper_bound": 0,
            "sum_other_doc_count": sum_other,
            "buckets": obj,
        }));
    }
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": sum_other,
        "buckets": out_buckets,
    }))
}

fn resolve_order_value(
    path: &str,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Option<f64>, ApiError> {
    let segs: Vec<String> = if path.contains('>') {
        path.split('>').map(|s| s.to_string()).collect()
    } else if path.contains('[') {
        path.replace('[', ".")
            .replace(']', "")
            .split('.')
            .map(|s| s.to_string())
            .collect()
    } else {
        path.split('.').map(|s| s.to_string()).collect()
    };
    let sub = match sub_aggs(def) {
        Some(s) => s,
        None => {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                &format!(
                    "Invalid aggregation order path [{}]. The provided aggregation [{}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                    path,
                    segs.first().cloned().unwrap_or_default()
                ),
                500,
            ))
        }
    };
    resolve_order_segs(&segs, &sub, docs, idx, prefix, typed, path)
}

fn resolve_order_segs(
    segs: &[String],
    sub: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
    full_path: &str,
) -> Result<Option<f64>, ApiError> {
    if segs.is_empty() {
        return Ok(None);
    }
    let head = &segs[0];
    let rest = &segs[1..];
    let adef = match sub.get(head).and_then(|v| v.as_object()) {
        Some(d) => d,
        None => {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                &format!(
                    "Invalid aggregation order path [{}]. The provided aggregation [{}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                    full_path, head
                ),
                500,
            ))
        }
    };
    let is_single_bucket = adef.contains_key("filter")
        || adef.contains_key("global")
        || adef.contains_key("nested")
        || adef.contains_key("missing")
        || adef.contains_key("filters");
    let is_multi_bucket = adef.contains_key("terms")
        || adef.contains_key("histogram")
        || adef.contains_key("date_histogram")
        || adef.contains_key("range")
        || adef.contains_key("multi_terms")
        || adef.contains_key("composite");
    if rest.is_empty() {
        if is_multi_bucket {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                &format!(
                    "Invalid aggregation order path [{}]. Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
                    full_path
                ),
                500,
            ));
        }
        if head == "_count" {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                &format!(
                    "Invalid aggregation order path [{}]. The provided aggregation [_count] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                    full_path
                ),
                500,
            ));
        }
        let res = eval_one(head, adef, docs, idx, prefix, typed)?;
        if let Some(obj) = res.as_object() {
            if let Some(v) = obj.get("value") {
                return Ok(v.as_f64());
            }
            if obj.contains_key("avg") {
                return Err(ApiError::new(
                    "aggregation_execution_exception",
                    &format!(
                        "Invalid aggregation order path [{}]. When ordering on a multi-value metrics aggregation a metric name must be specified.",
                        full_path
                    ),
                    500,
                ));
            }
            if let Some(dc) = obj.get("doc_count") {
                return Ok(dc.as_f64());
            }
        }
        return Ok(None);
    }
    if head == "_count" {
        return Err(ApiError::new(
            "aggregation_execution_exception",
            &format!(
                "Invalid aggregation order path [{}]. The provided aggregation [_count] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                full_path
            ),
            500,
        ));
    }
    if is_multi_bucket && !is_single_bucket {
        return Err(ApiError::new(
            "aggregation_execution_exception",
            &format!(
                "Invalid aggregation order path [{}]. Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
                full_path
            ),
            500,
        ));
    }
    if rest.len() == 1 {
        let parent = eval_one(head, adef, docs, idx, prefix, typed)?;
        if let Some(obj) = parent.as_object() {
            if let Some(v) = obj.get(&rest[0]) {
                if let Some(f) = v.as_f64() {
                    return Ok(Some(f));
                }
                if v.is_null() {
                    return Ok(None);
                }
            }
        }
    }
    let subsub = match sub_aggs(adef) {
        Some(s) => s,
        None => {
            return Err(ApiError::new(
                "aggregation_execution_exception",
                &format!(
                    "Invalid aggregation order path [{}]. The provided aggregation [{}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.",
                    full_path, rest[0]
                ),
                500,
            ))
        }
    };
    if let Some(q) = adef.get("filter").and_then(|v| v.as_object()) {
        let filtered: Vec<&Value> = docs.iter().filter(|d| doc_matches(idx, d, q)).copied().collect();
        return resolve_order_segs(rest, &subsub, &filtered, idx, prefix, typed, full_path);
    }
    resolve_order_segs(rest, &subsub, docs, idx, prefix, typed, full_path)
}

fn multi_terms_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let terms = t
        .get("terms")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if terms.len() < 2 {
        let msg = format!(
            "multi term aggregation must has at least 2 terms. Found [{}] in [{}] Use terms aggregation for single term aggregation",
            terms.len(),
            name
        );
        return Err(
            ApiError::new("illegal_argument_exception", &msg, 400)
                .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
        );
    }
    let size = t.get("size").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    let orders = parse_order_clause(t.get("order"))?;

    struct Bucket {
        key: Vec<Value>,
        kas: String,
        count: usize,
        docs: Vec<Value>,
    }

    let mut buckets: HashMap<String, Bucket> = HashMap::new();
    for d in docs {
        let mut term_choices: Vec<Vec<(Value, String)>> = vec![];
        let mut skip = false;
        for spec in &terms {
            let sm = match spec.as_object() {
                Some(m) => m,
                None => continue,
            };
            let field = map_str(sm, "field");
            let missing = sm.get("missing").cloned();
            let mut vals = doc_field_values(idx, d, &field, prefix);
            if vals.is_empty() {
                match missing {
                    Some(m) => vals.push(m),
                    None => {
                        skip = true;
                        break;
                    }
                }
            }
            let ft = field_kind(idx, &field, prefix);
            let mut choices: Vec<(Value, String)> = vec![];
            for v in vals {
                let (mut k, kas) = term_key_and_string(idx, &field, prefix, &v, "");
                let kas = kas.unwrap_or_else(|| key_string(&k));
                if ft == "date" || ft == "date_nanos" {
                    k = Value::String(kas.clone());
                }
                choices.push((k, kas));
            }
            term_choices.push(choices);
        }
        if skip || term_choices.len() != terms.len() || term_choices.iter().any(|c| c.is_empty()) {
            continue;
        }
        let mut combos: Vec<Vec<(Value, String)>> = vec![vec![]];
        for choices in &term_choices {
            let mut next: Vec<Vec<(Value, String)>> = vec![];
            for combo in &combos {
                for ch in choices {
                    let mut c = combo.clone();
                    c.push(ch.clone());
                    next.push(c);
                }
            }
            combos = next;
        }
        for per_term in combos {
            let id: String = per_term
                .iter()
                .map(|(_, s)| s.clone())
                .collect::<Vec<_>>()
                .join("|");
            let entry = buckets.entry(id).or_insert_with(|| Bucket {
                key: per_term.iter().map(|(k, _)| k.clone()).collect(),
                kas: per_term
                    .iter()
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>()
                    .join("|"),
                count: 0,
                docs: vec![],
            });
            entry.count += 1;
            entry.docs.push(Value::clone(d));
        }
    }
    let mut ids: Vec<String> = buckets.keys().cloned().collect();
    let mut metric_vals: HashMap<(String, String), Option<f64>> = HashMap::new();
    for (path, _) in &orders {
        if path != "_key" && path != "_count" {
            for id in &ids {
                let b = &buckets[id];
                let refs: Vec<&Value> = b.docs.iter().collect();
                let v = resolve_order_value(path, def, &refs, idx, prefix, typed)?;
                metric_vals.insert((id.clone(), path.clone()), v);
            }
        }
    }
    ids.sort_by(|a, b| {
        let ba = &buckets[a];
        let bb = &buckets[b];
        for (path, desc) in &orders {
            let c = match path.as_str() {
                "_key" => ba.kas.cmp(&bb.kas),
                "_count" => ba.count.cmp(&bb.count),
                _ => {
                    let va = metric_vals.get(&(a.clone(), path.clone())).copied().flatten();
                    let vb = metric_vals.get(&(b.clone(), path.clone())).copied().flatten();
                    match (va, vb) {
                        (None, None) => Ordering::Equal,
                        (None, Some(_)) => Ordering::Less,
                        (Some(_), None) => Ordering::Greater,
                        (Some(x), Some(y)) => {
                            x.partial_cmp(&y).unwrap_or(Ordering::Equal)
                        }
                    }
                }
            };
            let c = if *desc { c.reverse() } else { c };
            if c != Ordering::Equal {
                return c;
            }
        }
        bb.count.cmp(&ba.count).then_with(|| ba.kas.cmp(&bb.kas))
    });
    let total: usize = buckets.values().map(|b| b.count).sum();
    let mut out = vec![];
    let mut shown = 0usize;
    for id in ids.iter().take(size) {
        let b = &buckets[id];
        shown += b.count;
        let mut bm = Map::new();
        bm.insert("key".to_string(), Value::Array(b.key.clone()));
        bm.insert("key_as_string".to_string(), json!(b.kas));
        bm.insert("doc_count".to_string(), json!(b.count));
        if let Some(sub) = sub_aggs(def) {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    Ok(json!({
        "doc_count_error_upper_bound": 0,
        "sum_other_doc_count": total - shown,
        "buckets": out,
    }))
}

fn rare_terms_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let max_doc_count = t
        .get("max_doc_count")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(1);
    if max_doc_count > 100 {
        let msg = format!("[max_doc_count] must be smallerthan 100in [{}]", name);
        return Err(
            ApiError::new("illegal_argument_exception", &msg, 400)
                .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
        );
    }
    let include = t.get("include").cloned();
    let exclude = t.get("exclude").cloned();

    struct Bucket {
        key: Value,
        count: usize,
        docs: Vec<Value>,
    }
    let mut buckets: HashMap<String, Bucket> = HashMap::new();
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            let (key, _kas) = term_key_and_string(idx, &field, prefix, &v, "");
            let id = key_string(&key);
            let e = buckets.entry(id).or_insert_with(|| Bucket {
                key: key.clone(),
                count: 0,
                docs: vec![],
            });
            e.count += 1;
            e.docs.push(Value::clone(d));
        }
    }
    let mut ids: Vec<String> = buckets.keys().cloned().collect();
    ids.sort_by(|a, b| {
        let ba = &buckets[a];
        let bb = &buckets[b];
        ba.count
            .cmp(&bb.count)
            .then_with(|| key_string(&ba.key).cmp(&key_string(&bb.key)))
    });
    let mut out = vec![];
    for id in ids {
        let b = &buckets[&id];
        if (b.count as i64) > max_doc_count {
            continue;
        }
        if !value_matches_include_exclude(&b.key, &b.key, include.as_ref(), exclude.as_ref()) {
            continue;
        }
        let mut bm = Map::new();
        bm.insert("key".to_string(), b.key.clone());
        bm.insert("doc_count".to_string(), json!(b.count));
        if let Some(sub) = sub_aggs(def) {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    Ok(json!({"buckets": out}))
}

fn fmt_key(v: &Value) -> String {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) => fmt_bound(f),
            None => n.to_string(),
        },
        other => key_string(other),
    }
}

fn is_numeric_type(ft: &str) -> bool {
    matches!(
        ft,
        "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float"
            | "unsigned_long"
    )
}

fn cmp_vals_asc(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        _ => Ordering::Equal,
    }
}

fn order_of(t: &Map<String, Value>) -> (String, bool) {
    if let Some(Value::Object(o)) = t.get("order") {
        if let Some((k, v)) = o.iter().next() {
            return (k.clone(), v.as_str() == Some("desc"));
        }
    }
    ("_key".to_string(), false)
}

fn sub_order_value(bm: &Map<String, Value>, name: &str) -> Value {
    match bm.get(name) {
        Some(Value::Object(m)) => match m.get("value") {
            Some(v) => v.clone(),
            None => Value::Object(m.clone()),
        },
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn parse_calendar_interval(iv: &str) -> Option<&'static str> {
    Some(match iv {
        "minute" | "1m" => "minute",
        "hour" | "1h" => "hour",
        "day" | "1d" => "day",
        "week" | "1w" => "week",
        "month" | "1M" => "month",
        "quarter" | "1q" => "quarter",
        "year" | "1y" => "year",
        _ => return None,
    })
}

fn parse_fixed_interval(iv: &str) -> Option<i64> {
    let b = iv.as_bytes();
    if b.is_empty() || !b[0].is_ascii_digit() {
        return None;
    }
    let num: String = iv.chars().take_while(|c| c.is_ascii_digit()).collect();
    let unit = &iv[num.len()..];
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    let unit_ms = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * unit_ms)
}

fn parse_offset_duration(s: &str) -> i64 {
    let t = s.trim();
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1i64, t.strip_prefix('+').unwrap_or(t)),
    };
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        return 0;
    }
    let n: i64 = num.parse().unwrap_or(0);
    let unit = &rest[num.len()..];
    let ms = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" | "H" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => 0,
    };
    sign * n * ms
}

fn err_cause(cause: &str) -> ApiError {
    let mut e = ApiError::new("illegal_argument_exception", cause, 400);
    e.caused_by = Some(Box::new(ApiError::new("parsing_exception", cause, 400)));
    e
}

fn fmt_zone_offset(off: i64) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    let h = a / 3_600_000;
    let m = (a % 3_600_000) / 60_000;
    format!("{}{:02}:{:02}", sign, h, m)
}

fn dh_format(format: &str, ms: i64) -> String {
    match format {
        "epoch_millis" => ms.to_string(),
        "epoch_second" => ms.div_euclid(1000).to_string(),
        "strict_date" => apply_java_date_format("yyyy-MM-dd", ms)
            .unwrap_or_else(|| format_epoch_ms(ms)),
        "basic_date" => {
            apply_java_date_format("yyyyMMdd", ms).unwrap_or_else(|| format_epoch_ms(ms))
        }
        _ => apply_java_date_format(format, ms).unwrap_or_else(|| format_epoch_ms(ms)),
    }
}

fn weekday_of(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    let start = month_start_ms(year, month);
    let next = if month == 12 {
        month_start_ms(year + 1, 1)
    } else {
        month_start_ms(year, month + 1)
    };
    (next - start) / 86_400_000
}

fn nth_sunday_ms(year: i64, month: i64, nth: i64) -> i64 {
    let start_days = month_start_ms(year, month).div_euclid(86_400_000);
    let first_dow = weekday_of(start_days);
    let delta = (6 - first_dow).rem_euclid(7);
    (start_days + delta + (nth - 1) * 7) * 86_400_000
}

fn last_sunday_ms(year: i64, month: i64) -> i64 {
    let start_days = month_start_ms(year, month).div_euclid(86_400_000);
    let total = days_in_month(year, month);
    let last_days = start_days + total - 1;
    let last_dow = weekday_of(last_days);
    let back = (last_dow - 6).rem_euclid(7);
    (last_days - back) * 86_400_000
}

fn parse_hhmm(s: &str) -> Option<i64> {
    let (h, m) = match s.find(':') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, "0"),
    };
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(h * 3_600_000 + m * 60_000)
}

fn zone_offset(zone: &str, t: i64) -> Result<i64, String> {
    if zone.is_empty() || zone == "UTC" || zone == "Z" || zone == "GMT" || zone == "Etc/UTC" {
        return Ok(0);
    }
    if let Some(rest) = zone.strip_prefix('+') {
        return parse_hhmm(rest).ok_or_else(|| format!("Unknown time-zone ID: {}", zone));
    }
    if let Some(rest) = zone.strip_prefix('-') {
        return parse_hhmm(rest)
            .map(|v| -v)
            .ok_or_else(|| format!("Unknown time-zone ID: {}", zone));
    }
    let (y, _m, _d) = civil_from_days(t.div_euclid(86_400_000));
    match zone {
        "America/New_York" => {
            let start = nth_sunday_ms(y, 3, 2) + 7 * 3_600_000;
            let end = nth_sunday_ms(y, 11, 1) + 6 * 3_600_000;
            if t >= start && t < end {
                Ok(-4 * 3_600_000)
            } else {
                Ok(-5 * 3_600_000)
            }
        }
        "Europe/Kyiv" => {
            let start = last_sunday_ms(y, 3) + 3_600_000;
            let end = last_sunday_ms(y, 10) + 3_600_000;
            if t >= start && t < end {
                Ok(3 * 3_600_000)
            } else {
                Ok(2 * 3_600_000)
            }
        }
        _ => Err(format!("Unknown time-zone ID: {}", zone)),
    }
}

fn variable_width_histogram(
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    struct Cl {
        min: f64,
        max: f64,
        centroid: f64,
        docs: Vec<usize>,
    }
    fn add_val(c: &mut Cl, val: f64, di: usize) {
        c.max = c.max.max(val);
        c.min = c.min.min(val);
        let size = c.docs.len() as f64;
        c.centroid = ((c.centroid * size) + val) / (size + 1.0);
        c.docs.push(di);
    }
    fn merge(a: Cl, b: Cl) -> Cl {
        let n1 = a.docs.len() as f64;
        let n2 = b.docs.len() as f64;
        let mut docs = a.docs;
        docs.extend(b.docs);
        Cl {
            min: a.min.min(b.min),
            max: a.max.max(b.max),
            centroid: ((a.centroid * n1) + (b.centroid * n2)) / (n1 + n2),
            docs,
        }
    }
    let field = map_str(t, "field");
    let num_buckets = t.get("buckets").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    let shard_size = t
        .get("shard_size")
        .and_then(to_f64)
        .map(|f| f as usize)
        .unwrap_or(num_buckets * 50);
    let buffer_limit = t
        .get("initial_buffer")
        .and_then(to_f64)
        .map(|f| f as usize)
        .unwrap_or(5000);
    let mut vals: Vec<(f64, usize)> = vec![];
    for (di, d) in docs.iter().enumerate() {
        let mut prev: Option<f64> = None;
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some(x) = to_f64(&v) {
                if prev.map(|p| p == x).unwrap_or(false) {
                    continue;
                }
                prev = Some(x);
                vals.push((x, di));
            }
        }
    }
    if vals.is_empty() {
        return Ok(json!({"buckets": []}));
    }
    let initial_count = ((shard_size * 3) / 4).max(1);
    let buffer_n = vals.len().min(buffer_limit);
    let mut order: Vec<usize> = (0..buffer_n).collect();
    order.sort_by(|&a, &b| {
        vals[a].0.partial_cmp(&vals[b].0).unwrap_or(std::cmp::Ordering::Equal)
    });
    let docs_per_bucket = ((buffer_n as f64) / (initial_count as f64)).ceil() as usize;
    let docs_per_bucket = docs_per_bucket.max(1);
    let mut clusters: Vec<Cl> = vec![];
    for (i, &vi) in order.iter().enumerate() {
        let bucket_ord = i / docs_per_bucket;
        if clusters.len() == bucket_ord {
            clusters.push(Cl {
                min: vals[vi].0,
                max: vals[vi].0,
                centroid: vals[vi].0,
                docs: vec![vals[vi].1],
            });
        } else {
            add_val(&mut clusters[bucket_ord], vals[vi].0, vals[vi].1);
        }
    }
    let update_avg = |clusters: &Vec<Cl>| -> f64 {
        if clusters.len() > 1 {
            (clusters.last().unwrap().centroid - clusters[0].centroid) / (clusters.len() - 1) as f64
        } else {
            0.0
        }
    };
    let mut avg_dist = update_avg(&clusters);
    for i in buffer_n..vals.len() {
        let (val, di) = vals[i];
        let mut nearest = 0usize;
        let mut best = f64::INFINITY;
        for (ci, c) in clusters.iter().enumerate() {
            let d = (c.centroid - val).abs();
            if d < best {
                best = d;
                nearest = ci;
            }
        }
        if best > 2.0 * avg_dist && clusters.len() < shard_size {
            clusters.push(Cl {
                min: val,
                max: val,
                centroid: val,
                docs: vec![di],
            });
            let mut idx_pos = nearest;
            if val > clusters[nearest].centroid {
                idx_pos += 1;
            }
            let len = clusters.len();
            if idx_pos != len - 1 {
                let cl = clusters.pop().unwrap();
                clusters.insert(idx_pos, cl);
            }
            avg_dist = update_avg(&clusters);
        } else {
            add_val(&mut clusters[nearest], val, di);
            if nearest == 0 || nearest == clusters.len() - 1 {
                avg_dist = update_avg(&clusters);
            }
        }
    }
    let mut ranges = clusters;
    while ranges.len() > num_buckets {
        let mut ci = 0usize;
        let mut sd = f64::INFINITY;
        for i in 0..ranges.len() - 1 {
            let d = ranges[i + 1].centroid - ranges[i].centroid;
            if d < sd {
                sd = d;
                ci = i;
            }
        }
        let b = ranges.remove(ci + 1);
        let a = ranges.remove(ci);
        ranges.insert(ci, merge(a, b));
    }
    ranges.sort_by(|a, b| a.min.partial_cmp(&b.min).unwrap_or(std::cmp::Ordering::Equal));
    let mut i = 0usize;
    while i + 1 < ranges.len() {
        if ranges[i].min == ranges[i + 1].min {
            let b = ranges.remove(i + 1);
            let a = ranges.remove(i);
            ranges.insert(i, merge(a, b));
        } else {
            i += 1;
        }
    }
    for i in 1..ranges.len() {
        if ranges[i].min < ranges[i - 1].max {
            let mid = (ranges[i - 1].max + ranges[i].min) / 2.0;
            ranges[i].min = mid;
            ranges[i - 1].max = mid;
        }
    }
    let sub = sub_aggs(def);
    let mut out = vec![];
    for r in &ranges {
        let mut bm = Map::new();
        bm.insert("min".to_string(), json_num(r.min));
        bm.insert("key".to_string(), json_num(r.centroid));
        bm.insert("max".to_string(), json_num(r.max));
        bm.insert("doc_count".to_string(), json!(r.docs.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = r.docs.iter().map(|&di| docs[di]).collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    Ok(json!({"buckets": out}))
}

fn histogram_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let sub = sub_aggs(def);
    let field = map_str(t, "field");
    let ft = field_kind(idx, &field, prefix);
    if is_mapped_field(idx, &field, prefix) && !is_numeric_type(&ft) && ft != "date" && ft != "date_nanos"
    {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!(
                "Field [{}] of type [{}] is not supported for aggregation [histogram]",
                field, ft
            ),
            400,
        ));
    }
    let interval = match t.get("interval") {
        Some(v) => to_f64(v),
        None => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                "interval must be positive, got: 0.0",
                400,
            ))
        }
    };
    let interval = match interval {
        Some(f) if f > 0.0 => f,
        _ => {
            let msg = format!("[interval] must be >0 for histogram aggregation [{}]", name);
            let mut e = ApiError::new("illegal_argument_exception", &msg, 400);
            e.caused_by = Some(Box::new(ApiError::new("illegal_argument_exception", &msg, 400)));
            return Err(e);
        }
    };
    let off = t.get("offset").and_then(to_f64).unwrap_or(0.0);
    let missing = t.get("missing").and_then(to_f64);
    let min_doc_count = t
        .get("min_doc_count")
        .and_then(to_f64)
        .map(|f| f as usize)
        .unwrap_or(0);
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut counts: HashMap<i64, Vec<&Value>> = HashMap::new();
    for d in docs {
        let mut vals: Vec<f64> = vec![];
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some(x) = to_f64(&v) {
                vals.push(x);
            }
        }
        if vals.is_empty() {
            if let Some(m) = missing {
                vals.push(m);
            }
        }
        for x in vals {
            let ki = ((x - off) / interval + 1e-9).floor() as i64;
            counts.entry(ki).or_default().push(d);
        }
    }
    let mut start = counts.keys().min().copied();
    let mut end = counts.keys().max().copied();
    if let Some(eb) = t.get("extended_bounds").and_then(|v| v.as_object()) {
        let bmin = eb.get("min").and_then(to_f64);
        let bmax = eb.get("max").and_then(to_f64);
        if let (Some(mn), Some(mx)) = (bmin, bmax) {
            if mn > mx {
                return Err(ApiError::bad(&format!(
                    "max bound [{}] must be greater than min bound [{}]",
                    fmt_bound(mx),
                    fmt_bound(mn)
                )));
            }
        }
        if let Some(mn) = bmin {
            let k = ((mn - off) / interval + 1e-9).floor() as i64;
            start = Some(start.map_or(k, |s| s.min(k)));
        }
        if let Some(mx) = bmax {
            let k = ((mx - off) / interval + 1e-9).floor() as i64;
            end = Some(end.map_or(k, |e| e.max(k)));
        }
    }
    let mut hard_min: Option<f64> = None;
    let mut hard_max: Option<f64> = None;
    if let Some(hb) = t.get("hard_bounds").and_then(|v| v.as_object()) {
        hard_min = hb.get("min").and_then(to_f64);
        hard_max = hb.get("max").and_then(to_f64);
    }
    let (start, end) = match (start, end) {
        (Some(s), Some(e)) => (s, e),
        _ => return Ok(json!({"buckets": []})),
    };
    let mut buckets: Vec<(i64, f64, Map<String, Value>)> = vec![];
    let mut ki = start;
    while ki <= end {
        let key = ki as f64 * interval + off;
        if hard_min.map(|h| key < h).unwrap_or(false) {
            ki += 1;
            continue;
        }
        if hard_max.map(|h| key > h).unwrap_or(false) {
            break;
        }
        let bucket_docs = counts.get(&ki);
        let n = bucket_docs.map(|v| v.len()).unwrap_or(0);
        if n > 0 || min_doc_count == 0 {
            let mut bm = Map::new();
            bm.insert("key".to_string(), json_num(key));
            bm.insert("doc_count".to_string(), json!(n));
            if let Some(sub) = &sub {
                let empty: Vec<&Value> = Vec::new();
                let bd = bucket_docs.unwrap_or(&empty);
                if let Value::Object(m) = eval_aggs_typed(sub, bd, idx, prefix, typed)? {
                    for (sk, sv) in m {
                        bm.insert(sk, sv);
                    }
                }
            }
            buckets.push((ki, key, bm));
        }
        ki += 1;
    }
    let (order_key, desc) = order_of(t);
    let ocmp = |a: &(i64, f64, Map<String, Value>), b: &(i64, f64, Map<String, Value>)| {
        let va = match order_key.as_str() {
            "_key" => Value::Number(serde_json::Number::from_f64(a.1).unwrap_or(0.into())),
            "_count" => json!(a.2.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0)),
            other => sub_order_value(&a.2, other),
        };
        let vb = match order_key.as_str() {
            "_key" => Value::Number(serde_json::Number::from_f64(b.1).unwrap_or(0.into())),
            "_count" => json!(b.2.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0)),
            other => sub_order_value(&b.2, other),
        };
        let mut c = cmp_vals_asc(&va, &vb);
        if desc {
            c = c.reverse();
        }
        if c == Ordering::Equal {
            c = cmp_vals_asc(
                &Value::Number(serde_json::Number::from_f64(a.1).unwrap_or(0.into())),
                &Value::Number(serde_json::Number::from_f64(b.1).unwrap_or(0.into())),
            );
        }
        c
    };
    buckets.sort_by(ocmp);
    let mut out: Vec<Value> = buckets
        .into_iter()
        .map(|(_ki, _kf, b)| Value::Object(b))
        .collect();
    finish_bucket_pipelines(def, &mut out, "histogram", idx, prefix, typed)?;
    if keyed {
        let mut obj = Map::new();
        for b in &out {
            let k = b.get("key").map(fmt_key).unwrap_or_default();
            obj.insert(k, b.clone());
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

fn bound_ms(v: &Value, format: &str) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f as i64),
        Value::String(s) => {
            if !format.is_empty() {
                if let Some(ms) = parse_with_format(format, s) {
                    return Some(ms);
                }
            }
            if let Some(ms) = parse_date_math(s) {
                return Some(ms);
            }
            parse_date_flex(s)
        }
        _ => None,
    }
}

fn date_histogram(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let sub = sub_aggs(def);
    let field = map_str(t, "field");
    let ft = field_kind(idx, &field, prefix);
    let mapped = is_mapped_field(idx, &field, prefix);
    let numeric_mode = mapped && is_numeric_type(&ft);
    if mapped && !numeric_mode && ft != "date" && ft != "date_nanos" {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!(
                "Field [{}] of type [{}] is not supported for aggregation [date_histogram]",
                field, ft
            ),
            400,
        ));
    }
    let has_fixed = t.contains_key("fixed_interval");
    let has_cal = t.contains_key("calendar_interval");
    let mut fixed: Option<i64> = None;
    let mut cal: Option<&'static str> = None;
    if has_fixed && has_cal {
        return Err(err_cause(
            "Cannot use [fixed_interval] with [calendar_interval] configuration option.",
        ));
    }
    if has_fixed {
        let iv = t
            .get("fixed_interval")
            .map(|v| value_to_string(v).unwrap_or_default())
            .unwrap_or_default();
        match parse_fixed_interval(&iv) {
            Some(f) => fixed = Some(f),
            None => {
                return Err(err_cause(&format!(
                    "failed to parse setting [date_histogram.fixedInterval] with value [{}] as a time value: unit is missing or unrecognized",
                    iv
                )))
            }
        }
    } else if has_cal {
        let iv = t
            .get("calendar_interval")
            .map(|v| value_to_string(v).unwrap_or_default())
            .unwrap_or_default();
        match parse_calendar_interval(&iv) {
            Some(c) => cal = Some(c),
            None => {
                return Err(err_cause(&format!(
                    "The supplied interval [{}] could not be parsed as a calendar interval.",
                    iv
                )))
            }
        }
    } else if let Some(v) = t.get("interval") {
        let iv = value_to_string(v).unwrap_or_default();
        if let Some(c) = parse_calendar_interval(&iv) {
            cal = Some(c);
        } else if let Some(f) = parse_fixed_interval(&iv) {
            fixed = Some(f);
        } else {
            return Err(err_cause(&format!(
                "The supplied interval [{}] could not be parsed as a calendar interval.",
                iv
            )));
        }
    } else {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Invalid interval specified, must be non-null and non-empty",
            400,
        ));
    }
    let format = map_str(t, "format");
    let tz = t
        .get("time_zone")
        .map(|v| value_to_string(v).unwrap_or_default())
        .unwrap_or_default();
    if !tz.is_empty() {
        if let Err(msg) = zone_offset(&tz, 0) {
            let mut e = ApiError::new("illegal_argument_exception", &msg, 400);
            e.caused_by = Some(Box::new(ApiError::new("zone_rules_exception", &msg, 400)));
            return Err(e);
        }
    }
    let off_param = t
        .get("offset")
        .map(|v| parse_offset_duration(&value_to_string(v).unwrap_or_default()))
        .unwrap_or(0);
    let mut missing_ms: Option<i64> = None;
    if let Some(mv) = t.get("missing") {
        let s = value_to_string(mv).unwrap_or_default();
        let parsed = if !format.is_empty() {
            parse_with_format(&format, &s)
        } else {
            parse_date_flex(&s)
        }
        .or_else(|| to_f64(mv).map(|f| f as i64));
        match parsed {
            Some(ms) => missing_ms = Some(ms),
            None => {
                return Err(ApiError::new(
                    "parse_exception",
                    &format!(
                        "failed to parse date field [{}] with format [{}]",
                        s, format
                    ),
                    400,
                ))
            }
        }
    }
    let bstart = |ms: i64| -> i64 {
        if tz.is_empty() {
            let s = ms - off_param;
            date_bucket_start(s, fixed, cal) + off_param
        } else {
            tz_bucket_start(ms - off_param, fixed, cal, &tz) + off_param
        }
    };
    let next_key = |k: i64| -> i64 {
        if tz.is_empty() {
            if let Some(step) = fixed {
                return k + step;
            }
            let base = k - off_param;
            return match cal.unwrap_or("day") {
                "year" => {
                    let (y, _m, _d) = civil_from_days(base.div_euclid(86_400_000));
                    month_start_ms(y + 1, 1) + off_param
                }
                "quarter" => add_months_ms(base, 3) + off_param,
                "month" => add_months_ms(base, 1) + off_param,
                "week" => k + 7 * 86_400_000,
                "hour" => k + 3_600_000,
                "minute" => k + 60_000,
                "second" => k + 1000,
                _ => k + 86_400_000,
            };
        }
        let win: i64 = match (fixed, cal) {
            (Some(f), _) => (f * 2).max(7_200_000),
            (_, Some("minute")) | (_, Some("hour")) => 7_200_000,
            (_, Some("day")) => 50 * 3_600_000,
            (_, Some("week")) => 9 * 86_400_000,
            (_, Some("month")) => 32 * 86_400_000,
            (_, Some("quarter")) => 93 * 86_400_000,
            (_, Some("year")) => 367 * 86_400_000,
            _ => 50 * 3_600_000,
        };
        let mut hi = k + win;
        let mut guard = 0;
        while bstart(hi) <= k && guard < 10 {
            hi += win;
            guard += 1;
        }
        let mut lo = k + 1;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if bstart(mid) > k {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        bstart(lo)
    };
    let key_str = |k: i64| -> String {
        if tz.is_empty() {
            if format.is_empty() {
                format_epoch_ms(k)
            } else {
                dh_format(&format, k)
            }
        } else {
            let off = zone_offset(&tz, k).unwrap_or(0);
            let local = k + off;
            if format.is_empty() {
                let s = format_epoch_ms(local);
                let base = s.trim_end_matches('Z');
                format!("{}{}", base, fmt_zone_offset(off))
            } else {
                dh_format(&format, local)
            }
        }
    };
    let mut counts: HashMap<i64, Vec<&Value>> = HashMap::new();
    for d in docs {
        let mut vals: Vec<i64> = vec![];
        for v in doc_field_values(idx, d, &field, prefix) {
            let ms = if numeric_mode {
                to_f64(&v).map(|f| f as i64)
            } else {
                date_ms_of(&v)
            };
            if let Some(ms) = ms {
                vals.push(ms);
            }
        }
        if vals.is_empty() {
            if let Some(m) = missing_ms {
                vals.push(m);
            }
        }
        for ms in vals {
            let k = bstart(ms);
            counts.entry(k).or_default().push(d);
        }
    }
    let mut start = counts.keys().min().copied();
    let mut end = counts.keys().max().copied();
    if let Some(eb) = t.get("extended_bounds").and_then(|v| v.as_object()) {
        if let Some(mn) = eb.get("min").and_then(|v| bound_ms(v, &format)) {
            let b = bstart(mn);
            start = Some(start.map_or(b, |s| s.min(b)));
        }
        if let Some(mx) = eb.get("max").and_then(|v| bound_ms(v, &format)) {
            let b = bstart(mx);
            end = Some(end.map_or(b, |e| e.max(b)));
        }
    }
    let mut hard_min: Option<i64> = None;
    let mut hard_max: Option<i64> = None;
    if let Some(hb) = t.get("hard_bounds").and_then(|v| v.as_object()) {
        let hb_ms = |v: &Value| -> Option<i64> {
            if !format.is_empty() {
                parse_with_format(&format, &value_to_string(v).unwrap_or_default())
                    .or_else(|| to_f64(v).map(|f| f as i64))
            } else {
                bound_ms(v, &format)
            }
        };
        if let Some(mn) = hb.get("min") {
            hard_min = Some(hb_ms(mn).ok_or_else(|| {
                ApiError::new(
                    "parse_exception",
                    &format!(
                        "failed to parse date field [{}] with format [{}]",
                        value_to_string(mn).unwrap_or_default(),
                        format
                    ),
                    400,
                )
            })?);
        }
        if let Some(mx) = hb.get("max") {
            hard_max = Some(hb_ms(mx).ok_or_else(|| {
                ApiError::new(
                    "parse_exception",
                    &format!(
                        "failed to parse date field [{}] with format [{}]",
                        value_to_string(mx).unwrap_or_default(),
                        format
                    ),
                    400,
                )
            })?);
        }
    }
    let (start, end) = match (start, end) {
        (Some(s), Some(e)) => (s, e),
        _ => return Ok(json!({"buckets": []})),
    };
    let min_doc_count = t
        .get("min_doc_count")
        .and_then(to_f64)
        .map(|f| f as usize)
        .unwrap_or(0);
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let (order_key, desc) = order_of(t);
    let mut buckets: Vec<(i64, Map<String, Value>)> = vec![];
    let mut emit = |k: i64,
                    n: usize,
                    bucket_docs: Option<&Vec<&Value>>,
                    buckets: &mut Vec<(i64, Map<String, Value>)>|
     -> Result<(), ApiError> {
        let mut bm = Map::new();
        if !numeric_mode {
            bm.insert("key_as_string".to_string(), json!(key_str(k)));
        }
        bm.insert("key".to_string(), json!(k));
        bm.insert("doc_count".to_string(), json!(n));
        if let Some(sub) = &sub {
            let empty: Vec<&Value> = Vec::new();
            let bd = bucket_docs.unwrap_or(&empty);
            if let Value::Object(m) = eval_aggs_typed(sub, bd, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        buckets.push((k, bm));
        Ok(())
    };
    if min_doc_count > 0 {
        let mut keys: Vec<i64> = counts.keys().copied().collect();
        keys.sort();
        for k in keys {
            if k < start || k > end {
                continue;
            }
            if hard_min.map(|h| k < h).unwrap_or(false) {
                continue;
            }
            if hard_max.map(|h| k >= h).unwrap_or(false) {
                continue;
            }
            let bucket_docs = counts.get(&k);
            let n = bucket_docs.map(|v| v.len()).unwrap_or(0);
            if n >= min_doc_count && n > 0 {
                emit(k, n, bucket_docs, &mut buckets)?;
            }
        }
    } else {
        let mut k = start;
        let mut guard = 0;
        while k <= end {
            guard += 1;
            if guard > 200000 {
                break;
            }
            if hard_min.map(|h| k < h).unwrap_or(false) {
                k = next_key(k);
                continue;
            }
            if hard_max.map(|h| k >= h).unwrap_or(false) {
                break;
            }
            let bucket_docs = counts.get(&k);
            let n = bucket_docs.map(|v| v.len()).unwrap_or(0);
            emit(k, n, bucket_docs, &mut buckets)?;
            k = next_key(k);
        }
    }
    let ocmp = |a: &(i64, Map<String, Value>), b: &(i64, Map<String, Value>)| {
        let va = match order_key.as_str() {
            "_key" => json!(a.0),
            "_count" => json!(a.1.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0)),
            other => sub_order_value(&a.1, other),
        };
        let vb = match order_key.as_str() {
            "_key" => json!(b.0),
            "_count" => json!(b.1.get("doc_count").and_then(|v| v.as_u64()).unwrap_or(0)),
            other => sub_order_value(&b.1, other),
        };
        let mut c = cmp_vals_asc(&va, &vb);
        if desc {
            c = c.reverse();
        }
        if c == Ordering::Equal {
            c = a.0.cmp(&b.0);
        }
        c
    };
    buckets.sort_by(ocmp);
    let mut out: Vec<Value> = buckets
        .into_iter()
        .map(|(_k, b)| Value::Object(b))
        .collect();
    finish_bucket_pipelines(def, &mut out, "date_histogram", idx, prefix, typed)?;
    if keyed {
        let mut obj = Map::new();
        for b in &out {
            let ks = b
                .get("key_as_string")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| b.get("key").map(fmt_key).unwrap_or_default());
            obj.insert(ks, b.clone());
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

#[derive(Clone, Copy)]
enum AutoUnit {
    Fixed(i64, &'static str),
    Months(i64, &'static str),
    Years(i64, &'static str),
}

const AUTO_UNITS: &[AutoUnit] = &[
    AutoUnit::Fixed(1, "1s"),
    AutoUnit::Fixed(10, "10s"),
    AutoUnit::Fixed(30, "30s"),
    AutoUnit::Fixed(60, "1m"),
    AutoUnit::Fixed(300, "5m"),
    AutoUnit::Fixed(600, "10m"),
    AutoUnit::Fixed(1800, "30m"),
    AutoUnit::Fixed(3600, "1h"),
    AutoUnit::Fixed(10800, "3h"),
    AutoUnit::Fixed(43200, "12h"),
    AutoUnit::Fixed(86400, "1d"),
    AutoUnit::Fixed(604800, "7d"),
    AutoUnit::Fixed(2592000, "30d"),
    AutoUnit::Months(3, "3M"),
    AutoUnit::Years(1, "1y"),
    AutoUnit::Years(10, "10y"),
];

fn auto_key_block(
    sub: &Option<Map<String, Value>>,
    idx: &Index,
    prefix: &str,
    typed: bool,
    k: i64,
    cnt: usize,
    format: &str,
    bd: &[&Value],
) -> Result<Value, ApiError> {
    let mut bm = Map::new();
    let ks = if format.is_empty() {
        format_epoch_ms(k)
    } else {
        apply_java_date_format(format, k).unwrap_or_else(|| format_epoch_ms(k))
    };
    bm.insert("key_as_string".to_string(), json!(ks));
    bm.insert("key".to_string(), json!(k));
    bm.insert("doc_count".to_string(), json!(cnt));
    if let Some(sub) = sub {
        if let Value::Object(m) = eval_aggs_typed(sub, bd, idx, prefix, typed)? {
            for (sk, sv) in m {
                bm.insert(sk, sv);
            }
        }
    }
    Ok(Value::Object(bm))
}

fn auto_date_histogram(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let target = t
        .get("buckets")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(10)
        .max(1);
    let format = map_str(t, "format");
    let mut values: Vec<i64> = vec![];
    let mut pairs: Vec<(i64, &Value)> = vec![];
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some(ms) = date_ms_of(&v) {
                values.push(ms);
                pairs.push((ms, d));
            }
        }
    }
    if values.is_empty() {
        return Ok(json!({"buckets": [], "interval": "1s"}));
    }
    let vmin = *values.iter().min().unwrap();
    let vmax = *values.iter().max().unwrap();
    enum Pick {
        Fixed(i64, &'static str, i64, i64),
        Months(&'static str, i64, i64),
        Years(&'static str, i64, i64),
    }
    let mut chosen: Option<Pick> = None;
    for unit in AUTO_UNITS {
        match *unit {
            AutoUnit::Fixed(secs, label) => {
                let step = secs * 1000;
                let start = vmin.div_euclid(step) * step;
                let end = vmax.div_euclid(step) * step;
                let count = (end - start) / step + 1;
                if count <= target {
                    chosen = Some(Pick::Fixed(step, label, start, end));
                    break;
                }
            }
            AutoUnit::Months(n, label) => {
                let mi = month_index(vmin);
                let me = month_index(vmax);
                let count = (me - mi) / n + 1;
                if count <= target {
                    chosen = Some(Pick::Months(label, mi, me));
                    break;
                }
            }
            AutoUnit::Years(n, label) => {
                let (y1, _m, _d) = civil_from_days(vmin.div_euclid(86_400_000));
                let (y2, _m, _d) = civil_from_days(vmax.div_euclid(86_400_000));
                let count = (y2 / n - y1 / n) + 1;
                if count <= target {
                    chosen = Some(Pick::Years(label, y1, y2));
                    break;
                }
            }
        }
    }
    let pick = match chosen {
        Some(c) => c,
        None => Pick::Years("10y", 0, 0),
    };
    let mut out = vec![];
    match pick {
        Pick::Fixed(step, label, a, b) => {
            let mut k = a;
            while k <= b {
                let bd: Vec<&Value> = pairs
                    .iter()
                    .filter(|(ms, _)| ms.div_euclid(step) * step == k)
                    .map(|(_, d)| *d)
                    .collect();
                let cnt = bd.len();
                out.push(auto_key_block(&sub, idx, prefix, typed, k, cnt, &format, &bd)?);
                k += step;
            }
            Ok(json!({"buckets": out, "interval": label}))
        }
        Pick::Months(label, mi, me) => {
            let n: i64 = label[..label.len() - 1].parse().unwrap_or(1);
            let mut k = month_start_ms(mi.div_euclid(12), mi.rem_euclid(12) + 1);
            let end = month_start_ms(me.div_euclid(12), me.rem_euclid(12) + 1);
            while k <= end {
                let next = add_months_ms(k, n);
                let cnt = values.iter().filter(|&&ms| ms >= k && ms < next).count();
                out.push(auto_key_block(&sub, idx, prefix, typed, k, cnt, &format, &[])?);
                k = next;
            }
            Ok(json!({"buckets": out, "interval": label}))
        }
        Pick::Years(label, y1, y2) => {
            let n: i64 = label[..label.len() - 1].parse().unwrap_or(1);
            let mut y = y1;
            while y <= y2 {
                let k = month_start_ms(y, 1);
                let next = month_start_ms(y + n, 1);
                let cnt = values.iter().filter(|&&ms| ms >= k && ms < next).count();
                out.push(auto_key_block(&sub, idx, prefix, typed, k, cnt, &format, &[])?);
                y += n;
            }
            Ok(json!({"buckets": out, "interval": label}))
        }
    }
}

fn range_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let ranges = match t.get("ranges").and_then(|r| r.as_array()) {
        Some(r) if !r.is_empty() => r.clone(),
        _ => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("No [ranges] specified for the [{}] aggregation", name),
                400,
            ))
        }
    };
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let missing = t.get("missing").and_then(to_f64);
    let format = map_str(t, "format");
    struct RBucket {
        from: Option<f64>,
        to: Option<f64>,
        key: Option<String>,
        docs: Vec<Value>,
    }
    let mut parsed: Vec<RBucket> = vec![];
    for r in ranges {
        let rm = match r.as_object() {
            Some(m) => m,
            None => continue,
        };
        parsed.push(RBucket {
            from: rm.get("from").and_then(to_f64),
            to: rm.get("to").and_then(to_f64),
            key: rm.get("key").and_then(value_to_string),
            docs: vec![],
        });
    }
    for d in docs {
        let mut vals: Vec<f64> = doc_field_values(idx, d, &field, prefix)
            .iter()
            .filter_map(to_f64)
            .collect();
        if vals.is_empty() {
            if let Some(m) = missing {
                vals.push(m);
            }
        }
        for b in parsed.iter_mut() {
            for n in &vals {
                let ok_from = b.from.map(|f| *n >= f).unwrap_or(true);
                let ok_to = b.to.map(|tt| *n < tt).unwrap_or(true);
                if ok_from && ok_to {
                    b.docs.push(Value::clone(d));
                    break;
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..parsed.len()).collect();
    order.sort_by(|&i, &j| {
        let a = &parsed[i];
        let b = &parsed[j];
        let fa = a.from.unwrap_or(f64::NEG_INFINITY);
        let fb = b.from.unwrap_or(f64::NEG_INFINITY);
        fa.partial_cmp(&fb).unwrap_or(Ordering::Equal).then_with(|| {
            let ta = a.to.unwrap_or(f64::INFINITY);
            let tb = b.to.unwrap_or(f64::INFINITY);
            ta.partial_cmp(&tb).unwrap_or(Ordering::Equal)
        })
    });
    let num_fmt = |f: f64| -> String {
        if format.ends_with(".0") || format == "#.0" {
            format!("{:.1}", f)
        } else {
            fmt_bound(f)
        }
    };
    let mut out = vec![];
    for i in order {
        let b = &parsed[i];
        let key = b.key.clone().unwrap_or_else(|| {
            let f = b.from.map(fmt_bound).unwrap_or_else(|| "*".to_string());
            let tt = b.to.map(fmt_bound).unwrap_or_else(|| "*".to_string());
            format!("{}-{}", f, tt)
        });
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(key));
        if let Some(f) = b.from {
            bm.insert("from".to_string(), json_num(f));
            if !format.is_empty() {
                bm.insert("from_as_string".to_string(), json!(num_fmt(f)));
            }
        }
        if let Some(tt) = b.to {
            bm.insert("to".to_string(), json_num(tt));
            if !format.is_empty() {
                bm.insert("to_as_string".to_string(), json!(num_fmt(tt)));
            }
        }
        bm.insert("doc_count".to_string(), json!(b.docs.len()));
        if let Some(sub) = sub_aggs(def) {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(&sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    if keyed {
        let mut obj = Map::new();
        for mut b in out {
            let k = b.get("key").and_then(value_to_string).unwrap_or_default();
            if let Value::Object(ref mut m) = b {
                while m.shift_remove("key").is_some() {}
            }
            obj.insert(k, b);
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

fn tz_offset_ms(tz: &str) -> Option<i64> {
    if tz.is_empty() {
        return Some(0);
    }
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let body = tz.trim_start_matches(['+', '-']);
    let parts: Vec<&str> = body.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let h: i64 = parts[0].parse().ok()?;
    let m: i64 = if parts.len() > 1 { parts[1].parse().ok()? } else { 0 };
    Some(sign * (h * 3_600_000 + m * 60_000))
}

fn timed_string(epoch_ms: i64, tz: &str, format: &str) -> String {
    if !format.is_empty() {
        return apply_java_date_format(format, epoch_ms).unwrap_or_else(|| format_epoch_ms(epoch_ms));
    }
    let s = format_epoch_ms(epoch_ms);
    if tz.is_empty() {
        return s;
    }
    let t = s.trim_end_matches('Z');
    format!("{}{}", t, tz)
}

fn parse_with_format_or_math(s: &str, format: &str) -> Option<i64> {
    if !format.is_empty() {
        if let Some(ms) = parse_with_format(format, s) {
            return Some(ms);
        }
    }
    if let Some(ms) = parse_date_math(s) {
        return Some(ms);
    }
    parse_date_flex(s)
}

fn date_range_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let format = map_str(t, "format");
    let tz = map_str(t, "time_zone");
    let off = tz_offset_ms(&tz).unwrap_or(0);
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let ranges = t
        .get("ranges")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    struct DBucket {
        from: Option<i64>,
        to: Option<i64>,
        key: Option<String>,
        docs: Vec<Value>,
    }
    let mut parsed: Vec<DBucket> = vec![];
    for r in &ranges {
        let rm = match r.as_object() {
            Some(m) => m,
            None => continue,
        };
        let parse_bound = |v: Option<&Value>| -> Option<i64> {
            match v {
                Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
                Some(Value::String(s)) => parse_with_format_or_math(s, &format).map(|ms| ms - off),
                _ => None,
            }
        };
        parsed.push(DBucket {
            from: parse_bound(rm.get("from")),
            to: parse_bound(rm.get("to")),
            key: rm.get("key").and_then(value_to_string),
            docs: vec![],
        });
    }
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some(ms) = date_ms_of(&v) {
                for b in parsed.iter_mut() {
                    let ok_from = b.from.map(|f| ms >= f).unwrap_or(true);
                    let ok_to = b.to.map(|tt| ms < tt).unwrap_or(true);
                    if ok_from && ok_to {
                        b.docs.push(Value::clone(d));
                        break;
                    }
                }
            }
        }
    }
    let mut out = vec![];
    for b in parsed.iter() {
        let from_s = b.from.map(|ms| timed_string(ms + off, &tz, &format));
        let to_s = b.to.map(|ms| timed_string(ms + off, &tz, &format));
        let key = b.key.clone().unwrap_or_else(|| {
            format!(
                "{}-{}",
                from_s.clone().unwrap_or_else(|| "*".to_string()),
                to_s.clone().unwrap_or_else(|| "*".to_string())
            )
        });
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(key));
        if let Some(f) = b.from {
            bm.insert("from".to_string(), json!(f as f64));
            bm.insert("from_as_string".to_string(), json!(from_s.unwrap_or_default()));
        }
        if let Some(tt) = b.to {
            bm.insert("to".to_string(), json!(tt as f64));
            bm.insert("to_as_string".to_string(), json!(to_s.unwrap_or_default()));
        }
        bm.insert("doc_count".to_string(), json!(b.docs.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    if keyed {
        let mut obj = Map::new();
        for mut b in out {
            let k = b.get("key").and_then(value_to_string).unwrap_or_default();
            if let Value::Object(ref mut m) = b {
                while m.shift_remove("key").is_some() {}
            }
            obj.insert(k, b);
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

fn u128_to_ip(v: u128, v6: bool) -> String {
    if v6 {
        let mut gs = [0u16; 8];
        for i in 0..8 {
            gs[i] = ((v >> (16 * (7 - i))) & 0xffff) as u16;
        }
        let mut best_start = -1i32;
        let mut best_len = 0i32;
        let mut i = 0i32;
        while i < 8 {
            if gs[i as usize] == 0 {
                let start = i;
                while i < 8 && gs[i as usize] == 0 {
                    i += 1;
                }
                let len = i - start;
                if len > 1 && len > best_len {
                    best_start = start;
                    best_len = len;
                }
            } else {
                i += 1;
            }
        }
        let mut parts: Vec<String> = vec![];
        let mut j = 0usize;
        while j < 8 {
            if best_start >= 0 && j == best_start as usize {
                parts.push(String::new());
                if best_start + best_len >= 8 {
                    parts.push(String::new());
                }
                j += best_len as usize;
            } else {
                parts.push(format!("{:x}", gs[j]));
                j += 1;
            }
        }
        parts.join(":")
    } else {
        format!(
            "{}.{}.{}.{}",
            (v >> 24) & 0xff,
            (v >> 16) & 0xff,
            (v >> 8) & 0xff,
            v & 0xff
        )
    }
}

fn ip_range_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let ranges = t
        .get("ranges")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    struct IBucket {
        from: Option<u128>,
        to: Option<u128>,
        v6: bool,
        key: String,
        from_s: Option<String>,
        to_s: Option<String>,
        docs: Vec<Value>,
    }
    let mut parsed: Vec<IBucket> = vec![];
    for r in &ranges {
        let rm = match r.as_object() {
            Some(m) => m,
            None => continue,
        };
        if let Some(mask) = rm.get("mask").and_then(|v| v.as_str()) {
            let (ip_part, plen_s) = match mask.split_once('/') {
                Some(x) => x,
                None => continue,
            };
            let plen: u32 = plen_s.parse().unwrap_or(0);
            let (ipv, v6) = match ip_numeric(ip_part) {
                Some(x) => x,
                None => continue,
            };
            let bits = if v6 { 128 } else { 32 };
            if plen > bits {
                let msg = format!(
                    "Illegal prefix length [{}] in [{}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
                    plen, mask
                );
                return Err(
                    ApiError::new("illegal_argument_exception", &msg, 400)
                        .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
                );
            }
            let host_mask: u128 = if plen == 0 { 0 } else { (!0u128) << (bits - plen) };
            let net = ipv & host_mask;
            let size: u128 = if plen == 0 { u128::MAX } else { 1u128 << (bits - plen) };
            let endv = net.wrapping_add(size);
            parsed.push(IBucket {
                from: Some(net),
                to: Some(endv),
                v6,
                key: rm
                    .get("key")
                    .and_then(value_to_string)
                    .unwrap_or_else(|| mask.to_string()),
                from_s: Some(u128_to_ip(net, v6)),
                to_s: Some(u128_to_ip(endv, v6)),
                docs: vec![],
            });
        } else {
            let from = rm.get("from").and_then(|v| v.as_str()).and_then(ip_numeric);
            let to = rm.get("to").and_then(|v| v.as_str()).and_then(ip_numeric);
            let v6 = from.map(|f| f.1).or_else(|| to.map(|t| t.1)).unwrap_or(false);
            let key = rm.get("key").and_then(value_to_string).unwrap_or_else(|| {
                format!(
                    "{}-{}",
                    rm.get("from")
                        .and_then(value_to_string)
                        .unwrap_or_else(|| "*".to_string()),
                    rm.get("to")
                        .and_then(value_to_string)
                        .unwrap_or_else(|| "*".to_string())
                )
            });
            parsed.push(IBucket {
                from: from.map(|f| f.0),
                to: to.map(|t| t.0),
                v6,
                key,
                from_s: rm.get("from").and_then(value_to_string),
                to_s: rm.get("to").and_then(value_to_string),
                docs: vec![],
            });
        }
    }
    const V6_OFF: u128 = 1u128 << 32;
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            let raw = value_to_string(&v).unwrap_or_default();
            let ipv = match ip_numeric(&raw) {
                Some(x) => x,
                None => continue,
            };
            let num = if ipv.1 { ipv.0 + V6_OFF } else { ipv.0 };
            let mapped_num = if ipv.1 { ipv.0 } else { ipv.0 + V6_OFF };
            for b in parsed.iter_mut() {
                let (num_cmp, bf, bt) = if b.from.is_none() && b.to.is_some() && !b.v6 {
                    (
                        mapped_num,
                        b.from.map(|f| f + V6_OFF),
                        b.to.map(|tt| tt + V6_OFF),
                    )
                } else {
                    (
                        num,
                        b.from.map(|f| if b.v6 { f + V6_OFF } else { f }),
                        b.to.map(|tt| if b.v6 { tt + V6_OFF } else { tt }),
                    )
                };
                let ok_from = bf.map(|f| num_cmp >= f).unwrap_or(true);
                let ok_to = bt.map(|tt| num_cmp < tt).unwrap_or(true);
                if ok_from && ok_to {
                    b.docs.push(Value::clone(d));
                }
            }
        }
    }
    let mut order: Vec<usize> = (0..parsed.len()).collect();
    order.sort_by(|&i, &j| {
        let a = &parsed[i];
        let b = &parsed[j];
        let fa = a.from.unwrap_or(0);
        let fb = b.from.unwrap_or(0);
        fa.cmp(&fb).then_with(|| {
            let ta = a.to.unwrap_or(u128::MAX);
            let tb = b.to.unwrap_or(u128::MAX);
            ta.cmp(&tb)
        })
    });
    let mut out = vec![];
    for i in order {
        let b = &parsed[i];
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(b.key));
        if let Some(fs) = &b.from_s {
            bm.insert("from".to_string(), json!(fs));
        }
        if let Some(ts) = &b.to_s {
            bm.insert("to".to_string(), json!(ts));
        }
        bm.insert("doc_count".to_string(), json!(b.docs.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    if keyed {
        let mut obj = Map::new();
        for mut b in out {
            let k = b.get("key").and_then(value_to_string).unwrap_or_default();
            if let Value::Object(ref mut m) = b {
                while m.shift_remove("key").is_some() {}
            }
            obj.insert(k, b);
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

fn filters_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    if t.contains_key("keyed") {
        return Err(ApiError::new(
            "x_content_parse_exception",
            &format!("Unknown key for a VALUE_BOOLEAN in [{}]: [keyed].", name),
            400,
        ));
    }
    let filters = t.get("filters").cloned().unwrap_or(Value::Null);
    let other_key = t
        .get("other_bucket_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let other_enabled = other_key.is_some()
        || t.get("other_bucket").and_then(|v| v.as_bool()).unwrap_or(false);
    let ok = |fdocs: &[&Value]| -> Result<Value, ApiError> {
        let mut bm = Map::new();
        bm.insert("doc_count".to_string(), json!(fdocs.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, fdocs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        Ok(Value::Object(bm))
    };
    match filters {
        Value::Object(fo) => {
            if fo.is_empty() {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    "[filters] cannot be empty.",
                    400,
                ));
            }
            let mut names: Vec<&String> = fo.keys().collect();
            names.sort();
            let mut result = Map::new();
            let mut matched: HashSet<usize> = HashSet::new();
            for nm in names {
                let q = match fo[nm].as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let idxs: Vec<usize> = (0..docs.len())
                    .filter(|&i| doc_matches(idx, &docs[i], q))
                    .collect();
                for i in &idxs {
                    matched.insert(*i);
                }
                let fdocs: Vec<&Value> = idxs.iter().map(|&i| docs[i]).collect();
                result.insert(nm.clone(), ok(&fdocs)?);
            }
            if other_enabled {
                let rest: Vec<&Value> = (0..docs.len())
                    .filter(|i| !matched.contains(i))
                    .map(|i| docs[i])
                    .collect();
                let k = other_key.unwrap_or_else(|| "_other_".to_string());
                result.insert(k, ok(&rest)?);
            }
            Ok(json!({"buckets": result}))
        }
        Value::Array(fa) => {
            if fa.is_empty() {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    "[filters] cannot be empty.",
                    400,
                ));
            }
            let mut out = vec![];
            let mut matched: HashSet<usize> = HashSet::new();
            for f in fa {
                let q = match f.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let idxs: Vec<usize> = (0..docs.len())
                    .filter(|&i| doc_matches(idx, &docs[i], q))
                    .collect();
                for i in &idxs {
                    matched.insert(*i);
                }
                let fdocs: Vec<&Value> = idxs.iter().map(|&i| docs[i]).collect();
                out.push(ok(&fdocs)?);
            }
            if other_enabled {
                let rest: Vec<&Value> = (0..docs.len())
                    .filter(|i| !matched.contains(i))
                    .map(|i| docs[i])
                    .collect();
                out.push(ok(&rest)?);
            }
            Ok(json!({"buckets": out}))
        }
        _ => Err(ApiError::new(
            "illegal_argument_exception",
            "[filters] cannot be empty.",
            400,
        )),
    }
}

fn nested_elements(doc: &Value, path: &str) -> Vec<Value> {
    let mut elems: Vec<Value> = vec![];
    for part in path.split('.') {
        if elems.is_empty() {
            if let Some(Value::Array(a)) = doc.get(part) {
                elems.extend(a.iter().cloned());
            }
        } else {
            let mut next = vec![];
            for e in &elems {
                if let Some(Value::Array(a)) = e.get(part) {
                    next.extend(a.iter().cloned());
                }
            }
            elems = next;
        }
    }
    elems
}

fn nested_agg(
    path: &str,
    docs: &[&Value],
    idx: &Index,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let all_paths = NESTED_PATHS.with(|np| np.borrow().clone());
    let mut wrappers: Vec<Value> = vec![];
    for d in docs {
        let (src, p0, base_anc, root) =
            if let Some(n) = d.get("__nest").and_then(|v| v.as_object()) {
                (
                    n.get("elem").cloned().unwrap_or(Value::Null),
                    n.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    n.get("anc").cloned().unwrap_or_else(|| json!({})),
                    n.get("root").cloned().unwrap_or_else(|| Value::clone(d)),
                )
            } else {
                (Value::clone(d), String::new(), json!({}), Value::clone(d))
            };
        let (rest, abs_path) = if p0.is_empty() {
            (path.to_string(), path.to_string())
        } else if let Some(rest) = path.strip_prefix(&format!("{}.", p0)) {
            (rest.to_string(), path.to_string())
        } else if path == p0 {
            (String::new(), path.to_string())
        } else {
            (path.to_string(), path.to_string())
        };
        if !all_paths.contains(&abs_path) {
            continue;
        }
        let segs: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
        let mut anc = base_anc.clone();
        if !p0.is_empty() {
            if let Value::Object(ref mut m) = anc {
                m.insert(p0.clone(), Value::clone(&src));
            }
        }
        if segs.is_empty() {
            wrappers.push(make_wrapper(&abs_path, src, &root, anc));
        } else {
            walk_nested(&src, &segs, &p0, &anc, &root, &abs_path, &mut wrappers);
        }
    }
    let mut body = Map::new();
    body.insert("doc_count".to_string(), json!(wrappers.len()));
    if let Some(sub) = &sub {
        let frame = NestFrame {
            path: path.to_string(),
            elems: vec![],
            root_docs: vec![],
        };
        let refs: Vec<&Value> = wrappers.iter().collect();
        let evaluated = with_nest_frame(frame, || eval_aggs_typed(sub, &refs, idx, "", typed))?;
        if let Value::Object(m) = evaluated {
            for (sk, sv) in m {
                body.insert(sk, sv);
            }
        }
    }
    Ok(Value::Object(body))
}

fn make_wrapper(abs_path: &str, elem: Value, root: &Value, anc: Value) -> Value {
    json!({"__nest": {"path": abs_path, "elem": elem, "root": root, "anc": anc}})
}

fn walk_nested(
    node: &Value,
    segs: &[&str],
    base: &str,
    anc: &Value,
    root: &Value,
    abs_path: &str,
    out: &mut Vec<Value>,
) {
    if segs.is_empty() {
        out.push(make_wrapper(abs_path, Value::clone(node), root, anc.clone()));
        return;
    }
    let seg = segs[0];
    let values_of_node = values_of(agg_field_value(node, seg, ""));
    let new_base = if base.is_empty() {
        seg.to_string()
    } else {
        format!("{}.{}", base, seg)
    };
    for child in values_of_node {
        let mut a2 = anc.clone();
        if let Value::Object(ref mut m) = a2 {
            m.insert(new_base.clone(), child.clone());
        }
        walk_nested(&child, &segs[1..], &new_base, &a2, root, abs_path, out);
    }
}

#[derive(Clone, PartialEq, Debug)]
enum CompVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

fn comp_cmp(a: &CompVal, b: &CompVal, desc: bool) -> Ordering {
    let c = match (a, b) {
        (CompVal::Null, CompVal::Null) => Ordering::Equal,
        (CompVal::Null, _) => Ordering::Less,
        (_, CompVal::Null) => Ordering::Greater,
        (CompVal::Bool(x), CompVal::Bool(y)) => x.cmp(y),
        (CompVal::Bool(_), _) => Ordering::Less,
        (_, CompVal::Bool(_)) => Ordering::Greater,
        (CompVal::Num(x), CompVal::Num(y)) => {
            x.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (CompVal::Num(x), CompVal::Str(_)) => {
            x.partial_cmp(&0.0).unwrap_or(Ordering::Equal)
        }
        (CompVal::Str(_), CompVal::Num(y)) => {
            0.0f64.partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (CompVal::Str(x), CompVal::Str(y)) => x.cmp(y),
    };
    if desc {
        c.reverse()
    } else {
        c
    }
}

fn tz_bucket_start(ms: i64, fixed: Option<i64>, cal: Option<&str>, tz: &str) -> i64 {
    if tz.is_empty() {
        return date_bucket_start(ms, fixed, cal);
    }
    let off_t = zone_offset(tz, ms).unwrap_or(0);
    let l = ms + off_t;
    let lb = date_bucket_start(l, fixed, cal);
    let mut u = lb - off_t;
    for _ in 0..4 {
        let o = zone_offset(tz, u).unwrap_or(0);
        let nu = lb - o;
        if nu == u {
            break;
        }
        u = nu;
    }
    u
}

struct CompSource {
    name: String,
    kind: u8,
    field: String,
    script: Option<String>,
    interval: f64,
    fixed: Option<i64>,
    cal: Option<&'static str>,
    tz: String,
    format: String,
    missing_bucket: bool,
    desc: bool,
}

fn composite_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let sources = match t.get("sources").and_then(|v| v.as_array()) {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            return Err(ApiError::new(
                "illegal_argument_exception",
                "Composite [sources] cannot be null or empty",
                400,
            ))
        }
    };
    let size = t.get("size").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    let mut parsed: Vec<CompSource> = vec![];
    for s in &sources {
        let sm = match s.as_object() {
            Some(m) => m,
            None => continue,
        };
        for (nm, body) in sm {
            let bm = match body.as_object() {
                Some(m) => m,
                None => continue,
            };
            let inner = bm
                .get("terms")
                .or_else(|| bm.get("histogram"))
                .or_else(|| bm.get("date_histogram"))
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let missing_bucket = inner
                .get("missing_bucket")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let desc = inner
                .get("order")
                .and_then(|v| v.as_str())
                .map(|o| o == "desc")
                .unwrap_or(false);
            if let Some(tm) = bm.get("terms").and_then(|v| v.as_object()) {
                parsed.push(CompSource {
                    name: nm.clone(),
                    kind: 0,
                    field: map_str(tm, "field"),
                    script: agg_script_source(tm),
                    interval: 0.0,
                    fixed: None,
                    cal: None,
                    tz: String::new(),
                    format: String::new(),
                    missing_bucket,
                    desc,
                });
            } else if let Some(hm) = bm.get("histogram").and_then(|v| v.as_object()) {
                parsed.push(CompSource {
                    name: nm.clone(),
                    kind: 1,
                    field: map_str(hm, "field"),
                    script: None,
                    interval: hm.get("interval").and_then(to_f64).unwrap_or(1.0),
                    fixed: None,
                    cal: None,
                    tz: String::new(),
                    format: String::new(),
                    missing_bucket,
                    desc,
                });
            } else if let Some(dm) = bm.get("date_histogram").and_then(|v| v.as_object()) {
                let iv = dm
                    .get("calendar_interval")
                    .or_else(|| dm.get("interval"))
                    .and_then(value_to_string)
                    .unwrap_or_else(|| "day".to_string());
                let (fixed, cal) = date_step(&iv).unwrap_or((Some(86_400_000), None));
                parsed.push(CompSource {
                    name: nm.clone(),
                    kind: 2,
                    field: map_str(dm, "field"),
                    script: None,
                    interval: 0.0,
                    fixed,
                    cal,
                    tz: map_str(dm, "time_zone"),
                    format: map_str(dm, "format"),
                    missing_bucket,
                    desc,
                });
            }
        }
    }
    let script_params = agg_script_params(t);
    let mut buckets: HashMap<String, (Vec<CompVal>, Vec<Value>, Vec<Value>)> = HashMap::new();
    let mut keys_order: Vec<String> = vec![];
    for d in docs {
        let mut choices: Vec<Vec<(CompVal, Value)>> = vec![];
        let mut skip = false;
        for s in &parsed {
            let mut vals: Vec<(CompVal, Value)> = vec![];
            if let Some(src) = &s.script {
                let r = run_agg_script(idx, d, src, &script_params, None)?;
                if !r.is_null() {
                    let ks = key_string(&r);
                    vals.push((CompVal::Str(ks.clone()), Value::String(ks)));
                }
            } else if s.kind == 0 {
                for v in doc_field_values(idx, d, &s.field, prefix) {
                    let (k, _) = term_key_and_string(idx, &s.field, prefix, &v, "");
                    let (cv, kj) = match &k {
                        Value::Number(n) => (
                            CompVal::Num(n.as_f64().unwrap_or(0.0)),
                            Value::Number(n.clone()),
                        ),
                        Value::Bool(b) => (CompVal::Bool(*b), Value::Bool(*b)),
                        Value::String(st) => (CompVal::Str(st.clone()), Value::String(st.clone())),
                        other => {
                            let ks = key_string(other);
                            (CompVal::Str(ks.clone()), Value::String(ks))
                        }
                    };
                    vals.push((cv, kj));
                }
            } else if s.kind == 1 {
                for v in doc_field_values(idx, d, &s.field, prefix) {
                    if let Some(x) = to_f64(&v) {
                        let b = (x / s.interval).floor() * s.interval;
                        vals.push((CompVal::Num(b), json_num(b)));
                    }
                }
            } else {
                for v in doc_field_values(idx, d, &s.field, prefix) {
                    if let Some(ms) = date_ms_of(&v) {
                        let b = tz_bucket_start(ms, s.fixed, s.cal, &s.tz);
                        let kj = if s.format.is_empty() {
                            json!(b)
                        } else {
                            json!(dh_format(&s.format, b))
                        };
                        vals.push((CompVal::Num(b as f64), kj));
                    }
                }
            }
            if vals.is_empty() {
                if s.missing_bucket {
                    vals.push((CompVal::Null, Value::Null));
                } else {
                    skip = true;
                    break;
                }
            }
            choices.push(vals);
        }
        if skip || choices.len() != parsed.len() {
            continue;
        }
        let mut combos: Vec<Vec<(CompVal, Value)>> = vec![vec![]];
        for listed in &choices {
            let mut next: Vec<Vec<(CompVal, Value)>> = vec![];
            for combo in &combos {
                for ch in listed {
                    let mut c = combo.clone();
                    c.push(ch.clone());
                    next.push(c);
                }
            }
            combos = next;
        }
        for combo in combos {
            let tuple: Vec<CompVal> = combo.iter().map(|(c, _)| c.clone()).collect();
            let keyvals: Vec<Value> = combo.iter().map(|(_, k)| k.clone()).collect();
            let id = tuple
                .iter()
                .map(|v| match v {
                    CompVal::Null => "\u{0}n".to_string(),
                    CompVal::Bool(b) => format!("b{}", b),
                    CompVal::Num(f) => format!("n{:?}", f),
                    CompVal::Str(sv) => format!("s{}", sv),
                })
                .collect::<Vec<_>>()
                .join("\u{1}");
            let e = buckets.entry(id.clone()).or_insert_with(|| {
                keys_order.push(id.clone());
                (tuple.clone(), keyvals.clone(), vec![])
            });
            e.2.push(Value::clone(d));
        }
    }
    if let Some(after) = t.get("after").and_then(|v| v.as_object()) {
        for s in &parsed {
            if !after.contains_key(&s.name) {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Missing value for [after.{}]", s.name),
                    400,
                ));
            }
        }
        let mut after_vals: Vec<CompVal> = vec![];
        for s in &parsed {
            let av = after.get(&s.name).cloned().unwrap_or(Value::Null);
            let cv = match (&av, s.kind) {
                (Value::Null, _) => CompVal::Null,
                (Value::Bool(b), _) => CompVal::Bool(*b),
                (Value::Number(n), _) => CompVal::Num(n.as_f64().unwrap_or(0.0)),
                (Value::String(st), 2) => {
                    if s.format.is_empty() {
                        parse_date_flex(st)
                            .or_else(|| st.parse::<f64>().ok().map(|f| f as i64))
                            .map(|ms| CompVal::Num(ms as f64))
                            .unwrap_or(CompVal::Str(st.clone()))
                    } else {
                        parse_with_format(&s.format, st)
                            .or_else(|| st.parse::<f64>().ok().map(|f| f as i64))
                            .map(|ms| CompVal::Num(ms as f64))
                            .unwrap_or(CompVal::Str(st.clone()))
                    }
                }
                (Value::String(st), _) => CompVal::Str(st.clone()),
                _ => CompVal::Null,
            };
            after_vals.push(cv);
        }
        let keep: Vec<String> = keys_order
            .iter()
            .filter(|id| {
                let tuple = &buckets[*id].0;
                for (i, s) in parsed.iter().enumerate() {
                    let c = comp_cmp(&tuple[i], &after_vals[i], s.desc);
                    if c == Ordering::Greater {
                        return true;
                    }
                    if c == Ordering::Less {
                        return false;
                    }
                }
                false
            })
            .cloned()
            .collect();
        keys_order = keep;
    }
    let mut ids: Vec<String> = keys_order;
    ids.sort_by(|a, b| {
        let ta = &buckets[a].0;
        let tb = &buckets[b].0;
        for (i, s) in parsed.iter().enumerate() {
            let c = comp_cmp(&ta[i], &tb[i], s.desc);
            if c != Ordering::Equal {
                return c;
            }
        }
        Ordering::Equal
    });
    let mut out = vec![];
    for id in ids {
        let (_, keyvals, bdocs) = &buckets[&id];
        let mut km = Map::new();
        for (i, s) in parsed.iter().enumerate() {
            km.insert(s.name.clone(), keyvals[i].clone());
        }
        let mut bm = Map::new();
        bm.insert("key".to_string(), Value::Object(km));
        bm.insert("doc_count".to_string(), json!(bdocs.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = bdocs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
        if out.len() >= size {
            break;
        }
    }
    let mut res = Map::new();
    if let Some(last) = out.last() {
        if let Some(k) = last.get("key") {
            res.insert("after_key".to_string(), k.clone());
        }
    }
    res.insert("buckets".to_string(), Value::Array(out));
    Ok(Value::Object(res))
}

fn adjacency_matrix_agg(
    name: &str,
    t: &Map<String, Value>,
    def: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    typed: bool,
) -> Result<Value, ApiError> {
    let filters = match t.get("filters").and_then(|v| v.as_object()) {
        Some(f) if !f.is_empty() => f.clone(),
        _ => {
            return Err(ApiError::new(
                "action_request_validation_exception",
                &format!("[{}] is missing : filters parameter", name),
                500,
            ))
        }
    };
    let sep = t
        .get("separator")
        .and_then(|v| v.as_str())
        .unwrap_or("&")
        .to_string();
    let mut names: Vec<String> = filters.keys().cloned().collect();
    names.sort();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for nm in &names {
        let q = filters[nm].as_object().cloned().unwrap_or_default();
        let c = docs.iter().filter(|d| doc_matches(idx, d, &q)).count();
        counts.insert(nm.clone(), c);
    }
    let mut out = vec![];
    let mut push_bucket = |key: String, bdocs: &[&Value]| -> Result<(), ApiError> {
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(key));
        bm.insert("doc_count".to_string(), json!(bdocs.len()));
        if let Some(sub) = sub_aggs(def) {
            if let Value::Object(m) = eval_aggs_typed(&sub, bdocs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
        Ok(())
    };
    for i in 0..names.len() {
        let ni = &names[i];
        let qi = filters[ni].as_object().cloned().unwrap_or_default();
        let bi: Vec<&Value> = docs
            .iter()
            .filter(|d| doc_matches(idx, d, &qi))
            .copied()
            .collect();
        if !bi.is_empty() {
            push_bucket(ni.clone(), &bi)?;
        }
        for nj in names[i + 1..].iter() {
            let qj = filters[nj].as_object().cloned().unwrap_or_default();
            let bj: Vec<&Value> = bi
                .iter()
                .filter(|d| doc_matches(idx, d, &qj))
                .copied()
                .collect();
            if !bj.is_empty() {
                push_bucket(format!("{}{}{}", ni, sep, nj), &bj)?;
            }
        }
    }
    Ok(json!({"buckets": out}))
}

fn geo_point_of(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::String(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() == 2 {
                let lat = parts[0].trim().parse().ok()?;
                let lon = parts[1].trim().parse().ok()?;
                return Some((lat, lon));
            }
            let up = s.trim().to_uppercase();
            if let Some(rest) = up.strip_prefix("POINT") {
                let nums: Vec<f64> = rest
                    .trim()
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .split_whitespace()
                    .filter_map(|x| x.parse().ok())
                    .collect();
                if nums.len() >= 2 {
                    return Some((nums[1], nums[0]));
                }
            }
            None
        }
        Value::Object(m) => {
            let lat = m.get("lat").and_then(to_f64)?;
            let lon = m.get("lon").and_then(to_f64)?;
            Some((lat, lon))
        }
        Value::Array(a) if a.len() == 2 => {
            let lon = to_f64(&a[0])?;
            let lat = to_f64(&a[1])?;
            Some((lat, lon))
        }
        _ => None,
    }
}

fn geo_points_of_value(v: &Value) -> Vec<(f64, f64)> {
    match v {
        Value::Array(a) => {
            if a.len() == 2 {
                let num = |x: &Value| -> Option<f64> {
                    match x {
                        Value::Number(_) => to_f64(x),
                        Value::String(s) => s.trim().parse::<f64>().ok(),
                        _ => None,
                    }
                };
                if let (Some(lon), Some(lat)) = (num(&a[0]), num(&a[1])) {
                    return vec![(lat, lon)];
                }
            }
            let mut out = vec![];
            for e in a {
                out.extend(geo_points_of_value(e));
            }
            out
        }
        other => geo_point_of(other).map(|p| vec![p]).unwrap_or_default(),
    }
}

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371008.8f64;
    let p1 = lat1.to_radians();
    let p2 = lat2.to_radians();
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

fn plane_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dy = (lat2 - lat1) * 111319.9;
    let dx = (lon2 - lon1) * 111319.9 * ((lat1 + lat2) / 2.0).to_radians().cos();
    (dx * dx + dy * dy).sqrt()
}

const GEOHASH_CHARS: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

fn geohash_encode(lat: f64, lon: f64, precision: usize) -> String {
    let mut lat_lo = -90.0f64;
    let mut lat_hi = 90.0f64;
    let mut lon_lo = -180.0f64;
    let mut lon_hi = 180.0f64;
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut even = true;
    while out.len() < precision {
        if even {
            let mid = (lon_lo + lon_hi) / 2.0;
            if lon >= mid {
                bits = (bits << 1) | 1;
                lon_lo = mid;
            } else {
                bits <<= 1;
                lon_hi = mid;
            }
        } else {
            let mid = (lat_lo + lat_hi) / 2.0;
            if lat >= mid {
                bits = (bits << 1) | 1;
                lat_lo = mid;
            } else {
                bits <<= 1;
                lat_hi = mid;
            }
        }
        even = !even;
        nbits += 1;
        if nbits == 5 {
            out.push(GEOHASH_CHARS[bits as usize] as char);
            bits = 0;
            nbits = 0;
        }
    }
    out
}

fn geohash_grid_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let precision = t.get("precision").and_then(to_f64).map(|f| f as i64).unwrap_or(5);
    if !(1..=12).contains(&precision) {
        let msg = format!(
            "Invalid geohash aggregation precision of {}. Must be between 1 and 12.",
            precision
        );
        return Err(
            ApiError::new("illegal_argument_exception", &msg, 400)
                .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
        );
    }
    let size = t.get("size").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    let mut buckets: HashMap<String, Vec<Value>> = HashMap::new();
    let mut order: Vec<String> = vec![];
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some((lat, lon)) = geo_point_of(&v) {
                let gh = geohash_encode(lat, lon, precision as usize);
                if !buckets.contains_key(&gh) {
                    order.push(gh.clone());
                }
                buckets.entry(gh).or_default().push(Value::clone(d));
            }
        }
    }
    let mut keys = order;
    keys.sort_by(|a, b| {
        let ca = buckets[a].len();
        let cb = buckets[b].len();
        cb.cmp(&ca)
    });
    let mut out = vec![];
    for k in keys.into_iter().take(size) {
        let bd = &buckets[&k];
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(k));
        bm.insert("doc_count".to_string(), json!(bd.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = bd.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    Ok(json!({"buckets": out}))
}

fn geotile_grid_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let precision = t.get("precision").and_then(to_f64).map(|f| f as i64).unwrap_or(5);
    if !(0..=29).contains(&precision) {
        let msg = format!(
            "Invalid geotile_grid precision of {}. Must be between 0 and 29.",
            precision
        );
        return Err(
            ApiError::new("illegal_argument_exception", &msg, 400)
                .caused_by(ApiError::new("illegal_argument_exception", &msg, 400)),
        );
    }
    let size = t.get("size").and_then(to_f64).map(|f| f as usize).unwrap_or(10);
    let mut buckets: HashMap<String, Vec<Value>> = HashMap::new();
    let mut order: Vec<String> = vec![];
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            if let Some((lat, lon)) = geo_point_of(&v) {
                let n = 1i64 << precision;
                let mut x = (((lon + 180.0) / 360.0) * n as f64).floor() as i64;
                let merc = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0;
                let mut y = (merc * n as f64).floor() as i64;
                if x >= n {
                    x = n - 1;
                }
                if y >= n {
                    y = n - 1;
                }
                if x < 0 {
                    x = 0;
                }
                if y < 0 {
                    y = 0;
                }
                let key = format!("{}/{}/{}", precision, x, y);
                if !buckets.contains_key(&key) {
                    order.push(key.clone());
                }
                buckets.entry(key).or_default().push(Value::clone(d));
            }
        }
    }
    let mut keys = order;
    keys.sort_by(|a, b| {
        let ca = buckets[a].len();
        let cb = buckets[b].len();
        cb.cmp(&ca)
    });
    let mut out = vec![];
    for k in keys.into_iter().take(size) {
        let bd = &buckets[&k];
        let mut bm = Map::new();
        bm.insert("key".to_string(), json!(k));
        bm.insert("doc_count".to_string(), json!(bd.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = bd.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    Ok(json!({"buckets": out}))
}

fn geo_distance_agg(
    t: &Map<String, Value>,
    docs: &[&Value],
    idx: &Index,
    prefix: &str,
    sub: Option<Map<String, Value>>,
    typed: bool,
) -> Result<Value, ApiError> {
    let field = map_str(t, "field");
    let origin = t.get("origin").cloned().unwrap_or(Value::Null);
    let (olat, olon) = match geo_point_of(&origin) {
        Some(p) => p,
        None => {
            return Err(ApiError::new(
                "x_content_parse_exception",
                "Failed to parse origin",
                400,
            ))
        }
    };
    let unit = t
        .get("unit")
        .and_then(|v| v.as_str())
        .unwrap_or("m")
        .to_string();
    let unit_div = match unit.as_str() {
        "km" => 1000.0,
        "mi" => 1609.344,
        "yd" => 0.9144,
        "ft" => 0.3048,
        "in" => 0.0254,
        "cm" => 0.01,
        "mm" => 0.001,
        "nm" => 1852.0,
        _ => 1.0,
    };
    let distance_type = t
        .get("distance_type")
        .and_then(|v| v.as_str())
        .unwrap_or("arc")
        .to_string();
    let ranges = t
        .get("ranges")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    struct GBucket {
        from: f64,
        to: Option<f64>,
        key: Option<String>,
        docs: Vec<Value>,
    }
    let mut parsed: Vec<GBucket> = vec![];
    for r in &ranges {
        let rm = match r.as_object() {
            Some(m) => m,
            None => continue,
        };
        parsed.push(GBucket {
            from: rm.get("from").and_then(to_f64).unwrap_or(0.0),
            to: rm.get("to").and_then(to_f64),
            key: rm.get("key").and_then(value_to_string),
            docs: vec![],
        });
    }
    for d in docs {
        for v in doc_field_values(idx, d, &field, prefix) {
            let (lat, lon) = match geo_point_of(&v) {
                Some(p) => p,
                None => continue,
            };
            let meters = if distance_type == "plane" {
                plane_m(olat, olon, lat, lon)
            } else {
                haversine_m(olat, olon, lat, lon)
            };
            let dist = meters / unit_div;
            for b in parsed.iter_mut() {
                let ok_from = dist >= b.from;
                let ok_to = b.to.map(|tt| dist < tt).unwrap_or(true);
                if ok_from && ok_to {
                    b.docs.push(Value::clone(d));
                    break;
                }
            }
        }
    }
    parsed.sort_by(|a, b| {
        a.from
            .partial_cmp(&b.from)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                let ta = a.to.unwrap_or(f64::INFINITY);
                let tb = b.to.unwrap_or(f64::INFINITY);
                ta.partial_cmp(&tb).unwrap_or(Ordering::Equal)
            })
    });
    let keyed = t.get("keyed").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut out = vec![];
    let mut out_keys: Vec<String> = vec![];
    for b in &parsed {
        let key = b.key.clone().unwrap_or_else(|| {
            format!(
                "{}-{}",
                if b.from == 0.0 {
                    "*".to_string()
                } else {
                    fmt_bound(b.from)
                },
                b.to.map(fmt_bound).unwrap_or_else(|| "*".to_string())
            )
        });
        out_keys.push(key.clone());
        let mut bm = Map::new();
        if !keyed {
            bm.insert("key".to_string(), json!(key));
        }
        bm.insert("from".to_string(), json_num(b.from));
        if let Some(tt) = b.to {
            bm.insert("to".to_string(), json_num(tt));
        }
        bm.insert("doc_count".to_string(), json!(b.docs.len()));
        if let Some(sub) = &sub {
            let refs: Vec<&Value> = b.docs.iter().collect();
            if let Value::Object(m) = eval_aggs_typed(sub, &refs, idx, prefix, typed)? {
                for (sk, sv) in m {
                    bm.insert(sk, sv);
                }
            }
        }
        out.push(Value::Object(bm));
    }
    if keyed {
        let mut obj = Map::new();
        for (k, b) in out_keys.into_iter().zip(out) {
            obj.insert(k, b);
        }
        return Ok(json!({"buckets": obj}));
    }
    Ok(json!({"buckets": out}))
}

fn mean(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        0.0
    } else {
        vals.iter().sum::<f64>() / vals.len() as f64
    }
}

fn min_of(vals: &[f64]) -> f64 {
    vals.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max_of(vals: &[f64]) -> f64 {
    vals.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

fn sum_of(vals: &[f64]) -> f64 {
    vals.iter().sum()
}
