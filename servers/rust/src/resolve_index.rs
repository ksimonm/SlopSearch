use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{json_response, method_not_allowed};
use crate::resp::glob_match;
use crate::shared::{accept_class, default_expand, parse_expand_wildcards, sorted_index_names};
use crate::{Req, Store};

pub fn handle_resolve_index(store: &mut Store, req: &Req, spec: &str) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET", "HEAD"]);
    }
    let mut expand = default_expand();
    if let Some(v) = req.param("expand_wildcards") {
        if !v.is_empty() {
            if let Ok(set) = parse_expand_wildcards(v, None) {
                expand = set;
            }
        }
    }

    let mut seen_idx: Vec<String> = vec![];
    let mut seen_alias: Map<String, Value> = Map::new();
    let mut seen_ds: Vec<String> = vec![];

    for tok in spec.split(',') {
        let token = tok.trim();
        if token.is_empty() {
            continue;
        }
        let wildcard = token == "_all" || token == "*" || token.contains('*') || token.contains('?');
        if wildcard {
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if !glob_match(token, &name) && token != "_all" && token != "*" {
                    continue;
                }
                if !accept_class(idx, &expand) {
                    continue;
                }
                if !seen_idx.contains(&name) {
                    seen_idx.push(name.clone());
                }
            }
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if !accept_class(idx, &expand) {
                    continue;
                }
                for alias in idx.aliases.keys() {
                    if glob_match(token, alias) || token == "_all" || token == "*" {
                        let entry = seen_alias
                            .entry(alias.clone())
                            .or_insert_with(|| Value::Array(vec![]));
                        if let Some(arr) = entry.as_array_mut() {
                            if !arr.iter().any(|v| v.as_str() == Some(name.as_str())) {
                                arr.push(json!(name));
                            }
                        }
                    }
                }
            }
            let mut ds_names: Vec<&String> = store.data_streams.keys().collect();
            ds_names.sort();
            for ds_name in ds_names {
                if glob_match(token, ds_name) || token == "_all" || token == "*" {
                    if !seen_ds.contains(ds_name) {
                        seen_ds.push(ds_name.clone());
                    }
                }
            }
        } else if let Some(idx) = store.indices.get(token) {
            if accept_class(idx, &expand) && !seen_idx.contains(&token.to_string()) {
                seen_idx.push(token.to_string());
            }
        } else {
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if idx.aliases.contains_key(token) {
                    let entry = seen_alias
                        .entry(token.to_string())
                        .or_insert_with(|| Value::Array(vec![]));
                    if let Some(arr) = entry.as_array_mut() {
                        if !arr.iter().any(|v| v.as_str() == Some(name.as_str())) {
                            arr.push(json!(name));
                        }
                    }
                }
            }
            if store.data_streams.contains_key(token) && !seen_ds.contains(&token.to_string()) {
                seen_ds.push(token.to_string());
            }
        }
    }

    seen_idx.sort();
    let mut indices_out: Vec<Value> = vec![];
    for name in &seen_idx {
        let idx = &store.indices[name];
        let mut entry = Map::new();
        entry.insert("name".to_string(), json!(name));
        let mut alias_names: Vec<&String> = idx.aliases.keys().collect();
        if !alias_names.is_empty() {
            alias_names.sort();
            entry.insert("aliases".to_string(), json!(alias_names));
        }
        let mut attrs: Vec<Value> = vec![];
        if idx.is_hidden() {
            attrs.push(json!("hidden"));
        }
        if idx.closed {
            attrs.push(json!("closed"));
        } else {
            attrs.push(json!("open"));
        }
        entry.insert("attributes".to_string(), Value::Array(attrs));
        indices_out.push(Value::Object(entry));
    }

    let mut alias_list: Vec<&String> = seen_alias.keys().collect();
    alias_list.sort();
    let mut aliases_out: Vec<Value> = vec![];
    for a in alias_list {
        let mut index_names: Vec<String> = seen_alias[a]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        index_names.sort();
        aliases_out.push(json!({"name": a, "indices": index_names}));
    }

    let mut ds_list: Vec<&String> = seen_ds.iter().collect();
    ds_list.sort();
    let mut ds_out: Vec<Value> = vec![];
    for n in ds_list {
        let ds = &store.data_streams[n];
        ds_out.push(json!({
            "name": n,
            "backing_indices": ds.backing_indices,
            "timestamp_field": ds.timestamp_field,
        }));
    }

    json_response(
        200,
        json!({
            "indices": indices_out,
            "aliases": aliases_out,
            "data_streams": ds_out,
        }),
    )
}
