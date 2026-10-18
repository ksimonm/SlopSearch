use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, write_simple_error, ApiError};
use crate::resp::glob_match;
use crate::shared::{
    normalize_alias_def, parse_resolve_params, resolve_spec2, sorted_index_names,
    validate_alias_name, ResolveOpts2,
};
use crate::store::{Index, Store};
use crate::Req;

// ------------------------------------------------------------- alias views

fn alias_entry(def: &Value) -> Value {
    match def.as_object() {
        Some(m) => Value::Object(m.clone()),
        None => json!({}),
    }
}

fn index_aliases_view(store: &Store, names: &[String], name_spec: &str, include_all: bool) -> Value {
    let mut out = Map::new();
    for name in names {
        let idx = &store.indices[name];
        let mut aliases = Map::new();
        for (an, def) in &idx.aliases {
            if name_spec.is_empty() || include_all || glob_match(name_spec, an) {
                aliases.insert(an.clone(), alias_entry(def));
            }
        }
        if name_spec.is_empty() || include_all || !aliases.is_empty() {
            out.insert(idx.name.clone(), json!({"aliases": aliases}));
        }
    }
    Value::Object(out)
}

pub fn get_aliases_indexed(store: &mut Store, req: &Req, index_spec: &str, alias_spec: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, index_spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    if alias_spec.is_empty() {
        return json_response(200, index_aliases_view(store, &names, "", true));
    }
    let wants: Vec<String> = alias_spec.split(',').map(|s| s.to_string()).collect();
    let has_wildcard = alias_spec.contains('*') || alias_spec.contains('?');
    let mut found: Vec<String> = vec![];
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        let mut aliases = Map::new();
        for want in &wants {
            for (an, def) in &idx.aliases {
                if an == want || glob_match(want, an) {
                    aliases.insert(an.clone(), alias_entry(def));
                    if !found.contains(an) {
                        found.push(an.clone());
                    }
                    if !found.contains(want) {
                        found.push(want.clone());
                    }
                }
            }
        }
        if !aliases.is_empty() {
            out.insert(idx.name.clone(), json!({"aliases": aliases}));
        }
    }
    if has_wildcard {
        return json_response(200, Value::Object(out));
    }
    for want in &wants {
        if !found.contains(want) {
            return write_simple_error(404, &format!("alias [{}] missing", want));
        }
    }
    json_response(200, Value::Object(out))
}

pub fn get_aliases_global(
    store: &mut Store,
    _req: &Req,
    alias_spec: &str,
    include_no_alias: bool,
) -> Response {
    let mut out = Map::new();
    let mut missing = String::new();
    if alias_spec.is_empty() {
        for name in sorted_index_names(store) {
            let idx = &store.indices[&name];
            let mut aliases = Map::new();
            for (an, def) in &idx.aliases {
                aliases.insert(an.clone(), alias_entry(def));
            }
            if !aliases.is_empty() || include_no_alias {
                out.insert(idx.name.clone(), json!({"aliases": aliases}));
            }
        }
        return json_response(200, Value::Object(out));
    }
    let has_wildcard = alias_spec.contains('*') || alias_spec.contains('?');
    let wants: Vec<String> = alias_spec.split(',').map(|s| s.to_string()).collect();
    let mut found: Vec<String> = vec![];
    for name in sorted_index_names(store) {
        let idx = &store.indices[&name];
        let mut aliases = Map::new();
        for want in &wants {
            for (an, def) in &idx.aliases {
                if an == want || glob_match(want, an) {
                    aliases.insert(an.clone(), alias_entry(def));
                    if !found.contains(want) {
                        found.push(want.clone());
                    }
                }
            }
        }
        if !aliases.is_empty() {
            out.insert(idx.name.clone(), json!({"aliases": aliases}));
        }
    }
    for want in &wants {
        if !has_wildcard && !found.contains(want) {
            missing = want.clone();
            break;
        }
    }
    if has_wildcard && out.is_empty() {
        missing = wants[0].clone();
    }
    if !missing.is_empty() {
        if !out.is_empty() {
            out.insert("error".to_string(), json!(format!("alias [{}] missing", missing)));
            out.insert("status".to_string(), json!(404));
            return json_response(404, Value::Object(out));
        }
        return write_simple_error(404, &format!("alias [{}] missing", missing));
    }
    json_response(200, Value::Object(out))
}

pub fn head_aliases_global(store: &mut Store, _req: &Req, alias_spec: &str) -> Response {
    for idx in store.indices.values() {
        for an in idx.aliases.keys() {
            if an == alias_spec || glob_match(alias_spec, an) {
                return json_response(200, Value::Null);
            }
        }
    }
    json_response(404, Value::Null)
}

// ---------------------------------------------------------------- put/delete

fn alias_hidden_conflict(store: &Store, alias_name: &str, def: &Map<String, Value>) -> Option<ApiError> {
    let new_hidden = def.get("is_hidden");
    for idx in store.indices.values() {
        let existing = match idx.aliases.get(alias_name) {
            Some(e) => e,
            None => continue,
        };
        let old_hidden = existing.get("is_hidden");
        if new_hidden.is_some() != old_hidden.is_some()
            || (new_hidden.is_some() && old_hidden != new_hidden)
        {
            return Some(ApiError::new(
                "illegal_state_exception",
                "alias must have the same is_hidden setting on all indices",
                500,
            ));
        }
    }
    None
}

fn multiple_write_indices_err(alias: &str, mut indices: Vec<String>) -> ApiError {
    indices.sort();
    ApiError::new(
        "illegal_state_exception",
        &format!(
            "alias [{}] has more than one write index [{}]",
            alias,
            indices.join(",")
        ),
        500,
    )
}

pub fn put_alias_api(store: &mut Store, req: &Req, index_spec: &str, alias_name: &str) -> Response {
    if let Err(e) = validate_alias_name(alias_name) {
        return err_response(e);
    }
    for an in alias_name.split(',') {
        if store.indices.contains_key(an) {
            return err_response(ApiError::new(
                "invalid_alias_name_exception",
                &format!(
                    "Invalid alias name [{}], an index exists with the same name as the alias",
                    an
                ),
                400,
            ));
        }
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, index_spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut def = normalize_alias_def(&body);
    if let Some(filt) = def.get("filter") {
        if !filt.is_null() {
            if let Some(fq) = filt.as_object() {
                let dummy = Index::skeleton("");
                if let Err(err) = crate::search::validate_query(&dummy, &Value::Object(fq.clone())) {
                    let mut e = ApiError::bad(&format!(
                        "failed to parse filter for alias [{}]",
                        alias_name
                    ));
                    e.caused_by = Some(Box::new(ApiError::new(
                        "parsing_exception",
                        &err.reason,
                        400,
                    )));
                    return err_response(e);
                }
            }
        }
    }
    if names.is_empty() {
        return err_response(crate::shared::err_index_not_found_named(index_spec));
    }
    if let Some(e) = alias_hidden_conflict(store, alias_name, &def) {
        return err_response(e);
    }
    if def.get("is_write_index").and_then(|v| v.as_bool()) == Some(true) {
        for idx in &names {
            let mut others: Vec<String> = vec![];
            for other in store.indices.values() {
                if other.name == *idx {
                    continue;
                }
                if let Some(d) = other.aliases.get(alias_name) {
                    if d.get("is_write_index").and_then(|v| v.as_bool()) == Some(true) {
                        others.push(other.name.clone());
                    }
                }
            }
            if !others.is_empty() {
                others.push(idx.clone());
                return err_response(multiple_write_indices_err(alias_name, others));
            }
        }
    }
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        idx.aliases.insert(alias_name.to_string(), Value::Object(def.clone()));
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}

pub fn delete_alias_api(store: &mut Store, req: &Req, index_spec: &str, alias_spec: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, index_spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    if alias_spec.is_empty() {
        return json_response(200, json!({"acknowledged": true}));
    }
    let wants: Vec<String> = alias_spec.split(',').map(|s| s.to_string()).collect();
    let mut any_deleted = false;
    let mut missing: Vec<String> = vec![];
    for want in &wants {
        let mut deleted = false;
        for name in &names {
            let idx = store.indices.get_mut(name).unwrap();
            let to_remove: Vec<String> = idx
                .aliases
                .keys()
                .filter(|an| *an == want || glob_match(want, an))
                .cloned()
                .collect();
            for an in to_remove {
                idx.aliases.remove(&an);
                deleted = true;
                any_deleted = true;
            }
        }
        if !deleted {
            missing.push(want.clone());
        }
    }
    if !any_deleted && !missing.is_empty() {
        return err_response(
            ApiError::new(
                "aliases_not_found_exception",
                &format!("aliases [{}] missing", missing.join(",")),
                404,
            )
            .field("resource.type", json!("aliases"))
            .field("resource.id", json!(missing.join(","))),
        );
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}

// ---------------------------------------------------------------- _aliases

struct AliasAction {
    kind: String,
    def: Map<String, Value>,
}

fn expand_names(def: &Map<String, Value>) -> (Vec<String>, Vec<String>) {
    let mut index_names: Vec<String> = vec![];
    let mut alias_names: Vec<String> = vec![];
    let mut push_strings = |v: Option<&Value>, out: &mut Vec<String>| {
        match v {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(a)) => {
                for e in a {
                    if let Some(s) = e.as_str() {
                        out.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    };
    push_strings(def.get("index"), &mut index_names);
    push_strings(def.get("indices"), &mut index_names);
    push_strings(def.get("alias"), &mut alias_names);
    push_strings(def.get("aliases"), &mut alias_names);
    (index_names, alias_names)
}

pub fn post_aliases(store: &mut Store, req: &Req) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let raw_actions = match body.get("actions").and_then(|a| a.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => {
            return err_response(ApiError::bad("No action specified"));
        }
    };
    let mut actions: Vec<AliasAction> = vec![];
    for raw in raw_actions {
        let m = match raw.as_object() {
            Some(m) if m.len() == 1 => m,
            _ => {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    "Failed to parse alias action",
                    400,
                ))
            }
        };
        for (kind, def_raw) in m {
            let mut def = def_raw.as_object().cloned().unwrap_or_default();
            if def_raw.is_null() {
                def = Map::new();
            }
            match kind.as_str() {
                "add" | "remove" | "remove_index" => {}
                _ => {
                    return err_response(ApiError::new(
                        "x_content_parse_exception",
                        &format!("unknown alias action [{}]", kind),
                        400,
                    ))
                }
            }
            if !def.contains_key("index") && !def.contains_key("indices") {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    "Failed to parse alias action",
                    400,
                ));
            }
            if kind == "add" || kind == "remove" {
                if !def.contains_key("alias") && !def.contains_key("aliases") {
                    return err_response(ApiError::new(
                        "x_content_parse_exception",
                        "Failed to parse alias action",
                        400,
                    ));
                }
            }
            actions.push(AliasAction {
                kind: kind.clone(),
                def,
            });
        }
    }

    struct PendingChange {
        idx: String,
        alias: String,
        def: Map<String, Value>,
        delete: bool,
    }
    let mut changes: Vec<PendingChange> = vec![];
    let mut remove_indexes: Vec<String> = vec![];
    let mut alias_missing = String::new();
    let mut applied = false;

    let resolve_for_action = |store: &Store, name: &str, require_existing: bool| -> Result<Vec<String>, ApiError> {
        let mut matched: Vec<String> = vec![];
        if store.indices.contains_key(name) {
            matched.push(name.to_string());
        } else if name.contains('*') || name.contains('?') {
            for n in sorted_index_names(store) {
                let idx = &store.indices[&n];
                if glob_match(name, &idx.name) {
                    matched.push(n.clone());
                    continue;
                }
                for an in idx.aliases.keys() {
                    if glob_match(name, an) {
                        matched.push(n.clone());
                        break;
                    }
                }
            }
        } else {
            for n in sorted_index_names(store) {
                let idx = &store.indices[&n];
                if idx.aliases.contains_key(name) {
                    matched.push(n.clone());
                }
            }
        }
        if matched.is_empty() && require_existing {
            return Err(crate::shared::err_index_not_found_named(name));
        }
        Ok(matched)
    };

    for a in &actions {
        match a.kind.as_str() {
            "add" => {
                let (index_names, alias_names) = expand_names(&a.def);
                for alias_name in &alias_names {
                    if let Err(e) = validate_alias_name(alias_name) {
                        return err_response(e);
                    }
                }
                let mut target_indices: Vec<String> = vec![];
                for in_ in &index_names {
                    match resolve_for_action(store, in_, true) {
                        Ok(m) => target_indices.extend(m),
                        Err(e) => return err_response(e),
                    }
                }
                let mut def = Map::new();
                for (k, v) in &a.def {
                    match k.as_str() {
                        "index" | "indices" | "alias" | "aliases" => continue,
                        _ => {
                            def.insert(k.clone(), v.clone());
                        }
                    }
                }
                let def = normalize_alias_def(&def);
                if let Some(filt) = def.get("filter") {
                    if !filt.is_null() {
                        if let Some(fq) = filt.as_object() {
                            let dummy = Index::skeleton("");
                            if let Err(err) =
                                crate::search::validate_query(&dummy, &Value::Object(fq.clone()))
                            {
                                let mut e = ApiError::bad(&format!(
                                    "failed to parse filter for alias [{}]",
                                    alias_names.join(",")
                                ));
                                e.caused_by = Some(Box::new(ApiError::new(
                                    "parsing_exception",
                                    &err.reason,
                                    400,
                                )));
                                return err_response(e);
                            }
                        }
                    }
                }
                for alias_name in &alias_names {
                    if let Some(e) = alias_hidden_conflict(store, alias_name, &def) {
                        return err_response(e);
                    }
                }
                for idx in &target_indices {
                    for alias_name in &alias_names {
                        if def.get("is_write_index").and_then(|v| v.as_bool()) == Some(true) {
                            let mut others: Vec<String> = vec![];
                            for other in store.indices.values() {
                                if other.name == *idx {
                                    continue;
                                }
                                if let Some(d) = other.aliases.get(alias_name) {
                                    if d.get("is_write_index").and_then(|v| v.as_bool())
                                        == Some(true)
                                    {
                                        others.push(other.name.clone());
                                    }
                                }
                            }
                            for ch in &changes {
                                if ch.idx != *idx && ch.alias == *alias_name && !ch.delete {
                                    if ch.def.get("is_write_index").and_then(|v| v.as_bool())
                                        == Some(true)
                                    {
                                        others.push(ch.idx.clone());
                                    }
                                }
                            }
                            if !others.is_empty() {
                                others.push(idx.clone());
                                return err_response(multiple_write_indices_err(alias_name, others));
                            }
                        }
                        changes.push(PendingChange {
                            idx: idx.clone(),
                            alias: alias_name.clone(),
                            def: def.clone(),
                            delete: false,
                        });
                        applied = true;
                    }
                }
            }
            "remove" => {
                let (index_names, alias_names) = expand_names(&a.def);
                let require_existing = a
                    .def
                    .get("must_exist")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let mut target_indices: Vec<String> = vec![];
                for in_ in &index_names {
                    match resolve_for_action(store, in_, true) {
                        Ok(m) => target_indices.extend(m),
                        Err(e) => return err_response(e),
                    }
                }
                for idx in &target_indices {
                    for alias_name in &alias_names {
                        if store.indices[idx].aliases.contains_key(alias_name) {
                            changes.push(PendingChange {
                                idx: idx.clone(),
                                alias: alias_name.clone(),
                                def: Map::new(),
                                delete: true,
                            });
                            applied = true;
                        } else if require_existing && alias_missing.is_empty() && !applied {
                            alias_missing = alias_name.clone();
                        }
                    }
                }
            }
            "remove_index" => {
                let (index_names, _) = expand_names(&a.def);
                for in_ in &index_names {
                    if store.indices.contains_key(in_) {
                        remove_indexes.push(in_.clone());
                        continue;
                    }
                    if in_.contains('*') || in_.contains('?') {
                        match resolve_for_action(store, in_, true) {
                            Ok(m) => remove_indexes.extend(m),
                            Err(e) => return err_response(e),
                        }
                        continue;
                    }
                    let alias_hit = store
                        .indices
                        .values()
                        .any(|idx| idx.aliases.contains_key(in_));
                    if alias_hit {
                        return err_response(ApiError::bad(&format!(
                            "The provided expression [{}] matches an alias, specify the corresponding concrete indices instead.",
                            in_
                        )));
                    }
                    return err_response(crate::shared::err_index_not_found_named(in_));
                }
                if !remove_indexes.is_empty() {
                    applied = true;
                }
            }
            _ => {}
        }
    }

    if changes.is_empty() && remove_indexes.is_empty() {
        if !alias_missing.is_empty() {
            return err_response(
                ApiError::new(
                    "aliases_not_found_exception",
                    &format!("aliases [{}] missing", alias_missing),
                    404,
                )
                .field("resource.type", json!("aliases"))
                .field("resource.id", json!(alias_missing)),
            );
        }
        return json_response(200, json!({"acknowledged": true}));
    }
    for ch in &changes {
        let idx = store.indices.get_mut(&ch.idx).unwrap();
        if ch.delete {
            idx.aliases.remove(&ch.alias);
        } else {
            if let Some(existing) = idx.aliases.get_mut(&ch.alias).and_then(|v| v.as_object_mut()) {
                for (k, v) in &ch.def {
                    existing.insert(k.clone(), v.clone());
                }
            } else {
                idx.aliases
                    .insert(ch.alias.clone(), Value::Object(ch.def.clone()));
            }
        }
    }
    for name in &remove_indexes {
        store.indices.remove(name);
    }
    store.bump_state();
    json_response(200, json!({"acknowledged": true}))
}
