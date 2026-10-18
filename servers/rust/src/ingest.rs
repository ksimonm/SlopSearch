use crate::errors::{err_response, json_response, ApiError};
use crate::ingest_script;
use crate::resp::format_number;
use crate::{Req, Store};
use axum::response::Response;
use regex::Regex;
use serde_json::{json, Map, Value};

pub const PROCESSOR_TYPES: &[&str] = &[
    "append",
    "bytes",
    "community_id",
    "convert",
    "copy",
    "csv",
    "date",
    "date_index_name",
    "dissect",
    "dot_expander",
    "drop",
    "fail",
    "fingerprint",
    "foreach",
    "gsub",
    "grok",
    "html_strip",
    "json",
    "join",
    "kv",
    "lowercase",
    "pipeline",
    "remove",
    "remove_by_pattern",
    "rename",
    "script",
    "set",
    "sort",
    "split",
    "split_to_fields",
    "trim",
    "uppercase",
    "urldecode",
    "user_agent",
];

fn common_keys() -> Vec<&'static str> {
    vec!["description", "tag", "if", "ignore_failure", "on_failure"]
}

fn processor_keys(typ: &str) -> Option<Vec<&'static str>> {
    let mut k = common_keys();
    let specific: &[&str] = match typ {
        "set" => &["field", "value", "override", "ignore_empty_value"],
        "remove" => &["field", "exclude_field", "ignore_missing"],
        "rename" => &["field", "target_field", "ignore_missing", "override_target"],
        "copy" => &["source_field", "target_field", "ignore_missing", "override_target", "remove_source"],
        "append" => &["field", "value", "allow_duplicates", "ignore_failure", "media_type"],
        "convert" => &["field", "type", "target_field", "ignore_missing"],
        "lowercase" => &["field", "target_field", "ignore_missing"],
        "uppercase" => &["field", "target_field", "ignore_missing"],
        "trim" => &["field", "target_field", "ignore_missing"],
        "split" => &["field", "separator", "preserve_trailing", "target_field", "ignore_missing"],
        "join" => &["field", "separator", "target_field", "ignore_missing"],
        "gsub" => &["field", "pattern", "replacement", "target_field", "ignore_missing"],
        "sort" => &["field", "order", "target_field", "ignore_missing"],
        "grok" => &["field", "patterns", "pattern_definitions", "trace_match", "ignore_missing"],
        "dissect" => &["field", "pattern", "append_separator", "ignore_missing"],
        "date" => &["field", "formats", "target_field", "timezone", "locale", "output_format", "ignore_missing"],
        "date_index_name" => &["field", "date_formats", "date_rounding", "index_name_prefix", "index_name_format", "timezone", "locale", "ignore_missing"],
        "json" => &["field", "target_field", "add_to_root", "ignore_missing"],
        "kv" => &["field", "field_split", "value_split", "target_field", "prefix", "suffix", "include_keys", "exclude_keys", "trim_key", "trim_value", "strip_brackets", "ignore_missing"],
        "csv" => &["field", "target_fields", "separator", "quote", "trim", "empty_value", "ignore_missing"],
        "split_to_fields" => &["field", "separator", "target_fields", "preserve_trailing", "ignore_missing"],
        "urldecode" => &["field", "target_field", "ignore_missing"],
        "html_strip" => &["field", "target_field", "ignore_missing"],
        "bytes" => &["field", "target_field", "ignore_missing"],
        "fingerprint" => &["fields", "exclude_fields", "target_field", "hash_method", "ignore_missing"],
        "community_id" => &["source_ip_field", "source_port_field", "destination_ip_field", "destination_port_field", "protocol_field", "iana_protocol_number_field", "icmp_type_field", "icmp_code_field", "seed", "target_field"],
        "dot_expander" => &["field", "path", "ignore_missing"],
        "foreach" => &["field", "processor", "ignore_missing"],
        "fail" => &["message"],
        "drop" => &[],
        "script" => &["source", "id", "lang", "params"],
        "pipeline" => &["name"],
        "remove_by_pattern" => &["field_pattern", "exclude_field_pattern"],
        "user_agent" => &["field", "target_field", "properties", "ignore_missing"],
        _ => return None,
    };
    k.extend_from_slice(specific);
    Some(k)
}

fn required_key(typ: &str) -> Option<&'static str> {
    Some(match typ {
        "set" => "field",
        "remove" => return None,
        "rename" => "field",
        "copy" => "source_field",
        "append" => "field",
        "convert" => "field",
        "lowercase" | "uppercase" | "trim" | "sort" | "grok" | "dissect" | "date" | "json"
        | "kv" | "csv" | "split_to_fields" | "urldecode" | "html_strip" | "bytes"
        | "dot_expander" | "user_agent" => "field",
        "split" | "join" | "gsub" => "field",
        "date_index_name" => "field",
        "fingerprint" => return None,
        "community_id" => "source_ip_field",
        "foreach" => "field",
        "fail" => "message",
        "script" => return None,
        "pipeline" => "name",
        "remove_by_pattern" => return None,
        _ => return None,
    })
}

fn has_key_after(typ: &str, key: &str) -> bool {
    matches!(
        (typ, key),
        ("rename", "target_field")
            | ("copy", "target_field")
            | ("append", "value")
            | ("convert", "type")
            | ("split", "separator")
            | ("join", "separator")
            | ("gsub", "pattern")
            | ("gsub", "replacement")
            | ("grok", "patterns")
            | ("dissect", "pattern")
            | ("date", "formats")
            | ("date_index_name", "date_rounding")
            | ("csv", "target_fields")
            | ("split_to_fields", "separator")
            | ("split_to_fields", "target_fields")
            | ("dot_expander", "field")
            | ("kv", "field_split")
            | ("kv", "value_split")
            | ("foreach", "processor")
            | ("script", "source")
    )
}

fn parse_exception(msg: &str) -> ApiError {
    ApiError::new("parse_exception", msg, 400)
}

fn cfg_str<'a>(cfg: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    cfg.get(key).and_then(|v| v.as_str())
}

fn cfg_bool(cfg: &Map<String, Value>, key: &str, default: bool) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn cfg_strings(cfg: &Map<String, Value>, key: &str) -> Vec<String> {
    match cfg.get(key) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => vec![],
    }
}

fn build_inline_pipeline(body: &Map<String, Value>, name: &str) -> Result<Map<String, Value>, ApiError> {
    validate_pipeline_body(body, name)
}

fn validate_pipeline_body(
    body: &Map<String, Value>,
    name: &str,
) -> Result<Map<String, Value>, ApiError> {
    for key in body.keys() {
        if !["description", "version", "processors", "on_failure"].contains(&key.as_str()) {
            let mut e = parse_exception(&format!(
                "pipeline [{}] doesn't support one or more provided configuration parameters [{}]",
                name, key
            ));
            e.fields.insert("processor_type".to_string(), json!(key));
            return Err(e);
        }
    }
    let procs = match body.get("processors") {
        Some(Value::Array(a)) => a.clone(),
        Some(_) => {
            return Err(parse_exception(
                "[processors] required property is missing",
            )
            .field("property_name", json!("processors")))
        }
        None => {
            return Err(parse_exception(
                "[processors] required property is missing",
            )
            .field("property_name", json!("processors")))
        }
    };
    validate_processor_list(&procs, true)?;
    if let Some(of) = body.get("on_failure") {
        if !of.is_array() {
            return Err(parse_exception("[on_failure] list required"));
        }
        validate_processor_list(of.as_array().unwrap(), true)?;
    }
    let mut out = Map::new();
    for (k, v) in body {
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

fn validate_processor_list(list: &[Value], top: bool) -> Result<(), ApiError> {
    for p in list {
        validate_processor(p, top)?;
    }
    Ok(())
}

fn validate_processor(p: &Value, _top: bool) -> Result<(), ApiError> {
    let cfg = match p {
        Value::Object(m) => m,
        _ => return Err(parse_exception("processor must be an object")),
    };
    if cfg.len() != 1 {
        let keys: Vec<&String> = cfg.keys().collect();
        if keys.len() > 1 {
            for k in keys {
                if processor_keys(k).is_some() {
                    return Err(parse_exception(&format!(
                        "processor [{}] doesn't support one or more provided configuration parameters",
                        k
                    )));
                }
            }
        }
        return Err(parse_exception("processor must have exactly one type"));
    }
    let (typ, cfgv) = cfg.iter().next().unwrap();
    let keys = match processor_keys(typ) {
        Some(k) => k,
        None => {
            let mut e = parse_exception(&format!("No processor type exists with name [{}]", typ));
            e.fields.insert("processor_type".to_string(), json!(typ));
            return Err(e);
        }
    };
    let c = match cfgv {
        Value::Object(m) => m,
        _ => {
            let mut e = parse_exception(&format!("processor [{}] config must be an object", typ));
            e.fields.insert("processor_type".to_string(), json!(typ));
            return Err(e);
        }
    };
    for k in c.keys() {
        if !keys.contains(&k.as_str()) {
            let mut e = parse_exception(&format!(
                "processor [{}] doesn't support one or more provided configuration parameters [{}]",
                typ, k
            ));
            e.fields.insert("processor_type".to_string(), json!(typ));
            return Err(e);
        }
    }
    let mut required: Vec<&str> = Vec::new();
    if let Some(mut r) = required_key(typ) {
        required.push(r);
    }
    if typ == "set" {
        required.push("value");
    }
    if typ == "rename" {
        required.push("target_field");
    }
    if typ == "copy" {
        required.push("target_field");
    }
    if typ == "append" {
        required.push("value");
    }
    if typ == "dissect" {
        if let Some(pat) = cfg_str(c, "pattern") {
            if let Err(tail) = parse_dissect(pat) {
                let msg = format!("Unable to parse pattern: {} Reason: {}", pat, tail);
                return Err(ApiError::new("exception", &msg, 500)
                    .field("processor_type", json!(typ))
                    .caused_by(ApiError::new("pattern_parse", &msg, 400)));
            }
        }
    }
    if typ == "convert" {
        required.push("type");
    }
    if typ == "split" || typ == "join" {
        required.push("separator");
    }
    if typ == "gsub" {
        required.push("pattern");
        required.push("replacement");
    }
    if typ == "csv" || typ == "split_to_fields" {
        required.push("target_fields");
    }
    if typ == "date_index_name" {
        required.push("date_rounding");
    }
    if typ == "remove" && !c.contains_key("field") && !c.contains_key("exclude_field") {
        required.push("field");
    }
    if typ == "date" && !c.contains_key("formats") {
        let mut e = parse_exception("[formats] required property is missing");
        e.fields.insert("processor_type".to_string(), json!(typ));
        e.fields.insert("property_name".to_string(), json!("formats"));
        return Err(e);
    }
    for r in required {
        if !c.contains_key(r) {
            let mut e = parse_exception(&format!("[{}] required property is missing", r));
            e.fields.insert("processor_type".to_string(), json!(typ));
            e.fields.insert("property_name".to_string(), json!(r));
            return Err(e);
        }
    }
    if typ == "remove_by_pattern" {
        let has_fp = c.contains_key("field_pattern");
        let has_ex = c.contains_key("exclude_field_pattern");
        if has_fp == has_ex {
            return Err(parse_exception(
                "[field_pattern] either field_pattern or exclude_field_pattern must be set",
            ));
        }
        for f in cfg_strings(c, if has_fp { "field_pattern" } else { "exclude_field_pattern" }) {
            if f.starts_with('_') {
                return Err(parse_exception(&format!(
                    "[field_pattern] Validation Failed: 1: field_pattern [{}] must not start with '_';",
                    f
                )));
            }
        }
    }
    if typ == "dot_expander" {
        let f = cfg_str(c, "field").unwrap_or("");
        if !f.contains('.') {
            return Err(parse_exception("[field] field does not contain a dot"));
        }
    }
    if typ == "dissect" {
        if let Some(pat) = cfg_str(c, "pattern") {
            if let Err(tail) = parse_dissect(pat) {
                let msg = format!("Unable to parse pattern: {} Reason: {}", pat, tail);
                return Err(ApiError::new("exception", &msg, 500)
                    .field("processor_type", json!(typ))
                    .caused_by(ApiError::new("pattern_parse", &msg, 400)));
            }
        }
    }
    if typ == "convert" {
        let t = cfg_str(c, "type").unwrap_or("");
        if !["integer", "long", "float", "double", "boolean", "string", "auto", "ip"]
            .contains(&t)
        {
            let mut e = parse_exception(&format!(
                "[type] type [{}] not supported, cannot convert field.",
                t
            ));
            e.fields.insert("processor_type".to_string(), json!(typ));
            e.fields.insert("property_name".to_string(), json!("type"));
            return Err(e);
        }
    }
    if typ == "gsub" {
        if let Some(pat) = cfg_str(c, "pattern") {
            if let Err(e) = Regex::new(pat) {
                let detail = e.to_string();
                let detail = if let Some(idx) = detail.find("regex parse error") {
                    detail[idx + "regex parse error".len()..].trim_start_matches(':').trim()
                } else {
                    detail.as_str()
                };
                let msg = if detail.contains("unclosed character class") {
                    "[pattern] Invalid regex pattern. Unclosed character class".to_string()
                } else {
                    format!("[pattern] Invalid regex pattern. {}", detail)
                };
                let mut err = parse_exception(&msg);
                err.fields.insert("processor_type".to_string(), json!(typ));
                return Err(err);
            }
        }
    }
    if typ == "grok" {
        let patterns = cfg_strings(c, "patterns");
        if patterns.is_empty() {
            let mut e = parse_exception("[patterns] List of patterns must not be empty");
            e.fields.insert("processor_type".to_string(), json!(typ));
            return Err(e);
        }
        let defs = c
            .get("pattern_definitions")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        for p in &patterns {
            if let Err(missing) = check_grok_pattern(p, &defs) {
                let mut e = parse_exception(&format!(
                    "[patterns] Invalid regex pattern found in: [{}]. Unable to find pattern [{}] in Grok's pattern dictionary",
                    p, missing
                ));
                e.fields.insert("processor_type".to_string(), json!(typ));
                return Err(e);
            }
        }
    }
    if typ == "sort" {
        if let Some(o) = cfg_str(c, "order") {
            if o != "asc" && o != "desc" {
                return Err(parse_exception(&format!(
                    "[order] Sort direction [{}] not recognized. Valid values are: [asc, desc]",
                    o
                ))
                .field("processor_type", json!(typ)));
            }
        }
    }
    if typ == "fingerprint" {
        if c.contains_key("fields") && c.contains_key("exclude_fields") {
            return Err(parse_exception(
                "[fields] either fields or exclude_fields can be set",
            )
            .field("processor_type", json!(typ)));
        }
        if let Some(m) = cfg_str(c, "hash_method") {
            if !["MD5@2.16.0", "SHA-1@2.16.0", "SHA-256@2.16.0", "SHA3-256@2.16.0"]
                .contains(&m)
            {
                return Err(parse_exception(
                    "[hash_method] hash method must be MD5@2.16.0, SHA-1@2.16.0, SHA-256@2.16.0 or SHA3-256@2.16.0",
                )
                .field("processor_type", json!(typ)));
            }
        }
    }
    if typ == "csv" {
        if let Some(sep) = cfg_str(c, "separator") {
            if sep.chars().count() != 1 {
                return Err(parse_exception(
                    "[separator] separator has to be single character like , or ;",
                )
                .field("processor_type", json!(typ)));
            }
        }
        if let Some(q) = cfg_str(c, "quote") {
            if q.chars().count() != 1 {
                return Err(parse_exception(
                    "[quote] quote has to be single character like \"",
                )
                .field("processor_type", json!(typ)));
            }
        }
    }
    if typ == "json" && cfg_bool(c, "add_to_root", false) && c.contains_key("target_field") {
        return Err(parse_exception(
            "[target_field] Cannot set a target field while also setting `add_to_root` to true",
        )
        .field("processor_type", json!(typ)));
    }
    if typ == "user_agent" {
        if let Some(props) = c.get("properties").and_then(|v| v.as_array()) {
            let allowed = [
                "NAME", "MAJOR", "MINOR", "PATCH", "BUILD", "VERSION", "OS", "OS_NAME",
                "OS_MAJOR", "OS_MINOR", "OS_FULL", "DEVICE", "ORIGINAL",
            ];
            for p in props {
                let name = p.as_str().unwrap_or("");
                if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                    return Err(parse_exception(&format!(
                        "[properties] illegal property value [{}]. valid values are [NAME, MAJOR, MINOR, PATCH, BUILD, OS, OS_NAME, OS_MAJOR, OS_MINOR, OS_FULL, DEVICE, ORIGINAL]",
                        name
                    ))
                    .field("processor_type", json!(typ)));
                }
            }
        }
    }
    if typ == "script" {
        if c.contains_key("source") && c.contains_key("id") {
            return Err(parse_exception(
                "must only use one of [source, id] when specifying a script",
            )
            .field("processor_type", json!(typ)));
        }
        if let Some(lang) = cfg_str(c, "lang") {
            if lang == "mustache" {
                return Err(parse_exception(
                    "mustache engine does not know how to handle context [ingest]",
                )
                .field("processor_type", json!(typ)));
            }
        }
        if let Some(src) = cfg_str(c, "source") {
            if let Err(mut e) = ingest_script::compile_check(src) {
                if src.trim_end().ends_with('=') {
                    e.fields.insert(
                        "script_stack".to_string(),
                        json!([src, "        ^---- HERE"]),
                    );
                }
                e.fields.insert("processor_type".to_string(), json!("script"));
                attach_tag(&mut e, c);
                return Err(e);
            }
        }
    }
    if let Some(cond) = c.get("if") {
        let src = if let Some(s) = cond.as_str() {
            Some(s.to_string())
        } else if let Some(o) = cond.as_object() {
            o.get("source").and_then(|v| v.as_str()).map(|s| s.to_string())
        } else {
            None
        };
        if let Some(src) = src {
            if let Err(mut e) = ingest_script::compile_check(&src) {
                e.fields.insert("processor_type".to_string(), json!(typ));
                attach_tag(&mut e, c);
                return Err(e);
            }
        }
    }
    if let Some(of) = c.get("on_failure") {
        if let Some(arr) = of.as_array() {
            if arr.is_empty() {
                return Err(parse_exception(
                    "[on_failure] processors list cannot be empty",
                )
                .field("processor_type", json!(typ)));
            }
            validate_processor_list(arr, false)?;
        } else {
            return Err(parse_exception("[on_failure] processors list is not an array"));
        }
    }
    Ok(())
}

fn attach_tag(e: &mut ApiError, cfg: &Map<String, Value>) {
    if let Some(tag) = cfg_str(cfg, "tag") {
        e.fields.insert("processor_tag".to_string(), json!(tag));
    }
}

pub fn handle_ingest(store: &mut Store, req: &Req, segs: &[String]) -> Response {
    if segs.is_empty() {
        return json_response(404, json!({}));
    }
    if segs[0] == "processor" {
        if segs.len() == 2 && req.method == "GET" {
            if segs[1] == "grok" {
                return json_response(200, grok_patterns_json());
            }
            if PROCESSOR_TYPES.contains(&segs[1].as_str()) || segs[1] == "grok" {
                return json_response(200, json!({"type": segs[1]}));
            }
            return json_response(404, json!({}));
        }
        return json_response(404, json!({}));
    }
    if segs[0] != "pipeline" {
        return json_response(404, json!({}));
    }
    match req.method.as_str() {
        "GET" if segs.len() == 1 => {
            let out: Map<String, Value> = store
                .pipelines
                .iter()
                .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                .collect();
            json_response(200, Value::Object(out))
        }
        "GET" if segs.len() == 2 => {
            let name = &segs[1];
            if name.contains(',') || name.contains('*') || name.contains('?') {
                let out: Map<String, Value> = store
                    .pipelines
                    .iter()
                    .filter(|(k, _)| name.split(',').any(|part| glob_match(part, k)))
                    .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                    .collect();
                return json_response(200, Value::Object(out));
            }
            match store.pipelines.get(name) {
                Some(p) => json_response(200, json!({name: Value::Object(p.clone())})),
                None => json_response(404, json!({})),
            }
        }
        "PUT" | "POST" if segs.len() == 2 && segs[1] != "_simulate" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            let name = &segs[1];
            match build_inline_pipeline(&body, name) {
                Ok(p) => {
                    store.pipelines.insert(name.clone(), p);
                    json_response(200, json!({"acknowledged": true}))
                }
                Err(e) => err_response(e),
            }
        }
        "DELETE" if segs.len() == 2 => {
            let name = &segs[1];
            if store.pipelines.remove(name).is_some() {
                json_response(200, json!({"acknowledged": true}))
            } else {
                err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("pipeline [{}] is missing", name),
                    404,
                ))
            }
        }
        "POST" if segs.len() == 2 && segs[1] == "_simulate" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            simulate(store, req, body, None)
        }
        "POST" if segs.len() == 3 && segs[2] == "_simulate" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            let name = segs[1].clone();
            if !store.pipelines.contains_key(&name) {
                return err_response(ApiError::bad(&format!(
                    "pipeline [{}] does not exist",
                    name
                )));
            }
            simulate(store, req, body, Some(name))
        }
        _ => json_response(404, json!({})),
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern == "_all" {
        return true;
    }
    let mut p = String::new();
    for c in pattern.chars() {
        match c {
            '*' => p.push_str(".*"),
            '?' => p.push('.'),
            _ => {
                if ".+()|[]{}^$\\".contains(c) {
                    p.push('\\');
                }
                p.push(c)
            }
        }
    }
    Regex::new(&format!("^{}$", p))
        .map(|r| r.is_match(name))
        .unwrap_or(false)
}

fn simulate(
    store: &Store,
    req: &Req,
    body: Map<String, Value>,
    stored: Option<String>,
) -> Response {
    let verbose = req.bool_param("verbose");
    let (name, pipeline) = if let Some(n) = stored {
        let p = store.pipelines.get(&n).cloned().unwrap_or_default();
        (n, p)
    } else {
        let pv = match body.get("pipeline") {
            Some(Value::Object(m)) => m.clone(),
            _ => {
                return err_response(parse_exception(
                    "[pipeline] required property is missing",
                ))
            }
        };
        match build_inline_pipeline(&pv, "_simulate_pipeline") {
            Ok(p) => ("_simulate_pipeline".to_string(), p),
            Err(e) => return err_response(e),
        }
    };
    let docs = match body.get("docs") {
        Some(Value::Array(a)) => a.clone(),
        _ => return err_response(parse_exception("[docs] required property is missing")),
    };
    if docs.is_empty() {
        return err_response(ApiError::bad("must specify at least one document in [docs]"));
    }
    for d in &docs {
        if d.as_object().map(|m| m.contains_key("_source")).unwrap_or(false) == false {
            return err_response(parse_exception(
                "[_source] required property is missing",
            ));
        }
    }
    let processors = pipeline
        .get("processors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let pipeline_on_failure = pipeline.get("on_failure").cloned();
    let mut results = Vec::new();
    for d in docs {
        let dobj = d.as_object().unwrap();
        let mut source = dobj
            .get("_source")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut meta = Map::new();
        for key in [
            "_index",
            "_id",
            "_routing",
            "_version",
            "_version_type",
            "_if_seq_no",
            "_if_primary_term",
        ] {
            if let Some(v) = dobj.get(key) {
                if key == "_version" {
                    meta.insert(key.to_string(), Value::String(format_value(v)));
                } else {
                    meta.insert(key.to_string(), v.clone());
                }
            }
        }
        if !meta.contains_key("_index") {
            meta.insert("_index".to_string(), json!("_index"));
        }
        if !meta.contains_key("_id") {
            meta.insert("_id".to_string(), json!("_id"));
        }
        let mut ing = Map::new();
        ing.insert("timestamp".to_string(), json!(now_iso()));
        meta.insert("_ingest".to_string(), Value::Object(ing));
        let mut stack = vec![name.clone()];
        match run_list(
            store,
            &name,
            &processors,
            &mut meta,
            &mut source,
            verbose,
            &mut stack,
            pipeline_on_failure.as_ref(),
        ) {
            Ok((dropped, proc_results)) => {
                if dropped {
                    if verbose {
                        results.push(json!({"processor_results": proc_results}));
                    } else {
                        results.push(Value::Null);
                    }
                } else {
                    let mut doc = Map::new();
                    for (k, v) in &meta {
                        if k == "_ingest" {
                            continue;
                        }
                        doc.insert(k.clone(), v.clone());
                    }
                    let mut ing_out = meta
                        .get("_ingest")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    if !verbose {
                        ing_out.remove("pipeline");
                        ing_out.remove("_value");
                        ing_out.retain(|k, _| !k.starts_with("on_failure_"));
                    } else if !ing_out.contains_key("pipeline") {
                        ing_out.insert("pipeline".to_string(), json!(name));
                    }
                    doc.insert("_ingest".to_string(), Value::Object(ing_out));
                    doc.insert("_source".to_string(), Value::Object(source));
                    if verbose {
                        results.push(json!({"processor_results": proc_results}));
                    } else {
                        results.push(json!({"doc": Value::Object(doc)}));
                    }
                }
            }
            Err(e) => {
                if e.fields.contains_key("__fatal") {
                    let mut e = e;
                    e.fields.remove("__fatal");
                    e.fields.remove("__results");
                    return err_response(e);
                }
                if verbose {
                    let mut e = e;
                    let proc_results = e
                        .fields
                        .remove("__results")
                        .and_then(|v| match v {
                            Value::Array(a) => Some(a),
                            _ => None,
                        })
                        .unwrap_or_default();
                    results.push(json!({
                        "processor_results": proc_results,
                        "error": err_value(&e)
                    }));
                } else {
                    results.push(json!({"error": err_value(&e)}));
                }
            }
        }
    }
    json_response(200, json!({"docs": results}))
}

fn format_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn err_value(e: &ApiError) -> Value {
    e.to_value().get("error").cloned().unwrap_or(Value::Null)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as i64, d as i64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn now_iso() -> String {
    let ms = now_millis();
    fmt_iso(ms, None)
}

fn fmt_iso(ms: i64, tz_min: Option<i64>) -> String {
    let (local, suffix) = match tz_min {
        Some(t) => {
            let local = ms + t * 60000;
            let sign = if t >= 0 { '+' } else { '-' };
            let at = t.abs();
            (local, format!("{}{:02}:{:02}", sign, at / 60, at % 60))
        }
        None => (ms, "Z".to_string()),
    };
    let days = local.div_euclid(86_400_000);
    let rem = local.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3_600_000;
    let mm = (rem / 60_000) % 60;
    let ss = (rem / 1000) % 60;
    let milli = rem % 1000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}",
        y, m, d, hh, mm, ss, milli, suffix
    )
}

fn run_list(
    store: &Store,
    name: &str,
    list: &[Value],
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
    verbose: bool,
    stack: &mut Vec<String>,
    pipeline_on_failure: Option<&Value>,
) -> Result<(bool, Vec<Value>), ApiError> {
    let mut results = Vec::new();
    for p in list {
        let obj = match p.as_object() {
            Some(o) => o,
            None => continue,
        };
        let (typ, cfgv) = match obj.iter().next() {
            Some((t, c)) => (t.clone(), c),
            None => continue,
        };
        let cfg = cfgv.as_object().cloned().unwrap_or_default();
        {
            let ing = meta
                .entry("_ingest".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = ing {
                m.insert("pipeline".to_string(), json!(name));
            }
        }
        let cond_value = match cfg.get("if") {
            Some(v) => {
                let (csrc, cparams) = condition_parts(v);
                match ingest_script::eval_condition(&csrc, &cparams, meta, source) {
                    Ok(b) => Some((csrc, b)),
                    Err(e) => {
                        if cfg_bool(&cfg, "ignore_failure", false) {
                            if verbose {
                                results.push(json!({
                                    "processor_type": typ,
                                    "status": "error",
                                    "error": err_value(&e)
                                }));
                            }
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            None => None,
        };
        if let Some((ref csrc, false)) = cond_value {
            let mut r = json!({
                "processor_type": typ,
                "status": "skipped",
                "if": {"condition": csrc, "result": false}
            });
            if let Some(tag) = cfg_str(&cfg, "tag") {
                r.as_object_mut()
                    .unwrap()
                    .insert("tag".to_string(), json!(tag));
            }
            results.push(r);
            continue;
        }
        let mut extra = Vec::new();
        match run_processor(store, &typ, &cfg, meta, source, verbose, stack, &mut extra) {
            Ok(dropped) => {
                if verbose {
                    if typ == "pipeline" && !dropped {
                        results.push(json!({"processor_type": typ, "status": "success"}));
                        results.extend(extra);
                    } else {
                        let mut r = json!({
                            "processor_type": typ,
                            "status": if dropped { "dropped" } else { "success" }
                        });
                        if let Some(tag) = cfg_str(&cfg, "tag") {
                            r.as_object_mut()
                                .unwrap()
                                .insert("tag".to_string(), json!(tag));
                        }
                        if let Some((ref csrc, true)) = cond_value {
                            r.as_object_mut().unwrap().insert(
                                "if".to_string(),
                                json!({"condition": csrc, "result": true}),
                            );
                        }
                        if !dropped {
                            r.as_object_mut()
                                .unwrap()
                                .insert("doc".to_string(), snapshot_doc(meta, source));
                        }
                        results.push(r);
                    }
                }
                if dropped {
                    return Ok((true, results));
                }
            }
            Err(mut e) => {
                if e.fields.contains_key("__fatal") {
                    return Err(e);
                }
                if cfg_bool(&cfg, "ignore_failure", false) {
                    if verbose {
                        results.push(json!({
                            "processor_type": typ,
                            "status": "error",
                            "error": err_value(&e)
                        }));
                    }
                    continue;
                }
                set_failure_meta(meta, &e, &cfg, &typ);
                if let Some(of) = cfg.get("on_failure") {
                    if let Some(arr) = of.as_array() {
                        let (_, _extra) = run_list(
                            store, name, arr, meta, source, verbose, stack, None,
                        )?;
                    }
                    if verbose {
                        results.push(json!({
                            "processor_type": typ,
                            "status": "error",
                            "error": err_value(&e)
                        }));
                    }
                    continue;
                }
                if let Some(of) = pipeline_on_failure {
                    if let Some(arr) = of.as_array() {
                        run_list(store, name, arr, meta, source, verbose, stack, None)?;
                    }
                    return Ok((false, results));
                }
                if verbose {
                    results.push(json!({
                        "processor_type": typ,
                        "status": "error",
                        "error": err_value(&e)
                    }));
                    e.fields.insert("__results".to_string(), Value::Array(results));
                }
                return Err(e);
            }
        }
    }
    Ok((false, results))
}

fn condition_parts(v: &Value) -> (String, Value) {
    if let Some(s) = v.as_str() {
        (s.to_string(), Value::Object(Map::new()))
    } else if let Some(o) = v.as_object() {
        (
            o.get("source").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            o.get("params").cloned().unwrap_or(Value::Object(Map::new())),
        )
    } else {
        (String::new(), Value::Object(Map::new()))
    }
}

fn set_failure_meta(
    meta: &mut Map<String, Value>,
    e: &ApiError,
    cfg: &Map<String, Value>,
    typ: &str,
) {
    let ing = meta
        .entry("_ingest".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(m) = ing {
        let message = if e.typ == "script_exception" {
            e.caused_by
                .as_ref()
                .map(|c| c.reason.clone())
                .unwrap_or_else(|| e.reason.clone())
        } else {
            e.reason.clone()
        };
        m.insert("on_failure_message".to_string(), json!(message));
        m.insert(
            "on_failure_processor_type".to_string(),
            json!(typ),
        );
        if let Some(tag) = cfg_str(cfg, "tag") {
            m.insert("on_failure_processor_tag".to_string(), json!(tag));
        }
    }
}

fn snapshot_doc(meta: &Map<String, Value>, source: &Map<String, Value>) -> Value {
    let mut doc = Map::new();
    for (k, v) in meta {
        doc.insert(k.clone(), v.clone());
    }
    doc.insert("_source".to_string(), Value::Object(source.clone()));
    Value::Object(doc)
}

fn run_processor(
    store: &Store,
    typ: &str,
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
    verbose: bool,
    stack: &mut Vec<String>,
    extra: &mut Vec<Value>,
) -> Result<bool, ApiError> {
    match typ {
        "set" => proc_set(cfg, meta, source),
        "remove" => proc_remove(cfg, meta, source),
        "rename" => proc_rename(cfg, meta, source),
        "copy" => proc_copy(cfg, meta, source),
        "remove_by_pattern" => proc_remove_by_pattern(cfg, source),
        "append" => proc_append(cfg, meta, source),
        "convert" => proc_convert(cfg, meta, source),
        "lowercase" | "uppercase" | "trim" => proc_case(typ, cfg, meta, source),
        "split" => proc_split(cfg, meta, source),
        "join" => proc_join(cfg, meta, source),
        "gsub" => proc_gsub(cfg, meta, source),
        "sort" => proc_sort(cfg, meta, source),
        "grok" => proc_grok(cfg, meta, source),
        "dissect" => proc_dissect(cfg, meta, source),
        "date" => proc_date(cfg, meta, source),
        "date_index_name" => proc_date_index_name(cfg, meta, source),
        "json" => proc_json(cfg, meta, source),
        "kv" => proc_kv(cfg, meta, source),
        "csv" => proc_csv(cfg, meta, source),
        "split_to_fields" => proc_split_to_fields(cfg, meta, source),
        "urldecode" => proc_urldecode(cfg, meta, source),
        "html_strip" => proc_html_strip(cfg, meta, source),
        "bytes" => proc_bytes(cfg, meta, source),
        "fingerprint" => proc_fingerprint(cfg, meta, source),
        "community_id" => proc_community_id(cfg, meta, source),
        "dot_expander" => proc_dot_expander(cfg, source),
        "foreach" => proc_foreach(cfg, meta, source),
        "fail" => proc_fail(cfg, meta, source),
        "drop" => Ok(true),
        "script" => proc_script(store, cfg, meta, source),
        "pipeline" => proc_pipeline(store, cfg, meta, source, verbose, stack, extra),
        "user_agent" => proc_user_agent(cfg, meta, source),
        _ => Ok(false),
    }
}

fn resolve_template(s: &str, meta: &Map<String, Value>, source: &Map<String, Value>) -> String {
    let re = Regex::new(r"\{\{\{?\s*([^}]+?)\s*\}?\}\}").unwrap();
    re.replace_all(s, |caps: &regex::Captures| {
        let path = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        match lookup_path(meta, source, path) {
            Some(v) => format_value(&v),
            None => String::new(),
        }
    })
    .to_string()
}

fn resolve_value(
    v: &Value,
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
) -> Value {
    match v {
        Value::String(s) => Value::String(resolve_template(s, meta, source)),
        Value::Array(a) => Value::Array(a.iter().map(|x| resolve_value(x, meta, source)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), resolve_value(x, meta, source)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn resolve_field_name(
    cfg: &Map<String, Value>,
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
) -> String {
    let f = cfg_str(cfg, "field").unwrap_or("");
    resolve_template(f, meta, source)
}

fn lookup_path(
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
    path: &str,
) -> Option<Value> {
    if path.is_empty() {
        return None;
    }
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur: Value = if segs[0].starts_with('_') {
        Value::Object(meta.clone())
    } else {
        Value::Object(source.clone())
    };
    for s in segs {
        cur = match cur {
            Value::Object(m) => m.get(s).cloned()?,
            Value::Array(a) => a.get(s.parse::<usize>().ok()?)?.clone(),
            _ => return None,
        };
    }
    Some(cur)
}

fn source_get(source: &Map<String, Value>, path: &str) -> Option<Value> {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = Value::Object(source.clone());
    for s in segs {
        cur = match cur {
            Value::Object(m) => m.get(s).cloned()?,
            Value::Array(a) => a.get(s.parse::<usize>().ok()?)?.clone(),
            _ => return None,
        };
    }
    Some(cur)
}

fn meta_get(meta: &Map<String, Value>, path: &str) -> Option<Value> {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = Value::Object(meta.clone());
    for s in segs {
        cur = match cur {
            Value::Object(m) => m.get(s).cloned()?,
            _ => return None,
        };
    }
    Some(cur)
}

fn java_type(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "java.lang.String",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "java.lang.Integer"
            } else {
                "java.lang.Double"
            }
        }
        Value::Bool(_) => "java.lang.Boolean",
        Value::Array(_) => "java.util.ArrayList",
        Value::Object(_) => "java.util.HashMap",
        Value::Null => "null",
    }
}

fn set_path(
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
    path: &str,
    val: Value,
) -> Result<(), ApiError> {
    let segs: Vec<String> = path.split('.').map(|s| s.to_string()).collect();
    if segs.is_empty() {
        return Ok(());
    }
    if segs[0].starts_with('_') {
        let mut root = Value::Object(std::mem::take(meta));
        set_nested_checked(&mut root, &segs, val, path)?;
        *meta = match root {
            Value::Object(m) => m,
            _ => Map::new(),
        };
    } else {
        let mut root = Value::Object(std::mem::take(source));
        set_nested_checked(&mut root, &segs, val, path)?;
        *source = match root {
            Value::Object(m) => m,
            _ => Map::new(),
        };
    }
    Ok(())
}

fn set_nested_checked(
    root: &mut Value,
    segs: &[String],
    val: Value,
    full: &str,
) -> Result<(), ApiError> {
    if segs.is_empty() {
        *root = val;
        return Ok(());
    }
    if segs.len() == 1 {
        if !root.is_object() {
            *root = Value::Object(Map::new());
        }
        root.as_object_mut().unwrap().insert(segs[0].clone(), val);
        return Ok(());
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let obj = root.as_object_mut().unwrap();
    let entry = obj
        .entry(segs[0].clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        return Err(ApiError::bad(&format!(
            "cannot set [{}] with parent object of type [{}] as part of path [{}]",
            segs[1],
            java_type(entry),
            full
        )));
    }
    set_nested_checked(entry, &segs[1..], val, full)
}

fn remove_path(
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
    path: &str,
) -> bool {
    let segs: Vec<&str> = path.split('.').collect();
    if segs[0].starts_with('_') {
        return remove_nested(meta, &segs);
    }
    remove_nested(source, &segs)
}

fn remove_nested(map: &mut Map<String, Value>, segs: &[&str]) -> bool {
    if segs.is_empty() {
        return false;
    }
    if segs.len() == 1 {
        return map.remove(segs[0]).is_some();
    }
    match map.get_mut(segs[0]) {
        Some(Value::Object(m)) => remove_nested(m, &segs[1..]),
        _ => false,
    }
}

fn set_source_path(
    source: &mut Map<String, Value>,
    path: &str,
    val: Value,
) -> Result<(), ApiError> {
    let segs: Vec<String> = path.split('.').map(|s| s.to_string()).collect();
    let mut root = Value::Object(std::mem::take(source));
    set_nested_checked(&mut root, &segs, val, path)?;
    *source = match root {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    Ok(())
}

fn get_any(
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
    path: &str,
) -> Option<Value> {
    if path.starts_with('_') {
        meta_get(meta, path)
    } else {
        source_get(source, path)
    }
}

fn proc_set(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let raw = cfg.get("value").cloned().unwrap_or(Value::Null);
    let is_template = raw
        .as_str()
        .map(|t| t.contains("{{"))
        .unwrap_or(false);
    let mut val = resolve_value(&raw, meta, source);
    if cfg_bool(cfg, "ignore_empty_value", false) && is_template && is_empty_value(&val) {
        return Ok(false);
    }
    let override_ = cfg_bool(cfg, "override", true);
    if !override_ {
        if let Some(existing) = get_any(meta, source, &field) {
            if !existing.is_null() {
                return Ok(false);
            }
        }
    }
    set_path(meta, source, &field, val)?;
    Ok(false)
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

fn proc_remove(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    if let Some(ex) = cfg_str(cfg, "exclude_field") {
        let ex = resolve_template(ex, meta, source);
        let keep: Vec<(String, Value)> = source
            .iter()
            .filter(|(k, _)| **k == ex)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        source.clear();
        for (k, v) in keep {
            source.insert(k, v);
        }
        return Ok(false);
    }
    let field = resolve_field_name(cfg, meta, source);
    let fields: Vec<String> = match cfg.get("field") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| resolve_template(s, meta, source))
            .collect(),
        _ => vec![field],
    };
    for f in fields {
        if !remove_path(meta, source, &f) {
            if cfg_bool(cfg, "ignore_missing", false) {
                continue;
            }
            return Err(ApiError::bad(&format!("field [{}] doesn't exist", f)));
        }
    }
    Ok(false)
}

fn proc_rename(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = resolve_template(cfg_str(cfg, "target_field").unwrap_or(""), meta, source);
    let val = match get_any(meta, source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!("field [{}] doesn't exist", field)));
        }
    };
    if !cfg_bool(cfg, "override_target", false) && get_any(meta, source, &target).is_some() {
        return Err(ApiError::bad(&format!("field [{}] already exists", target)));
    }
    remove_path(meta, source, &field);
    set_path(meta, source, &target, val)?;
    Ok(false)
}

fn proc_copy(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let srcf = resolve_template(cfg_str(cfg, "source_field").unwrap_or(""), meta, source);
    let target = resolve_template(cfg_str(cfg, "target_field").unwrap_or(""), meta, source);
    if srcf == target {
        return Err(ApiError::bad(
            "source field path and target field path cannot be same",
        ));
    }
    let val = match get_any(meta, source, &srcf) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!("source field [{}] doesn't exist", srcf)));
        }
    };
    if !cfg_bool(cfg, "override_target", false) && get_any(meta, source, &target).is_some() {
        return Err(ApiError::bad(&format!("target field [{}] already exists", target)));
    }
    set_path(meta, source, &target, val)?;
    if cfg_bool(cfg, "remove_source", false) {
        remove_path(meta, source, &srcf);
    }
    Ok(false)
}

fn glob_simple(pattern: &str, name: &str) -> bool {
    let mut re = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            _ => {
                if ".+()|[]{}^$\\".contains(c) {
                    re.push('\\');
                }
                re.push(c);
            }
        }
    }
    re.push('$');
    Regex::new(&re).map(|r| r.is_match(name)).unwrap_or(false)
}

fn proc_remove_by_pattern(
    cfg: &Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let keys: Vec<String> = source.keys().cloned().collect();
    if let Some(pats) = cfg.get("field_pattern") {
        let pats: Vec<String> = match pats {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => vec![],
        };
        for k in keys {
            if pats.iter().any(|p| glob_simple(p, &k)) {
                source.remove(&k);
            }
        }
    } else if let Some(pats) = cfg.get("exclude_field_pattern") {
        let pats: Vec<String> = match pats {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
            _ => vec![],
        };
        for k in keys {
            if !pats.iter().any(|p| glob_simple(p, &k)) {
                source.remove(&k);
            }
        }
    }
    Ok(false)
}

fn proc_append(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = resolve_value(&cfg.get("value").cloned().unwrap_or(Value::Null), meta, source);
    let new_vals: Vec<Value> = match value {
        Value::Array(a) => a,
        v => vec![v],
    };
    let allow_dup = cfg_bool(cfg, "allow_duplicates", true);
    match source_get(source, &field) {
        None => {
            let mut arr = Vec::new();
            for v in new_vals {
                if allow_dup || !arr.contains(&v) {
                    arr.push(v);
                }
            }
            set_source_path(source, &field, Value::Array(arr))?;
        }
        Some(Value::Array(mut a)) => {
            for v in new_vals {
                if allow_dup || !a.contains(&v) {
                    a.push(v);
                }
            }
            set_source_path(source, &field, Value::Array(a))?;
        }
        Some(existing) => {
            if !allow_dup && new_vals.iter().any(|v| *v == existing) {
                return Ok(false);
            }
            let mut arr = vec![existing];
            for v in new_vals {
                if allow_dup || !arr.contains(&v) {
                    arr.push(v);
                }
            }
            set_source_path(source, &field, Value::Array(arr))?;
        }
    }
    Ok(false)
}

fn proc_convert(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let typ = cfg_str(cfg, "type").unwrap_or("auto");
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    if val.is_null() {
        return Err(ApiError::bad(&format!(
            "Field [{}] is null, cannot be converted to type [{}]",
            field, typ
        )));
    }
    let converted = match &val {
        Value::Array(a) => {
            let mut out = Vec::new();
            for v in a {
                out.push(convert_one(v, typ, &field)?);
            }
            Value::Array(out)
        }
        v => convert_one(v, typ, &field)?,
    };
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    set_source_path(source, &target, converted)?;
    Ok(false)
}

fn convert_one(v: &Value, typ: &str, field: &str) -> Result<Value, ApiError> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(_) | Value::Bool(_) => {
            if typ == "string" {
                return Ok(Value::String(format_value(v)));
            }
            if typ == "auto" {
                return Ok(v.clone());
            }
            if typ == "boolean" {
                return Err(ApiError::bad(&format!(
                    "[{}] is not a boolean value, cannot convert to boolean",
                    format_value(v)
                )));
            }
            return Err(ApiError::bad(&format!(
                "unable to convert [{}] to {}",
                format_value(v),
                typ
            )));
        }
        Value::Null => {
            return Err(ApiError::bad(&format!(
                "Field [{}] is null, cannot be converted to type [{}]",
                field, typ
            )))
        }
        _ => {
            return Err(ApiError::bad(&format!(
                "unable to convert [{}] to {}",
                format_value(v),
                typ
            )))
        }
    };
    let bad = || ApiError::bad(&format!("unable to convert [{}] to {}", s, typ));
    match typ {
        "integer" => {
            let parsed = if let Some(hex) = s.strip_prefix("0x") {
                i64::from_str_radix(hex, 16).ok()
            } else if let Some(hex) = s.strip_prefix("-0x") {
                i64::from_str_radix(hex, 16).ok().map(|x| -x)
            } else {
                s.parse::<i64>().ok()
            };
            match parsed {
                Some(n) if n >= i32::MIN as i64 && n <= i32::MAX as i64 && no_space(&s) => {
                    Ok(Value::Number(n.into()))
                }
                Some(_) | None => {
                    let mut e = bad();
                    e = e.caused_by(ApiError::new(
                        "number_format_exception",
                        &format!("For input string: \"{}\"", s),
                        400,
                    ));
                    Err(e)
                }
            }
        }
        "long" => match s.parse::<i64>() {
            Ok(n) if no_space(&s) => Ok(Value::Number(n.into())),
            _ => Err(bad().caused_by(ApiError::new(
                "number_format_exception",
                &format!("For input string: \"{}\"", s),
                400,
            ))),
        },
        "float" | "double" => {
            if s == "NaN" {
                return Ok(Value::String("NaN".to_string()));
            }
            match s.parse::<f64>() {
                Ok(f) if no_space(&s) => match serde_json::Number::from_f64(f) {
                    Some(n) => Ok(Value::Number(n)),
                    None => Ok(Value::String(format_number(f))),
                },
                _ => Err(ApiError::bad(&format!(
                    "unable to convert [{}] to {}",
                    s, typ
                ))),
            }
        }
        "boolean" => {
            if s.eq_ignore_ascii_case("true") {
                Ok(Value::Bool(true))
            } else if s.eq_ignore_ascii_case("false") {
                Ok(Value::Bool(false))
            } else {
                Err(ApiError::bad(&format!(
                    "[{}] is not a boolean value, cannot convert to boolean",
                    s
                )))
            }
        }
        "string" => Ok(Value::String(s)),
        "ip" => match s.parse::<std::net::IpAddr>() {
            Ok(_) => Ok(Value::String(s)),
            Err(_) => Err(ApiError::bad(&format!(
                "[{}] is not a valid ipv4/ipv6 address",
                s
            ))),
        },
        "auto" => Ok(auto_convert(&s)),
        _ => Err(bad()),
    }
}

fn no_space(s: &str) -> bool {
    !s.chars().any(|c| c.is_whitespace())
}

fn auto_convert(s: &str) -> Value {
    if s.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if let Ok(i) = s.parse::<i64>() {
        if !s.contains('.') {
            return Value::Number(i.into());
        }
    }
    if s.contains('.') || s.contains('e') || s.contains('E') {
        if let Ok(f) = s.parse::<f64>() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return Value::Number(n);
            }
        }
    }
    Value::String(s.to_string())
}

fn proc_case(
    typ: &str,
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let apply = |v: &Value| -> Result<Value, ApiError> {
        match v {
            Value::String(s) => Ok(Value::String(match typ {
                "lowercase" => s.to_lowercase(),
                "uppercase" => s.to_uppercase(),
                _ => s.trim().to_string(),
            })),
            Value::Null => {
                if cfg_bool(cfg, "ignore_missing", false) {
                    Ok(Value::Null)
                } else {
                    Err(ApiError::bad(&format!(
                        "field [{}] is null, cannot process it.",
                        field
                    )))
                }
            }
            other => Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(other)
            ))),
        }
    };
    let out = match &val {
        Value::Array(a) => {
            let mut out = Vec::new();
            for v in a {
                if !v.is_string() && !v.is_null() {
                    return Err(ApiError::bad(&format!(
                        "value [{}] of type [{}] in list field [{}] cannot be cast to [java.lang.String]",
                        format_value(v),
                        java_type(v),
                        field
                    )));
                }
                out.push(apply(v)?);
            }
            Value::Array(out)
        }
        v => apply(v)?,
    };
    set_source_path(source, &target, out)?;
    Ok(false)
}

fn proc_split(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let s = match val {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let sep = cfg_str(cfg, "separator").unwrap_or(",");
    let preserve = cfg_bool(cfg, "preserve_trailing", false);
    let mut parts: Vec<String> = if sep == "|" {
        s.chars().map(|c| c.to_string()).collect()
    } else {
        match Regex::new(sep) {
            Ok(re) => re.split(&s).map(|x| x.to_string()).collect(),
            Err(_) => s.split(sep).map(|x| x.to_string()).collect(),
        }
    };
    if s.is_empty() {
        parts = vec![String::new()];
    } else if !preserve {
        while parts.len() > 1 && parts.last().map(|x| x.is_empty()).unwrap_or(false) {
            parts.pop();
        }
    }
    set_source_path(
        source,
        &target,
        Value::Array(parts.into_iter().map(Value::String).collect()),
    )?;
    Ok(false)
}

fn proc_join(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let arr = match val {
        Value::Array(a) => a,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.util.List]",
                field,
                java_type(&other)
            )))
        }
    };
    let sep = cfg_str(cfg, "separator").unwrap_or(",");
    let mut parts = Vec::new();
    for v in arr {
        if v.is_null() {
            return Err(ApiError::new(
                "null_pointer_exception",
                "Cannot invoke \"Object.toString()\" because value is null",
                400,
            ));
        }
        parts.push(format_value(&v));
    }
    set_source_path(source, &target, Value::String(parts.join(sep)))?;
    Ok(false)
}

fn proc_gsub(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    let re = Regex::new(cfg_str(cfg, "pattern").unwrap_or("")).map_err(|_| {
        parse_exception("[pattern] Invalid regex pattern. Unclosed character class")
    })?;
    let repl = cfg_str(cfg, "replacement").unwrap_or("");
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let do_one = |v: &Value| -> Result<Value, ApiError> {
        match v {
            Value::String(s) => Ok(Value::String(
                re.replace_all(s, repl as &str).to_string(),
            )),
            other => Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(other)
            ))),
        }
    };
    let out = match &val {
        Value::Array(a) => {
            let mut out = Vec::new();
            for v in a {
                out.push(do_one(v)?);
            }
            Value::Array(out)
        }
        v => do_one(v)?,
    };
    set_source_path(source, &target, out)?;
    Ok(false)
}

fn proc_sort(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source))
        .unwrap_or_else(|| field.clone());
    let val = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let mut arr = match val {
        Value::Array(a) => a,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.util.List]",
                field,
                java_type(&other)
            )))
        }
    };
    let all_num = arr.iter().all(|v| v.is_number());
    let all_str = arr.iter().all(|v| v.is_string());
    if !all_num && !all_str {
        return Err(ApiError::new(
            "class_cast_exception",
            "cannot cast value to comparable type",
            400,
        ));
    }
    arr.sort_by(|a, b| {
        if all_num {
            let x = a.as_f64().unwrap_or(0.0);
            let y = b.as_f64().unwrap_or(0.0);
            x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a.as_str().unwrap_or("").cmp(b.as_str().unwrap_or(""))
        }
    });
    if cfg_str(cfg, "order") == Some("desc") {
        arr.reverse();
    }
    set_source_path(source, &target, Value::Array(arr))?;
    Ok(false)
}

fn grok_builtin(name: &str) -> Option<&'static str> {
    Some(match name {
        "WORD" => r"\b\w+\b",
        "INT" => r"-?\d+",
        "NUMBER" => r"-?\d+(?:\.\d+)?",
        "BASE10NUM" => r"-?\d+(?:\.\d+)?",
        "IP" => r"\b(?:\d{1,3}\.){3}\d{1,3}\b",
        "IPORHOST" => r"\S+",
        "HOSTNAME" => r"\S+",
        "GREEDYDATA" => r".*",
        "DATA" => r".*?",
        "NOTSPACE" => r"\S+",
        "SPACE" => r"\s*",
        "USER" => r"\S+",
        "USERNAME" => r"\S+",
        "HTTPDATE" => r"\d{2}/\w{3}/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4}",
        "TIMESTAMP_ISO8601" => r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?",
        "LOGLEVEL" => r"(?:TRACE|DEBUG|INFO|WARN|ERROR|FATAL)",
        "COMBINEDAPACHELOG" => "",
        _ => return None,
    })
}

const COMBINED_APACHE: &str = r#"(?<clientip>\S+) (?<ident>\S+) (?<auth>\S+) \[(?<timestamp>[^\]]+)\] "(?<verb>\S+) (?<request>[^"]*) HTTP/(?<httpversion>[^"]+)" (?<response>\d+) (?<bytes>\S+) (?<referrer>"[^"]*") (?<agent>"[^"]*")"#;

fn check_grok_pattern(pattern: &str, defs: &Map<String, Value>) -> Result<(), String> {
    let mut rest = pattern;
    while let Some(start) = rest.find("%{") {
        let after = &rest[start + 2..];
        let end = match after.find('}') {
            Some(e) => e,
            None => return Ok(()),
        };
        let content = &after[..end];
        let name = content.split(':').next().unwrap_or("");
        if grok_builtin(name).is_none() && !defs.contains_key(name) {
            return Err(name.to_string());
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

pub fn grok_patterns_json() -> Value {
    let mut m = Map::new();
    for name in [
        "WORD",
        "INT",
        "NUMBER",
        "BASE10NUM",
        "IP",
        "IPORHOST",
        "HOSTNAME",
        "GREEDYDATA",
        "DATA",
        "NOTSPACE",
        "SPACE",
        "USER",
        "USERNAME",
        "HTTPDATE",
        "TIMESTAMP_ISO8601",
        "LOGLEVEL",
        "COMBINEDAPACHELOG",
    ] {
        if let Some(rx) = grok_builtin(name) {
            m.insert(name.to_string(), json!(rx));
        }
    }
    json!({"patterns": m})
}

struct GrokField {
    name: String,
    typ: String,
    group: String,
}

fn expand_grok(
    pattern: &str,
    defs: &Map<String, Value>,
    fields: &mut Vec<GrokField>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut used: Vec<String> = Vec::new();
    let mut rest = pattern;
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = match after.find('}') {
            Some(e) => e,
            None => {
                out.push_str(&rest[start..]);
                return Ok(out);
            }
        };
        let content = &after[..end];
        rest = &after[end + 1..];
        let parts: Vec<&str> = content.split(':').collect();
        let name = parts[0];
        if name == "COMBINEDAPACHELOG" {
            out.push_str(COMBINED_APACHE);
            for (f, t) in [
                ("clientip", "string"),
                ("ident", "string"),
                ("auth", "string"),
                ("timestamp", "string"),
                ("verb", "string"),
                ("request", "string"),
                ("httpversion", "string"),
                ("response", "string"),
                ("bytes", "string"),
                ("referrer", "string"),
                ("agent", "string"),
            ] {
                fields.push(GrokField {
                    name: f.to_string(),
                    typ: t.to_string(),
                    group: f.to_string(),
                });
            }
            continue;
        }
        let rx: String = if let Some(v) = defs.get(name).and_then(|v| v.as_str()) {
            v.to_string()
        } else if let Some(v) = grok_builtin(name) {
            v.to_string()
        } else {
            return Err(name.to_string());
        };
        let field = parts.get(1).copied().unwrap_or("");
        let typ = parts.get(2).copied().unwrap_or("string");
        let skip = field.is_empty() || field.starts_with('?');
        if skip {
            out.push_str("(?:");
            out.push_str(&rx);
            out.push(')');
            continue;
        }
        let group = field.replace('.', "_");
        let group = if group.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            format!("g{}", group)
        } else {
            group
        };
        if used.contains(&field.to_string()) {
            out.push_str("(?:");
            out.push_str(&rx);
            out.push(')');
        } else {
            used.push(field.to_string());
            out.push_str(&format!("(?P<{}>{})", group, rx));
            fields.push(GrokField {
                name: field.to_string(),
                typ: typ.to_string(),
                group,
            });
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn proc_grok(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let msg = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let patterns = cfg_strings(cfg, "patterns");
    let defs = cfg
        .get("pattern_definitions")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    for (idx, pat) in patterns.iter().enumerate() {
        let mut fields = Vec::new();
        let rx = match expand_grok(pat, &defs, &mut fields) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let re = match Regex::new(&rx) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(caps) = re.captures(&msg) {
            if fields.is_empty() {
                for name in re.capture_names().flatten() {
                    if let Some(m) = caps.name(name) {
                        set_source_path(source, name, json!(m.as_str()))?;
                    }
                }
            }
            for f in &fields {
                if let Some(m) = caps.name(&f.group) {
                    let raw = m.as_str();
                    let val = match f.typ.as_str() {
                        "int" | "long" => raw
                            .parse::<i64>()
                            .map(|n| json!(n))
                            .unwrap_or_else(|_| json!(raw)),
                        "float" | "double" => raw
                            .parse::<f64>()
                            .ok()
                            .and_then(|n| serde_json::Number::from_f64(n))
                            .map(Value::Number)
                            .unwrap_or_else(|| json!(raw)),
                        "boolean" => json!(raw == "true"),
                        _ => json!(raw),
                    };
                    set_source_path(source, &f.name, val)?;
                }
            }
            if cfg_bool(cfg, "trace_match", false) {
                let ing = meta
                    .entry("_ingest".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(m) = ing {
                    m.insert("_grok_match_index".to_string(), json!(idx.to_string()));
                }
            }
            return Ok(false);
        }
    }
    Err(ApiError::bad(&format!(
        "Provided Grok expressions do not match field value: [{}]",
        msg
    )))
}

#[derive(Clone, Debug)]
enum DTok {
    Lit(String),
    Key {
        name: String,
        append: bool,
        order: i64,
        pad: bool,
        skip: bool,
        star: bool,
        reference: bool,
    },
}

fn parse_dissect(pattern: &str) -> Result<(Vec<DTok>, Vec<String>), String> {
    let mut toks: Vec<DTok> = Vec::new();
    let mut lit = String::new();
    let mut chars = pattern.chars().peekable();
    let mut seq = 0i64;
    while let Some(c) = chars.next() {
        if c == '%' && chars.peek() == Some(&'{') {
            chars.next();
            let mut content = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    closed = true;
                    break;
                }
                content.push(c2);
            }
            if !closed {
                return Err("Unable to find closing bracket".to_string());
            }
            if !lit.is_empty() {
                toks.push(DTok::Lit(std::mem::take(&mut lit)));
            }
            let mut name = content.clone();
            let mut append = false;
            let mut order = -1i64;
            let mut pad = false;
            let mut skip = false;
            let mut star = false;
            let mut reference = false;
            if name.ends_with("->") {
                pad = true;
                name.truncate(name.len() - 2);
            }
            if let Some(idx) = name.find('/') {
                if let Ok(n) = name[idx + 1..].parse::<i64>() {
                    order = n;
                }
                name.truncate(idx);
            }
            if name.starts_with('+') {
                append = true;
                name.remove(0);
            }
            if name.starts_with('?') {
                skip = true;
                name.remove(0);
            } else if name.starts_with('*') {
                star = true;
                name.remove(0);
            } else if name.starts_with('&') {
                reference = true;
                name.remove(0);
            }
            if name.is_empty() {
                skip = true;
            }
            if order < 0 {
                order = seq;
            }
            seq += 1;
            toks.push(DTok::Key {
                name,
                append,
                order,
                pad,
                skip,
                star,
                reference,
            });
        } else {
            lit.push(c);
        }
    }
    if !lit.is_empty() {
        toks.push(DTok::Lit(lit));
    }
    let has_key = toks.iter().any(|t| matches!(t, DTok::Key { .. }));
    if !has_key {
        return Err("Unable to find any keys or delimiters.".to_string());
    }
    let mut stars: Vec<String> = Vec::new();
    for t in &toks {
        if let DTok::Key { name, star, skip, .. } = t {
            if *star && !*skip {
                stars.push(name.clone());
            }
        }
    }
    let mut bad_refs: Vec<String> = Vec::new();
    for t in &toks {
        if let DTok::Key { name, reference, .. } = t {
            if *reference && !stars.contains(name) {
                bad_refs.push(name.clone());
            }
        }
    }
    if !bad_refs.is_empty() {
        bad_refs.sort();
        bad_refs.dedup();
        return Err(format!(
            "Found invalid key/reference associations: '{}' Please ensure each '*<key>' is matched with a matching '&<key>",
            bad_refs.join("', '")
        ));
    }
    Ok((toks, stars))
}

fn proc_dissect(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let pattern = cfg_str(cfg, "pattern").unwrap_or("");
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let src_text = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let toks = match parse_dissect(pattern) {
        Ok((t, _)) => t,
        Err(_) => {
            return Err(ApiError::bad(
                "Unable to find match for dissect pattern",
            ))
        }
    };
    if src_text.is_empty() {
        return Err(ApiError::new(
            "find_match",
            &format!(
                "Unable to find match for dissect pattern: {} against source: {}",
                pattern, src_text
            ),
            400,
        ));
    }
    let mut pos = 0usize;
    let mut i = 0usize;
    let mut resolved: Vec<(String, String, i64, bool)> = Vec::new();
    let mut star_val: Option<String> = None;
    let chars: Vec<char> = src_text.chars().collect();
    let byte_at = |p: usize| -> usize {
        chars[..p.min(chars.len())].iter().map(|c| c.len_utf8()).sum()
    };
    while i < toks.len() {
        match &toks[i] {
            DTok::Lit(l) => {
                let bpos = byte_at(pos);
                if !src_text[bpos..].starts_with(l.as_str()) {
                    return dissect_nomatch(pattern, &src_text);
                }
                pos += l.chars().count();
                i += 1;
            }
            DTok::Key {
                name,
                append,
                order,
                pad,
                skip,
                star,
                reference,
            } => {
                let next_lit = match toks.get(i + 1) {
                    Some(DTok::Lit(l)) => Some(l.clone()),
                    _ => None,
                };
                let bpos = byte_at(pos);
                let raw: String;
                if let Some(l) = next_lit {
                    if l.is_empty() {
                        raw = src_text[bpos..].to_string();
                        pos = chars.len();
                        i += 1;
                    } else {
                        let found = src_text[bpos..].find(&l);
                        match found {
                            Some(off) => {
                                raw = src_text[bpos..bpos + off].to_string();
                                pos += src_text[bpos..bpos + off].chars().count();
                                pos += l.chars().count();
                                i += 2;
                            }
                            None => return dissect_nomatch(pattern, &src_text),
                        }
                    }
                } else {
                    raw = src_text[bpos..].to_string();
                    pos = chars.len();
                    i += 1;
                }
                let mut cap = raw;
                if *pad {
                    cap = cap.trim_end().to_string();
                    while pos < chars.len() && chars[pos].is_whitespace() {
                        pos += 1;
                    }
                }
                if *skip {
                    continue;
                }
                if *star {
                    star_val = Some(cap);
                    continue;
                }
                if *reference {
                    let key = star_val.clone().unwrap_or_default();
                    resolved.push((key, cap, *order, *append));
                    continue;
                }
                resolved.push((name.clone(), cap, *order, *append));
            }
        }
    }
    let sep = cfg_str(cfg, "append_separator").unwrap_or("");
    let mut appended: Vec<(String, Vec<(i64, String)>)> = Vec::new();
    for (name, val, order, append) in resolved {
        if append {
            if let Some(entry) = appended.iter_mut().find(|(n, _)| *n == name) {
                entry.1.push((order, val));
            } else {
                appended.push((name, vec![(order, val)]));
            }
        } else {
            set_source_path(source, &name, json!(val))?;
        }
    }
    for (name, mut vals) in appended {
        vals.sort_by_key(|(o, _)| *o);
        let joined = vals
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
            .join(sep);
        set_source_path(source, &name, json!(joined))?;
    }
    Ok(false)
}

fn dissect_nomatch<T>(pattern: &str, source: &str) -> Result<T, ApiError> {
    Err(ApiError::new(
        "find_match",
        &format!(
            "Unable to find match for dissect pattern: {} against source: {}",
            pattern, source
        ),
        400,
    ))
}

fn parse_iso_ms(s: &str) -> Option<(i64, Option<i64>)> {
    let s = s.trim();
    let (date_part, time_part) = if let Some(idx) = s.find('T') {
        (&s[..idx], Some(&s[idx + 1..]))
    } else if let Some(idx) = s.find(' ') {
        if s[..idx].chars().all(|c| c.is_ascii_digit() || c == '-')
            && s.len() > idx + 1
        {
            (&s[..idx], Some(&s[idx + 1..]))
        } else {
            (s, None)
        }
    } else {
        (s, None)
    };
    let dparts: Vec<&str> = date_part.split('-').collect();
    if dparts.len() != 3 {
        return None;
    }
    let y: i64 = dparts[0].parse().ok()?;
    let mo: i64 = dparts[1].parse().ok()?;
    let d: i64 = dparts[2].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) * 86_400_000;
    let mut offset: Option<i64> = None;
    if let Some(tp) = time_part {
        let mut t = tp;
        let mut sign = 1i64;
        if t.ends_with('Z') || t.ends_with('z') {
            offset = Some(0);
            t = &t[..t.len() - 1];
        } else if let Some(idx) = t.rfind(['+', '-']) {
            if idx > 0 {
                let off = &t[idx..];
                let neg = off.starts_with('-');
                let body = &off[1..];
                let (hh, mm) = if let Some(colon) = body.find(':') {
                    (
                        body[..colon].parse::<i64>().ok()?,
                        body[colon + 1..].parse::<i64>().ok()?,
                    )
                } else if body.len() == 4 {
                    (
                        body[..2].parse::<i64>().ok()?,
                        body[2..].parse::<i64>().ok()?,
                    )
                } else if body.len() == 2 {
                    (body.parse::<i64>().ok()?, 0)
                } else {
                    return None;
                };
                sign = if neg { -1 } else { 1 };
                offset = Some(sign * (hh * 60 + mm));
                t = &t[..idx];
            }
        }
        let (hms, frac) = match t.find('.') {
            Some(idx) => (&t[..idx], Some(&t[idx + 1..])),
            None => (t, None),
        };
        let hms_parts: Vec<&str> = hms.split(':').collect();
        if hms_parts.is_empty() || hms_parts.len() > 3 {
            return None;
        }
        let hh: i64 = hms_parts[0].parse().ok()?;
        let mm: i64 = if hms_parts.len() > 1 {
            hms_parts[1].parse().ok()?
        } else {
            0
        };
        let ss: i64 = if hms_parts.len() > 2 {
            hms_parts[2].parse().ok()?
        } else {
            0
        };
        if hh > 23 || mm > 59 || ss > 60 {
            return None;
        }
        ms += (hh * 3600 + mm * 60 + ss) * 1000;
        if let Some(f) = frac {
            let mut digits = f.chars().take(3).collect::<String>();
            while digits.len() < 3 {
                digits.push('0');
            }
            ms += digits.parse::<i64>().ok()?;
        }
        if let Some(o) = offset {
            ms -= o * 60_000;
        }
    }
    Some((ms, offset))
}

fn tz_offset(name: &str, ms: i64) -> Result<i64, ApiError> {
    match name {
        "UTC" | "GMT" | "Etc/UTC" | "Z" => Ok(0),
        "Europe/Kyiv" | "Europe/Kiev" => {
            let days = ms.div_euclid(86_400_000);
            let (_, m, d) = civil_from_days(days);
            if (4..=9).contains(&m) || (m == 3 && d >= 29) || (m == 10 && d < 25) {
                Ok(180)
            } else {
                Ok(120)
            }
        }
        "America/New_York" => {
            let days = ms.div_euclid(86_400_000);
            let (_, m, d) = civil_from_days(days);
            if (4..=10).contains(&m) || (m == 3 && d >= 8) || (m == 11 && d < 3) {
                Ok(-240)
            } else {
                Ok(-300)
            }
        }
        other => {
            if let Some(off) = parse_tz_expr(other) {
                Ok(off)
            } else {
                Err(ApiError::bad(&format!("Unknown time-zone ID: {}", other))
                    .caused_by(ApiError::new(
                        "zone_rules_exception",
                        &format!("Unknown time-zone ID: {}", other),
                        400,
                    )))
            }
        }
    }
}

fn parse_tz_expr(s: &str) -> Option<i64> {
    if let Some(rest) = s.strip_prefix('+').or_else(|| s.strip_prefix('-')) {
        let neg = s.starts_with('-');
        let (h, m) = if let Some(idx) = rest.find(':') {
            (rest[..idx].parse::<i64>().ok()?, rest[idx + 1..].parse::<i64>().ok()?)
        } else if rest.len() == 4 {
            (rest[..2].parse::<i64>().ok()?, rest[2..].parse::<i64>().ok()?)
        } else if rest.len() == 2 {
            (rest.parse::<i64>().ok()?, 0)
        } else {
            return None;
        };
        let v = h * 60 + m;
        Some(if neg { -v } else { v })
    } else {
        None
    }
}

fn parse_month_name(s: &str) -> Option<i64> {
    let low = s.to_lowercase();
    let names = [
        ("janvier", 1), ("février", 2), ("fevrier", 2), ("mars", 3), ("avril", 4),
        ("mai", 5), ("juin", 6), ("juillet", 7), ("août", 8), ("aout", 8),
        ("septembre", 9), ("octobre", 10), ("novembre", 11), ("décembre", 12), ("decembre", 12),
        ("january", 1), ("february", 2), ("march", 3), ("april", 4), ("may", 5), ("june", 6),
        ("july", 7), ("august", 8), ("september", 9), ("october", 10), ("november", 11), ("december", 12),
        ("jan", 1), ("feb", 2), ("mar", 3), ("apr", 4), ("jun", 6), ("jul", 7), ("aug", 8),
        ("sep", 9), ("sept", 9), ("oct", 10), ("nov", 11), ("dec", 12),
    ];
    names
        .iter()
        .find(|(n, _)| low.starts_with(n))
        .map(|(_, v)| *v)
}

fn parse_with_format(s: &str, fmt: &str) -> Option<i64> {
    let s = s.trim();
    match fmt {
        "ISO8601" | "strict_date_optional_time" => parse_iso_ms(s).map(|(ms, _)| ms),
        "UNIX" => s.parse::<f64>().ok().map(|f| (f * 1000.0).round() as i64),
        "UNIX_MS" => s.parse::<i64>().ok(),
        "TAI64N" => {
            if s.len() < 24 {
                return None;
            }
            let secs = u64::from_str_radix(&s[..16], 16).ok()?;
            let nanos = u64::from_str_radix(&s[16..24], 16).ok()?;
            let unix = secs.wrapping_sub(1u64 << 62).wrapping_sub(10) as i64;
            Some(unix * 1000 + (nanos / 1_000_000) as i64)
        }
        "dd/MM/yyyy" | "dd/MM/yyyy HH:mm" => {
            let (date_s, time_s) = match s.find(' ') {
                Some(i) => (&s[..i], Some(&s[i + 1..])),
                None => (s, None),
            };
            let p: Vec<&str> = date_s.split('/').collect();
            if p.len() != 3 {
                return None;
            }
            let d: i64 = p[0].parse().ok()?;
            let m: i64 = p[1].parse().ok()?;
            let y: i64 = p[2].parse().ok()?;
            let mut ms = days_from_civil(y, m, d) * 86_400_000;
            if fmt.ends_with("HH:mm") {
                let t = time_s?;
                let tp: Vec<&str> = t.split(':').collect();
                if tp.len() < 2 {
                    return None;
                }
                ms += (tp[0].parse::<i64>().ok()? * 60 + tp[1].parse::<i64>().ok()?) * 60_000;
            }
            Some(ms)
        }
        "MM/dd" => {
            let p: Vec<&str> = s.split('/').collect();
            if p.len() != 2 {
                return None;
            }
            let m: i64 = p[0].parse().ok()?;
            let d: i64 = p[1].parse().ok()?;
            let days = now_millis().div_euclid(86_400_000);
            let (y, _, _) = civil_from_days(days);
            Some(days_from_civil(y, m, d) * 86_400_000)
        }
        "dd MMM yyyy" => {
            let p: Vec<&str> = s.split_whitespace().collect();
            if p.len() != 3 {
                return None;
            }
            let d: i64 = p[0].parse().ok()?;
            let m = parse_month_name(p[1])?;
            let y: i64 = p[2].parse().ok()?;
            Some(days_from_civil(y, m, d) * 86_400_000)
        }
        "yyyy-MM-dd" => {
            let p: Vec<&str> = s.split('-').collect();
            if p.len() != 3 {
                return None;
            }
            Some(
                days_from_civil(
                    p[0].parse().ok()?,
                    p[1].parse().ok()?,
                    p[2].parse().ok()?,
                ) * 86_400_000,
            )
        }
        _ => parse_iso_ms(s).map(|(ms, _)| ms),
    }
}

fn parse_date_value(
    v: &Value,
    formats: &[String],
    tz: Option<i64>,
) -> Result<i64, ApiError> {
    if let Value::Number(n) = v {
        if formats.iter().any(|f| f == "UNIX_MS") {
            return Ok(n.as_i64().unwrap_or(0));
        }
        if let Some(f) = n.as_f64() {
            return Ok((f * 1000.0).round() as i64);
        }
    }
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(date_parse_error(&format_value(v), formats)),
    };
    for fmt in formats {
        if let Some(ms) = parse_with_format(&s, fmt) {
            let naive = fmt == "dd/MM/yyyy"
                || fmt == "dd/MM/yyyy HH:mm"
                || fmt == "MM/dd"
                || fmt == "dd MMM yyyy"
                || fmt == "yyyy-MM-dd";
            let is_iso_naive = matches!(fmt.as_str(), "ISO8601" | "strict_date_optional_time")
                && parse_iso_ms(&s).map(|(_, off)| off.is_none()).unwrap_or(false);
            if let Some(t) = tz {
                if naive || is_iso_naive {
                    return Ok(ms - t * 60_000);
                }
            }
            return Ok(ms);
        }
    }
    Err(date_parse_error(&s, formats))
}

fn date_parse_error(value: &str, formats: &[String]) -> ApiError {
    let fmt = formats.first().cloned().unwrap_or_default();
    ApiError::bad(&format!("unable to parse date [{}]", value)).caused_by(
        ApiError::bad(&format!(
            "failed to parse date field [{}] with format [{}]",
            value, fmt
        ))
        .caused_by(ApiError::new(
            "date_time_parse_exception",
            &format!("failed to parse date field [{}]", value),
            400,
        )),
    )
}

fn fmt_custom(ms: i64, fmt: &str, tz: Option<i64>) -> String {
    let local = ms + tz.unwrap_or(0) * 60_000;
    let days = local.div_euclid(86_400_000);
    let rem = local.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3_600_000;
    let mi = (rem / 60_000) % 60;
    let ss = (rem / 1000) % 60;
    match fmt {
        "yyyy/MM/dd" => format!("{:04}/{:02}/{:02}", y, m, d),
        "yyyy-MM-dd" => format!("{:04}-{:02}-{:02}", y, m, d),
        "yyyy" => format!("{:04}", y),
        _ => fmt_iso(ms, tz),
    }
    .replace("{hh}", &format!("{:02}", hh))
    .replace("{mm}", &format!("{:02}", mi))
    .replace("{ss}", &format!("{:02}", ss))
}

fn date_value_is_naive(v: &Value, formats: &[String]) -> bool {
    let iso = formats.iter().any(|f| {
        matches!(f.as_str(), "ISO8601" | "strict_date_optional_time" | "date_optional_time")
    });
    if !iso {
        let named = formats.iter().any(|f| {
            matches!(f.as_str(), "UNIX" | "UNIX_MS" | "TAI64N")
        });
        return !named;
    }
    match v.as_str() {
        Some(s) => {
            !(s.ends_with('Z')
                || s.contains('+')
                || s.get(10..).map(|r| r.contains('-')).unwrap_or(false))
        }
        None => false,
    }
}

fn proc_date(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let formats = cfg_strings(cfg, "formats");
    let tz_name = cfg_str(cfg, "timezone");
    if let Some(t) = tz_name {
        tz_offset(t, now_millis())?;
    }
    let value = match get_any(meta, source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let ms_naive = parse_date_value(&value, &formats, None)?;
    let tz = match tz_name {
        Some(t) => Some(tz_offset(t, ms_naive)?),
        None => None,
    };
    let ms = match tz {
        Some(off) if date_value_is_naive(&value, &formats) => ms_naive - off * 60_000,
        _ => ms_naive,
    };
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .unwrap_or("@timestamp");
    let out = match cfg_str(cfg, "output_format") {
        Some(f) => fmt_custom(ms, f, tz),
        None => fmt_iso(ms, tz),
    };
    set_path(meta, source, target, json!(out))?;
    Ok(false)
}

fn proc_date_index_name(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let formats = if cfg.contains_key("date_formats") {
        cfg_strings(cfg, "date_formats")
    } else {
        vec!["yyyy-MM-dd'T'HH:mm:ss.SSSXX".to_string()]
    };
    let strict_default = !cfg.contains_key("date_formats");
    let value = match get_any(meta, source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let mut ms = if strict_default {
        let s = match &value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => format_value(&value),
        };
        if !s.contains('.') {
            return Err(date_parse_error(&s, &formats));
        }
        parse_iso_ms(&s)
            .map(|(m, _)| m)
            .ok_or_else(|| date_parse_error(&s, &formats))?
    } else {
        parse_date_value(&value, &formats, None)?
    };
    let rounding = cfg_str(cfg, "date_rounding").unwrap_or("d");
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let _ = ms;
    let date_str = if cfg_str(cfg, "index_name_format") == Some("yyyy") {
        format!("{:04}", y)
    } else {
        format!("{:04}-{:02}-{:02}", y, m, d)
    };
    let fmt = cfg_str(cfg, "index_name_format").unwrap_or("yyyy-MM-dd");
    let tz_suffix = cfg_str(cfg, "timezone").unwrap_or("UTC");
    let prefix = cfg_str(cfg, "index_name_prefix").unwrap_or("");
    let mut index = String::new();
    index.push('<');
    index.push_str(prefix);
    index.push('{');
    index.push_str(&date_str);
    index.push_str("||/");
    index.push_str(rounding);
    index.push('{');
    index.push_str(fmt);
    index.push('|');
    index.push_str(tz_suffix);
    index.push('}');
    index.push('}');
    index.push('>');
    set_path(meta, source, "_index", json!(index))?;
    Ok(false)
}

fn proc_json(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let s = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let parsed = match serde_json::Deserializer::from_str(&s).into_iter::<Value>().next() {
        Some(Ok(v)) => v,
        _ => {
            return Err(ApiError::bad(&format!("Unrecognized token in JSON [{}]", s))
                .caused_by(ApiError::new(
                    "json_parse_exception",
                    "Unexpected character",
                    400,
                )))
        }
    };
    if cfg_bool(cfg, "add_to_root", false) {
        match parsed {
            Value::Object(m) => {
                for (k, v) in m {
                    source.insert(k, v);
                }
                return Ok(false);
            }
            _ => {
                return Err(ApiError::bad(
                    "cannot add non-map fields to root of document",
                ))
            }
        }
    }
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| field.clone());
    set_source_path(source, &target, parsed)?;
    Ok(false)
}

fn proc_kv(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let msg = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let fsplit = cfg_str(cfg, "field_split").unwrap_or("&");
    let vsplit = cfg_str(cfg, "value_split").unwrap_or("=");
    let fre = Regex::new(fsplit).map_err(|_| parse_exception("invalid field_split"))?;
    let vre = Regex::new(vsplit).map_err(|_| parse_exception("invalid value_split"))?;
    let tokens: Vec<String> = fre
        .split(&msg)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err(ApiError::bad(&format!(
            "field [{}] does not contain value_split [{}]",
            field, vsplit
        )));
    }
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| resolve_template(s, meta, source));
    let include = cfg_strings(cfg, "include_keys");
    let exclude = cfg_strings(cfg, "exclude_keys");
    let prefix = cfg_str(cfg, "prefix").unwrap_or("");
    let suffix = cfg_str(cfg, "suffix").unwrap_or("");
    let trim_key = cfg_str(cfg, "trim_key").unwrap_or("");
    let trim_value = cfg_str(cfg, "trim_value").unwrap_or("");
    let strip_brackets = cfg_bool(cfg, "strip_brackets", false);
    let mut parsed: Vec<(String, Value)> = Vec::new();
    for tok in &tokens {
        let m = match vre.find(tok) {
            Some(m) => m,
            None => {
                return Err(ApiError::bad(&format!(
                    "field [{}] does not contain value_split [{}]",
                    field, vsplit
                )))
            }
        };
        let key = &tok[..m.start()];
        let val = &tok[m.end()..];
        if key.is_empty() {
            return Err(ApiError::bad("path cannot be null nor empty"));
        }
        let mut key = key.to_string();
        let mut val = val.to_string();
        if !trim_key.is_empty() {
            key = key
                .trim_matches(|c: char| trim_key.contains(c))
                .to_string();
        }
        if !trim_value.is_empty() {
            val = val
                .trim_matches(|c: char| trim_value.contains(c))
                .to_string();
        }
        if strip_brackets {
            val = strip_surrounding(&val);
        }
        if !include.is_empty() && !include.contains(&key) {
            continue;
        }
        if exclude.contains(&key) {
            continue;
        }
        let key = format!("{}{}{}", prefix, key, suffix);
        if let Some(t) = &target {
            let entry = parsed.iter_mut().find(|(k, _)| *k == key);
            if let Some((_, v)) = entry {
                if let Value::Array(a) = v {
                    a.push(json!(val));
                } else {
                    let old = v.clone();
                    *v = json!([old, val]);
                }
            } else {
                parsed.push((key, json!(val)));
            }
        } else {
            let existing = source_get(source, &key);
            let newval = match existing {
                Some(old) => {
                    if let Value::Array(mut a) = old {
                        a.push(json!(val));
                        Value::Array(a)
                    } else {
                        json!([old, val])
                    }
                }
                None => json!(val),
            };
            set_source_path(source, &key, newval)?;
        }
    }
    if let Some(t) = target {
        let mut obj = Map::new();
        for (k, v) in parsed {
            obj.insert(k, v);
        }
        set_source_path(source, &t, Value::Object(obj))?;
    }
    Ok(false)
}

fn strip_surrounding(s: &str) -> String {
    let pairs = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('"', '"'), ('\'', '\'')];
    for (a, b) in pairs {
        if s.starts_with(a) && s.ends_with(b) && s.len() >= 2 {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn parse_csv(msg: &str, sep: char, quote: char) -> Result<Vec<String>, ApiError> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == quote {
                if chars.peek() == Some(&quote) {
                    chars.next();
                    cur.push(quote);
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == quote && cur.trim().is_empty() {
            cur.clear();
            in_quotes = true;
        } else if c == sep {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if in_quotes {
        return Err(ApiError::bad("Unmatched quote"));
    }
    fields.push(cur);
    Ok(fields)
}

fn proc_csv(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let msg = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let sep = cfg_str(cfg, "separator")
        .and_then(|s| s.chars().next())
        .unwrap_or(',');
    let quote = cfg_str(cfg, "quote")
        .and_then(|s| s.chars().next())
        .unwrap_or('"');
    let trim = cfg_bool(cfg, "trim", false);
    let empty_value = cfg_str(cfg, "empty_value");
    let target_fields = cfg_strings(cfg, "target_fields");
    let mut parts = parse_csv(&msg, sep, quote)?;
    let last_idx = parts.len().saturating_sub(1);
    if trim {
        for (i, p) in parts.iter_mut().enumerate() {
            let t = p.trim_start();
            if i < last_idx {
                *p = t.trim_end().to_string();
            } else {
                *p = t.to_string();
            }
        }
    }
    for (i, name) in target_fields.iter().enumerate() {
        let val = match parts.get(i) {
            Some(s) if !s.is_empty() => json!(s),
            Some(_) => match empty_value {
                Some(e) => json!(e),
                None => continue,
            },
            None => continue,
        };
        set_source_path(source, name, val)?;
    }
    Ok(false)
}

fn proc_split_to_fields(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let msg = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let sep = cfg_str(cfg, "separator").unwrap_or(",");
    let preserve = cfg_bool(cfg, "preserve_trailing", false);
    let re = Regex::new(sep).map_err(|_| parse_exception("invalid separator"))?;
    let mut parts: Vec<String> = re.split(&msg).map(|s| s.to_string()).collect();
    if !preserve {
        while parts.len() > 1 && parts.last().map(|s| s.is_empty()).unwrap_or(false) {
            parts.pop();
        }
    }
    let target_fields = cfg_strings(cfg, "target_fields");
    for (i, name) in target_fields.iter().enumerate() {
        if let Some(v) = parts.get(i) {
            set_source_path(source, name, json!(v))?;
        }
    }
    Ok(false)
}

fn proc_urldecode(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| field.clone());
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let decode = |s: &str| -> Result<Value, ApiError> {
        Ok(Value::String(url_decode(s)?))
    };
    let out = match &value {
        Value::String(s) => decode(s)?,
        Value::Array(a) => {
            let mut v = Vec::new();
            for x in a {
                match x {
                    Value::String(s) => v.push(decode(s)?),
                    _ => v.push(x.clone()),
                }
            }
            Value::Array(v)
        }
        other => other.clone(),
    };
    set_source_path(source, &target, out)?;
    Ok(false)
}

fn url_decode(s: &str) -> Result<String, ApiError> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(ApiError::bad(
                        "URLDecoder: Illegal hex characters in escape (%) pattern - end of string",
                    ));
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        return Err(ApiError::bad(&format!(
                            "URLDecoder: Illegal hex characters in escape (%) pattern - Error at index 0 in: \"{}\"",
                            hex
                        )))
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

fn proc_html_strip(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| field.clone());
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let strip = |s: &str| -> String {
        let script = Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
        let mut t = script.replace_all(s, "\n").to_string();
        let block = Regex::new(r"(?i)</?(?:p|br|div|li|ul|ol|tr|td|th|h[1-6]|table|blockquote|pre|hr)[^>]*/?>").unwrap();
        t = block.replace_all(&t, "\n").to_string();
        let tags = Regex::new(r"<[^>]+>").unwrap();
        t = tags.replace_all(&t, "").to_string();
        html_decode(&t)
    };
    let out = match &value {
        Value::String(s) => Value::String(strip(s)),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| match x {
                    Value::String(s) => Value::String(strip(s)),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    };
    set_source_path(source, &target, out)?;
    Ok(false)
}

fn html_decode(s: &str) -> String {
    s.replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn proc_bytes(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let target = cfg
        .get("target_field")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| field.clone());
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let s = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let parsed = parse_bytes(&s)?;
    set_source_path(source, &target, json!(parsed))?;
    Ok(false)
}

fn parse_bytes(s: &str) -> Result<i64, ApiError> {
    let trimmed = s.trim();
    if trimmed.starts_with('-') {
        return Err(parse_exception(&format!(
            "failed to parse setting [Ingest Field] with value [{}] as a size in bytes: negative values are not allowed",
            s
        )));
    }
    let lower = trimmed.to_lowercase();
    let (num_str, unit) = {
        let mut idx = 0;
        let chars: Vec<char> = lower.chars().collect();
        while idx < chars.len() && (chars[idx].is_ascii_digit() || chars[idx] == '.') {
            idx += 1;
        }
        let num: String = chars[..idx].iter().collect();
        let rest: String = chars[idx..].iter().collect();
        (num, rest.trim().to_string())
    };
    if num_str.is_empty() {
        return Err(parse_exception(&format!(
            "failed to parse setting [Ingest Field] with value [{}] as a size in bytes: unit is missing or unrecognized",
            s
        )));
    }
    if num_str.contains('.') {
        return Err(parse_exception(&format!(
            "Failed to parse bytes value [{}]. Fractional bytes values have been deprecated and are no longer supported. Please use a non-fractional bytes value.",
            s
        )));
    }
    let n: i64 = num_str.parse().unwrap_or(0);
    if n < 0 || s.contains('-') && !s.contains("b-") {
        if trimmed.starts_with('-') {
            return Err(parse_exception(&format!(
                "failed to parse setting [Ingest Field] with value [{}] as a size in bytes: negative values are not allowed",
                s
            )));
        }
    }
    let mult: i64 = match unit.as_str() {
        "b" => 1,
        "k" | "kb" => 1024,
        "m" | "mb" => 1024 * 1024,
        "g" | "gb" => 1024 * 1024 * 1024,
        "t" | "tb" => 1024i64.pow(4),
        "p" | "pb" => 1024i64.pow(5),
        _ => {
            return Err(parse_exception(&format!(
                "failed to parse setting [Ingest Field] with value [{}] as a size in bytes: unit is missing or unrecognized",
                s
            )))
        }
    };
    Ok(n * mult)
}

fn proc_fingerprint(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let mut fields = cfg_strings(cfg, "fields");
    if fields.is_empty() && !cfg.contains_key("fields") && cfg.contains_key("exclude_fields") {
        let exclude = cfg_strings(cfg, "exclude_fields");
        fields = source
            .keys()
            .filter(|k| !exclude.contains(k))
            .cloned()
            .collect();
    } else if fields.is_empty() && !cfg.contains_key("fields") {
        fields = source.keys().cloned().collect();
    }
    fields.sort();
    let explicit = cfg.contains_key("fields");
    let mut canonical = String::new();
    let mut included = 0usize;
    for f in &fields {
        let val = match source_get(source, f) {
            Some(v) => v,
            None => {
                if cfg_bool(cfg, "ignore_missing", false) {
                    continue;
                }
                return Err(ApiError::bad(&format!("field [{}] doesn't exist", f)));
            }
        };
        included += 1;
        let text = match &val {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".to_string(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        canonical.push('|');
        canonical.push_str(f);
        canonical.push_str("|1:");
        canonical.push_str(&text);
    }
    canonical.push('|');
    if explicit && included == 0 && cfg_bool(cfg, "ignore_missing", false) {
        return Ok(false);
    }
    let method = cfg_str(cfg, "hash_method").unwrap_or("SHA-1@2.16.0");
    let digest: Vec<u8> = if method.starts_with("MD5") {
        crate::hashes::md5(canonical.as_bytes()).to_vec()
    } else if method.starts_with("SHA-1") {
        crate::hashes::sha1(canonical.as_bytes()).to_vec()
    } else if method.starts_with("SHA-256") {
        crate::hashes::sha256(canonical.as_bytes()).to_vec()
    } else {
        crate::hashes::sha3_256(canonical.as_bytes()).to_vec()
    };
    let hash = base64::encode(&digest);
    let target = cfg_str(cfg, "target_field").unwrap_or("fingerprint");
    set_source_path(source, target, json!(format!("{}:{}", method, hash)))?;
    let _ = meta;
    Ok(false)
}

fn proc_community_id(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let _ = meta;
    let get = |key: &str| -> Option<Value> {
        cfg_str(cfg, key).and_then(|p| source_get(source, p))
    };
    let sip = get("source_ip_field")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| ApiError::bad("source ip field path cannot be null nor empty"))?;
    let dip = get("destination_ip_field")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| ApiError::bad("destination ip field path cannot be null nor empty"))?;
    let proto = if let Some(p) = get("iana_protocol_number_field") {
        match p {
            Value::Number(n) => n.as_i64().unwrap_or(0) as u8,
            Value::String(s) => s.parse::<u8>().unwrap_or(0),
            _ => 0,
        }
    } else if let Some(p) = get("protocol_field") {
        let name = p.as_str().unwrap_or("").to_lowercase();
        match name.as_str() {
            "tcp" => 6,
            "udp" => 17,
            "icmp" => 1,
            "icmpv6" => 58,
            "sctp" => 132,
            _ => p
                .as_str()
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(0),
        }
    } else {
        0
    };
    let ip1 = parse_ip_bytes(&sip)
        .ok_or_else(|| ApiError::bad(&format!("[{}] is not a valid ipv4/ipv6 address", sip)))?;
    let ip2 = parse_ip_bytes(&dip)
        .ok_or_else(|| ApiError::bad(&format!("[{}] is not a valid ipv4/ipv6 address", dip)))?;
    let (mut a_ip, mut b_ip) = (ip1, ip2);
    let (mut a_port, mut b_port): (u16, u16);
    if proto == 1 {
        let t = get("icmp_type_field")
            .and_then(|v| v.as_i64().map(|x| x as u16))
            .unwrap_or(0);
        let c = get("icmp_code_field")
            .and_then(|v| v.as_i64().map(|x| x as u16))
            .unwrap_or(0);
        let mapped = match t {
            8 => 0,
            0 => 8,
            13 => 14,
            14 => 13,
            15 => 16,
            16 => 15,
            17 => 18,
            18 => 17,
            9 => 10,
            10 => 9,
            _ => c,
        };
        let one_way = !matches!(t, 0 | 8 | 9 | 10 | 13 | 14 | 15 | 16 | 17 | 18);
        a_port = t;
        b_port = mapped;
        if !one_way && (a_ip.as_slice(), a_port) > (b_ip.as_slice(), b_port) {
            std::mem::swap(&mut a_ip, &mut b_ip);
            std::mem::swap(&mut a_port, &mut b_port);
        }
    } else {
        let sp = get("source_port_field")
            .and_then(|v| v.as_i64().map(|x| x as u16));
        let dp = get("destination_port_field")
            .and_then(|v| v.as_i64().map(|x| x as u16));
        match (sp, dp) {
            (Some(s), Some(d)) => {
                a_port = s;
                b_port = d;
            }
            _ => {
                return Err(ApiError::bad(
                    "both source port and destination port field path cannot be null nor empty",
                ))
            }
        }
        if (a_ip.as_slice(), a_port) > (b_ip.as_slice(), b_port) {
            std::mem::swap(&mut a_ip, &mut b_ip);
            std::mem::swap(&mut a_port, &mut b_port);
        }
    }
    let seed = cfg.get("seed").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&seed.to_be_bytes());
    buf.extend_from_slice(&a_ip);
    buf.extend_from_slice(&b_ip);
    buf.push(proto);
    buf.push(0);
    buf.extend_from_slice(&a_port.to_be_bytes());
    buf.extend_from_slice(&b_port.to_be_bytes());
    let digest = crate::hashes::sha1(&buf);
    let target = cfg_str(cfg, "target_field").unwrap_or("community_id");
    set_source_path(
        source,
        target,
        json!(format!("1:{}", base64::encode(&digest))),
    )?;
    Ok(false)
}

fn parse_ip_bytes(s: &str) -> Option<Vec<u8>> {
    match s.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v)) => Some(v.octets().to_vec()),
        Ok(std::net::IpAddr::V6(v)) => Some(v.octets().to_vec()),
        Err(_) => None,
    }
}

fn proc_dot_expander(
    cfg: &Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = cfg_str(cfg, "field").unwrap_or("");
    let path = cfg_str(cfg, "path");
    let container_path: Vec<String> = path
        .map(|p| p.split('.').map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let mut container = if container_path.is_empty() {
        Value::Object(source.clone())
    } else {
        match source_get(source, path.unwrap()) {
            Some(v) => v,
            None => return Ok(false),
        }
    };
    if !container.is_object() {
        return Ok(false);
    }
    let obj = container.as_object_mut().unwrap();
    let flat = match obj.get(field) {
        Some(v) => v.clone(),
        None => return Ok(false),
    };
    let segs: Vec<String> = field.split('.').map(|s| s.to_string()).collect();
    if let Some(first) = segs.first() {
        if let Some(existing) = obj.get(first) {
            if !existing.is_object() && segs.len() > 1 {
                return Err(ApiError::bad(&format!(
                    "cannot expend [{}], because [{}] is not an object field, but a value field",
                    field, first
                )));
            }
        }
    }
    obj.remove(field);
    let mut newval = flat.clone();
    if let Some(existing) = get_nested(obj, &segs) {
        newval = json!([existing, flat]);
    }
    set_nested_map(obj, &segs, newval, field)?;
    if container_path.is_empty() {
        *source = container.as_object().cloned().unwrap_or_default();
    } else {
        let p = path.unwrap();
        set_source_path(source, p, container)?;
    }
    Ok(false)
}

fn get_nested(obj: &Map<String, Value>, segs: &[String]) -> Option<Value> {
    if segs.is_empty() {
        return None;
    }
    let mut cur = obj.get(&segs[0])?.clone();
    for s in &segs[1..] {
        cur = match cur {
            Value::Object(m) => m.get(s)?.clone(),
            _ => return None,
        };
    }
    Some(cur)
}

fn set_nested_map(
    obj: &mut Map<String, Value>,
    segs: &[String],
    val: Value,
    field: &str,
) -> Result<(), ApiError> {
    if segs.is_empty() {
        return Ok(());
    }
    if segs.len() == 1 {
        obj.insert(segs[0].clone(), val);
        return Ok(());
    }
    let entry = obj
        .entry(segs[0].clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        return Err(ApiError::bad(&format!(
            "cannot expend [{}], because [{}] is not an object field, but a value field",
            field, segs[0]
        )));
    }
    set_nested_map(entry.as_object_mut().unwrap(), &segs[1..], val, field)
}

fn proc_foreach(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            let mut e = ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            ));
            e.fields.insert("__fatal".to_string(), json!(true));
            return Err(e);
        }
    };
    let mut arr = match value {
        Value::Array(a) => a,
        other => {
            let mut e = ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.util.List]",
                field,
                java_type(&other)
            ));
            e.fields.insert("__fatal".to_string(), json!(true));
            return Err(e);
        }
    };
    let inner = match cfg.get("processor").and_then(|v| v.as_object()) {
        Some(m) => m.clone(),
        None => return Ok(false),
    };
    for item in arr.iter_mut() {
        let original = item.clone();
        {
            let ing = meta
                .entry("_ingest".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = ing {
                m.insert("_value".to_string(), original.clone());
            }
            let src_ing = source
                .entry("_ingest".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = src_ing {
                m.insert("_value".to_string(), original.clone());
            }
        }
        let mut extra = Vec::new();
        let mut stack: Vec<String> = Vec::new();
        if let Some((typ, cfgv)) = inner.iter().next() {
            let icfg = cfgv.as_object().cloned().unwrap_or_default();
            run_processor(
                &Store::new(),
                typ,
                &icfg,
                meta,
                source,
                false,
                &mut stack,
                &mut extra,
            )?;
        }
        let meta_val = meta
            .get_mut("_ingest")
            .and_then(|v| v.as_object_mut())
            .and_then(|m| m.remove("_value"));
        let src_val = source
            .get_mut("_ingest")
            .and_then(|v| v.as_object_mut())
            .and_then(|m| m.remove("_value"));
        if let Some(v) = src_val.clone() {
            if v != original {
                *item = v;
                if let Some(mv) = meta_val.clone() {
                    if mv != original {
                        *item = mv;
                    }
                }
            } else if let Some(mv) = meta_val {
                *item = mv;
            }
        } else if let Some(mv) = meta_val {
            *item = mv;
        }
        let empty = source
            .get("_ingest")
            .and_then(|v| v.as_object())
            .map(|m| m.is_empty())
            .unwrap_or(true);
        if empty {
            source.remove("_ingest");
        }
        let empty_meta = meta
            .get("_ingest")
            .and_then(|v| v.as_object())
            .map(|m| m.is_empty())
            .unwrap_or(false);
        if empty_meta {
            meta.remove("_ingest");
        }
    }
    set_source_path(source, &field, Value::Array(arr))?;
    Ok(false)
}

fn proc_fail(
    cfg: &Map<String, Value>,
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
) -> Result<bool, ApiError> {
    let msg = resolve_template(cfg_str(cfg, "message").unwrap_or(""), meta, source);
    Err(ApiError::new("fail_processor_exception", &msg, 500))
}

fn proc_script(
    store: &Store,
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let mut params = cfg.get("params").cloned().unwrap_or(json!({}));
    if !params.is_object() {
        params = json!({});
    }
    let mut src = cfg_str(cfg, "source").unwrap_or("").to_string();
    if let Some(id) = cfg_str(cfg, "id") {
        match store.scripts.get(id) {
            Some(stored) => {
                src = stored
                    .get("script")
                    .and_then(|s| s.get("source"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                params = cfg.get("params").cloned().unwrap_or(json!({}));
            }
            None => {
                let mut e = ApiError::new(
                    "resource_not_found_exception",
                    &format!("unable to find script [{}] in cluster state", id),
                    404,
                );
                e.fields.insert("__fatal".to_string(), json!(true));
                return Err(e);
            }
        }
    }
    match ingest_script::run_source(&src, &params, meta, source) {
        Ok(()) => Ok(false),
        Err(mut e) => {
            e.fields.insert("lang".to_string(), json!("painless"));
            Err(e)
        }
    }
}

fn proc_pipeline(
    store: &Store,
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
    verbose: bool,
    stack: &mut Vec<String>,
    extra: &mut Vec<Value>,
) -> Result<bool, ApiError> {
    let name = resolve_template(cfg_str(cfg, "name").unwrap_or(""), meta, source);
    let pipeline = match store.pipelines.get(&name) {
        Some(p) => p,
        None => {
            return Err(ApiError::new(
                "illegal_state_exception",
                &format!(
                    "Pipeline processor configured for non-existent pipeline [{}]",
                    name
                ),
                400,
            ))
        }
    };
    if stack.contains(&name) {
        return Err(ApiError::new(
            "illegal_state_exception",
            &format!("Cycle detected for pipeline: {}", name),
            400,
        ));
    }
    stack.push(name.clone());
    let processors = pipeline
        .get("processors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let of = pipeline.get("on_failure").cloned();
    let res = run_list(
        store, &name, &processors, meta, source, verbose, stack, of.as_ref(),
    );
    stack.pop();
    match res {
        Ok((dropped, results)) => {
            if verbose {
                extra.extend(results);
            }
            Ok(dropped)
        }
        Err(e) => Err(e),
    }
}

fn proc_user_agent(
    cfg: &Map<String, Value>,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<bool, ApiError> {
    let field = resolve_field_name(cfg, meta, source);
    let value = match source_get(source, &field) {
        Some(v) => v,
        None => {
            if cfg_bool(cfg, "ignore_missing", false) {
                return Ok(false);
            }
            return Err(ApiError::bad(&format!(
                "field [{}] not present as part of path [{}]",
                field, field
            )));
        }
    };
    let ua = match value {
        Value::String(s) => s,
        other => {
            return Err(ApiError::bad(&format!(
                "field [{}] of type [{}] cannot be cast to [java.lang.String]",
                field,
                java_type(&other)
            )))
        }
    };
    let mut parsed = parse_user_agent(&ua);
    if let Some(props) = cfg.get("properties").and_then(|v| v.as_array()) {
        let mut filtered = Map::new();
        for p in props {
            if let Some(name) = p.as_str() {
                if let Some(v) = parsed.get(name) {
                    filtered.insert(name.to_string(), v.clone());
                }
            }
        }
        parsed = filtered;
    }
    let target = cfg_str(cfg, "target_field").unwrap_or("user_agent");
    set_source_path(source, target, Value::Object(parsed))?;
    Ok(false)
}

fn parse_user_agent(ua: &str) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("original".to_string(), json!(ua));
    if ua.starts_with("curl/") {
        out.insert("name".to_string(), json!("curl"));
        out.insert(
            "version".to_string(),
            json!(ua.trim_start_matches("curl/")),
        );
        out.insert("device".to_string(), json!({"name": "Other"}));
        return out;
    }
    if ua.contains("iPhone") && ua.contains("Mobile") {
        out.insert("name".to_string(), json!("Mobile Safari"));
        let os_ver = Regex::new(r"OS (\d+)_(\d+)").unwrap();
        let (mut ov, mut ofull) = ("", String::new());
        if let Some(c) = os_ver.captures(ua) {
            let major = c.get(1).map(|m| m.as_str()).unwrap_or("");
            let minor = c.get(2).map(|m| m.as_str()).unwrap_or("");
            ov = major;
            ofull = format!("{}.{}", major, minor);
            out.insert(
                "os".to_string(),
                json!({"name": "iOS", "version": ofull.clone(), "full": format!("iOS {}", ofull)}),
            );
        }
        let _ = ov;
        out.insert("device".to_string(), json!({"name": "iPhone"}));
        let ver = Regex::new(r"Version/([\d.]+)").unwrap();
        if let Some(c) = ver.captures(ua) {
            out.insert("version".to_string(), json!(c.get(1).map(|m| m.as_str()).unwrap_or("")));
        }
        return out;
    }
    if ua.contains("Chrome/") {
        out.insert("name".to_string(), json!("Chrome"));
        let ver = Regex::new(r"Chrome/([\d.]+)").unwrap();
        if let Some(c) = ver.captures(ua) {
            out.insert("version".to_string(), json!(c.get(1).map(|m| m.as_str()).unwrap_or("")));
        }
        if ua.contains("Mac OS X") {
            let os = Regex::new(r"Mac OS X (\d+)_(\d+)_(\d+)").unwrap();
            if let Some(c) = os.captures(ua) {
                let mj = c.get(1).map(|m| m.as_str()).unwrap_or("");
                let mn = c.get(2).map(|m| m.as_str()).unwrap_or("");
                let pt = c.get(3).map(|m| m.as_str()).unwrap_or("");
                let full = format!("{}.{}.{}", mj, mn, pt);
                out.insert(
                    "os".to_string(),
                    json!({"name": "Mac OS X", "version": full.clone(), "full": format!("Mac OS X {}", full)}),
                );
            }
            out.insert("device".to_string(), json!({"name": "Mac"}));
        }
        return out;
    }
    out.insert("name".to_string(), json!("Other"));
    out.insert("device".to_string(), json!({"name": "Other"}));
    out
}

// ------------------------------------------------------------- index-time use

pub struct IngestOutcome {
    pub dropped: bool,
    pub meta: Map<String, Value>,
    pub source: Map<String, Value>,
}

pub fn effective_ingest_pipeline(
    store: &Store,
    index: &str,
    explicit: &str,
) -> Result<Option<String>, ApiError> {
    let default = store
        .indices
        .get(index)
        .and_then(|i| crate::store::find_setting(&i.settings, "default_pipeline"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let chosen = if !explicit.is_empty() {
        if explicit == "_none" {
            None
        } else {
            Some(explicit.to_string())
        }
    } else {
        default
    };
    if let Some(n) = &chosen {
        if !store.pipelines.contains_key(n) {
            return Err(ApiError::bad(&format!(
                "pipeline with id [{}] does not exist",
                n
            )));
        }
    }
    Ok(chosen)
}

pub fn final_ingest_pipeline(store: &Store, index: &str) -> Option<String> {
    store
        .indices
        .get(index)
        .and_then(|i| crate::store::find_setting(&i.settings, "final_pipeline"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn run_ingest_pipeline(
    store: &Store,
    name: &str,
    source: Map<String, Value>,
    mut meta: Map<String, Value>,
) -> Result<IngestOutcome, ApiError> {
    let pipeline = match store.pipelines.get(name) {
        Some(p) => p.clone(),
        None => {
            return Err(ApiError::bad(&format!(
                "pipeline with id [{}] does not exist",
                name
            )))
        }
    };
    let mut ing = Map::new();
    ing.insert("timestamp".to_string(), json!(now_iso()));
    ing.insert("pipeline".to_string(), json!(name));
    meta.insert("_ingest".to_string(), Value::Object(ing));
    let processors = pipeline
        .get("processors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let on_failure = pipeline.get("on_failure").cloned();
    let mut stack = vec![name.to_string()];
    let mut source = source;
    match run_list(
        store,
        name,
        &processors,
        &mut meta,
        &mut source,
        false,
        &mut stack,
        on_failure.as_ref(),
    ) {
        Ok((dropped, _)) => Ok(IngestOutcome {
            dropped,
            meta,
            source,
        }),
        Err(mut e) => {
            e.fields.remove("__fatal");
            e.fields.remove("__results");
            Err(e)
        }
    }
}

// ------------------------------------------------------------ search pipeline

#[derive(Default)]
pub struct SearchPipelineState {
    pub response_processors: Vec<Value>,
    pub original_size: Option<usize>,
    pub verbose: bool,
    pub request_results: Vec<Value>,
}

fn search_processor_allowed(kind: &str, name: &str) -> bool {
    match kind {
        "request_processors" => matches!(name, "filter_query" | "script" | "oversample"),
        _ => matches!(
            name,
            "rename_field" | "truncate_hits" | "collapse" | "split" | "sort"
        ),
    }
}

pub fn validate_search_pipeline(body: &Map<String, Value>) -> Result<(), ApiError> {
    for key in body.keys() {
        if !["description", "version", "request_processors", "response_processors"]
            .contains(&key.as_str())
        {
            return Err(ApiError::bad(&format!(
                "pipeline doesn't support one or more provided configuration parameters [{}]",
                key
            )));
        }
    }
    for kind in ["request_processors", "response_processors"] {
        if let Some(arr) = body.get(kind).and_then(|v| v.as_array()) {
            for p in arr {
                let m = match p.as_object() {
                    Some(m) => m,
                    None => return Err(ApiError::bad("processor must be an object")),
                };
                let (name, cfg) = match m.iter().next() {
                    Some((n, c)) => (n.as_str(), c.as_object().cloned().unwrap_or_default()),
                    None => continue,
                };
                if !search_processor_allowed(kind, name) {
                    return Err(ApiError::bad(&format!(
                        "Invalid processor type {}",
                        name
                    )));
                }
                if name == "filter_query" && !cfg.contains_key("query") {
                    return Err(ApiError::bad(
                        "Did not specify the query property in processor of type filter_query",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn boost_query(q: &Value) -> Value {
    let m = match q.as_object() {
        Some(m) => m,
        None => return q.clone(),
    };
    let (k, v) = match m.iter().next() {
        Some((k, v)) => (k.as_str(), v),
        None => return q.clone(),
    };
    match k {
        "match_all" => json!({"match_all": {"boost": 1.0}}),
        "term" => {
            if let Some(tm) = v.as_object() {
                if let Some((f, val)) = tm.iter().next() {
                    if val.is_object() {
                        return json!({"term": {f: val.clone()}});
                    }
                    return json!({"term": {f: {"value": val.clone(), "boost": 1.0}}});
                }
            }
            q.clone()
        }
        _ => q.clone(),
    }
}

fn apply_request_script(body: &mut Map<String, Value>, src: &str) {
    for stmt in src.split(';') {
        let st = stmt.trim();
        if st.is_empty() {
            continue;
        }
        let (lhs, rhs) = match st.split_once('=') {
            Some((l, r)) => (l.trim(), r.trim()),
            None => continue,
        };
        let key = if let Some(rest) = lhs.strip_prefix("ctx._source") {
            let rest = rest.trim();
            if let Some(br) = rest.strip_prefix('[') {
                br.trim_end_matches(']')
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string()
            } else if let Some(name) = rest.strip_prefix('.') {
                name.to_string()
            } else {
                continue;
            }
        } else {
            continue;
        };
        let val: Value = if let Ok(i) = rhs.parse::<i64>() {
            json!(i)
        } else if let Ok(f) = rhs.parse::<f64>() {
            json!(f)
        } else if rhs == "true" || rhs == "false" {
            json!(rhs == "true")
        } else {
            json!(rhs.trim_matches(|c| c == '\'' || c == '"'))
        };
        body.insert(key, val);
    }
}

pub fn prepare_search_pipeline(
    store: &Store,
    index: &str,
    body: &mut Map<String, Value>,
    req: &Req,
) -> Result<SearchPipelineState, ApiError> {
    let mut state = SearchPipelineState::default();
    state.verbose = req.bool_param("verbose_pipeline");
    let param = req.param("search_pipeline").unwrap_or("").to_string();
    let body_sp = body.remove("search_pipeline");
    let mut inline: Option<Map<String, Value>> = None;
    let mut name = String::new();
    if !param.is_empty() {
        name = param;
    } else if let Some(v) = body_sp {
        match v {
            Value::String(s) => name = s,
            Value::Object(m) => inline = Some(m),
            _ => {}
        }
    }
    let pipeline: Option<Map<String, Value>> = if let Some(m) = inline {
        validate_search_pipeline(&m)?;
        Some(m)
    } else if !name.is_empty() {
        if name == "_none" {
            None
        } else {
            match store.search_pipelines.get(&name) {
                Some(p) => {
                    validate_search_pipeline(p)?;
                    Some(p.clone())
                }
                None => {
                    return Err(ApiError::bad(&format!(
                        "Pipeline {} is not defined",
                        name
                    )))
                }
            }
        }
    } else {
        let default = store
            .indices
            .get(index)
            .and_then(|i| crate::store::find_setting(&i.settings, "search.default_pipeline"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        match default {
            Some(n) => match store.search_pipelines.get(&n) {
                Some(p) => {
                    validate_search_pipeline(p)?;
                    Some(p.clone())
                }
                None => {
                    return Err(ApiError::bad(&format!(
                        "Pipeline {} is not defined",
                        n
                    )))
                }
            },
            None => None,
        }
    };
    let pipeline = match pipeline {
        Some(p) => p,
        None => return Ok(state),
    };
    state.response_processors = pipeline
        .get("response_processors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let request_processors = pipeline
        .get("request_processors")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for p in request_processors {
        let m = match p.as_object() {
            Some(m) => m,
            None => continue,
        };
        let (name, cfg) = match m.iter().next() {
            Some((n, c)) => (n.as_str(), c.as_object().cloned().unwrap_or_default()),
            None => continue,
        };
        match name {
            "filter_query" => {
                let fq = cfg.get("query").cloned().unwrap_or(Value::Null);
                let orig = body
                    .get("query")
                    .cloned()
                    .unwrap_or_else(|| json!({"match_all": {}}));
                let internal = json!({"bool": {"must": [orig.clone()], "filter": [fq.clone()]}});
                let verbose_q = json!({"bool": {
                    "filter": [boost_query(&fq)],
                    "must": [boost_query(&orig)]
                }});
                body.insert("query".to_string(), internal);
                if state.verbose {
                    state.request_results.push(json!({
                        "processor_name": "filter_query",
                        "status": "success",
                        "duration_millis": 0,
                        "output_data": {"query": verbose_q}
                    }));
                }
            }
            "script" => {
                if let Some(src) = cfg.get("source").and_then(|v| v.as_str()) {
                    apply_request_script(body, src);
                }
                if state.verbose {
                    state.request_results.push(json!({
                        "processor_name": "script",
                        "status": "success",
                        "duration_millis": 0,
                        "output_data": {}
                    }));
                }
            }
            "oversample" => {
                let factor = match cfg.get("sample_factor") {
                    Some(v) => v.as_f64().unwrap_or(0.0),
                    None => {
                        return Err(parse_exception(
                            "[sample_factor] required property is missing",
                        ))
                    }
                };
                if factor < 1.0 {
                    return Err(parse_exception(
                        "[sample_factor] Value must be >= 1.0",
                    ));
                }
                let orig = body.get("size").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                state.original_size = Some(orig);
                body.insert(
                    "size".to_string(),
                    json!(((orig as f64) * factor) as usize),
                );
                if state.verbose {
                    state.request_results.push(json!({
                        "processor_name": "oversample",
                        "status": "success",
                        "duration_millis": 0,
                        "output_data": {}
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(state)
}

pub fn apply_search_response(
    state: &SearchPipelineState,
    result: &mut Value,
) -> Result<(), ApiError> {
    if state.response_processors.is_empty() {
        return Ok(());
    }
    for p in &state.response_processors {
        let m = match p.as_object() {
            Some(m) => m,
            None => continue,
        };
        let (name, cfg) = match m.iter().next() {
            Some((n, c)) => (n.as_str(), c.as_object().cloned().unwrap_or_default()),
            None => continue,
        };
        match name {
            "rename_field" => {
                let field = cfg.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let target = cfg
                    .get("target_field")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let ignore = cfg
                    .get("ignore_missing")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(hits) = result
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    for h in hits.iter_mut() {
                        let id = h
                            .get("_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let src = match h.get_mut("_source").and_then(|s| s.as_object_mut()) {
                            Some(s) => s,
                            None => continue,
                        };
                        match src.remove(field) {
                            Some(v) => {
                                src.insert(target.to_string(), v);
                            }
                            None => {
                                if !ignore {
                                    return Err(ApiError::bad(&format!(
                                        "Document with id {} is missing field {}",
                                        id, field
                                    )));
                                }
                            }
                        }
                    }
                }
            }
            "truncate_hits" => {
                let size = cfg
                    .get("target_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .or(state.original_size);
                let size = match size {
                    Some(s) => s,
                    None => {
                        return Err(ApiError::new(
                            "illegal_state_exception",
                            "Must specify target_size unless an earlier processor set original_size",
                            500,
                        ))
                    }
                };
                if let Some(hits) = result
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    hits.truncate(size);
                }
            }
            "collapse" => {
                let field = cfg.get("field").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(hits) = result
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    let mut seen = std::collections::HashSet::new();
                    hits.retain(|h| {
                        let key = h
                            .get("_source")
                            .and_then(|s| s.get(field))
                            .cloned()
                            .unwrap_or(Value::Null);
                        seen.insert(key.to_string())
                    });
                }
            }
            "split" => {
                let field = cfg.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let sep = cfg.get("separator").and_then(|v| v.as_str()).unwrap_or(",");
                let target = cfg.get("target_field").and_then(|v| v.as_str());
                if let Some(hits) = result
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    for h in hits.iter_mut() {
                        let src = match h.get_mut("_source").and_then(|s| s.as_object_mut()) {
                            Some(s) => s,
                            None => continue,
                        };
                        if let Some(Value::String(s)) = src.get(field) {
                            let parts: Vec<Value> = s
                                .split(sep)
                                .map(|x| json!(x))
                                .collect();
                            if let Some(t) = target {
                                src.insert(t.to_string(), Value::Array(parts));
                            } else {
                                src.insert(field.to_string(), Value::Array(parts));
                            }
                        }
                    }
                }
            }
            "sort" => {
                let field = cfg.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let desc = cfg.get("order").and_then(|v| v.as_str()) == Some("desc");
                if let Some(hits) = result
                    .get_mut("hits")
                    .and_then(|h| h.get_mut("hits"))
                    .and_then(|h| h.as_array_mut())
                {
                    for h in hits.iter_mut() {
                        let src = match h.get_mut("_source").and_then(|s| s.as_object_mut()) {
                            Some(s) => s,
                            None => continue,
                        };
                        if let Some(Value::Array(arr)) = src.get_mut(field) {
                            arr.sort_by(|a, b| {
                                let sa = format_value(a);
                                let sb = format_value(b);
                                sa.cmp(&sb)
                            });
                            if desc {
                                arr.reverse();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn handle_search_pipeline(store: &mut Store, req: &Req, segs: &[String]) -> Response {
    match req.method.as_str() {
        "GET" if segs.is_empty() => {
            let out: Map<String, Value> = store
                .search_pipelines
                .iter()
                .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                .collect();
            json_response(200, Value::Object(out))
        }
        "GET" if segs.len() == 1 => {
            let name = &segs[0];
            if name.contains(',') || name.contains('*') || name.contains('?') {
                let out: Map<String, Value> = store
                    .search_pipelines
                    .iter()
                    .filter(|(k, _)| name.split(',').any(|part| glob_match(part, k)))
                    .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                    .collect();
                return json_response(200, Value::Object(out));
            }
            match store.search_pipelines.get(name) {
                Some(p) => json_response(200, json!({name: Value::Object(p.clone())})),
                None => json_response(404, json!({})),
            }
        }
        "PUT" | "POST" if segs.len() == 1 => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            if let Err(e) = validate_search_pipeline(&body) {
                return err_response(e);
            }
            store.search_pipelines.insert(segs[0].clone(), body);
            json_response(200, json!({"acknowledged": true}))
        }
        "DELETE" if segs.len() == 1 => {
            let name = &segs[0];
            if store.search_pipelines.remove(name).is_some() {
                json_response(200, json!({"acknowledged": true}))
            } else {
                err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("pipeline [{}] is missing", name),
                    404,
                ))
            }
        }
        _ => json_response(404, json!({})),
    }
}
