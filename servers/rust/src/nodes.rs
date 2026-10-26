use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::cluster::{format_utc_millis, node_attributes, node_roles, NODE_HOST};
use crate::errors::{err_response, json_response, method_not_allowed, ApiError};

fn bare_no_handler(req: &Req) -> Response {
    json_response(
        400,
        json!({
            "error": format!("no handler found for uri [{}] and method [{}]", req.path, req.method),
        }),
    )
}
use crate::resp::glob_match;
use crate::shared::resolve_indices;
use crate::store::*;
use crate::{Req, Store};
use axum::response::Response;

const NODE_TRANSPORT: &str = "127.0.0.1:9300";

const NODE_INFO_METRICS: &[&str] = &[
    "all",
    "settings",
    "os",
    "process",
    "jvm",
    "thread_pool",
    "transport",
    "http",
    "plugins",
    "ingest",
    "aggregations",
    "indices",
    "search_pipelines",
    "_none",
    "usage",
    "stats",
    "hot_threads",
];

const NODE_STATS_METRICS: &[&str] = &[
    "all",
    "jvm",
    "os",
    "process",
    "fs",
    "indices",
    "http",
    "transport",
    "thread_pool",
    "ingest",
    "script",
    "discovery",
    "breaker",
    "adaptive_selection",
    "_all",
    "indexing_pressure",
    "_none",
];

const NODE_INDEX_STATS_METRICS: &[&str] = &[
    "all",
    "docs",
    "store",
    "indexing",
    "get",
    "search",
    "merge",
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
    "suggest",
    "status_counter",
];

const MODULE_NAMES: &[&str] = &[
    "aggs-matrix-stats",
    "analysis-common",
    "geo",
    "ingest-common",
    "lang-mustache",
    "lang-painless",
    "mapper-extras",
    "parent-join",
    "percolator",
    "rank-eval",
    "reindex",
    "search-pipeline-common",
];

fn node_info_base() -> Map<String, Value> {
    json!({
        "name": NODE_NAME,
        "transport_address": NODE_TRANSPORT,
        "host": NODE_HOST,
        "ip": NODE_HOST,
        "version": NODE_VERSION,
        "build_type": "tar",
        "build_hash": "0000000000000000000000000000000000000000",
        "roles": node_roles(),
        "attributes": node_attributes(),
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

pub const PLUGIN_NAMES: &[&str] = &[
    "opensearch-alerting",
    "opensearch-anomaly-detection",
    "opensearch-asynchronous-search",
    "opensearch-cross-cluster-replication",
    "opensearch-custom-codecs",
    "opensearch-flow-framework",
    "opensearch-geospatial",
    "opensearch-index-management",
    "opensearch-job-scheduler",
    "opensearch-knn",
    "opensearch-ltr",
    "opensearch-ml",
    "opensearch-neural-search",
    "opensearch-notifications",
    "opensearch-notifications-core",
    "opensearch-observability",
    "opensearch-performance-analyzer",
    "opensearch-reports-scheduler",
    "opensearch-search-relevance",
    "opensearch-security",
    "opensearch-security-analytics",
    "opensearch-skills",
    "opensearch-sql",
    "opensearch-system-templates",
    "opensearch-ubi",
    "query-insights",
];

pub fn plugins_list() -> Vec<Value> {
    PLUGIN_NAMES
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "version": "3.9.0.0",
                "opensearch_version": NODE_VERSION,
                "java_version": "25",
                "description": format!("{} plugin", name),
                "classname": format!("org.opensearch.plugin.{}", name.replace('-', ".")),
                "has_native_controller": false,
                "licensed": false,
                "type": "plugin",
            })
        })
        .collect()
}

fn modules_list() -> Vec<Value> {
    MODULE_NAMES
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "version": NODE_VERSION,
                "opensearch_version": NODE_VERSION,
                "java_version": "25",
                "description": format!("{} module", name),
                "classname": format!("org.opensearch.{}", name),
                "has_native_controller": false,
            })
        })
        .collect()
}

fn node_info_for_metric(metric: &str) -> Map<String, Value> {
    let v: Value = match metric {
        "settings" => json!({
            "settings": {
                "cluster": {"name": CLUSTER_NAME},
                "node": {"name": NODE_NAME, "attr": {"shard_indexing_pressure_enabled": "true"}},
                "path": {"home": "/tmp/os", "data": [], "logs": "/tmp/logs"},
            }
        }),
        "os" => json!({
            "os": {
                "refresh_interval_in_millis": 1000,
                "name": "Linux",
                "pretty_name": "Linux",
                "arch": "amd64",
                "version": "6.0.0",
                "available_processors": 8,
                "allocated_processors": 8,
                "cpu": {"percent": 0},
                "mem": {"total_in_bytes": 1, "free_in_bytes": 1, "used_in_bytes": 0, "free_percent": 100, "used_percent": 0},
                "swap": {"total_in_bytes": 1, "free_in_bytes": 1, "used_in_bytes": 0},
            }
        }),
        "process" => json!({
            "process": {
                "refresh_interval_in_millis": 1000,
                "id": 1,
                "mlockall": false,
                "cpu": {"percent": 0, "total_in_millis": 0},
                "mem": {"total_virtual_in_bytes": 1},
                "open_file_descriptors": 100,
                "max_file_descriptors": 65535,
            }
        }),
        "jvm" => json!({"jvm": node_jvm_info(false)}),
        "thread_pool" => json!({"thread_pool": node_thread_pools()}),
        "transport" => json!({
            "transport": {
                "bound_address": [NODE_TRANSPORT],
                "publish_address": NODE_TRANSPORT,
                "profiles": {},
            }
        }),
        "http" => json!({
            "http": {
                "bound_address": [http_publish_address()],
                "publish_address": http_publish_address(),
                "max_content_length_in_bytes": 104857600,
            }
        }),
        "plugins" => json!({"plugins": plugins_list(), "modules": modules_list()}),
        "ingest" => {
                    let procs: Vec<Value> = crate::ingest::PROCESSOR_TYPES
                        .iter()
                        .map(|t| json!({"type": t}))
                        .collect();
                    json!({"ingest": {"processors": procs}})
                },
        "aggregations" => json!({"aggregations": {}}),
        "search_pipelines" => json!({
            "search_pipelines": {"request_processors": [], "response_processors": []}
        }),
        "indices" => json!({"total_indexing_buffer_in_bytes": 1073741824}),
        _ => json!({}),
    };
    v.as_object().cloned().unwrap_or_default()
}

fn http_port_num() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(9200)
}

fn http_publish_address() -> String {
    format!("{}:{}", NODE_HOST, http_port_num())
}

fn node_jvm_info(human: bool) -> Value {
    let mut mem = json!({"heap_max_in_bytes": 1073741824, "heap_used_in_bytes": 0});
    if human {
        mem["heap_max"] = json!("1gb");
    }
    json!({
        "pid": 1234,
        "version": "25",
        "vm_name": "OpenJDK 64-Bit Server VM",
        "vm_version": "25",
        "vm_vendor": "Eclipse Adoptium",
        "bundled_jdk": true,
        "using_bundled_jdk": true,
        "start_time_in_millis": now_millis(),
        "mem": mem,
        "gc_collectors": ["G1 Young Generation", "G1 Old Generation"],
        "memory_pools": ["G1 Eden Space", "G1 Old Gen", "G1 Survivor Space"],
        "input_arguments": ["-Xms1g", "-Xmx1g"],
    })
}

pub(crate) fn node_thread_pools() -> Value {
    let pool = |kind: &str, size: i64, queue: i64| {
        json!({
            "type": kind, "size": size, "queue_size": queue, "active": 0, "queue": 0,
            "rejected": 0, "largest": 0, "completed": 0,
        })
    };
    json!({
        "search": pool("resizable", 13, 1000),
        "write": pool("fixed", 8, 10000),
        "get": pool("fixed", 8, 1000),
        "management": pool("scaling", 2, 10000),
        "generic": pool("scaling", 4, 10000),
        "analyze": pool("fixed", 1, 16),
        "snapshot": pool("scaling", 1, 1000),
        "refresh": pool("scaling", 2, 1000),
        "flush": pool("scaling", 1, 1000),
        "fetch_shard_started": pool("scaling", 2, 1000),
        "fetch_shard_store": pool("scaling", 2, 1000),
        "warmer": pool("scaling", 2, 1000),
        "force_merge": pool("fixed", 1, 1000),
        "system_read": pool("fixed", 4, 1000),
        "system_write": pool("fixed", 4, 1000),
        "system_critical_read": pool("fixed", 4, 1000),
        "system_critical_write": pool("fixed", 4, 1000),
    })
}

fn empty_nodes_response() -> Value {
    json!({
        "_nodes": {"total": 0, "successful": 0, "failed": 0},
        "cluster_name": CLUSTER_NAME,
        "nodes": {},
    })
}

pub fn handle_nodes(_store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    let mut selector = String::new();
    let mut rest2: &[String] = rest;
    if !rest.is_empty() {
        let first = rest[0].as_str();
        let is_endpoint = first == "stats" || first == "usage" || first == "hot_threads";
        if !is_endpoint && !NODE_INFO_METRICS.contains(&first) {
            selector = first.to_string();
            rest2 = &rest[1..];
        }
    }
    if !rest2.is_empty() {
        match rest2[0].as_str() {
            "stats" => {
                if rest2.len() > 3 {
                    return bare_no_handler(req);
                }
                let metric = rest2.get(1).map(|s| s.as_str()).unwrap_or("");
                let index_metric = rest2.get(2).map(|s| s.as_str()).unwrap_or("");
                return handle_node_stats(_store, req, &selector, metric, index_metric);
            }
            "usage" => return handle_node_usage(req),
            "hot_threads" => return handle_hot_threads(req),
            _ => {}
        }
    }
    if rest2.len() > 1 {
        return bare_no_handler(req);
    }
    if !selector.is_empty() && !nodes_selector_matches(&selector) {
        return json_response(200, empty_nodes_response());
    }
    let metric = if rest2.is_empty() { "" } else { rest2[0].as_str() };
    write_node_info(req, metric)
}

fn nodes_selector_matches(selector: &str) -> bool {
    for tok in selector.split(',') {
        let token = tok.trim();
        match token {
            "_local" | "_all" | "*" | "_cluster_manager" | "_master" | "data:true"
            | "cluster_manager:true" | "master:true" | "ingest:true" => return true,
            _ => {}
        }
        if token == NODE_ID || token == NODE_NAME {
            return true;
        }
        if token.starts_with('_') {
            continue;
        }
        if (token.contains('*') || token.contains('?'))
            && (glob_match(token, NODE_ID) || glob_match(token, NODE_NAME))
        {
            return true;
        }
    }
    false
}

fn write_node_info(req: &Req, metric: &str) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    let defaults = [
        "settings",
        "os",
        "process",
        "jvm",
        "thread_pool",
        "transport",
        "http",
        "plugins",
        "ingest",
        "aggregations",
        "search_pipelines",
        "indices",
    ];
    let metrics: Vec<String> = if metric.is_empty() || metric == "all" {
        defaults.iter().map(|s| s.to_string()).collect()
    } else {
        metric
            .split(',')
            .filter(|m| {
                NODE_INFO_METRICS.contains(m)
                    && *m != "all"
                    && *m != "_none"
                    && *m != "usage"
                    && *m != "stats"
                    && *m != "hot_threads"
            })
            .map(|s| s.to_string())
            .collect()
    };
    let mut info = node_info_base();
    let human = req.bool_param("human");
    for m in &metrics {
        if m == "jvm" {
            info.insert("jvm".to_string(), node_jvm_info(human));
            continue;
        }
        for (k, v) in node_info_for_metric(m) {
            info.insert(k, v);
        }
    }
    json_response(
        200,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": CLUSTER_NAME,
            "nodes": {NODE_ID: Value::Object(info)},
        }),
    )
}

fn handle_node_stats(
    store: &mut Store,
    req: &Req,
    selector: &str,
    metric: &str,
    index_metric: &str,
) -> Response {
    if !selector.is_empty() && !nodes_selector_matches(selector) {
        return json_response(200, empty_nodes_response());
    }
    let all_default = [
        "indices",
        "os",
        "process",
        "jvm",
        "thread_pool",
        "fs",
        "transport",
        "http",
        "breakers",
        "script",
        "discovery",
        "ingest",
    ];
    let mut metrics: Vec<String> = vec![];
    let explicit_metrics = !metric.is_empty();
    let mut index_metric = index_metric.to_string();
    if explicit_metrics {
        for m in metric.split(',') {
            if !NODE_STATS_METRICS.contains(&m) {
                if m == "breakers" {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!(
                            "request [{}] contains unrecognized metric: [{}] -> did you mean [breaker]?",
                            req.path, m
                        ),
                        400,
                    ));
                }
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "request [{}] contains unrecognized metric: [{}]",
                        req.path, m
                    ),
                    400,
                ));
            }
        }
        if metric == "_all" || metric == "all" {
            if !index_metric.is_empty() {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!(
                        "request [{}] contains index metrics [{}] but all stats requested",
                        req.path, index_metric
                    ),
                    400,
                ));
            }
            metrics = all_default.iter().map(|s| s.to_string()).collect();
        } else {
            for m in metric.split(',') {
                if m != "_none" {
                    metrics.push(m.to_string());
                }
            }
        }
    } else {
        metrics = all_default.iter().map(|s| s.to_string()).collect();
    }
    if !index_metric.is_empty() {
        if index_metric == "_all" || index_metric == "all" {
            index_metric = String::new();
        } else {
            for m in index_metric.split(',') {
                if !NODE_INDEX_STATS_METRICS.contains(&m) {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!(
                            "request [{}] contains unrecognized index metric: [{}]",
                            req.path, m
                        ),
                        400,
                    ));
                }
            }
            if !metrics.iter().any(|m| m == "indices") {
                metrics.push("indices".to_string());
            }
        }
    }
    let level = req.param("level").unwrap_or("").to_string();
    if !level.is_empty() && level != "node" && level != "indices" && level != "shards" {
        return json_response(
            200,
            json!({
                "_nodes": {
                    "total": 1, "successful": 0, "failed": 1,
                    "failures": [{
                        "type": "failed_node_exception",
                        "reason": format!("failed to execute stats on node [{}]", NODE_ID),
                        "node_id": NODE_ID,
                        "caused_by": {
                            "type": "illegal_argument_exception",
                            "reason": format!("level [{}] is not supported", level),
                        },
                    }],
                },
                "cluster_name": CLUSTER_NAME,
                "nodes": {},
            }),
        );
    }
    let stats = build_node_stats(store, &metrics, &index_metric, &level);
    json_response(
        200,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": CLUSTER_NAME,
            "nodes": {NODE_ID: Value::Object(stats)},
        }),
    )
}

fn build_node_stats(
    store: &mut Store,
    metrics: &[String],
    index_metric: &str,
    level: &str,
) -> Map<String, Value> {
    let mut stats = Map::new();
    stats.insert("timestamp".into(), json!(now_millis()));
    stats.insert("name".into(), json!(NODE_NAME));
    stats.insert("transport_address".into(), json!(NODE_TRANSPORT));
    stats.insert("host".into(), json!(NODE_HOST));
    stats.insert("ip".into(), json!(NODE_HOST));
    stats.insert("roles".into(), node_roles());
    stats.insert("attributes".into(), node_attributes());

    let mut want: Vec<&str> = metrics.iter().map(|s| s.as_str()).collect();
    if want.contains(&"breaker") {
        want.push("breakers");
    }
    let has = |m: &str| want.contains(&m);

    if has("indices") {
        let names: Vec<String> = resolve_indices(store, "*", false, true, false)
            .map(|v| v.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default();
        let mut docs_total: i64 = 0;
        let deleted_total: i64 = 0;
        let mut per_index = Map::new();
        for name in &names {
            let idx = match store.indices.get(name) {
                Some(i) => i,
                None => continue,
            };
            if idx.closed {
                continue;
            }
            docs_total += idx.docs.len() as i64;
            per_index.insert(
                name.clone(),
                json!({"docs": {"count": idx.docs.len() as i64, "deleted": 0}}),
            );
        }
        let mut idx_stats = Map::new();
        idx_stats.insert(
            "docs".into(),
            json!({"count": docs_total, "deleted": deleted_total}),
        );
        idx_stats.insert(
            "store".into(),
            json!({"size_in_bytes": docs_total * 64, "reserved_in_bytes": 0}),
        );
        idx_stats.insert(
            "status_counter".into(),
            json!({"total": 0, "successful": 0, "failed": 0}),
        );
        if !index_metric.is_empty() && !index_metric.contains("_all") {
            let mut only = Map::new();
            for m in index_metric.split(',') {
                if let Some(v) = idx_stats.get(m) {
                    only.insert(m.to_string(), v.clone());
                }
            }
            only.insert(
                "status_counter".into(),
                idx_stats.get("status_counter").cloned().unwrap_or(json!({})),
            );
            idx_stats = only;
        }
        if level == "indices" {
            idx_stats.insert("indices".into(), Value::Object(per_index));
        }
        stats.insert("indices".into(), Value::Object(idx_stats));
    }
    if has("os") {
        stats.insert(
            "os".into(),
            json!({
                "timestamp": now_millis(),
                "cpu": {"percent": 0, "load_average": {"1m": 0.0, "5m": 0.0, "15m": 0.0}},
                "mem": {"total_in_bytes": 1, "free_in_bytes": 1, "used_in_bytes": 0, "free_percent": 100, "used_percent": 0},
                "swap": {"total_in_bytes": 1, "free_in_bytes": 1, "used_in_bytes": 0},
            }),
        );
    }
    if has("process") {
        stats.insert(
            "process".into(),
            json!({
                "timestamp": now_millis(),
                "open_file_descriptors": 100,
                "max_file_descriptors": 65535,
                "cpu": {"percent": 0, "total_in_millis": 0},
                "mem": {"total_virtual_in_bytes": 1},
            }),
        );
    }
    if has("jvm") {
        stats.insert(
            "jvm".into(),
            json!({
                "timestamp": now_millis(),
                "uptime_in_millis": 1000,
                "mem": {"heap_used_in_bytes": 0, "heap_used_percent": 0, "heap_max_in_bytes": 1073741824},
                "threads": {"count": 50, "peak_count": 60},
                "gc": {"collectors": {"young": {"collection_count": 0}, "old": {"collection_count": 0}}},
                "buffer_pools": {},
                "classes": {"current_loaded_count": 1000, "total_loaded_count": 1000, "total_unloaded_count": 0},
            }),
        );
    }
    if has("thread_pool") {
        stats.insert("thread_pool".into(), node_thread_pools());
    }
    if has("fs") {
        stats.insert(
            "fs".into(),
            json!({
                "timestamp": now_millis(),
                "total": {"total_in_bytes": 1, "free_in_bytes": 1, "available_in_bytes": 1},
                "data": [{"path": "/tmp/os", "mount": "/", "type": "ext4", "total_in_bytes": 1, "free_in_bytes": 1, "available_in_bytes": 1}],
                "io_stats": {"devices": [], "total": {"operations": 0, "read_operations": 0, "write_operations": 0, "read_kilobytes": 0, "write_kilobytes": 0}},
            }),
        );
    }
    if has("transport") {
        stats.insert(
            "transport".into(),
            json!({
                "server_open": 1, "total_outbound_connections": 0, "rx_count": 0,
                "rx_size_in_bytes": 0, "tx_count": 0, "tx_size_in_bytes": 0,
            }),
        );
    }
    if has("http") {
        stats.insert("http".into(), json!({"current_open": 0, "total_opened": 1}));
    }
    if has("breakers") {
        let breaker = |limit: i64| {
            json!({
                "limit_size_in_bytes": limit, "limit_size": "1gb", "estimated_size_in_bytes": 0,
                "estimated_size": "0b", "overhead": 1.0, "tripped": 0,
            })
        };
        stats.insert(
            "breakers".into(),
            json!({
                "request": breaker(536870912),
                "fielddata": breaker(214748364),
                "in_flight_requests": breaker(1073741824),
                "parent": breaker(1000000000),
                "accounting": breaker(1073741824),
            }),
        );
    }
    if has("script") {
        stats.insert(
            "script".into(),
            json!({"compilations": 0, "cache_evictions": 0, "compilation_limit_triggered": 0}),
        );
    }
    if has("discovery") {
        stats.insert(
            "discovery".into(),
            json!({
                "cluster_state_queue": {"total": 0, "pending": 0, "committed": 0},
                "published_cluster_states": {"full_states": 0, "incompatible_diffs": 0, "compatible_diffs": 0},
            }),
        );
    }
    if has("ingest") {
        stats.insert(
            "ingest".into(),
            json!({"total": {"count": 0, "time_in_millis": 0, "current": 0, "failed": 0}}),
        );
    }
    if has("adaptive_selection") {
        stats.insert("adaptive_selection".into(), json!({}));
    }
    if has("indexing_pressure") {
        stats.insert(
            "indexing_pressure".into(),
            json!({
                "memory": {
                    "current": {"all_in_bytes": 0},
                    "total": {"all_in_bytes": 0},
                }
            }),
        );
    }
    stats
}

fn handle_node_usage(req: &Req) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    let now = now_millis();
    json_response(
        200,
        json!({
            "_nodes": {"total": 1, "successful": 1, "failed": 0},
            "cluster_name": CLUSTER_NAME,
            "nodes": {
                NODE_ID: {
                    "timestamp": now,
                    "since": now - 1000,
                    "rest_actions": {},
                    "aggregations": {},
                }
            },
        }),
    )
}

fn handle_hot_threads(req: &Req) -> Response {
    let ts = format_utc_millis(now_millis());
    let body = format!(
        "::: {{{}}}{{{}}}\n   Hot threads at {}, interval=500ms, busiestThreads=3, ignoreIdleThreads=true:\n\n",
        NODE_NAME, NODE_ID, ts
    );
    crate::errors::text_response(200, "text/plain; charset=UTF-8", body)
}

// -------------------------------------------------------------------- tasks

pub(crate) fn synthetic_tasks() -> Vec<Map<String, Value>> {
    let parent_id = "1";
    let child_id = "2";
    let now = now_millis();
    let mut parent = Map::new();
    parent.insert("node".into(), json!(NODE_ID));
    parent.insert("id".into(), json!(parent_id));
    parent.insert("type".into(), json!("transport"));
    parent.insert("action".into(), json!("cluster:monitor/tasks/lists"));
    parent.insert("start_time_in_millis".into(), json!(now));
    parent.insert("running_time_in_nanos".into(), json!(1000));
    parent.insert("cancellable".into(), json!(false));
    parent.insert("cancelled".into(), json!(false));
    parent.insert("headers".into(), json!({}));
    parent.insert("description".into(), json!("cluster:monitor/tasks/lists"));

    let mut child = Map::new();
    child.insert("node".into(), json!(NODE_ID));
    child.insert("id".into(), json!(child_id));
    child.insert("type".into(), json!("direct"));
    child.insert("action".into(), json!("cluster:monitor/tasks/lists[n]"));
    child.insert("start_time_in_millis".into(), json!(now));
    child.insert("running_time_in_nanos".into(), json!(500));
    child.insert("cancellable".into(), json!(true));
    child.insert("cancelled".into(), json!(false));
    child.insert("headers".into(), json!({}));
    child.insert(
        "parent_task_id".into(),
        json!(format!("{}:{}", NODE_ID, parent_id)),
    );
    child.insert(
        "description".into(),
        json!("cluster:monitor/tasks/lists[n]"),
    );
    vec![parent, child]
}

pub(crate) fn task_action_matches(task: &Map<String, Value>, actions: &str) -> bool {
    if actions.is_empty() || actions == "*" {
        return true;
    }
    let action = task.get("action").and_then(|v| v.as_str()).unwrap_or("");
    actions.split(',').any(|pat| glob_match(pat, action))
}

fn task_json(task: &Map<String, Value>, detailed: bool) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in task {
        if k == "description" && !detailed {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

pub fn handle_tasks(_store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if rest.len() == 1 && rest[0] == "_cancel" {
        if req.method != "POST" {
            return method_not_allowed(req, &["POST"]);
        }
        let actions = req.param("actions").unwrap_or("");
        let matched: Vec<Map<String, Value>> = synthetic_tasks()
            .into_iter()
            .filter(|t| task_action_matches(t, actions))
            .collect();
        if matched.is_empty() {
            return json_response(200, json!({"nodes": {}}));
        }
        let mut tasks_out = Map::new();
        for t in &matched {
            let key = format!(
                "{}:{}",
                t.get("node").and_then(|v| v.as_str()).unwrap_or(""),
                t.get("id").and_then(|v| v.as_str()).unwrap_or("")
            );
            tasks_out.insert(
                key,
                json!({"action": t.get("action").cloned().unwrap_or(Value::Null), "cancelled": true}),
            );
        }
        return json_response(
            200,
            json!({
                "nodes": {NODE_ID: {"name": NODE_NAME, "tasks": tasks_out}},
            }),
        );
    }
    if req.method != "GET" && req.method != "HEAD" {
        return method_not_allowed(req, &["GET"]);
    }
    if rest.len() == 1 {
        let id = &rest[0];
        if !id.contains(':') {
            return err_response(ApiError::new(
                "illegal_argument_exception",
                &format!("malformed task id {}", id),
                400,
            ));
        }
        let parts: Vec<&str> = id.splitn(2, ':').collect();
        if parts[0] != NODE_ID {
            return err_response(ApiError::new(
                "resource_not_found_exception",
                &format!(
                    "task [{}] belongs to the node [{}] which is not part of the cluster",
                    id, parts[0]
                ),
                404,
            ));
        }
        return err_response(ApiError::new(
            "resource_not_found_exception",
            &format!("task [{}] isn't running and hasn't stored its results", id),
            404,
        ));
    }
    let group_by = match req.param("group_by") {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => "nodes".to_string(),
    };
    if group_by != "nodes" && group_by != "parents" && group_by != "none" {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            &format!(
                "[group_by] must be one of [nodes], [parents] or [none] but was [{}]",
                group_by
            ),
            400,
        ));
    }
    let detailed = req.bool_param("detailed");
    let actions = req.param("actions").unwrap_or("");
    let matched: Vec<Map<String, Value>> = synthetic_tasks()
        .into_iter()
        .filter(|t| task_action_matches(t, actions))
        .collect();
    if matched.is_empty() {
        return json_response(200, json!({"nodes": {}}));
    }
    match group_by.as_str() {
        "none" => {
            let list: Vec<Value> = matched
                .iter()
                .map(|t| Value::Object(task_json(t, detailed)))
                .collect();
            json_response(200, json!({"tasks": list}))
        }
        "parents" => {
            let mut parents = Map::new();
            let mut children: HashMap<String, Vec<Value>> = HashMap::new();
            for t in &matched {
                if let Some(pid) = t.get("parent_task_id").and_then(|v| v.as_str()) {
                    children
                        .entry(pid.to_string())
                        .or_default()
                        .push(Value::Object(task_json(t, detailed)));
                } else {
                    let key = format!(
                        "{}:{}",
                        t.get("node").and_then(|v| v.as_str()).unwrap_or(""),
                        t.get("id").and_then(|v| v.as_str()).unwrap_or("")
                    );
                    parents.insert(key, Value::Object(task_json(t, detailed)));
                }
            }
            for (pid, kids) in children {
                if let Some(parent) = parents.get_mut(&pid).and_then(|p| p.as_object_mut()) {
                    parent.insert("children".to_string(), Value::Array(kids));
                }
            }
            json_response(200, json!({"tasks": parents}))
        }
        _ => {
            let mut tasks_out = Map::new();
            for t in &matched {
                let key = format!(
                    "{}:{}",
                    t.get("node").and_then(|v| v.as_str()).unwrap_or(""),
                    t.get("id").and_then(|v| v.as_str()).unwrap_or("")
                );
                tasks_out.insert(key, Value::Object(task_json(t, detailed)));
            }
            json_response(
                200,
                json!({
                    "nodes": {NODE_ID: {"name": NODE_NAME, "tasks": tasks_out}},
                }),
            )
        }
    }
}
