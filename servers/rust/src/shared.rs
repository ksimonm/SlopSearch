//! Cross-cutting helpers shared by all handler modules: name validation, index
//! spec resolution, blocks, settings views. Mirrors the helpers in the Go
//! server's admin.go / model.go / server.go.

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::errors::ApiError;
use crate::resp::{format_number, glob_match};
use crate::store::{
    find_setting, settings_value_int, settings_value_string, stringify_scalar, DataStreamInfo,
    Index, Store, VERSION_CREATED,
};
use crate::Req;

// ---------------------------------------------------------------- name rules

pub const INVALID_NAME_CHARS: &str = " ,\",*,\\,<,|,,,>,/,?";

pub fn validate_index_name(name: &str) -> Result<(), ApiError> {
    validate_name(name, false)
}

pub fn validate_alias_name(name: &str) -> Result<(), ApiError> {
    validate_name(name, true)
}

pub fn validate_name(name: &str, alias: bool) -> Result<(), ApiError> {
    let (kind, kind_caps, sep) = if alias {
        ("alias", "alias", ": ")
    } else {
        ("index", "index", ", ")
    };
    let fail = |reason: &str| -> ApiError {
        let mut e = ApiError::new(
            &format!("invalid_{}_name_exception", kind),
            &format!("Invalid {} name [{}]{}{}", kind_caps, name, sep, reason),
            400,
        );
        if !alias {
            e.fields.insert("index".to_string(), Value::String(name.to_string()));
            e.fields
                .insert("index_uuid".to_string(), Value::String("_na_".to_string()));
        }
        e
    };
    if name.is_empty() {
        return Err(fail("must not be empty"));
    }
    if name == "." || name == ".." {
        return Err(fail("must not be '.' or '..'"));
    }
    if name.starts_with('_') || name.starts_with('-') || name.starts_with('+') {
        return Err(fail("must not start with '_', '-', or '+'"));
    }
    if !alias {
        for r in name.chars() {
            if r.is_uppercase() {
                return Err(fail("must be lowercase"));
            }
        }
    }
    if name.contains(':') {
        return Err(fail("must not contain ':'"));
    }
    if name.contains('#') {
        return Err(fail("must not contain '#'"));
    }
    for r in name.chars() {
        if "\\/*?\"<>| ,".contains(r) {
            return Err(fail(&format!(
                "must not contain the following characters [{}]",
                INVALID_NAME_CHARS
            )));
        }
    }
    if !alias && name.len() > 255 {
        return Err(fail(&format!(
            "index name is too long, ({} > 255)",
            name.len()
        )));
    }
    Ok(())
}

pub fn err_index_not_found_named(name: &str) -> ApiError {
    let mut e = ApiError::not_found(name);
    e.fields
        .insert("index".to_string(), Value::String(name.to_string()));
    e.fields
        .insert("index_uuid".to_string(), Value::String("_na_".to_string()));
    e.fields
        .insert("resource.type".to_string(), Value::String("index_or_alias".to_string()));
    e.fields
        .insert("resource.id".to_string(), Value::String(name.to_string()));
    e
}

pub fn sorted_index_names(store: &Store) -> Vec<String> {
    let mut names: Vec<String> = store.indices.keys().cloned().collect();
    names.sort();
    names
}

/// Mirrors Go's `(s *Server) resolveIndices(spec, resolveOpts{...})` from
/// meta.go. `missing404` returns an index_not_found_exception for explicit
/// missing names; `wildcard_closed` lets wildcards match closed indices;
/// `named_closed` is kept for parity but has no effect (Go adds explicitly
/// named indices unconditionally).
pub fn resolve_indices<'a>(
    store: &'a Store,
    spec: &str,
    missing404: bool,
    wildcard_closed: bool,
    _named_closed: bool,
) -> Result<Vec<&'a Index>, ApiError> {
    let spec = if spec.is_empty() { "*" } else { spec };
    let mut seen: Vec<String> = vec![];
    let mut add = |name: &str, seen: &mut Vec<String>| {
        if !seen.iter().any(|n| n == name) {
            seen.push(name.to_string());
        }
    };
    for tok in spec.split(',') {
        let token = tok.trim();
        if token.is_empty() {
            continue;
        }
        if token == "_all" || token == "*" {
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if idx.closed && !wildcard_closed {
                    continue;
                }
                add(&name, &mut seen);
            }
            continue;
        }
        if token.contains('*') || token.contains('?') {
            let mut matched = false;
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if glob_match(token, &idx.name) {
                    if idx.closed && !wildcard_closed {
                        continue;
                    }
                    add(&idx.name, &mut seen);
                    matched = true;
                }
            }
            if !matched {
                for name in sorted_index_names(store) {
                    let idx = &store.indices[&name];
                    if idx.closed && !wildcard_closed {
                        continue;
                    }
                    for alias in idx.aliases.keys() {
                        if glob_match(token, alias) {
                            add(&idx.name, &mut seen);
                            break;
                        }
                    }
                }
            }
            continue;
        }
        if let Some(idx) = store.indices.get(token) {
            add(&idx.name, &mut seen);
            continue;
        }
        let mut matched_alias = false;
        for name in sorted_index_names(store) {
            let idx = &store.indices[&name];
            if idx.aliases.contains_key(token) {
                add(&idx.name, &mut seen);
                matched_alias = true;
            }
        }
        if matched_alias {
            continue;
        }
        if missing404 {
            return Err(err_index_not_found_named(token));
        }
    }
    seen.sort();
    seen.dedup();
    Ok(seen.iter().map(|n| &store.indices[n]).collect())
}

// ------------------------------------------------------------ spec resolution

#[derive(Clone, Default)]
pub struct ResolveOpts2 {
    pub expand: Option<HashMap<String, bool>>,
    pub allow_no_indices: Option<bool>,
    pub ignore_unavailable: bool,
    pub reject_alias: bool,
    pub closed_error: bool,
}

pub fn default_expand() -> HashMap<String, bool> {
    let mut m = HashMap::new();
    m.insert("open".to_string(), true);
    m
}

pub fn parse_expand_wildcards(
    v: &str,
    def: Option<HashMap<String, bool>>,
) -> Result<HashMap<String, bool>, ApiError> {
    if v.is_empty() {
        return Ok(def.unwrap_or_else(default_expand));
    }
    if v == "none" {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    for p in v.split(',') {
        let p = p.trim();
        match p {
            "open" | "closed" | "hidden" => {
                out.insert(p.to_string(), true);
            }
            "all" => {
                let mut m = HashMap::new();
                m.insert("open".to_string(), true);
                m.insert("closed".to_string(), true);
                m.insert("hidden".to_string(), true);
                return Ok(m);
            }
            "" => {}
            other => {
                return Err(ApiError::bad(&format!(
                    "No valid expand wildcard value [{}]",
                    other
                )))
            }
        }
    }
    Ok(out)
}

pub fn accept_class(idx: &Index, expand: &HashMap<String, bool>) -> bool {
    if idx.closed {
        if !expand.get("closed").copied().unwrap_or(false) {
            return false;
        }
    } else if !expand.get("open").copied().unwrap_or(false) {
        return false;
    }
    if idx.is_hidden() && !expand.get("hidden").copied().unwrap_or(false) {
        return false;
    }
    true
}

pub fn err_index_closed(idx: &Index) -> ApiError {
    let mut e = ApiError::new("index_closed_exception", "closed", 400);
    e.fields
        .insert("index".to_string(), Value::String(idx.name.clone()));
    e.fields
        .insert("index_uuid".to_string(), Value::String(idx.uuid.clone()));
    e
}

pub fn resolve_data_stream<'a>(store: &'a Store, name: &str) -> Option<&'a DataStreamInfo> {
    store.data_streams.get(name)
}

pub fn data_stream_indices(store: &Store, ds: &DataStreamInfo) -> Vec<String> {
    ds.backing_indices
        .iter()
        .filter(|n| store.indices.contains_key(*n))
        .cloned()
        .collect()
}

pub fn resolve_spec2(store: &Store, spec: &str, o: &ResolveOpts2) -> Result<Vec<String>, ApiError> {
    let spec = if spec.is_empty() { "*" } else { spec };
    let empty_expand = default_expand();
    let expand = o.expand.as_ref().unwrap_or(&empty_expand);
    let allow_no = o.allow_no_indices.unwrap_or(true);
    let mut seen: Vec<String> = vec![];
    let mut add = |name: &str, seen: &mut Vec<String>| {
        if !seen.iter().any(|n| n == name) {
            seen.push(name.to_string());
        }
    };

    for tok in spec.split(',') {
        let mut token = tok.trim().to_string();
        if token.is_empty() {
            continue;
        }
        let exclude = token.starts_with('-');
        if exclude {
            token = token[1..].to_string();
        }
        let mut matched: Vec<String> = vec![];
        let mut api_err: Option<ApiError> = None;
        if token == "_all" || token == "*" {
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if accept_class(idx, expand) {
                    matched.push(name);
                }
            }
        } else if token.contains('*') || token.contains('?') {
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if glob_match(&token, &name) && accept_class(idx, expand) {
                    matched.push(name);
                }
            }
            if matched.is_empty() {
                for name in sorted_index_names(store) {
                    let idx = &store.indices[&name];
                    if !accept_class(idx, expand) {
                        continue;
                    }
                    for alias in idx.aliases.keys() {
                        if glob_match(&token, alias) {
                            matched.push(name.clone());
                            break;
                        }
                    }
                }
            }
            if matched.is_empty() && !allow_no {
                api_err = Some(err_index_not_found_named(&token));
            }
        } else if store.indices.contains_key(&token) {
            matched.push(token.clone());
        } else {
            let mut alias_hit = false;
            for name in sorted_index_names(store) {
                let idx = &store.indices[&name];
                if idx.aliases.contains_key(&token) {
                    matched.push(name);
                    alias_hit = true;
                }
            }
            if alias_hit && o.reject_alias {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!("The provided expression [{}] matches an alias, specify the corresponding concrete indices instead.", token),
                    400,
                ));
            }
            if !alias_hit {
                if let Some(ds) = resolve_data_stream(store, &token) {
                    if !o.reject_alias {
                        for name in data_stream_indices(store, ds) {
                            matched.push(name);
                        }
                    } else {
                        api_err = Some(err_index_not_found_named(&token));
                    }
                } else if o.ignore_unavailable {
                    continue;
                } else {
                    api_err = Some(err_index_not_found_named(&token));
                }
            }
        }
        if let Some(e) = api_err {
            if o.ignore_unavailable {
                continue;
            }
            return Err(e);
        }
        if o.closed_error {
            let mut open = vec![];
            for name in matched {
                let idx = &store.indices[&name];
                if idx.closed {
                    if o.ignore_unavailable {
                        continue;
                    }
                    return Err(err_index_closed(idx));
                }
                open.push(name);
            }
            matched = open;
        }
        for name in matched {
            if exclude {
                seen.retain(|n| n != &name);
            } else {
                add(&name, &mut seen);
            }
        }
    }
    seen.sort();
    Ok(seen)
}

pub fn parse_resolve_params(req: &Req, mut o: ResolveOpts2) -> ResolveOpts2 {
    if let Some(v) = req.param("expand_wildcards") {
        if let Ok(set) = parse_expand_wildcards(v, None) {
            o.expand = Some(set);
        }
    }
    if let Some(v) = req.param("allow_no_indices") {
        o.allow_no_indices = Some(v.is_empty() || v == "true");
    }
    if let Some(v) = req.param("ignore_unavailable") {
        o.ignore_unavailable = v.is_empty() || v == "true";
    }
    o
}

pub fn err_no_write_index(alias: &str) -> ApiError {
    ApiError::bad(&format!(
        "no write index is defined for alias [{}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index",
        alias
    ))
}

pub fn resolve_read_target<'a>(store: &'a Store, name: &str) -> Result<&'a Index, ApiError> {
    if let Some(idx) = store.indices.get(name) {
        return Ok(idx);
    }
    let mut with_alias: Vec<&Index> = vec![];
    let mut write_idx: Option<&Index> = None;
    let mut names = sorted_index_names(store);
    names.sort();
    for n in names {
        let idx = &store.indices[&n];
        if let Some(def) = idx.aliases.get(name) {
            with_alias.push(idx);
            if def.get("is_write_index").and_then(|v| v.as_bool()).unwrap_or(false) {
                write_idx = Some(idx);
            }
        }
    }
    if let Some(w) = write_idx {
        return Ok(w);
    }
    if let Some(first) = with_alias.first() {
        return Ok(first);
    }
    if let Some(ds) = resolve_data_stream(store, name) {
        let indices = data_stream_indices(store, ds);
        if let Some(last) = indices.last() {
            return Ok(&store.indices[last]);
        }
    }
    validate_index_name(name)?;
    Err(err_index_not_found_named(name))
}

pub fn resolve_write_target<'a>(
    store: &'a mut Store,
    name: &str,
) -> Result<&'a Index, ApiError> {
    if store.indices.contains_key(name) {
        // Find names first to satisfy the borrow checker, then return.
        return Ok(store.indices.get(name).unwrap());
    }
    let mut with_alias: Vec<String> = vec![];
    let mut write_idx: Option<String> = None;
    for n in sorted_index_names(store) {
        let idx = &store.indices[&n];
        if let Some(def) = idx.aliases.get(name) {
            with_alias.push(n.clone());
            if def.get("is_write_index").and_then(|v| v.as_bool()).unwrap_or(false) {
                write_idx = Some(n);
            }
        }
    }
    if !with_alias.is_empty() {
        if let Some(w) = write_idx {
            return Ok(&store.indices[&w]);
        }
        if with_alias.len() == 1 {
            let only = &with_alias[0];
            let def = store.indices[only].aliases.get(name);
            if let Some(v) = def.and_then(|d| d.get("is_write_index")) {
                if !v.as_bool().unwrap_or(false) {
                    return Err(err_no_write_index(name));
                }
            }
            return Ok(&store.indices[only]);
        }
        return Err(err_no_write_index(name));
    }
    if let Some(idx_name) = crate::datastreams::maybe_auto_create_data_stream(store, name)? {
        return Ok(&store.indices[&idx_name]);
    }
    validate_index_name(name)?;
    Err(err_index_not_found_named(name))
}

pub fn parse_index_block_error(
    idx: &Index,
    block: &str,
    code: i32,
    status: u16,
) -> Option<ApiError> {
    if !idx.block_on(block) {
        return None;
    }
    Some(ApiError::new(
        "cluster_block_exception",
        &format!(
            "index [{}] blocked by: [FORBIDDEN/{}/index {} (api)];",
            idx.name,
            code,
            block.replace('_', "-")
        ),
        status,
    ))
}

pub fn write_block_error(idx: &Index) -> Option<ApiError> {
    if let Some(e) = parse_index_block_error(idx, "write", 8, 403) {
        return Some(e);
    }
    if let Some(e) = parse_index_block_error(idx, "read_only", 5, 403) {
        return Some(e);
    }
    if idx.block_on("read_only_allow_delete") {
        return Some(ApiError::new(
            "cluster_block_exception",
            &format!(
                "index [{}] blocked by: [TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark, index has read-only-allow-delete block];",
                idx.name
            ),
            429,
        ));
    }
    None
}

pub fn read_block_error(idx: &Index) -> Option<ApiError> {
    parse_index_block_error(idx, "read", 7, 403)
}

pub fn meta_read_block(idx: &Index) -> Option<ApiError> {
    parse_index_block_error(idx, "metadata", 9, 403)
}

// ------------------------------------------------------------- settings

pub fn is_known_index_setting(key: &str) -> bool {
    let rest = match key.strip_prefix("index.") {
        Some(r) => r,
        None => return false,
    };
    if rest.is_empty() {
        return false;
    }
    let root = match rest.find('.') {
        Some(i) => &rest[..i],
        None => rest,
    };
    matches!(
        root,
        "number_of_shards"
            | "number_of_replicas"
            | "refresh_interval"
            | "max_result_window"
            | "max_inner_result_window"
            | "max_terms_count"
            | "max_regex_length"
            | "max_ngram_diff"
            | "max_shingle_diff"
            | "max_refresh_listeners"
            | "max_docvalue_fields_search"
            | "max_script_fields"
            | "max_analyzed_offset"
            | "max_auto_expand_replicas"
            | "hidden"
            | "blocks"
            | "analysis"
            | "mapping"
            | "routing"
            | "default_pipeline"
            | "final_pipeline"
            | "auto_expand_replicas"
            | "codec"
            | "soft_deletes"
            | "translog"
            | "merge"
            | "write"
            | "sort"
            | "shard"
            | "queries"
            | "requests"
            | "store"
            | "warmer"
            | "gc_deletes"
            | "search"
            | "query"
            | "highlight"
            | "fielddata"
            | "similarity"
            | "replication"
            | "knn"
            | "plugins"
            | "engine"
            | "compound_format"
            | "number_of_routing_shards"
            | "routing_partition_size"
            | "uuid"
            | "version"
            | "provided_name"
            | "creation_date"
            | "resize"
            | "verified_before_close"
            | "data_path"
            | "load_fixed_bitset_filters_eagerly"
            | "eager_global_ordinals"
            | "priority"
            | "allocation"
            | "unassigned"
            | "recovery"
    )
}

fn time_setting_ok(val: &str) -> bool {
    let mut rest = val;
    if let Some(r) = rest.strip_prefix('-') {
        rest = r;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let unit = &rest[digits.len()..];
    matches!(unit, "" | "ms" | "s" | "m" | "h" | "d")
}

pub fn validate_setting_value(
    key: &str,
    raw: &Value,
    _creating: bool,
) -> Result<String, ApiError> {
    let rest = key.strip_prefix("index.").unwrap_or(key);
    let val = settings_value_string(raw);
    match rest {
        "number_of_shards" => {
            let n = match settings_value_int(raw) {
                Some(n) => n,
                None => {
                    let mut e = ApiError::bad(&format!(
                        "Failed to parse value [{}] for setting [index.number_of_shards]",
                        val
                    ));
                    e.caused_by = Some(Box::new(ApiError::new(
                        "number_format_exception",
                        &format!("For input string: \"{}\"", val),
                        400,
                    )));
                    return Err(e);
                }
            };
            if n < 1 {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [index.number_of_shards] must be >= 1",
                    val
                )));
            }
            if n > 1024 {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [index.number_of_shards] must be <= 1024",
                    val
                )));
            }
            Ok(n.to_string())
        }
        "number_of_replicas" => {
            let n = match settings_value_int(raw) {
                Some(n) => n,
                None => {
                    let mut e = ApiError::bad(&format!(
                        "Failed to parse value [{}] for setting [index.number_of_replicas]",
                        val
                    ));
                    e.caused_by = Some(Box::new(ApiError::new(
                        "number_format_exception",
                        &format!("For input string: \"{}\"", val),
                        400,
                    )));
                    return Err(e);
                }
            };
            if n < 0 {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [index.number_of_replicas] must be >= 0",
                    val
                )));
            }
            if n > 1023 {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [index.number_of_replicas] must be <= 1023",
                    val
                )));
            }
            Ok(n.to_string())
        }
        "refresh_interval" => {
            if !time_setting_ok(&val) {
                return Err(ApiError::bad(&format!(
                    "failed to parse setting [index.refresh_interval] with value [{}] as a time value: unit is missing or unrecognized",
                    val
                )));
            }
            Ok(val)
        }
        "max_result_window" | "max_inner_result_window" | "max_terms_count"
        | "max_regex_length" | "max_ngram_diff" | "max_shingle_diff"
        | "max_refresh_listeners" => {
            let n = match settings_value_int(raw) {
                Some(n) => n,
                None => {
                    return Err(ApiError::bad(&format!(
                        "Failed to parse value [{}] for setting [{}]",
                        val, key
                    )))
                }
            };
            if (rest == "max_result_window" || rest == "max_inner_result_window") && n < 1 {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [{}] must be >= 1",
                    val, key
                )));
            }
            Ok(n.to_string())
        }
        _ if rest == "hidden" || rest.starts_with("blocks.") => {
            if let Value::Bool(b) = raw {
                return Ok(b.to_string());
            }
            if val == "true" || val == "false" {
                return Ok(val);
            }
            Err(ApiError::bad(&format!(
                "Failed to parse value [{}] for setting [{}]",
                val, key
            )))
        }
        _ => Ok(val),
    }
}

pub fn flatten_settings(raw: &Map<String, Value>, out: &mut Map<String, Value>) {
    for (k, v) in raw {
        let key = if k != "index" && !k.starts_with("index.") {
            format!("index.{}", k)
        } else {
            k.clone()
        };
        flatten_setting_value(&key, v, out);
    }
}

fn flatten_setting_value(key: &str, v: &Value, out: &mut Map<String, Value>) {
    if let Value::Object(m) = v {
        for (k, vv) in m {
            flatten_setting_value(&format!("{}.{}", key, k), vv, out);
        }
        return;
    }
    out.insert(key.to_string(), v.clone());
}

pub fn canonical_body_settings(body: &Map<String, Value>) -> Map<String, Value> {
    let mut inner: &Map<String, Value> = body;
    if body.len() == 1 {
        if let Some(Value::Object(s)) = body.get("settings") {
            inner = s;
        }
    }
    let mut out = Map::new();
    flatten_settings(inner, &mut out);
    out
}

fn settings_set_nested(root: &mut Map<String, Value>, path: &[&str], v: Value) {
    if path.len() == 1 {
        root.insert(path[0].to_string(), v);
        return;
    }
    let entry = root
        .entry(path[0].to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    settings_set_nested(entry.as_object_mut().unwrap(), &path[1..], v);
}

pub fn settings_to_nested(flat: &Map<String, Value>) -> Value {
    let mut root = Map::new();
    let mut keys: Vec<&String> = flat.keys().collect();
    keys.sort();
    for k in keys {
        let parts: Vec<&str> = k.split('.').collect();
        settings_set_nested(&mut root, &parts, flat[k].clone());
    }
    Value::Object(root)
}

pub fn materialized_flat(idx: &Index) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in &idx.settings {
        out.insert(k.clone(), stringify_scalar(v));
    }
    if !out.contains_key("index.number_of_shards") {
        out.insert(
            "index.number_of_shards".to_string(),
            Value::String(idx.num_shards().to_string()),
        );
    }
    if !out.contains_key("index.number_of_replicas") {
        out.insert(
            "index.number_of_replicas".to_string(),
            Value::String(idx.num_replicas().to_string()),
        );
    }
    out.insert("index.uuid".to_string(), Value::String(idx.uuid.clone()));
    out.insert(
        "index.creation_date".to_string(),
        Value::String(idx.creation_date.to_string()),
    );
    out.insert(
        "index.provided_name".to_string(),
        Value::String(idx.name.clone()),
    );
    out.insert(
        "index.version.created".to_string(),
        Value::String(VERSION_CREATED.to_string()),
    );
    out.insert(
        "index.replication.type".to_string(),
        Value::String("DOCUMENT".to_string()),
    );
    if idx.closed || idx.verified {
        out.insert(
            "index.verified_before_close".to_string(),
            Value::String("true".to_string()),
        );
    }
    out
}

pub fn default_settings_flat() -> &'static Map<String, Value> {
    static DEFAULTS: OnceLock<Map<String, Value>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        let mut m = Map::new();
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        put("index.number_of_shards", Value::String("1".into()));
        put("index.number_of_replicas", Value::String("1".into()));
        put("index.auto_expand_replicas", Value::String("false".into()));
        put("index.blocks.metadata", Value::String("false".into()));
        put("index.blocks.read", Value::String("false".into()));
        put("index.blocks.read_only", Value::String("false".into()));
        put(
            "index.blocks.read_only_allow_delete",
            Value::String("false".into()),
        );
        put("index.blocks.write", Value::String("false".into()));
        put("index.compound_format", Value::String("false".into()));
        put("index.default_pipeline", Value::String("_none".into()));
        put("index.final_pipeline", Value::String("_none".into()));
        put("index.eager_global_ordinals", Value::String("false".into()));
        put("index.fielddata.cache", Value::String("node".into()));
        put("index.flush_threshold_size", Value::String("512mb".into()));
        put("index.gc_deletes", Value::String("60s".into()));
        put("index.hidden", Value::String("false".into()));
        put(
            "index.highlight.max_analyzed_offset",
            Value::String("1000000".into()),
        );
        put("index.indexing.slowlog.level", Value::String("info".into()));
        put(
            "index.load_fixed_bitset_filters_eagerly",
            Value::String("true".into()),
        );
        put("index.mapping.coerce", Value::String("true".into()));
        put("index.mapping.depth.limit", Value::String("20".into()));
        put(
            "index.mapping.field_name_length.limit",
            Value::String("9223372036854775807".into()),
        );
        put(
            "index.mapping.ignore_malformed",
            Value::String("false".into()),
        );
        put(
            "index.mapping.nested_fields.limit",
            Value::String("50".into()),
        );
        put(
            "index.mapping.nested_objects.limit",
            Value::String("10000".into()),
        );
        put(
            "index.mapping.total_fields.limit",
            Value::String("1000".into()),
        );
        put(
            "index.max_adjacency_matrix_filters",
            Value::String("100".into()),
        );
        put(
            "index.max_docvalue_fields_search",
            Value::String("100".into()),
        );
        put(
            "index.max_inner_result_window",
            Value::String("100".into()),
        );
        put("index.max_ngram_diff", Value::String("1".into()));
        put("index.max_refresh_listeners", Value::String("1000".into()));
        put("index.max_regex_length", Value::String("1000".into()));
        put("index.max_result_window", Value::String("10000".into()));
        put("index.max_script_fields", Value::String("32".into()));
        put("index.max_shingle_diff", Value::String("3".into()));
        put(
            "index.max_slices_per_scroll",
            Value::String("1024".into()),
        );
        put("index.max_terms_count", Value::String("65536".into()));
        put(
            "index.merge.policy.deletes_pct_allowed",
            Value::String("33.0".into()),
        );
        put(
            "index.merge.policy.expunge_deletes_allowed",
            Value::String("10.0".into()),
        );
        put(
            "index.merge.policy.floor_segment",
            Value::String("2mb".into()),
        );
        put(
            "index.merge.policy.max_merge_at_once",
            Value::String("10".into()),
        );
        put(
            "index.merge.policy.max_merged_segment",
            Value::String("5gb".into()),
        );
        put(
            "index.merge.policy.segments_per_tier",
            Value::String("10".into()),
        );
        put(
            "index.merge.scheduler.max_thread_count",
            Value::String("4".into()),
        );
        put("index.query.default_field", Value::Array(vec![Value::String("*".into())]));
        put(
            "index.query.parse.allow_unmapped_fields",
            Value::String("true".into()),
        );
        put("index.queries.cache.enabled", Value::String("true".into()));
        put("index.queries.cache.size", Value::String("10%".into()));
        put("index.refresh_interval", Value::String("1s".into()));
        put("index.requests.cache.enable", Value::String("true".into()));
        put(
            "index.routing.allocation.enable",
            Value::String("all".into()),
        );
        put("index.search.default_pipeline", Value::String("_none".into()));
        put("index.search.idle.after", Value::String("30s".into()));
        put("index.search.slowlog.level", Value::String("TRACE".into()));
        put("index.soft_deletes.enabled", Value::String("true".into()));
        put("index.store.preload", Value::Array(vec![]));
        put("index.store.type", Value::String("fs".into()));
        put("index.translog.durability", Value::String("REQUEST".into()));
        put(
            "index.translog.flush_threshold_size",
            Value::String("512mb".into()),
        );
        put(
            "index.translog.retention.age",
            Value::String("12h".into()),
        );
        put(
            "index.translog.retention.size",
            Value::String("-1".into()),
        );
        put(
            "index.translog.sync_interval",
            Value::String("5s".into()),
        );
        put("index.warmer.enabled", Value::String("true".into()));
        put(
            "index.write.wait_for_active_shards",
            Value::String("1".into()),
        );
        m
    })
}

pub fn match_any_setting_pattern(patterns: &[String], key: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    for p in patterns {
        if p.is_empty() {
            continue;
        }
        if glob_match(p, key) {
            return true;
        }
        if let Some(prefix) = p.strip_suffix('*') {
            if key.starts_with(prefix) {
                return true;
            }
        }
    }
    false
}

pub fn parse_settings_filter(filter: &str) -> Vec<String> {
    if filter.is_empty() {
        return vec![];
    }
    filter
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

pub fn settings_view(
    idx: &Index,
    flat: bool,
    include_defaults: bool,
    filter: &str,
) -> Option<Value> {
    let patterns = parse_settings_filter(filter);
    let flat_all = materialized_flat(idx);
    let mut matched = Map::new();
    for (k, v) in &flat_all {
        if match_any_setting_pattern(&patterns, k) {
            matched.insert(k.clone(), v.clone());
        }
    }
    let mut defaults_matched: Option<Map<String, Value>> = None;
    if include_defaults {
        let mut dm = Map::new();
        for (k, v) in default_settings_flat() {
            if flat_all.contains_key(k) {
                continue;
            }
            if match_any_setting_pattern(&patterns, k) {
                dm.insert(k.clone(), v.clone());
            }
        }
        defaults_matched = Some(dm);
    }
    if matched.is_empty()
        && defaults_matched
            .as_ref()
            .map(|d| d.is_empty())
            .unwrap_or(true)
    {
        return None;
    }
    let mut out = Map::new();
    if flat {
        out.insert("settings".to_string(), Value::Object(matched));
        if let Some(dm) = defaults_matched {
            out.insert("defaults".to_string(), Value::Object(dm));
        }
    } else {
        out.insert("settings".to_string(), settings_to_nested(&matched));
        if let Some(dm) = defaults_matched {
            out.insert("defaults".to_string(), settings_to_nested(&dm));
        }
    }
    Some(Value::Object(out))
}

pub fn merge_settings(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        if let Value::Object(m) = v {
            let entry = dst
                .entry(k.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            merge_settings(entry.as_object_mut().unwrap(), m);
        } else {
            dst.insert(k.clone(), v.clone());
        }
    }
}

pub fn normalize_alias_def(def: &Map<String, Value>) -> Map<String, Value> {
    let mut out: Map<String, Value> = def.clone();
    if let Some(r) = out.get("routing").cloned() {
        if !r.is_null() {
            if !out.contains_key("index_routing") {
                out.insert("index_routing".to_string(), r.clone());
            }
            if !out.contains_key("search_routing") {
                out.insert("search_routing".to_string(), r);
            }
            out.remove("routing");
        }
    }
    out
}

pub fn validate_mapping_full(mappings: &Value) -> Result<(), ApiError> {
    let props = match mappings.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Ok(()),
    };
    if let Err(reason) = validate_props_shared(props) {
        let mut e = ApiError::new(
            "mapper_parsing_exception",
            &format!("Failed to parse mapping [_doc]: {}", reason),
            400,
        );
        e.caused_by = Some(Box::new(ApiError::new(
            "mapper_parsing_exception",
            &reason,
            400,
        )));
        return Err(e);
    }
    Ok(())
}

fn validate_props_shared(props: &Map<String, Value>) -> Result<(), String> {
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        if let Some(t) = def.get("type").and_then(|v| v.as_str()) {
            if !crate::store::valid_type(t) {
                return Err(format!(
                    "No handler for type [{}] declared on field [{}]",
                    t, name
                ));
            }
        }
        if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
            validate_props_shared(sub)?;
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            validate_props_shared(fields)?;
        }
    }
    Ok(())
}

pub fn param_int(req: &Req, name: &str) -> Option<i64> {
    req.param(name)
        .and_then(|v| if v.is_empty() { None } else { v.parse::<i64>().ok() })
}

pub fn wildcard_match(pat: &str, s: &str) -> bool {
    glob_match(pat, s)
}

pub fn float_setting(idx: &Index, key: &str) -> Option<f64> {
    find_setting(&idx.settings, key).and_then(|v| crate::store::to_f64(v))
}

pub fn _unused(_: &dyn Fn() -> String, _: &str) -> String {
    format_number(0.0)
}
