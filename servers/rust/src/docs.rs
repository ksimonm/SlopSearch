use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, ApiError};
use crate::search::{eval_doc, precompute_doc_values};
use crate::shared::{
    parse_resolve_params, resolve_read_target, resolve_write_target, ResolveOpts2,
};
use crate::store::{analyze_with, Index, Store};
use crate::Req;

// ----------------------------------------------------------------- helpers

/// The alias `filter` configured for `alias_name` on `idx`, if any.
pub fn search_alias_filter(idx: &Index, alias_name: &str) -> Option<Value> {
    if idx.name == alias_name {
        return None;
    }
    let def = idx.aliases.get(alias_name)?;
    let f = def.get("filter")?;
    if f.is_null() {
        return None;
    }
    Some(f.clone())
}

/// The `index_routing` configured for `alias_name` on `idx`, if any.
pub fn alias_routing_for(idx: &Index, alias_name: &str) -> String {
    if idx.name == alias_name {
        return String::new();
    }
    match idx.aliases.get(alias_name) {
        Some(def) => def
            .get("index_routing")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

/// Enforces a mapping-level "dynamic":"strict" setting.
fn check_dynamic_strict(
    dynamic: Option<String>,
    props: &Map<String, Value>,
    source: &Map<String, Value>,
) -> Option<ApiError> {
    if dynamic.as_deref() != Some("strict") {
        return None;
    }
    for k in source.keys() {
        if !props.contains_key(k) {
            return Some(ApiError::new(
                "strict_dynamic_mapping_exception",
                &format!(
                    "mapping set to strict, dynamic introduction of [{}] within [_doc] is not allowed",
                    k
                ),
                400,
            ));
        }
    }
    None
}

fn object_depth_exceeded(v: &Value, depth: usize, limit: usize) -> bool {
    if depth > limit {
        return true;
    }
    match v {
        Value::Object(m) => m.values().any(|c| object_depth_exceeded(c, depth + 1, limit)),
        Value::Array(a) => a.iter().any(|c| object_depth_exceeded(c, depth + 1, limit)),
        _ => false,
    }
}

fn field_name_too_long(v: &Value, limit: usize) -> Option<String> {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                for part in k.split('.') {
                    if part.len() > limit {
                        return Some(part.to_string());
                    }
                }
                if let Some(found) = field_name_too_long(c, limit) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|c| field_name_too_long(c, limit)),
        _ => None,
    }
}

fn mixed_array_path(v: &Value, prefix: &str) -> Option<String> {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                let p = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                if let Some(found) = mixed_array_path(c, &p) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(a) => {
            let has_obj = a.iter().any(|e| e.is_object());
            let has_scalar = a.iter().any(|e| !e.is_object() && !e.is_null());
            if has_obj && has_scalar {
                return Some(prefix.to_string());
            }
            for c in a {
                if let Some(found) = mixed_array_path(c, prefix) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

pub fn resolve_index_name(
    store: &mut Store,
    name: &str,
    for_write: bool,
) -> Result<String, ApiError> {
    if for_write {
        let idx = resolve_write_target(store, name)?;
        Ok(idx.name.clone())
    } else {
        let idx = resolve_read_target(store, name)?;
        Ok(idx.name.clone())
    }
}

// ------------------------------------------------------------- write targets

fn parse_index_body(req: &Req) -> Result<Map<String, Value>, ApiError> {
    let mut raw = String::from_utf8_lossy(&req.body).to_string();
    if raw.trim().is_empty() {
        if let Some(src) = req.param("source") {
            raw = src.to_string();
        } else {
            return Err(ApiError::new("parse_exception", "request body is required", 400));
        }
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(m)) => {
            let mut v = Value::Object(m);
            crate::fix_exponent_literals(&raw, &mut v);
            match v {
                Value::Object(m) => Ok(m),
                _ => Ok(Map::new()),
            }
        }
        Ok(_) => Err(ApiError::new(
            "mapper_parsing_exception",
            "failed to parse",
            400,
        )
        .caused_by(ApiError::new("not_x_content_exception", "Compressor detection can only be called on some xcontent bytes or compressed xcontent bytes", 400))),
        Err(e) => {
            let cause = if e.is_eof() {
                ApiError::new("unexpected_end_of_input_exception", "Unexpected end-of-input", 400)
            } else {
                ApiError::new("illegal_argument_exception", &e.to_string(), 400)
            };
            Err(ApiError::new("mapper_parsing_exception", "failed to parse", 400).caused_by(cause))
        }
    }
}

fn percolator_query_fields(q: &Map<String, Value>, out: &mut Vec<String>) {
    for (k, v) in q {
        match k.as_str() {
            "bool" => {
                if let Some(bm) = v.as_object() {
                    for key in ["must", "should", "filter", "must_not"] {
                        if let Some(cl) = bm.get(key) {
                            let items: Vec<&Value> = match cl {
                                Value::Array(a) => a.iter().collect(),
                                other => vec![other],
                            };
                            for item in items {
                                if let Some(sub) = item.as_object() {
                                    percolator_query_fields(sub, out);
                                }
                            }
                        }
                    }
                }
            }
            "term" | "terms" | "range" | "match" | "match_phrase" | "prefix"
            | "wildcard" | "regexp" | "fuzzy" | "exists" | "terms_set" | "match_bool_prefix"
            | "match_phrase_prefix" | "query_string" | "simple_query_string" => {
                if let Some(bm) = v.as_object() {
                    for key in bm.keys() {
                        if key != "boost" && key != "_name" && key != "case_insensitive" {
                            out.push(key.clone());
                            break;
                        }
                    }
                }
            }
            "nested" => {
                if let Some(bm) = v.as_object() {
                    if let Some(p) = bm.get("path").and_then(|x| x.as_str()) {
                        out.push(p.to_string());
                    }
                    if let Some(inner) = bm.get("query").and_then(|x| x.as_object()) {
                        percolator_query_fields(inner, out);
                    }
                }
            }
            "constant_score" => {
                if let Some(bm) = v.as_object() {
                    if let Some(f) = bm.get("filter").and_then(|x| x.as_object()) {
                        percolator_query_fields(f, out);
                    }
                }
            }
            "function_score" => {
                if let Some(bm) = v.as_object() {
                    if let Some(f) = bm.get("query").and_then(|x| x.as_object()) {
                        percolator_query_fields(f, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_percolator_fields(idx: &Index, q: &Map<String, Value>) -> Result<(), ApiError> {
    if let Err(e) = crate::eval::validate_query(idx, q) {
        return Err(ApiError::new(
            "mapper_parsing_exception",
            "failed to parse",
            400,
        )
        .caused_by(ApiError::new(&e.typ, &e.reason, 400)));
    }
    let mut fields = Vec::new();
    percolator_query_fields(q, &mut fields);
    for f in fields {
        if crate::eval::mapping_field_def(idx, &f).is_none() {
            return Err(ApiError::new(
                "mapper_parsing_exception",
                "failed to parse",
                400,
            )
            .caused_by(ApiError::new(
                "query_shard_exception",
                &format!("No field mapping can be found for the field with name [{}]", f),
                400,
            )));
        }
    }
    Ok(())
}

fn bad_param(req: &Req) -> Option<Response> {
    const ALLOWED: &[&str] = &[
        "refresh", "timeout", "wait_for_active_shards", "version", "version_type",
        "if_seq_no", "if_primary_term", "op_type", "routing", "pipeline", "require_alias",
        "error_trace", "pretty", "human", "filter_path", "_source", "_source_includes",
        "_source_excludes", "stored_fields", "realtime", "preference", "source", "index",
        "type", "q", "conflicts", "slices", "max_docs", "scroll_size", "wait_for_completion",
        "requests_per_second", "retry_on_conflict", "analyze_wildcard", "default_operator", "df", "lenient",
        "allow_partial_search_results", "expand_wildcards", "ignore_unavailable", "allow_no_indices",
    ];
    for (k, _) in &req.query {
        if !ALLOWED.contains(&k.as_str()) {
            return Some(err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("request [{}] contains unrecognized parameter: [{}]", req.path, k),
                400,
            )));
        }
    }
    None
}

fn check_refresh(req: &Req) -> Option<Response> {
    if let Some(v) = req.param("refresh") {
        if !v.is_empty() && v != "true" && v != "false" && v != "wait_for" {
            return Some(err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("Unknown value for refresh: [{}].", v),
                400,
            )));
        }
    }
    None
}

fn version_plan(req: &Req, current: i64) -> Result<i64, ApiError> {
    version_plan_vals(
        req.param("version_type").unwrap_or("internal"),
        req.param("version").and_then(|v| v.parse::<i64>().ok()),
        current,
    )
}

fn version_plan_vals(vt: &str, vp: Option<i64>, current: i64) -> Result<i64, ApiError> {
    match vt {
        "internal" | "" => {
            if vp.is_some() {
                return Err(ApiError::new(
                    "action_request_validation_exception",
                    "Validation Failed: 1: internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead;",
                    400,
                ));
            }
            Ok(current.max(0) + 1)
        }
        "external" | "external_gte" => {
            let Some(v) = vp else {
                return Err(ApiError::new(
                    "action_request_validation_exception",
                    "Validation Failed: 1: illegal version value [-1] for version type [external];",
                    400,
                ));
            };
            if v < 0 {
                return Err(ApiError::new(
                    "action_request_validation_exception",
                    &format!("Validation Failed: 1: illegal version value [{}] for version type [{}];", v, vt),
                    400,
                ));
            }
            let cur = current.max(-1);
            let conflict = if vt == "external" { cur >= v } else { cur > v };
            if conflict {
                let cmp = if vt == "external" { "higher or equal to" } else { "higher than" };
                return Err(ApiError::new(
                    "version_conflict_engine_exception",
                    &format!(
                        "[:id]: version conflict, current version [{}] is {} the one provided [{}]",
                        cur, cmp, v
                    ),
                    409,
                ));
            }
            Ok(v)
        }
        other => Err(ApiError::new(
            "illegal_argument_exception",
            &format!("No version type match [{}]", other),
            400,
        )),
    }
}

fn check_if_seq(req: &Req, idx: &Index, id: &str) -> Option<ApiError> {
    let Some(seqp) = req.param("if_seq_no") else { return None; };
    let termp = req.param("if_primary_term");
    if termp.is_none() {
        return Some(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: ifSeqNo is set, but primary term is [0];",
            400,
        ));
    }
    let want: i64 = seqp.parse().unwrap_or(-1);
    let want_term: i64 = termp.unwrap().parse().unwrap_or(0);
    let cur = idx.seq_nos.get(id).copied().unwrap_or(-1);
    if cur != want || want_term != 1 {
        return Some(ApiError::new(
            "version_conflict_engine_exception",
            &format!(
                "[{}]: version conflict, required seqNo [{}], primary term [{}]. current document has seqNo [{}] and primary term [1]",
                id, want, want_term, cur
            ),
            409,
        ));
    }
    None
}

pub fn handle_index_doc(store: &mut Store, req: &Req, name: &str, id: &str, op_type: &str) -> Response {
    if let Some(r) = bad_param(req) { return r; }
    if let Some(r) = check_refresh(req) { return r; }
    if id.len() > 512 {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            &format!(
                "Validation Failed: 1: id [{}] is too long, must be no longer than 512 bytes but was: {};",
                id,
                id.len()
            ),
            400,
        ));
    }
    let mut target = match resolve_write_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => {
            if e.typ == "index_not_found_exception" && !name.contains('*') && !name.contains(',') {
                if crate::admin::create_index_internal(store, name, &Map::new(), false).is_err() {
                    return err_response(e);
                }
                name.to_string()
            } else {
                return err_response(e);
            }
        }
    };
    let mut body = match parse_index_body(req) {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let depth_limit = crate::store::setting_int(
        &store.indices[&target].settings,
        "mapping.depth.limit",
    )
    .unwrap_or(20);
    if object_depth_exceeded(&Value::Object(body.clone()), 0, depth_limit as usize) {
        let reason = format!(
            "The depth of the field has exceeded the allowed limit of [{}]. This limit can be set by changing the [index.mapping.depth.limit] index level setting.",
            depth_limit
        );
        let mut e = ApiError::new("mapper_parsing_exception", "failed to parse", 400)
            .caused_by(ApiError::new("parse_exception", &reason, 400));
        e.root_cause = Some(Box::new(ApiError::new("parse_exception", &reason, 400)));
        return err_response(e);
    }
    if let Some(field) = mixed_array_path(&Value::Object(body.clone()), "") {
        let disabled = store.indices[&target]
            .mappings
            .get("properties")
            .and_then(|p| p.as_object())
            .and_then(|p| p.get(&field))
            .and_then(|d| d.get("enabled"))
            .and_then(|v| v.as_bool())
            == Some(false);
        if !disabled {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("can't merge a non object mapping [{}] with an object mapping", field),
                400,
            ));
        }
    }
    {
        let idx = &store.indices[&target];
        let rp = req.param("routing").unwrap_or("");
        if routing_required(idx) && rp.is_empty() {
            if id.is_empty() {
                return err_response(ApiError::new(
                    "null_pointer_exception",
                    "Cannot invoke \"String.length()\" because \"id\" is null",
                    500,
                ));
            }
            return err_response(ApiError::new(
                "routing_missing_exception",
                &format!("routing is required for [{}]/[{}]", target, id),
                400,
            ));
        }
        if idx.closed {
            return err_response(crate::shared::err_index_closed(idx));
        }
        if let Some(e) = crate::shared::write_block_error(idx) {
            return err_response(e);
        }
        if let Some(w) = req.param("wait_for_active_shards") {
            if let Ok(n) = w.parse::<i64>() {
                if n > idx.num_shards() {
                    return err_response(ApiError::new(
                        "unavailable_shards_exception",
                        &format!("primary shard is not active Timeout: [{}], request: [shards [{}/{}]]", req.param("timeout").unwrap_or("1m"), n, idx.num_shards()),
                        503,
                    ));
                }
            }
        }
    }
    let pipeline_param = req.param("pipeline").unwrap_or("").to_string();
    let effective = match crate::ingest::effective_ingest_pipeline(store, &target, &pipeline_param) {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    let mut pmeta = Map::new();
    pmeta.insert("_index".to_string(), json!(target));
    if !id.is_empty() {
        pmeta.insert("_id".to_string(), json!(id));
    }
    if let Some(r) = req.param("routing") {
        if !r.is_empty() {
            pmeta.insert("_routing".to_string(), json!(r));
        }
    }
    let mut pipeline_routing = String::new();
    let mut pipeline_id = String::new();
    let mut pipeline_applied = false;
    if let Some(pname) = effective {
        pipeline_applied = true;
        match crate::ingest::run_ingest_pipeline(store, &pname, body.clone(), pmeta.clone()) {
            Ok(out) => {
                if out.dropped {
                    let nid = out
                        .meta
                        .get("_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(id)
                        .to_string();
                    let nidx = out
                        .meta
                        .get("_index")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&target)
                        .to_string();
                    return json_response(
                        200,
                        json!({"_index": nidx, "_id": nid, "result": "noop", "_version": -3,
                            "_shards": {"total": 0, "successful": 0, "failed": 0}}),
                    );
                }
                body = out.source;
                pmeta = out.meta;
            }
            Err(e) => return err_response(e),
        }
    }
    if let Some(idxname) = pmeta.get("_index").and_then(|v| v.as_str()) {
        if idxname != target {
            target = idxname.to_string();
        }
    }
    if let Some(nid) = pmeta.get("_id").and_then(|v| v.as_str()) {
        pipeline_id = nid.to_string();
    }
    if let Some(r) = pmeta.get("_routing").and_then(|v| v.as_str()) {
        pipeline_routing = r.to_string();
    }
    if !store.indices.contains_key(&target) {
        let _ = crate::admin::create_index_internal(store, &target, &Map::new(), false);
    }
    if let Some(fpname) = crate::ingest::final_ingest_pipeline(store, &target) {
        pipeline_applied = true;
        let mut fmeta = pmeta.clone();
        fmeta.insert("_index".to_string(), json!(target));
        match crate::ingest::run_ingest_pipeline(store, &fpname, body.clone(), fmeta) {
            Ok(out) => {
                if out.dropped {
                    let nid = out
                        .meta
                        .get("_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(id)
                        .to_string();
                    return json_response(
                        200,
                        json!({"_index": target, "_id": nid, "result": "noop", "_version": -3,
                            "_shards": {"total": 0, "successful": 0, "failed": 0}}),
                    );
                }
                let new_index = out
                    .meta
                    .get("_index")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&target)
                    .to_string();
                if new_index != target {
                    return err_response(ApiError::new(
                        "illegal_state_exception",
                        &format!("FINAL pipeline [{}] can't change the target index", fpname),
                        500,
                    ));
                }
                body = out.source;
            }
            Err(e) => return err_response(e),
        }
    }
    let ds_field: Option<String> = {
        let idx = &store.indices[&target];
        if !idx.data_stream.is_empty() {
            if op_type != "create" && !id.is_empty() {
                return err_response(ApiError::bad(
                    "only write ops with an op_type of create are allowed in data streams",
                ));
            }
            store
                .data_streams
                .get(&idx.data_stream)
                .map(|ds| ds.timestamp_field.clone())
        } else {
            None
        }
    };
    if let Some(field) = &ds_field {
        if let Some(derr) = validate_data_stream_doc(field, &body) {
            return err_response(derr);
        }
    }
    {
        let idx = store.indices.get_mut(&target).unwrap();
        let dynamic = idx
            .mappings
            .get("dynamic")
            .and_then(|d| d.as_str())
            .map(|s| s.to_string());
        let props = crate::store::ensure_properties(&mut idx.mappings);
        if let Some(derr) = check_dynamic_strict(dynamic.clone(), props, &body) {
            return err_response(derr);
        }
        idx.ensure_field_types();
    }
    let doc_id = if !pipeline_id.is_empty() {
        pipeline_id
    } else if id.is_empty() {
        store.next_id()
    } else {
        id.to_string()
    };
    let idx = store.indices.get_mut(&target).unwrap();
    if let Some(jf) = crate::eval_extra::find_join_field(idx) {
        let is_child = body
            .get(&jf)
            .and_then(|v| v.as_object())
            .map(|o| o.contains_key("parent"))
            .unwrap_or(false);
        if is_child
            && req.param("routing").unwrap_or("").is_empty()
            && pipeline_routing.is_empty()
        {
            return err_response(
                ApiError::new("mapper_parsing_exception", "failed to parse", 400).caused_by(
                    ApiError::new(
                        "illegal_argument_exception",
                        &format!("[routing] is missing for join field [{}]", jf),
                        400,
                    ),
                ),
            );
        }
    }
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        for (fname, fdef) in props {
            if fdef.get("type").and_then(|t| t.as_str()) != Some("percolator") {
                continue;
            }
            if let Some(qv) = body.get(fname) {
                match qv.as_object() {
                    Some(q) => {
                        if let Err(e) = validate_percolator_fields(idx, q) {
                            return err_response(e);
                        }
                    }
                    None => {
                        let reason = "[_na] query malformed, must start with start_object";
                        return err_response(
                            ApiError::new("mapper_parsing_exception", "failed to parse", 400)
                                .caused_by(ApiError::new("parsing_exception", reason, 400)),
                        );
                    }
                }
            }
        }
    }
    if let Err(e) = crate::fields::compute_doc_values(idx, &doc_id, &mut body) {
        return err_response(e);
    }
    let field_limit = crate::store::setting_int(&idx.settings, "mapping.total_fields.limit")
        .unwrap_or(1000);
    if crate::shared::count_mapped_fields(&idx.mappings) as i64 > field_limit {
        return err_response(ApiError::bad(&format!(
            "Limit of total fields [{}] has been exceeded",
            field_limit
        )));
    }
    let name_limit = crate::store::setting_int(&idx.settings, "mapping.field_name_length.limit")
        .unwrap_or(512);
    if let Some(name) = field_name_too_long(&Value::Object(body.clone()), name_limit as usize) {
        return err_response(ApiError::bad(&format!(
            "Field name [{}] is longer than the limit of [{}] characters",
            name, name_limit
        )));
    }
    let nested_limit = crate::store::setting_int(&idx.settings, "mapping.nested_objects.limit")
        .unwrap_or(10000);
    if crate::shared::count_nested_instances(&idx.mappings, &Value::Object(body.clone())) as i64
        > nested_limit
    {
        return err_response(ApiError::new(
            "mapper_parsing_exception",
            &format!(
                "The number of nested documents has exceeded the allowed limit of [{}]. This limit can be set by changing the [index.mapping.nested_objects.limit] index level setting.",
                nested_limit
            ),
            400,
        ));
    }
    if op_type != "create" && op_type != "index" && !op_type.is_empty() {
        return err_response(ApiError::bad(&format!(
            "opType must be 'create' or 'index', found: [{}]",
            op_type
        )));
    }
    let existing = idx.docs.contains_key(&doc_id);
    if let Some(e) = check_if_seq(req, idx, &doc_id) {
        return err_response(e);
    }
    let old_ver = idx.versions.get(&doc_id).copied().unwrap_or(0);
    let plan_current = if existing || idx.versions.contains_key(&doc_id) { old_ver } else { -1 };
    let planned_version = match version_plan(req, plan_current) {
        Ok(v) => v,
        Err(mut e) => {
            e.reason = e.reason.replace("[:id]", &format!("[{}]", doc_id));
            return err_response(e);
        }
    };
    if op_type == "create" && existing {
        let cur = idx.versions.get(&doc_id).copied().unwrap_or(1);
        return err_response(
            ApiError::new(
                "version_conflict_engine_exception",
                &format!(
                    "[{}]: version conflict, document already exists (current version [{}])",
                    doc_id, cur
                ),
                409,
            )
            .field("index", json!(idx.name))
            .field("shard", json!("0")),
        );
    }
    if !existing {
        idx.order.push(doc_id.clone());
    }
    let seq = idx.index_total;
    idx.versions.insert(doc_id.clone(), planned_version);
    idx.seq_nos.insert(doc_id.clone(), seq);
    idx.docs.insert(doc_id.clone(), Value::Object(body.clone()));
    let raw_store = if !pipeline_applied && !req.body.is_empty() {
        req.body.clone()
    } else {
        serde_json::to_vec(&Value::Object(body.clone())).unwrap_or_default()
    };
    idx.raw_sources.insert(doc_id.clone(), raw_store);
    idx.index_total += 1;
    let mut routing = if !pipeline_routing.is_empty() {
        pipeline_routing
    } else {
        req.param("routing").unwrap_or("").to_string()
    };
    if routing.is_empty() {
        routing = alias_routing_for(idx, name);
    }
    if !routing.is_empty() {
        idx.routing.insert(doc_id.clone(), routing.clone());
    }
    if req.bool_param("refresh") || req.param("refresh") == Some("wait_for") {
        idx.pending.remove(&doc_id);
    } else {
        idx.pending
            .insert(doc_id.clone(), Value::Object(Map::new()));
    }
    let (status, result) = if existing { (200, "updated") } else { (201, "created") };
    let mut resp = Map::new();
    resp.insert("_index".to_string(), json!(idx.name));
    resp.insert("_id".to_string(), json!(doc_id));
    resp.insert("_version".to_string(), json!(planned_version));
    resp.insert("result".to_string(), json!(result));
    if req.bool_param("refresh") {
        resp.insert("forced_refresh".to_string(), json!(true));
    }
    resp.insert(
        "_shards".to_string(),
        json!({"total": idx.num_shards(), "successful": idx.num_shards(), "failed": 0}),
    );
    resp.insert("_seq_no".to_string(), json!(seq));
    resp.insert("_primary_term".to_string(), json!(1));
    if !routing.is_empty() {
        resp.insert("_routing".to_string(), json!(routing));
    }
    json_response(status, Value::Object(resp))
}

fn source_enabled(idx: &Index) -> bool {
    idx.mappings
        .get("_source")
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn get_source_spec(req: &Req) -> Option<Value> {
    let mut includes: Vec<String> = Vec::new();
    let mut excludes: Vec<String> = Vec::new();
    let has_inc = req.param("_source_includes").is_some();
    if let Some(v) = req.param("_source_includes") {
        includes = v.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
    } else if let Some(v) = req.param("_source") {
        if v == "false" {
            return None;
        }
        if v != "true" && !v.is_empty() {
            includes = v.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
        }
    }
    if let Some(v) = req.param("_source_excludes") {
        excludes = v.split(',').filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
    }
    if !has_inc && includes.is_empty() && excludes.is_empty() {
        return Some(json!({"includes": ["*"]}));
    }
    let mut m = Map::new();
    m.insert("includes".to_string(), json!(includes));
    m.insert("excludes".to_string(), json!(excludes));
    Some(Value::Object(m))
}

fn is_stored_field(idx: &Index, name: &str) -> bool {
    let mut cur = idx.mappings.get("properties");
    for seg in name.split('.') {
        let Some(node) = cur.and_then(|c| c.get(seg)) else { return false; };
        let last = seg == name.rsplit('.').next().unwrap_or("");
        if last {
            return node.get("store").and_then(|v| v.as_bool()).unwrap_or(false);
        }
        cur = node.get("properties");
    }
    false
}

fn stored_fields_json(idx: &Index, doc: &Value, spec: &str) -> Map<String, Value> {
    let patterns: Vec<String> = spec
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let mut paths = Vec::new();
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
            if def.get("store").and_then(|v| v.as_bool()) == Some(true)
                && patterns
                    .iter()
                    .any(|p| p == "*" || p == &path || crate::shared::wildcard_match(p, &path))
            {
                out.push(path.clone());
            }
            if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
                walk(sub, &path, patterns, out);
            }
        }
    }
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        walk(props, "", &patterns, &mut paths);
    }
    let mut out = Map::new();
    for name in paths {
        if let Some(v) = crate::store::field_value(doc, &name) {
            let fdef = crate::eval::mapping_field_def(idx, &name);
            let ftype = fdef
                .as_ref()
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let fmt = |x: &Value| -> Value {
                if matches!(ftype, "date" | "date_nanos") {
                    if let Some(s) = x.as_str() {
                        if let Some(d) = fdef.as_ref() {
                            if let Some(ms) = crate::fields::parse_date_any(d, s) {
                                return crate::search::format_stored_value("date", &json!(ms));
                            }
                        }
                    }
                }
                x.clone()
            };
            let vals = if v.is_array() {
                Value::Array(v.as_array().unwrap().iter().map(fmt).collect())
            } else {
                json!([fmt(v)])
            };
            out.insert(name, vals);
        }
    }
    out
}

pub fn handle_get_doc(store: &mut Store, req: &Req, name: &str, id: &str) -> Response {
    if let Some(r) = bad_param(req) { return r; }
    let target = match resolve_read_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(mut e) => {
            if e.typ == "index_not_found_exception" {
                e.fields.insert("resource.type".to_string(), json!("index_expression"));
            }
            return err_response(e);
        }
    };
    let idx = &store.indices[&target];
    if idx.closed {
        return err_response(crate::shared::err_index_closed(idx));
    }
    if let Some(e) = crate::shared::read_block_error(idx) {
        return err_response(e);
    }
    let get_routing = req.param("routing").unwrap_or("");
    if routing_required(idx) && get_routing.is_empty() {
        return err_response(ApiError::new(
            "routing_missing_exception",
            &format!("routing is required for [{}]/[{}]", idx.name, id),
            400,
        ));
    }
    let realtime = req.param("realtime").map(|v| v != "false").unwrap_or(true);
    let missing = || json_response(404, json!({"_index": idx.name, "_id": id, "found": false}));
    if idx.num_shards() > 1 {
        let stored_routing = idx.routing.get(id).map(|s| s.as_str()).unwrap_or("");
        if !get_routing.is_empty() {
            if stored_routing != get_routing {
                return missing();
            }
        } else if !stored_routing.is_empty() {
            return missing();
        }
    }
    if !realtime && idx.pending.contains_key(id) {
        return missing();
    }
    let mut doc = match idx.docs.get(id) {
        Some(d) => d.clone(),
        None => return missing(),
    };
    if let Some(m) = doc.as_object_mut() {
        m.remove("_ignored");
    }
    let version = idx.versions.get(id).copied().unwrap_or(1);
    if let Some(v) = req.param("version") {
        let want: i64 = v.parse().unwrap_or(-1);
        if want != version {
            return err_response(ApiError::new(
                "version_conflict_engine_exception",
                &format!(
                    "[{}]: version conflict, current version [{}] is different than the one provided [{}]",
                    id, version, want
                ),
                409,
            ));
        }
    }
    let mut resp = Map::new();
    resp.insert("_index".to_string(), json!(idx.name));
    resp.insert("_id".to_string(), json!(id));
    resp.insert("_version".to_string(), json!(version));
    resp.insert("_seq_no".to_string(), json!(idx.seq_nos.get(id).copied().unwrap_or(0)));
    resp.insert("_primary_term".to_string(), json!(1));
    resp.insert("found".to_string(), json!(true));
    if let Some(ign) = idx.doc_ignored.get(id) {
        let mut sorted = ign.clone();
        sorted.sort();
        resp.insert(
            "_ignored".to_string(),
            json!(sorted.iter().map(|f| json!(f)).collect::<Vec<_>>()),
        );
    }
    let stored_spec = req.param("stored_fields").unwrap_or("");
    let req_filter = req.param("_source").is_some()
        || req.param("_source_includes").is_some()
        || req.param("_source_excludes").is_some();
    let source_spec = if !stored_spec.is_empty() && req.param("_source").is_none() {
        None
    } else if req_filter {
        get_source_spec(req)
    } else {
        crate::search::mapping_source_spec(idx).or_else(|| get_source_spec(req))
    };
    let source_explicit = req.param("_source").map(|v| v == "true").unwrap_or(false);
    if source_enabled(idx) {
        if let Some(spec) = source_spec {
            if req.param("_source").map(|v| v != "false").unwrap_or(true) {
                resp.insert("_source".to_string(), crate::search::filter_source(&doc, &spec));
            }
        }
    }
    if !stored_spec.is_empty() {
        let fields = stored_fields_json(idx, &doc, stored_spec);
        if !fields.is_empty() {
            resp.insert("fields".to_string(), Value::Object(fields));
        }
    } else if source_explicit {
        // no fields
    }
    let has_filter = req.param("_source").is_some()
        || req.param("_source_includes").is_some()
        || req.param("_source_excludes").is_some()
        || crate::search::mapping_source_spec(idx).is_some();
    let has_routing = idx.routing.get(id).map(|r| !r.is_empty()).unwrap_or(false);
    if !has_filter
        && !has_routing
        && stored_spec.is_empty()
        && source_enabled(idx)
        && !idx.doc_ignored.contains_key(id)
    {
        if let Some(raw) = idx.raw_sources.get(id) {
            let prefix = format!(
                "{{\"_index\":{},\"_id\":{},\"_version\":{},\"_seq_no\":{},\"_primary_term\":1,\"found\":true,\"_source\":",
                serde_json::to_string(&idx.name).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string()),
                version,
                idx.seq_nos.get(id).copied().unwrap_or(0)
            );
            let mut body = prefix.into_bytes();
            body.extend_from_slice(raw);
            body.push(b'}');
            return raw_json_response(200, body);
        }
    }
    if let Some(r) = idx.routing.get(id) {
        if !r.is_empty() {
            resp.insert("_routing".to_string(), json!(r));
        }
    }
    json_response(200, Value::Object(resp))
}

pub fn handle_get_source(store: &mut Store, req: &Req, name: &str, id: &str) -> Response {
    if let Some(r) = bad_param(req) { return r; }
    let target = match resolve_read_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => return err_response(e),
    };
    let idx = &store.indices[&target];
    if idx.closed {
        return err_response(crate::shared::err_index_closed(idx));
    }
    if let Some(e) = crate::shared::read_block_error(idx) {
        return err_response(e);
    }
    let realtime = req.param("realtime").map(|v| v != "false").unwrap_or(true);
    if !realtime && idx.pending.contains_key(id) {
        return err_response(ApiError::new(
            "resource_not_found_exception",
            &format!("Document not found [{}]/[{}]", idx.name, id),
            404,
        ));
    }
    let source_missing = || {
        err_response(ApiError::new(
            "resource_not_found_exception",
            &format!("Source not found [{}]/[{}]", idx.name, id),
            404,
        ))
    };
    if !source_enabled(idx) {
        return source_missing();
    }
    match idx.docs.get(id) {
        Some(d) => {
            let explicit = req.param("_source").is_some()
                || req.param("_source_includes").is_some()
                || req.param("_source_excludes").is_some();
            if !explicit {
                if let Some(raw) = idx.raw_sources.get(id) {
                    return raw_json_response(200, raw.clone());
                }
            }
            match get_source_spec(req) {
                Some(spec) => {
                    let mut out = crate::search::filter_source(d, &spec);
                    if explicit {
                        normalize_filtered_numbers(&mut out);
                    }
                    json_response(200, out)
                }
                None => json_response(200, json!({})),
            }
        },
        None => err_response(ApiError::new(
            "resource_not_found_exception",
            &format!("Document not found [{}]/[{}]", idx.name, id),
            404,
        )),
    }
}

pub fn handle_delete_doc(store: &mut Store, req: &Req, name: &str, id: &str) -> Response {
    if let Some(r) = bad_param(req) { return r; }
    if let Some(r) = check_refresh(req) { return r; }
    let target = match resolve_write_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => return err_response(e),
    };
    let idx = store.indices.get_mut(&target).unwrap();
    if idx.closed {
        return err_response(crate::shared::err_index_closed(idx));
    }
    if let Some(e) = crate::shared::write_block_error(idx) {
        return err_response(e);
    }
    if routing_required(idx) && req.param("routing").unwrap_or("").is_empty() {
        return err_response(ApiError::new(
            "routing_missing_exception",
            &format!("routing is required for [{}]/[{}]", idx.name, id),
            400,
        ));
    }
    let existed = idx.docs.contains_key(id);
    if let Some(e) = check_if_seq(req, idx, id) {
        return err_response(e);
    }
    let old_ver = idx.versions.get(id).copied().unwrap_or(0);
    let planned = match version_plan(req, old_ver) {
        Ok(v) => v,
        Err(mut e) => {
            e.reason = e.reason.replace("[:id]", &format!("[{}]", id));
            return err_response(e);
        }
    };
    idx.docs.remove(id);
    idx.pending.remove(id);
    idx.routing.remove(id);
    crate::fields::remove_doc_values(idx, id);
    let seq = idx.index_total;
    idx.versions.insert(id.to_string(), planned);
    idx.seq_nos.insert(id.to_string(), seq);
    idx.index_total += 1;
    idx.delete_total += 1;
    let mut out = json!({"_index": idx.name, "_id": id, "_version": planned,
        "_shards": {"total": idx.num_shards(), "successful": idx.num_shards(), "failed": 0},
        "result": if existed { "deleted" } else { "not_found" }, "_seq_no": seq, "_primary_term": 1});
    if req.bool_param("refresh") {
        out.as_object_mut().unwrap().insert("forced_refresh".to_string(), json!(true));
    }
    json_response(if existed { 200 } else { 404 }, out)
}

fn src_path_get(m: &Map<String, Value>, path: &str) -> Value {
    let mut cur: Option<&Value> = None;
    for (i, seg) in path.split('.').enumerate() {
        let v = if i == 0 { m.get(seg) } else { cur.and_then(|c| c.get(seg)) };
        match v {
            Some(v) => cur = Some(v),
            None => return Value::Null,
        }
    }
    cur.cloned().unwrap_or(Value::Null)
}

fn src_path_set(m: &mut Map<String, Value>, path: &str, val: Value) {
    let segs: Vec<&str> = path.split('.').collect();
    if segs.len() == 1 {
        m.insert(path.to_string(), val);
        return;
    }
    let mut cur = m;
    for seg in &segs[..segs.len() - 1] {
        if !cur.get(*seg).map(|v| v.is_object()).unwrap_or(false) {
            cur.insert(seg.to_string(), Value::Object(Map::new()));
        }
        cur = cur.get_mut(*seg).unwrap().as_object_mut().unwrap();
    }
    cur.insert(segs[segs.len() - 1].to_string(), val);
}

fn src_path_remove(m: &mut Map<String, Value>, path: &str) {
    let segs: Vec<&str> = path.split('.').collect();
    if segs.len() == 1 {
        m.remove(path);
        return;
    }
    let mut cur = m;
    for seg in &segs[..segs.len() - 1] {
        let Some(next) = cur.get_mut(*seg).and_then(|v| v.as_object_mut()) else { return; };
        cur = next;
    }
    cur.remove(segs[segs.len() - 1]);
}

fn split_top(src: &str, ch: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = '\0';
    let mut depth = 0i32;
    let mut bracket = 0i32;
    for c in src.chars() {
        if quote != '\0' {
            cur.push(c);
            if c == quote { quote = '\0'; }
            continue;
        }
        match c {
            '\'' | '"' => { quote = c; cur.push(c); }
            '(' => { depth += 1; cur.push(c); }
            ')' => { depth -= 1; cur.push(c); }
            '[' => { bracket += 1; cur.push(c); }
            ']' => { bracket -= 1; cur.push(c); }
            _ if c == ch && depth == 0 && bracket == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur);
    }
    out
}

fn matching_paren(s: &str, open: usize, oc: char, cc: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote = '\0';
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        if quote != '\0' {
            if c == quote { quote = '\0'; }
            continue;
        }
        if c == '\'' || c == '"' { quote = c; continue; }
        if c == oc { depth += 1; }
        if c == cc { depth -= 1; if depth == 0 { return Some(i); } }
    }
    None
}

fn ctx_expr(
    expr: &str,
    idx: &Index,
    source: &Map<String, Value>,
    params: &Map<String, Value>,
    id: &str,
    version: i64,
) -> Result<Value, String> {
    let e = expr.trim();
    if e.starts_with('[') && e.ends_with(']') {
        let inner = &e[1..e.len() - 1];
        let mut out = Vec::new();
        if !inner.trim().is_empty() {
            for part in split_top(inner, ',') {
                out.push(ctx_expr(&part, idx, source, params, id, version)?);
            }
        }
        return Ok(Value::Array(out));
    }
    let mut p = params.clone();
    p.insert("__ctx_source".to_string(), Value::Object(source.clone()));
    p.insert("__ctx_index".to_string(), json!(idx.name));
    p.insert("__ctx_id".to_string(), json!(id));
    p.insert("__ctx_version".to_string(), json!(version));
    let v = Value::Null;
    let ctx = crate::eval::QueryCtx::new(idx, idx.name.as_str(), &v);
    let env = crate::painless::ScriptEnv { c: &ctx, params: p };
    env.eval_script_expression(e)
}

fn run_ctx_script(
    src: &str,
    idx: &Index,
    source: &mut Map<String, Value>,
    params: &Map<String, Value>,
    id: &str,
    version: i64,
    op: &mut String,
) -> Result<(), (bool, String)> {
    let statements = split_top(src, ';');
    for st in statements {
        let st = st.trim();
        if st.is_empty() { continue; }
        if st.starts_with("throw new ") {
            let msg = st
                .find('\'')
                .and_then(|a| st.rfind('\'').filter(|b| *b > a).map(|b| &st[a + 1..b]))
                .or_else(|| st.find('"').and_then(|a| st.rfind('"').filter(|b| *b > a).map(|b| &st[a + 1..b])))
                .unwrap_or("runtime error")
                .to_string();
            return Err((true, msg));
        }
        if st.starts_with("if") && st.as_bytes().get(2).map(|c| c.is_ascii_whitespace() || *c == b'(').unwrap_or(false) {
            let Some(a) = st.find('(') else { return Err((false, "compile error".into())); };
            let Some(b) = matching_paren(st, a, '(', ')') else { return Err((false, "compile error".into())); };
            let cond = &st[a + 1..b];
            let rest = st[b + 1..].trim();
            let Some(open) = rest.find('{') else { return Err((false, "compile error".into())); };
            let Some(close) = matching_paren(rest, open, '{', '}') else { return Err((false, "compile error".into())); };
            let then_block = &rest[open + 1..close];
            let condv = ctx_expr(cond, idx, source, params, id, version)
                .map_err(|_| (false, "compile error".into()))?;
            if crate::painless::truthy(&condv) {
                run_ctx_script(then_block, idx, source, params, id, version, op)?;
            } else {
                let tail = rest[close + 1..].trim();
                if let Some(else_rest) = tail.strip_prefix("else") {
                    let else_rest = else_rest.trim();
                    if let Some(eo) = else_rest.find('{') {
                        if let Some(ec) = matching_paren(else_rest, eo, '{', '}') {
                            run_ctx_script(&else_rest[eo + 1..ec], idx, source, params, id, version, op)?;
                        }
                    }
                }
            }
            continue;
        }
        if let Some(rest) = st.strip_prefix("ctx.op") {
            let rhs = rest.trim_start().trim_start_matches('=').trim();
            let val = rhs.trim_matches(|c| c == '\'' || c == '"');
            *op = val.to_string();
            continue;
        }
        if st.starts_with("ctx._id") {
            continue;
        }
        if let Some(rest) = st.strip_prefix("ctx._source.remove(") {
            let arg = rest.trim_end_matches(')').trim().trim_matches(|c| c == '\'' || c == '"');
            src_path_remove(source, arg);
            continue;
        }
        if let Some(rest) = st.strip_prefix("ctx._source.") {
            let rest = rest.trim();
            if let Some(pos) = rest.find(".add(") {
                let path = &rest[..pos];
                let args = &rest[pos + 5..];
                let arg = args.trim_end_matches(')').trim();
                let val = ctx_expr(arg, idx, source, params, id, version)
                    .map_err(|_| (false, "compile error".into()))?;
                let mut cur = src_path_get(source, path);
                if !cur.is_array() {
                    cur = Value::Array(Vec::new());
                }
                cur.as_array_mut().unwrap().push(val);
                src_path_set(source, path, cur);
                continue;
            }
            if let Some(path) = rest.strip_prefix("++").map(|_| rest) {
                let _ = path;
            }
            let (path, delta) = if let Some(p) = rest.strip_suffix("++") {
                (p.trim(), 1.0)
            } else if let Some(p) = rest.strip_suffix("--") {
                (p.trim(), -1.0)
            } else {
                ("", 0.0)
            };
            if !path.is_empty() {
                let cur = src_path_get(source, path);
                let n = crate::painless::script_float(&cur) + delta;
                src_path_set(source, path, json!(n as i64));
                continue;
            }
            if let Some(pos) = rest.find("+=") {
                let path = rest[..pos].trim();
                let rhs = rest[pos + 2..].trim();
                let val = ctx_expr(rhs, idx, source, params, id, version)
                    .map_err(|_| (false, "compile error".into()))?;
                let cur = src_path_get(source, path);
                let nv = if let Value::String(ls) = &cur {
                    let rs = crate::painless::_value_to_script_string_public(&val).unwrap_or_default();
                    Value::String(format!("{}{}", ls, rs))
                } else {
                    json!(crate::painless::script_float(&cur) + crate::painless::script_float(&val))
                };
                src_path_set(source, path, nv);
                continue;
            }
            if let Some(pos) = rest.find("-=") {
                let path = rest[..pos].trim();
                let rhs = rest[pos + 2..].trim();
                let val = ctx_expr(rhs, idx, source, params, id, version)
                    .map_err(|_| (false, "compile error".into()))?;
                let cur = src_path_get(source, path);
                src_path_set(source, path, json!(crate::painless::script_float(&cur) - crate::painless::script_float(&val)));
                continue;
            }
            if let Some(pos) = rest.find('=') {
                let path = rest[..pos].trim();
                let rhs = rest[pos + 1..].trim();
                if rhs.is_empty() {
                    return Err((false, "compile error".into()));
                }
                let val = ctx_expr(rhs, idx, source, params, id, version)
                    .map_err(|_| (false, "compile error".into()))?;
                src_path_set(source, path, val);
                continue;
            }
        }
    }
    Ok(())
}

fn deep_merge(dst: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (k, v) in patch {
        match (dst.get_mut(k), v) {
            (Some(Value::Object(d)), Value::Object(p)) => deep_merge(d, p),
            _ => { dst.insert(k.clone(), v.clone()); }
        }
    }
}

pub fn handle_update_doc(store: &mut Store, req: &Req, name: &str, id: &str) -> Response {
    if let Some(r) = bad_param(req) { return r; }
    if let Some(r) = check_refresh(req) { return r; }
    if req.param("version").is_some() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead;",
            400,
        ));
    }
    let mut target = match resolve_write_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => {
            if e.typ == "index_not_found_exception" && !name.contains('*') && !name.contains(',') {
                if crate::admin::create_index_internal(store, name, &Map::new(), false).is_err() {
                    return err_response(e);
                }
                name.to_string()
            } else {
                return err_response(e);
            }
        }
    };
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    for k in body.keys() {
        match k.as_str() {
            "script" | "doc" | "upsert" | "doc_as_upsert" | "detect_noop" | "scripted_upsert" | "_source" => {}
            _ => {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    &format!("[UpdateRequest] unknown field [{}]", k),
                    400,
                ))
            }
        }
    }
    if body.get("script").is_some() && body.get("doc").is_some() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: can't provide both script and doc;",
            400,
        ));
    }
    if body.get("script").is_none() && body.get("doc").is_none() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: script or doc is missing;",
            400,
        ));
    }
    let script_obj = body.get("script").and_then(|v| v.as_object());
    let script_src = match body.get("script") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(m)) => m.get("source").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    };
    let mut params = script_obj.map(crate::painless::script_params_of).unwrap_or_default();
    if let Some(Value::Object(bm)) = body.get("script") {
        if let Some(Value::String(lang)) = bm.get("lang") {
            if lang != "painless" {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("script_lang not supported [{}]", lang),
                    400,
                ));
            }
        }
    }
    let default_pipeline = match crate::ingest::effective_ingest_pipeline(store, &target, "") {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    let idx = match store.indices.get_mut(&target) {
        Some(i) => i,
        None => return err_response(crate::shared::err_index_not_found_named(&target)),
    };
    if idx.closed {
        return err_response(crate::shared::err_index_closed(idx));
    }
    if let Some(e) = crate::shared::write_block_error(idx) {
        return err_response(e);
    }
    if routing_required(idx) && req.param("routing").unwrap_or("").is_empty() {
        return err_response(ApiError::new(
            "routing_missing_exception",
            &format!("routing is required for [{}]/[{}]", idx.name, id),
            400,
        ));
    }
    if !source_enabled(idx) {
        return err_response(ApiError::new(
            "document_source_missing_exception",
            &format!("[{}]: document source missing", id),
            400,
        ));
    }
    if let Some(e) = check_if_seq(req, idx, id) {
        return err_response(e);
    }
    let existing_doc = idx.docs.get(id).and_then(|d| d.as_object()).cloned();
    let old_version = idx.versions.get(id).copied().unwrap_or(1);
    let mut op = String::new();
    let mut result;
    let mut status;
    let mut new_version = old_version;
    let mut new_seq = idx.seq_nos.get(id).copied().unwrap_or(0);
    let mut out_source: Option<Map<String, Value>> = None;
    let mut missing_created = false;

    if existing_doc.is_none() && body.get("upsert").is_none() && !body.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false) {
        return err_response(
            ApiError::new(
                "document_missing_exception",
                &format!("[{}]: document missing", id),
                404,
            )
            .field("index", json!(idx.name))
            .field("shard", json!("0")),
        );
    }

    if let Some(mut cur) = existing_doc.clone() {
        result = "noop";
        status = 200;
        if let Some(doc) = body.get("doc").and_then(|d| d.as_object()) {
            let mut merged = cur.clone();
            deep_merge(&mut merged, doc);
            if merged != cur || body.get("detect_noop").and_then(|v| v.as_bool()) == Some(false) {
                result = "updated";
                cur = merged;
            }
        }
        if let Some(src) = &script_src {
            let mut sc = cur.clone();
            match run_ctx_script(src, idx, &mut sc, &params, id, old_version, &mut op) {
                Ok(()) => {}
                Err((runtime, msg)) => {
                    let wrapper = ApiError::new("illegal_argument_exception", "failed to execute script", 400)
                        .caused_by(ApiError::new(
                            "script_exception",
                            if runtime { "runtime error" } else { "compile error" },
                            400,
                        ));
                    let mut wrapper = wrapper;
                    if runtime {
                        if let Some(cb) = wrapper.caused_by.as_mut() {
                            cb.caused_by = Some(Box::new(ApiError::new(
                                "illegal_argument_exception",
                                &msg,
                                400,
                            )));
                        }
                    }
                    if let Some(cb) = wrapper.caused_by.as_mut() {
                        **cb = (**cb).clone().field("lang", json!("painless"));
                    }
                    return err_response(wrapper);
                }
            }
            params.clear();
            if op == "delete" {
                result = "deleted";
            } else if op == "noop" || op == "none" || (!op.is_empty() && op != "index") {
                result = "noop";
            } else if sc != cur {
                result = "updated";
                cur = sc;
            }
        }
        if result == "deleted" {
            store.indices.get_mut(&target).unwrap().docs.remove(id);
            store.indices.get_mut(&target).unwrap().raw_sources.remove(id);
            crate::fields::remove_doc_values(store.indices.get_mut(&target).unwrap(), id);
            let idx = store.indices.get_mut(&target).unwrap();
            new_version = old_version + 1;
            new_seq = idx.index_total;
            idx.versions.insert(id.to_string(), new_version);
            idx.seq_nos.insert(id.to_string(), new_seq);
            idx.index_total += 1;
            idx.delete_total += 1;
        } else if result == "updated" {
            if let Some(pname) = &default_pipeline {
                let mut meta = Map::new();
                meta.insert("_index".to_string(), json!(target));
                meta.insert("_id".to_string(), json!(id));
                match crate::ingest::run_ingest_pipeline(store, pname, cur.clone(), meta) {
                    Ok(out) => {
                        if out.dropped {
                            return json_response(200, json!({"_index": target, "_id": id,
                                "_version": old_version, "result": "noop",
                                "_shards": {"total": 0, "successful": 0, "failed": 0},
                                "_seq_no": new_seq, "_primary_term": 1}));
                        }
                        cur = out.source;
                    }
                    Err(e) => return err_response(e),
                }
            }
            let idx = store.indices.get_mut(&target).unwrap();
            if let Err(e) = crate::fields::compute_doc_values(idx, id, &mut cur) {
                return err_response(e);
            }
            new_version = old_version + 1;
            new_seq = idx.index_total;
            idx.versions.insert(id.to_string(), new_version);
            idx.seq_nos.insert(id.to_string(), new_seq);
            idx.index_total += 1;
            idx.docs.insert(id.to_string(), Value::Object(cur.clone()));
            idx.raw_sources.insert(id.to_string(), serde_json::to_vec(&Value::Object(cur.clone())).unwrap_or_default());
            if req.bool_param("refresh") || req.param("refresh") == Some("wait_for") {
                idx.pending.remove(id);
            } else {
                idx.pending.insert(id.to_string(), Value::Object(cur.clone()));
            }
            out_source = Some(cur);
        } else {
            out_source = Some(cur);
        }
    } else {
        result = "created";
        status = 201;
        missing_created = true;
        let mut base = if body.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false) {
            body.get("doc").and_then(|d| d.as_object()).cloned().unwrap_or_default()
        } else {
            body.get("upsert").and_then(|d| d.as_object()).cloned().unwrap_or_default()
        };
        let scripted_upsert = body.get("scripted_upsert").and_then(|v| v.as_bool()).unwrap_or(false);
        if scripted_upsert {
            if let Some(src) = &script_src {
                match run_ctx_script(src, idx, &mut base, &params, id, 0, &mut op) {
                    Ok(()) => {}
                    Err((runtime, msg)) => {
                        let mut wrapper = ApiError::new("illegal_argument_exception", "failed to execute script", 400)
                            .caused_by(ApiError::new(
                                "script_exception",
                                if runtime { "runtime error" } else { "compile error" },
                                400,
                            ));
                        if runtime {
                            if let Some(cb) = wrapper.caused_by.as_mut() {
                                cb.caused_by = Some(Box::new(ApiError::new(
                                    "illegal_argument_exception",
                                    &msg,
                                    400,
                                )));
                            }
                        }
                        if let Some(cb) = wrapper.caused_by.as_mut() {
                            **cb = (**cb).clone().field("lang", json!("painless"));
                        }
                        return err_response(wrapper);
                    }
                }
            }
            if op == "none" || op == "noop" {
                return json_response(200, json!({"_index": idx.name, "_id": id, "_version": -1,
                    "result": "noop", "get": {"found": false}}));
            }
        }
        let idx = store.indices.get_mut(&target).unwrap();
        if let Err(e) = crate::fields::compute_doc_values(idx, id, &mut base) {
            return err_response(e);
        }
        idx.order.push(id.to_string());
        let seq = idx.index_total;
        idx.versions.insert(id.to_string(), 1);
        idx.seq_nos.insert(id.to_string(), seq);
        idx.index_total += 1;
        idx.docs.insert(id.to_string(), Value::Object(base.clone()));
        idx.raw_sources.insert(id.to_string(), serde_json::to_vec(&Value::Object(base.clone())).unwrap_or_default());
        if req.bool_param("refresh") || req.param("refresh") == Some("wait_for") {
            idx.pending.remove(id);
        } else {
            idx.pending.insert(id.to_string(), Value::Object(base.clone()));
        }
        new_version = 1;
        new_seq = seq;
        out_source = Some(base);
    }

    let mut resp = Map::new();
    resp.insert("_index".to_string(), json!(store.indices[&target].name));
    resp.insert("_id".to_string(), json!(id));
    resp.insert("_version".to_string(), json!(new_version));
    resp.insert("result".to_string(), json!(result));
    resp.insert("_shards".to_string(), json!({"total": if missing_created { 1 } else { 0 }, "successful": if missing_created { 1 } else { 0 }, "failed": 0}));
    resp.insert("_seq_no".to_string(), json!(new_seq));
    resp.insert("_primary_term".to_string(), json!(1));
    if req.bool_param("refresh") {
        resp.insert("forced_refresh".to_string(), json!(true));
    }
    let source_spec = body.get("_source").cloned().or_else(|| {
        if req.param("_source").is_some() || req.param("_source_includes").is_some() || req.param("_source_excludes").is_some() {
            get_source_spec(req)
        } else {
            None
        }
    });
    let source_false = source_spec.as_ref().and_then(|v| v.as_bool()) == Some(false)
        || req.param("_source") == Some("false");
    if let Some(spec) = source_spec {
        if !source_false {
            if let Some(srcmap) = out_source {
                let filtered = match &spec {
                    Value::Bool(true) => Value::Object(srcmap),
                    other => crate::search::filter_source(&Value::Object(srcmap), other),
                };
                resp.insert("get".to_string(), json!({
                    "_seq_no": new_seq,
                    "_primary_term": 1,
                    "found": true,
                    "_source": filtered,
                }));
            } else {
                resp.insert("get".to_string(), json!({"found": false}));
            }
        }
    }
    json_response(status, Value::Object(resp))
}

pub fn handle_mget(store: &mut Store, req: &Req, name: &str) -> Response {
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let mut docs_spec: Vec<Value> = Vec::new();
    if let Some(ids) = body.get("ids") {
        let Some(arr) = ids.as_array() else {
            return err_response(ApiError::new(
                "parsing_exception",
                "unknown key [ids] for a START_OBJECT, expected [docs] or [ids]",
                400,
            ));
        };
        for id in arr {
            docs_spec.push(json!({"_id": id}));
        }
    } else if let Some(docs) = body.get("docs") {
        let Some(arr) = docs.as_array() else {
            return err_response(ApiError::new(
                "parsing_exception",
                "unknown key [docs] for a START_OBJECT, expected [docs] or [ids]",
                400,
            ));
        };
        docs_spec = arr.clone();
    } else if body.is_empty() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: no documents to get;",
            400,
        ));
    } else {
        let key = body.keys().next().cloned().unwrap_or_default();
        return err_response(ApiError::new(
            "parsing_exception",
            &format!(
                "unknown key [{}] for a START_ARRAY, expected [docs] or [ids]",
                key
            ),
            400,
        ));
    }
    if docs_spec.is_empty() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: no documents to get;",
            400,
        ));
    }
    let path_idx = if name.is_empty() { None } else { Some(name) };
    for (i, d) in docs_spec.iter_mut().enumerate() {
        if !d.is_object() {
            return err_response(ApiError::new(
                "action_request_validation_exception",
                &format!("Validation Failed: 1: doc [{}] is not an object;", i),
                400,
            ));
        }
        if d.get("_id").is_none() {
            return err_response(ApiError::new(
                "action_request_validation_exception",
                &format!("Validation Failed: 1: id is missing for doc {};", i),
                400,
            ));
        }
        if d.get("_index").is_none() {
            if let Some(pi) = path_idx {
                d.as_object_mut().unwrap().insert("_index".to_string(), json!(pi));
            } else {
                return err_response(ApiError::new(
                    "action_request_validation_exception",
                    &format!("Validation Failed: 1: index is missing for doc {};", i),
                    400,
                ));
            }
        }
    }
    let mut out = Vec::new();
    for d in &docs_spec {
        let spec = d.as_object().unwrap();
        let idx_name = spec.get("_index").and_then(|v| v.as_str()).unwrap_or("");
        let id = spec.get("_id").and_then(|v| v.as_str()).unwrap_or("");
        let routing = spec.get("routing").and_then(|v| v.as_str()).unwrap_or("");
        let idx = match store.indices.get(idx_name) {
            Some(i) => i,
            None => {
                out.push(json!({"_index": idx_name, "_id": id,
                    "error": {"type": "index_not_found_exception",
                        "reason": format!("no such index [{}]", idx_name),
                        "index": idx_name, "resource.type": "index_expression",
                        "root_cause": [{"type": "index_not_found_exception",
                            "reason": format!("no such index [{}]", idx_name),
                            "index": idx_name, "resource.type": "index_expression"}]}}));
                continue;
            }
        };
        if routing_required(idx) && routing.is_empty() {
            out.push(json!({"_index": idx_name, "_id": id,
                "error": {"type": "routing_missing_exception",
                    "reason": format!("routing is required for [{}]/[{}]", idx_name, id),
                    "index": idx_name}}));
            continue;
        }
        if idx.num_shards() > 1 {
            let rid = idx.routing.get(id).map(|s| s.as_str()).unwrap_or("");
            if !routing.is_empty() && rid != routing {
                out.push(json!({"_index": idx_name, "_id": id, "found": false}));
                continue;
            }
            if routing.is_empty() && !rid.is_empty() {
                out.push(json!({"_index": idx_name, "_id": id, "found": false}));
                continue;
            }
        }
        if !req.param("realtime").map(|v| v != "false").unwrap_or(true)
            && idx.pending.contains_key(id)
        {
            out.push(json!({"_index": idx_name, "_id": id, "found": false}));
            continue;
        }
        let Some(doc) = idx.docs.get(id) else {
            out.push(json!({"_index": idx_name, "_id": id, "found": false}));
            continue;
        };
        let version = idx.versions.get(id).copied().unwrap_or(1);
        let mut m = Map::new();
        m.insert("_index".to_string(), json!(idx_name));
        m.insert("_id".to_string(), json!(id));
        m.insert("_version".to_string(), json!(version));
        m.insert("_seq_no".to_string(), json!(idx.seq_nos.get(id).copied().unwrap_or(0)));
        m.insert("_primary_term".to_string(), json!(1));
        m.insert("found".to_string(), json!(true));
        let stored_spec = spec
            .get("stored_fields")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","),
                _ => String::new(),
            })
            .unwrap_or_else(|| req.param("stored_fields").unwrap_or("").to_string());
        let per_source = spec.get("_source");
        let mut include_source = true;
        if let Some(ps) = per_source {
            if ps == &json!(false) {
                include_source = false;
            }
        } else if !stored_spec.is_empty() {
            include_source = false;
        }
        if include_source {
            let filtered = match per_source {
                Some(Value::Bool(true)) => Some(doc.clone()),
                Some(Value::Bool(false)) => None,
                Some(v) => Some(crate::search::filter_source(doc, v)),
                None => get_source_spec(req).map(|sp| crate::search::filter_source(doc, &sp)),
            };
            if let Some(v) = filtered {
                m.insert("_source".to_string(), v);
            }
        }
        if !stored_spec.is_empty() {
            let fields = stored_fields_json(idx, doc, &stored_spec);
            if !fields.is_empty() {
                m.insert("fields".to_string(), Value::Object(fields));
            }
        }
        let show_routing = if !routing.is_empty() {
            routing.to_string()
        } else {
            idx.routing.get(id).cloned().unwrap_or_default()
        };
        if !show_routing.is_empty() {
            m.insert("_routing".to_string(), json!(show_routing));
        }
        out.push(Value::Object(m));
    }
    json_response(200, json!({"docs": out}))
}

fn routing_required(idx: &Index) -> bool {
    idx.mappings
        .get("_routing")
        .and_then(|r| r.get("required"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ------------------------------------------------------------------- count

fn q_split_bool(q: &str, sep: &str) -> Option<Vec<String>> {
    let bytes = q.as_bytes();
    let sb = sep.as_bytes();
    let mut depth = 0i32;
    let mut quote = false;
    let mut i = 0;
    while i + sb.len() <= bytes.len() {
        let c = bytes[i] as char;
        if c == '"' {
            quote = !quote;
        } else if !quote && (c == '[' || c == '{' || c == '(') {
            depth += 1;
        } else if !quote && (c == ']' || c == '}' || c == ')') {
            depth -= 1;
        }
        if !quote && depth == 0 && &bytes[i..i + sb.len()] == sb {
            let left = q[..i].trim();
            let right = q[i + sb.len()..].trim();
            if !left.is_empty() && !right.is_empty() {
                let mut out = vec![left.to_string()];
                out.extend(q_split_bool(right, sep).unwrap_or_else(|| vec![right.to_string()]));
                return Some(out);
            }
        }
        i += 1;
    }
    None
}

fn q_typed(v: &str) -> Value {
    let t = v.trim();
    if let Ok(f) = t.parse::<f64>() {
        return json!(f);
    }
    match t {
        "true" => json!(true),
        "false" => json!(false),
        _ => json!(t),
    }
}

pub fn parse_q_param(q: &str) -> Value {
    let q = q.trim();
    if q.is_empty() {
        return json!({"match_none": {}});
    }
    if let Some(parts) = q_split_bool(q, " OR ") {
        let should: Vec<Value> = parts.iter().map(|p| parse_q_param(p)).collect();
        return json!({"bool": {"should": should, "minimum_should_match": 1}});
    }
    if let Some(parts) = q_split_bool(q, " AND ") {
        let must: Vec<Value> = parts.iter().map(|p| parse_q_param(p)).collect();
        return json!({"bool": {"must": must}});
    }
    if let Some(rest) = q.strip_prefix('-') {
        return json!({"bool": {"must_not": [parse_q_param(rest)]}});
    }
    if let Some(field) = q.strip_prefix("_exists_:") {
        return json!({"exists": {"field": field.trim()}});
    }
    if q == "*" {
        return json!({"match_all": {}});
    }
    if let Some(idx) = q.find(':') {
        if idx > 0 {
            let field = &q[..idx];
            let mut value = q[idx + 1..].trim();
            if field == "_id" {
                let vals: Vec<Value> = value
                    .split(',')
                    .filter(|v| !v.is_empty())
                    .map(|v| Value::String(v.to_string()))
                    .collect();
                return json!({"ids": {"values": vals}});
            }
            if value.starts_with('[') || value.starts_with('{') {
                let inc_lo = value.starts_with('[');
                let inc_hi = value.ends_with(']');
                let inner = value
                    .trim_start_matches(['[', '{'])
                    .trim_end_matches([']', '}']);
                if let Some((lo, hi)) = inner.split_once(" TO ") {
                    let mut r = serde_json::Map::new();
                    let lo = lo.trim();
                    let hi = hi.trim();
                    if lo != "*" {
                        r.insert(if inc_lo { "gte".into() } else { "gt".into() }, q_typed(lo));
                    }
                    if hi != "*" {
                        r.insert(if inc_hi { "lte".into() } else { "lt".into() }, q_typed(hi));
                    }
                    return json!({"range": {field: Value::Object(r)}});
                }
            }
            for (op, key) in [(">=", "gte"), (">", "gt"), ("<=", "lte"), ("<", "lt")] {
                if let Some(rest) = value.strip_prefix(op) {
                    return json!({"range": {field: {key: q_typed(rest)}}});
                }
            }
            if value.starts_with('(') {
                value = value.trim_start_matches('(').trim_end_matches(')');
                return json!({"bool": {"must": q_split_bool(value, " AND ").unwrap_or_else(|| vec![value.to_string()]).iter().map(|p| parse_q_param(&format!("{}:{}", field, p))).collect::<Vec<_>>()}});
            }
            let typed = q_typed(value);
            return json!({"term": {field: typed}});
        }
    }
    json!({"multi_match": {"query": q}})
}

pub fn handle_count(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" && req.method != "GET" {
        return crate::errors::method_not_allowed(req, &["POST", "GET"]);
    }
    let mut body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    if body.contains_key("size") {
        return err_response(ApiError::parsing("request does not support [size]"));
    }
    if req.param("q").is_some() && body.get("query").is_some() {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            &format!(
                "request [{}] contains unrecognized parameter: [q]",
                req.path
            ),
            400,
        ));
    }
    let routing_tokens: Option<std::collections::HashSet<String>> =
        req.param("routing").map(|r| {
            r.split(',')
                .filter(|x| !x.is_empty())
                .map(|x| x.to_string())
                .collect()
        });
    let o = parse_resolve_params(
        req,
        ResolveOpts2 {
            closed_error: true,
            ..Default::default()
        },
    );
    let names = match crate::shared::resolve_spec2(store, name, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let default_index = names
        .first()
        .cloned()
        .unwrap_or_else(|| name.to_string());
    if let Err(e) =
        crate::ingest::prepare_search_pipeline(store, &default_index, &mut body, req)
    {
        return err_response(e);
    }
    let mut query = body.get("query").cloned();
    if query.is_none() {
        if let Some(qp) = req.param("q") {
            if !qp.is_empty() {
                query = Some(parse_q_param(qp));
            }
        }
    }
    let mut query = query.unwrap_or_else(|| json!({"match_all": {}}));
    if req.param("q").is_some() {
        if let Some(mm) = query.get_mut("multi_match").and_then(|v| v.as_object_mut()) {
            if let Some(op) = req.param("default_operator") {
                mm.insert("default_operator".to_string(), json!(op));
            }
            if let Some(df) = req.param("df") {
                mm.insert("fields".to_string(), json!([df]));
            }
        }
    }
    let match_all = query.get("match_all").is_some();
    for n in &names {
        let idx = &store.indices[n];
        let qm = query.as_object().cloned().unwrap_or_default();
        if let Err(e) = crate::eval::validate_query(idx, &qm) {
            if e.typ == "parsing_exception" && e.reason.starts_with("unknown query") {
                return err_response(e);
            }
            let base = e
                .root_cause
                .as_ref()
                .map(|r| r.reason.clone())
                .unwrap_or_else(|| e.reason.clone());
            let reason = if base.starts_with("failed to create query") {
                base
            } else {
                format!("failed to create query: {}", base)
            };
            let mut out = ApiError::new(
                "search_phase_execution_exception",
                "all shards failed",
                400,
            );
            let cause = ApiError::new("query_shard_exception", &reason, 400);
            out.root_cause = Some(Box::new(cause.clone()));
            out.caused_by = Some(Box::new(cause));
            return err_response(out);
        }
    }
    let mut total = 0i64;
    let mut shards = 0i64;
    for n in &names {
        let idx = &store.indices[n];
        let mut q = query.clone();
        if let Some(f) = search_alias_filter(idx, name) {
            q = json!({"bool": {"must": [query, f]}});
        }
        if let Some(tokens) = &routing_tokens {
            let mut used = std::collections::HashSet::new();
            for t in tokens {
                used.insert(crate::search::routing_shard_of_pub(t, idx.num_shards()));
            }
            shards += used.len() as i64;
        } else {
            shards += idx.num_shards();
        }
        let extra = precompute_doc_values(idx);
        let qm = q.as_object().cloned().unwrap_or_default();
        for id in &idx.order {
            let doc = match idx.docs.get(id) {
                Some(d) => d,
                None => continue,
            };
            if idx.pending.contains_key(id) {
                continue;
            }
            if let Some(tokens) = &routing_tokens {
                let rid = idx.routing.get(id).map(|s| s.as_str()).unwrap_or("");
                if !rid.is_empty() {
                    let sh = crate::search::routing_shard_of_pub(rid, idx.num_shards());
                    let ok = tokens.iter().any(|t| {
                        crate::search::routing_shard_of_pub(t, idx.num_shards()) == sh
                    });
                    if !ok {
                        continue;
                    }
                }
            }
            match eval_doc(idx, id, doc, &extra, &qm) {
                Ok((matched, _)) => {
                    if matched {
                        total += 1;
                    }
                }
                Err(e) => {
                    if e.typ == "parsing_exception" && e.reason.starts_with("unknown query") {
                        return err_response(e);
                    }
                    let reason = if let (Some(a), Some(b)) =
                        (e.reason.find('['), e.reason.find(']'))
                    {
                        let inner = &e.reason[a + 1..b];
                        format!("failed to create query: For input string: \"{}\"", inner)
                    } else {
                        format!("failed to create query: {}", e.reason)
                    };
                    let mut out = ApiError::new(
                        "search_phase_execution_exception",
                        "all shards failed",
                        400,
                    );
                    let cause = ApiError::new("query_shard_exception", &reason, 400);
                    out.root_cause = Some(Box::new(cause.clone()));
                    out.caused_by = Some(Box::new(cause));
                    return err_response(out);
                }
            }
        }
    }
    let mut resp = Map::new();
    resp.insert("count".to_string(), json!(total));
    resp.insert(
        "_shards".to_string(),
        json!({"total": shards, "successful": shards, "skipped": 0, "failed": 0}),
    );
    if let Some(ta) = req.param("terminate_after") {
        if let Ok(n) = ta.parse::<i64>() {
            if n > 0 {
                let mut terminated = match_all;
                if !terminated && total > n {
                    resp.insert("count".to_string(), json!(n));
                    terminated = true;
                }
                resp.insert("terminated_early".to_string(), json!(terminated));
            }
        }
    }
    json_response(200, Value::Object(resp))
}

// -------------------------------------------------------------------- bulk

fn bulk_err(op: &str, index: &str, id: &str, status: u16, typ: &str, reason: &str) -> Value {
    let idv = if id.is_empty() { Value::Null } else { json!(id) };
    json!({op: {"_index": index, "_id": idv, "status": status,
        "error": {"type": typ, "reason": reason}}})
}

fn bulk_err_api(op: &str, index: &str, id: &str, e: &ApiError) -> Value {
    let idv = if id.is_empty() { Value::Null } else { json!(id) };
    json!({op: {"_index": index, "_id": idv, "status": e.status,
        "error": crate::errors::nested_error_value(e)}})
}

fn version_plan_meta(meta: &Option<Map<String, Value>>, current: i64) -> Result<i64, ApiError> {
    let vt = meta
        .as_ref()
        .and_then(|m| m.get("version_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("internal");
    let vp = meta
        .as_ref()
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_i64());
    version_plan_vals(vt, vp, current)
}

fn check_meta_seq(meta: &Option<Map<String, Value>>, idx: &Index, id: &str) -> Option<ApiError> {
    let Some(m) = meta else { return None; };
    let Some(seqv) = m.get("if_seq_no") else { return None; };
    let want = seqv.as_i64().unwrap_or(-1);
    let want_term = m.get("if_primary_term").and_then(|v| v.as_i64()).unwrap_or(0);
    let cur = idx.seq_nos.get(id).copied().unwrap_or(-1);
    if cur != want || want_term != 1 {
        return Some(ApiError::new(
            "version_conflict_engine_exception",
            &format!(
                "[{}]: version conflict, required seqNo [{}], primary term [{}]. current document has seqNo [{}] and primary term [1]",
                id, want, want_term, cur
            ),
            409,
        ));
    }
    None
}

pub fn handle_bulk(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" && req.method != "PUT" {
        return crate::errors::method_not_allowed(req, &["POST", "PUT"]);
    }
    let raw = String::from_utf8_lossy(&req.body).to_string();
    if raw.is_empty() {
        return err_response(ApiError::new("parse_exception", "request body is required", 400));
    }
    if !raw.ends_with('\n') {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            "The bulk request must be terminated by a newline [\\n]",
            400,
        ));
    }
    if raw.trim().is_empty() {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: no requests added;",
            400,
        ));
    }
    let lines: Vec<&str> = raw.lines().collect();
    {
        let mut vi = 0usize;
        while vi < lines.len() {
            let aline = lines[vi].trim();
            vi += 1;
            if aline.is_empty() {
                continue;
            }
            let act = match serde_json::from_str::<Value>(aline) {
                Ok(Value::Object(m)) => m,
                Ok(Value::String(_)) => {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [VALUE_STRING]", vi),
                        400,
                    ))
                }
                Ok(Value::Array(_)) => {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [START_ARRAY]", vi),
                        400,
                    ))
                }
                Ok(_) => {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [VALUE_NUMBER]", vi),
                        400,
                    ))
                }
                Err(_) => continue,
            };
            let Some((vop, vmeta)) = act.iter().next() else { continue };
            if !matches!(vop.as_str(), "create" | "delete" | "index" | "update") {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Malformed action/metadata line [{}], expected one of [create, delete, index, update] but found [{}]", vi, vop),
                    400,
                ));
            }
            if let Some(m) = vmeta.as_object() {
                let allowed: &[&str] = if vop == "update" {
                    &["_index", "_id", "routing", "retry_on_conflict", "_source", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
                } else if vop == "delete" {
                    &["_index", "_id", "routing", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
                } else {
                    &["_index", "_id", "routing", "pipeline", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
                };
                for k in m.keys() {
                    if !allowed.contains(&k.as_str()) {
                        return err_response(ApiError::new(
                            "illegal_argument_exception",
                            &format!("Action/metadata line [{}] contains an unknown parameter [{}]", vi, k),
                            400,
                        ));
                    }
                }
                if vop == "update" {
                    if let Some(sv) = m.get("_source") {
                        if !matches!(sv, Value::Bool(_) | Value::String(_)) {
                            return err_response(ApiError::new(
                                "illegal_argument_exception",
                                "expected a simple value for field [_source] but found [START_ARRAY]",
                                400,
                            ));
                        }
                    }
                }
            }
            if vop == "delete" {
                continue;
            }
            if vi >= lines.len() {
                return err_response(ApiError::new(
                    "action_request_validation_exception",
                    "Validation Failed: 1: no requests added;",
                    400,
                ));
            }
            let sline = lines[vi].trim();
            vi += 1;
            if vop == "update" {
                if let Ok(Value::Object(wrapper)) = serde_json::from_str::<Value>(sline) {
                    for k in wrapper.keys() {
                        match k.as_str() {
                            "script" | "doc" | "upsert" | "doc_as_upsert" | "detect_noop" | "scripted_upsert" | "_source" | "retry_on_conflict" => {}
                            _ => {
                                return err_response(ApiError::new(
                                    "x_content_parse_exception",
                                    &format!("[UpdateRequest] unknown field [{}]", k),
                                    400,
                                ))
                            }
                        }
                    }
                    if let Some(sv) = wrapper.get("_source") {
                        if !matches!(sv, Value::Bool(_) | Value::String(_)) {
                            return err_response(ApiError::new(
                                "illegal_argument_exception",
                                "expected a simple value for field [_source] but found [START_ARRAY]",
                                400,
                            ));
                        }
                    }
                }
            }
        }
    }
    let mut items: Vec<Value> = vec![];
    let mut has_errors = false;
    let refresh = req.bool_param("refresh");
    let mut ingest_ran = false;
    let mut i = 0;
    let mut pending_requests = 0usize;
    while i < lines.len() {
        let action_line_no = i + 1;
        let line = lines[i].trim();
        i += 1;
        if line.is_empty() {
            continue;
        }
        let action_value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [MALFORMED]", action_line_no),
                    400,
                ))
            }
        };
        let action = match action_value {
            Value::Object(m) => m,
            Value::Array(_) => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [START_ARRAY]", action_line_no),
                    400,
                ))
            }
            Value::String(_) => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [VALUE_STRING]", action_line_no),
                    400,
                ))
            }
            _ => {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Malformed action/metadata line [{}], expected START_OBJECT but found [VALUE_NUMBER]", action_line_no),
                    400,
                ))
            }
        };
        let Some((op, meta_val)) = action.iter().next() else {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("Malformed action/metadata line [{}], expected one of [create, delete, index, update] but found [null]", action_line_no),
                400,
            ));
        };
        let op = op.clone();
        if !matches!(op.as_str(), "create" | "delete" | "index" | "update") {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("Malformed action/metadata line [{}], expected one of [create, delete, index, update] but found [{}]", action_line_no, op),
                400,
            ));
        }
        let meta: Option<Map<String, Value>> = meta_val.as_object().cloned();
        if let Some(m) = &meta {
            let allowed: &[&str] = if op == "update" {
                &["_index", "_id", "routing", "retry_on_conflict", "_source", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
            } else if op == "delete" {
                &["_index", "_id", "routing", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
            } else {
                &["_index", "_id", "routing", "pipeline", "if_seq_no", "if_primary_term", "version", "version_type", "require_alias"]
            };
            for k in m.keys() {
                if !allowed.contains(&k.as_str()) {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("Action/metadata line [{}] contains an unknown parameter [{}]", action_line_no, k),
                        400,
                    ));
                }
            }
        }
        let mut idx_name = name.to_string();
        if let Some(m) = &meta {
            if let Some(mi) = m.get("_index").and_then(|v| v.as_str()) {
                if !mi.is_empty() {
                    idx_name = mi.to_string();
                }
            }
        }
        if idx_name.is_empty() {
            return err_response(ApiError::new(
                "action_request_validation_exception",
                "Validation Failed: 1: index is missing;",
                400,
            ));
        }
        let id_present = meta.as_ref().map(|m| m.contains_key("_id")).unwrap_or(false);
        let mut doc_id = meta
            .as_ref()
            .and_then(|m| m.get("_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(op.as_str(), "delete" | "update") && !id_present {
            return err_response(ApiError::new(
                "action_request_validation_exception",
                "Validation Failed: 1: id is missing;",
                400,
            ));
        }
        if id_present && doc_id.is_empty() {
            has_errors = true;
            pending_requests += 1;
            items.push(bulk_err(
                &op,
                &idx_name,
                "",
                400,
                "illegal_argument_exception",
                "if _id is specified it must not be empty",
            ));
            if !matches!(op.as_str(), "delete") {
                if i < lines.len() && !lines[i].trim().is_empty() {
                    i += 1;
                }
            }
            continue;
        }
        let require_alias = meta
            .as_ref()
            .and_then(|m| m.get("require_alias"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || req.bool_param("require_alias");
        if require_alias {
            let is_alias = store.indices.values().any(|ix| ix.aliases.contains_key(&idx_name));
            let exists = store.indices.contains_key(&idx_name);
            if !is_alias {
                has_errors = true;
                pending_requests += 1;
                items.push(bulk_err(
                    &op,
                    &idx_name,
                    &doc_id,
                    404,
                    "index_not_found_exception",
                    &format!("no such index [{}] and [require_alias] request flag is [true]", idx_name),
                ));
                if !matches!(op.as_str(), "delete") {
                    if i < lines.len() && !lines[i].trim().is_empty() {
                        i += 1;
                    }
                }
                let _ = exists;
                continue;
            }
        }
        pending_requests += 1;

        if op == "delete" {
            let target = match resolve_write_target(store, &idx_name) {
                Ok(t) => t.name.clone(),
                Err(e) => {
                    has_errors = true;
                    items.push(bulk_err_api("delete", &idx_name, &doc_id, &e));
                    continue;
                }
            };
            let idx = store.indices.get_mut(&target).unwrap();
            if idx.closed {
                has_errors = true;
                items.push(bulk_err_api(
                    "delete",
                    &idx_name,
                    &doc_id,
                    &crate::shared::err_index_closed(idx),
                ));
                continue;
            }
            if let Some(e) = crate::shared::write_block_error(idx) {
                has_errors = true;
                items.push(bulk_err_api("delete", &idx_name, &doc_id, &e));
                continue;
            }
            if let Some(e) = check_meta_seq(&meta, idx, &doc_id) {
                has_errors = true;
                items.push(bulk_err_api("delete", &idx_name, &doc_id, &e));
                continue;
            }
            let old_ver = idx.versions.get(&doc_id).copied().unwrap_or(0);
            let planned = match version_plan_meta(&meta, old_ver) {
                Ok(v) => v,
                Err(e) => {
                    has_errors = true;
                    items.push(bulk_err_api("delete", &idx_name, &doc_id, &e));
                    continue;
                }
            };
            let existed = idx.docs.remove(&doc_id).is_some();
            idx.pending.remove(&doc_id);
            idx.routing.remove(&doc_id);
            crate::fields::remove_doc_values(idx, &doc_id);
            let seq = idx.index_total;
            idx.versions.insert(doc_id.clone(), planned);
            idx.seq_nos.insert(doc_id.clone(), seq);
            idx.index_total += 1;
            idx.delete_total += 1;
            let result = if existed { "deleted" } else { "not_found" };
            items.push(json!({"delete": {"_index": idx.name, "_id": doc_id,
                "_version": planned, "result": result,
                "_shards": {"total": idx.num_shards(), "successful": idx.num_shards(), "failed": 0},
                "_seq_no": seq, "_primary_term": 1, "status": if existed { 200 } else { 404 }}}));
            continue;
        }

        let mut doc_line = String::new();
        if i < lines.len() {
            doc_line = lines[i].trim().to_string();
            i += 1;
        }
        let mut source: Option<Map<String, Value>> = None;
        if !doc_line.is_empty() {
            match serde_json::from_str::<Value>(&doc_line) {
                Ok(Value::Object(m)) => source = Some(m),
                _ => {
                    has_errors = true;
                    items.push(bulk_err(
                        &op,
                        &idx_name,
                        &doc_id,
                        400,
                        "mapper_parsing_exception",
                        "failed to parse",
                    ));
                    continue;
                }
            }
        }

        if op == "update" {
            let wrapper = source.clone().unwrap_or_default();
            let script_val = wrapper.get("script");
            let script_src = match script_val {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Object(m)) => m.get("source").and_then(|v| v.as_str()).map(|s| s.to_string()),
                _ => None,
            };
            let mut sparams = script_val
                .and_then(|v| v.as_object())
                .map(crate::painless::script_params_of)
                .unwrap_or_default();
            let target = match resolve_write_target(store, &idx_name) {
                Ok(t) => t.name.clone(),
                Err(e) => {
                    has_errors = true;
                    items.push(bulk_err_api("update", &idx_name, &doc_id, &e));
                    continue;
                }
            };
            let idx = store.indices.get_mut(&target).unwrap();
            if idx.closed {
                has_errors = true;
                items.push(bulk_err_api("update", &idx_name, &doc_id, &crate::shared::err_index_closed(idx)));
                continue;
            }
            if let Some(e) = crate::shared::write_block_error(idx) {
                has_errors = true;
                items.push(bulk_err_api("update", &idx_name, &doc_id, &e));
                continue;
            }
            if let Some(e) = check_meta_seq(&meta, idx, &doc_id) {
                has_errors = true;
                items.push(bulk_err_api("update", &idx_name, &doc_id, &e));
                continue;
            }
            let existing = idx.docs.get(&doc_id).and_then(|d| d.as_object()).cloned();
            let old_ver = idx.versions.get(&doc_id).copied().unwrap_or(0);
            let has_upsert = wrapper.get("upsert").is_some();
            let doc_as_upsert = wrapper.get("doc_as_upsert").and_then(|v| v.as_bool()).unwrap_or(false);
            if existing.is_none() && !has_upsert && !doc_as_upsert {
                has_errors = true;
                items.push(bulk_err(
                    "update",
                    &idx_name,
                    &doc_id,
                    404,
                    "document_missing_exception",
                    &format!("[{}]: document missing", doc_id),
                ));
                continue;
            }
            let mut status = 200u16;
            let mut result = "noop".to_string();
            let mut changed = false;
            let mut deleted = false;
            let mut out_source: Option<Map<String, Value>> = None;
            let mut new_doc = existing.clone().unwrap_or_default();
            if let Some(mut cur) = existing.clone() {
                let mut op = String::new();
                if let Some(doc) = wrapper.get("doc").and_then(|d| d.as_object()) {
                    let mut merged = cur.clone();
                    deep_merge(&mut merged, doc);
                    if merged != cur || wrapper.get("detect_noop").and_then(|v| v.as_bool()) == Some(false) {
                        changed = true;
                        result = "updated".to_string();
                        cur = merged;
                    }
                } else if let Some(src) = &script_src {
                    if !has_upsert {
                        if let Err((runtime, msg)) = run_ctx_script(src, idx, &mut cur, &sparams, &doc_id, old_ver, &mut op) {
                            has_errors = true;
                            items.push(json!({"update": {"_index": idx.name, "_id": doc_id, "status": 400,
                                "error": {"type": "illegal_argument_exception", "reason": if runtime { msg } else { "compile error".to_string() }}}}));
                            continue;
                        }
                        if op == "delete" {
                            deleted = true;
                            result = "deleted".to_string();
                        } else if op == "noop" || op == "none" || (!op.is_empty() && op != "index") {
                            result = "noop".to_string();
                        } else if cur != existing.clone().unwrap_or_default() {
                            changed = true;
                            result = "updated".to_string();
                        }
                    } else {
                        result = "updated".to_string();
                    }
                } else if has_upsert {
                    result = "updated".to_string();
                }
                new_doc = cur;
            } else {
                let mut base = if doc_as_upsert {
                    wrapper.get("doc").and_then(|d| d.as_object()).cloned().unwrap_or_default()
                } else {
                    wrapper.get("upsert").and_then(|d| d.as_object()).cloned().unwrap_or_default()
                };
                let mut op = String::new();
                if let Some(src) = &script_src {
                    if let Err((runtime, msg)) = run_ctx_script(src, idx, &mut base, &sparams, &doc_id, 0, &mut op) {
                        has_errors = true;
                        items.push(json!({"update": {"_index": idx.name, "_id": doc_id, "status": 400,
                            "error": {"type": "illegal_argument_exception", "reason": if runtime { msg } else { "compile error".to_string() }}}}));
                        continue;
                    }
                }
                new_doc = base;
                changed = true;
                status = 201;
                result = "created".to_string();
            }
            let mut seq = idx.seq_nos.get(&doc_id).copied().unwrap_or(0);
            let mut version = old_ver;
            if deleted {
                idx.docs.remove(&doc_id);
                idx.raw_sources.remove(&doc_id);
                crate::fields::remove_doc_values(idx, &doc_id);
                seq = idx.index_total;
                version = old_ver + 1;
                idx.versions.insert(doc_id.clone(), version);
                idx.seq_nos.insert(doc_id.clone(), seq);
                idx.index_total += 1;
                idx.delete_total += 1;
            } else if changed {
                if let Err(e) = crate::fields::compute_doc_values(idx, &doc_id, &mut new_doc) {
                    has_errors = true;
                    items.push(bulk_err_api("update", &idx.name, &doc_id, &e));
                    continue;
                }
                seq = idx.index_total;
                version = old_ver + 1;
                if existing.is_none() {
                    idx.order.push(doc_id.clone());
                }
                idx.versions.insert(doc_id.clone(), version);
                idx.seq_nos.insert(doc_id.clone(), seq);
                idx.index_total += 1;
                idx.docs.insert(doc_id.clone(), Value::Object(new_doc.clone()));
                idx.raw_sources.insert(doc_id.clone(), serde_json::to_vec(&Value::Object(new_doc.clone())).unwrap_or_default());
                if refresh {
                    idx.pending.remove(&doc_id);
                } else {
                    idx.pending.insert(doc_id.clone(), Value::Object(new_doc.clone()));
                }
                out_source = Some(new_doc.clone());
            } else {
                version = old_ver;
                out_source = Some(new_doc.clone());
            }
            let mut item = Map::new();
            item.insert("_index".to_string(), json!(idx.name));
            item.insert("_id".to_string(), json!(doc_id));
            item.insert("_version".to_string(), json!(version));
            item.insert("result".to_string(), json!(result));
            item.insert("_shards".to_string(), json!({"total": if status == 201 { 1 } else { 0 }, "successful": if status == 201 { 1 } else { 0 }, "failed": 0}));
            item.insert("_seq_no".to_string(), json!(seq));
            item.insert("_primary_term".to_string(), json!(1));
            if refresh {
                item.insert("forced_refresh".to_string(), json!(true));
            }
            item.insert("status".to_string(), json!(status));
            let mut spec = wrapper.get("_source").cloned().or_else(|| {
                meta.as_ref().and_then(|m| m.get("_source")).cloned()
            });
            if spec.is_none() && (req.param("_source").is_some() || req.param("_source_includes").is_some() || req.param("_source_excludes").is_some()) {
                spec = get_source_spec(req);
            }
            let source_false = spec.as_ref().and_then(|v| v.as_bool()) == Some(false);
            if let (Some(spec), false) = (spec, source_false) {
                if let Some(srcmap) = out_source {
                    let sv = Value::Object(srcmap);
                    let filtered = match &spec {
                        Value::Bool(true) => sv,
                        Value::String(s) => crate::search::filter_source(&sv, &json!({"includes": [s]})),
                        Value::Array(a) => crate::search::filter_source(&sv, &json!({"includes": a})),
                        other => crate::search::filter_source(&sv, other),
                    };
                    item.insert("get".to_string(), json!({
                        "_index": idx.name, "_id": doc_id, "_version": version,
                        "_seq_no": seq, "_primary_term": 1, "found": true,
                        "_source": filtered
                    }));
                }
            }
            items.push(json!({"update": item}));
            continue;
        }
        let target = match resolve_write_target(store, &idx_name) {
            Ok(t) => t.name.clone(),
            Err(e) => {
                if e.typ == "index_not_found_exception" && !idx_name.contains('*') && !idx_name.contains(',') {
                    if crate::admin::create_index_internal(store, &idx_name, &Map::new(), false).is_ok() {
                        idx_name.clone()
                    } else {
                        has_errors = true;
                        items.push(bulk_err_api(&op, &idx_name, &doc_id, &e));
                        continue;
                    }
                } else {
                    has_errors = true;
                    items.push(bulk_err_api(&op, &idx_name, &doc_id, &e));
                    continue;
                }
            }
        };
        {
            let idx = &store.indices[&target];
            if idx.closed {
                has_errors = true;
                items.push(bulk_err_api(
                    &op,
                    &idx_name,
                    &doc_id,
                    &crate::shared::err_index_closed(idx),
                ));
                continue;
            }
            if let Some(e) = crate::shared::write_block_error(idx) {
                has_errors = true;
                items.push(bulk_err_api(&op, &idx_name, &doc_id, &e));
                continue;
            }
            let has_routing = meta.as_ref().and_then(|m| m.get("routing")).and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
                || req.param("routing").map(|s| !s.is_empty()).unwrap_or(false);
            if routing_required(idx) && !has_routing {
                has_errors = true;
                items.push(json!({op.clone(): {"_index": idx_name, "_id": doc_id,
                    "status": 400, "error": {"type": "routing_missing_exception",
                        "reason": format!("routing is required for [{}]/[{}]", idx_name, doc_id)}}}));
                continue;
            }
        }
        let mut target = target;
        let action_pipeline = meta
            .as_ref()
            .and_then(|m| m.get("pipeline"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let request_pipeline = req.param("pipeline").unwrap_or("").to_string();
        let explicit = if !action_pipeline.is_empty() {
            action_pipeline.as_str()
        } else {
            request_pipeline.as_str()
        };
        let mut source = source.unwrap_or_default();
        let effective =
            match crate::ingest::effective_ingest_pipeline(store, &target, explicit) {
                Ok(p) => p,
                Err(e) => {
                    has_errors = true;
                    items.push(json!({op.clone(): {"_index": target, "_id": doc_id,
                        "status": e.status,
                        "error": {"type": e.typ, "reason": e.reason}}}));
                    continue;
                }
            };
        let mut pipeline_routing = String::new();
        let mut pipeline_id = String::new();
        if let Some(pname) = effective {
            ingest_ran = true;
            let mut pm = Map::new();
            pm.insert("_index".to_string(), json!(target));
            if !doc_id.is_empty() {
                pm.insert("_id".to_string(), json!(doc_id));
            }
            if let Some(r) = meta
                .as_ref()
                .and_then(|m| m.get("routing"))
                .and_then(|v| v.as_str())
            {
                pm.insert("_routing".to_string(), json!(r));
            }
            match crate::ingest::run_ingest_pipeline(store, &pname, source.clone(), pm) {
                Ok(out) => {
                    if out.dropped {
                        let nid = out
                            .meta
                            .get("_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| doc_id.clone());
                        let nidx = out
                            .meta
                            .get("_index")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&target)
                            .to_string();
                        items.push(json!({op.clone(): {"_index": nidx, "_id": nid,
                            "status": 200, "result": "noop"}}));
                        continue;
                    }
                    source = out.source;
                    if let Some(ni) = out.meta.get("_index").and_then(|v| v.as_str()) {
                        if ni != target {
                            target = ni.to_string();
                        }
                    }
                    if let Some(nid) = out.meta.get("_id").and_then(|v| v.as_str()) {
                        if !nid.is_empty() {
                            pipeline_id = nid.to_string();
                        }
                    }
                    if let Some(r) = out.meta.get("_routing").and_then(|v| v.as_str()) {
                        pipeline_routing = r.to_string();
                    }
                }
                Err(e) => {
                    has_errors = true;
                    items.push(json!({op.clone(): {"_index": target, "_id": doc_id,
                        "status": e.status,
                        "error": {"type": e.typ, "reason": e.reason}}}));
                    continue;
                }
            }
        }
        if !pipeline_id.is_empty() {
            doc_id = pipeline_id;
        }
        if !store.indices.contains_key(&target) {
            let _ = crate::admin::create_index_internal(store, &target, &Map::new(), false);
        }
        if let Some(fpname) = crate::ingest::final_ingest_pipeline(store, &target) {
            let mut fm = Map::new();
            fm.insert("_index".to_string(), json!(target));
            if !doc_id.is_empty() {
                fm.insert("_id".to_string(), json!(doc_id));
            }
            match crate::ingest::run_ingest_pipeline(store, &fpname, source.clone(), fm) {
                Ok(out) => {
                    if out.dropped {
                        items.push(json!({op.clone(): {"_index": target, "_id": doc_id,
                            "status": 200, "result": "noop"}}));
                        continue;
                    }
                    let ni = out
                        .meta
                        .get("_index")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&target)
                        .to_string();
                    if ni != target {
                        has_errors = true;
                        items.push(json!({op.clone(): {"_index": target, "_id": doc_id,
                            "status": 500,
                            "error": {"type": "illegal_state_exception",
                                "reason": format!("FINAL pipeline [{}] can't change the target index", fpname)}}}));
                        continue;
                    }
                    source = out.source;
                }
                Err(e) => {
                    has_errors = true;
                    items.push(json!({op.clone(): {"_index": target, "_id": doc_id,
                        "status": e.status,
                        "error": {"type": e.typ, "reason": e.reason}}}));
                    continue;
                }
            }
        }
        {
            let idx = store.indices.get_mut(&target).unwrap();
            let dynamic = idx
                .mappings
                .get("dynamic")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let props = crate::store::ensure_properties(&mut idx.mappings);
            if let Some(derr) = check_dynamic_strict(dynamic.clone(), props, &source) {
                has_errors = true;
                items.push(bulk_err_api(&op, &idx.name, &doc_id, &derr));
                continue;
            }
            idx.ensure_field_types();
        }
        if doc_id.is_empty() {
            doc_id = store.next_id();
        }
        let idx = store.indices.get_mut(&target).unwrap();
        if let Err(e) = crate::fields::compute_doc_values(idx, &doc_id, &mut source) {
            has_errors = true;
            items.push(bulk_err_api(&op, &idx.name, &doc_id, &e));
            continue;
        }
        if let Some(e) = check_meta_seq(&meta, idx, &doc_id) {
            has_errors = true;
            items.push(bulk_err_api(&op, &idx.name, &doc_id, &e));
            continue;
        }
        let exists = idx.docs.contains_key(&doc_id);
        let old_ver = idx.versions.get(&doc_id).copied().unwrap_or(0);
        let planned = match version_plan_meta(&meta, if exists || idx.versions.contains_key(&doc_id) { old_ver } else { -1 }) {
            Ok(v) => v,
            Err(e) => {
                has_errors = true;
                items.push(bulk_err_api(&op, &idx.name, &doc_id, &e));
                continue;
            }
        };
        if op == "create" && exists {
            has_errors = true;
            let cur = idx.versions.get(&doc_id).copied().unwrap_or(1);
            items.push(bulk_err(
                "create",
                &idx_name,
                &doc_id,
                409,
                "version_conflict_engine_exception",
                &format!("[{}]: version conflict, document already exists (current version [{}])", doc_id, cur),
            ));
            continue;
        }
        if !exists {
            idx.order.push(doc_id.clone());
        }
        let seq = idx.index_total;
        idx.versions.insert(doc_id.clone(), planned);
        idx.seq_nos.insert(doc_id.clone(), seq);
        idx.docs.insert(doc_id.clone(), Value::Object(source.clone()));
        idx.raw_sources.insert(doc_id.clone(), serde_json::to_vec(&Value::Object(source.clone())).unwrap_or_default());
        idx.index_total += 1;
        let mut routing = meta
            .as_ref()
            .and_then(|m| m.get("routing"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| req.param("routing").unwrap_or(""))
            .to_string();
        if !pipeline_routing.is_empty() {
            routing = pipeline_routing;
        }
        if routing.is_empty() {
            routing = alias_routing_for(idx, &idx_name);
        }
        if !routing.is_empty() {
            idx.routing.insert(doc_id.clone(), routing);
        }
        if !refresh && req.param("refresh") != Some("wait_for") {
            idx.pending
                .insert(doc_id.clone(), Value::Object(source));
        } else {
            idx.pending.remove(&doc_id);
        }
        let status = if exists { 200 } else { 201 };
        let result = if exists { "updated" } else { "created" };
        let mut item = Map::new();
        item.insert("_index".to_string(), json!(idx.name));
        item.insert("_id".to_string(), json!(doc_id));
        item.insert("_version".to_string(), json!(planned));
        item.insert("result".to_string(), json!(result));
        item.insert("_shards".to_string(), json!({"total": idx.num_shards(), "successful": idx.num_shards(), "failed": 0}));
        item.insert("_seq_no".to_string(), json!(seq));
        item.insert("_primary_term".to_string(), json!(1));
        if refresh {
            item.insert("forced_refresh".to_string(), json!(true));
        }
        item.insert("status".to_string(), json!(status));
        items.push(json!({op: item}));
    }
    if pending_requests == 0 {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: no requests added;",
            400,
        ));
    }
    if refresh || req.param("refresh") == Some("wait_for") {
        for idx in store.indices.values_mut() {
            idx.pending.clear();
        }
    }
    let mut resp = json!({"took": 1, "errors": has_errors, "items": items});
    if ingest_ran {
        if let Some(m) = resp.as_object_mut() {
            m.insert("ingest_took".to_string(), json!(0));
        }
    }
    json_response(200, resp)
}

// -------------------------------------------------------------- termvectors

#[derive(Clone)]
struct TvToken {
    term: String,
    pos: i64,
    start: i64,
    end: i64,
}

fn tv_tokens(analyzer: &str, text: &str) -> Vec<TvToken> {
    let a = analyzer.to_ascii_lowercase();
    if a.contains("keyword") {
        if text.is_empty() {
            return vec![];
        }
        return vec![TvToken { term: text.to_string(), pos: 0, start: 0, end: text.len() as i64 }];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() || c == '_' {
            if cur.is_empty() {
                start = i;
            }
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(TvToken {
                term: cur.to_lowercase(),
                pos: out.len() as i64,
                start: start as i64,
                end: i as i64,
            });
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(TvToken {
            term: cur.to_lowercase(),
            pos: out.len() as i64,
            start: start as i64,
            end: text.len() as i64,
        });
    }
    out
}

fn tv_field_analyzer(idx: &Index, field: &str, per_field: Option<&Map<String, Value>>) -> String {
    if let Some(pf) = per_field {
        if let Some(a) = pf.get(field).and_then(|v| v.as_str()) {
            return a.to_string();
        }
    }
    if let Some(ft) = idx.field_types.get(field) {
        if ft == "keyword" {
            return "keyword".to_string();
        }
    }
    let props = idx.mappings.get("properties").and_then(|v| v.as_object());
    if let Some(m) = props.and_then(|p| p.get(field)).and_then(|v| v.as_object()) {
        if m.get("type").and_then(|v| v.as_str()) == Some("keyword") {
            return "keyword".to_string();
        }
        if let Some(a) = m.get("analyzer").and_then(|v| v.as_str()) {
            return a.to_string();
        }
    }
    "standard".to_string()
}

fn tv_doc_text(doc: &Value, field: &str) -> Option<String> {
    crate::store::field_value(doc, field).map(|v| match v {
        Value::String(s) => s.clone(),
        other => match other {
            Value::Array(a) => a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" "),
            _ => other.to_string(),
        },
    })
}

fn tv_index_stats_for_field(idx: &Index, field: &str) -> (Map<String, Value>, std::collections::HashMap<String, i64>, std::collections::HashMap<String, i64>) {
    let mut terms_df: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut terms_ttf: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut doc_count = 0i64;
    let mut sum_ttf = 0i64;
    for id in &idx.order {
        let Some(doc) = idx.docs.get(id) else { continue };
        if let Some(text) = tv_doc_text(doc, field) {
            doc_count += 1;
            let an = tv_field_analyzer(idx, field, None);
            let toks = tv_tokens(&an, &text);
            sum_ttf += toks.len() as i64;
            let mut seen: std::collections::HashSet<String> = Default::default();
            for t in toks {
                if seen.insert(t.term.clone()) {
                    *terms_df.entry(t.term.clone()).or_insert(0) += 1;
                }
                *terms_ttf.entry(t.term.clone()).or_insert(0) += 1;
            }
        }
    }
    let sum_doc_freq: i64 = terms_df.values().sum();
    let mut stats = Map::new();
    stats.insert("sum_doc_freq".to_string(), json!(sum_doc_freq));
    stats.insert("doc_count".to_string(), json!(doc_count));
    stats.insert("sum_ttf".to_string(), json!(sum_ttf));
    (stats, terms_df, terms_ttf)
}

fn idx_mapping_fields(idx: &Index) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(props) = idx.mappings.get("properties").and_then(|v| v.as_object()) {
        for (f, m) in props {
            if let Some(o) = m.as_object() {
                if let Some(t) = o.get("type").and_then(|v| v.as_str()) {
                    out.push((f.clone(), t.to_string()));
                }
            }
        }
    }
    out
}

fn tv_term_value(
    term: &str,
    tokens: &[TvToken],
    term_stats: bool,
    positions: bool,
    offsets: bool,
    df: &std::collections::HashMap<String, i64>,
    ttf: &std::collections::HashMap<String, i64>,
) -> Value {
    let mut m = Map::new();
    m.insert("term_freq".to_string(), json!(tokens.len()));
    if term_stats {
        if let Some(d) = df.get(term) {
            m.insert("doc_freq".to_string(), json!(d));
        }
        if let Some(t) = ttf.get(term) {
            m.insert("ttf".to_string(), json!(t));
        }
    }
    if positions || offsets {
        let toks: Vec<Value> = tokens
            .iter()
            .map(|t| {
                let mut tm = Map::new();
                if positions {
                    tm.insert("position".to_string(), json!(t.pos));
                }
                if offsets {
                    tm.insert("start_offset".to_string(), json!(t.start));
                    tm.insert("end_offset".to_string(), json!(t.end));
                }
                Value::Object(tm)
            })
            .collect();
        m.insert("tokens".to_string(), Value::Array(toks));
    }
    Value::Object(m)
}

fn termvectors_one(
    store: &Store,
    index_name: &str,
    id: Option<&str>,
    body: &Map<String, Value>,
    q: &std::collections::HashMap<String, String>,
) -> Result<Value, ApiError> {
    let idx = store
        .indices
        .get(index_name)
        .ok_or_else(|| crate::shared::err_index_not_found_named(index_name))?;
    let existing = id.and_then(|i| idx.docs.get(i).cloned());
    if let Some(i) = id {
        if existing.is_none() {
            return Ok(json!({"_index": idx.name, "_id": i, "found": false}));
        }
    }
    let doc = if let Some(d) = existing {
        d
    } else {
        body.get("doc").cloned().unwrap_or_else(|| json!({}))
    };
    let mut field_list: Vec<String> = Vec::new();
    let mut add_fields = |v: &Value| {
        match v {
            Value::String(s) => {
                for f in s.split(',') {
                    if !f.is_empty() {
                        field_list.push(f.to_string());
                    }
                }
            }
            Value::Array(a) => {
                for f in a {
                    if let Some(s) = f.as_str() {
                        field_list.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    };
    if let Some(f) = body.get("fields") {
        add_fields(f);
    }
    if let Some(f) = q.get("fields") {
        add_fields(&json!(f));
    }
    if field_list.is_empty() {
        let mut all: Vec<String> = idx
            .mappings
            .get("properties")
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        all.sort();
        field_list = all;
    }
    let pf = body.get("per_field_analyzer").and_then(|v| v.as_object());
    let bool_param = |name: &str, default: bool| -> bool {
        if let Some(v) = body.get(name).and_then(|v| v.as_bool()) {
            return v;
        }
        if let Some(v) = q.get(name) {
            return v == "true";
        }
        default
    };
    let term_stats = bool_param("term_statistics", false);
    let field_stats = bool_param("field_statistics", true);
    let positions = bool_param("positions", true);
    let offsets = bool_param("offsets", true);
    let filter = body.get("filter").and_then(|v| v.as_object());
    let mut tvs = Map::new();
    for field in field_list {
        let Some(text) = tv_doc_text(&doc, &field) else { continue };
        let analyzer = tv_field_analyzer(idx, &field, pf);
        let (fstats, df, ttf) = tv_index_stats_for_field(idx, &field);
        let tokens = tv_tokens(&analyzer, &text);
        let mut grouped: std::collections::BTreeMap<String, Vec<TvToken>> = Default::default();
        for t in tokens {
            grouped.entry(t.term.clone()).or_default().push(t);
        }
        let mut terms = Map::new();
        for (term, toks) in &grouped {
            terms.insert(
                term.clone(),
                tv_term_value(term, toks, term_stats, positions, offsets, &df, &ttf),
            );
        }
        if let Some(f) = filter {
            let maxn = f.get("max_num_terms").and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
            let minf = f.get("min_term_freq").and_then(|v| v.as_i64()).unwrap_or(1);
            let mindf = f.get("min_doc_freq").and_then(|v| v.as_i64()).unwrap_or(0);
            let mut kept: Vec<(String, Value)> = Vec::new();
            for (term, val) in terms.iter() {
                let tf = val.get("term_freq").and_then(|v| v.as_i64()).unwrap_or(0);
                let d = df.get(term).copied().unwrap_or(0);
                if tf < minf || d < mindf {
                    continue;
                }
                kept.push((term.clone(), val.clone()));
            }
            kept.sort_by(|a, b| a.0.cmp(&b.0));
            kept.truncate(maxn.max(0) as usize);
            terms = kept.into_iter().collect();
        }
        let mut fv = Map::new();
        if field_stats {
            fv.insert("field_statistics".to_string(), Value::Object(fstats.clone()));
        }
        if !terms.is_empty() {
            fv.insert("terms".to_string(), Value::Object(terms.clone()));
        }
        tvs.insert(field, Value::Object(fv));
    }
    let mut out = Map::new();
    out.insert("_index".to_string(), json!(idx.name));
    if let Some(i) = id {
        out.insert("_id".to_string(), json!(i));
        out.insert("_version".to_string(), json!(idx.versions.get(i).copied().unwrap_or(1)));
    } else {
        out.insert("_version".to_string(), json!(0));
    }
    out.insert("found".to_string(), json!(true));
    out.insert("took".to_string(), json!(1));
    if !tvs.is_empty() {
        out.insert("term_vectors".to_string(), Value::Object(tvs));
    }
    Ok(Value::Object(out))
}

pub fn handle_termvectors(store: &mut Store, req: &Req, name: &str, id: Option<&str>) -> Response {
    let body = if req.method == "POST" || !req.body.is_empty() {
        match req.json_body() {
            Ok(m) => m,
            Err(e) => return err_response(e),
        }
    } else {
        Map::new()
    };
    let qmap: std::collections::HashMap<String, String> = req.query.iter().cloned().collect();
    match termvectors_one(store, name, id, &body, &qmap) {
        Ok(v) => json_response(200, v),
        Err(e) => err_response(e),
    }
}

pub fn handle_mtermvectors(store: &mut Store, req: &Req, name: &str) -> Response {
    let body = match req.json_body() {
        Ok(m) => m,
        Err(e) => return err_response(e),
    };
    let params = body.get("parameters").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let mut docs_out = Vec::new();
    if let Some(docs) = body.get("docs").and_then(|v| v.as_array()) {
        for d in docs {
            let mut entry: Map<String, Value> = d.as_object().cloned().unwrap_or_default();
            let id = entry.get("_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            for (k, v) in &params {
                entry.entry(k.clone()).or_insert_with(|| v.clone());
            }
            let idx_name = entry.get("_index").and_then(|v| v.as_str()).unwrap_or(name).to_string();
            let q = std::collections::HashMap::new();
            match termvectors_one(store, &idx_name, id.as_deref(), &entry, &q) {
                Ok(v) => docs_out.push(v),
                Err(e) => docs_out.push(crate::errors::nested_error_value(&e)),
            }
        }
    } else if let Some(ids) = body.get("ids").and_then(|v| v.as_array()) {
        for id in ids {
            let sid = id.as_str().unwrap_or("").to_string();
            match termvectors_one(store, name, Some(&sid), &params, &std::collections::HashMap::new()) {
                Ok(v) => docs_out.push(v),
                Err(e) => docs_out.push(crate::errors::nested_error_value(&e)),
            }
        }
    }
    json_response(200, json!({"docs": docs_out}))
}

pub fn handle_explain(store: &mut Store, req: &Req, name: &str, id: &str) -> Response {
    let body = if req.method == "POST" || !req.body.is_empty() {
        match req.json_body() {
            Ok(m) => m,
            Err(e) => return err_response(e),
        }
    } else {
        Map::new()
    };
    let query_val = if let Some(q) = req.param("q") {
        parse_q_param(q)
    } else if let Some(q) = body.get("query") {
        q.clone()
    } else {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: query is missing;",
            400,
        ));
    };
    let idx = match store.indices.get(name) {
        Some(i) => i,
        None => return err_response(crate::shared::err_index_not_found_named(name)),
    };
    let Some(doc) = idx.docs.get(id).cloned() else {
        return json_response(404, json!({"_index": name, "_id": id, "matched": false}));
    };
    let query_map = query_val.as_object().cloned().unwrap_or_default();
    let (matched, score) = match crate::search::eval_doc(idx, id, &doc, &crate::search::precompute_doc_values(idx), &query_map) {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    let (field, term) = explain_field_term(&query_val);
    let mut out = Map::new();
    out.insert("_index".to_string(), json!(name));
    out.insert("_id".to_string(), json!(id));
    out.insert("matched".to_string(), json!(matched));
    let expl = if matched {
        bm25_explanation(idx, id, &query_val, score, name)
    } else if query_val.get("term").is_some() {
        json!({
            "value": 0.0,
            "description": format!("query doesn't match id [{}].", id),
            "details": []
        })
    } else {
        json!({
            "value": 0.0,
            "description": "no matching term",
            "details": []
        })
    };
    out.insert("explanation".to_string(), expl);
    if req.param("_source").is_some() || req.param("_source_includes").is_some() || req.param("_source_excludes").is_some() {
        let spec = get_source_spec(req);
        let src = if let Some(spec) = spec {
            match spec {
                Value::Bool(true) => Some(doc.clone()),
                other => Some(crate::search::filter_source(&doc, &other)),
            }
        } else {
            None
        };
        let mut g = Map::new();
        g.insert("_index".to_string(), json!(name));
        g.insert("_id".to_string(), json!(id));
        g.insert("_version".to_string(), json!(idx.versions.get(id).copied().unwrap_or(1)));
        g.insert("_seq_no".to_string(), json!(idx.seq_nos.get(id).copied().unwrap_or(0)));
        g.insert("_primary_term".to_string(), json!(1));
        g.insert("found".to_string(), json!(true));
        if let Some(s) = src {
            g.insert("_source".to_string(), s);
        }
        out.insert("get".to_string(), Value::Object(g));
    }
    json_response(200, Value::Object(out))
}

pub fn bm25_explanation(
    idx: &crate::store::Index,
    doc_id: &str,
    q: &Value,
    score: f64,
    index_name: &str,
) -> Value {
    let (field, term) = explain_field_term(q);
    if field.is_empty() {
        return json!({
            "value": score,
            "description": "match_all",
            "details": []
        });
    }
    let doc_tokens: Vec<String> = idx
        .doc_values
        .get(doc_id)
        .and_then(|m| m.get(&field))
        .map(|v| {
            let mut out = Vec::new();
            for val in crate::eval::array_values(v) {
                if let Some(s) = crate::fields::value_scalar_string(&val) {
                    out.extend(crate::store::analyze(&s));
                }
            }
            out
        })
        .unwrap_or_default();
    let dl = doc_tokens.len() as f64;
    let freq = doc_tokens.iter().filter(|t| **t == term).count() as f64;
    let mut n = 0usize;
    let mut total_docs = 0usize;
    let mut total_len = 0usize;
    for id in &idx.order {
        let toks: Vec<String> = idx
            .doc_values
            .get(id)
            .and_then(|m| m.get(&field))
            .map(|v| {
                let mut out = Vec::new();
                for val in crate::eval::array_values(v) {
                    if let Some(s) = crate::fields::value_scalar_string(&val) {
                        out.extend(crate::store::analyze(&s));
                    }
                }
                out
            })
            .unwrap_or_default();
        if toks.is_empty() {
            continue;
        }
        total_docs += 1;
        total_len += toks.len();
        if toks.iter().any(|t| *t == term) {
            n += 1;
        }
    }
    let nf = n as f64;
    let ntot = total_docs as f64;
    let avgdl = if total_docs > 0 {
        total_len as f64 / total_docs as f64
    } else {
        1.0
    };
    let idf = (1.0 + (ntot - nf + 0.5) / (nf + 0.5)).ln();
    json!({
        "value": score,
        "description": format!(
            "weight({}:{} in {})[PerFieldSimilarity], result of:",
            field, term, index_name
        ),
        "details": [{
            "value": score,
            "description": "score(freq=1.0), computed as boost * idf * tf from:",
            "details": [
                {
                    "value": idf,
                    "description": "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:",
                    "details": [
                        {"value": nf, "description": "n, number of documents containing term"},
                        {"value": ntot, "description": "N, total number of documents with field"}
                    ]
                },
                {
                    "value": freq / (freq + 1.2 * (1.0 - 0.75 + 0.75 * dl / avgdl)),
                    "description": "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:",
                    "details": [
                        {"value": 1.2, "description": "k1, term saturation parameter"},
                        {"value": 0.75, "description": "b, length normalization parameter"},
                        {"value": dl, "description": "dl, length of field"},
                        {"value": avgdl, "description": "avgdl, average length of field"}
                    ]
                }
            ]
        }]
    })
}

fn explain_field_term(q: &Value) -> (String, String) {
    if let Some(m) = q.get("match").and_then(|v| v.as_object()) {
        if let Some((f, v)) = m.iter().next() {
            let term = v
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| v.get("query").and_then(|x| x.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            return (f.clone(), term.to_lowercase());
        }
    }
    if let Some(m) = q.get("term").and_then(|v| v.as_object()) {
        if let Some((f, v)) = m.iter().next() {
            let term = v
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| v.get("value").and_then(|x| x.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            return (f.clone(), term);
        }
    }
    (String::new(), String::new())
}

fn raw_json_response(status: u16, body: Vec<u8>) -> Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        body,
    )
        .into_response()
}

fn normalize_filtered_numbers(v: &mut Value) {
    match v {
        Value::Number(n) => {
            let s = n.to_string();
            if s.contains('.') || s.contains('e') || s.contains('E') {
                if let Ok(f) = s.parse::<f64>() {
                    *v = Value::from(f);
                }
            }
        }
        Value::Array(a) => {
            for e in a {
                normalize_filtered_numbers(e);
            }
        }
        Value::Object(m) => {
            for e in m.values_mut() {
                normalize_filtered_numbers(e);
            }
        }
        _ => {}
    }
}

// ----------------------------------------------------------------- analyze

pub fn handle_analyze(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" && req.method != "GET" {
        return crate::errors::method_not_allowed(req, &["POST", "GET"]);
    }
    if !name.is_empty() && !store.indices.contains_key(name) {
        return err_response(crate::shared::err_index_not_found_named(name));
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let text = body
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let analyzer = body
        .get("analyzer")
        .and_then(|a| a.as_str())
        .or_else(|| body.get("tokenizer").and_then(|t| t.as_str()))
        .unwrap_or("")
        .to_string();
    let tokens = analyze_with(&analyzer, &text);
    let out: Vec<Value> = tokens
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"token": t, "position": i, "type": "<ALPHANUM>"}))
        .collect();
    json_response(200, json!({"tokens": out}))
}

// ------------------------------------------------------------ data streams

fn validate_data_stream_doc(timestamp_field: &str, body: &Map<String, Value>) -> Option<ApiError> {
    let reason = format!(
        "documents must contain a single-valued timestamp field '{}' of date type",
        timestamp_field
    );
    let err = || {
        ApiError::new("mapper_parsing_exception", "failed to parse", 400).caused_by(
            ApiError::new("mapper_parsing_exception", &reason, 400),
        )
    };
    let v = match body.get(timestamp_field) {
        Some(v) => v,
        None => return Some(err()),
    };
    if v.is_array() {
        return Some(err());
    }
    if let Some(s) = v.as_str() {
        if crate::store::parse_date_string(s).is_none() {
            return Some(err());
        }
    }
    None
}


fn byq_report(total: i64, updated: i64, deleted: i64, noops: i64, batches: i64, failures: &[Value]) -> Value {
    let mut v = json!({
        "took": 1, "timed_out": false, "total": total,
        "deleted": deleted, "batches": batches, "version_conflicts": 0,
        "noops": noops, "retries": {"bulk": 0, "search": 0},
        "throttled_millis": 0, "requests_per_second": -1.0,
        "throttled_until_millis": 0, "failures": failures,
    });
    v
}

fn byq_query(req: &Req, body: &Map<String, Value>, required: bool) -> Result<Value, ApiError> {
    if let Some(q) = body.get("query") {
        return Ok(q.clone());
    }
    if let Some(qp) = req.param("q") {
        if !qp.is_empty() {
            return Ok(parse_q_param(qp));
        }
    }
    if required {
        return Err(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: query is missing;",
            400,
        ));
    }
    Ok(json!({"match_all": {}}))
}

fn byq_max_docs(req: &Req, body: &Map<String, Value>) -> Option<usize> {
    body.get("max_docs")
        .and_then(|v| v.as_i64())
        .or_else(|| req.param("max_docs").and_then(|v| v.parse::<i64>().ok()))
        .filter(|n| *n >= 0)
        .map(|n| n as usize)
}

fn byq_matched(store: &Store, target: &str, req: &Req, body: &Map<String, Value>, required: bool) -> Result<Vec<String>, ApiError> {
    let query = byq_query(req, body, required)?;
    let qm = query.as_object().cloned().unwrap_or_default();
    let idx = &store.indices[target];
    let extra = precompute_doc_values(idx);
    let mut ids = Vec::new();
    for id in &idx.order {
        if idx.pending.contains_key(id) { continue; }
        let Some(doc) = idx.docs.get(id) else { continue; };
        match crate::search::eval_doc(idx, id, doc, &extra, &qm) {
            Ok((true, _)) => ids.push(id.clone()),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(ids)
}

pub fn handle_delete_by_query(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    if let Some(v) = req.param("conflicts") {
        if v != "proceed" && v != "abort" {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("conflicts may only be \"proceed\" or \"abort\" but was [{}]", v),
                400,
            ));
        }
    }
    let target = match resolve_write_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => return err_response(e),
    };
    let mut body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let mut ids = match byq_matched(store, &target, req, &body, true) {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    if let Some(max) = byq_max_docs(req, &body) {
        ids.truncate(max);
    }
    let total = ids.len() as i64;
    let mut deleted = 0i64;
    for id in &ids {
        let idx = store.indices.get_mut(&target).unwrap();
        if idx.docs.remove(id).is_some() {
            crate::fields::remove_doc_values(idx, id);
            let old_ver = idx.versions.get(id).copied().unwrap_or(1);
            let seq = idx.index_total;
            idx.versions.insert(id.clone(), old_ver + 1);
            idx.seq_nos.insert(id.clone(), seq);
            idx.index_total += 1;
            idx.delete_total += 1;
            deleted += 1;
        }
    }
    if req.bool_param("refresh") {
        store.indices.get_mut(&target).unwrap().pending.clear();
    }
    let batches = if total > 0 { 1 } else { 0 };
    let mut out = byq_report(total, 0, deleted, 0, batches, &[]);
    let slices = req.param("slices").and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0);
    if let Some(n) = slices {
        let mut arr = Vec::new();
        for i in 0..n {
            let c = if n == 0 { 0 } else { deleted / n as i64 + if (i as i64) < deleted % n as i64 { 1 } else { 0 } };
            arr.push(json!({"slice_id": i, "total": c, "deleted": c}));
        }
        out.as_object_mut().unwrap().insert("slices".to_string(), json!(arr));
    }
    json_response(200, out)
}

pub fn handle_update_by_query(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    let target = match resolve_write_target(store, name) {
        Ok(t) => t.name.clone(),
        Err(e) => return err_response(e),
    };
    let mut body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let mut ids = match byq_matched(store, &target, req, &body, false) {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    if let Some(max) = byq_max_docs(req, &body) {
        ids.truncate(max);
    }
    let total = ids.len() as i64;
    let script_obj = body.get("script").and_then(|v| v.as_object());
    let script_src = match body.get("script") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(m)) => m.get("source").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    };
    let params = script_obj.map(crate::painless::script_params_of).unwrap_or_default();
    let pipeline_name = match crate::ingest::effective_ingest_pipeline(store, &target, req.param("pipeline").unwrap_or("")) {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    if let Some(src) = &script_src {
        let idx = &store.indices[&target];
        let mut sc = Map::new();
        let mut op = String::new();
        if let Err((runtime, msg)) = run_ctx_script(src, idx, &mut sc, &params, "", 0, &mut op) {
            let mut e = ApiError::new("script_exception", if runtime { "runtime error" } else { "compile error" }, 400);
            e = e.field("lang", json!("painless"));
            if msg != "compile error" && msg != "runtime error" {
                e.caused_by = Some(Box::new(ApiError::new("illegal_argument_exception", &msg, 400)));
            }
            return err_response(e);
        }
    }
    let mut updated = 0i64;
    let mut deleted = 0i64;
    let mut noops = 0i64;
    let mut failures: Vec<Value> = Vec::new();
    let mut failure_status = 0u16;
    for id in &ids {
        let old = match store.indices.get(&target).and_then(|i| i.docs.get(id)).and_then(|d| d.as_object()).cloned() {
            Some(d) => d,
            None => continue,
        };
        let old_ver = store.indices.get(&target).and_then(|i| i.versions.get(id)).copied().unwrap_or(1);
        let mut cur = old.clone();
        let mut op = String::new();
        if let Some(src) = &script_src {
            let idx = &store.indices[&target];
            if let Err((_runtime, _msg)) = run_ctx_script(src, idx, &mut cur, &params, id, old_ver, &mut op) {
                failure_status = 400;
                failures.push(json!({"id": id, "index": target,
                    "cause": {"type": "script_exception", "reason": "compile error"}}));
                continue;
            }
        }
        if op == "delete" {
            let idx = store.indices.get_mut(&target).unwrap();
            idx.docs.remove(id);
            crate::fields::remove_doc_values(idx, id);
            let seq = idx.index_total;
            idx.versions.insert(id.clone(), old_ver + 1);
            idx.seq_nos.insert(id.clone(), seq);
            idx.index_total += 1;
            idx.delete_total += 1;
            deleted += 1;
            continue;
        }
        if op == "noop" || op == "none" {
            noops += 1;
            continue;
        }
        let mut newdoc = cur;
        let mut pipeline_applied = false;
        if let Some(pname) = &pipeline_name {
            let mut meta = Map::new();
            meta.insert("_index".to_string(), json!(target));
            meta.insert("_id".to_string(), json!(id));
            match crate::ingest::run_ingest_pipeline(store, pname, newdoc.clone(), meta) {
                Ok(out) => {
                    if out.dropped {
                        let idx = store.indices.get_mut(&target).unwrap();
                        idx.docs.remove(id);
                        idx.raw_sources.remove(id);
                        crate::fields::remove_doc_values(idx, id);
                        deleted += 1;
                        continue;
                    }
                    newdoc = out.source;
                    pipeline_applied = true;
                }
                Err(e) => {
                    failure_status = e.status;
                    failures.push(json!({"id": id, "index": target,
                        "cause": {"type": e.typ, "reason": e.reason}}));
                    continue;
                }
            }
        }
        let idx = store.indices.get_mut(&target).unwrap();
        if let Err(e) = crate::fields::compute_doc_values(idx, id, &mut newdoc) {
            failure_status = e.status;
            failures.push(json!({"id": id, "index": target,
                "cause": {"type": e.typ, "reason": e.reason}}));
            continue;
        }
        let changed = pipeline_applied
            || (script_src.is_some() && Value::Object(newdoc.clone()) != Value::Object(old));
        if script_src.is_some() && !changed {
            noops += 1;
            continue;
        }
        let seq = idx.index_total;
        idx.versions.insert(id.clone(), old_ver + 1);
        idx.seq_nos.insert(id.clone(), seq);
        idx.index_total += 1;
        idx.docs.insert(id.clone(), Value::Object(newdoc.clone()));
        idx.raw_sources.insert(id.clone(), serde_json::to_vec(&Value::Object(newdoc)).unwrap_or_default());
        updated += 1;
    }
    if req.bool_param("refresh") {
        store.indices.get_mut(&target).unwrap().pending.clear();
    }
    let batches = if total > 0 { 1 } else { 0 };
    let mut out = byq_report(total, updated, deleted, noops, batches, &failures);
    out.as_object_mut().unwrap().insert("updated".to_string(), json!(updated));
    if let Some(n) = req.param("slices").and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0) {
        let mut arr = Vec::new();
        for i in 0..n {
            let c = if n == 0 { 0 } else { updated / n as i64 + if (i as i64) < updated % n as i64 { 1 } else { 0 } };
            arr.push(json!({"slice_id": i, "total": c, "updated": c}));
        }
        out.as_object_mut().unwrap().insert("slices".to_string(), json!(arr));
    }
    json_response(if failure_status >= 400 { failure_status } else { 200 }, out)
}

