mod admin;
mod aggs;
mod alias;
mod cat;
mod cluster;
mod datastreams;
mod docs;
mod errors;
mod eval;
mod eval_extra;
mod fields;
mod hashes;
mod hdr;
mod highlight;
mod ingest;
mod ingest_script;
mod maintenance;
mod meta;
mod nodes;
mod painless;
mod painless_exec;
mod scripts;
mod reindex;
mod render;
mod resize;
mod resolve_index;
mod resp;
mod rollover;
mod search;
mod shared;
mod store;
mod suggest;
mod templates;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use store::*;

use errors::{json_response, no_handler, text_response, ApiError};
use resp::{filter_path_value, pretty_json};

pub type Shared = Arc<Mutex<Store>>;

/// Common request abstraction shared by all handlers.
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Req {
    pub fn param(&self, k: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(qk, _)| qk == k)
            .map(|(_, v)| v.as_str())
    }

    pub fn param_or(&self, k: &str, default: &str) -> String {
        self.param(k).unwrap_or(default).to_string()
    }

    pub fn has_param(&self, k: &str) -> bool {
        self.query.iter().any(|(qk, _)| qk == k)
    }

    /// Mirrors Go's queryBool: presence with empty value counts as true.
    pub fn bool_param(&self, k: &str) -> bool {
        match self.param(k) {
            None => false,
            Some(v) => v == "true" || v.is_empty(),
        }
    }

    pub fn json_body(&self) -> Result<Map<String, Value>, ApiError> {
        let mut s = String::from_utf8_lossy(&self.body).to_string();
        if s.trim().is_empty() {
            if let Some(src) = self.param("source") {
                s = src.to_string();
            } else {
                return Ok(Map::new());
            }
        }
        match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(mut m)) => {
                let mut v = Value::Object(std::mem::take(&mut m));
                fix_exponent_literals(&s, &mut v);
                match v {
                    Value::Object(m) => Ok(m),
                    _ => Ok(Map::new()),
                }
            }
            Ok(_) => Err(ApiError::new(
                "json_parse_exception",
                "Failed to parse request body",
                400,
            )),
            Err(e) => Err(ApiError::new(
                "json_parse_exception",
                &format!("Failed to parse request body: {}", e),
                400,
            )),
        }
    }
}

/// serde_json normalizes an exponent marker `E` to `e` while parsing. Go's
/// decoder (UseNumber) keeps the literal text, so documents indexed with
/// `1E+2` echo back verbatim. Re-apply the original spelling for numbers that
/// contained an uppercase exponent marker.
pub(crate) fn fix_exponent_literals(s: &str, v: &mut Value) {
    let bytes = s.as_bytes();
    let mut map: HashMap<String, String> = HashMap::new();
    let mut i = 0;
    let mut in_str = false;
    let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == b'-' || c.is_ascii_digit() {
            let start = i;
            if c == b'-' {
                i += 1;
            }
            while i < bytes.len() {
                let d = bytes[i];
                if d.is_ascii_digit() || matches!(d, b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                } else {
                    break;
                }
            }
            let tok = &s[start..i];
            if tok.contains('E') {
                if let Ok(Value::Number(n)) = serde_json::from_str::<Value>(tok) {
                    map.insert(n.to_string(), tok.to_string());
                }
            }
            continue;
        }
        i += 1;
    }
    if map.is_empty() {
        return;
    }
    fn walk(v: &mut Value, map: &HashMap<String, String>) {
        match v {
            Value::Number(n) => {
                let key = n.to_string();
                if let Some(raw) = map.get(&key) {
                    *n = serde_json::Number::from_string_unchecked(raw.clone());
                }
            }
            Value::Array(a) => {
                for e in a.iter_mut() {
                    walk(e, map);
                }
            }
            Value::Object(m) => {
                for (_, e) in m.iter_mut() {
                    walk(e, map);
                }
            }
            _ => {}
        }
    }
    walk(v, &map);
}

fn pct_decode_impl(s: &str, plus_as_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if plus_as_space && bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn pct_decode(s: &str) -> String {
    pct_decode_impl(s, true)
}

/// Path segments keep a literal '+'; only query strings map '+' to space.
fn path_decode(s: &str) -> String {
    pct_decode_impl(s, false)
}

pub fn duplicate_object_key(s: &str) -> Option<String> {
    use std::collections::HashSet;
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<HashSet<String>> = vec![];
    let mut in_str = false;
    let mut esc = false;
    let mut cur = String::new();
    let mut last_str: Option<String> = None;
    let mut expect_key = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                cur.push(c as char);
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
                last_str = Some(std::mem::take(&mut cur));
            } else {
                cur.push(c as char);
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                cur.clear();
            }
            b'{' => {
                stack.push(HashSet::new());
                expect_key = true;
                last_str = None;
            }
            b'}' => {
                stack.pop();
                expect_key = false;
                last_str = None;
            }
            b',' => {
                expect_key = true;
                last_str = None;
            }
            b':' => {
                if expect_key {
                    if let Some(k) = last_str.clone() {
                        if let Some(top) = stack.last_mut() {
                            if !top.insert(k.clone()) {
                                return Some(k);
                            }
                        }
                    }
                    expect_key = false;
                }
                last_str = None;
            }
            b'[' | b']' => {
                expect_key = false;
                last_str = None;
            }
            _ => {
                if !c.is_ascii_whitespace() {
                    last_str = None;
                }
            }
        }
        i += 1;
    }
    None
}

fn parse_query(uri: &Uri) -> Vec<(String, String)> {
    let mut out = vec![];
    if let Some(q) = uri.query() {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            out.push((pct_decode(k), pct_decode(v)));
        }
    }
    out
}


async fn handle(State(store): State<Shared>, request: axum::extract::Request) -> axum::response::Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    // http.max_content_length: OpenSearch answers 413 with an empty body
    let body = match axum::body::to_bytes(request.into_body(), MAX_CONTENT_LENGTH).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let req = Req {
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        query: parse_query(&uri),
        headers,
        body: body.to_vec(),
    };

    // Content-Type gate (mirrors Go ServeHTTP)
    if !req.body.is_empty() {
        let ct = req
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let ok = matches!(
            ct.as_str(),
            "application/json"
                | "application/x-ndjson"
                | "application/yaml"
                | "application/x-yaml"
                | "text/yaml"
                | "application/yml"
        );
        if !ok {
            let mut resp = errors::write_simple_error(
                406,
                &format!("Content-Type header [{}] is not supported", ct),
            );
            resp.headers_mut()
                .insert("x-opensearch-version", "OpenSearch/3.9.0 (opensearch)".parse().unwrap());
            return resp;
        }
    }

    let mut resp = {
        let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
        dispatch(&mut guard, &req)
    };

    // Post-processing (mirrors Go ServeHTTP)
    let status = resp.status().as_u16();
    let mut ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if ctype.is_empty() {
        ctype = "application/json; charset=UTF-8".to_string();
        resp.headers_mut()
            .insert("content-type", ctype.parse().unwrap());
    }
    let ordered = resp.headers_mut().remove("x-lite-ordered").is_some();
    let fp = req.param("filter_path").unwrap_or("").to_string();
    let pretty = req.has_param("pretty") && req.param("pretty") != Some("false");
    let error_trace = req.bool_param("error_trace");
    if ctype.starts_with("application/json") {
        let (mut parts, resp_body) = resp.into_parts();
        let body_bytes = axum::body::to_bytes(resp_body, usize::MAX)
            .await
            .unwrap_or_default();
        let mut body: Vec<u8> = body_bytes.to_vec();
        let mut reencoded = false;
        if !fp.is_empty() && status < 400 {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                body = serde_json::to_vec(&filter_path_value(&v, &fp)).unwrap_or_default();
                reencoded = true;
            }
        }
        if !ordered || reencoded {
            if pretty && status < 400 {
                if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                    body = pretty_json(&v).into_bytes();
                }
            } else {
                while body.last() == Some(&b'\n') {
                    body.pop();
                }
            }
        }
        if status >= 400 && error_trace {
            if let Ok(mut v) = serde_json::from_slice::<Value>(&body) {
                if let Some(err_obj) = v.get_mut("error").and_then(|e| e.as_object_mut()) {
                    let stk = serde_json::json!([
                        format!(
                            "java.lang.IllegalArgumentException: {}",
                            err_obj
                                .get("reason")
                                .and_then(|r| r.as_str())
                                .unwrap_or("")
                        ),
                        "\tat org.opensearch.action.support.master.TransportMasterNodeAction$AsyncSingleAction.doExecute(TransportMasterNodeAction.java:210)"
                    ]);
                    err_obj.insert("stack_trace".to_string(), stk.clone());
                    if let Some(rc) = err_obj.get_mut("root_cause").and_then(|r| r.as_array_mut()) {
                        for c in rc.iter_mut() {
                            if let Some(cm) = c.as_object_mut() {
                                cm.insert("stack_trace".to_string(), stk.clone());
                            }
                        }
                    }
                }
                body = serde_json::to_vec(&v).unwrap_or_default();
            }
        }
        let acc = req
            .headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if req.param("format") == Some("yaml") || acc.contains("yaml") {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                parts
                    .headers
                    .insert("content-type", "application/yaml".parse().unwrap());
                body = resp::to_yaml(&v).into_bytes();
            }
        } else if acc.contains("application/smile") {
            parts
                .headers
                .insert("content-type", "application/smile".parse().unwrap());
        } else if acc.contains("application/cbor") {
            parts
                .headers
                .insert("content-type", "application/cbor".parse().unwrap());
        }
        resp = axum::response::Response::from_parts(parts, axum::body::Body::from(body));
    }
    resp.headers_mut().insert(
        "x-opensearch-version",
        "OpenSearch/3.9.0 (opensearch)".parse().unwrap(),
    );
    if req.method == "HEAD" {
        *resp.body_mut() = axum::body::Body::empty();
    }
    resp
}

pub fn dispatch(store: &mut Store, req: &Req) -> axum::response::Response {
    let path = req.path.trim_matches('/').to_string();
    let segs: Vec<String> = if path.is_empty() {
        vec![]
    } else {
        path.split('/').map(path_decode).collect()
    };

    if segs.is_empty() || segs[0].is_empty() {
        return cluster::handle_root(store, req);
    }
    if let Some(resp) = meta::route_server_endpoints(store, req, &segs) {
        return resp;
    }
    let index = segs[0].as_str();

    match segs[0].as_str() {
        "_bulk" => return docs::handle_bulk(store, req, ""),
        "_mget" => return docs::handle_mget(store, req, ""),
        "_analyze" => return docs::handle_analyze(store, req, ""),
        "_search" => {
            if segs.len() >= 2 && segs[1] == "pipeline" {
                return crate::ingest::handle_search_pipeline(store, req, &segs[2..]);
            }
            if segs.len() >= 2 && segs[1] == "scroll" {
                return search::handle_scroll(store, req, &segs[2..]);
            }
            if segs.len() >= 2 && segs[1] == "point_in_time" {
                return search::handle_pit_route(store, req, &segs[2..]);
            }
            return search::handle_search(store, req, "");
        }
        "_msearch" => {
            let template = segs.len() >= 2 && segs[1] == "template";
            return render::handle_msearch(store, req, "", template);
        }
        "_render" => {
            let id = if segs.len() >= 3 { Some(segs[2].as_str()) } else { None };
            return render::handle_render(store, req, id);
        }
        "_count" => return docs::handle_count(store, req, ""),
        "_field_caps" => return search::handle_field_caps(store, req, ""),
        "_settings" => {
            let filter = segs[1..].join("/");
            if req.method != "GET" && req.method != "HEAD" {
                return errors::method_not_allowed(req, &["GET", "HEAD"]);
            }
            return admin::get_settings_api(store, req, "*", &filter);
        }
        "_refresh" | "_flush" | "_forcemerge" => {
            if req.method != "POST" && req.method != "GET" {
                return errors::method_not_allowed(req, &["POST", "GET"]);
            }
            let op = segs[0].trim_start_matches('_');
            return maintenance::maintenance_op(store, req, "*", op);
        }
        "_cache" => {
            if segs.len() == 2 && segs[1] == "clear" {
                return maintenance::maintenance_op(store, req, "*", "cache");
            }
            return no_handler(&req.method, &req.path);
        }
        "_stats" => {
            if req.method != "GET" && req.method != "HEAD" {
                return errors::method_not_allowed(req, &["GET", "HEAD"]);
            }
            let metrics = segs[1..].join(",");
            return maintenance::handle_stats_api(store, req, "*", &metrics);
        }
        "_segments" => return maintenance::handle_segments_api(store, req, "*"),
        "_recovery" => return maintenance::handle_recovery_api(store, req, "*"),
        "_all" if segs.len() == 1 => {
            return match req.method.as_str() {
                "GET" | "HEAD" => admin::get_index_meta(store, req, "_all"),
                "DELETE" => admin::delete_indices(store, req, "_all"),
                _ => errors::method_not_allowed(req, &["GET", "HEAD", "DELETE"]),
            };
        }
        "_mapping" if segs.len() >= 3 && segs[1] == "field" => {
            return admin::get_mapping_api(store, req, "*", &segs[2..].join(","));
        }
        "_alias" => {
            let alias_spec = segs[1..].join(",");
            return match req.method.as_str() {
                "GET" => alias::get_aliases_global(store, req, &alias_spec, false),
                "HEAD" => {
                    if alias_spec.is_empty() {
                        return json_response(200, Value::Null);
                    }
                    alias::head_aliases_global(store, req, &alias_spec)
                }
                _ => errors::method_not_allowed(req, &["GET", "HEAD"]),
            };
        }
        "_aliases" => {
            let alias_spec = segs[1..].join(",");
            return match req.method.as_str() {
                "POST" => alias::post_aliases(store, req),
                "GET" => alias::get_aliases_global(store, req, &alias_spec, true),
                "HEAD" => {
                    if alias_spec.is_empty() {
                        return json_response(200, Value::Null);
                    }
                    alias::head_aliases_global(store, req, &alias_spec)
                }
                _ => errors::method_not_allowed(req, &["GET", "POST", "HEAD"]),
            };
        }
        "_cat" => return cat::handle_cat(store, req, &segs[1..]),
        _ => {}
    }

    if segs[0].starts_with('_') && segs[0] != "_all" {
        if req.method == "PUT" {
            return admin::create_index(store, req, &segs[0]);
        }
        return errors::err_response(ApiError::new(
            "invalid_index_name_exception",
            &format!("Invalid index name [{}], must not start with '_'.", segs[0]),
            400,
        ));
    }

    match segs.len() {
        1 => match req.method.as_str() {
            "PUT" => admin::create_index(store, req, index),
            "DELETE" => admin::delete_indices(store, req, index),
            "GET" | "HEAD" => admin::get_index_meta(store, req, index),
            _ => errors::method_not_allowed(req, &["GET", "PUT", "DELETE", "HEAD"]),
        },
        2 => match segs[1].as_str() {
            "_mapping" | "_mappings" => match req.method.as_str() {
                "GET" | "HEAD" => admin::get_mapping_api(store, req, index, ""),
                "PUT" | "POST" => admin::put_mapping_api(store, req, index),
                _ => errors::method_not_allowed(req, &["GET", "PUT", "POST", "HEAD"]),
            },
            "_settings" => match req.method.as_str() {
                "GET" | "HEAD" => admin::get_settings_api(store, req, index, ""),
                "PUT" => admin::put_settings_api(store, req, index),
                _ => errors::method_not_allowed(req, &["GET", "PUT", "HEAD"]),
            },
            "_analyze" => docs::handle_analyze(store, req, index),
            "_bulk" => docs::handle_bulk(store, req, index),
            "_mget" => docs::handle_mget(store, req, index),
            "_termvectors" => docs::handle_termvectors(store, req, index, None),
            "_mtermvectors" => docs::handle_mtermvectors(store, req, index),
            "_update_by_query" => docs::handle_update_by_query(store, req, index),
            "_delete_by_query" => docs::handle_delete_by_query(store, req, index),
            "_search" => search::handle_search(store, req, index),
            "_search_shards" => search::handle_search_shards(store, req, index),
            "_rank_eval" => search::handle_rank_eval(store, req, index),
            "_msearch" => render::handle_msearch(store, req, index, false),
            "_count" => docs::handle_count(store, req, index),
            "_field_caps" => search::handle_field_caps(store, req, index),
            "_refresh" | "_flush" | "_forcemerge" => {
                if req.method != "POST" && req.method != "GET" {
                    return errors::method_not_allowed(req, &["POST", "GET"]);
                }
                let op = segs[1].trim_start_matches('_');
                maintenance::maintenance_op(store, req, index, op)
            }
            "_cache" => {
                if segs.len() == 2 {
                    return maintenance::maintenance_op(store, req, index, "cache");
                }
                no_handler(&req.method, &req.path)
            }
            "_stats" => {
                if req.method != "GET" && req.method != "HEAD" {
                    return errors::method_not_allowed(req, &["GET", "HEAD"]);
                }
                maintenance::handle_stats_api(store, req, index, "")
            }
            "_segments" => maintenance::handle_segments_api(store, req, index),
            "_recovery" => maintenance::handle_recovery_api(store, req, index),
            "_rollover" => rollover::handle_rollover(store, req, index, ""),
            "_close" => admin::handle_close_open(store, req, index, true),
            "_open" => admin::handle_close_open(store, req, index, false),
            "_alias" | "_aliases" => match req.method.as_str() {
                "GET" | "HEAD" => alias::get_aliases_indexed(store, req, index, ""),
                "DELETE" => alias::delete_alias_api(store, req, index, ""),
                _ => errors::method_not_allowed(req, &["GET", "HEAD", "DELETE"]),
            },
            "_doc" => {
                if req.method == "POST" {
                    let op = req.param("op_type").unwrap_or("index").to_string();
                    docs::handle_index_doc(store, req, index, "", &op)
                } else {
                    errors::method_not_allowed(req, &["POST"])
                }
            }
            _ => no_handler(&req.method, &req.path),
        },
        3 => match segs[1].as_str() {
            "_doc" => match req.method.as_str() {
                "PUT" | "POST" => {
                    let op = req.param("op_type").unwrap_or("index").to_string();
                    docs::handle_index_doc(store, req, index, &segs[2], &op)
                }
                "GET" | "HEAD" => docs::handle_get_doc(store, req, index, &segs[2]),
                "DELETE" => docs::handle_delete_doc(store, req, index, &segs[2]),
                _ => no_handler(&req.method, &req.path),
            },
            "_source" => {
                if req.method != "GET" && req.method != "HEAD" {
                    return errors::method_not_allowed(req, &["GET", "HEAD"]);
                }
                docs::handle_get_source(store, req, index, &segs[2])
            }
            "_update" => docs::handle_update_doc(store, req, index, &segs[2]),
            "_termvectors" => docs::handle_termvectors(store, req, index, Some(&segs[2])),
            "_explain" => docs::handle_explain(store, req, index, &segs[2]),
            "_search" if segs[2] == "template" => {
                render::handle_search_template(store, req, index)
            }
            "_search" if segs[2] == "point_in_time" => {
                search::handle_pit_create(store, req, index)
            }
            "_search_shards" => search::handle_search_shards(store, req, index),
            "_validate" if segs[2] == "query" => {
                search::handle_validate_query(store, req, index)
            }
            "_msearch" if segs[2] == "template" => {
                render::handle_msearch(store, req, index, true)
            }
            "_rollover" => rollover::handle_rollover(store, req, index, &segs[2]),
            "_clone" | "_shrink" | "_split" => {
                resize::handle_resize(store, req, index, segs[1].trim_start_matches('_'), &segs[2])
            }
            "_create" => docs::handle_index_doc(store, req, index, &segs[2], "create"),
            "_alias" | "_aliases" => match req.method.as_str() {
                "GET" | "HEAD" => alias::get_aliases_indexed(store, req, index, &segs[2]),
                "PUT" | "POST" => alias::put_alias_api(store, req, index, &segs[2]),
                "DELETE" => alias::delete_alias_api(store, req, index, &segs[2]),
                _ => errors::method_not_allowed(req, &["GET", "PUT", "POST", "DELETE", "HEAD"]),
            },
            "_settings" => {
                if req.method != "GET" && req.method != "HEAD" {
                    return errors::method_not_allowed(req, &["GET", "HEAD"]);
                }
                admin::get_settings_api(store, req, index, &segs[2])
            }
            "_stats" => {
                if req.method != "GET" && req.method != "HEAD" {
                    return errors::method_not_allowed(req, &["GET", "HEAD"]);
                }
                maintenance::handle_stats_api(store, req, index, &segs[2])
            }
            "_cache" => {
                if segs[2] == "clear" {
                    return maintenance::maintenance_op(store, req, index, "cache");
                }
                no_handler(&req.method, &req.path)
            }
            "_block" => {
                if req.method != "PUT" {
                    return errors::method_not_allowed(req, &["PUT"]);
                }
                admin::put_block_api(store, req, index, &segs[2])
            }
            _ => no_handler(&req.method, &req.path),
        },
        _ => {
            if segs.len() == 4 && segs[1] == "_mapping" && segs[2] == "field" {
                return admin::get_mapping_api(store, req, index, &segs[3]);
            }
            no_handler(&req.method, &req.path)
        }
    }
}

/// Mirrors OpenSearch's default http.max_content_length (100mb).
const MAX_CONTENT_LENGTH: usize = 100 << 20;

#[tokio::main]
async fn main() {
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "9200".to_string());
    let addr = format!("{}:{}", host, port);
    let store: Shared = Arc::new(Mutex::new(Store::new()));
    let app = Router::new().fallback(any(handle)).with_state(store);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("slopensearch (rust): cannot listen on {}: {}", addr, e);
            std::process::exit(1);
        }
    };
    println!("slopensearch (rust) listening on {}", addr);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("slopensearch (rust): {}", e);
        std::process::exit(1);
    }
}

// Silence unused import warnings for scaffolding helpers used by handler modules.
#[allow(dead_code)]
fn _keep(_: HashMap<String, String>, _: StatusCode) {}
