use crate::errors::{json_response, ApiError};
use crate::{Req, Store};
use axum::response::Response;
use serde_json::{json, Map, Value};

pub const SCRIPT_CONTEXTS: &[(&str, &str, &[(&str, &str, &str)])] = &[
    ("aggregation_selector", "boolean", &[("execute", "boolean", "java.util.Map")]),
    ("aggs", "java.lang.Object", &[("execute", "java.lang.Object", "java.util.Map")]),
    ("bucket_aggregation", "void", &[("execute", "void", "java.util.Map")]),
    ("field", "java.lang.Object", &[("execute", "java.lang.Object", "java.util.Map")]),
    ("filter", "boolean", &[("execute", "boolean", "java.util.Map")]),
    ("ingest", "void", &[("execute", "void", "java.util.Map")]),
    ("number_sort", "double", &[("execute", "double", "java.util.Map")]),
    ("painless_test", "java.lang.Object", &[("execute", "java.lang.Object", "java.util.Map")]),
    ("score", "double", &[("execute", "double", "java.util.Map"), ("get_score", "double", "")]),
    ("string_sort", "java.lang.String", &[("execute", "java.lang.String", "java.util.Map")]),
    ("template", "java.lang.String", &[("execute", "java.lang.String", "java.util.Map")]),
    ("update", "void", &[("execute", "void", "java.util.Map")]),
];

pub fn script_context_response() -> Value {
    let mut contexts: Vec<Value> = SCRIPT_CONTEXTS
        .iter()
        .map(|(name, rt, methods)| {
            let ms: Vec<Value> = methods
                .iter()
                .map(|(m, mrt, params)| {
                    let ps: Vec<Value> = if params.is_empty() {
                        vec![]
                    } else {
                        params
                            .split(',')
                            .map(|p| json!({"type": p, "name": if *name == "ingest" { "ctx" } else { "args0" }}))
                            .collect()
                    };
                    json!({"name": m, "return_type": mrt, "params": ps})
                })
                .collect();
            let _ = rt;
            json!({"name": name, "methods": ms})
        })
        .collect();
    contexts.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json!({"contexts": contexts})
}

pub fn script_language_response() -> Value {
    json!({
        "types_allowed": ["inline", "stored"],
        "language_contexts": [
            {"language": "expression", "contexts": ["aggs", "bucket_aggregation", "field", "filter", "number_sort", "score", "string_sort", "update"]},
            {"language": "knn", "contexts": ["score"]},
            {"language": "mustache", "contexts": ["template"]},
            {"language": "opensearch_compounded_script", "contexts": ["aggs", "filter"]},
            {"language": "painless", "contexts": ["aggs", "aggregation_selector", "bucket_aggregation", "field", "filter", "ingest", "number_sort", "painless_test", "score", "string_sort", "template", "update"]},
            {"language": "ranklib", "contexts": ["ranklib"]}
        ]
    })
}

fn validate_script_for_context(source: &str, context: &str) -> Result<(), ApiError> {
    let known = [
        "aggs",
        "aggregation_selector",
        "bucket_aggregation",
        "field",
        "filter",
        "ingest",
        "number_sort",
        "painless_test",
        "score",
        "string_sort",
        "template",
        "update",
        "ranklib",
    ];
    if !known.contains(&context) {
        return Err(ApiError::bad(&format!("Unknown context [{}]", context)));
    }
    if context == "score" {
        let s = source.trim();
        if s.starts_with('\'') || s.starts_with('"') {
            return Err(ApiError::new("script_exception", "compile error", 400)
                .caused_by(ApiError::new(
                    "class_cast_exception",
                    "Cannot cast from [java.lang.String] to [double].",
                    400,
                )));
        }
    }
    Ok(())
}

fn java_map_string(m: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let body: Vec<String> = keys
        .into_iter()
        .map(|k| format!("{}={}", k, java_value(&m[k])))
        .collect();
    format!("{{{}}}", body.join(", "))
}

fn java_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(java_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(o) => java_map_string(o),
    }
}

pub fn handle_scripts(store: &mut Store, req: &Req, segs: &[String]) -> Response {
    if segs.is_empty() {
        if req.method == "GET" {
            let out: Map<String, Value> = store
                .scripts
                .iter()
                .map(|(k, v)| (k.clone(), Value::Object(v.clone())))
                .collect();
            return json_response(200, Value::Object(out));
        }
        return json_response(405, json!({}));
    }
    if segs[0] == "_script_context" || segs[0] == "_script_language" {
        return json_response(200, json!({}));
    }
    if segs.len() >= 2 && segs[1] == "_execute" {
        return crate::painless_exec::handle_execute(store, req, &segs[0]);
    }
    let id = &segs[0];
    let context = if segs.len() >= 2 {
        Some(segs[1].as_str())
    } else {
        None
    };
    match req.method.as_str() {
        "GET" => match store.scripts.get(id) {
            Some(v) => {
                let mut out = v.clone();
                out.insert("_id".to_string(), json!(id));
                out.insert("found".to_string(), json!(true));
                json_response(200, Value::Object(out))
            }
            None => json_response(404, json!({"_id": id, "found": false})),
        },
        "DELETE" => {
            if store.scripts.remove(id).is_some() {
                json_response(200, json!({"acknowledged": true}))
            } else {
                crate::errors::err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("stored script [{}] does not exist and cannot be deleted", id),
                    404,
                ))
            }
        }
        "PUT" | "POST" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return crate::errors::err_response(e),
            };
            let mut lang = "mustache".to_string();
            let mut explicit_lang = false;
            let mut source = String::new();
            let mut options: Map<String, Value> = Map::new();
            if let Some(script) = body.get("script") {
                match script {
                    Value::String(_) => {
                        return crate::errors::err_response(ApiError::new(
                            "parsing_exception",
                            "unexpected token [VALUE_STRING], expected [{, <source>]",
                            400,
                        ));
                    }
                    Value::Object(m) => {
                        if let Some(l) = m.get("lang").and_then(|v| v.as_str()) {
                            lang = l.to_string();
                            explicit_lang = true;
                        }
                        match m.get("source") {
                            Some(Value::String(s)) => source = s.clone(),
                            Some(Value::Object(o)) => {
                                source = serde_json::to_string(&Value::Object(o.clone()))
                                    .unwrap_or_default();
                                options.insert(
                                    "content_type".to_string(),
                                    json!("application/json; charset=UTF-8"),
                                );
                            }
                            Some(Value::Null) | None => {}
                            Some(_) => {
                                return crate::errors::err_response(ApiError::new(
                                    "parsing_exception",
                                    "unexpected token, expected [VALUE_STRING]",
                                    400,
                                ))
                            }
                        }
                        if lang == "mustache" && source.is_empty() {
                            source = String::new();
                        }
                        if let Some(Value::Object(o)) = m.get("options") {
                            if !o.is_empty() {
                                let mut err = ApiError::new(
                                    "illegal_argument_exception",
                                    &format!(
                                        "illegal compiler options [{}] specified",
                                        java_map_string(o)
                                    ),
                                    400,
                                );
                                err.status = 400;
                                return crate::errors::err_response(err);
                            }
                        }
                    }
                    Value::Null => {}
                    _ => {}
                }
            }
            if !explicit_lang && body.get("script").map(|s| s.is_object()).unwrap_or(false) {
                if body.get("script").and_then(|s| s.get("source")).is_some() {
                    return crate::errors::err_response(ApiError::bad(
                        "must specify lang for stored script",
                    ));
                }
            }
            if !lang.is_empty() && lang != "mustache" {
                if !["painless", "expression", "mustache", "knn", "ranklib", "opensearch_compounded_script"]
                    .contains(&lang.as_str())
                {
                    return crate::errors::err_response(ApiError::bad(&format!(
                        "unable to put stored script with unsupported lang [{}]",
                        lang
                    )));
                }
                if body.get("script").and_then(|s| s.get("source")).is_none() {
                    return crate::errors::err_response(ApiError::bad(
                        "must specify source for stored script",
                    ));
                }
                if source.is_empty() {
                    return crate::errors::err_response(ApiError::bad("source cannot be empty"));
                }
            } else if body.get("script").and_then(|s| s.get("lang")).is_some()
                && body.get("script").and_then(|s| s.get("source")).is_none()
            {
                return crate::errors::err_response(ApiError::bad(
                    "must specify source for stored script",
                ));
            }
            if lang != "mustache" {
                if let Some(ctx) = context {
                    if let Err(e) = validate_script_for_context(&source, ctx) {
                        if e.typ == "script_exception" {
                            let mut err = e;
                            err.fields.insert("script".to_string(), json!(id));
                            return crate::errors::err_response(err);
                        }
                        return crate::errors::err_response(ApiError::bad(&format!(
                            "failed to parse/compile stored script [{}]",
                            id
                        ))
                        .caused_by(e));
                    }
                }
            }
            let mut script = Map::new();
            script.insert("lang".to_string(), json!(lang));
            script.insert("source".to_string(), json!(source));
            if !options.is_empty() {
                script.insert("options".to_string(), Value::Object(options));
            }
            let mut stored = Map::new();
            stored.insert("script".to_string(), Value::Object(script));
            store.scripts.insert(id.clone(), stored);
            json_response(200, json!({"acknowledged": true}))
        }
        _ => json_response(405, json!({})),
    }
}
