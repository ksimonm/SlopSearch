use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, ApiError};
use crate::shared::{parse_resolve_params, resolve_spec2, ResolveOpts2};
use crate::store::{Store, NODE_ID, NODE_NAME};
use crate::Req;

// ----------------------------------------------------------- refresh/flush

pub fn maintenance_op(store: &mut Store, req: &Req, spec: &str, op: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut shards = 0i64;
    for name in &names {
        let idx = &store.indices[name];
        if idx.closed {
            return err_response(crate::shared::err_index_closed(idx));
        }
        shards += idx.num_shards();
    }
    for name in &names {
        let idx = store.indices.get_mut(name).unwrap();
        match op {
            "refresh" => idx.pending.clear(),
            "forcemerge" => idx.force_merged = true,
            _ => {}
        }
    }
    if op == "forcemerge" {
        for name in &names {
            store.indices.get_mut(name).unwrap().pending.clear();
        }
    }
    store.bump_state();
    json_response(
        200,
        json!({"_shards": {"total": shards, "successful": shards, "failed": 0}}),
    )
}

// ------------------------------------------------------------------ stats

pub const STATS_METRIC_NAMES: &[&str] = &[
    "docs",
    "store",
    "indexing",
    "get",
    "search",
    "merges",
    "refresh",
    "flush",
    "warmer",
    "query_cache",
    "fielddata",
    "completion",
    "segments",
    "translog",
    "request_cache",
    "recovery",
];

fn empty_metric(name: &str) -> Value {
    match name {
        "docs" => json!({"count": 0, "deleted": 0}),
        "store" => json!({"size_in_bytes": 0, "total_data_set_size_in_bytes": 0, "reserved_in_bytes": 0}),
        "indexing" => json!({"index_total": 0, "index_time_in_millis": 0, "index_current": 0,
            "index_failed": 0, "delete_total": 0, "delete_time_in_millis": 0, "delete_current": 0,
            "noop_update_total": 0, "is_throttled": false, "throttle_time_in_millis": 0}),
        "get" => json!({"total": 0, "time_in_millis": 0, "exists_total": 0, "exists_time_in_millis": 0,
            "missing_total": 0, "missing_time_in_millis": 0, "current": 0}),
        "search" => json!({"query_total": 0, "query_time_in_millis": 0, "query_current": 0,
            "fetch_total": 0, "fetch_time_in_millis": 0, "fetch_current": 0,
            "scroll_total": 0, "scroll_time_in_millis": 0, "scroll_current": 0,
            "suggest_total": 0, "suggest_time_in_millis": 0, "suggest_current": 0}),
        "merges" => json!({"current": 0, "current_docs": 0, "current_size_in_bytes": 0,
            "total": 0, "total_time_in_millis": 0, "total_docs": 0, "total_size_in_bytes": 0,
            "total_stopped_time_in_millis": 0, "total_throttled_time_in_millis": 0, "total_auto_throttle_in_bytes": 0}),
        "refresh" => json!({"total": 0, "total_time_in_millis": 0, "external_total": 0,
            "external_total_time_in_millis": 0, "listeners": 0}),
        "flush" => json!({"total": 0, "periodic": 0, "total_time_in_millis": 0}),
        "warmer" => json!({"current": 0, "total": 0, "total_time_in_millis": 0}),
        "query_cache" => json!({"memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
            "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0}),
        "fielddata" => json!({"memory_size_in_bytes": 0, "evictions": 0}),
        "completion" => json!({"size_in_bytes": 0}),
        "segments" => json!({"count": 0, "memory_in_bytes": 0, "terms_memory_in_bytes": 0,
            "stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0,
            "norms_memory_in_bytes": 0, "points_memory_in_bytes": 0, "doc_values_memory_in_bytes": 0,
            "index_writer_memory_in_bytes": 0, "version_map_memory_in_bytes": 0,
            "fixed_bit_set_memory_in_bytes": 0, "max_unsafe_auto_id_timestamp": -1,
            "file_sizes": {}}),
        "translog" => json!({"operations": 0, "size_in_bytes": 0, "uncommitted_operations": 0,
            "uncommitted_size_in_bytes": 0, "earliest_last_modified_age": 0}),
        "request_cache" => json!({"memory_size_in_bytes": 0, "evictions": 0, "hit_count": 0, "miss_count": 0}),
        "recovery" => json!({"current_as_source": 0, "current_as_target": 0, "throttle_time_in_millis": 0}),
        _ => json!({}),
    }
}

fn index_metrics(idx: &crate::store::Index, requested: Option<&Vec<String>>) -> Value {
    let mut out = Map::new();
    for name in STATS_METRIC_NAMES {
        if let Some(req) = requested {
            if !req.iter().any(|r| r == name) {
                continue;
            }
        }
        let mut m = empty_metric(name);
        match *name {
            "docs" => {
                m["count"] = json!(idx.docs.len());
                m["deleted"] = json!(idx.delete_total);
            }
            "store" => {
                m["size_in_bytes"] = json!(idx.store_bytes());
                m["total_data_set_size_in_bytes"] = json!(idx.store_bytes());
            }
            "indexing" => {
                m["index_total"] = json!(idx.index_total);
                m["delete_total"] = json!(idx.delete_total);
            }
            "segments" => {
                m["count"] = json!(1);
                m["memory_in_bytes"] = json!(0);
            }
            _ => {}
        }
        out.insert(name.to_string(), m);
    }
    Value::Object(out)
}

fn merge_metrics(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        let sv = match v.as_object() {
            Some(s) => s,
            None => continue,
        };
        if !dst.get(k).map(|d| d.is_object()).unwrap_or(false) {
            dst.insert(k.clone(), Value::Object(Map::new()));
        }
        let dv = dst.get_mut(k).unwrap().as_object_mut().unwrap();
        for (mk, mv) in sv {
            match mv {
                Value::Number(n) if n.is_i64() || n.is_u64() => {
                    let a = n.as_i64().unwrap_or(0);
                    if let Some(b) = dv.get(mk).and_then(|x| x.as_i64()) {
                        dv.insert(mk.clone(), json!(a + b));
                    } else {
                        dv.insert(mk.clone(), mv.clone());
                    }
                }
                _ => {
                    if !dv.contains_key(mk) {
                        dv.insert(mk.clone(), mv.clone());
                    }
                }
            }
        }
    }
}

pub fn handle_stats_api(store: &mut Store, req: &Req, spec: &str, metrics: &str) -> Response {
    let mut requested: Option<Vec<String>> = None;
    if !metrics.is_empty() {
        let mut req_list: Vec<String> = vec![];
        for m in metrics.split(',') {
            let m = m.trim();
            if m.is_empty() {
                continue;
            }
            if m == "_all" || m == "*" {
                req_list.clear();
                requested = None;
                break;
            }
            if !STATS_METRIC_NAMES.contains(&m) {
                return err_response(ApiError::bad(&format!(
                    "request [{}] contains unrecognized metric: [{}]",
                    req.path, m
                )));
            }
            req_list.push(m.to_string());
        }
        if !req_list.is_empty() {
            requested = Some(req_list);
        }
    }
    let mut level = req.param_or("level", "indices");
    if level.is_empty() {
        level = "indices".to_string();
    }
    match level.as_str() {
        "cluster" | "indices" | "shards" => {}
        _ => {
            return err_response(ApiError::bad(&format!(
                "level parameter must be one of [cluster] or [indices] or [shards] but was [{}]",
                level
            )))
        }
    }
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut shards = 0i64;
    let mut indices_out = Map::new();
    let mut all_prim = Map::new();
    let mut all_total = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        if idx.closed {
            return err_response(crate::shared::err_index_closed(idx));
        }
        shards += idx.num_shards();
        let prim = index_metrics(idx, requested.as_ref());
        let total = index_metrics(idx, requested.as_ref());
        let mut entry = Map::new();
        entry.insert("uuid".to_string(), json!(idx.uuid));
        entry.insert("primaries".to_string(), prim.clone());
        entry.insert("total".to_string(), total.clone());
        if level == "shards" {
            let shard_arr = json!([{
                "id": 0,
                "routing": {"state": "STARTED", "primary": true, "node": NODE_ID},
                "docs": prim.get("docs"),
                "store": prim.get("store"),
                "indexing": prim.get("indexing"),
                "search": prim.get("search"),
                "merges": prim.get("merges"),
                "refresh": prim.get("refresh"),
                "flush": prim.get("flush"),
                "segments": {"count": 1},
            }]);
            entry.insert("shards".to_string(), json!({"0": shard_arr}));
        }
        indices_out.insert(idx.name.clone(), Value::Object(entry));
        if let Some(p) = prim.as_object() {
            merge_metrics(&mut all_prim, p);
        }
        if let Some(t) = total.as_object() {
            merge_metrics(&mut all_total, t);
        }
    }
    let mut resp = Map::new();
    resp.insert(
        "_shards".to_string(),
        json!({"total": shards, "successful": shards, "failed": 0}),
    );
    resp.insert(
        "_all".to_string(),
        json!({"primaries": all_prim, "total": all_total}),
    );
    if level != "cluster" {
        resp.insert("indices".to_string(), Value::Object(indices_out));
    }
    json_response(200, Value::Object(resp))
}

// --------------------------------------------------------------- segments

pub fn handle_segments_api(store: &mut Store, req: &Req, spec: &str) -> Response {
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut shards = 0i64;
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        if idx.closed {
            return err_response(crate::shared::err_index_closed(idx));
        }
        shards += idx.num_shards();
        let mut shard_map = Map::new();
        for i in 0..idx.num_shards() {
            let num_docs = if i == 0 { idx.docs.len() } else { 0 };
            let segments = json!({
                "_0": {
                    "generation": 0,
                    "num_docs": num_docs,
                    "deleted_docs": 0,
                    "size_in_bytes": idx.store_bytes() / (idx.num_shards() + 1),
                    "committed": true,
                    "search": true,
                    "compound": false,
                    "version": "8.11.1",
                }
            });
            let entry = json!({
                "routing": {"state": "STARTED", "primary": true, "node": NODE_ID},
                "num_committed_segments": 1,
                "num_search_segments": 1,
                "segments": segments,
            });
            shard_map.insert(i.to_string(), json!([entry]));
        }
        out.insert(
            idx.name.clone(),
            json!({"shards": Value::Object(shard_map)}),
        );
    }
    json_response(
        200,
        json!({"_shards": {"total": shards, "successful": shards, "failed": 0}, "indices": out}),
    )
}

// --------------------------------------------------------------- recovery

pub fn handle_recovery_api(store: &mut Store, req: &Req, spec: &str) -> Response {
    if req.bool_param("active_only") {
        return json_response(200, json!({}));
    }
    let o = parse_resolve_params(req, ResolveOpts2::default());
    let names = match resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut out = Map::new();
    for name in &names {
        let idx = &store.indices[name];
        let mut shard_list = vec![];
        for i in 0..idx.num_shards() {
            shard_list.push(json!({
                "id": i,
                "type": "EMPTY_STORE",
                "stage": "DONE",
                "primary": true,
                "start_time_in_millis": idx.creation_date,
                "stop_time_in_millis": idx.creation_date,
                "total_time_in_millis": 0,
                "source": {},
                "target": {"id": NODE_ID, "host": "127.0.0.1", "transport_address": "127.0.0.1:9300", "ip": "127.0.0.1", "name": NODE_NAME},
                "index": {"size": {"total_in_bytes": idx.store_bytes()}, "files": {"total": 1, "reused": 0, "recovered": 0}, "total_time_in_millis": 0},
                "translog": {"recovered": 0, "total": 0, "percent": "100.0%", "total_on_start": 0, "total_time_in_millis": 0},
                "verify_index": {"check_index_time_in_millis": 0, "total_time_in_millis": 0},
            }));
        }
        out.insert(idx.name.clone(), json!({"shards": shard_list}));
    }
    json_response(200, Value::Object(out))
}
