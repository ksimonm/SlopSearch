use serde_json::{json, Map, Number, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::resp::parse_simple_yaml;
use crate::shared::{
    err_index_not_found_named, resolve_indices, settings_to_nested, sorted_index_names,
};
use crate::store::*;
use crate::{Req, Store};
use axum::response::Response;

pub(crate) const NODE_HOST: &str = "127.0.0.1";
const NODE_EPHEMERAL: &str = "slopsearch-ephemeral-id";
const NODE_TRANSPORT: &str = "127.0.0.1:9300";
const STATE_UUID: &str = "slopsearch-state-uuid";

pub(crate) fn node_roles() -> Value {
    json!(["cluster_manager", "data", "ingest", "remote_cluster_client"])
}

pub(crate) fn node_attributes() -> Value {
    json!({"shard_indexing_pressure_enabled": "true"})
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Mirrors Go's `time.UnixMilli(ms).UTC().Format("2006-01-02T15:04:05.000Z")`.
pub(crate) fn format_utc_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mi = (rem % 3600) / 60;
    let ss = rem % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, hh, mi, ss, millis
    )
}

pub fn root_version() -> Value {
    json!({
        "distribution": "opensearch",
        "number": NODE_VERSION,
        "build_type": "tar",
        "build_hash": "0000000000000000000000000000000000000000",
        "build_date": "2026-01-01T00:00:00.000000000Z",
        "build_snapshot": false,
        "lucene_version": LUCENE_VERSION,
        "minimum_wire_compatibility_version": "2.19.0",
        "minimum_index_compatibility_version": "2.0.0",
    })
}

pub fn handle_root(_store: &mut Store, req: &Req) -> Response {
    match req.method.as_str() {
        "GET" | "HEAD" => {}
        "DELETE" => {
            return err_response(ApiError::new(
                "action_request_validation_exception",
                "Validation Failed: 1: index / indices is missing;",
                400,
            ))
        }
        "OPTIONS" => {
            let mut resp = Response::builder()
                .status(200)
                .body(axum::body::Body::empty())
                .unwrap();
            resp.headers_mut()
                .insert("allow", "DELETE,HEAD,GET".parse().unwrap());
            return resp;
        }
        m => return method_not_allowed(req, &["DELETE", "HEAD", "GET"]),
    }
    let body = json!({
        "name": NODE_NAME,
        "cluster_name": CLUSTER_NAME,
        "cluster_uuid": CLUSTER_UUID,
        "version": root_version(),
        "tagline": "The OpenSearch Project: https://opensearch.org/",
    });
    let pretty = match req.param("pretty") {
        None => true,
        Some(v) => v != "false",
    };
    let rendered = crate::resp::render_ordered_json(
        &[
            ("name", body.get("name").unwrap()),
            ("cluster_name", body.get("cluster_name").unwrap()),
            ("cluster_uuid", body.get("cluster_uuid").unwrap()),
            ("version", body.get("version").unwrap()),
            ("tagline", body.get("tagline").unwrap()),
        ],
        pretty,
    );
    Response::builder()
        .status(200)
        .header("content-type", "application/json; charset=UTF-8")
        .header("x-lite-ordered", "1")
        .body(axum::body::Body::from(rendered))
        .unwrap()
}

// ------------------------------------------------------------------- health

const HEALTH_KNOWN_PARAMS: &[&str] = &[
    "level",
    "wait_for_status",
    "wait_for_active_shards",
    "wait_for_nodes",
    "wait_for_events",
    "wait_for_no_relocating_shards",
    "wait_for_no_initializing_shards",
    "timeout",
    "cluster_manager_timeout",
    "master_timeout",
    "local",
    "human",
    "expand_wildcards",
    "ignore_unavailable",
    "allow_no_indices",
    "filter_path",
    "pretty",
    "format",
    "error_trace",
];

fn validate_known_params(req: &Req, known: &[&str], req_path: &str) -> Option<ApiError> {
    let mut seen: Vec<&str> = vec![];
    for (name, _) in &req.query {
        if seen.contains(&name.as_str()) {
            continue;
        }
        seen.push(name.as_str());
        if !known.contains(&name.as_str()) {
            return Some(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "request [{}] contains unrecognized parameter: [{}]",
                    req_path, name
                ),
                400,
            ));
        }
    }
    None
}

fn parse_timeout(v: &str) -> bool {
    if v.is_empty() {
        return true;
    }
    let units: [&str; 5] = ["ms", "s", "m", "h", "d"];
    for u in units {
        if let Some(rest) = v.strip_suffix(u) {
            return rest.parse::<f64>().is_ok();
        }
    }
    v.parse::<f64>().is_ok()
}

struct ShardHealth {
    status: String,
    primary_active: bool,
    active_shards: i64,
    relocating: i64,
    initializing: i64,
    unassigned: i64,
}

pub(crate) struct IndexHealth {
    pub(crate) status: String,
    pub(crate) num_shards: i64,
    pub(crate) num_replicas: i64,
    pub(crate) active_primary: i64,
    pub(crate) active: i64,
    pub(crate) unassigned: i64,
    shards: Vec<ShardHealth>,
}

pub(crate) fn compute_index_health(idx: &Index) -> IndexHealth {
    let mut h = IndexHealth {
        status: "green".to_string(),
        num_shards: idx.num_shards(),
        num_replicas: idx.num_replicas(),
        active_primary: 0,
        active: 0,
        unassigned: 0,
        shards: vec![],
    };
    if idx.closed {
        return h;
    }
    h.active_primary = h.num_shards;
    h.active = h.num_shards;
    h.unassigned = h.num_shards * h.num_replicas;
    if h.unassigned > 0 {
        h.status = "yellow".to_string();
    }
    for _ in 0..h.num_shards {
        let mut sh = ShardHealth {
            status: "green".to_string(),
            primary_active: true,
            active_shards: 1,
            relocating: 0,
            initializing: 0,
            unassigned: 0,
        };
        if h.num_replicas > 0 {
            sh.status = "yellow".to_string();
            sh.unassigned = h.num_replicas;
        }
        h.shards.push(sh);
    }
    h
}

pub(crate) fn worst_status(a: &str, b: &str) -> String {
    let rank = |s: &str| match s {
        "green" => 0,
        "yellow" => 1,
        "red" => 2,
        _ => 0,
    };
    if rank(b) > rank(a) {
        b.to_string()
    } else {
        a.to_string()
    }
}

fn wait_for_nodes_satisfied(expr: &str, nodes: i64) -> bool {
    let expr = expr.trim();
    if let Some(rest) = expr.strip_prefix(">=") {
        return rest.trim().parse::<i64>().map(|n| nodes >= n).unwrap_or(false);
    }
    if let Some(rest) = expr.strip_prefix("<=") {
        return rest.trim().parse::<i64>().map(|n| nodes <= n).unwrap_or(false);
    }
    for prefix in ["le(", "ge(", "lt(", "gt("] {
        if expr.starts_with(prefix) && expr.ends_with(')') {
            let inner = &expr[prefix.len()..expr.len() - 1];
            let n = match inner.parse::<i64>() {
                Ok(n) => n,
                Err(_) => return false,
            };
            return match prefix {
                "le(" => nodes <= n,
                "ge(" => nodes >= n,
                "lt(" => nodes < n,
                "gt(" => nodes > n,
                _ => false,
            };
        }
    }
    expr.parse::<i64>().map(|n| nodes == n).unwrap_or(false)
}

fn one_decimal(v: f64) -> Value {
    format!("{:.1}", v)
        .parse::<Number>()
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn handle_health(store: &mut Store, req: &Req, index_spec: &str) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    if let Some(e) = validate_known_params(req, HEALTH_KNOWN_PARAMS, "/_cluster/health") {
        return err_response(e);
    }
    if let Some(v) = req.param("wait_for_status") {
        if !v.is_empty() {
            let up = v.to_uppercase();
            if up != "GREEN" && up != "YELLOW" && up != "RED" {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "No enum constant org.opensearch.cluster.health.ClusterHealthStatus.{}",
                        up
                    ),
                    400,
                ));
            }
        }
    }
    if let Some(v) = req.param("wait_for_active_shards") {
        if !v.is_empty() && v != "all" {
            let ok = v.parse::<i64>().map(|n| n >= 0).unwrap_or(false);
            if !ok {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("cannot parse ActiveShardCount[{}]", v),
                    400,
                ));
            }
        }
    }
    if let Some(v) = req.param("wait_for_events") {
        if !v.is_empty() {
            let valid = matches!(
                v.to_lowercase().as_str(),
                "immediate" | "urgent" | "high" | "normal" | "low" | "languid"
            );
            if !valid {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("No enum constant org.opensearch.common.Priority.{}", v.to_uppercase()),
                    400,
                ));
            }
        }
    }
    if !parse_timeout(req.param("timeout").unwrap_or("")) {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            &format!("failed to parse [{}]", req.param("timeout").unwrap_or("")),
            400,
        ));
    }

    let level = req.param("level").unwrap_or("").to_string();
    let include_indices = level == "indices" || level == "shards";
    let mut missing = false;
    let mut indices: Vec<&Index> = vec![];
    let ignore_unavailable = req.param("ignore_unavailable") == Some("true");
    let allow_no_indices = req.param("allow_no_indices") == Some("true");
    if !index_spec.is_empty() {
        match resolve_indices(store, index_spec, true, true, true) {
            Ok(v) => indices = v,
            Err(_) => {
                missing = true;
                if ignore_unavailable || allow_no_indices {
                    missing = false;
                    indices = vec![];
                }
            }
        }
    } else {
        indices = resolve_indices(store, "*", false, true, false).unwrap_or_default();
    }

    let mut status = "green".to_string();
    let mut total_prim: i64 = 0;
    let mut total_active: i64 = 0;
    let mut total_unassigned: i64 = 0;
    if missing {
        status = "red".to_string();
    }
    let mut per_index: Vec<(String, IndexHealth)> = vec![];
    for idx in &indices {
        let h = compute_index_health(idx);
        status = worst_status(&status, &h.status);
        total_prim += h.active_primary;
        total_active += h.active;
        total_unassigned += h.unassigned;
        per_index.push((idx.name.clone(), h));
    }

    let mut timed_out = missing;
    if !missing {
        if let Some(v) = req.param("wait_for_status") {
            if !v.is_empty() {
                let rank = |s: &str| match s {
                    "green" => 0,
                    "yellow" => 1,
                    "red" => 2,
                    _ => 0,
                };
                if rank(&status) > rank(&v.to_lowercase()) {
                    timed_out = true;
                }
            }
        }
        if let Some(v) = req.param("wait_for_active_shards") {
            if !v.is_empty() {
                if v == "all" {
                    if total_unassigned > 0 {
                        timed_out = true;
                    }
                } else if let Ok(n) = v.parse::<i64>() {
                    if n > total_active {
                        timed_out = true;
                    }
                }
            }
        }
        if let Some(v) = req.param("wait_for_nodes") {
            if !v.is_empty() && !wait_for_nodes_satisfied(v, 1) {
                timed_out = true;
            }
        }
    }

    let total = total_active + total_unassigned;
    let pct = if total > 0 {
        total_active as f64 / total as f64 * 100.0
    } else {
        100.0
    };

    let mut body = Map::new();
    body.insert("cluster_name".into(), json!(CLUSTER_NAME));
    body.insert("status".into(), json!(status));
    body.insert("timed_out".into(), json!(timed_out));
    body.insert("number_of_nodes".into(), json!(1));
    body.insert("number_of_data_nodes".into(), json!(1));
    body.insert("discovered_master".into(), json!(true));
    body.insert("discovered_cluster_manager".into(), json!(true));
    body.insert("active_primary_shards".into(), json!(total_prim));
    body.insert("active_shards".into(), json!(total_active));
    body.insert("relocating_shards".into(), json!(0));
    body.insert("initializing_shards".into(), json!(0));
    body.insert("unassigned_shards".into(), json!(total_unassigned));
    body.insert("delayed_unassigned_shards".into(), json!(0));
    body.insert("number_of_pending_tasks".into(), json!(0));
    body.insert("number_of_in_flight_fetch".into(), json!(0));
    body.insert("task_max_waiting_in_queue_millis".into(), json!(0));
    body.insert("active_shards_percent_as_number".into(), one_decimal(pct));
    if req.bool_param("human") {
        body.insert("task_max_waiting_in_queue".into(), json!("0s"));
    }
    if include_indices {
        let mut out = Map::new();
        for (name, h) in &per_index {
            let mut entry = Map::new();
            entry.insert("status".into(), json!(h.status));
            entry.insert("number_of_shards".into(), json!(h.num_shards));
            entry.insert("number_of_replicas".into(), json!(h.num_replicas));
            entry.insert("active_primary_shards".into(), json!(h.active_primary));
            entry.insert("active_shards".into(), json!(h.active));
            entry.insert("relocating_shards".into(), json!(0));
            entry.insert("initializing_shards".into(), json!(0));
            entry.insert("unassigned_shards".into(), json!(h.unassigned));
            if level == "shards" {
                let mut shards = Map::new();
                for (i, sh) in h.shards.iter().enumerate() {
                    shards.insert(
                        i.to_string(),
                        json!({
                            "status": sh.status,
                            "primary_active": sh.primary_active,
                            "active_shards": sh.active_shards,
                            "relocating_shards": sh.relocating,
                            "initializing_shards": sh.initializing,
                            "unassigned_shards": sh.unassigned,
                        }),
                    );
                }
                entry.insert("shards".into(), Value::Object(shards));
            }
            out.insert(name.clone(), Value::Object(entry));
        }
        body.insert("indices".into(), Value::Object(out));
    }
    let code = if timed_out { 408 } else { 200 };
    json_response(code, Value::Object(body))
}

// -------------------------------------------------------------------- state

fn state_nodes() -> Value {
    json!({
        NODE_ID: {
            "name": NODE_NAME,
            "ephemeral_id": NODE_EPHEMERAL,
            "transport_address": NODE_TRANSPORT,
            "attributes": node_attributes(),
            "roles": node_roles(),
            "version": NODE_VERSION,
        }
    })
}

pub fn handle_cluster_state(
    store: &mut Store,
    req: &Req,
    metrics: &str,
    index_spec: &str,
) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    let mut requested: Vec<String> = vec![];
    let metrics = if metrics.is_empty() { "_all" } else { metrics };
    if metrics == "_all" {
        for m in [
            "version",
            "master_node",
            "cluster_manager_node",
            "nodes",
            "metadata",
            "routing_table",
            "routing_nodes",
            "blocks",
        ] {
            requested.push(m.to_string());
        }
    } else {
        for m in metrics.split(',') {
            requested.push(m.to_string());
        }
    }
    let want = |m: &str| requested.iter().any(|r| r == m);

    let mut resp_headers: Vec<(&str, String)> = vec![];
    if want("master_node") {
        resp_headers.push((
            "warning",
            "299 OpenSearch-3.9.0-00000000 \"[cluster_manager_node] is deprecated\"".to_string(),
        ));
    }

    let mut body = Map::new();
    body.insert("cluster_name".into(), json!(CLUSTER_NAME));
    body.insert("cluster_uuid".into(), json!(CLUSTER_UUID));
    if want("version") {
        body.insert("version".into(), json!(store.state_version));
        body.insert("state_uuid".into(), json!(STATE_UUID));
    }
    if want("master_node") {
        body.insert("master_node".into(), json!(NODE_ID));
    }
    if want("cluster_manager_node") {
        body.insert("cluster_manager_node".into(), json!(NODE_ID));
    }
    if want("nodes") {
        body.insert("nodes".into(), state_nodes());
    }
    let indices: Vec<&Index> = if !index_spec.is_empty() {
        resolve_indices(store, index_spec, false, true, true).unwrap_or_default()
    } else {
        resolve_indices(store, "*", false, true, false).unwrap_or_default()
    };
    if want("metadata") {
        body.insert("metadata".into(), build_state_metadata(&indices));
    }
    if want("routing_table") {
        body.insert("routing_table".into(), build_routing_table(&indices));
    }
    if want("routing_nodes") {
        body.insert("routing_nodes".into(), build_routing_nodes(&indices));
    }
    if want("blocks") {
        body.insert("blocks".into(), build_blocks(&indices));
    }
    let mut resp = json_response(200, Value::Object(body));
    for (k, v) in resp_headers {
        resp.headers_mut().insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    resp
}

fn build_state_metadata(indices: &[&Index]) -> Value {
    let mut idx_map = Map::new();
    for idx in indices {
        let state = if idx.closed { "close" } else { "open" };
        let mut aliases = Map::new();
        for (name, def) in &idx.aliases {
            aliases.insert(name.clone(), def.clone());
        }
        idx_map.insert(
            idx.name.clone(),
            json!({
                "version": 1,
                "mapping_version": idx.mapping_version,
                "settings_version": 1,
                "aliases_version": 1,
                "routing_num_shards": 1,
                "state": state,
                "settings": index_settings_for_metadata(idx),
                "mappings": mappings_for_metadata(idx),
                "aliases": Value::Object(aliases),
            }),
        );
    }
    json!({
        "cluster_uuid": CLUSTER_UUID,
        "templates": {},
        "indices": Value::Object(idx_map),
    })
}

fn index_settings_for_metadata(idx: &Index) -> Value {
    let mut inner = Map::new();
    inner.insert(
        "number_of_shards".into(),
        json!(idx.num_shards().to_string()),
    );
    inner.insert(
        "number_of_replicas".into(),
        json!(idx.num_replicas().to_string()),
    );
    inner.insert("provided_name".into(), json!(idx.name));
    inner.insert(
        "creation_date".into(),
        json!(idx.creation_date.to_string()),
    );
    inner.insert("uuid".into(), json!(idx.uuid));
    inner.insert("version".into(), json!({"created": VERSION_CREATED}));
    for (k, v) in &idx.settings {
        if let Value::Object(m) = v {
            if k != "index" {
                for (k2, v2) in m {
                    inner.insert(k2.clone(), json!(settings_value_string(v2)));
                }
                continue;
            }
        }
        let key = k.strip_prefix("index.").unwrap_or(k);
        if key != "index" {
            inner.insert(key.to_string(), json!(settings_value_string(v)));
        }
    }
    json!({"index": Value::Object(inner)})
}

fn mappings_for_metadata(idx: &Index) -> Value {
    let props = idx
        .mappings
        .get("properties")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    json!({"_doc": {"properties": Value::Object(props)}})
}

fn build_routing_table(indices: &[&Index]) -> Value {
    let mut idx_map = Map::new();
    for idx in indices {
        let mut shards = Map::new();
        if !idx.closed {
            for i in 0..idx.num_shards() {
                let mut copies: Vec<Value> = vec![];
                copies.push(json!({
                    "index": idx.name,
                    "shard": i,
                    "primary": true,
                    "state": "STARTED",
                    "node": NODE_ID,
                    "allocation_id": {"id": NODE_ID},
                    "recovery_source": {"type": "PEER"},
                    "unassigned_info": Value::Null,
                }));
                for _ in 0..idx.num_replicas() {
                    copies.push(json!({
                        "index": idx.name,
                        "shard": i,
                        "primary": false,
                        "state": "UNASSIGNED",
                        "node": Value::Null,
                        "allocation_id": {"id": ""},
                        "recovery_source": {"type": "PEER"},
                        "unassigned_info": {
                            "reason": "INDEX_CREATED",
                            "at": format_utc_millis(idx.creation_date),
                            "last_allocation_status": "no_valid_shard_copy",
                        },
                    }));
                }
                shards.insert(i.to_string(), Value::Array(copies));
            }
        }
        idx_map.insert(idx.name.clone(), json!({"shards": Value::Object(shards)}));
    }
    json!({"indices": Value::Object(idx_map)})
}

fn build_routing_nodes(indices: &[&Index]) -> Value {
    let mut assigned: Vec<Value> = vec![];
    let mut unassigned: Vec<Value> = vec![];
    for idx in indices {
        if idx.closed {
            continue;
        }
        for i in 0..idx.num_shards() {
            assigned.push(json!({
                "index": idx.name, "shard": i, "primary": true, "state": "STARTED", "node": NODE_ID,
            }));
            for _ in 0..idx.num_replicas() {
                unassigned.push(json!({
                    "index": idx.name, "shard": i, "primary": false, "state": "UNASSIGNED", "node": Value::Null,
                }));
            }
        }
    }
    json!({
        "nodes": {NODE_ID: assigned},
        "unassigned": unassigned,
    })
}

fn build_blocks(indices: &[&Index]) -> Value {
    let mut blocks = Map::new();
    for idx in indices {
        if idx.closed {
            blocks.insert(
                idx.name.clone(),
                json!({
                    "4": {
                        "description": "index closed",
                        "retryable": false,
                        "levels": ["read", "write"],
                    }
                }),
            );
        }
    }
    json!({"indices": Value::Object(blocks)})
}

// -------------------------------------------------------------------- stats

pub fn handle_cluster_stats(store: &mut Store, req: &Req, _node_spec: &str) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    let names: Vec<String> = resolve_indices(store, "*", false, true, false)
        .map(|v| v.iter().map(|i| i.name.clone()).collect())
        .unwrap_or_default();
    let mut open: i64 = 0;
    let mut field_types: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut doc_count: i64 = 0;
    for name in &names {
        let idx = match store.indices.get_mut(name) {
            Some(i) => i,
            None => continue,
        };
        if idx.closed {
            continue;
        }
        open += 1;
        doc_count += idx.docs.len() as i64;
        idx.ensure_field_types();
        for ft in idx.field_types.values() {
            *field_types.entry(ft.clone()).or_insert(0) += 1;
        }
    }
    let mut ft_list: Vec<Value> = vec![];
    for (name, count) in &field_types {
        ft_list.push(json!({"name": name, "count": count, "index_count": 1}));
    }
    ft_list.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .cmp(&b.get("name").and_then(|v| v.as_str()))
    });

    let body = json!({
        "_nodes": {"total": 1, "successful": 1, "failed": 0},
        "cluster_name": CLUSTER_NAME,
        "cluster_uuid": CLUSTER_UUID,
        "timestamp": now_millis(),
        "status": "green",
        "indices": {
            "count": open,
            "shards": {
                "total": open,
                "primaries": open,
                "replication": 0,
                "index": {"shards": {"min": open, "max": open, "avg": open}},
            },
            "docs": {"count": doc_count, "deleted": 0},
            "store": {"size_in_bytes": 0, "reserved_in_bytes": 0},
            "fielddata": {"memory_size_in_bytes": 0, "evictions": 0},
            "query_cache": {
                "memory_size_in_bytes": 0, "total_count": 0, "hit_count": 0,
                "miss_count": 0, "cache_size": 0, "cache_count": 0, "evictions": 0,
            },
            "completion": {"size_in_bytes": 0},
            "segments": {
                "count": open, "memory_in_bytes": 0, "terms_memory_in_bytes": 0,
                "stored_fields_memory_in_bytes": 0, "term_vectors_memory_in_bytes": 0,
                "norms_memory_in_bytes": 0, "points_memory_in_bytes": 0,
                "doc_values_memory_in_bytes": 0, "index_writer_memory_in_bytes": 0,
                "version_map_memory_in_bytes": 0, "fixed_bit_set_memory_in_bytes": 0,
            },
            "mappings": {"field_types": ft_list},
            "analysis": {
                "char_filter_types": [], "tokenizer_types": [], "filter_types": [],
                "analyzer_types": [], "built_in_char_filters": [], "built_in_tokenizers": [],
                "built_in_filters": [], "built_in_analyzers": [],
            },
        },
        "nodes": {
            "count": {"total": 1, "data": 1, "cluster_manager": 1, "ingest": 1},
            "versions": [NODE_VERSION],
            "os": {
                "available_processors": 8, "allocated_processors": 8,
                "names": [], "pretty_names": [],
                "mem": {"total_in_bytes": 1},
            },
            "process": {
                "cpu": {"percent": 0},
                "open_file_descriptors": {"min": 0, "max": 0, "avg": 0},
            },
            "jvm": {
                "max_uptime_in_millis": 1, "versions": [],
                "mem": {"heap_used_in_bytes": 1, "heap_max_in_bytes": 1},
                "threads": 1,
            },
            "fs": {"total_in_bytes": 1, "free_in_bytes": 1, "available_in_bytes": 1},
            "plugins": [],
        },
    });
    json_response(200, body)
}

// ---------------------------------------------------------- cluster settings

const CLUSTER_SETTING_PREFIXES: &[&str] = &[
    "cluster.",
    "indices.",
    "index.",
    "search.",
    "discovery.",
    "node.",
    "action.",
    "gateway.",
    "http.",
    "transport.",
    "thread_pool.",
    "path.",
    "bootstrap.",
    "script.",
    "snapshot.",
    "repositories.",
    "ingest.",
    "plugins.",
];

const CLUSTER_SETTING_EXACT: &[&str] = &[
    "cluster.name",
    "discovery.type",
    "node.name",
    "node.roles",
    "search.max_open_scroll_context",
    "indices.query.bool.max_clause_count",
    "search.max_keep_alive",
    "indices.recovery.max_bytes_per_sec",
    "cluster.max_shards_per_node",
    "cluster.routing.allocation.enable",
];

fn is_known_cluster_setting(key: &str) -> bool {
    if CLUSTER_SETTING_EXACT.contains(&key) {
        return true;
    }
    if key.starts_with("index.") {
        return false;
    }
    CLUSTER_SETTING_PREFIXES.iter().any(|p| key.starts_with(p))
}

fn cluster_defaults_for() -> Map<String, Value> {
    let mut d = Map::new();
    d.insert("cluster.name".into(), json!(CLUSTER_NAME));
    d.insert("discovery.type".into(), json!("single-node"));
    d.insert("search.max_open_scroll_context".into(), json!("500"));
    d.insert(
        "indices.query.bool.max_clause_count".into(),
        json!("1024"),
    );
    d.insert("search.max_keep_alive".into(), json!("24h"));
    d.insert(
        "node.roles".into(),
        json!(["ingest", "remote_cluster_client", "data", "cluster_manager"]),
    );
    d.insert("cluster.max_shards_per_node".into(), json!("1000"));
    d.insert("node.name".into(), json!(NODE_NAME));
    d
}

fn flatten_map(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
    if let Value::Object(m) = v {
        for (k, val) in m {
            flatten_map(&format!("{}{}.", prefix, k), val, out);
        }
        return;
    }
    let key = prefix.strip_suffix('.').unwrap_or(prefix);
    out.insert(key.to_string(), v.clone());
}

pub fn handle_cluster_settings(store: &mut Store, req: &Req) -> Response {
    match req.method.as_str() {
        "GET" | "HEAD" => {
            let flat = req.bool_param("flat_settings");
            let include_defaults = req.bool_param("include_defaults");
            let mut body = Map::new();
            if flat {
                body.insert(
                    "persistent".into(),
                    Value::Object(store.cluster_persistent.clone()),
                );
                body.insert(
                    "transient".into(),
                    Value::Object(store.cluster_transient.clone()),
                );
            } else {
                body.insert(
                    "persistent".into(),
                    settings_to_nested(&store.cluster_persistent),
                );
                body.insert(
                    "transient".into(),
                    settings_to_nested(&store.cluster_transient),
                );
            }
            if include_defaults {
                let mut defs = cluster_defaults_for();
                for k in store.cluster_persistent.keys() {
                    defs.remove(k);
                }
                for k in store.cluster_transient.keys() {
                    defs.remove(k);
                }
                if flat {
                    body.insert("defaults".into(), Value::Object(defs));
                } else {
                    body.insert("defaults".into(), settings_to_nested(&defs));
                }
            }
            json_response(200, Value::Object(body))
        }
        "PUT" => handle_cluster_settings_put(store, req),
        _ => method_not_allowed(req, &["PUT", "GET"]),
    }
}

fn handle_cluster_settings_put(store: &mut Store, req: &Req) -> Response {
    let ctype = req
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if req.body.is_empty() {
        return err_response(ApiError::new(
            "parse_exception",
            "request body is required",
            400,
        ));
    }
    let base = ctype.split(';').next().unwrap_or("").trim().to_string();
    let body: Option<Map<String, Value>> = match base.as_str() {
        "application/json" | "application/x-ndjson" => match serde_json::from_slice::<Value>(&req.body)
        {
            Ok(Value::Object(m)) => Some(m),
            Ok(_) => {
                return err_response(ApiError::new(
                    "json_parse_exception",
                    "cannot deserialize value of type map",
                    400,
                ))
            }
            Err(e) => {
                return err_response(ApiError::new(
                    "json_parse_exception",
                    &e.to_string(),
                    400,
                ))
            }
        },
        "application/yaml" | "application/x-yaml" | "text/yaml" => {
            Some(parse_simple_yaml(&String::from_utf8_lossy(&req.body)))
        }
        _ => None,
    };

    let persistent_raw = body.as_ref().and_then(|b| b.get("persistent")).cloned();
    let transient_raw = body.as_ref().and_then(|b| b.get("transient")).cloned();
    let non_empty = |v: &Option<Value>| matches!(v, Some(Value::Object(m)) if !m.is_empty());
    if !non_empty(&persistent_raw) && !non_empty(&transient_raw) {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: no settings to update;",
            400,
        ));
    }
    if let Err(e) = apply_cluster_settings(store, "persistent", &persistent_raw) {
        return err_response(e);
    }
    if let Err(e) = apply_cluster_settings(store, "transient", &transient_raw) {
        return err_response(e);
    }
    store.bump_state();

    if ctype == "application/yaml" {
        return crate::errors::text_response(
            200,
            "application/yaml",
            "---\nacknowledged: true\n".to_string(),
        );
    }

    let flat = req.bool_param("flat_settings");
    let mut resp = Map::new();
    resp.insert("acknowledged".into(), json!(true));
    if flat {
        resp.insert(
            "persistent".into(),
            Value::Object(store.cluster_persistent.clone()),
        );
        resp.insert(
            "transient".into(),
            Value::Object(store.cluster_transient.clone()),
        );
    } else {
        resp.insert(
            "persistent".into(),
            settings_to_nested(&store.cluster_persistent),
        );
        resp.insert(
            "transient".into(),
            settings_to_nested(&store.cluster_transient),
        );
    }
    json_response(200, Value::Object(resp))
}

fn apply_cluster_settings(
    store: &mut Store,
    scope: &str,
    raw: &Option<Value>,
) -> Result<(), ApiError> {
    let raw_map = match raw {
        Some(Value::Object(m)) => m,
        _ => return Ok(()),
    };
    let mut candidates = Map::new();
    flatten_map("", &Value::Object(raw_map.clone()), &mut candidates);
    let mut ops: Vec<(String, Option<String>)> = vec![];
    for (key, val) in &candidates {
        if !is_known_cluster_setting(key) {
            return Err(ApiError::new(
                "settings_exception",
                &format!("{} setting [{}], not recognized", scope, key),
                400,
            ));
        }
        if val.is_null() {
            ops.push((key.clone(), None));
            continue;
        }
        let mut str_val = settings_value_string(val);
        if key == "cluster.max_shards_per_node" {
            if str_val.parse::<i64>().is_err() {
                return Err(ApiError::bad(&format!(
                    "Failed to parse value [{}] for setting [cluster.max_shards_per_node]",
                    str_val
                )));
            }
        } else if key == "cluster.routing.allocation.enable" {
            let valid = matches!(
                str_val.as_str(),
                "all" | "primaries" | "new_primaries" | "none"
            );
            if !valid {
                return Err(ApiError::bad(&format!(
                    "Illegal allocation.enable value [{}]",
                    str_val.to_uppercase()
                )));
            }
        }
        ops.push((key.clone(), Some(str_val)));
    }
    let target = if scope == "persistent" {
        &mut store.cluster_persistent
    } else {
        &mut store.cluster_transient
    };
    for (key, val) in ops {
        match val {
            None => {
                target.remove(&key);
            }
            Some(s) => {
                target.insert(key, Value::String(s));
            }
        }
    }
    Ok(())
}

// -------------------------------------------------------- allocation explain

pub fn handle_allocation_explain(store: &mut Store, req: &Req) -> Response {
    if req.method != "GET" && req.method != "POST" {
        return method_not_allowed(req, &["GET", "POST"]);
    }
    let mut body: Map<String, Value> = Map::new();
    if !req.body.is_empty() {
        match serde_json::from_slice::<Value>(&req.body) {
            Ok(Value::Object(m)) => body = m,
            Ok(_) => {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    "cannot deserialize value of type map",
                    400,
                ))
            }
            Err(e) => {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    &e.to_string(),
                    400,
                ))
            }
        }
    }
    for k in body.keys() {
        if !matches!(
            k.as_str(),
            "index" | "shard" | "primary" | "current_node" | "include_yes_decisions"
                | "include_disk_info"
        ) {
            return err_response(ApiError::new(
                "x_content_parse_exception",
                &format!("[1:2] [cluster/allocation/explain] unknown field [{}]", k),
                400,
            ));
        }
    }

    let mut index_name = body
        .get("index")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut shard_num: i64 = -1;
    let mut primary = false;
    let mut has_shard = false;
    if let Some(v) = body.get("shard") {
        if let Some(f) = to_f64(v) {
            shard_num = f as i64;
        }
        has_shard = true;
    }
    if let Some(v) = body.get("primary") {
        primary = v.as_bool().unwrap_or(false);
    }
    if index_name.is_empty() {
        for name in sorted_index_names(store) {
            let idx = &store.indices[&name];
            if idx.closed {
                continue;
            }
            let h = compute_index_health(idx);
            if h.unassigned > 0 {
                index_name = idx.name.clone();
                shard_num = 0;
                primary = false;
                has_shard = true;
                break;
            }
        }
        if index_name.is_empty() {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                "unable to find any unassigned shards to explain",
                400,
            ));
        }
    }
    if !store.indices.contains_key(&index_name) {
        return err_response(err_index_not_found_named(&index_name));
    }
    if !has_shard || shard_num < 0 {
        return err_response(ApiError::new(
            "action_request_validation_exception",
            "Validation Failed: 1: shard must be specified;2: primary must be specified;",
            400,
        ));
    }
    let idx = &store.indices[&index_name];
    if shard_num >= idx.num_shards() {
        let mut e = ApiError::new(
            "shard_not_found_exception",
            &format!("no shard with id [{}]", shard_num),
            404,
        );
        e.extra = Some(Value::String(shard_num.to_string()));
        return err_response(e);
    }
    if primary {
        return json_response(
            200,
            json!({
                "index": idx.name,
                "shard": shard_num,
                "primary": true,
                "current_state": "started",
                "current_node": {
                    "id": NODE_ID,
                    "name": NODE_NAME,
                    "ephemeral_id": NODE_EPHEMERAL,
                    "transport_address": NODE_TRANSPORT,
                    "attributes": node_attributes(),
                },
                "can_remain_on_current_node": "yes",
                "can_rebalance_cluster": "yes",
                "can_rebalance_to_other_node": "no",
                "rebalance_explanation": "no attempt to rebalance",
                "can_allocate": "no",
                "allocate_explanation": "the shard is already allocated",
            }),
        );
    }
    json_response(
        200,
        json!({
            "index": idx.name,
            "shard": shard_num,
            "primary": false,
            "current_state": "unassigned",
            "unassigned_info": {
                "reason": "INDEX_CREATED",
                "at": format_utc_millis(idx.creation_date),
                "last_allocation_status": "no_valid_shard_copy",
            },
            "can_allocate": "no",
            "allocate_explanation": "cannot allocate because a previous copy of the primary shard existed but can no longer be found on the nodes in the cluster",
            "node_allocation_decisions": [{
                "node_id": NODE_ID,
                "node_name": NODE_NAME,
                "transport_address": NODE_TRANSPORT,
                "node_attributes": node_attributes(),
                "node_decision": "no",
                "weight_ranking": 1,
                "deciders": [{
                    "decider": "same_shard",
                    "decision": "NO",
                    "explanation": "a copy of this shard is already allocated to this node [[%s][%d], node[%s], [P], s[STARTED]]",
                }],
            }],
        }),
    )
}

// --------------------------------------------------------------------- misc

pub fn handle_pending_tasks(_store: &mut Store, req: &Req) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    json_response(200, json!({"tasks": []}))
}

pub fn handle_remote_info(_store: &mut Store, req: &Req) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    json_response(200, json!({}))
}

pub fn snapshot_state(_store: &mut Store) -> Value {
    Value::Object(Map::new())
}
