use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::shared::{err_index_not_found_named, flatten_settings, validate_index_name};
use crate::store::{settings_value_int, settings_value_string, Store};
use crate::Req;

pub fn handle_resize(
    store: &mut Store,
    req: &Req,
    source: &str,
    kind: &str,
    target: &str,
) -> Response {
    if req.method != "POST" {
        return method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let idx = match store.indices.get(source) {
        Some(i) => i.clone(),
        None => return err_response(err_index_not_found_named(source)),
    };
    if !idx.block_on("write") {
        return err_response(ApiError::new(
            "illegal_state_exception",
            &format!("index {} must block write operations to resize index", source),
            500,
        ));
    }
    if store.indices.contains_key(target) {
        return err_response(
            ApiError::new(
                "resource_already_exists_exception",
                &format!("index [{}] already exists", target),
                400,
            )
            .field("index", json!(target)),
        );
    }
    if let Err(e) = validate_index_name(target) {
        return err_response(e);
    }
    let mut body_settings = serde_json::Map::new();
    if let Some(settings_body) = body.get("settings").and_then(|s| s.as_object()) {
        flatten_settings(settings_body, &mut body_settings);
    }
    let n_raw = body_settings.get("index.number_of_shards");
    let has_n = n_raw.is_some();
    let src_shards = idx
        .settings
        .get("index.number_of_shards")
        .and_then(settings_value_int)
        .unwrap_or(1);
    let mut want = src_shards;
    match kind {
        "clone" => {
            if has_n {
                let n_raw = n_raw.unwrap();
                match settings_value_int(n_raw) {
                    Some(n) => want = n,
                    None => {
                        return err_response(ApiError::bad(&format!(
                            "Failed to parse value [{}] for setting [index.number_of_shards]",
                            settings_value_string(n_raw)
                        )))
                    }
                }
            }
            if want != src_shards {
                return err_response(ApiError::bad(&format!(
                    "the number of target shards ({}) must be the same as the number of source shards ({})",
                    want, src_shards
                )));
            }
        }
        "shrink" => {
            if src_shards == 1 {
                return err_response(ApiError::bad("can't shrink an index with only one shard"));
            }
            want = 1;
            if has_n {
                let n_raw = n_raw.unwrap();
                match settings_value_int(n_raw) {
                    Some(n) => want = n,
                    None => {
                        return err_response(ApiError::bad(&format!(
                            "Failed to parse value [{}] for setting [index.number_of_shards]",
                            settings_value_string(n_raw)
                        )))
                    }
                }
            }
            if want >= src_shards {
                return err_response(ApiError::bad(&format!(
                    "the number of target shards [{}] must be less that the number of source shards [{}]",
                    want, src_shards
                )));
            }
        }
        "split" => {
            if !has_n {
                return err_response(ApiError::new(
                    "action_request_validation_exception",
                    "Validation Failed: 1: index.number_of_shards is required for split operations;",
                    400,
                ));
            }
            let n_raw = n_raw.unwrap();
            match settings_value_int(n_raw) {
                Some(n) => want = n,
                None => {
                    return err_response(ApiError::bad(&format!(
                        "Failed to parse value [{}] for setting [index.number_of_shards]",
                        settings_value_string(n_raw)
                    )))
                }
            }
            if src_shards == 0 || want % src_shards != 0 {
                return err_response(ApiError::bad(&format!(
                    "the number of source shards [{}] must be a factor of [{}]",
                    src_shards, want
                )));
            }
        }
        _ => {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("unknown resize operation [{}]", kind),
                400,
            ))
        }
    }
    let mut new_settings = idx.settings.clone();
    new_settings.insert(
        "index.number_of_shards".to_string(),
        json!(want.to_string()),
    );
    for (k, v) in &body_settings {
        new_settings.insert(k.clone(), v.clone());
    }
    new_settings.insert("index.resize.source.name".to_string(), json!(source));
    new_settings.insert("index.resize.source.uuid".to_string(), json!(idx.uuid));
    new_settings.insert("index.resize.type".to_string(), json!(kind));

    let mut new_body = serde_json::Map::new();
    new_body.insert("settings".to_string(), Value::Object(new_settings));
    new_body.insert("mappings".to_string(), idx.mappings.clone());
    if let Some(a) = body.get("aliases") {
        if !a.is_null() {
            new_body.insert("aliases".to_string(), a.clone());
        }
    }
    if let Err(e) = crate::admin::create_index_internal(store, target, &new_body, false) {
        return err_response(e);
    }
    let docs: Vec<(String, Map<String, Value>)> = idx
        .order
        .iter()
        .filter_map(|id| {
            idx.docs
                .get(id)
                .and_then(|d| d.as_object())
                .map(|d| (id.clone(), d.clone()))
        })
        .collect();
    if let Some(new_idx) = store.indices.get_mut(target) {
        for (id, mut doc) in docs {
            let _ = crate::fields::compute_doc_values(new_idx, &id, &mut doc);
            new_idx.docs.insert(id.clone(), Value::Object(doc));
            new_idx.order.push(id);
        }
    }
    store.bump_state();
    json_response(
        200,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": target}),
    )
}
