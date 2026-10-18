use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::shared::{
    canonical_body_settings, default_expand, is_known_index_setting,
    meta_read_block, normalize_alias_def, parse_resolve_params, settings_to_nested, settings_view,
    validate_alias_name, validate_index_name, validate_mapping_full, validate_setting_value,
    ResolveOpts2,
};
use crate::store::{now_millis, random_uuid, settings_value_string, Index, Store};
use crate::templates::{compose_for_index, merge_mappings_deep};
use crate::Req;

fn read_body_map(req: &Req) -> Result<Map<String, Value>, ApiError> {
    let s = String::from_utf8_lossy(&req.body);
    if s.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&s) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(ApiError::new(
            "not_x_content_exception",
            "Compressor detection can only be called on some xcontent bytes or compressed xcontent bytes",
            400,
        )),
    }
}

// ------------------------------------------------------------ create / index

pub fn create_index(store: &mut Store, req: &Req, name: &str) -> Response {
    if let Err(e) = validate_index_name(name) {
        return err_response(e);
    }
    let body = match read_body_map(req) {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    match create_index_internal(store, name, &body, false) {
        Ok(_) => json_response(
            200,
            json!({"acknowledged": true, "shards_acknowledged": true, "index": name}),
        ),
        Err(e) => err_response(e),
    }
}

/// Builds and registers a new index. When `is_backing` is true, template
/// composition is skipped (the backing body was built from the data stream
/// template already).
pub fn create_index_internal(
    store: &mut Store,
    name: &str,
    body: &Map<String, Value>,
    is_backing: bool,
) -> Result<(), ApiError> {
    if store.indices.contains_key(name) {
        return Err(ApiError::new(
            "resource_already_exists_exception",
            &format!("index [{}]", name),
            400,
        )
        .field("index", json!(name)));
    }
    for idx in store.indices.values() {
        if idx.aliases.contains_key(name) {
            return Err(ApiError::new(
                "invalid_index_name_exception",
                &format!("Invalid index name [{}], already exists as alias", name),
                400,
            )
            .field("index", json!(name))
            .field("index_uuid", json!("_na_")));
        }
    }
    for k in body.keys() {
        match k.as_str() {
            "settings" | "mappings" | "aliases" => {}
            _ => {
                return Err(ApiError::new(
                    "parse_exception",
                    &format!("unknown key [{}] for create index", k),
                    400,
                ))
            }
        }
    }

    let composed = if is_backing {
        None
    } else {
        let c = compose_for_index(store, name);
        if c.ds_def.is_some() {
            return Err(ApiError::bad(&format!(
                "cannot create index with name [{}], because it matches with template [{}] that creates data streams only, use create data stream api instead",
                name, c.template
            )));
        }
        Some(c)
    };

    let mut canonical: Map<String, Value> = Map::new();
    if let Some(c) = &composed {
        for (k, v) in &c.settings {
            canonical.insert(k.clone(), v.clone());
        }
    }
    if let Some(settings_raw) = body.get("settings").and_then(|s| s.as_object()) {
        crate::shared::flatten_settings(settings_raw, &mut canonical);
    }
    let keys: Vec<String> = canonical.keys().cloned().collect();
    for k in keys {
        let v = canonical[&k].clone();
        if v.is_null() {
            canonical.remove(&k);
            continue;
        }
        if !is_known_index_setting(&k) {
            return Err(ApiError::new(
                "settings_exception",
                &format!("unknown setting [{}] please check that any required plugins are installed, or check the breaking changes documentation for removed settings", k),
                400,
            ));
        }
        let val = validate_setting_value(&k, &v, true)?;
        canonical.insert(k, Value::String(val));
    }
    if !canonical.contains_key("index.number_of_shards") {
        canonical.insert("index.number_of_shards".to_string(), json!("1"));
    }
    if !canonical.contains_key("index.number_of_replicas") {
        canonical.insert("index.number_of_replicas".to_string(), json!("1"));
    }

    if let Some(mappings_raw) = body.get("mappings").and_then(|m| m.as_object()) {
        for (k, v) in mappings_raw {
            if k == "properties" {
                continue;
            }
            if v.is_object() && k == "_doc" {
                return Err(ApiError::bad(
                    "The mapping definition cannot be nested under a type",
                ));
            }
        }
    }
    let mut merged_mappings = Map::new();
    if let Some(c) = &composed {
        if let Some(m) = c.mappings.as_object() {
            merge_mappings_deep(&mut merged_mappings, m);
        }
    }
    if let Some(mappings_raw) = body.get("mappings").and_then(|m| m.as_object()) {
        merge_mappings_deep(&mut merged_mappings, mappings_raw);
    }
    validate_mapping_full(&Value::Object(merged_mappings.clone()))?;

    let mut aliases: Map<String, Value> = Map::new();
    let mut add_alias = |store: &Store,
                         aliases: &mut Map<String, Value>,
                         alias_name: &str,
                         def_raw: Option<&Value>|
     -> Result<(), ApiError> {
        validate_alias_name(alias_name)?;
        if store.indices.contains_key(alias_name) {
            return Err(ApiError::new(
                "invalid_alias_name_exception",
                &format!(
                    "Invalid alias name [{}], an index exists with the same name as the alias",
                    alias_name
                ),
                400,
            ));
        }
        let def = def_raw.and_then(|d| d.as_object()).cloned().unwrap_or_default();
        if let Some(filt) = def.get("filter") {
            if !filt.is_null() {
                if let Some(fq) = filt.as_object() {
                    let dummy = Index::skeleton("");
                    if let Err(err) =
                        crate::search::validate_query(&dummy, &Value::Object(fq.clone()))
                    {
                        let mut e = ApiError::bad(&format!(
                            "failed to parse filter for alias [{}]",
                            alias_name
                        ));
                        e.caused_by = Some(Box::new(ApiError::new(
                            "parsing_exception",
                            &err.reason,
                            400,
                        )));
                        return Err(e);
                    }
                }
            }
        }
        aliases.insert(alias_name.to_string(), Value::Object(normalize_alias_def(&def)));
        Ok(())
    };
    if let Some(c) = &composed {
        for (alias_name, def_raw) in &c.aliases {
            add_alias(store, &mut aliases, alias_name, Some(def_raw))?;
        }
    }
    if let Some(aliases_raw) = body.get("aliases").and_then(|a| a.as_object()) {
        for (alias_name, def_raw) in aliases_raw {
            add_alias(store, &mut aliases, alias_name, Some(def_raw))?;
        }
    }

    let idx = Index {
        name: name.to_string(),
        uuid: random_uuid(),
        creation_date: now_millis(),
        mappings: Value::Object(merged_mappings),
        settings: canonical,
        docs: std::collections::HashMap::new(),
        order: Vec::new(),
        field_types: std::collections::HashMap::new(),
        closed: false,
        aliases,
        mapping_version: 0,
        pending: std::collections::HashMap::new(),
        routing: std::collections::HashMap::new(),
        seg_gen: 0,
        deletes: 0,
        data_stream: String::new(),
        verified: false,
        index_total: 0,
        delete_total: 0,
        force_merged: false,
        doc_values: std::collections::HashMap::new(),
        doc_ignored: std::collections::HashMap::new(),
        versions: std::collections::HashMap::new(),
        seq_nos: std::collections::HashMap::new(),
        raw_sources: std::collections::HashMap::new(),
    };
    store.indices.insert(name.to_string(), idx);
    if let Some(i) = store.indices.get_mut(name) {
        i.ensure_field_types();
    }
    store.bump_state();
    Ok(())
}

// -------------------------------------------------------------- get / delete

pub fn get_index_meta(store: &mut Store, req: &Req, spec: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    if req.method == "HEAD" {
        let has_wildcard = spec.contains('*') || spec.contains('?') || spec == "_all";
        let status = if !names.is_empty() || has_wildcard {
            200
        } else {
            404
        };
        return (
            StatusCode::from_u16(status).unwrap(),
            [(axum::http::header::CONTENT_TYPE, "application/json; charset=UTF-8")],
            "",
        )
            .into_response();
    }
    let flat = req.bool_param("flat_settings");
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        let mut alias_defs = Map::new();
        for (an, def) in &idx.aliases {
            alias_defs.insert(an.clone(), def.clone());
        }
        let settings = if flat {
            Value::Object(crate::shared::materialized_flat(idx))
        } else {
            settings_to_nested(&crate::shared::materialized_flat(idx))
        };
        let mut entry = Map::new();
        entry.insert("aliases".to_string(), Value::Object(alias_defs));
        entry.insert("mappings".to_string(), idx.mappings.clone());
        entry.insert("settings".to_string(), settings);
        if !idx.data_stream.is_empty() {
            entry.insert("data_stream".to_string(), json!(idx.data_stream));
        }
        out.insert(idx.name.clone(), Value::Object(entry));
    }
    json_response(200, Value::Object(out))
}

pub fn delete_indices(store: &mut Store, req: &Req, spec: &str) -> Response {
    let mut o = parse_resolve_params(req, ResolveOpts2::default());
    o.reject_alias = true;
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    for name in &names {
        let idx = &store.indices[name];
        if !idx.data_stream.is_empty() {
            if let Some(ds) = store.data_streams.get(&idx.data_stream) {
                if !ds.backing_indices.is_empty()
                    && ds.backing_indices.last() == Some(&idx.name)
                {
                    return err_response(ApiError::bad(&format!(
                        "index [{}] is the write index for data stream [{}] and cannot be deleted",
                        idx.name, idx.data_stream
                    )));
                }
            }
        }
        if let Some(err) = crate::shared::parse_index_block_error(idx, "metadata", 9, 403) {
            return err_response(err);
        }
        if let Some(err) = crate::shared::parse_index_block_error(idx, "read_only", 5, 403) {
            return err_response(err);
        }
    }
    for name in &names {
        store.indices.remove(name);
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}

// ---------------------------------------------------------------- mappings

fn collect_leaves(props: &Map<String, Value>, prefix: &str, out: &mut Vec<(String, String, Map<String, Value>)>) {
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
            collect_leaves(sub, &path, out);
        }
        if def.contains_key("type") {
            out.push((path.clone(), name.clone(), def.clone()));
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            for (fname, fraw) in fields {
                if let Some(fdef) = fraw.as_object() {
                    out.push((format!("{}.{}", path, fname), fname.clone(), fdef.clone()));
                }
            }
        }
    }
}

fn merge_field_defaults(def: &mut Map<String, Value>) {
    let t = def.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let base: Value = match t {
        "float" | "double" | "integer" | "long" | "short" | "byte" | "half_float"
        | "scaled_float" => json!({"index": true, "doc_values": true, "store": false,
            "coerce": true, "ignore_malformed": false, "null_value": null}),
        "keyword" | "constant_keyword" | "wildcard" => json!({"index": true, "doc_values": true, "store": false,
            "ignore_above": 2147483647, "null_value": null}),
        "text" => json!({"index": true, "store": false, "analyzer": "default",
            "search_analyzer": "default", "boost": 1.0, "null_value": null}),
        "date" => json!({"index": true, "doc_values": true, "store": false, "null_value": null}),
        "boolean" => json!({"index": true, "doc_values": true, "store": false, "null_value": null}),
        "object" | "nested" => json!({"dynamic": "true", "enabled": true}),
        "ip" | "binary" | "geo_point" | "geo_shape" | "completion" => {
            json!({"index": true, "doc_values": true, "store": false, "null_value": null})
        }
        _ => json!({}),
    };
    for (k, v) in base.as_object().unwrap() {
        if !def.contains_key(k) {
            def.insert(k.clone(), v.clone());
        }
    }
}

fn field_mappings(idx: &Index, spec: &str, include_defaults: bool) -> Value {
    let mut leaves = vec![];
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        collect_leaves(props, "", &mut leaves);
    }
    let patterns: Vec<String> = spec.split(',').map(|s| s.to_string()).collect();
    let mut result = Map::new();
    for (path, leaf_name, def) in leaves {
        if !crate::shared::match_any_setting_pattern(&patterns, &path) {
            continue;
        }
        let mut def = def;
        if include_defaults {
            merge_field_defaults(&mut def);
        }
        result.insert(
            path.clone(),
            json!({"full_name": path, "mapping": {leaf_name: def}}),
        );
    }
    Value::Object(result)
}

pub fn get_mapping_api(store: &mut Store, req: &Req, spec: &str, fields: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let include_defaults = req.bool_param("include_defaults");
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        if let Some(err) = meta_read_block(idx) {
            return err_response(err);
        }
        if fields.is_empty() {
            out.insert(
                idx.name.clone(),
                json!({"mappings": idx.mappings.clone()}),
            );
        } else {
            out.insert(
                idx.name.clone(),
                json!({"mappings": field_mappings(idx, fields, include_defaults)}),
            );
        }
    }
    json_response(200, Value::Object(out))
}

pub fn put_mapping_api(store: &mut Store, req: &Req, spec: &str) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    for k in body.keys() {
        let known = matches!(
            k.as_str(),
            "properties"
                | "dynamic"
                | "_source"
                | "_meta"
                | "dynamic_templates"
                | "date_detection"
                | "numeric_detection"
                | "_routing"
                | "_field_names"
                | "runtime"
                | "_size"
                | "_timestamp"
                | "_ttl"
        );
        if !known {
            return err_response(ApiError::bad(
                "Types cannot be provided in put mapping requests",
            ));
        }
    }
    if let Some(props) = body.get("properties").and_then(|p| p.as_object()) {
        if let Err(e) = crate::shared::validate_mapping_full(&json!({"properties": props})) {
            let reason = e
                .caused_by
                .as_ref()
                .map(|c| c.reason.clone())
                .unwrap_or_else(|| e.reason.clone());
            let mut err = ApiError::new(
                "mapper_parsing_exception",
                &format!("Failed to parse mapping [_doc]: {}", reason),
                400,
            );
            err.caused_by = Some(Box::new(ApiError::new(
                "mapper_parsing_exception",
                &reason,
                400,
            )));
            return err_response(err);
        }
    }
    for name in &names {
        let idx = &store.indices[name];
        if let Some(err) = meta_read_block(idx) {
            return err_response(err);
        }
    }
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        if idx.mappings.get("properties").and_then(|p| p.as_object()).is_none() {
            idx.mappings
                .as_object_mut()
                .unwrap()
                .insert("properties".to_string(), Value::Object(Map::new()));
        }
        if let Some(props) = body.get("properties").and_then(|p| p.as_object()) {
            let existing = idx
                .mappings
                .get_mut("properties")
                .and_then(|p| p.as_object_mut())
                .unwrap();
            if let Err(e) = merge_mapping_checked(existing, props) {
                return err_response(e);
            }
        }
        if let Some(d) = body.get("dynamic") {
            idx.mappings
                .as_object_mut()
                .unwrap()
                .insert("dynamic".to_string(), d.clone());
        }
        for (k, v) in &body {
            if k == "properties" || k == "dynamic" {
                continue;
            }
            idx.mappings
                .as_object_mut()
                .unwrap()
                .insert(k.clone(), v.clone());
        }
        idx.mapping_version += 1;
        idx.ensure_field_types();
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}

/// Merges new mapping properties into existing ones, returning
/// OpenSearch-compatible conflict errors.
fn merge_mapping_checked(
    existing: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
) -> Result<(), ApiError> {
    for (name, raw_new) in incoming {
        let new_def = match raw_new.as_object() {
            Some(d) => d,
            None => continue,
        };
        let old_def = match existing.get(name).and_then(|o| o.as_object()) {
            Some(o) => o.clone(),
            None => {
                existing.insert(name.clone(), raw_new.clone());
                continue;
            }
        };
        let new_type = new_def.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let old_type = old_def.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let old_props = old_def.get("properties").and_then(|p| p.as_object());
        let new_props = new_def.get("properties").and_then(|p| p.as_object());
        if (old_props.is_some() && !new_type.is_empty() && new_props.is_none())
            || (old_props.is_none() && new_props.is_some())
        {
            return Err(ApiError::bad(&format!(
                "can't merge a non object mapping [{}] with an object mapping",
                name
            )));
        }
        let mut old_clone = old_def.clone();
        if let (Some(op), Some(np)) = (old_props, new_props) {
            let mut op2 = op.clone();
            merge_mapping_checked(&mut op2, np)?;
            old_clone.insert("properties".to_string(), Value::Object(op2));
        }
        if !new_type.is_empty() && !old_type.is_empty() && new_type != old_type {
            return Err(ApiError::bad(&format!(
                "mapper [{}] cannot be changed from type [{}] to [{}]",
                name, old_type, new_type
            )));
        }
        for (pk, pv) in new_def {
            match pk.as_str() {
                "type" | "properties" => continue,
                "fields" => {
                    if !old_clone
                        .get("fields")
                        .map(|f| f.is_object())
                        .unwrap_or(false)
                    {
                        old_clone.insert("fields".to_string(), Value::Object(Map::new()));
                    }
                    let old_fields =
                        old_clone.get_mut("fields").unwrap().as_object_mut().unwrap();
                    if let Some(new_fields) = pv.as_object() {
                        for (fk, fv) in new_fields {
                            old_fields.insert(fk.clone(), fv.clone());
                        }
                    }
                    continue;
                }
                _ => {}
            }
            match old_clone.get(pk) {
                None => {
                    if pk == "analyzer" && old_type == "text" {
                        let pvs = settings_value_string(pv);
                        if pvs != "default" && pvs != "standard" {
                            return Err(ApiError::bad(&format!(
                                "Mapper for [{}] conflicts with existing mapper:\n\tCannot update parameter [analyzer] from [default] to [{}]",
                                name, pvs
                            )));
                        }
                    }
                    old_clone.insert(pk.clone(), pv.clone());
                }
                Some(old_val) => {
                    if settings_value_string(old_val) != settings_value_string(pv) {
                        return Err(ApiError::bad(&format!(
                            "Mapper for [{}] conflicts with existing mapper:\n\tCannot update parameter [{}] from [{}] to [{}]",
                            name,
                            pk,
                            settings_value_string(old_val),
                            settings_value_string(pv)
                        )));
                    }
                }
            }
        }
        existing.insert(name.clone(), Value::Object(old_clone));
    }
    Ok(())
}

// ---------------------------------------------------------------- settings

pub fn get_settings_api(store: &mut Store, req: &Req, spec: &str, filter: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let flat = req.bool_param("flat_settings");
    let include_defaults = req.bool_param("include_defaults");
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        if let Some(err) = meta_read_block(idx) {
            return err_response(err);
        }
        if let Some(view) = settings_view(idx, flat, include_defaults, filter) {
            out.insert(idx.name.clone(), view);
        }
    }
    json_response(200, Value::Object(out))
}

pub fn put_settings_api(store: &mut Store, req: &Req, spec: &str) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let updates = canonical_body_settings(&body);
    if updates.is_empty() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: no settings to update;",
        ));
    }
    let mut o = parse_resolve_params(req, ResolveOpts2::default());
    if o.allow_no_indices.is_none() {
        o.allow_no_indices = Some(false);
    }
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let preserve = req.bool_param("preserve_existing");
    let only_blocks = updates.keys().all(|k| k.starts_with("index.blocks."));
    for name in &names {
        let idx = &store.indices[name];
        if !only_blocks {
            if let Some(err) = crate::shared::parse_index_block_error(idx, "read_only", 5, 403) {
                return err_response(err);
            }
            if let Some(err) = meta_read_block(idx) {
                return err_response(err);
            }
        }
        let mut non_dynamic: Vec<String> = vec![];
        let mut closed_only: Vec<String> = vec![];
        for (k, v) in &updates {
            if v.is_null() || (preserve && idx.settings.contains_key(k)) {
                continue;
            }
            if !is_known_index_setting(k) && !k.starts_with("index.analysis.") {
                return err_response(ApiError::new(
                    "settings_exception",
                    &format!("unknown setting [{}] please check that any required plugins are installed, or check the breaking changes documentation for removed settings", k),
                    400,
                ));
            }
            if k == "index.number_of_shards" {
                non_dynamic.push(k.clone());
                continue;
            }
            if k.starts_with("index.analysis.") {
                closed_only.push(k.clone());
                continue;
            }
            if let Err(e) = validate_setting_value(k, v, false) {
                return err_response(e);
            }
        }
        non_dynamic.sort();
        closed_only.sort();
        if !non_dynamic.is_empty() {
            if idx.closed {
                return err_response(ApiError::new(
                    "settings_exception",
                    &format!(
                        "Validation Failed: 1: setting [{}], not updateable;",
                        non_dynamic[0]
                    ),
                    400,
                ));
            }
            return err_response(ApiError::bad(&format!(
                "Can't update non dynamic settings [[{}]] for open indices [[{}/{}]]",
                non_dynamic.join(", "),
                idx.name,
                idx.uuid
            )));
        }
        if !closed_only.is_empty() && !idx.closed {
            return err_response(ApiError::bad(&format!(
                "Can't update non dynamic settings [[{}]] for open indices [[{}/{}]]",
                closed_only.join(", "),
                idx.name,
                idx.uuid
            )));
        }
    }
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        for (k, v) in &updates {
            if preserve && idx.settings.contains_key(k) {
                continue;
            }
            if v.is_null() {
                idx.settings.remove(k);
                continue;
            }
            if k.starts_with("index.analysis.") {
                idx.settings.insert(k.clone(), v.clone());
                continue;
            }
            if let Ok(val) = validate_setting_value(k, v, false) {
                idx.settings.insert(k.clone(), Value::String(val));
            }
        }
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}

// ------------------------------------------------------------------ blocks

pub fn put_block_api(store: &mut Store, req: &Req, spec: &str, block: &str) -> Response {
    if !matches!(
        block,
        "write" | "read" | "read_only" | "read_only_allow_delete" | "metadata"
    ) {
        return err_response(ApiError::bad(&format!("No block found with name {}", block)));
    }
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut arr = vec![];
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        idx.settings.insert(
            format!("index.blocks.{}", block),
            Value::String("true".to_string()),
        );
        arr.push(json!({"name": idx.name, "blocked": true}));
    }
    store.bump_state();
    json_response(
        200,
        json!({"acknowledged": true, "shards_acknowledged": true, "indices": arr}),
    )
}

// -------------------------------------------------------------- close / open

pub fn handle_close_open(store: &mut Store, req: &Req, spec: &str, close: bool) -> Response {
    if req.method != "POST" {
        return method_not_allowed(req, &["POST"]);
    }
    let mut o = ResolveOpts2 {
        expand: Some(default_expand().clone()),
        ..Default::default()
    };
    if let Some(expand) = o.expand.as_mut() {
        expand.insert("closed".to_string(), true);
    }
    o = parse_resolve_params(req, o);
    let names = match crate::shared::resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut out = Map::new();
    let mut changed = 0;
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        if close {
            if idx.closed {
                continue;
            }
            idx.closed = true;
            idx.settings.insert(
                "index.verified_before_close".to_string(),
                Value::String("true".to_string()),
            );
            changed += 1;
            out.insert(idx.name.clone(), json!({"closed": true}));
        } else {
            if !idx.closed {
                continue;
            }
            idx.closed = false;
            idx.settings.remove("index.verified_before_close");
            changed += 1;
        }
    }
    store.bump_state();
    if close {
        json_response(
            200,
            json!({
                "acknowledged": true,
                "shards_acknowledged": changed > 0,
                "indices": out,
            }),
        )
    } else {
        json_response(200, json!({"acknowledged": true, "shards_acknowledged": true}))
    }
}
