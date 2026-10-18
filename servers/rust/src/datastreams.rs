use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::resp::glob_match;
use crate::shared::err_index_not_found_named;
use crate::store::{field_value, parse_date_string, DataStreamInfo, Store};
use crate::Req;

// ------------------------------------------------------- template matching

pub fn find_data_stream_template(store: &Store, name: &str) -> Option<(String, Map<String, Value>)> {
    let mut best_name = String::new();
    let mut best_prio = i32::MIN as i64;
    let mut names: Vec<&String> = store.composable_templates.keys().collect();
    names.sort();
    for n in names {
        let tpl = &store.composable_templates[n];
        if !tpl.contains_key("data_stream") {
            continue;
        }
        if !crate::templates::template_matches(tpl, name) {
            continue;
        }
        let p = crate::templates::priority_of(tpl);
        if p > best_prio || (p == best_prio && (best_name.is_empty() || *n < best_name)) {
            best_prio = p;
            best_name = n.clone();
        }
    }
    if best_name.is_empty() {
        return None;
    }
    Some((best_name.clone(), store.composable_templates[&best_name].clone()))
}

pub fn timestamp_field_of(tpl: &Map<String, Value>) -> String {
    let mut field = "@timestamp".to_string();
    if let Some(ds) = tpl.get("data_stream").and_then(|d| d.as_object()) {
        if let Some(tf) = ds.get("timestamp_field").and_then(|t| t.as_object()) {
            if let Some(n) = tf.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    field = n.to_string();
                }
            }
        }
    }
    field
}

pub fn pad_generation(gen: i64) -> String {
    let mut s = gen.to_string();
    while s.len() < 6 {
        s.insert(0, '0');
    }
    s
}

pub fn backing_index_name(ds_name: &str, generation: i64) -> String {
    format!(".ds-{}-{}", ds_name, pad_generation(generation))
}

pub fn create_data_stream(
    store: &mut Store,
    name: &str,
    tpl_name: &str,
    tpl: &Map<String, Value>,
) -> Result<DataStreamInfo, ApiError> {
    if name != name.to_lowercase() {
        return Err(ApiError::bad(&format!(
            "data_stream [{}] must be lowercase",
            name
        )));
    }
    let ts_field = timestamp_field_of(tpl);
    let mut info = DataStreamInfo {
        name: name.to_string(),
        backing_indices: vec![],
        generation: 1,
        timestamp_field: ts_field,
        template: tpl_name.to_string(),
    };
    let idx = create_backing_index(store, &info)?;
    info.backing_indices.push(idx);
    store.data_streams.insert(name.to_string(), info.clone());
    Ok(info)
}

pub fn create_backing_index(store: &mut Store, info: &DataStreamInfo) -> Result<String, ApiError> {
    let composed = crate::templates::compose_for_index(store, &info.name);
    let mut body = Map::new();
    let mut settings = composed.settings.clone();
    settings.insert("index.hidden".to_string(), json!("true"));
    body.insert("settings".to_string(), Value::Object(settings));
    let mut mappings = Map::new();
    if let Some(src) = composed.mappings.as_object() {
        crate::templates::merge_mappings_deep(&mut mappings, src);
    }
    let props = mappings
        .entry("properties".to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .unwrap();
    if !props.contains_key(&info.timestamp_field) {
        props.insert(info.timestamp_field.clone(), json!({"type": "date"}));
    }
    mappings.insert(
        "_data_stream_timestamp".to_string(),
        json!({"enabled": true}),
    );
    body.insert("mappings".to_string(), Value::Object(mappings));
    let aliases = composed.aliases.clone();
    if !aliases.is_empty() {
        body.insert("aliases".to_string(), Value::Object(aliases));
    }
    let backing = backing_index_name(&info.name, info.generation);
    crate::admin::create_index_internal(store, &backing, &body, true)?;
    if let Some(idx) = store.indices.get_mut(&backing) {
        idx.data_stream = info.name.clone();
    }
    Ok(backing)
}

/// Auto-creates a data stream on first document if a matching template exists.
pub fn maybe_auto_create_data_stream(store: &mut Store, name: &str) -> Result<Option<String>, ApiError> {
    if let Some(ds) = store.data_streams.get(name) {
        if let Some(last) = ds.backing_indices.last() {
            if store.indices.contains_key(last) {
                return Ok(Some(last.clone()));
            }
        }
    }
    let found = find_data_stream_template(store, name);
    let (tpl_name, tpl) = match found {
        Some(f) => f,
        None => return Ok(None),
    };
    let info = create_data_stream(store, name, &tpl_name, &tpl)?;
    match info.backing_indices.last() {
        Some(last) if store.indices.contains_key(last) => Ok(Some(last.clone())),
        _ => Err(err_index_not_found_named(name)),
    }
}

pub fn data_stream_view(store: &Store, ds: &DataStreamInfo) -> Value {
    let mut indices: Vec<Value> = vec![];
    for name in &ds.backing_indices {
        if let Some(idx) = store.indices.get(name) {
            indices.push(json!({"index_name": name, "index_uuid": idx.uuid}));
        }
    }
    json!({
        "name": ds.name,
        "timestamp_field": {"name": ds.timestamp_field},
        "indices": indices,
        "generation": ds.generation,
        "status": "GREEN",
        "template": ds.template,
    })
}

pub fn sorted_data_stream_names(store: &Store) -> Vec<String> {
    let mut names: Vec<String> = store.data_streams.keys().cloned().collect();
    names.sort();
    names
}

// ---------------------------------------------------------------- handlers

pub fn handle_data_stream(store: &mut Store, req: &Req, spec: &str) -> Response {
    match req.method.as_str() {
        "PUT" | "POST" => {
            if store.data_streams.contains_key(spec) {
                return err_response(ApiError::new(
                    "resource_already_exists_exception",
                    &format!("data_stream [{}] already exists", spec),
                    400,
                ));
            }
            let found = find_data_stream_template(store, spec);
            let (tpl_name, tpl) = match found {
                Some(f) => f,
                None => {
                    return err_response(ApiError::bad(&format!(
                        "no matching index template found for data stream [{}]",
                        spec
                    )))
                }
            };
            if let Err(e) = create_data_stream(store, spec, &tpl_name, &tpl) {
                return err_response(e);
            }
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "GET" | "HEAD" => {
            let names: Vec<String> = spec.split(',').map(|s| s.to_string()).collect();
            let mut out: Vec<Value> = vec![];
            let mut missing = String::new();
            let mut matched: Vec<String> = vec![];
            for n in &names {
                let n = n.trim();
                if n.is_empty() {
                    continue;
                }
                if let Some(ds) = store.data_streams.get(n) {
                    out.push(data_stream_view(store, ds));
                    matched.push(n.to_string());
                    continue;
                }
                if n.contains('*') || n.contains('?') {
                    for ds_name in sorted_data_stream_names(store) {
                        let ds = &store.data_streams[&ds_name];
                        if glob_match(n, &ds_name) && !matched.contains(&ds_name) {
                            out.push(data_stream_view(store, ds));
                            matched.push(ds_name.clone());
                        }
                    }
                    continue;
                }
                missing = n.to_string();
            }
            if !missing.is_empty() {
                return err_response(
                    ApiError::new(
                        "index_not_found_exception",
                        &format!("no such index [{}]", missing),
                        404,
                    )
                    .field("index", json!(missing))
                    .field("index_uuid", json!("_na_")),
                );
            }
            json_response(200, json!({"data_streams": out}))
        }
        "DELETE" => {
            let names: Vec<String> = spec.split(',').map(|s| s.to_string()).collect();
            let mut deleted = false;
            for n in &names {
                let n = n.trim();
                if n.is_empty() {
                    continue;
                }
                if let Some(ds) = store.data_streams.get(n) {
                    for bn in &ds.backing_indices {
                        store.indices.remove(bn);
                    }
                    store.data_streams.remove(n);
                    deleted = true;
                    continue;
                }
                if n.contains('*') || n.contains('?') {
                    for ds_name in sorted_data_stream_names(store) {
                        if glob_match(n, &ds_name) {
                            let ds = store.data_streams.get(&ds_name).unwrap();
                            let backing = ds.backing_indices.clone();
                            for bn in backing {
                                store.indices.remove(&bn);
                            }
                            store.data_streams.remove(&ds_name);
                            deleted = true;
                        }
                    }
                    continue;
                }
                return err_response(ApiError::new(
                    "index_not_found_exception",
                    &format!("no such index [{}]", n),
                    404,
                ));
            }
            if deleted {
                store.bump_state();
            }
            json_response(200, json!({"acknowledged": true}))
        }
        _ => method_not_allowed(req, &["PUT", "GET", "DELETE"]),
    }
}

pub fn handle_data_stream_stats(store: &mut Store, req: &Req, spec: &str) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET", "HEAD"]);
    }
    let mut out: Vec<Value> = vec![];
    let mut count = 0;
    let mut backing = 0;
    for n in sorted_data_stream_names(store) {
        if !glob_match(spec, &n) {
            continue;
        }
        let ds = &store.data_streams[&n];
        count += 1;
        backing += ds.backing_indices.len();
        let mut max_ts: i64 = 0;
        for bn in &ds.backing_indices {
            let idx = match store.indices.get(bn) {
                Some(i) => i,
                None => continue,
            };
            for id in &idx.order {
                let doc = match idx.docs.get(id) {
                    Some(d) => d,
                    None => continue,
                };
                if let Some(v) = field_value(doc, &ds.timestamp_field) {
                    let parsed = match v {
                        Value::String(s) => parse_date_string(s),
                        Value::Number(num) => num.as_f64(),
                        _ => None,
                    };
                    if let Some(t) = parsed {
                        if t as i64 > max_ts {
                            max_ts = t as i64;
                        }
                    }
                }
            }
        }
        out.push(json!({
            "data_stream": n,
            "backing_indices": ds.backing_indices.len(),
            "store_size_bytes": 0,
            "maximum_timestamp": max_ts,
        }));
    }
    json_response(
        200,
        json!({
            "data_stream_count": count,
            "backing_indices": backing,
            "total_store_size_bytes": 0,
            "data_streams": out,
            "_shards": {"total": backing, "successful": backing, "failed": 0},
        }),
    )
}
