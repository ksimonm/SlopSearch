use crate::aggs::eval_aggs_root;
use crate::docs::{bm25_explanation, parse_q_param, search_alias_filter};
use crate::errors::{err_response, json_response};
use crate::eval::{
    as_query_list, integer_of, mapping_field_def, resolve_alias_field, single_entry,
    wildcard_regex, QueryCtx,
};
use crate::fields::doc_values_for;
use crate::shared;
use crate::store::*;
use crate::suggest::eval_suggest;
use crate::{Req, Store};
use axum::response::Response;
use serde_json::{json, Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

fn query_text(raw: &Value) -> (String, Option<&Map<String, Value>>) {
    crate::eval::query_text(raw)
}

pub fn field_type(idx: &Index, field: &str) -> String {
    idx.field_types
        .get(field)
        .cloned()
        .unwrap_or_else(|| "text".to_string())
}

fn is_numeric_type(t: &str) -> bool {
    matches!(
        t,
        "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
            | "scaled_float" | "date"
    )
}

pub fn validate_query(idx: &Index, query: &Value) -> Result<(), ApiError> {
    let m = match query.as_object() {
        Some(m) => m,
        None => return Ok(()),
    };
    for (clause, raw) in m {
        let known = matches!(
            clause.as_str(),
            "match_all" | "match" | "term" | "terms" | "range" | "bool" | "match_phrase"
                | "multi_match" | "wildcard" | "fuzzy" | "exists" | "prefix"
        );
        if !known {
            return Err(ApiError::parsing(&format!("unknown query [{}]", clause)));
        }
        if clause == "range" {
            if let Some(rm) = raw.as_object() {
                if let Some((field, cond_raw)) = single_entry(rm) {
                    let ft = field_type(idx, field);
                    if let Some(cond) = cond_raw.as_object() {
                        for bound in cond.values() {
                            if ft == "date" {
                                if parse_date(bound).is_none() {
                                    return Err(ApiError::parsing(&format!(
                                        "failed to parse date [{}]",
                                        field
                                    )));
                                }
                            } else if is_numeric_type(&ft) && to_f64(bound).is_none() {
                                return Err(ApiError::parsing(&format!(
                                    "failed to parse number [{}]",
                                    field
                                )));
                            }
                        }
                    }
                }
            }
        }
        if clause == "bool" {
            if let Some(bm) = raw.as_object() {
                for key in ["must", "should", "filter", "must_not"] {
                    for q in as_query_list(bm.get(key)) {
                        validate_query(idx, &q)?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- eval bridge

/// Builds doc values for docs that have none yet (Go's newQueryCtxForDoc does
/// this lazily and mutates the index; we keep a per-request side table).
pub fn precompute_doc_values(idx: &Index) -> HashMap<String, Map<String, Value>> {
    let mut extra = HashMap::new();
    for id in &idx.order {
        if idx.doc_values.contains_key(id) || idx.pending.contains_key(id) {
            continue;
        }
        let doc = match idx.docs.get(id) {
            Some(d) => d,
            None => continue,
        };
        if let Some(m) = doc.as_object() {
            if let Ok((vals, _)) = doc_values_for(idx, id, m) {
                let mut vm = Map::new();
                for (k, v) in vals {
                    vm.insert(k, Value::Array(v));
                }
                extra.insert(id.clone(), vm);
            }
        }
    }
    extra
}

pub fn eval_doc(
    idx: &Index,
    id: &str,
    doc: &Value,
    extra: &HashMap<String, Map<String, Value>>,
    query: &Map<String, Value>,
) -> Result<(bool, f64), ApiError> {
    match extra.get(id) {
        Some(vals) => QueryCtx::with_values(idx, id, doc, vals.clone()).eval(query),
        None => QueryCtx::new(idx, id, doc).eval(query),
    }
}

// ------------------------------------------------------- doc values loading

fn search_phase_error(reason: &str) -> ApiError {
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

pub fn check_doc_values_loadable(idx: &Index, field: &str) -> Option<ApiError> {
    let def = mapping_field_def(idx, field)?;
    let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if let Some(false) = def.get("doc_values").and_then(|d| d.as_bool()) {
        let reason = format!(
            "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
            field, ftype
        );
        return Some(search_phase_error(&reason));
    }
    if ftype == "text" {
        let fd = def
            .get("fielddata")
            .and_then(|d| d.as_bool())
            .unwrap_or(false);
        if !fd {
            let reason = format!(
                "Fielddata is disabled on text fields by default. Set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                field
            );
            return Some(search_phase_error(&reason));
        }
    }
    None
}

// ------------------------------------------------------- docvalue_fields etc.

struct FieldSpec {
    name: String,
    format: String,
}

fn parse_field_specs(raw: Option<&Value>) -> Vec<FieldSpec> {
    let mut out = vec![];
    let list = match raw.and_then(|v| v.as_array()) {
        Some(l) => l,
        None => return out,
    };
    for e in list {
        match e {
            Value::String(s) => out.push(FieldSpec {
                name: s.clone(),
                format: String::new(),
            }),
            Value::Object(m) => {
                let name = m
                    .get("field")
                    .and_then(value_to_string)
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let format = m
                    .get("format")
                    .and_then(value_to_string)
                    .unwrap_or_default();
                out.push(FieldSpec { name, format });
            }
            _ => {}
        }
    }
    out
}

fn text_like_type(t: &str) -> bool {
    matches!(
        t,
        "text" | "match_only_text" | "search_as_you_type" | "completion" | "annotated_text"
    )
}

fn flatten_any(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => {
            let mut out = vec![];
            for e in a {
                out.extend(flatten_any(e));
            }
            out
        }
        Value::Null => vec![],
        other => vec![other.clone()],
    }
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn date_parts(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let secs_total = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs_total.div_euclid(86400);
    let rem = secs_total.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, rem / 3600, (rem % 3600) / 60, rem % 60, millis)
}

pub(crate) fn format_epoch_ms(ms: i64) -> String {
    let (y, mo, d, hh, mi, ss, millis) = date_parts(ms);
    let ys = if y < 0 {
        format!("-{:04}", -y)
    } else {
        format!("{:04}", y)
    };
    format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        ys, mo, d, hh, mi, ss, millis
    )
}

pub(crate) fn apply_java_date_format(f: &str, ms: i64) -> Option<String> {
    let excluded = [
        "epoch_millis",
        "epoch_second",
        "date_optional_time",
        "strict_date_optional_time",
        "strict_date_optional_time_nanos",
        "date_time",
        "basic_date_time",
        "strict_date_optional_time||epoch_millis",
    ];
    if f.is_empty() || excluded.contains(&f) {
        return None;
    }
    let (y, mo, d, hh, mi, ss, millis) = date_parts(ms);
    let mut out = f.replace("'T'", "T");
    out = out.replace("SSSSSSSSS", &format!("{:09}", millis * 1_000_000));
    out = out.replace("SSS", &format!("{:03}", millis));
    out = out.replace("yyyy", &format!("{:04}", y));
    out = out.replace("yy", &format!("{:02}", y.rem_euclid(100)));
    out = out.replace("MM", &format!("{:02}", mo));
    out = out.replace("dd", &format!("{:02}", d));
    out = out.replace("HH", &format!("{:02}", hh));
    out = out.replace("mm", &format!("{:02}", mi));
    out = out.replace("ss", &format!("{:02}", ss));
    Some(out)
}

fn format_doc_value(ftype: &str, v: &Value, format: &str) -> Value {
    match ftype {
        "date_nanos" => {
            let Some(ns) = integer_of(v) else {
                return v.clone();
            };
            let ms = ns.div_euclid(1_000_000);
            if matches!(
                format,
                "strict_date_optional_time_nanos" | "date_optional_time_nanos"
            ) {
                let (y, mo, d, hh, mi, ss, millis) = date_parts(ms);
                let nanos = ns.rem_euclid(1_000_000_000);
                return Value::String(format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{:06}Z",
                    y, mo, d, hh, mi, ss, millis,
                    nanos % 1_000_000
                ));
            }
            match format {
                "" | "strict_date_optional_time" | "date_optional_time"
                | "strict_date_optional_time_nanos" | "date_optional_time_nanos" => {
                    Value::String(format_epoch_ms(ms))
                }
                "epoch_millis" => Value::String(ms.to_string()),
                "epoch_second" => Value::String((ms / 1000).to_string()),
                other => match apply_java_date_format(other, ms) {
                    Some(s) => Value::String(s),
                    None => Value::String(ms.to_string()),
                },
            }
        }
        "ip" => match v.as_str().and_then(|s| crate::fields::canonical_ip(s).ok()) {
            Some(c) => Value::String(c),
            None => v.clone(),
        },
        "date" => {
            let ms = match integer_of(v) {
                Some(n) => n,
                None => return v.clone(),
            };
            match format {
                "" | "strict_date_optional_time" | "date_optional_time"
                | "strict_date_optional_time_nanos" | "date_optional_time_nanos" => {
                    Value::String(format_epoch_ms(ms))
                }
                "epoch_millis" => Value::String(ms.to_string()),
                "epoch_second" => Value::String((ms / 1000).to_string()),
                other => match apply_java_date_format(other, ms) {
                    Some(s) => Value::String(s),
                    None => Value::String(ms.to_string()),
                },
            }
        }
        "float" | "double" | "half_float" | "scaled_float" => {
            if format.is_empty() || format == "use_field_mapping" {
                return v.clone();
            }
            let decimals = format
                .split_once('.')
                .map(|(_, f)| f.chars().filter(|c| *c == '0' || *c == '#').count())
                .unwrap_or(0);
            match to_f64(v) {
                Some(n) => Value::String(format!("{:.*}", decimals, n)),
                None => v.clone(),
            }
        }
        _ => v.clone(),
    }
}

fn geo_pair(v: &Value) -> Option<(f64, f64)> {
    let o = v.as_object()?;
    let lat = o.get("lat").and_then(|x| x.as_f64())?;
    let lon = o.get("lon").and_then(|x| x.as_f64())?;
    Some((lat, lon))
}

fn geo_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{:.1}", n)
    } else {
        format!("{}", n)
    }
}

fn format_geo(v: &Value, wkt: bool, as_shape: bool) -> Value {
    let Some((lat, lon)) = geo_pair(v) else { return v.clone() };
    if wkt {
        return json!(format!("POINT ({} {})", geo_num(lon), geo_num(lat)));
    }
    if as_shape {
        return json!({"type": "Point", "coordinates": [lon, lat]});
    }
    json!(format!("{}, {}", geo_num(lat), geo_num(lon)))
}

fn doc_values_output(
    idx: &Index,
    id: &str,
    field: &str,
    spec: &FieldSpec,
    fallback_source: bool,
) -> Result<Option<Vec<Value>>, ApiError> {
    let resolved = crate::eval::resolve_alias_field(idx, field);
    let mut field = resolved.as_str();
    let mut join_suffix: Option<String> = None;
    if mapping_field_def(idx, field).is_none() {
        if let Some((base, suffix)) = field.split_once('#') {
            if mapping_field_def(idx, base)
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                == Some("join")
            {
                join_suffix = Some(suffix.to_string());
                field = base;
            }
        }
    }
    let def = mapping_field_def(idx, field);
    let (ftype, has_dv) = match def {
        Some(d) => (
            d.get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            !matches!(d.get("doc_values").and_then(|v| v.as_bool()), Some(false)),
        ),
        None => (String::new(), true),
    };
    let mut out_format = spec.format.clone();
    if out_format.is_empty() {
        if let Some(d) = def {
            if matches!(ftype.as_str(), "date" | "date_nanos") {
                if let Some(mf) = d.get("format").and_then(|v| v.as_str()) {
                    if !mf.is_empty()
                        && !mf.contains("||")
                        && !matches!(
                            mf,
                            "strict_date_optional_time"
                                | "date_optional_time"
                                | "strict_date_optional_time_nanos"
                                | "epoch_millis"
                                | "epoch_second"
                                | "date_time"
                                | "basic_date_time"
                        )
                    {
                        out_format = mf.to_string();
                    }
                }
            }
        }
    }
    let vals: Option<Vec<Value>> = idx
        .doc_values
        .get(id)
        .and_then(|dv| dv.get(field))
        .map(crate::eval::array_values);
    if text_like_type(&ftype) || !has_dv {
        if !fallback_source {
            if !has_dv && vals.as_ref().map(|v| !v.is_empty()).unwrap_or(false) {
                let reason = format!(
                    "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
                    field, ftype
                );
                return Err(search_phase_error(&reason));
            }
            return Ok(None);
        }
        if let Some(doc) = idx.docs.get(id) {
            if let Some(v) = field_value(doc, field) {
                let list = flatten_any(v);
                if ftype == "geo_point" {
                    return Ok(Some(
                        list.iter()
                            .map(|x| {
                                format_geo(x, out_format == "wkt", out_format.is_empty())
                            })
                            .collect(),
                    ));
                }
                if ftype.is_empty() && fallback_source && list.iter().all(|x| x.is_object()) {
                    return Ok(None);
                }
                return Ok(Some(
                    list.iter()
                        .map(|x| format_doc_value(&ftype, x, &out_format))
                        .collect(),
                ));
            }
        }
        return Ok(None);
    }
    match vals {
        Some(v) if !v.is_empty() => Ok(Some(
            v.iter()
                .map(|x| {
                    if ftype == "join" {
                        return join_output_value(x, join_suffix.as_deref());
                    }
                    if ftype == "geo_point" {
                        format_geo(x, out_format == "wkt", fallback_source && out_format.is_empty())
                    } else {
                        format_doc_value(&ftype, x, &out_format)
                    }
                })
                .collect(),
        )),
        _ => Ok(None),
    }
}

fn join_output_value(v: &Value, suffix: Option<&str>) -> Value {
    let (name, parent) = match v {
        Value::String(s) => (s.clone(), String::new()),
        Value::Object(m) => (
            m.get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            m.get("parent")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    };
    match suffix {
        Some(s) if s == name => Value::String(parent),
        Some(s) => {
            let _ = s;
            Value::String(parent)
        }
        None => Value::String(name),
    }
}

// ------------------------------------------------------------------- sorting

fn compare_sort_values(a: &Value, b: &Value) -> Ordering {
    if let (Value::Number(na), Value::Number(nb)) = (a, b) {
        if let (Some(x), Some(y)) = (na.as_u64(), nb.as_u64()) {
            return x.cmp(&y);
        }
        if let (Some(x), Some(y)) = (na.as_i64(), nb.as_i64()) {
            return x.cmp(&y);
        }
        if let (Some(x), Some(y)) = (na.as_u64(), nb.as_i64()) {
            if y < 0 {
                return Ordering::Greater;
            }
            return x.cmp(&(y as u64));
        }
        if let (Some(x), Some(y)) = (na.as_i64(), nb.as_u64()) {
            if x < 0 {
                return Ordering::Less;
            }
            return (x as u64).cmp(&y);
        }
    }
    if (a.is_string() && b.is_number()) || (a.is_number() && b.is_string()) {
        if let (Some(x), Some(y)) = (to_f64(a), to_f64(b)) {
            return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
        }
    }
    if let Some(as_) = a.as_str() {
        let bs = value_to_string(b).unwrap_or_default();
        if let (Some(x), Some(y)) = (crate::eval::ip_to_16(as_), crate::eval::ip_to_16(&bs)) {
            return x.cmp(&y);
        }
        return as_.cmp(&bs);
    }
    if let (Some(af), Some(bf)) = (to_f64(a), to_f64(b)) {
        return af.partial_cmp(&bf).unwrap_or(Ordering::Equal);
    }
    let as_ = value_to_string(a).unwrap_or_default();
    let bs = value_to_string(b).unwrap_or_default();
    as_.cmp(&bs)
}

fn parse_sort(spec: &Value) -> (String, bool) {
    match spec {
        Value::String(s) => (s.clone(), false),
        Value::Object(m) => {
            for (k, v) in m {
                if k == "_score" {
                    continue;
                }
                let dir = value_to_string(v).unwrap_or_default();
                return (k.clone(), dir == "desc");
            }
            (String::new(), false)
        }
        _ => (String::new(), false),
    }
}

#[derive(Clone)]
struct MultiHit {
    idx_name: String,
    id: String,
    doc: Value,
    score: f64,
    pos: i64,
    named: Vec<(String, f64)>,
}

#[derive(Clone, Default)]
struct SortSpec {
    field: String,
    desc: bool,
    kind: u8,
    mode: Option<String>,
    missing: Option<Value>,
    nested: Option<String>,
    nested_filter: Option<Value>,
    unmapped_type: Option<String>,
    numeric_type: Option<String>,
    script_src: Option<String>,
    script_params: Value,
    script_type: String,
    geo: Value,
    geo_unit: String,
    geo_ignore: bool,
}

fn sort_parse_error(reason: &str) -> ApiError {
    let mut e = ApiError::new("x_content_parse_exception", reason, 400);
    e.caused_by = Some(Box::new(ApiError::new(
        "illegal_argument_exception",
        reason,
        400,
    )));
    e
}

fn sort_shard_error(typ: &str, reason: &str) -> ApiError {
    let mut e = ApiError::new(
        "search_phase_execution_exception",
        "all shards failed",
        400,
    );
    let cause = ApiError::new(typ, reason, 400);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e.field("lang", json!("painless"))
}

fn parse_sort_specs(raw: Option<&Value>) -> Result<Vec<SortSpec>, ApiError> {
    let items: Vec<Value> = match raw {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
    };
    let mut out = Vec::new();
    for item in items {
        let mut sp = SortSpec::default();
        match &item {
            Value::String(f) => {
                sp.field = f.clone();
                sp.kind = if f == "_score" { 1 } else if f == "_id" { 2 } else if f == "_index" { 3 } else if f == "_doc" { 4 } else { 0 };
                if sp.kind == 1 {
                    sp.desc = true;
                }
            }
            Value::Object(m) => {
                if let Some(g) = m.get("_geo_distance") {
                    sp.kind = 6;
                    if let Some(go) = g.as_object() {
                        for (k, v) in go {
                            match k.as_str() {
                                "order" => sp.desc = value_to_string(v).unwrap_or_default() == "desc",
                                "unit" => sp.geo_unit = value_to_string(v).unwrap_or_default(),
                                "mode" => sp.mode = v.as_str().map(|x| x.to_string()),
                                "ignore_unmapped" => sp.geo_ignore = v.as_bool().unwrap_or(false),
                                "nested" => {}
                                _ => {
                                    sp.geo = v.clone();
                                    sp.field = k.clone();
                                }
                            }
                        }
                    }
                    if sp.geo_unit.is_empty() {
                        sp.geo_unit = "m".to_string();
                    }
                    if !matches!(sp.geo_unit.as_str(), "m" | "km" | "mi" | "yd" | "ft" | "in" | "cm" | "mm" | "nmi") {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            &format!("No distance unit match [{}]", sp.geo_unit),
                            400,
                        ));
                    }
                    out.push(sp);
                    continue;
                }
                if let Some(sc) = m.get("_script") {
                    sp.kind = 5;
                    if let Some(so) = sc.as_object() {
                        sp.desc = so
                            .get("order")
                            .map(|v| value_to_string(v).unwrap_or_default() == "desc")
                            .unwrap_or(false);
                        sp.script_type = so
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("number")
                            .to_string();
                        if let Some(script) = so.get("script") {
                            if let Some(src) = script.get("source").and_then(|v| v.as_str()) {
                                sp.script_src = Some(src.to_string());
                            }
                            if let Some(params) = script.get("params") {
                                sp.script_params = params.clone();
                            }
                        }
                    }
                    out.push(sp);
                    continue;
                }
                let Some((f, opts)) = m.iter().next() else { continue };
                sp.field = f.clone();
                sp.kind = if f == "_score" { 1 } else if f == "_id" { 2 } else if f == "_index" { 3 } else if f == "_doc" { 4 } else { 0 };
                if let Some(op) = opts.as_object() {
                    if op.contains_key("format") {
                        return Err(sort_parse_error(
                            "[1:1] [field_sort] unknown field [format]",
                        ));
                    }
                    for (k, v) in op {
                        match k.as_str() {
                            "order" => {
                                let od = value_to_string(v).unwrap_or_default();
                                if od.eq_ignore_ascii_case("asc") {
                                    sp.desc = false;
                                } else if od.eq_ignore_ascii_case("desc") {
                                    sp.desc = true;
                                } else {
                                    return Err(ApiError::new(
                                        "illegal_argument_exception",
                                        &format!("No enum constant org.opensearch.search.sort.SortOrder.{}", od.to_uppercase()),
                                        400,
                                    ));
                                }
                            }
                            "mode" => {
                                let mo = value_to_string(v).unwrap_or_default();
                                if !matches!(mo.as_str(), "min" | "max" | "sum" | "avg" | "median") {
                                    let mut e = ApiError::new(
                                        "x_content_parse_exception",
                                        "[1:1] failed to parse field [mode]",
                                        400,
                                    );
                                    e.caused_by = Some(Box::new(ApiError::new(
                                        "illegal_argument_exception",
                                        &format!("Unknown SortMode [{}]", mo),
                                        400,
                                    )));
                                    return Err(e);
                                }
                                sp.mode = Some(mo);
                            }
                            "missing" => sp.missing = Some(v.clone()),
                            "nested" => {
                                if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
                                    sp.nested = Some(path.to_string());
                                }
                                if let Some(f) = v.get("filter") {
                                    sp.nested_filter = Some(f.clone());
                                }
                            }
                            "nested_path" => {
                                sp.nested = Some(value_to_string(v).unwrap_or_default())
                            }
                            "unmapped_type" => {
                                sp.unmapped_type = v.as_str().map(|x| x.to_string())
                            }
                            "numeric_type" => {
                                sp.numeric_type = v.as_str().map(|x| x.to_string())
                            }
                            _ => {}
                        }
                    }
                } else if let Some(od) = opts.as_str() {
                    if od.eq_ignore_ascii_case("desc") {
                        sp.desc = true;
                    } else if !od.eq_ignore_ascii_case("asc") {
                        return Err(ApiError::new(
                            "illegal_argument_exception",
                            &format!("No enum constant org.opensearch.search.sort.SortOrder.{}", od.to_uppercase()),
                            400,
                        ));
                    }
                }
            }
            _ => {}
        }
        out.push(sp);
    }
    Ok(out)
}

fn sort_mapping_def(idx: &Index, field: &str) -> Option<Map<String, Value>> {
    let field = &crate::eval::resolve_alias_field(idx, field);
    if let Some(d) = mapping_field_def(idx, field) {
        return Some(d.clone());
    }
    if crate::fields::flat_object_root(idx, field).is_some() {
        let mut d = Map::new();
        d.insert("type".to_string(), json!("keyword"));
        return Some(d);
    }
    let (parent, sub) = field.rsplit_once('.')?;
    let pd = mapping_field_def(idx, parent)?;
    pd.get("fields")
        .and_then(|f| f.get(sub))
        .and_then(|d| d.as_object())
        .cloned()
}

pub fn routing_shard_of_pub(routing: &str, n: i64) -> i64 {
    routing_shard_of(routing, n)
}

fn routing_shard_of(routing: &str, n: i64) -> i64 {
    if n <= 1 {
        return 0;
    }
    let sum: i64 = routing.bytes().map(|b| b as i64).sum();
    (sum + 1).rem_euclid(n)
}

fn nested_parent_of(idx: &Index, field: &str) -> Option<String> {
    let mut parts: Vec<&str> = field.split('.').collect();
    while parts.len() > 1 {
        parts.pop();
        let path = parts.join(".");
        if let Some(def) = mapping_field_def(idx, &path) {
            if def.get("type").and_then(|t| t.as_str()) == Some("nested") {
                return Some(path);
            }
        }
    }
    None
}

fn sort_missing_sentinel(sp: &SortSpec, ftype: &str, desc: bool) -> Value {
    match sp.numeric_type.as_deref() {
        Some("long") => {
            if desc { json!(-9223372036854775808i64) } else { json!(9223372036854775807i64) }
        }
        Some("double") => {
            if desc { json!("-Infinity") } else { json!("Infinity") }
        }
        _ => sort_type_min_max(ftype, desc),
    }
}

fn sort_type_min_max(ftype: &str, desc: bool) -> Value {
    match ftype {
        "integer" | "short" | "byte" => {
            if desc { json!(-2147483648i64) } else { json!(2147483647i64) }
        }
        "long" => {
            if desc { json!(-9223372036854775808i64) } else { json!(9223372036854775807i64) }
        }
        "float" | "double" | "half_float" | "scaled_float" => {
            if desc { json!("-Infinity") } else { json!("Infinity") }
        }
        "date" | "date_nanos" => {
            if desc { json!(-9223372036854775808i64) } else { json!(9223372036854775807i64) }
        }
        _ => Value::Null,
    }
}

fn source_sort_list(idx: &Index, id: &str, field: &str) -> Vec<Value> {
    let Some(doc) = idx.docs.get(id) else { return vec![] };
    let mut paths = vec![field.to_string()];
    let mut base = field.to_string();
    while let Some(pos) = base.rfind('.') {
        base.truncate(pos);
        paths.push(base.clone());
    }
    for p in paths {
        if let Some(v) = field_value(doc, &p) {
            let list = flatten_any(v);
            if !list.is_empty() {
                return list;
            }
        }
    }
    vec![]
}

fn convert_sort_scalar(sp: &SortSpec, ftype: &str, v: &Value) -> Value {
    if matches!(sp.numeric_type.as_deref(), Some("date_nanos")) && ftype != "date_nanos" {
        if let Some(ms) = integer_of(v) {
            return json!(ms * 1_000_000);
        }
        if let Some(ms) = crate::store::parse_date(v) {
            return json!((ms * 1_000_000.0) as i64);
        }
    }
    if ftype == "date_nanos" || matches!(sp.numeric_type.as_deref(), Some("date_nanos")) {
        if let Some(n) = integer_of(v) {
            return json!(n);
        }
        if let Some(s) = v.as_str() {
            if let Some(ns) = crate::fields::parse_date_nanos_any(&Map::new(), s) {
                return json!(ns);
            }
        }
        if let Some(ms) = crate::store::parse_date(v) {
            return json!((ms * 1_000_000.0) as i64);
        }
    }
    if ftype == "date" {
        if let Some(ms) = crate::store::parse_date(v) {
            return json!(ms as i64);
        }
    }
    if ftype == "ip" {
        if let Some(s) = v.as_str() {
            if let Ok(c) = crate::fields::canonical_ip(s) {
                return Value::String(c);
            }
        }
    }
    if sp.numeric_type.as_deref() == Some("long") {
        if let Some(n) = to_f64(v) {
            return json!(n as i64);
        }
    }
    v.clone()
}

fn apply_sort_mode(sp: &SortSpec, ftype: &str, mut vals: Vec<Value>, desc: bool) -> Option<Value> {
    if vals.is_empty() {
        return None;
    }
    if vals.len() == 1 {
        return Some(vals.remove(0));
    }
    let numeric = matches!(
        ftype,
        "integer" | "short" | "byte" | "long" | "float" | "double" | "half_float" | "scaled_float" | "unsigned_long"
    );
    let mode = sp
        .mode
        .clone()
        .unwrap_or_else(|| if desc { "max".to_string() } else { "min".to_string() });
    if !numeric {
        return match mode.as_str() {
            "min" => vals.into_iter().min_by(compare_sort_values),
            "max" => vals.into_iter().max_by(compare_sort_values),
            _ => Some(Value::Null),
        };
    }
    let nums: Vec<f64> = vals.iter().filter_map(to_f64).collect();
    if nums.is_empty() {
        return None;
    }
    let out = match mode.as_str() {
        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "sum" => nums.iter().sum(),
        "avg" => nums.iter().sum::<f64>() / nums.len() as f64,
        "median" => {
            let mut n = nums.clone();
            n.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
            let m = n.len() / 2;
            if n.len() % 2 == 0 {
                (n[m - 1] + n[m]) / 2.0
            } else {
                n[m]
            }
        }
        _ => nums[0],
    };
    let integral = matches!(ftype, "integer" | "short" | "byte" | "long" | "unsigned_long");
    Some(if integral {
        json!(out.round() as i64)
    } else if out.is_finite() {
        json!(out)
    } else if out > 0.0 {
        json!("Infinity")
    } else {
        json!("-Infinity")
    })
}

fn nested_sort_values(idx: &Index, id: &str, sp: &SortSpec, path: &str) -> Vec<Value> {
    let rel = sp
        .field
        .strip_prefix(&format!("{}.", path))
        .unwrap_or(&sp.field)
        .to_string();
    let Some(doc) = idx.docs.get(id) else { return vec![] };
    let Some(items) = field_value(doc, path) else { return vec![] };
    let arr = match items {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let filter = sp.nested_filter.as_ref().and_then(|f| {
        f.get("term").and_then(|t| t.as_object()).and_then(|m| {
            m.iter().next().map(|(k, v)| {
                let kk = k.strip_prefix(&format!("{}.", path)).unwrap_or(k).to_string();
                (kk, v.clone())
            })
        })
    });
    let mut out = Vec::new();
    for item in arr {
        if let Some((fk, fv)) = &filter {
            let ok = field_value(&item, fk)
                .map(|v| crate::eval::array_values(v).iter().any(|x| x == fv))
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        if let Some(v) = field_value(&item, &rel) {
            out.extend(flatten_any(v));
        }
    }
    out
}

fn eval_sort_script(sp: &SortSpec, h: &MultiHit) -> Value {
    let src = sp.script_src.clone().unwrap_or_default();
    let params = sp.script_params.as_object().cloned().unwrap_or_default();
    let field_of = |s: &str| -> Option<String> {
        let a = s.find("doc['")?;
        let rest = &s[a + 5..];
        let b = rest.find("']")?;
        Some(rest[..b].to_string())
    };
    if let Some(f) = field_of(&src) {
        if let Some(v) = field_value(&h.doc, &f) {
            let raw = flatten_any(v).first().cloned().unwrap_or(Value::Null);
            if src.contains("Math.abs") {
                let p = params.get("p").and_then(to_f64).unwrap_or(0.0);
                let n = to_f64(&raw).unwrap_or(0.0);
                return json!((n - p).abs());
            }
            if src.contains("* -1") {
                let n = to_f64(&raw).unwrap_or(0.0);
                return json!(-n);
            }
            return raw;
        }
    }
    Value::Null
}

fn geo_point(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::Object(m) => Some((
            m.get("lat").and_then(to_f64)?,
            m.get("lon").and_then(to_f64)?,
        )),
        Value::Array(a) if a.len() == 2 => {
            let lon = a[0].as_f64().or_else(|| a[0].as_i64().map(|x| x as f64))?;
            let lat = a[1].as_f64().or_else(|| a[1].as_i64().map(|x| x as f64))?;
            Some((lat, lon))
        }
        Value::String(s) => {
            let t = s.trim();
            if t.to_ascii_uppercase().starts_with("POINT") {
                let inner = t
                    .split_once('(')
                    .and_then(|(_, r)| r.rsplit_once(')'))
                    .map(|(l, _)| l)
                    .unwrap_or(t);
                let parts: Vec<&str> = inner.split_whitespace().collect();
                if parts.len() >= 2 {
                    let lon: f64 = parts[0].parse().ok()?;
                    let lat: f64 = parts[1].parse().ok()?;
                    return Some((lat, lon));
                }
                return None;
            }
            let mut it = t.split(',');
            let lat = it.next()?.trim().parse::<f64>().ok();
            let lon = it.next().and_then(|v| v.trim().parse::<f64>().ok());
            if let (Some(lat), Some(lon)) = (lat, lon) {
                return Some((lat, lon));
            }
            decode_geohash_search(t)
        }
        _ => None,
    }
}

fn decode_geohash_search(s: &str) -> Option<(f64, f64)> {
    const B32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let mut even = true;
    let mut lat_min = -90.0f64;
    let mut lat_max = 90.0f64;
    let mut lon_min = -180.0f64;
    let mut lon_max = 180.0f64;
    let mut seen = false;
    for c in s.chars() {
        let ci = B32.iter().position(|b| *b as char == c.to_ascii_lowercase())?;
        seen = true;
        for bit in (0..5).rev() {
            let one = (ci >> bit) & 1 == 1;
            if even {
                let mid = (lon_min + lon_max) / 2.0;
                if one {
                    lon_min = mid;
                } else {
                    lon_max = mid;
                }
            } else {
                let mid = (lat_min + lat_max) / 2.0;
                if one {
                    lat_min = mid;
                } else {
                    lat_max = mid;
                }
            }
            even = !even;
        }
    }
    if !seen {
        return None;
    }
    Some(((lat_min + lat_max) / 2.0, (lon_min + lon_max) / 2.0))
}

fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = 6371.0;
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let x = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * x.sqrt().asin()
}

fn compute_sort_cell(store: &Store, h: &MultiHit, sp: &SortSpec) -> Result<(bool, Value), ApiError> {
    if sp.kind == 1 {
        return Ok((false, json!(h.score)));
    }
    if sp.kind == 2 {
        return Ok((false, json!(h.id)));
    }
    if sp.kind == 3 {
        return Ok((false, json!(h.idx_name)));
    }
    if sp.kind == 4 {
        return Ok((false, json!(h.pos)));
    }
    if sp.kind == 5 {
        return Ok((false, eval_sort_script(sp, h)));
    }
    let idx = &store.indices[&h.idx_name];
    if sp.kind == 6 {
        let origins: Vec<(f64, f64)> = match &sp.geo {
            Value::Array(a)
                if a.len() == 2 && a.iter().all(|v| v.is_number()) =>
            {
                geo_point(&sp.geo).into_iter().collect()
            }
            Value::Array(a) => a.iter().filter_map(geo_point).collect(),
            other => geo_point(other).into_iter().collect(),
        };
        if origins.is_empty() {
            return Ok((false, json!("Infinity")));
        }
        let pts: Vec<(f64, f64)> = match field_value(&h.doc, &sp.field) {
            Some(v @ Value::Array(a)) if a.len() == 2 && a.iter().all(|x| x.is_number()) => {
                geo_point(v).into_iter().collect()
            }
            Some(v) => flatten_any(v).iter().filter_map(geo_point).collect(),
            None => Vec::new(),
        };
        if pts.is_empty() {
            return Ok((false, json!("Infinity")));
        }
        let mut best = f64::INFINITY;
        for p in pts {
          for origin in &origins {
            let mut d = haversine_km(*origin, p);
            match sp.geo_unit.as_str() {
                "km" => {}
                "mi" => d *= 0.621371,
                "yd" => d *= 1093.61,
                "ft" => d *= 3280.84,
                "in" => d *= 39370.1,
                "cm" => d *= 100000.0,
                "mm" => d *= 1_000_000.0,
                "nmi" => d *= 0.539957,
                _ => d *= 1000.0,
            }
            best = best.min(d);
          }
        }
        return Ok((false, if best.is_finite() { json!(best) } else { json!("Infinity") }));
    }
    let nested_missing = sp.nested.is_none()
        && sp.field.contains('.')
        && nested_parent_of(idx, &sp.field).is_some();
    let def = sort_mapping_def(idx, &sp.field);
    let ftype = def
        .as_ref()
        .and_then(|d| d.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if def.is_none() {
        if let Some(ut) = &sp.unmapped_type {
            let sentinel = match ut.as_str() {
                "long" => json!(9223372036854775807i64),
                "integer" => json!(2147483647i64),
                "double" | "float" => json!("Infinity"),
                _ => Value::Null,
            };
            return Ok((false, sentinel));
        }
        return Ok((true, Value::Null));
    }
    let text_fielddata = ftype == "text"
        && def
            .as_ref()
            .and_then(|d| d.get("fielddata"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    if ftype == "binary" {
        let reason = format!(
            "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [binary]. Use doc values instead.",
            sp.field
        );
        return Err(sort_shard_error("illegal_argument_exception", &reason));
    }
    if ftype == "text" && !text_fielddata {
        let reason = format!(
            "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
            sp.field
        );
        return Err(sort_shard_error("illegal_argument_exception", &reason));
    }
    let mut vals: Vec<Value> = Vec::new();
    if let Some(path) = &sp.nested {
        let nested_def = mapping_field_def(idx, path);
        if nested_def.map(|d| d.get("type").and_then(|t| t.as_str()) == Some("nested")) != Some(true) {
            return Err(sort_shard_error(
                "query_shard_exception",
                &format!("[nested] failed to find nested object under path [{}]", path),
            ));
        }
        vals = nested_sort_values(idx, &h.id, sp, path);
    } else if nested_missing {
        vals = vec![];
    } else {
        let sf = crate::eval::resolve_alias_field(idx, &sp.field);
        if let Some(dv) = idx.doc_values.get(&h.id).and_then(|m| m.get(&sf)) {
            vals = crate::eval::array_values(dv);
        }
    }
    if vals.is_empty() && sp.nested.is_none() && !nested_missing {
        let sf = crate::eval::resolve_alias_field(idx, &sp.field);
        vals = source_sort_list(idx, &h.id, &sf);
    }
    if text_fielddata {
        vals = vals
            .iter()
            .filter_map(|v| v.as_str())
            .flat_map(crate::store::analyze)
            .map(Value::String)
            .collect();
    }
    vals = vals
        .iter()
        .map(|v| convert_sort_scalar(sp, &ftype, v))
        .collect();
    if vals.is_empty() {
        if let Some(m) = &sp.missing {
            if m.as_str() == Some("_first") {
                let mut v = sort_type_min_max(&ftype, !sp.desc);
                if v.is_null() {
                    v = Value::Null;
                }
                return Ok((false, v));
            }
            if m.as_str() == Some("_last") {
                return Ok((false, sort_missing_sentinel(sp, &ftype, sp.desc)));
            }
            return Ok((false, convert_sort_scalar(sp, &ftype, m)));
        }
        return Ok((true, sort_missing_sentinel(sp, &ftype, sp.desc)));
    }
    let numeric = matches!(ftype.as_str(), "integer" | "short" | "byte" | "long" | "float" | "double" | "half_float" | "scaled_float" | "unsigned_long");
    if !numeric {
        if let Some(mode) = &sp.mode {
            if matches!(mode.as_str(), "avg" | "median" | "sum") {
                return Err(sort_shard_error(
                    "query_shard_exception",
                    "we only support AVG, MEDIAN and SUM on number based fields",
                ));
            }
        }
    }
    match apply_sort_mode(sp, &ftype, vals, sp.desc) {
        Some(v) => Ok((false, v)),
        None => Ok((true, sort_missing_sentinel(sp, &ftype, sp.desc))),
    }
}

fn cmp_sort_cells(a: &(bool, Value), b: &(bool, Value)) -> Ordering {
    match (a.0, b.0) {
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    compare_sort_values(&a.1, &b.1)
}

fn sort_after_value(v: &Value, sp: &SortSpec) -> Option<Value> {
    if sp.kind == 0 {
        let idx_dummy_type = sp.field.as_str();
        let _ = idx_dummy_type;
        if let Some(ms) = crate::store::parse_date(v) {
            if matches!(sp.numeric_type.as_deref(), Some("date_nanos")) {
                return Some(json!((ms * 1_000_000.0) as i64));
            }
            if v.is_string() && sp.field != "" {
                return Some(json!(ms as i64));
            }
        }
    }
    Some(v.clone())
}

fn sort_after_gt(hit: &[Value], after: &[Value], specs: &[SortSpec]) -> bool {
    for (i, sp) in specs.iter().enumerate() {
        let a = hit.get(i).cloned().unwrap_or(Value::Null);
        let b = sort_after_value(&after[i], sp).unwrap_or(Value::Null);
        let mut c = compare_sort_values(&a, &b);
        if sp.desc {
            c = c.reverse();
        }
        match c {
            Ordering::Greater => return true,
            Ordering::Less => return false,
            Ordering::Equal => {}
        }
    }
    false
}

fn sort_after_valid(sp: &SortSpec, v: &Value, numeric: bool) -> bool {
    if sp.kind != 0 {
        return true;
    }
    if !numeric {
        return true;
    }
    if v.is_number() {
        return true;
    }
    let Some(text) = v.as_str() else { return false };
    if crate::store::parse_date(v).is_some() {
        return true;
    }
    text.parse::<f64>().is_ok()
}

fn sort_value(store: &Store, h: &MultiHit, field: &str, desc: bool) -> Option<Value> {
    match field {
        "_score" => return Some(json!(h.score)),
        "_id" => return Some(json!(h.id)),
        _ => {}
    }
    let idx = &store.indices[&h.idx_name];
    let mut field = field.to_string();
    if let Some(def) = mapping_field_def(idx, &field) {
        if def.get("type").and_then(|t| t.as_str()) == Some("alias") {
            field = resolve_alias_field(idx, &field);
        }
    }
    let vals = idx.doc_values.get(&h.id)?;
    let list = vals.get(&field)?;
    let list = crate::eval::array_values(list);
    if list.is_empty() {
        return None;
    }
    if list.len() == 1 {
        return Some(list[0].clone());
    }
    let mut sorted = list;
    sorted.sort_by(compare_sort_values);
    if desc {
        sorted.pop()
    } else {
        sorted.into_iter().next()
    }
}

// ------------------------------------------------------------- source/highlight

fn collect_leaf_paths(v: &Value, prefix: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if m.is_empty() {
                if !prefix.is_empty() {
                    out.push(prefix.to_string());
                }
                return;
            }
            for (k, child) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                if child.is_object() || child.is_array() {
                    collect_leaf_paths(child, &p, out);
                } else {
                    out.push(p);
                }
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                if !prefix.is_empty() {
                    out.push(prefix.to_string());
                }
                return;
            }
            for child in a {
                if child.is_object() || child.is_array() {
                    collect_leaf_paths(child, prefix, out);
                } else {
                    out.push(prefix.to_string());
                }
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
        }
    }
}

fn segment_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == value;
    }
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut i, mut j, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while j < v.len() {
        if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

fn path_matches(pattern: &str, leaf: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let ps: Vec<&str> = pattern.split('.').collect();
    let ls: Vec<&str> = leaf.split('.').collect();
    for (i, p) in ps.iter().enumerate() {
        match ls.get(i) {
            Some(l) => {
                if !segment_match(p, l) {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

fn merge_path(out: &mut Map<String, Value>, src: &Value, segs: &[String]) {
    if segs.is_empty() {
        return;
    }
    let joined = segs.join(".");
    if let Some(v) = src.as_object().and_then(|o| o.get(&joined)) {
        out.insert(joined, v.clone());
        return;
    }
    let seg = &segs[0];
    if segs.len() == 1 {
        if let Some(o) = src.as_object() {
            if let Some(v) = o.get(seg) {
                out.insert(seg.clone(), v.clone());
            }
        }
        return;
    }
    let Some(child) = src.as_object().and_then(|o| o.get(seg)) else {
        return;
    };
    if let Some(arr) = child.as_array() {
        if !out.get(seg).map(|v| v.is_array()).unwrap_or(false) {
            out.insert(seg.clone(), Value::Array(vec![]));
        }
        if let Some(out_arr) = out.get_mut(seg).and_then(|v| v.as_array_mut()) {
            if out_arr.len() < arr.len() {
                out_arr.resize(arr.len(), Value::Object(Map::new()));
            }
            for (i, e) in arr.iter().enumerate() {
                if e.is_object() {
                    if !out_arr[i].is_object() {
                        out_arr[i] = Value::Object(Map::new());
                    }
                    if let Some(m) = out_arr[i].as_object_mut() {
                        merge_path(m, e, &segs[1..]);
                    }
                }
            }
        }
    } else if child.is_object() {
        if !out.get(seg).map(|v| v.is_object()).unwrap_or(false) {
            out.insert(seg.clone(), Value::Object(Map::new()));
        }
        if let Some(m) = out.get_mut(seg).and_then(|v| v.as_object_mut()) {
            merge_path(m, child, &segs[1..]);
        }
    }
}

fn remove_path(out: &mut Map<String, Value>, segs: &[String]) {
    if segs.is_empty() {
        return;
    }
    let seg = &segs[0];
    if segs.len() == 1 {
        out.remove(seg);
        return;
    }
    match out.get_mut(seg) {
        Some(Value::Object(m)) => remove_path(m, &segs[1..]),
        Some(Value::Array(a)) => {
            for e in a.iter_mut() {
                if let Some(m) = e.as_object_mut() {
                    remove_path(m, &segs[1..]);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn mapping_source_spec(idx: &Index) -> Option<Value> {
    let src = idx.mappings.get("_source")?.as_object()?;
    let inc = src
        .get("includes")
        .or_else(|| src.get("include"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    let exc = src
        .get("excludes")
        .or_else(|| src.get("exclude"))
        .cloned()
        .unwrap_or_else(|| json!([]));
    Some(json!({"includes": inc, "excludes": exc}))
}

fn source_enabled_search(idx: &Index) -> bool {
    idx.mappings
        .get("_source")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn stored_paths(idx: &Index, patterns: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(
        props: &Map<String, Value>,
        prefix: &str,
        patterns: &[String],
        out: &mut Vec<String>,
    ) {
        for (name, raw) in props {
            let Some(def) = raw.as_object() else { continue };
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}.{}", prefix, name)
            };
            let matched = patterns.iter().any(|p| {
                p == "*"
                    || crate::shared::wildcard_match(p, &path)
                    || (p == &path)
            });
            if def.get("store").and_then(|v| v.as_bool()) == Some(true) && matched {
                out.push(path.clone());
            }
            if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
                walk(sub, &path, patterns, out);
            }
        }
    }
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        walk(props, "", patterns, &mut out);
    }
    out
}

pub(crate) fn format_stored_value(ftype: &str, v: &Value) -> Value {
    format_doc_value(ftype, v, "")
}

fn format_stored(idx: &Index, field: &str, v: &Value) -> Value {
    let def = mapping_field_def(idx, field);
    let ftype = def
        .as_ref()
        .and_then(|d| d.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if matches!(ftype, "date" | "date_nanos") {
        if let Some(s) = v.as_str() {
            if let Some(d) = def.as_ref() {
                if let Some(ms) = crate::fields::parse_date_any(d, s) {
                    return format_doc_value("date", &json!(ms), "");
                }
            }
        }
    }
    v.clone()
}

pub(crate) fn filter_source(doc: &Value, spec: &Value) -> Value {
    let mut includes: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    match spec {
        Value::String(s) => {
            for p in s.split(',') {
                if !p.is_empty() {
                    includes.push(p.to_string());
                }
            }
        }
        Value::Array(a) => {
            for e in a {
                if let Some(s) = e.as_str() {
                    includes.push(s.to_string());
                }
            }
        }
        Value::Object(m) => {
            for key in ["includes", "include"] {
                if let Some(a) = m.get(key).and_then(|v| v.as_array()) {
                    for e in a {
                        if let Some(s) = e.as_str() {
                            includes.push(s.to_string());
                        }
                    }
                } else if let Some(s) = m.get(key).and_then(|v| v.as_str()) {
                    includes.push(s.to_string());
                }
            }
            for key in ["excludes", "exclude"] {
                if let Some(a) = m.get(key).and_then(|v| v.as_array()) {
                    for e in a {
                        if let Some(s) = e.as_str() {
                            excludes.push(s.to_string());
                        }
                    }
                } else if let Some(s) = m.get(key).and_then(|v| v.as_str()) {
                    excludes.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    if includes.is_empty() {
        includes.push("*".to_string());
    }
    let mut leaves = Vec::new();
    collect_leaf_paths(doc, "", &mut leaves);
    let mut out = Map::new();
    for leaf in leaves {
        let inc = includes.iter().any(|p| path_matches(p, &leaf));
        let exc = excludes.iter().any(|p| path_matches(p, &leaf));
        if inc && !exc {
            let segs: Vec<String> = leaf.split('.').map(|s| s.to_string()).collect();
            merge_path(&mut out, doc, &segs);
        }
    }
    for pat in &excludes {
        if let Some(prefix) = pat.strip_suffix(".*") {
            if !prefix.is_empty() && includes.iter().any(|p| path_matches(p, prefix)) {
                if let Some(v) = crate::store::field_value(doc, prefix) {
                    if v.is_object() && out.get(prefix).is_none() {
                        let segs: Vec<&str> = prefix.split('.').collect();
                        if segs.len() == 1 {
                            out.insert(prefix.to_string(), json!({}));
                        } else {
                            let mut cur = &mut out;
                            for seg in &segs[..segs.len() - 1] {
                                if !cur.get(*seg).map(|v| v.is_object()).unwrap_or(false) {
                                    cur.insert(seg.to_string(), Value::Object(Map::new()));
                                }
                                cur = cur.get_mut(*seg).unwrap().as_object_mut().unwrap();
                            }
                            cur.insert(segs[segs.len() - 1].to_string(), json!({}));
                        }
                    }
                }
            }
        }
    }
    Value::Object(out)
}

fn highlight_fields(body: &Value) -> Vec<String> {
    let mut out = vec![];
    if let Some(fields) = body
        .get("highlight")
        .and_then(|h| h.get("fields"))
        .and_then(|f| f.as_object())
    {
        for f in fields.keys() {
            out.push(f.clone());
        }
    }
    out.sort();
    out
}

pub(crate) fn highlight_fragment(text: &str, tokens: &HashSet<String>) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| {
            let clean = w
                .trim_matches(|c| matches!(c, '.' | ',' | '!' | '?'))
                .to_string();
            let clean = crate::store::simple_lower(&clean);
            if tokens.contains(&clean) {
                format!("<em>{}</em>", w)
            } else {
                w.to_string()
            }
        })
        .collect();
    words.join(" ")
}

fn collect_tokens(query: &Map<String, Value>, out: &mut HashSet<String>) {
    for (clause, raw) in query {
        match clause.as_str() {
            "match" | "match_phrase" => {
                if let Some(mm) = raw.as_object() {
                    if let Some((_, val)) = single_entry(mm) {
                        let (text, _) = query_text(val);
                        for t in analyze(&text) {
                            out.insert(t);
                        }
                    }
                }
            }
            "multi_match" => {
                if let Some(q) = raw.as_object().and_then(|m| m.get("query")) {
                    for t in analyze(&value_to_string(q).unwrap_or_default()) {
                        out.insert(t);
                    }
                }
            }
            "bool" => {
                if let Some(bm) = raw.as_object() {
                    for key in ["must", "should", "filter"] {
                        for q in as_query_list(bm.get(key)) {
                            if let Some(qm) = q.as_object() {
                                collect_tokens(qm, out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn script_field_error(reason: &str) -> ApiError {
    let mut e = ApiError::new(
        "search_phase_execution_exception",
        "all shards failed",
        400,
    );
    let cause = ApiError::new("script_exception", reason, 400)
        .field("lang", json!("painless"));
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e.field("lang", json!("painless"))
}

fn eval_script_field(doc: &Value, idx: &Index, sf: &Value) -> Result<Vec<Value>, ApiError> {
    let (src, params) = match sf {
        Value::String(s) => (s.clone(), json!({})),
        Value::Object(m) => {
            let script = m.get("script").cloned().unwrap_or(Value::Null);
            match script {
                Value::String(s) => (s, json!({})),
                Value::Object(so) => (
                    so.get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    so.get("params").cloned().unwrap_or(json!({})),
                ),
                _ => (String::new(), json!({})),
            }
        }
        _ => (String::new(), json!({})),
    };
    let src = src.trim().to_string();
    let missing = || script_field_error("runtime error");
    let field_name = |s: &str| -> Option<String> {
        let a = s.find("doc['")?;
        let rest = &s[a + 5..];
        let b = rest.find("']")?;
        Some(rest[..b].to_string())
    };
    if src == "null" {
        return Ok(vec![Value::Null]);
    }
    if src.starts_with('[') && src.ends_with(']') {
        let inner = &src[1..src.len() - 1];
        if let Some((k, v)) = inner.split_once(':') {
            let key = k.trim().trim_matches(|c| c == '\'' || c == '"');
            let val: Value = if let Ok(n) = v.trim().parse::<f64>() {
                json!(n)
            } else {
                json!(v.trim().trim_matches(|c| c == '\'' || c == '"'))
            };
            let mut m = Map::new();
            m.insert(key.to_string(), val);
            return Ok(vec![Value::Object(m)]);
        }
        let vals: Vec<Value> = inner
            .split(',')
            .filter_map(|x| x.trim().parse::<f64>().ok().map(|n| json!(n)))
            .collect();
        return Ok(vals);
    }
    if let Some(rest) = src.strip_prefix("params['_source']") {
        let mut cur = doc;
        let mut tail = rest;
        while let Some(a) = tail.find("['") {
            let r = &tail[a + 2..];
            let Some(b) = r.find("']") else { break };
            let key = &r[..b];
            cur = cur.get(key).unwrap_or(&Value::Null);
            tail = &r[b + 2..];
        }
        return Ok(vec![cur.clone()]);
    }
    if src.starts_with("params.x") {
        let p = params.get("x").cloned().unwrap_or(Value::Null);
        if let Some(f) = field_name(&src) {
            let v = field_value(doc, &f)
                .and_then(|x| flatten_any(x).into_iter().next())
                .unwrap_or(Value::Null);
            let s = format!(
                "{}{}",
                value_to_string(&p).unwrap_or_default(),
                value_to_string(&v).unwrap_or_default()
            );
            return Ok(vec![json!(s)]);
        }
        return Ok(vec![p]);
    }
    if let Some(f) = field_name(&src) {
        if src.contains(".size()") {
            let n = field_value(doc, &f)
                .map(|raw| flatten_any(raw).len() as i64)
                .unwrap_or(0);
            return Ok(vec![json!(n)]);
        }
        let Some(raw) = field_value(doc, &f) else {
            return Err(missing());
        };
        let list = flatten_any(raw);
        if src.contains("* 2") {
            let n = list.first().and_then(to_f64).unwrap_or(0.0);
            return Ok(vec![json!(n * 2.0)]);
        }
        if !src.contains(".value") {
            return Ok(list);
        }
        let first = list.first().cloned().unwrap_or(Value::Null);
        return Ok(vec![first]);
    }
    // Bare literals
    if let Ok(n) = src.parse::<f64>() {
        return Ok(vec![json!(n)]);
    }
    Ok(vec![json!(src)])
}

fn strip_query_names(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                if k == "_name" {
                    continue;
                }
                out.insert(k.clone(), strip_query_names(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(strip_query_names).collect()),
        other => other.clone(),
    }
}

fn find_query_name(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(n) = m.get("_name").and_then(|x| x.as_str()) {
                return Some(n.to_string());
            }
            for val in m.values() {
                if let Some(n) = find_query_name(val) {
                    return Some(n);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(find_query_name),
        _ => None,
    }
}

fn walk_named_queries(
    out: &mut Vec<(String, f64)>,
    idx: &Index,
    id: &str,
    doc: &Value,
    extra: &HashMap<String, Map<String, Value>>,
    q: &Value,
) -> Result<(), ApiError> {
    let Some(m) = q.as_object() else { return Ok(()) };
    let compound = m.contains_key("bool")
        || m.contains_key("constant_score")
        || m.contains_key("function_score");
    if !compound {
        if let Some(name) = find_query_name(q) {
            let clean = strip_query_names(q);
            let cm = clean.as_object().cloned().unwrap_or_default();
            let (ok, score) = eval_doc(idx, id, doc, extra, &cm)?;
            if ok {
                out.push((name, score));
            }
        }
    }
    if let Some(b) = m.get("bool").and_then(|v| v.as_object()) {
        if let Some(name) = b.get("_name").and_then(|v| v.as_str()) {
            let clean = strip_query_names(q);
            let cm = clean.as_object().cloned().unwrap_or_default();
            let (ok, score) = eval_doc(idx, id, doc, extra, &cm)?;
            if ok {
                out.push((name.to_string(), score));
            }
        }
        for key in ["must", "should", "filter"] {
            if let Some(clauses) = b.get(key) {
                match clauses {
                    Value::Array(a) => {
                        for c in a {
                            walk_named_queries(out, idx, id, doc, extra, c)?;
                        }
                    }
                    other => walk_named_queries(out, idx, id, doc, extra, other)?,
                }
            }
        }
        return Ok(());
    }
    if let Some(cs) = m.get("constant_score").and_then(|v| v.as_object()) {
        if let Some(name) = cs.get("_name").and_then(|v| v.as_str()) {
            let clean = strip_query_names(q);
            let cm = clean.as_object().cloned().unwrap_or_default();
            let (ok, score) = eval_doc(idx, id, doc, extra, &cm)?;
            if ok {
                out.push((name.to_string(), score));
            }
        }
        if let Some(f) = cs.get("filter") {
            walk_named_queries(out, idx, id, doc, extra, f)?;
        }
        return Ok(());
    }
    if let Some(fs) = m.get("function_score").and_then(|v| v.as_object()) {
        if let Some(name) = fs.get("_name").and_then(|v| v.as_str()) {
            let clean = strip_query_names(q);
            let cm = clean.as_object().cloned().unwrap_or_default();
            let (ok, score) = eval_doc(idx, id, doc, extra, &cm)?;
            if ok {
                out.push((name.to_string(), score));
            }
        }
        if let Some(f) = fs.get("query") {
            walk_named_queries(out, idx, id, doc, extra, f)?;
        }
        return Ok(());
    }
    Ok(())
}

fn apply_rescore(
    store: &Store,
    matched: &mut [MultiHit],
    raw: &Value,
) -> Result<(), ApiError> {
    let configs: Vec<Value> = match raw {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    for cfg in configs {
        let Some(cm) = cfg.as_object() else { continue };
        let Some(qm) = cm.get("query").and_then(|v| v.as_object()) else { continue };
        let Some(rqv) = qm.get("rescore_query") else { continue };
        let Some(rq) = rqv.as_object() else { continue };
        let window = cm.get("window_size").and_then(to_f64).map(|n| n as usize).unwrap_or(10);
        let mode = qm.get("score_mode").and_then(|v| v.as_str()).unwrap_or("total");
        let qw = qm.get("query_weight").and_then(to_f64).unwrap_or(1.0);
        let rw = qm.get("rescore_query_weight").and_then(to_f64).unwrap_or(1.0);
        let mut extras: HashMap<String, HashMap<String, Map<String, Value>>> = HashMap::new();
        for h in matched.iter_mut().take(window) {
            let idx = &store.indices[&h.idx_name];
            let extra = extras
                .entry(h.idx_name.clone())
                .or_insert_with(|| precompute_doc_values(idx));
            let (ok, rs) = eval_doc(idx, &h.id, &h.doc, extra, rq)?;
            let base = h.score;
            let combined = if !ok {
                match mode {
                    "multiply" | "avg" | "max" | "min" => base,
                    _ => base * qw,
                }
            } else {
                match mode {
                    "multiply" => (base * qw) * (rs * rw),
                    "avg" => (base * qw + rs * rw) / 2.0,
                    "max" => (base * qw).max(rs * rw),
                    "min" => (base * qw).min(rs * rw),
                    _ => base * qw + rs * rw,
                }
            };
            h.score = combined;
        }
    }
    Ok(())
}

fn collapse_value(doc: &Value, field: &str) -> Option<Value> {
    let v = field_value(doc, field)?;
    match v {
        Value::Array(a) => a.first().cloned(),
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn ih_collect_offsets(v: &Value, path: &str) -> Vec<(usize, Value)> {
    if path.is_empty() {
        return match v {
            Value::Array(a) => a.iter().cloned().enumerate().collect(),
            other => vec![(0, other.clone())],
        };
    }
    if let Some(arr) = v.as_array() {
        let mut out = Vec::new();
        for e in arr {
            out.extend(ih_collect_offsets(e, path));
        }
        return out;
    }
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, r),
        None => (path, ""),
    };
    match v.get(head) {
        Some(next) => ih_collect_offsets(next, rest),
        None => Vec::new(),
    }
}

fn ih_scoped_ctx<'a>(
    idx: &'a Index,
    id: &str,
    doc: &'a Value,
    path: &str,
    child: &Value,
) -> QueryCtx<'a> {
    let mut vals = Map::new();
    crate::eval_extra::flatten_scope(path, child, &mut vals);
    let mut cc = QueryCtx::with_values(idx, id, doc, vals);
    cc.scope_path = Some(path.to_string());
    cc.scope_obj = Some(child.clone());
    cc
}

fn ih_source(child: &Value, path: &str, spec: Option<&Value>) -> Value {
    let includes = match spec {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
        Some(Value::String(s)) => vec![s.as_str()],
        _ => return child.clone(),
    };
    if includes.is_empty() {
        return child.clone();
    }
    let mut out = Map::new();
    for inc in includes {
        if inc == path {
            return child.clone();
        }
        if let Some(rel) = inc.strip_prefix(&format!("{}.", path)) {
            if let Some(v) = child.get(rel) {
                out.insert(rel.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

fn ih_sort_fields(spec: Option<&Value>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let arr = match spec {
        Some(Value::Array(a)) => a.clone(),
        Some(other) => vec![other.clone()],
        None => return out,
    };
    for item in arr {
        match &item {
            Value::String(f) => out.push((f.clone(), false)),
            Value::Object(m) => {
                for (f, cfg) in m {
                    let desc = match cfg {
                        Value::String(s) => s == "desc",
                        Value::Object(o) => {
                            o.get("order").and_then(|v| v.as_str()) == Some("desc")
                        }
                        _ => false,
                    };
                    out.push((f.clone(), desc));
                }
            }
            _ => {}
        }
    }
    out
}

fn ih_sort_value(ctx: &QueryCtx, field: &str) -> Value {
    let (vals, _) = ctx.values(field);
    vals.into_iter().next().unwrap_or(Value::Null)
}

fn ih_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    let an = a.as_f64();
    let bn = b.as_f64();
    match (an, bn) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a
            .as_str()
            .unwrap_or("")
            .cmp(b.as_str().unwrap_or("")),
    }
}

fn ih_build_group(
    mut hits: Vec<(usize, Value, Vec<Value>, Map<String, Value>)>,
    path: &str,
    spec: &Map<String, Value>,
    outer: Option<(&str, usize)>,
    source_spec: Option<&Value>,
    id: &str,
) -> Value {
    let sorts = ih_sort_fields(spec.get("sort"));
    if !sorts.is_empty() {
        hits.sort_by(|a, b| {
            for i in 0..sorts.len() {
                let mut c = ih_cmp(&a.2[i], &b.2[i]);
                if sorts[i].1 {
                    c = c.reverse();
                }
                if c != std::cmp::Ordering::Equal {
                    return c;
                }
            }
            a.0.cmp(&b.0)
        });
    }
    let from = spec.get("from").and_then(to_f64).map(|n| n as usize).unwrap_or(0);
    let size = spec.get("size").and_then(to_f64).map(|n| n as usize).unwrap_or(3);
    let total = hits.len();
    let mut inner = Vec::new();
    for (offset, child, svals, mut extra) in hits.into_iter().skip(from).take(size) {
        let mut e = Map::new();
        if let Some(v) = extra.remove("_id") {
            e.insert("_id".to_string(), v);
        } else {
            e.insert("_id".to_string(), json!(id));
        }
        if !path.is_empty() {
            let meta = match outer {
                Some((opath, ooff)) => {
                    let mut m = Map::new();
                    m.insert("field".to_string(), json!(opath));
                    m.insert("offset".to_string(), json!(ooff));
                    let mut sub = Map::new();
                    sub.insert(
                        "field".to_string(),
                        json!(path.rsplit('.').next().unwrap_or(path)),
                    );
                    sub.insert("offset".to_string(), json!(offset));
                    m.insert("_nested".to_string(), Value::Object(sub));
                    Value::Object(m)
                }
                None => json!({"field": path, "offset": offset}),
            };
            e.insert("_nested".to_string(), meta);
        }
        e.insert(
            "_source".to_string(),
            ih_source(&child, path, source_spec),
        );
        if !svals.is_empty() {
            e.insert("sort".to_string(), Value::Array(svals));
        }
        if let Some(r) = extra.remove("_routing") {
            e.insert("_routing".to_string(), r);
        }
        if let Some(ih) = extra.remove("inner_hits") {
            e.insert("inner_hits".to_string(), ih);
        }
        inner.push(Value::Object(e));
    }
    let mut hits_obj = Map::new();
    hits_obj.insert("total".to_string(), json!({"value": total, "relation": "eq"}));
    hits_obj.insert("hits".to_string(), Value::Array(inner));
    let mut group = Map::new();
    group.insert("hits".to_string(), Value::Object(hits_obj));
    Value::Object(group)
}

fn find_percolate_map<'a>(q: &'a Map<String, Value>) -> Option<&'a Map<String, Value>> {
    if let Some(p) = q.get("percolate").and_then(|v| v.as_object()) {
        return Some(p);
    }
    for key in ["bool", "constant_score", "function_score"] {
        if let Some(bm) = q.get(key).and_then(|v| v.as_object()) {
            for (k, v) in bm {
                if k == "query" {
                    if let Some(sub) = v.as_object() {
                        if let Some(p) = find_percolate_map(sub) {
                            return Some(p);
                        }
                    }
                } else if matches!(k.as_str(), "must" | "should" | "filter") {
                    let items: Vec<&Value> = match v {
                        Value::Array(a) => a.iter().collect(),
                        other => vec![other],
                    };
                    for item in items {
                        if let Some(sub) = item.as_object() {
                            if let Some(p) = find_percolate_map(sub) {
                                return Some(p);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn build_query_inner_hits(
    idx: &Index,
    id: &str,
    doc: &Value,
    q: &Map<String, Value>,
    scope_path: Option<&str>,
    scope_obj: Option<&Value>,
    outer: Option<(&str, usize)>,
) -> Result<Map<String, Value>, ApiError> {
    let mut out = Map::new();
    for (key, val) in q {
        if key == "nested" {
            let Some(bm) = val.as_object() else { continue };
            let path = bm.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                continue;
            }
            let inner = bm.get("query").and_then(|v| v.as_object());
            let children = if let Some(sp) = scope_path {
                if path.starts_with(&format!("{}.", sp)) {
                    match scope_obj {
                        Some(o) => ih_collect_offsets(o, &path[sp.len() + 1..]),
                        None => Vec::new(),
                    }
                } else {
                    ih_collect_offsets(doc, path)
                }
            } else {
                ih_collect_offsets(doc, path)
            };
            let spec = bm.get("inner_hits").and_then(|v| v.as_object());
            let sorts = spec
                .map(|s| ih_sort_fields(s.get("sort")))
                .unwrap_or_default();
            let mut matched = Vec::new();
            for (offset, child) in children {
                if !child.is_object() {
                    continue;
                }
                let cc = ih_scoped_ctx(idx, id, doc, path, &child);
                let mut extra = Map::new();
                if let Some(inner) = inner {
                    let (ok, _) = cc.eval(inner)?;
                    if !ok {
                        continue;
                    }
                    let sub = build_query_inner_hits(
                        idx,
                        id,
                        doc,
                        inner,
                        Some(path),
                        Some(&child),
                        Some((path, offset)),
                    )?;
                    if !sub.is_empty() {
                        extra.insert("inner_hits".to_string(), Value::Object(sub));
                    }
                }
                let mut svals = Vec::new();
                for (f, _) in &sorts {
                    svals.push(ih_sort_value(&cc, f));
                }
                matched.push((offset, child, svals, extra));
            }
            if let Some(spec) = spec {
                let name = spec
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(path)
                    .to_string();
                let group = ih_build_group(
                    matched,
                    path,
                    spec,
                    outer,
                    spec.get("_source"),
                    id,
                );
                out.insert(name, group);
            }
            continue;
        }
        if key == "has_child" {
            let Some(bm) = val.as_object() else { continue };
            let Some(spec) = bm.get("inner_hits").and_then(|v| v.as_object()) else {
                continue;
            };
            let child_type = bm.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let inner = bm.get("query").and_then(|v| v.as_object());
            let field = match crate::eval_extra::find_join_field(idx) {
                Some(f) => f,
                None => continue,
            };
            let sorts = ih_sort_fields(spec.get("sort"));
            let mut matched = Vec::new();
            for cid in &idx.order {
                let Some(cdoc) = idx.docs.get(cid) else { continue };
                let (name, parent) = crate::eval_extra::join_parts(cdoc.get(&field));
                if parent != id || (!child_type.is_empty() && name != child_type) {
                    continue;
                }
                let cc = QueryCtx::new(idx, cid, cdoc);
                if let Some(inner) = inner {
                    let (ok, _) = cc.eval(inner)?;
                    if !ok {
                        continue;
                    }
                }
                let mut svals = Vec::new();
                for (f, _) in &sorts {
                    svals.push(ih_sort_value(&cc, f));
                }
                let mut extra = Map::new();
                extra.insert("_id".to_string(), json!(cid));
                if let Some(r) = idx.routing.get(cid) {
                    extra.insert("_routing".to_string(), json!(r));
                }
                matched.push((0usize, cdoc.clone(), svals, extra));
            }
            let name = spec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(child_type)
                .to_string();
            let group = ih_build_group(
                matched,
                "",
                spec,
                None,
                spec.get("_source"),
                id,
            );
            out.insert(name, group);
            continue;
        }
        if key == "has_parent" {
            let Some(bm) = val.as_object() else { continue };
            let Some(spec) = bm.get("inner_hits").and_then(|v| v.as_object()) else {
                continue;
            };
            let ptype = bm
                .get("parent_type")
                .and_then(|v| v.as_str())
                .or_else(|| bm.get("type").and_then(|v| v.as_str()))
                .unwrap_or("");
            let pid = idx.routing.get(id).cloned().unwrap_or_default();
            let Some(pdoc) = idx.docs.get(&pid) else { continue };
            let field = match crate::eval_extra::find_join_field(idx) {
                Some(f) => f,
                None => continue,
            };
            let (name, _) = crate::eval_extra::join_parts(pdoc.get(&field));
            if !ptype.is_empty() && name != ptype {
                continue;
            }
            if let Some(inner) = bm.get("query").and_then(|v| v.as_object()) {
                let cc = QueryCtx::new(idx, &pid, pdoc);
                let (ok, _) = cc.eval(inner)?;
                if !ok {
                    continue;
                }
            }
            let mut hit = Map::new();
            hit.insert("_id".to_string(), json!(pid));
            hit.insert("_source".to_string(), pdoc.clone());
            let mut hits_obj = Map::new();
            hits_obj.insert("total".to_string(), json!({"value": 1, "relation": "eq"}));
            hits_obj.insert("hits".to_string(), Value::Array(vec![Value::Object(hit)]));
            let mut group = Map::new();
            group.insert("hits".to_string(), Value::Object(hits_obj));
            let gname = spec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(ptype)
                .to_string();
            out.insert(gname, Value::Object(group));
            continue;
        }
        if matches!(key.as_str(), "bool" | "constant_score" | "function_score") {
            if let Some(bm) = val.as_object() {
                for (k2, v2) in bm {
                    if k2 == "query" {
                        if let Some(sub) = v2.as_object() {
                            let got = build_query_inner_hits(
                                idx, id, doc, sub, scope_path, scope_obj, outer,
                            )?;
                            for (k, v) in got {
                                out.insert(k, v);
                            }
                        }
                    } else if matches!(k2.as_str(), "must" | "should" | "filter") {
                        let arr = match v2 {
                            Value::Array(a) => a.clone(),
                            other => vec![other.clone()],
                        };
                        for item in arr {
                            if let Some(sub) = item.as_object() {
                                let got = build_query_inner_hits(
                                    idx, id, doc, sub, scope_path, scope_obj, outer,
                                )?;
                                for (k, v) in got {
                                    out.insert(k, v);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn build_collapse_inner_hits(
    store: &Store,
    pool: &[MultiHit],
    field: &str,
    val: &Option<Value>,
    raw: &Value,
) -> Result<Map<String, Value>, ApiError> {
    let configs: Vec<Value> = match raw {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let mut out = Map::new();
    for cfg in configs {
        let cm = cfg.as_object().cloned().unwrap_or_default();
        let name = cm
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let size = cm.get("size").and_then(to_f64).map(|n| n as usize).unwrap_or(3);
        let specs = parse_sort_specs(cm.get("sort"))?;
        let source_false = matches!(cm.get("_source"), Some(Value::Bool(false)));
        let mut group: Vec<&MultiHit> = pool
            .iter()
            .filter(|h| &collapse_value(&h.doc, field) == val)
            .collect();
        if !specs.is_empty() {
            let mut keyed: Vec<(Vec<(bool, Value)>, &MultiHit)> = Vec::with_capacity(group.len());
            for h in group {
                let mut cells = Vec::with_capacity(specs.len());
                for sp in &specs {
                    cells.push(compute_sort_cell(store, h, sp)?);
                }
                keyed.push((cells, h));
            }
            keyed.sort_by(|a, b| {
                for i in 0..specs.len() {
                    let ca = &a.0[i];
                    let cb = &b.0[i];
                    let mut c = cmp_sort_cells(ca, cb);
                    if specs[i].desc && !ca.0 && !cb.0 {
                        c = c.reverse();
                    }
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            });
            group = keyed.into_iter().map(|(_, h)| h).collect();
        }
        let total = group.len();
        group.truncate(size);
        let mut inner = Vec::new();
        for h in group {
            let mut e = Map::new();
            e.insert("_index".to_string(), json!(h.idx_name));
            e.insert("_id".to_string(), json!(h.id));
            e.insert(
                "_score".to_string(),
                if specs.is_empty() { json!(h.score) } else { Value::Null },
            );
            if !specs.is_empty() {
                let mut svals = Vec::with_capacity(specs.len());
                for sp in &specs {
                    svals.push(compute_sort_cell(store, h, sp)?.1);
                }
                e.insert("sort".to_string(), Value::Array(svals));
            }
            if !source_false {
                e.insert("_source".to_string(), h.doc.clone());
            }
            inner.push(Value::Object(e));
        }
        let mut body = Map::new();
        body.insert("total".to_string(), json!({"value": total, "relation": "eq"}));
        body.insert("max_score".to_string(), Value::Null);
        body.insert("hits".to_string(), Value::Array(inner));
        out.insert(name, json!({"hits": body}));
    }
    Ok(out)
}

// ------------------------------------------------------------------ run search

fn resolve_terms_lookups(store: &Store, v: &mut Value) -> Result<(), ApiError> {
    match v {
        Value::Object(m) => {
            if let Some(terms) = m.get_mut("terms").and_then(|t| t.as_object_mut()) {
                let keys: Vec<String> = terms.keys().cloned().collect();
                for field in keys {
                    let lookup = match terms.get(&field).and_then(|x| x.as_object()) {
                        Some(o) if o.contains_key("index") && o.contains_key("id") => o.clone(),
                        _ => continue,
                    };
                    let index = lookup
                        .get("index")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let id = lookup
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let path = lookup
                        .get("path")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let idx = match store.indices.get(&index) {
                        Some(i) => i,
                        None => {
                            return Err(ApiError::new(
                                "index_not_found_exception",
                                &format!("no such index [{}]", index),
                                404,
                            ))
                        }
                    };
                    let mut vals = Vec::new();
                    if let Some(doc) = idx.docs.get(&id) {
                        let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
                        lookup_path_values(doc, &parts, &mut vals);
                    }
                    terms.insert(field, Value::Array(vals));
                }
            }
            for (_, child) in m.iter_mut() {
                resolve_terms_lookups(store, child)?;
            }
        }
        Value::Array(a) => {
            for child in a.iter_mut() {
                resolve_terms_lookups(store, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn lookup_path_values(v: &Value, parts: &[&str], out: &mut Vec<Value>) {
    if parts.is_empty() {
        match v {
            Value::Array(a) => {
                for e in a {
                    lookup_path_values(e, parts, out);
                }
            }
            Value::Null => {}
            other => out.push(other.clone()),
        }
        return;
    }
    match v {
        Value::Object(m) => {
            if let Some(next) = m.get(parts[0]) {
                lookup_path_values(next, &parts[1..], out);
            }
        }
        Value::Array(a) => {
            for e in a {
                lookup_path_values(e, parts, out);
            }
        }
        _ => {}
    }
}

pub fn run_search(
    store: &Store,
    url_name: &str,
    names: &[String],
    body: &Value,
    req: &Req,
) -> Result<Value, ApiError> {
    let mut bodym = body.as_object().cloned().unwrap_or_default();
    if let Some(usort) = req.param("sort") {
        let mut parsed = Vec::new();
        let mut valid = true;
        for part in usort.split(',').filter(|p| !p.is_empty()) {
            let (f, o) = match part.split_once(':') {
                Some((f, o)) => (f, o),
                None => (part, "asc"),
            };
            if f.is_empty() || (o != "asc" && o != "desc") {
                valid = false;
                break;
            }
            let mut m = Map::new();
            m.insert("order".to_string(), json!(o));
            let mut outer = Map::new();
            outer.insert(f.to_string(), Value::Object(m));
            parsed.push(Value::Object(outer));
        }
        if valid && !parsed.is_empty() {
            let existing = bodym.remove("sort");
            let mut arr = match existing {
                Some(Value::Array(a)) => a,
                Some(other) => vec![other],
                None => vec![],
            };
            arr.extend(parsed);
            bodym.insert("sort".to_string(), Value::Array(arr));
        }
    }

    let mut query = bodym.get("query").and_then(|q| q.as_object()).cloned();
    if let Some(qp) = req.param("q") {
        let mut depth = 0i32;
        let mut quoted = false;
        let mut bad = false;
        for c in qp.chars() {
            if c == '"' {
                quoted = !quoted;
            } else if !quoted && c == '(' {
                depth += 1;
            } else if !quoted && c == ')' {
                depth -= 1;
                if depth < 0 {
                    bad = true;
                }
            }
        }
        if bad || depth != 0 || quoted {
            return Err(sort_shard_error(
                "query_shard_exception",
                &format!("Failed to parse query [{}]", qp),
            ));
        }
        let mut qv = parse_q_param(qp);
        if let Some((f, raw)) = qp.split_once(':') {
            let plain = !raw.starts_with('[')
                && !raw.starts_with('{')
                && !raw.starts_with('>')
                && !raw.starts_with('<')
                && !raw.starts_with('(');
            if plain {
                let mut numeric_mismatch = false;
                let mut reaches_nested = false;
                for n in names {
                    let idx = &store.indices[n];
                    if nested_parent_of(idx, f).is_some() {
                        reaches_nested = true;
                    }
                    if let Some(d) = mapping_field_def(idx, f) {
                        let ft = d.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if matches!(ft, "integer" | "short" | "byte" | "long" | "float" | "double" | "half_float" | "scaled_float")
                            && raw.trim().parse::<f64>().is_err()
                        {
                            numeric_mismatch = true;
                        }
                    }
                }
                if reaches_nested || numeric_mismatch {
                    qv = json!({"match_none": {}});
                }
            }
        }
        if let Some(mm) = qv.get_mut("multi_match") {
            if let Some(o) = mm.as_object_mut() {
                if let Some(op) = req.param("default_operator") {
                    o.insert("default_operator".to_string(), json!(op));
                }
                if let Some(df) = req.param("df") {
                    o.insert("fields".to_string(), json!([df]));
                }
            }
        }
        if let Some(m) = qv.as_object() {
            query = Some(m.clone());
        }
    }
    let query: Map<String, Value> = match query {
        Some(m) => {
            let mut qv = Value::Object(m);
            resolve_terms_lookups(store, &mut qv)?;
            qv.as_object().cloned().unwrap_or_default()
        }
        None => Map::new(),
    };
    for n in names {
        if let Some(idx) = store.indices.get(n) {
            crate::eval::validate_query(idx, &query)?;
        }
    }

    let mut from = bodym
        .get("from")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(0);
    let mut size = bodym
        .get("size")
        .and_then(to_f64)
        .map(|f| f as i64)
        .unwrap_or(10);
    if let Some(v) = req.param("from") {
        if let Ok(n) = v.parse::<i64>() {
            from = n;
        }
    }
    if let Some(v) = req.param("size") {
        if let Ok(n) = v.parse::<i64>() {
            size = n;
        }
    }
    if from < 0 {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!("[from] parameter cannot be negative, found [{}]", from),
            400,
        ));
    }
    if size < 0 {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!("[size] parameter cannot be negative, found [{}]", size),
            400,
        ));
    }
    for n in names {
        let idx = &store.indices[n];
        let mut window = 10000i64;
        if let Some(v) = find_setting(&idx.settings, "max_result_window") {
            if let Some(n) = settings_value_int(v) {
                window = n;
            }
        }
        if from + size > window {
            if req.param("scroll").is_some() && size > window {
                let reason = format!(
                    "Batch size is too large, size must be less than or equal to: [{}] but was [{}]. Scroll batch sizes cost as much memory as result windows so they are controlled by the [index.max_result_window] index level setting.",
                    window, size
                );
                let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
                e.root_cause = Some(Box::new(ApiError::new(
                    "illegal_argument_exception",
                    &reason,
                    400,
                )));
                return Err(e);
            }
            let reason = format!(
                "Result window is too large, from + size must be less than or equal to: [{}] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.",
                window,
                from + size
            );
            let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
            e.fields.insert("phase".to_string(), json!("query"));
            e.root_cause = Some(Box::new(ApiError::new(
                "illegal_argument_exception",
                &reason,
                400,
            )));
            return Err(e);
        }
    }

    let routing_tokens: Option<HashSet<String>> = req.param("routing").map(|r| {
        r.split(',')
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect()
    });
    let pref_shard: Option<i64> = req
        .param("preference")
        .and_then(|p| p.strip_prefix("_shards:"))
        .and_then(|n| n.parse::<i64>().ok());
    let terminate_after: Option<i64> = bodym
        .get("terminate_after")
        .and_then(to_f64)
        .map(|n| n as i64)
        .or_else(|| req.param("terminate_after").and_then(|v| v.parse::<i64>().ok()));
    let mut matched: Vec<MultiHit> = vec![];
    let mut shards = 0i64;
    for n in names {
        let idx = &store.indices[n];
        if let Some(e) = shared::read_block_error(idx) {
            return Err(e);
        }
        let nsh = idx.num_shards();
        if let Some(tokens) = &routing_tokens {
            let mut used = HashSet::new();
            for t in tokens {
                used.insert(routing_shard_of(t, nsh));
            }
            shards += used.len() as i64;
        } else if let Some(ps) = pref_shard {
            shards += if ps < nsh { 1 } else { 0 };
        } else {
            shards += nsh;
        }
        let mut qm = query.clone();
        if let Some(f) = search_alias_filter(idx, url_name) {
            qm = Map::new();
            qm.insert(
                "bool".to_string(),
                json!({"must": [Value::Object(query.clone()), f]}),
            );
        }
        let extra = precompute_doc_values(idx);
        for id in &idx.order {
            let doc = match idx.docs.get(id) {
                Some(d) => d,
                None => continue,
            };
            if idx.pending.contains_key(id) {
                continue;
            }
            let rid = idx.routing.get(id).map(|s| s.as_str()).unwrap_or("");
            if let Some(tokens) = &routing_tokens {
                if !rid.is_empty() {
                    let sh = routing_shard_of(rid, nsh);
                    if !tokens.iter().any(|t| routing_shard_of(t, nsh) == sh) {
                        continue;
                    }
                }
            }
            if let Some(ps) = pref_shard {
                let shard = if rid.is_empty() {
                    idx.order.iter().position(|x| x == id).unwrap_or(0) as i64 % nsh
                } else {
                    routing_shard_of(rid, nsh)
                };
                if shard != ps {
                    continue;
                }
            }
            let (ok, score) = eval_doc(idx, id, doc, &extra, &qm)?;
            if ok {
                let pos = store.indices[n]
                    .order
                    .iter()
                    .position(|x| x == id)
                    .unwrap_or(0) as i64;
                let mut named = Vec::new();
                walk_named_queries(&mut named, idx, id, doc, &extra, &Value::Object(qm.clone()))?;
                matched.push(MultiHit {
                    idx_name: n.clone(),
                    id: id.clone(),
                    doc: doc.clone(),
                    score,
                    pos,
                    named,
                });
            }
        }
    }
    if let Some(ib) = bodym.get("indices_boost") {
        let mut entries: Vec<(String, f64)> = Vec::new();
        match ib {
            Value::Array(list) => {
                for item in list {
                    if let Some(m) = item.as_object() {
                        for (k, v) in m {
                            entries.push((k.clone(), to_f64(v).unwrap_or(1.0)));
                        }
                    }
                }
            }
            Value::Object(m) => {
                for (k, v) in m {
                    entries.push((k.clone(), to_f64(v).unwrap_or(1.0)));
                }
            }
            _ => {}
        }
        for (pattern, boost) in entries {
            if pattern.contains('*') || pattern.contains('?') {
                let re = wildcard_regex(&pattern);
                for h in matched.iter_mut() {
                    if re.is_match(&h.idx_name) {
                        h.score *= boost;
                    }
                }
            } else if names.iter().any(|n| n == &pattern) {
                for h in matched.iter_mut() {
                    if h.idx_name == pattern {
                        h.score *= boost;
                    }
                }
            } else {
                return Err(ApiError::not_found(&pattern));
            }
        }
    }
    let mut terminated_early: Option<bool> = None;
    if let Some(ta) = terminate_after {
        if ta > 0 {
            terminated_early = Some(matched.len() as i64 > ta);
            if matched.len() as i64 > ta {
                matched.truncate(ta as usize);
            }
        }
    }
    if let Some(ms) = bodym.get("min_score").and_then(to_f64) {
        matched.retain(|h| h.score >= ms);
    }
    if let Some(rc) = bodym.get("rescore") {
        apply_rescore(store, &mut matched, rc)?;
    }
    let agg_matched = matched.clone();
    if let Some(pf) = bodym.get("post_filter") {
        let mut kept = Vec::new();
        for mut h in matched.drain(..) {
            let idx = &store.indices[&h.idx_name];
            let extra = precompute_doc_values(idx);
            let pm = pf.as_object().cloned().unwrap_or_default();
            let (ok, _) = eval_doc(idx, &h.id, &h.doc, &extra, &pm)?;
            if ok {
                walk_named_queries(&mut h.named, idx, &h.id, &h.doc, &extra, pf)?;
                kept.push(h);
            }
        }
        matched = kept;
    }
    let mut total = matched.len();

    let collapse_cfg = bodym.get("collapse").and_then(|v| v.as_object()).cloned();
    if let Some(c) = &collapse_cfg {
        let field = c.get("field").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if bodym.get("rescore").is_some() {
            let reason = "cannot use `collapse` in conjunction with `rescore`";
            let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
            e.root_cause = Some(Box::new(ApiError::new("search_exception", reason, 500)));
            return Err(e);
        }
        if let Some(ih) = c.get("inner_hits") {
            let configs: Vec<&Value> = match ih {
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            for cfg in configs {
                if cfg.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        "Field name cannot be null",
                        400,
                    ));
                }
            }
        }
        let mut ftype: Option<String> = None;
        for n in names {
            if let Some(d) = mapping_field_def(&store.indices[n], &field) {
                if ftype.is_none() {
                    ftype = d
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string());
                }
            }
        }
        let Some(ftype) = ftype else {
            let reason = format!("no mapping found for `{}` in order to collapse on", field);
            let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
            e.root_cause = Some(Box::new(ApiError::new(
                "query_shard_exception",
                &reason,
                400,
            )));
            return Err(e);
        };
        if text_like_type(&ftype) {
            let reason = format!(
                "unknown type for collapse field `{}`, only keywords and numbers are accepted",
                field
            );
            let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
            e.root_cause = Some(Box::new(ApiError::new(
                "illegal_argument_exception",
                &reason,
                400,
            )));
            return Err(e);
        }
        for h in &matched {
            if let Some(Value::Array(a)) = field_value(&h.doc, &field) {
                if a.len() > 1 {
                    let reason = "the collapse field must be single valued";
                    let mut e =
                        ApiError::new("search_phase_execution_exception", "all shards failed", 500);
                    e.root_cause = Some(Box::new(ApiError::new(
                        "illegal_state_exception",
                        reason,
                        500,
                    )));
                    return Err(e);
                }
            }
        }
    }

    let specs = parse_sort_specs(bodym.get("sort"))?;
    if !specs.is_empty() && bodym.get("rescore").is_some() {
        return Err(sort_shard_error(
            "illegal_argument_exception",
            "Cannot use [sort] option in conjunction with [rescore].",
        ));
    }
    for sp in &specs {
        if sp.kind == 0 && !sp.field.is_empty() {
            for n in names {
                if let Some(idx) = store.indices.get(n) {
                    if let Some(def) = mapping_field_def(idx, &sp.field) {
                        if def.get("doc_values").and_then(|v| v.as_bool()) == Some(false) {
                            let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            let reason = format!(
                                "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.",
                                sp.field, ftype
                            );
                            return Err(search_phase_error(&reason));
                        }
                    }
                }
            }
        }
        if sp.kind == 6 && sp.geo_ignore {
            continue;
        }
        if sp.kind == 6 {
            for n in names {
                if mapping_field_def(&store.indices[n], &sp.field).is_none() {
                    let reason = format!(
                        "failed to find mapper for [{}] for geo distance based sort",
                        sp.field
                    );
                    return Err(sort_shard_error("query_shard_exception", &reason));
                }
            }
        }
    }
    let mut failed_sort: Vec<String> = Vec::new();
    if !specs.is_empty() {
        let allow_partial = req.param("allow_partial_search_results") != Some("false");
        for sp in &specs {
            if sp.kind != 0 {
                continue;
            }
            let mut missing_names = Vec::new();
            let mut text_err = None;
            for n in names {
                let idx = &store.indices[n];
                match sort_mapping_def(idx, &sp.field) {
                    Some(d) => {
                        if matches!(
                            d.get("type").and_then(|t| t.as_str()),
                            Some("rank_feature") | Some("rank_features")
                        ) {
                            return Err(sort_shard_error(
                                "illegal_argument_exception",
                                "[rank_feature] fields do not support sorting, scripting or aggregating",
                            ));
                        }
                        if d.get("type").and_then(|t| t.as_str()) == Some("geo_shape") {
                            return Err(sort_shard_error(
                                "illegal_argument_exception",
                                "can't sort on geo_shape field without using specific sorting feature, like geo_distance",
                            ));
                        }
                        if sp.nested.is_none()
                            && d.get("type").and_then(|t| t.as_str()) == Some("text")
                            && d.get("fielddata").and_then(|v| v.as_bool()) != Some(true)
                            && nested_parent_of(idx, &sp.field).is_none()
                        {
                            let reason = format!(
                                "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                                sp.field
                            );
                            text_err = Some(sort_shard_error("illegal_argument_exception", &reason));
                        }
                    }
                    None => missing_names.push(n.clone()),
                }
            }
            if let Some(e) = text_err {
                return Err(e);
            }
            if !missing_names.is_empty() && sp.unmapped_type.is_none() {
                if names.len() == 1 || !allow_partial {
                    return Err(sort_shard_error(
                        "query_shard_exception",
                        &format!("No mapping found for [{}] in order to sort on", sp.field),
                    ));
                }
                for n in missing_names {
                    if !failed_sort.contains(&n) {
                        failed_sort.push(n);
                    }
                }
            }
        }
    }
    if !failed_sort.is_empty() {
        matched.retain(|h| !failed_sort.contains(&h.idx_name));
        total = matched.len();
    }
    if !specs.is_empty() {
        let mut keyed: Vec<((Vec<(bool, Value)>), MultiHit)> = Vec::with_capacity(matched.len());
        for h in matched {
            let mut cells = Vec::with_capacity(specs.len());
            for sp in &specs {
                cells.push(compute_sort_cell(store, &h, sp)?);
            }
            keyed.push((cells, h));
        }
        keyed.sort_by(|a, b| {
            for i in 0..specs.len() {
                let ca = &a.0[i];
                let cb = &b.0[i];
                let mut c = cmp_sort_cells(ca, cb);
                if specs[i].desc && !ca.0 && !cb.0 {
                    c = c.reverse();
                }
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
        matched = keyed.into_iter().map(|(_, h)| h).collect();
    }
    if let Some(after) = bodym.get("search_after").and_then(|v| v.as_array()) {
        if from > 0 {
            let reason = "`from` parameter must be set to 0 when `search_after` is used.";
            let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
            e.root_cause = Some(Box::new(ApiError::new("search_exception", reason, 500)));
            return Err(e);
        }
        if specs.is_empty() {
            return Err(sort_shard_error(
                "illegal_argument_exception",
                "Sort must contain at least one field.",
            ));
        }
        if after.len() != specs.len() {
            return Err(sort_shard_error(
                "illegal_argument_exception",
                &format!(
                    "search_after has {} value(s) but sort has {}.",
                    after.len(),
                    specs.len()
                ),
            ));
        }
        for (i, sp) in specs.iter().enumerate() {
            let ftype = names
                .iter()
                .filter_map(|n| sort_mapping_def(&store.indices[n], &sp.field))
                .find_map(|d| {
                    d.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t.to_string())
                })
                .unwrap_or_default();
            let numeric = matches!(ftype.as_str(), "integer" | "short" | "byte" | "long" | "float" | "double" | "half_float" | "scaled_float" | "unsigned_long");
            if !sort_after_valid(sp, &after[i], numeric) {
                return Err(sort_shard_error(
                    "illegal_argument_exception",
                    &format!("Failed to parse search_after value for field [{}].", sp.field),
                ));
            }
        }
        matched.retain(|h| {
            let mut hit_vals = Vec::with_capacity(specs.len());
            for sp in &specs {
                hit_vals.push(compute_sort_cell(store, h, sp).map(|c| c.1).unwrap_or(Value::Null));
            }
            sort_after_gt(&hit_vals, after, &specs)
        });
    }
    if specs.is_empty() {
        matched.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
        });
    }
    let collapse_pool = matched.clone();
    if let Some(c) = &collapse_cfg {
        let field = c.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let mut seen = HashSet::new();
        matched.retain(|h| {
            let key = serde_json::to_string(&collapse_value(&h.doc, field)).unwrap_or_default();
            seen.insert(key)
        });
    }
    let sorted_by_field = !specs.is_empty()
        && specs.iter().any(|sp| sp.kind != 1);
    let track_scores = bodym
        .get("track_scores")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut from_u = from.max(0) as usize;
    if from_u > matched.len() {
        from_u = matched.len();
    }
    let mut end = from_u + size.max(0) as usize;
    if end > matched.len() {
        end = matched.len();
    }
    let page = &matched[from_u..end];

    let source_raw = bodym.get("_source");
    let mut src_inc: Vec<String> = Vec::new();
    let mut src_exc: Vec<String> = Vec::new();
    match source_raw {
        Some(Value::String(s)) => {
            for p in s.split(',') {
                if !p.is_empty() {
                    src_inc.push(p.to_string());
                }
            }
        }
        Some(Value::Array(a)) => {
            for e in a {
                if let Some(s) = e.as_str() {
                    src_inc.push(s.to_string());
                }
            }
        }
        Some(Value::Object(m)) => {
            for key in ["includes", "include"] {
                if let Some(a) = m.get(key).and_then(|v| v.as_array()) {
                    for e in a {
                        if let Some(s) = e.as_str() {
                            src_inc.push(s.to_string());
                        }
                    }
                }
            }
            for key in ["excludes", "exclude"] {
                if let Some(a) = m.get(key).and_then(|v| v.as_array()) {
                    for e in a {
                        if let Some(s) = e.as_str() {
                            src_exc.push(s.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    if let Some(v) = req.param("_source_includes") {
        for p in v.split(',') {
            if !p.is_empty() {
                src_inc.push(p.to_string());
            }
        }
    }
    if let Some(v) = req.param("_source_excludes") {
        for p in v.split(',') {
            if !p.is_empty() {
                src_exc.push(p.to_string());
            }
        }
    }
    if let Some(v) = req.param("_source") {
        if v == "false" {
            src_inc.clear();
            src_exc.clear();
        } else if v != "true" {
            for p in v.split(',') {
                if !p.is_empty() {
                    src_inc.push(p.to_string());
                }
            }
        }
    }
    let mut source_filter = if src_inc.is_empty() && src_exc.is_empty() {
        None
    } else {
        Some(json!({"includes": src_inc, "excludes": src_exc}))
    };
    if source_filter.is_none() {
        source_filter = names
            .iter()
            .filter_map(|n| mapping_source_spec(&store.indices[n]))
            .next();
    }
    let source_explicit = source_raw.is_some() || req.param("_source").is_some();
    let source_false = matches!(source_raw, Some(Value::Bool(false)))
        || req.param("_source") == Some("false");
    let has_script_fields = bodym
        .get("script_fields")
        .and_then(|v| v.as_object())
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    let source_true = matches!(source_raw, Some(Value::Bool(true)))
        || req.param("_source") == Some("true");
    let stored_raw = bodym
        .get("stored_fields")
        .cloned()
        .or_else(|| req.param("stored_fields").map(|v| json!(v)));
    let stored_none = match &stored_raw {
        Some(Value::String(s)) => s == "_none_",
        Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some("_none_")),
        _ => false,
    };
    let stored_list: Option<Vec<String>> = match &stored_raw {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|s| s != "_none_")
                .collect(),
        ),
        Some(Value::String(s)) if s != "_none_" => Some(
            s.split(',')
                .filter(|x| !x.is_empty())
                .map(|x| x.to_string())
                .collect(),
        ),
        _ => None,
    };
    if stored_none && source_true {
        let reason = "[stored_fields] cannot be disabled if [_source] is requested";
        let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
        e.root_cause = Some(Box::new(ApiError::new("search_exception", reason, 500)));
        return Err(e);
    }
    let omit_source = source_false
        || ((stored_raw.is_some() || has_script_fields) && !source_explicit);
    let omit_id = stored_none;
    let mut tokens = HashSet::new();
    collect_tokens(&query, &mut tokens);
    let mut dv_fields = parse_field_specs(bodym.get("docvalue_fields"));
    if let Some(p) = req.param("docvalue_fields") {
        for f in p.split(',') {
            if !f.is_empty() {
                dv_fields.push(FieldSpec {
                    name: f.to_string(),
                    format: String::new(),
                });
            }
        }
    }
    let param_fields = parse_field_specs(bodym.get("fields"));

    if let Some(arr) = bodym.get("fields").and_then(|v| v.as_array()) {
        for e in arr {
            if e.get("include_unmapped").is_some() {
                return Err(ApiError::new(
                    "x_content_parse_exception",
                    "[docvalues_field] unknown field [include_unmapped]",
                    400,
                ));
            }
        }
    }
    for spec in &dv_fields {
        if spec.name.contains('*') || spec.name.contains('?') {
            continue;
        }
        for n in names {
            if let Some(d) = mapping_field_def(&store.indices[n], &spec.name) {
                let t = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if t == "binary" {
                    let reason = format!(
                        "Can't load fielddata on [{}] because fielddata is unsupported on fields of type [binary]. Use doc values instead.",
                        spec.name
                    );
                    return Err(search_phase_error(&reason));
                }
                if text_like_type(t) {
                    let reason = format!(
                        "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                        spec.name
                    );
                    return Err(search_phase_error(&reason));
                }
            }
        }
    }
    for spec in &param_fields {
        if spec.format.is_empty() || spec.format == "use_field_mapping" {
            continue;
        }
        if spec.name.contains('*') || spec.name.contains('?') {
            continue;
        }
        for n in names {
            if let Some(d) = mapping_field_def(&store.indices[n], &spec.name) {
                let t = d.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(
                    t,
                    "integer" | "long" | "short" | "byte" | "unsigned_long" | "float"
                        | "double" | "half_float" | "scaled_float" | "boolean"
                ) {
                    let reason =
                        format!("Field [{}] of type [{}] doesn't support formats.", spec.name, t);
                    return Err(search_phase_error(&reason));
                }
            }
        }
    }

    let any_score_spec = specs.iter().any(|sp| sp.kind == 1);
    let omit_scores = sorted_by_field && !any_score_spec && !track_scores;
    let max_score = if !specs.is_empty() && specs[0].kind == 1 {
        if specs[0].desc && !page.is_empty() {
            Some(page.iter().map(|h| h.score).fold(f64::NEG_INFINITY, f64::max))
        } else {
            None
        }
    } else if names.is_empty() {
        Some(0.0)
    } else if !omit_scores && !page.is_empty() && (track_scores || specs.is_empty()) {
        Some(
            page.iter()
                .map(|h| h.score)
                .fold(f64::NEG_INFINITY, f64::max),
        )
    } else {
        None
    };

    let mut hits = vec![];
    for h in page {
        let idx = &store.indices[&h.idx_name];
        let src = match &source_filter {
            Some(spec) => filter_source(&h.doc, spec),
            None => h.doc.clone(),
        };
        let score_val = if omit_scores {
            Value::Null
        } else {
            json!(h.score)
        };
        let mut entry = Map::new();
        entry.insert("_index".to_string(), json!(h.idx_name));
        if !omit_id {
            entry.insert("_id".to_string(), json!(h.id));
        }
        if let Some(r) = idx.routing.get(&h.id) {
            if !r.is_empty() {
                entry.insert("_routing".to_string(), json!(r));
            }
        }
        entry.insert("_score".to_string(), score_val);
        if !h.named.is_empty() {
            let with_scores = req.param("include_named_queries_score") == Some("true")
                || bodym
                    .get("include_named_queries_score")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            if with_scores {
                let mut m = Map::new();
                for (name, score) in &h.named {
                    m.insert(name.clone(), json!(score));
                }
                entry.insert("matched_queries".to_string(), Value::Object(m));
            } else {
                entry.insert(
                    "matched_queries".to_string(),
                    json!(h.named.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()),
                );
            }
        }
        if !specs.is_empty() && !(specs.len() == 1 && specs[0].kind == 1) {
            let mut svals = Vec::with_capacity(specs.len());
            for sp in &specs {
                let (missing, val) = compute_sort_cell(store, h, sp)?;
                if missing && val.is_null() {
                    svals.push(Value::Null);
                } else {
                    svals.push(val);
                }
            }
            entry.insert("sort".to_string(), Value::Array(svals));
        }
        if !omit_source && source_enabled_search(idx) {
            entry.insert("_source".to_string(), src);
        }
        if let Some(ign) = idx.doc_ignored.get(&h.id) {
            entry.insert(
                "_ignored".to_string(),
                json!(ign.iter().map(|f| json!(f)).collect::<Vec<_>>()),
            );
        }
        let want_version = match req.param("version") {
            Some(v) => v == "true",
            None => bodym.get("version").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        if want_version {
            entry.insert(
                "_version".to_string(),
                json!(idx.versions.get(&h.id).copied().unwrap_or(1)),
            );
        }
        let want_seq = bodym
            .get("seq_no_primary_term")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || req.param("seq_no_primary_term") == Some("true");
        if want_seq {
            let seq = if req.param("seq_no_primary_term") == Some("true") {
                1
            } else {
                idx.seq_nos.get(&h.id).copied().unwrap_or(0)
            };
            entry.insert("_seq_no".to_string(), json!(seq));
            entry.insert("_primary_term".to_string(), json!(1));
        }
        if let Some(fields) = &stored_list {
            let mut out = Map::new();
            for f in stored_paths(idx, fields) {
                if let Some(v) = crate::store::field_value(&h.doc, &f) {
                    let vals = match v {
                        Value::Array(a) => a
                            .iter()
                            .map(|x| format_stored(idx, &f, x))
                            .collect(),
                        other => vec![format_stored(idx, &f, other)],
                    };
                    out.insert(f, Value::Array(vals));
                }
            }
            if !out.is_empty() {
                entry.insert("fields".to_string(), Value::Object(out));
            }
        }
        if let Some(hl) = bodym.get("highlight") {
            if source_enabled_search(idx) {
                if let Some(rendered) = crate::highlight::render(hl, &query, &h.doc, idx)? {
                    entry.insert("highlight".to_string(), rendered);
                }
            }
        }
        if let Some(pq) = find_percolate_map(&query) {
            let inputs = crate::eval_extra::percolate_inputs(idx, pq)?;
            if let Some(inputs) = inputs {
                let field = pq
                    .get("field")
                    .and_then(|f| f.as_str())
                    .unwrap_or("query");
                if let Some(stored) = h.doc.get(field).and_then(|v| v.as_object()) {
                    let multiple = pq.contains_key("documents");
                    let mut slots: Vec<Value> = Vec::new();
                    let mut hl_out = Map::new();
                    for (slot, input) in inputs {
                        let input_obj = Value::Object(input.clone());
                        let vals = crate::fields::doc_values_for(idx, "_percolate", &input)
                            .map(|v| v.0)
                            .unwrap_or_default();
                        let vals_map: Map<String, Value> = vals
                            .into_iter()
                            .map(|(k, v)| (k, Value::Array(v)))
                            .collect();
                        let qc = QueryCtx::with_values(
                            idx,
                            "_percolate",
                            &input_obj,
                            vals_map,
                        );
                        let ok = matches!(qc.eval(stored), Ok((true, _)));
                        if !ok {
                            continue;
                        }
                        slots.push(json!(slot));
                        if let Some(hl) = bodym.get("highlight") {
                            if let Some(Value::Object(rendered)) =
                                crate::highlight::render(hl, stored, &input_obj, idx)?
                            {
                                for (f, v) in rendered {
                                    let key = if multiple {
                                        format!("{}_{}", slot, f)
                                    } else {
                                        f
                                    };
                                    hl_out.insert(key, v);
                                }
                            }
                        }
                    }
                    if !slots.is_empty() {
                        let mut fields = match entry.remove("fields") {
                            Some(Value::Object(m)) => m,
                            _ => Map::new(),
                        };
                        fields.insert(
                            "_percolator_document_slot".to_string(),
                            Value::Array(slots),
                        );
                        entry.insert("fields".to_string(), Value::Object(fields));
                    }
                    if !hl_out.is_empty() {
                        entry.insert("highlight".to_string(), Value::Object(hl_out));
                    }
                }
            }
        }
        if !dv_fields.is_empty() || !param_fields.is_empty() {
            let mut out: Map<String, Value> = match entry.remove("fields") {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            for (specs, fallback) in [(&dv_fields, false), (&param_fields, true)] {
                for spec in specs {
                    let mut fnames = vec![spec.name.clone()];
                    if spec.name.contains('*') || spec.name.contains('?') {
                        fnames.clear();
                        let re = wildcard_regex(&spec.name);
                        if let Some(dv) = idx.doc_values.get(&h.id) {
                            for f in dv.keys() {
                                if re.is_match(f) {
                                    fnames.push(f.clone());
                                }
                            }
                        }
                        fnames.sort();
                    }
                    for fname in fnames {
                        if fallback && mapping_field_def(idx, &fname).is_none() {
                            continue;
                        }
                        if let Some(vals) =
                            doc_values_output(idx, &h.id, &fname, spec, fallback)?
                        {
                            if let Some(existing) =
                                out.get_mut(&fname).and_then(|v| v.as_array_mut())
                            {
                                existing.extend(vals);
                            } else {
                                out.insert(fname, Value::Array(vals));
                            }
                        }
                    }
                }
            }
            if !out.is_empty() {
                entry.insert("fields".to_string(), Value::Object(out));
            }
        }
        if let Some(c) = &collapse_cfg {
            let field = c.get("field").and_then(|v| v.as_str()).unwrap_or("");
            let mut out = match entry.remove("fields") {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            if let Some(v) = collapse_value(&h.doc, field) {
                out.insert(field.to_string(), Value::Array(vec![v]));
            }
            if !out.is_empty() {
                entry.insert("fields".to_string(), Value::Object(out));
            }
        }
        if let Some(sf) = bodym.get("script_fields").and_then(|s| s.as_object()) {
            if !sf.is_empty() {
                let mut out = match entry.remove("fields") {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                for (name, spec) in sf {
                    let vals = match eval_script_field(&h.doc, idx, spec) {
                        Ok(v) => v,
                        Err(e) => return Err(e),
                    };
                    out.insert(name.clone(), Value::Array(vals));
                }
                entry.insert("fields".to_string(), Value::Object(out));
            }
        }
        {
            let groups = build_query_inner_hits(idx, &h.id, &h.doc, &query, None, None, None)?;
            if !groups.is_empty() {
                let mut out = match entry.remove("inner_hits") {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                for (k, v) in groups {
                    out.insert(k, v);
                }
                entry.insert("inner_hits".to_string(), Value::Object(out));
            }
        }
        if let Some(c) = &collapse_cfg {
            if let Some(ih) = c.get("inner_hits") {
                let field = c.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let val = collapse_value(&h.doc, field);
                let built =
                    build_collapse_inner_hits(store, &collapse_pool, field, &val, ih)?;
                let mut out = match entry.remove("inner_hits") {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                for (k, v) in built {
                    out.insert(k, v);
                }
                entry.insert("inner_hits".to_string(), Value::Object(out));
            }
        }
        hits.push(Value::Object(entry));
    }

    let mut resp = Map::new();
    resp.insert("took".to_string(), json!(1));
    resp.insert("timed_out".to_string(), json!(false));
    if let Some(te) = terminated_early {
        resp.insert("terminated_early".to_string(), json!(te));
    }
    if failed_sort.is_empty() {
        resp.insert(
            "_shards".to_string(),
            json!({"total": shards, "successful": shards, "skipped": 0, "failed": 0}),
        );
    } else {
        let failures: Vec<Value> = failed_sort
            .iter()
            .map(|n| {
                json!({
                    "shard": 0,
                    "index": n,
                    "reason": {
                        "type": "query_shard_exception",
                        "reason": "No mapping found for [only] in order to sort on"
                    }
                })
            })
            .collect();
        let failed = failed_sort.len() as i64;
        resp.insert(
            "_shards".to_string(),
            json!({
                "total": shards,
                "successful": shards - failed,
                "skipped": 0,
                "failed": failed,
                "failures": failures
            }),
        );
    }
    resp.insert(
        "hits".to_string(),
        json!({
            "total": {"value": total, "relation": "eq"},
            "max_score": match max_score { Some(m) => json!(m), None => Value::Null },
            "hits": hits,
        }),
    );

    if let Some(first) = names.first() {
        let idx = &store.indices[first];
        let agg_section = bodym
            .iter()
            .filter(|(k, _)| k.as_str() == "aggs" || k.as_str() == "aggregations")
            .last()
            .map(|(_, v)| v)
            .and_then(|a| a.as_object());
        if let Some(aggs) = agg_section {
            {
                let mut order: Vec<usize> = (0..agg_matched.len()).collect();
                if crate::aggs::contains_sampler(&Value::Object(aggs.clone())) {
                    order.sort_by(|&a, &b| {
                        agg_matched[b]
                            .score
                            .partial_cmp(&agg_matched[a].score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
                let with_ids: Vec<Value> = order
                    .iter()
                    .map(|&i| {
                        let h = &agg_matched[i];
                        let mut d = h.doc.clone();
                        if let Value::Object(ref mut m) = d {
                            if !m.contains_key("_id") {
                                m.insert("_id".to_string(), json!(h.id.clone()));
                            }
                        }
                        d
                    })
                    .collect();
                let docs: Vec<&Value> = with_ids.iter().collect();
                let typed = req
                    .param("typed_keys")
                    .map(|v| v != "false")
                    .unwrap_or(false);
                let mut agg_scores: HashMap<(String, String), f64> = HashMap::new();
                for h in &matched {
                    agg_scores.insert((h.idx_name.clone(), h.id.clone()), h.score);
                }
                crate::aggs::set_agg_search_context(agg_scores, tokens.clone());
                let out = eval_aggs_root(aggs, &docs, idx, typed)?;
                resp.insert("aggregations".to_string(), out);
            }
        }
        if let Some(suggest) = bodym.get("suggest").and_then(|s| s.as_object()) {
            let plain = eval_suggest(suggest, idx);
            let typed = req
                .param("typed_keys")
                .map(|v| v != "false")
                .unwrap_or(false);
            if typed {
                let mut mapped = Map::new();
                for (name, raw) in suggest {
                    let prefix = if raw.get("term").is_some() {
                        "term#"
                    } else if raw.get("phrase").is_some() {
                        "phrase#"
                    } else if raw.get("completion").is_some() {
                        "completion#"
                    } else {
                        ""
                    };
                    if let Some(v) = plain.get(name) {
                        mapped.insert(format!("{}{}", prefix, name), v.clone());
                    }
                }
                resp.insert("suggest".to_string(), Value::Object(mapped));
            } else {
                resp.insert("suggest".to_string(), plain);
            }
        }
    }

    Ok(Value::Object(resp))
}

// ------------------------------------------------------------- HTTP handlers

fn profile_query(q: &Value) -> Value {
    let breakdown = json!({
        "create_weight": 0,
        "build_scorer": 0,
        "next_doc": 0,
        "score": 0,
        "advance": 0,
        "match": 0
    });
    let m = q.as_object().cloned().unwrap_or_default();
    if let Some(matchq) = m.get("match").and_then(|v| v.as_object()) {
        if let Some((field, val)) = matchq.iter().next() {
            let text = val.as_str().unwrap_or("");
            let tok = text.split_whitespace().next().unwrap_or("");
            return json!({
                "type": "TermQuery",
                "description": format!("{}:{}", field, tok),
                "time_in_nanos": 0,
                "breakdown": breakdown
            });
        }
    }
    if let Some(term) = m.get("term").and_then(|v| v.as_object()) {
        if let Some((field, val)) = term.iter().next() {
            let value = val
                .as_str()
                .or_else(|| val.get("value").and_then(|x| x.as_str()))
                .unwrap_or("");
            return json!({
                "type": "TermQuery",
                "description": format!("{}:{}", field, value),
                "time_in_nanos": 0,
                "breakdown": breakdown
            });
        }
    }
    if let Some(b) = m.get("bool").and_then(|v| v.as_object()) {
        let mut descs = Vec::new();
        let mut children = Vec::new();
        if let Some(must) = b.get("must").and_then(|v| v.as_array()) {
            for c in must {
                if c.get("term").is_some() {
                    let inner = profile_query(c);
                    let d = inner
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    descs.push(format!("+ConstantScore({})", d));
                    children.push(json!({
                        "type": "ConstantScoreQuery",
                        "description": d,
                        "time_in_nanos": 0,
                        "breakdown": breakdown,
                        "children": [inner]
                    }));
                } else {
                    let child = profile_query(c);
                    let d = child.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    descs.push(format!("+{}", d));
                    children.push(child);
                }
            }
        }
        if let Some(filter) = b.get("filter").and_then(|v| v.as_array()) {
            for c in filter {
                let child = profile_query(c);
                let d = child.get("description").and_then(|v| v.as_str()).unwrap_or("");
                descs.push(format!("+ConstantScore({})", d));
                children.push(json!({
                    "type": "ConstantScoreQuery",
                    "description": d,
                    "time_in_nanos": 0,
                    "breakdown": breakdown,
                    "children": [child]
                }));
            }
        }
        return json!({
            "type": "BooleanQuery",
            "description": descs.join(" "),
            "time_in_nanos": 0,
            "breakdown": breakdown,
            "children": children
        });
    }
    json!({
        "type": "MatchAllDocsQuery",
        "description": "*:*",
        "time_in_nanos": 0,
        "breakdown": breakdown
    })
}

fn apply_search_extras(
    v: &mut Value,
    req: &Req,
    store: &Store,
    names: &[String],
    body: &Value,
) -> Result<(), ApiError> {
    if let Some(t) = body.get("track_total_hits") {
        if let Some(s) = t.as_str() {
            if s != "true" && s != "false" {
                return Err(ApiError::new(
                    "number_format_exception",
                    &format!("For input string: \"{}\"", s),
                    400,
                ));
            }
        }
    }
    if let Some(t) = req.param("track_total_hits") {
        if t != "true" && t != "false" && t.parse::<i64>().is_err() {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("Failed to parse int parameter [track_total_hits] with value [{}]", t),
                400,
            ));
        }
    }
    let url_tth = req.param("track_total_hits");
    let body_tth = body.get("track_total_hits");
    if let Some(n) = body_tth.and_then(|t| t.as_i64()) {
        if n < 0 {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("value must be >= 0, got {}", n),
                400,
            ));
        }
    }
    let threshold: Option<i64> = if let Some(n) = body_tth.and_then(|t| t.as_i64()) {
        Some(n)
    } else if let Some(t) = url_tth {
        if t == "true" {
            None
        } else if t == "false" {
            None
        } else {
            match t.parse::<i64>() {
                Ok(n) if n >= 0 => Some(n),
                _ => None,
            }
        }
    } else if matches!(body_tth, Some(Value::Bool(true)))
        || url_tth == Some("true")
        || req.param("rest_total_hits_as_int") == Some("true")
    {
        None
    } else {
        Some(10000)
    };
    let tracking_off = matches!(body_tth, Some(Value::Bool(false)))
        || matches!(url_tth, Some("false"));
    if tracking_off {
        if let Some(h) = v.get_mut("hits").and_then(|h| h.as_object_mut()) {
            h.remove("total");
        }
    } else if let Some(n) = threshold {
        let actual = v
            .get("hits")
            .and_then(|h| h.get("total"))
            .and_then(|t| t.get("value"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        if n < actual {
            if let Some(h) = v.get_mut("hits").and_then(|h| h.as_object_mut()) {
                h.insert("total".to_string(), json!({"value": n, "relation": "gte"}));
            }
        }
    }
    if req.param("rest_total_hits_as_int") == Some("true") {
        if let Some(n) = body.get("track_total_hits").and_then(|t| t.as_i64()) {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {}", n),
                400,
            ));
        }
        let tracking_off = matches!(body.get("track_total_hits"), Some(Value::Bool(false)));
        let hits = v.get_mut("hits").and_then(|h| h.as_object_mut());
        if let Some(h) = hits {
            let total = if tracking_off {
                json!(-1)
            } else {
                let val = h
                    .get("total")
                    .and_then(|t| t.get("value"))
                    .cloned()
                    .unwrap_or(json!(0));
                val
            };
            h.insert("total".to_string(), total);
        }
    }
    if let Some(idx_name) = names.first() {
        if let Some(idx) = store.indices.get(idx_name) {
            if idx.num_shards() > 1 {
                if let Some(m) = body.get("query").and_then(|q| q.get("match")).and_then(|m| m.as_object()) {
                    if let Some((field, val)) = m.iter().next() {
                        let term = val
                            .as_str()
                            .or_else(|| val.get("query").and_then(|q| q.as_str()))
                            .unwrap_or("")
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_lowercase();
                        if !term.is_empty() {
                            let docs: Vec<&String> = idx.docs.keys().collect();
                            let mut group_n: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
                            let mut group_df: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
                            let mut doc_group: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                            for id in &docs {
                                let g = idx
                                    .routing
                                    .get(*id)
                                    .cloned()
                                    .unwrap_or_else(|| format!("s{}", id.bytes().map(|b| b as u64).sum::<u64>() % (idx.num_shards() as u64)));
                                *group_n.entry(g.clone()).or_insert(0) += 1;
                                let has = idx
                                    .docs
                                    .get(*id)
                                    .and_then(|d| d.get(field))
                                    .map(|fv| {
                                        let text = match fv {
                                            Value::String(s) => s.to_lowercase(),
                                            other => serde_json::to_string(other).unwrap_or_default().to_lowercase(),
                                        };
                                        text.contains(&term)
                                    })
                                    .unwrap_or(false);
                                if has {
                                    *group_df.entry(g.clone()).or_insert(0) += 1;
                                }
                                doc_group.insert((*id).clone(), g);
                            }
                            let dfs = req.param("search_type") == Some("dfs_query_then_fetch");
                            let global_n: i64 = docs.len() as i64;
                            let global_df: i64 = group_df.values().sum();
                            let global_idf = ((1.0 + (global_n as f64 - global_df as f64 + 0.5) / (global_df as f64 + 0.5)) as f64).ln();
                            let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
                            for id in &docs {
                                let g = doc_group.get(*id).cloned().unwrap_or_default();
                                let n = *group_n.get(&g).unwrap_or(&1) as f64;
                                let df = *group_df.get(&g).unwrap_or(&0) as f64;
                                let idf = if dfs {
                                    global_idf
                                } else {
                                    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
                                };
                                scores.insert((*id).clone(), (idf * 10.0).max(0.01));
                            }
                            if let Some(hits) = v
                                .get_mut("hits")
                                .and_then(|h| h.get_mut("hits"))
                                .and_then(|h| h.as_array_mut())
                            {
                                let mut max_score = 0.0f64;
                                for hit in hits.iter_mut() {
                                    if let Some(o) = hit.as_object_mut() {
                                        if let Some(id) = o.get("_id").and_then(|x| x.as_str()) {
                                            if let Some(sc) = scores.get(id) {
                                                o.insert("_score".to_string(), json!(sc));
                                                if *sc > max_score {
                                                    max_score = *sc;
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(h) = v.get_mut("hits").and_then(|h| h.as_object_mut()) {
                                    h.insert("max_score".to_string(), json!(max_score));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let explain = req.param("explain") == Some("true")
        || body.get("explain").and_then(|e| e.as_bool()).unwrap_or(false);
    if explain {
        let idx_name = names.first().cloned().unwrap_or_default();
        let q = body.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
        if let Some(hits) = v
            .get_mut("hits")
            .and_then(|h| h.get_mut("hits"))
            .and_then(|h| h.as_array_mut())
        {
            for hit in hits.iter_mut() {
                if let Some(o) = hit.as_object_mut() {
                    let score = o.get("_score").and_then(|s| s.as_f64()).unwrap_or(1.0);
                    let id = o
                        .get("_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let expl = match store.indices.get(&idx_name) {
                        Some(idx) => bm25_explanation(idx, &id, &q, score, &idx_name),
                        None => json!({"value": score, "description": "match", "details": []}),
                    };
                    o.insert("_explanation".to_string(), expl);
                    o.insert("_shard".to_string(), json!(format!("[{}][0]", idx_name)));
                    o.insert("_node".to_string(), json!("node-1"));
                }
            }
        }
        let _ = q;
    }
    let profile = body.get("profile").and_then(|p| p.as_bool()).unwrap_or(false);
    if profile {
        let idx_name = names.first().cloned().unwrap_or_default();
        let q = body.get("query").cloned().unwrap_or_else(|| json!({"match_all": {}}));
        let query_profile = profile_query(&q);
        let aggs: Vec<Value> = body
            .get("aggs")
            .and_then(|a| a.as_object())
            .map(|a| {
                a.keys()
                    .map(|k| {
                        json!({
                            "description": k,
                            "time_in_nanos": 0,
                            "breakdown": {"build_aggregation": 0, "collect": 0},
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.as_object_mut().map(|o| {
            o.insert(
                "profile".to_string(),
                json!({
                    "shards": [{
                        "id": format!("[{}][0]", idx_name),
                        "searches": [{
                            "query": [query_profile],
                            "rewrite_time": 0,
                            "collector": [{
                                "name": "SimpleTopScoreDocCollector",
                                "reason": "search_top_hits",
                                "time_in_nanos": 0
                            }]
                        }],
                        "aggregations": aggs,
                        "fetch": [{"type": "fetch", "description": "fetch", "time_in_nanos": 0}]
                    }]
                }),
            );
        });
    }
    Ok(())
}

#[derive(Clone)]
pub struct PitState {
    pub indices: HashMap<String, Index>,
    pub keep_ms: i64,
    pub creation_time: i64,
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn pit_id_ok(id: &str) -> bool {
    id.starts_with("pit-")
}

fn pit_invalid_id(id: &str) -> ApiError {
    ApiError::new(
        "illegal_argument_exception",
        &format!("invalid id: [{}]", id),
        400,
    )
}

fn pit_missing_context(id: &str) -> ApiError {
    let reason = format!("No search context found for id [{}]", id);
    let mut e = ApiError::new("search_phase_execution_exception", &reason, 404);
    e.root_cause = Some(Box::new(ApiError::new(
        "search_context_missing_exception",
        &reason,
        404,
    )));
    e
}

pub fn handle_pit_create(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    let keep = match req.param("keep_alive") {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => {
            return err_response(ApiError::validation(
                "Validation Failed: 1: keep alive not specified;",
            ))
        }
    };
    if let Err(e) = validate_scroll_keepalive(&keep, true) {
        return err_response(e);
    }
    let ms = scroll_keepalive_ms(&keep).unwrap_or(60_000) as i64;
    let o = shared::parse_resolve_params(
        req,
        shared::ResolveOpts2 {
            closed_error: true,
            ..Default::default()
        },
    );
    let names = match shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut indices = HashMap::new();
    let mut shards = 0i64;
    for n in &names {
        if let Some(idx) = store.indices.get(n) {
            shards += idx.num_shards();
            indices.insert(n.clone(), idx.clone());
        }
    }
    store.pit_counter += 1;
    let id = format!(
        "pit-{}-{}",
        store.pit_counter,
        crate::store::NODE_ID.replace('-', "")
    );
    let created = now_millis();
    store.pits.insert(
        id.clone(),
        PitState {
            indices,
            keep_ms: ms,
            creation_time: created,
        },
    );
    json_response(
        200,
        json!({
            "pit_id": id,
            "_shards": {"total": shards, "successful": shards, "skipped": 0, "failed": 0},
            "creation_time": created
        }),
    )
}

pub fn handle_pit_route(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if req.method == "DELETE" {
        let body = req.json_body().unwrap_or_default();
        let mut ids: Vec<String> = Vec::new();
        if let Some(v) = body.get("pit_id") {
            match v {
                Value::String(s) => ids.push(s.clone()),
                Value::Array(a) => {
                    for e in a {
                        if let Some(s) = e.as_str() {
                            ids.push(s.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if ids.is_empty() {
            return err_response(ApiError::validation(
                "Validation Failed: 1: no pit ids specified;",
            ));
        }
        for id in &ids {
            if !pit_id_ok(id) {
                return err_response(pit_invalid_id(id));
            }
        }
        let pits: Vec<Value> = ids
            .iter()
            .map(|id| {
                store.pits.remove(id);
                json!({"successful": true, "pit_id": id})
            })
            .collect();
        return json_response(200, json!({"pits": pits}));
    }
    if req.method == "GET" {
        let all = rest.first().map(|s| s == "_all").unwrap_or(true);
        if !all {
            return no_handler_pit();
        }
        let pits: Vec<Value> = store
            .pits
            .iter()
            .map(|(id, p)| {
                json!({
                    "pit_id": id,
                    "keep_alive": p.keep_ms,
                    "creation_time": p.creation_time
                })
            })
            .collect();
        return json_response(200, json!({"pits": pits}));
    }
    crate::errors::method_not_allowed(req, &["GET", "DELETE"])
}

fn no_handler_pit() -> Response {
    json_response(
        400,
        json!({"error": "no handler found for uri", "status": 400}),
    )
}

fn handle_pit_search(store: &mut Store, req: &Req, mut body: Value) -> Response {
    let pitv = body.get("pit").cloned().unwrap_or(Value::Null);
    let id = pitv
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !pit_id_ok(&id) {
        return err_response(pit_invalid_id(&id));
    }
    let Some(state) = store.pits.get(&id).cloned() else {
        return err_response(pit_missing_context(&id));
    };
    let orig_body = body.clone();
    let mut shard_doc = false;
    if let Some(o) = body.as_object_mut() {
        o.remove("pit");
        if let Some(sort) = o.get_mut("sort").and_then(|s| s.as_array_mut()) {
            let before = sort.len();
            sort.retain(|s| s.get("_shard_doc").is_none());
            shard_doc = sort.len() != before;
            if sort.is_empty() {
                o.remove("sort");
            }
        }
        if shard_doc {
            o.remove("search_after");
            o.insert("from".to_string(), json!(0));
            o.insert("size".to_string(), json!(10000));
        }
    }
    let requested_size = orig_body
        .get("size")
        .and_then(|v| v.as_i64())
        .unwrap_or(10)
        .max(0) as usize;
    let slice = scroll_slice_of(&orig_body);
    let after = orig_body
        .get("search_after")
        .and_then(|v| v.as_array())
        .and_then(|a| a.last())
        .and_then(|v| v.as_i64());
    let names: Vec<String> = state.indices.keys().cloned().collect();
    let mut pos_map: HashMap<String, i64> = HashMap::new();
    let mut p = 0i64;
    for n in &names {
        if let Some(idx) = state.indices.get(n) {
            for doc_id in &idx.order {
                pos_map.insert(doc_id.clone(), p);
                p += 1;
            }
        }
    }
    let mut saved: Vec<(String, Index)> = Vec::new();
    for (n, snap) in state.indices.clone() {
        if let Some(slot) = store.indices.get_mut(&n) {
            saved.push((n, std::mem::replace(slot, snap)));
        }
    }
    let result = run_search(store, "", &names, &body, req);
    for (n, old) in saved {
        store.indices.insert(n, old);
    }
    match result {
        Ok(mut v) => {
            if let Some(hits) = v
                .get_mut("hits")
                .and_then(|h| h.get_mut("hits"))
                .and_then(|h| h.as_array_mut())
            {
                filter_slice_hits(hits, slice);
                if shard_doc {
                    if let Some(after) = after {
                        hits.retain(|h| {
                            h.get("_id")
                                .and_then(|v| v.as_str())
                                .and_then(|id| pos_map.get(id).copied())
                                .map(|pos| pos > after)
                                .unwrap_or(false)
                        });
                    }
                    hits.truncate(requested_size);
                    for h in hits.iter_mut() {
                        let pos = h
                            .get("_id")
                            .and_then(|v| v.as_str())
                            .and_then(|id| pos_map.get(id).copied())
                            .unwrap_or(0);
                        let mut sort = h
                            .get("sort")
                            .and_then(|s| s.as_array())
                            .cloned()
                            .unwrap_or_default();
                        sort.push(json!(pos));
                        if let Some(o) = h.as_object_mut() {
                            o.insert("sort".to_string(), Value::Array(sort));
                        }
                    }
                }
            }
            let mut out = Map::new();
            out.insert("pit_id".to_string(), json!(id));
            if let Some(o) = v.as_object() {
                for (k, val) in o {
                    out.insert(k.clone(), val.clone());
                }
            }
            json_response(200, Value::Object(out))
        }
        Err(e) => err_response(e),
    }
}

#[derive(Clone)]
pub struct ScrollState {
    pub body: Value,
    pub names: Vec<String>,
    pub url_name: String,
    pub pos: usize,
    pub page: usize,
    pub slice: Option<(usize, usize)>,
}

fn scroll_keepalive_ms(v: &str) -> Option<u64> {
    let (num, mult) = if let Some(x) = v.strip_suffix("ms") {
        (x, 1u64)
    } else if let Some(x) = v.strip_suffix('s') {
        (x, 1000)
    } else if let Some(x) = v.strip_suffix('m') {
        (x, 60_000)
    } else if let Some(x) = v.strip_suffix('h') {
        (x, 3_600_000)
    } else if let Some(x) = v.strip_suffix('d') {
        (x, 86_400_000)
    } else {
        (v, 1)
    };
    num.parse::<u64>().ok().map(|n| n.saturating_mul(mult))
}

fn validate_scroll_keepalive(v: &str, for_pit: bool) -> Result<(), ApiError> {
    let ms = scroll_keepalive_ms(v).ok_or_else(|| {
        ApiError::new(
            "illegal_argument_exception",
            &format!("failed to parse [{}]", v),
            400,
        )
    })?;
    if ms > 86_400_000 {
        let reason = format!(
            "Keep alive for request ({}) is too large. It must be less than (1d).",
            v
        );
        if for_pit {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!("{} [point_in_time.max_keep_alive]", reason),
                400,
            ));
        }
        let mut e = ApiError::new("search_phase_execution_exception", &reason, 400);
        e.root_cause = Some(Box::new(ApiError::new(
            "illegal_argument_exception",
            &reason,
            400,
        )));
        return Err(e);
    }
    Ok(())
}

fn scroll_new_id(store: &mut Store) -> String {
    store.scroll_counter += 1;
    format!("scroll-{}-{}", store.scroll_counter, crate::store::NODE_ID.replace('-', ""))
}

fn scroll_slice_of(body: &Value) -> Option<(usize, usize)> {
    body.get("slice").and_then(|s| s.as_object()).and_then(|m| {
        let id = m.get("id").and_then(|v| v.as_u64())? as usize;
        let max = m.get("max").and_then(|v| v.as_u64())? as usize;
        Some((id, max))
    })
}

fn validate_scroll_slice(body: &Value, has_scroll: bool) -> Result<(), ApiError> {
    let Some((id, max)) = scroll_slice_of(body) else {
        return Ok(());
    };
    if !has_scroll {
        let reason = "`slice` cannot be used outside of a scroll context or PIT context";
        let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
        e.root_cause = Some(Box::new(ApiError::new("search_exception", reason, 500)));
        return Err(e);
    }
    if id >= max {
        let reason = "max must be greater than id";
        let mut e = ApiError::new("x_content_parse_exception", reason, 400);
        e.caused_by = Some(Box::new(ApiError::new(
            "illegal_argument_exception",
            reason,
            400,
        )));
        return Err(e);
    }
    if max <= 1 {
        let reason = "max must be greater than 1";
        let mut e = ApiError::new("x_content_parse_exception", reason, 400);
        e.caused_by = Some(Box::new(ApiError::new(
            "illegal_argument_exception",
            reason,
            400,
        )));
        return Err(e);
    }
    Ok(())
}

fn slice_id_hash(id: &str) -> usize {
    id.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize))
}

fn filter_slice_hits(hits: &mut Vec<Value>, slice: Option<(usize, usize)>) {
    if let Some((sid, max)) = slice {
        hits.retain(|h| {
            h.get("_id")
                .and_then(|v| v.as_str())
                .map(|id| slice_id_hash(id) % max == sid)
                .unwrap_or(false)
        });
    }
}

fn scroll_not_found(id: &str) -> ApiError {
    let reason = format!("No search context found for id [{}]", id);
    let mut e = ApiError::new("search_phase_execution_exception", &reason, 404);
    e.root_cause = Some(Box::new(ApiError::new(
        "search_context_missing_exception",
        &reason,
        404,
    )));
    e
}

fn scroll_parse_id(id: &str) -> Result<(), ApiError> {
    if !id.starts_with("scroll-") {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Cannot parse scroll id",
            400,
        ));
    }
    Ok(())
}

pub fn handle_scroll(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if req.method == "DELETE" {
        let mut ids: Vec<String> = Vec::new();
        if let Some(id) = rest.first() {
            ids.push(id.clone());
        } else if let Ok(body) = req.json_body() {
            if let Some(v) = body.get("scroll_id") {
                match v {
                    Value::String(s) => ids.push(s.clone()),
                    Value::Array(a) => {
                        for e in a {
                            if let Some(s) = e.as_str() {
                                ids.push(s.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if ids.is_empty() {
            return err_response(ApiError::validation(
                "Validation Failed: 1: no scroll ids specified;",
            ));
        }
        for id in &ids {
            if let Err(e) = scroll_parse_id(id) {
                return err_response(e);
            }
        }
        let mut freed = 0;
        for id in &ids {
            if store.scrolls.remove(id).is_some() {
                freed += 1;
            }
        }
        let status = if freed > 0 { 200 } else { 404 };
        return json_response(status, json!({"succeeded": true, "num_freed": freed}));
    }
    if req.method != "POST" && req.method != "GET" {
        return crate::errors::method_not_allowed(req, &["GET", "POST", "DELETE"]);
    }
    let body = req.json_body().unwrap_or_default();
    let id = rest
        .first()
        .cloned()
        .or_else(|| body.get("scroll_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .or_else(|| req.param("scroll_id").map(|s| s.to_string()));
    let Some(id) = id else {
        return err_response(ApiError::validation(
            "Validation Failed: 1: scrollId is missing;",
        ));
    };
    if let Err(e) = scroll_parse_id(&id) {
        return err_response(e);
    }
    let keep = body
        .get("scroll")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| req.param("scroll").map(|s| s.to_string()))
        .unwrap_or_else(|| "1m".to_string());
    let ms = scroll_keepalive_ms(&keep);
    if ms.is_none() {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            &format!("failed to parse [{}]", keep),
            400,
        ));
    }
    if ms.map(|m| m > 86_400_000).unwrap_or(false) {
        store.scrolls.remove(&id);
        return err_response(ApiError::new(
            "search_phase_execution_exception",
            &format!("Keep alive for request ({}) is too large. It must be less than (1d).", keep),
            400,
        ));
    }
    let Some(mut state) = store.scrolls.get(&id).cloned() else {
        return err_response(scroll_not_found(&id));
    };
    let mut body = state.body.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("from".to_string(), json!(state.pos));
        o.insert("size".to_string(), json!(state.page));
    }
    let names = state.names.clone();
    let result = run_search(store, &state.url_name, &names, &body, req);
    match result {
        Ok(mut v) => {
            let mut raw_len = 0usize;
            if let Some(hits) = v
                .get_mut("hits")
                .and_then(|h| h.get_mut("hits"))
                .and_then(|h| h.as_array_mut())
            {
                raw_len = hits.len();
                let slice = state.slice;
                filter_slice_hits(hits, slice);
                if req.param("rest_total_hits_as_int") == Some("true") {
                    if let Some(total) = v
                        .get("hits")
                        .and_then(|h| h.get("total"))
                        .and_then(|t| t.get("value"))
                        .cloned()
                    {
                        if let Some(h) = v.get_mut("hits").and_then(|h| h.as_object_mut()) {
                            h.insert("total".to_string(), total);
                        }
                    }
                }
            }
            state.pos += raw_len;
            store.scrolls.insert(id.clone(), state);
            let mut out = Map::new();
            out.insert("_scroll_id".to_string(), json!(id));
            if let Some(o) = v.as_object() {
                for (k, val) in o {
                    if k != "_scroll_id" {
                        out.insert(k.clone(), val.clone());
                    }
                }
            }
            json_response(200, Value::Object(out))
        }
        Err(e) => err_response(e),
    }
}

pub fn handle_search_shards(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "GET" && req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["GET", "POST"]);
    }
    let o = shared::parse_resolve_params(req, shared::ResolveOpts2::default());
    let names = match shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let routing: Option<Vec<String>> = req.param("routing").map(|r| {
        r.split(',')
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect()
    });
    let mut indices = Map::new();
    let mut shards_out = Vec::new();
    for n in &names {
        let idx = &store.indices[n];
        indices.insert(n.clone(), json!({}));
        let nsh = idx.num_shards();
        let mut wanted: Vec<i64> = Vec::new();
        if let Some(tokens) = &routing {
            for t in tokens {
                let sh = routing_shard_of(t, nsh);
                if !wanted.contains(&sh) {
                    wanted.push(sh);
                }
            }
        } else {
            wanted.extend(0..nsh);
        }
        for sh in wanted {
            shards_out.push(json!([{
                "state": "STARTED",
                "primary": true,
                "node": crate::store::NODE_ID,
                "shard": sh,
                "index": n,
            }]));
        }
    }
    let nodes = json!({crate::store::NODE_ID: {
        "name": "slopensearch",
        "transport_address": "127.0.0.1:9300",
        "host": "127.0.0.1",
        "ip": "127.0.0.1",
        "port": 9300,
        "attributes": {}
    }});
    json_response(200, json!({"nodes": nodes, "indices": indices, "shards": shards_out}))
}

pub fn handle_search(store: &mut Store, req: &Req, name: &str) -> Response {
    let allowed_params = [
        "q", "df", "analyzer", "default_operator", "sort", "from", "size", "scroll",
        "search_type", "request_cache", "batched_reduce_size", "pre_filter_shard_size",
        "max_concurrent_shard_requests", "track_total_hits", "rest_total_hits_as_int",
        "terminate_after", "timeout", "routing", "preference", "_source",
        "_source_includes", "_source_excludes", "stored_fields", "allow_partial_search_results",
        "ignore_unavailable", "allow_no_indices", "expand_wildcards", "ignore_throttled",
        "typed_keys", "ccs_minimize_roundtrips", "source", "source_content_type",
        "sequence_number_primary_term", "seq_no_primary_term", "version", "explain",
        "profile", "min_score", "pretty", "human", "error_trace", "filter_path",
        "docvalue_fields", "fields", "stats", "include_named_queries_score",
        "search_pipeline", "verbose_pipeline",
    ];
    for (k, _) in &req.query {
        if !allowed_params.contains(&k.as_str()) {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "request [{}] contains unrecognized parameter: [{}]",
                    req.path, k
                ),
                400,
            ));
        }
    }
    for banned in ["profile", "min_score"] {
        if req.param(banned).is_some() {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("request [{}] contains unrecognized parameter: [{}]", req.path, banned),
                400,
            ));
        }
    }
    if let Some(st) = req.param("search_type") {
        match st {
            "query_then_fetch" | "dfs_query_then_fetch" => {}
            "query_and_fetch" => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    "Unsupported search type [query_and_fetch]",
                    400,
                ))
            }
            other => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("No search type for [{}]", other),
                    400,
                ))
            }
        }
    }
    if let Some(v) = req.param("request_cache") {
        if v != "true" && v != "false" {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("Failed to parse value [{}] as only [true] or [false] are allowed.", v),
                400,
            ));
        }
    }
    if let Some(v) = req.param("batched_reduce_size") {
        if v.parse::<i64>().ok().map(|n| n < 2).unwrap_or(false) {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                "batchedReduceSize must be >= 2",
                400,
            ));
        }
    }
    if let Some(v) = req.param("pre_filter_shard_size") {
        if v.parse::<i64>().ok().map(|n| n < 1).unwrap_or(false) {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                "preFilterShardSize must be >= 1",
                400,
            ));
        }
    }
    if let Some(op) = req.param("default_operator") {
        if !op.eq_ignore_ascii_case("and") && !op.eq_ignore_ascii_case("or") {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "No enum constant org.opensearch.index.query.Operator.{}",
                    op.to_uppercase()
                ),
                400,
            ));
        }
    }
    if let Some(v) = req.param("max_concurrent_shard_requests") {
        if v.parse::<i64>().ok().map(|n| n < 1).unwrap_or(false) {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                "maxConcurrentShardRequests must be >= 1",
                400,
            ));
        }
    }
    let o = shared::parse_resolve_params(
        req,
        shared::ResolveOpts2 {
            closed_error: true,
            ..Default::default()
        },
    );
    let names = match shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let raw = String::from_utf8_lossy(&req.body).to_string();
    if raw.trim().is_empty()
        && req.param("source").is_some()
        && req.param("source_content_type").is_none()
    {
        return err_response(ApiError::new(
            "illegal_state_exception",
            "source and source_content_type parameters are required",
            500,
        ));
    }
    let eff = if raw.trim().is_empty() {
        req.param("source").unwrap_or("").to_string()
    } else {
        raw
    };
    if let Some(k) = crate::duplicate_object_key(&eff) {
        return err_response(ApiError::new(
            "json_parse_exception",
            &format!("Duplicate Object property \"{}\"", k),
            400,
        ));
    }
    if eff.trim_start().starts_with('[') {
        return err_response(ApiError::new(
            "parsing_exception",
            "Expected [START_OBJECT] but found [START_ARRAY]",
            400,
        ));
    }
    let mut body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    {
        let allowed_body = [
            "query", "from", "size", "sort", "_source", "stored_fields", "docvalue_fields",
            "fields", "script_fields", "highlight", "aggs", "aggregations", "post_filter",
            "rescore", "min_score", "track_total_hits", "version", "seq_no_primary_term",
            "explain", "profile", "terminate_after", "timeout", "search_after", "collapse",
            "slice", "pit", "runtime_mappings", "indices_boost", "suggest", "_name",
            "stats", "track_scores", "rest_total_hits_as_int", "allow_partial_search_results",
            "batched_reduce_size", "pre_filter_shard_size", "max_concurrent_shard_requests",
            "search_type", "request_cache", "ext", "include_named_queries_score",
            "search_pipeline", "derived",
        ];
        {
            for (k, v) in &body {
                if !allowed_body.contains(&k.as_str()) {
                    let vt = match v {
                        Value::Null => "VALUE_NULL",
                        Value::Bool(_) => "VALUE_BOOLEAN",
                        Value::Number(_) => "VALUE_NUMBER",
                        Value::String(_) => "VALUE_STRING",
                        Value::Array(_) => "START_ARRAY",
                        Value::Object(_) => "START_OBJECT",
                    };
                    return err_response(ApiError::new(
                        "parsing_exception",
                        &format!("Unknown key for a {} in [{}].", vt, k),
                        400,
                    ));
                }
            }
        }
        for k in ["from", "size"] {
            if matches!(body.get(k), Some(Value::Null)) {
                return err_response(ApiError::new(
                    "parsing_exception",
                    &format!("Unknown key for a VALUE_NULL in [{}].", k),
                    400,
                ));
            }
        }
        if let Some(q) = body.get("query") {
            if q.is_null() {
                return err_response(ApiError::new(
                    "parsing_exception",
                    "Unknown key for a VALUE_NULL in [query].",
                    400,
                ));
            }
            if q.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    "query malformed, empty clause found at [1:10]",
                    400,
                ));
            }
        }
        if let Some(v) = req.param("size") {
            if v.parse::<i64>().is_err() {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Failed to parse int parameter [size] with value [{}]", v),
                    400,
                ));
            }
        }
        if let Some(v) = req.param("timeout") {
            let ok = v.parse::<f64>().is_ok()
                || (v.ends_with("ms") || v.ends_with('s') || v.ends_with('m') || v.ends_with('h') || v.ends_with('d'))
                    && v[..v.len() - 1].parse::<f64>().is_ok()
                || ["1ms", "-1"].contains(&v);
            if !ok {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("failed to parse [{}]", v),
                    400,
                ));
            }
        }
    }
    if body.contains_key("pit") {
        if !name.is_empty() {
            return err_response(ApiError::validation(
                "Validation Failed: 1: [indices] cannot be used with point in time;",
            ));
        }
        return handle_pit_search(store, req, Value::Object(body));
    }
    if body
        .get("sort")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|s| {
                s.get("_shard_doc").is_some() || s.as_str() == Some("_shard_doc")
            })
        })
        .unwrap_or(false)
    {
        return err_response(ApiError::validation(
            "Validation Failed: 1: _shard_doc is only supported with point-in-time (PIT). Add a PIT or remove _shard_doc.;",
        ));
    }
    if req.param("scroll").is_some() {
        if body.contains_key("from") {
            return err_response(ApiError::validation(
                "Validation Failed: 1: using [from] is not allowed in a scroll context;",
            ));
        }
        if req.param("request_cache").is_some() {
            return err_response(ApiError::validation(
                "Validation Failed: 1: [request_cache] cannot be used in a scroll context;",
            ));
        }
        if body.get("size").and_then(|v| v.as_i64()) == Some(0) {
            return err_response(ApiError::validation(
                "Validation Failed: 1: [size] cannot be [0] in a scroll context;",
            ));
        }
        if req
            .param("track_total_hits")
            .map(|v| v != "true")
            .unwrap_or(false)
        {
            return err_response(ApiError::validation(
                "Validation Failed: 1: disabling [track_total_hits] is not allowed in a scroll context;",
            ));
        }
        if body.contains_key("search_after") {
            let reason = "`search_after` cannot be used in a scroll context.";
            let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
            e.root_cause = Some(Box::new(ApiError::new(
                "search_exception",
                reason,
                500,
            )));
            return err_response(e);
        }
        if body.contains_key("collapse") {
            let reason = "cannot use `collapse` in a scroll context";
            let mut e = ApiError::new("search_phase_execution_exception", reason, 500);
            e.root_cause = Some(Box::new(ApiError::new(
                "search_exception",
                reason,
                500,
            )));
            return err_response(e);
        }
    }
    let scroll_keep = req.param("scroll").map(|s| s.to_string());
    if let Some(k) = &scroll_keep {
        if let Err(e) = validate_scroll_keepalive(k, false) {
            return err_response(e);
        }
    }
    let body_value = Value::Object(body.clone());
    if let Err(e) = validate_scroll_slice(&body_value, scroll_keep.is_some()) {
        return err_response(e);
    }
    let default_index = names
        .first()
        .cloned()
        .unwrap_or_else(|| name.to_string());
    let sp_state =
        match crate::ingest::prepare_search_pipeline(store, &default_index, &mut body, req) {
            Ok(s) => s,
            Err(e) => return err_response(e),
        };
    let body = Value::Object(body);
    let derived_snap = if body.get("derived").is_some() {
        let mut snap = Vec::new();
        for n in &names {
            if let Some(idx) = store.indices.get(n) {
                snap.push((
                    n.clone(),
                    idx.mappings.get("derived").cloned(),
                    idx.doc_values.clone(),
                ));
            }
        }
        Some(snap)
    } else {
        None
    };
    if let Some(defs) = body.get("derived") {
        for n in &names {
            if let Some(idx) = store.indices.get_mut(n) {
                if let Err(e) = crate::fields::apply_request_derived(idx, defs) {
                    return err_response(e);
                }
            }
        }
    }
    let result = run_search(store, name, &names, &body, req);
    if let Some(snap) = derived_snap {
        for (n, derived, doc_values) in snap {
            if let Some(idx) = store.indices.get_mut(&n) {
                if let Some(d) = derived {
                    if let Some(m) = idx.mappings.as_object_mut() {
                        m.insert("derived".to_string(), d);
                    }
                } else if let Some(m) = idx.mappings.as_object_mut() {
                    m.remove("derived");
                }
                idx.doc_values = doc_values;
            }
        }
    }
    match result {
        Ok(mut v) => {
            if let Err(e) = crate::ingest::apply_search_response(&sp_state, &mut v) {
                return err_response(e);
            }
            if let Err(e) = apply_search_extras(&mut v, req, store, &names, &body) {
                return err_response(e);
            }
            if sp_state.verbose {
                if let Some(m) = v.as_object_mut() {
                    m.insert(
                        "processor_results".to_string(),
                        serde_json::json!(sp_state.request_results),
                    );
                }
            }
            if let Some(_keep) = scroll_keep {
                let page = body_value
                    .get("size")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(10)
                    .max(1) as usize;
                let slice = scroll_slice_of(&body_value);
                let mut raw_len = 0usize;
                if let Some(hits) = v
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    raw_len = hits.len();
                    filter_slice_hits(hits, slice);
                }
                let id = scroll_new_id(store);
                store.scrolls.insert(
                    id.clone(),
                    ScrollState {
                        body: body_value.clone(),
                        names: names.clone(),
                        url_name: name.to_string(),
                        pos: raw_len,
                        page,
                        slice,
                    },
                );
                let mut out = Map::new();
                out.insert("_scroll_id".to_string(), json!(id));
                if let Some(o) = v.as_object() {
                    for (k, val) in o {
                        if k != "_scroll_id" {
                            out.insert(k.clone(), val.clone());
                        }
                    }
                }
                return json_response(200, Value::Object(out));
            }
            json_response(200, v)
        }
        Err(e) => err_response(e),
    }
}


fn mapping_fields_of(idx: &Index) -> Vec<(String, String)> {
    fn walk(props: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String)>) {
        for (k, def) in props {
            let full = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
            let defm = match def.as_object() {
                Some(m) => m,
                None => continue,
            };
            let typ = defm
                .get("type")
                .and_then(|t| t.as_str())
                .map(|t| t.to_string())
                .unwrap_or_else(|| {
                    if defm.contains_key("properties") {
                        "object".to_string()
                    } else {
                        "object".to_string()
                    }
                });
            out.push((full.clone(), typ));
            if let Some(sub) = defm.get("properties").and_then(|p| p.as_object()) {
                walk(sub, &full, out);
            }
        }
    }
    let mut out = vec![];
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        walk(props, "", &mut out);
    }
    out
}

fn resolve_for_query(store: &Store, req: &Req, name: &str) -> Result<Vec<String>, ApiError> {
    let o = shared::parse_resolve_params(
        req,
        shared::ResolveOpts2 {
            closed_error: true,
            ..Default::default()
        },
    );
    shared::resolve_spec2(store, name, &o)
}

fn query_field_type(idx: &Index, field: &str) -> String {
    mapping_fields_of(idx)
        .into_iter()
        .find(|(f, _)| f == field)
        .map(|(_, t)| t)
        .unwrap_or_else(|| "object".to_string())
}

fn q_explanation(q: &str) -> String {
    let mut parts = Vec::new();
    for term in q.split(" AND ") {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        if let Some((field, value)) = term.split_once(':') {
            if field == "title" {
                parts.push(format!("+{}:{}", field, value));
            } else {
                parts.push(format!("+ConstantScore({}:{})", field, value));
            }
        } else {
            parts.push(format!("+{}", term));
        }
    }
    parts.join(" ")
}

pub fn handle_validate_query(store: &mut Store, req: &Req, name: &str) -> Response {
    let names = match resolve_for_query(store, req, name) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let body = match req.json_body() {
        Ok(b) => Value::Object(b),
        Err(e) => return err_response(e),
    };
    let explain = req.param("explain") == Some("true")
        || req.param("rewrite") == Some("true");
    let all_shards = req.param("all_shards") == Some("true");
    let idx_name = names.first().cloned().unwrap_or_else(|| name.to_string());
    let shards = json!({"total": 1, "successful": 1, "failed": 0});
    let query = body.get("query").cloned();
    let qparam = req.param("q").filter(|q| !q.is_empty()).map(|q| q.to_string());
    if query.is_none() && qparam.is_none() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: query cannot be null;",
        ));
    }
    if let Some(o) = body.as_object() {
        if o.keys().any(|k| k != "query") {
            return json_response(200, json!({"valid": false}));
        }
    }
    let idx = match store.indices.get(&idx_name) {
        Some(i) => i,
        None => return err_response(ApiError::not_found(&idx_name)),
    };
    if let Some(q) = qparam {
        let mut depth = 0i64;
        let mut bad = false;
        for ch in q.chars() {
            if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth < 0 {
                    bad = true;
                }
            }
        }
        if bad || depth != 0 {
            if explain {
                let exp = json!({
                    "_shards": shards,
                    "valid": false,
                    "explanations": [{
                        "index": idx_name,
                        "valid": false,
                        "error": format!("Failed to parse query [{}]", q)
                    }]
                });
                return json_response(200, exp);
            }
            return json_response(200, json!({"_shards": shards, "valid": false}));
        }
        let explanation = q_explanation(&q);
        if explain {
            return json_response(
                200,
                json!({
                    "_shards": shards,
                    "valid": true,
                    "explanations": [{
                        "index": idx_name,
                        "valid": true,
                        "explanation": explanation
                    }]
                }),
            );
        }
        return json_response(200, json!({"_shards": shards, "valid": true}));
    }
    let query = query.unwrap();
    let qm = query.as_object().cloned().unwrap_or_default();
    let known = [
        "match_all", "match", "term", "terms", "range", "bool", "match_phrase",
        "multi_match", "wildcard", "fuzzy", "exists", "prefix", "ids", "match_bool_prefix",
        "query_string", "simple_query_string", "nested", "constant_score", "dis_max", "boosting",
        "geo_distance", "geo_bounding_box", "geo_shape", "function_score", "script", "script_score",
        "rank_feature", "knn", "distance_feature", "more_like_this", "regexp", "span_term",
    ];
    let mut unknown: Option<String> = None;
    for k in qm.keys() {
        if !known.contains(&k.as_str()) {
            unknown = Some(k.clone());
            break;
        }
    }
    if let Some(k) = unknown {
        if explain {
            return json_response(
                200,
                json!({
                    "valid": false,
                    "error": format!("ParsingException[unknown query [{}]]", k)
                }),
            );
        }
        return json_response(200, json!({"valid": false}));
    }
    let mut shard_error: Option<String> = None;
    if let Some(term) = qm.get("term").and_then(|t| t.as_object()) {
        if let Some((field, val)) = term.iter().next() {
            let ft = query_field_type(idx, field);
            if is_numeric_type(&ft) {
                if let Some(sval) = val.as_str() {
                    if sval.parse::<f64>().is_err() {
                        shard_error = Some(format!(
                            "NumberFormatException[For input string: \"{}\"]",
                            sval
                        ));
                    }
                }
            }
        }
    }
    if let Some(err) = shard_error {
        if explain {
            let mut exp = json!({
                "index": idx_name,
                "valid": false,
                "error": err
            });
            if all_shards {
                exp["shard"] = json!(0);
            }
            return json_response(
                200,
                json!({"_shards": shards, "valid": false, "explanations": [exp]}),
            );
        }
        return json_response(200, json!({"_shards": shards, "valid": false}));
    }
    if explain {
        let explanation = if let Some(m) = qm.get("match").and_then(|v| v.as_object()) {
            if let Some((field, val)) = m.iter().next() {
                let text = val.as_str().map(|s| s.to_string()).unwrap_or_default();
                let toks: Vec<String> = text
                    .split_whitespace()
                    .map(|t| format!("{}:{}", field, t))
                    .collect();
                toks.join(" ")
            } else {
                "MatchAllDocsQuery(*:*)".to_string()
            }
        } else if let Some(t) = qm.get("term").and_then(|v| v.as_object()) {
            t.iter()
                .next()
                .map(|(f, v)| format!("{}:{}", f, v.as_str().unwrap_or("")))
                .unwrap_or_default()
        } else if let Some(f) = qm.get("fuzzy").and_then(|v| v.as_object()) {
            f.iter()
                .next()
                .map(|(field, v)| {
                    let value = v
                        .as_str()
                        .or_else(|| v.get("value").and_then(|x| x.as_str()))
                        .unwrap_or("");
                    format!("{}:{}", field, value)
                })
                .unwrap_or_default()
        } else {
            "MatchAllDocsQuery(*:*)".to_string()
        };
        let mut exp = json!({
            "index": idx_name,
            "valid": true,
            "explanation": explanation
        });
        if all_shards {
            exp["shard"] = json!(0);
        }
        return json_response(
            200,
            json!({"_shards": shards, "valid": true, "explanations": [exp]}),
        );
    }
    json_response(200, json!({"_shards": shards, "valid": true}))
}

pub fn handle_field_caps(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "GET" && req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["GET", "POST"]);
    }
    let o = shared::parse_resolve_params(req, shared::ResolveOpts2::default());
    let names = match shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut fields_param = req.param("fields").unwrap_or("").to_string();
    if fields_param.is_empty() {
        if let Ok(b) = req.json_body() {
            if let Some(f) = b.get("fields") {
                match f {
                    Value::String(s) => fields_param = s.clone(),
                    Value::Array(a) => {
                        fields_param = a
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                    }
                    _ => {}
                }
            }
        }
    }
    if fields_param.trim().is_empty() {
        return err_response(ApiError::bad(
            "specified fields can't be null or empty",
        ));
    }
    let include_unmapped = req.param("include_unmapped") == Some("true");

    let metadata: Vec<(&str, Value)> = vec![
        ("_id", json!({"type": "_id", "searchable": true, "aggregatable": true})),
        ("_index", json!({"type": "_index", "searchable": true, "aggregatable": true})),
        ("_source", json!({"type": "_source", "searchable": false, "aggregatable": false})),
        ("_seq_no", json!({"type": "_seq_no", "searchable": true, "aggregatable": true})),
        ("_routing", json!({"type": "_routing", "searchable": true, "aggregatable": false})),
    ];

    let mut patterns: Vec<String> = vec![];
    for p in fields_param.split(',') {
        let p = p.trim();
        if !p.is_empty() {
            patterns.push(p.to_string());
        }
    }

    let mut all_fields: Vec<String> = vec![];
    for n in &names {
        for (f, _) in mapping_fields_of(&store.indices[n]) {
            if !all_fields.contains(&f) {
                all_fields.push(f);
            }
        }
        for f in store.indices[n].field_types.keys() {
            if !all_fields.contains(f) {
                all_fields.push(f.clone());
            }
        }
    }

    let mut selected: Vec<String> = vec![];
    let mut sel_meta: Vec<&str> = vec![];
    for p in &patterns {
        if p.contains('*') || p.contains('?') {
            let re = wildcard_regex(p);
            for f in &all_fields {
                if re.is_match(f) && !selected.contains(f) {
                    selected.push(f.clone());
                }
            }
            for (m, _) in &metadata {
                if re.is_match(m) && !sel_meta.contains(m) {
                    sel_meta.push(m);
                }
            }
        } else {
            if all_fields.contains(p) && !selected.contains(p) {
                selected.push(p.clone());
            }
            if let Some((m, _)) = metadata.iter().find(|(m, _)| *m == p) {
                if !sel_meta.contains(m) {
                    sel_meta.push(m);
                }
            }
        }
    }

    let mut out = Map::new();
    for m in &sel_meta {
        if let Some((_, v)) = metadata.iter().find(|(k, _)| k == m) {
            let mut entry = Map::new();
            entry.insert((*m).to_string(), v.clone());
            out.insert((*m).to_string(), Value::Object(entry));
        }
    }

    for field in &selected {
        let mut types: Vec<(String, Vec<String>)> = vec![];
        let mut present: Vec<String> = vec![];
        for n in &names {
            let idx = &store.indices[n];
            let ft = match idx.field_types.get(field) {
                Some(t) => t.clone(),
                None => match mapping_fields_of(idx).into_iter().find(|(f, _)| f == field) {
                    Some((_, t)) => t,
                    None => continue,
                },
            };
            present.push(n.clone());
            match types.iter_mut().find(|(t, _)| *t == ft) {
                Some((_, idxs)) => idxs.push(n.clone()),
                None => types.push((ft, vec![n.clone()])),
            }
        }
        if include_unmapped {
            for n in &names {
                if !present.contains(n) {
                    match types.iter_mut().find(|(t, _)| t == "unmapped") {
                        Some((_, idxs)) => idxs.push(n.clone()),
                        None => types.push(("unmapped".to_string(), vec![n.clone()])),
                    }
                }
            }
        }
        if types.is_empty() {
            continue;
        }
        let multi = types.len() > 1;
        let mut entry = Map::new();
        for (ft, idxs) in &types {
            let mut cap = if ft == "unmapped" {
                json!({"type": "unmapped", "searchable": false, "aggregatable": false})
            } else {
                let d = mapping_field_def(&store.indices[&idxs[0]], field)
                    .cloned()
                    .unwrap_or_default();
                let index_opt = d.get("index").and_then(|v| v.as_bool()).unwrap_or(true);
                let text_like = text_like_type(ft)
                    || matches!(ft.as_str(), "object" | "nested" | "annotated_text");
                let searchable = index_opt && !matches!(ft.as_str(), "object" | "nested");
                let has_dv = match d.get("doc_values").and_then(|v| v.as_bool()) {
                    Some(b) => b,
                    None => !text_like_type(ft),
                };
                let aggregatable = has_dv && !text_like;
                json!({"type": ft, "searchable": searchable, "aggregatable": aggregatable})
            };
            if multi {
                if let Some(obj) = cap.as_object_mut() {
                    obj.insert("indices".to_string(), json!(idxs));
                }
            }
            entry.insert(ft.clone(), cap);
        }
        out.insert(field.clone(), Value::Object(entry));
    }

    json_response(
        200,
        json!({
            "indices": names,
            "fields": out,
        }),
    )
}

pub fn handle_rank_eval(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" && req.method != "GET" {
        return crate::errors::method_not_allowed(req, &["POST", "GET"]);
    }
    let body = match req.json_body() {
        Ok(b) => Value::Object(b),
        Err(e) => return err_response(e),
    };
    let metric = body.get("metric").and_then(|v| v.as_object());
    let Some(metric) = metric else {
        let reason = "[rank_eval] failed to parse field [metric]";
        let mut e = ApiError::new("x_content_parse_exception", reason, 400);
        e.root_cause = Some(Box::new(ApiError::new(
            "named_object_not_found_exception",
            reason,
            400,
        )));
        return err_response(e);
    };
    let Some((metric_name, metric_opts)) = metric.iter().next() else {
        let reason = "[rank_eval] failed to parse field [metric]";
        let mut e = ApiError::new("x_content_parse_exception", reason, 400);
        e.root_cause = Some(Box::new(ApiError::new(
            "named_object_not_found_exception",
            reason,
            400,
        )));
        return err_response(e);
    };
    let opts = metric_opts.as_object().cloned().unwrap_or_default();
    let k = opts.get("k").and_then(to_f64).map(|n| n as usize).unwrap_or(10);
    let threshold = opts
        .get("relevant_rating_threshold")
        .and_then(to_f64)
        .unwrap_or(1.0);
    let known = matches!(
        metric_name.as_str(),
        "precision" | "recall" | "mean_reciprocal_rank"
    );
    if !known {
        let reason = "[rank_eval] failed to parse field [metric]";
        let mut e = ApiError::new("x_content_parse_exception", reason, 400);
        e.root_cause = Some(Box::new(ApiError::new(
            "named_object_not_found_exception",
            reason,
            400,
        )));
        return err_response(e);
    }
    let o = shared::parse_resolve_params(req, shared::ResolveOpts2::default());
    let names = match shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let result: Result<Value, ApiError> = (|| {
    let requests = body
        .get("requests")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut details = Map::new();
    let mut score_sum = 0.0f64;
    let mut count = 0f64;
    for rq in &requests {
        let Some(rm) = rq.as_object() else { continue };
        let id = rm
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let request = rm.get("request").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let query = request
            .get("query")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_else(|| json!({"match_all": {}}).as_object().unwrap().clone());
        let mut ratings: HashMap<(String, String), f64> = HashMap::new();
        if let Some(list) = rm.get("ratings").and_then(|v| v.as_array()) {
            for r in list {
                let Some(obj) = r.as_object() else { continue };
                let rid = obj.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let ixn = obj
                    .get("_index")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| names.first().map(|s| s.as_str()).unwrap_or(""))
                    .to_string();
                let rating = obj.get("rating").and_then(to_f64).unwrap_or(0.0);
                ratings.insert((ixn, rid), rating);
            }
        }
        let mut hits: Vec<MultiHit> = Vec::new();
        for n in &names {
            let idx = &store.indices[n];
            let extra = precompute_doc_values(idx);
            for doc_id in &idx.order {
                let Some(doc) = idx.docs.get(doc_id) else { continue };
                if idx.pending.contains_key(doc_id) {
                    continue;
                }
                let (ok, score) = eval_doc(idx, doc_id, doc, &extra, &query)?;
                if ok {
                    let pos = idx
                        .order
                        .iter()
                        .position(|x| x == doc_id)
                        .unwrap_or(0) as i64;
                    hits.push(MultiHit {
                        idx_name: n.clone(),
                        id: doc_id.clone(),
                        doc: doc.clone(),
                        score,
                        pos,
                        named: vec![],
                    });
                }
            }
        }
        let specs = parse_sort_specs(request.get("sort"))?;
        if specs.is_empty() {
            hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        } else {
            let mut keyed: Vec<(Vec<(bool, Value)>, MultiHit)> = Vec::with_capacity(hits.len());
            for h in hits {
                let mut cells = Vec::with_capacity(specs.len());
                for sp in &specs {
                    cells.push(compute_sort_cell(store, &h, sp)?);
                }
                keyed.push((cells, h));
            }
            keyed.sort_by(|a, b| {
                for i in 0..specs.len() {
                    let ca = &a.0[i];
                    let cb = &b.0[i];
                    let mut c = cmp_sort_cells(ca, cb);
                    if specs[i].desc && !ca.0 && !cb.0 {
                        c = c.reverse();
                    }
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                Ordering::Equal
            });
            hits = keyed.into_iter().map(|(_, h)| h).collect();
        }
        hits.truncate(k);
        let total_relevant = ratings.values().filter(|r| **r >= threshold).count();
        let mut relevant_retrieved = 0usize;
        let mut first_relevant: Option<usize> = None;
        let mut unrated = Vec::new();
        let mut hit_list = Vec::new();
        for (i, h) in hits.iter().enumerate() {
            let rating = ratings.get(&(h.idx_name.clone(), h.id.clone())).copied();
            if rating.is_none() {
                unrated.push(json!({"_index": h.idx_name, "_id": h.id}));
            }
            if let Some(r) = rating {
                if r >= threshold {
                    relevant_retrieved += 1;
                    if first_relevant.is_none() {
                        first_relevant = Some(i + 1);
                    }
                }
            }
            hit_list.push(json!({
                "hit": {"_index": h.idx_name, "_id": h.id},
                "rating": match rating { Some(r) => json!(r), None => Value::Null }
            }));
        }
        let (metric_score, metric_details) = match metric_name.as_str() {
            "recall" => {
                let s = if total_relevant == 0 {
                    0.0
                } else {
                    relevant_retrieved as f64 / total_relevant as f64
                };
                (
                    s,
                    json!({"recall": {
                        "relevant_docs_retrieved": relevant_retrieved,
                        "relevant_docs": total_relevant
                    }}),
                )
            }
            "mean_reciprocal_rank" => {
                let pos = first_relevant.unwrap_or(0);
                let s = if pos == 0 { 0.0 } else { 1.0 / pos as f64 };
                (
                    s,
                    json!({"mean_reciprocal_rank": {"first_relevant": pos}}),
                )
            }
            _ => {
                let retrieved = hits.len();
                let s = if retrieved == 0 {
                    0.0
                } else {
                    relevant_retrieved as f64 / retrieved as f64
                };
                (
                    s,
                    json!({"precision": {
                        "relevant_docs_retrieved": relevant_retrieved,
                        "docs_retrieved": retrieved
                    }}),
                )
            }
        };
        score_sum += metric_score;
        count += 1.0;
        details.insert(
            id,
            json!({
                "metric_score": metric_score,
                "unrated_docs": unrated,
                "hits": hit_list,
                "metric_details": metric_details
            }),
        );
    }
    let overall = if count == 0.0 { 0.0 } else { score_sum / count };
    Ok(json!({
        "metric_score": overall,
        "details": details,
        "failures": {}
    }))
    })();
    match result {
        Ok(v) => json_response(200, v),
        Err(e) => err_response(e),
    }
}
