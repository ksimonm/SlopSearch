use crate::errors::{json_response, no_handler};
use crate::{Req, Store};
use axum::response::Response;
use serde_json::{json, Value};

/// Routes `_cluster`, `_nodes`, `_tasks`, `_remote`, `_template`,
/// `_index_template`, `_component_template`, `_data_stream`, `_resolve`,
/// `_reindex`. Returns None when not handled. Mirrors Go routeServerEndpoints.
pub fn route_server_endpoints(store: &mut Store, req: &Req, segs: &[String]) -> Option<Response> {
    match segs[0].as_str() {
        "_ingest" => {
            return Some(crate::ingest::handle_ingest(store, req, &segs[1..]));
        }
        "_scripts" => {
            return Some(crate::scripts::handle_scripts(store, req, &segs[1..]));
        }
        "_script_context" if segs.len() == 1 => {
            return Some(crate::errors::json_response(
                200,
                crate::scripts::script_context_response(),
            ));
        }
        "_script_language" if segs.len() == 1 => {
            return Some(crate::errors::json_response(
                200,
                crate::scripts::script_language_response(),
            ));
        }
        "_cluster" => {
            if segs.len() == 1 {
                return Some(no_handler(&req.method, &req.path));
            }
            let resp = match segs[1].as_str() {
                "health" => {
                    let spec = if segs.len() == 3 {
                        segs[2].as_str()
                    } else if segs.len() > 3 {
                        return Some(no_handler(&req.method, &req.path));
                    } else {
                        ""
                    };
                    Some(crate::cluster::handle_health(store, req, spec))
                }
                "state" => {
                    let (metrics, spec) = match segs.len() {
                        3 => (segs[2].as_str(), ""),
                        4 => (segs[2].as_str(), segs[3].as_str()),
                        _ if segs.len() > 4 => return Some(no_handler(&req.method, &req.path)),
                        _ => ("", ""),
                    };
                    Some(crate::cluster::handle_cluster_state(
                        store, req, metrics, spec,
                    ))
                }
                "stats" => {
                    let spec = if segs.len() == 3 {
                        segs[2].clone()
                    } else if segs.len() == 4 && segs[2] == "nodes" {
                        segs[3].clone()
                    } else if segs.len() > 3 {
                        return Some(no_handler(&req.method, &req.path));
                    } else {
                        String::new()
                    };
                    Some(crate::cluster::handle_cluster_stats(store, req, &spec))
                }
                "settings" => {
                    if segs.len() > 2 {
                        return Some(no_handler(&req.method, &req.path));
                    }
                    Some(crate::cluster::handle_cluster_settings(store, req))
                }
                "allocation" => {
                    if segs.len() == 3 && segs[2] == "explain" {
                        Some(crate::cluster::handle_allocation_explain(store, req))
                    } else {
                        Some(no_handler(&req.method, &req.path))
                    }
                }
                "pending_tasks" => Some(crate::cluster::handle_pending_tasks(store, req)),
                "voting_config_exclusions" => {
                    Some(json_response(200, Value::Object(serde_json::Map::new())))
                }
                "reroute" => Some(json_response(
                    200,
                    json!({"acknowledged": true, "state": crate::cluster::snapshot_state(store)}),
                )),
                _ => Some(no_handler(&req.method, &req.path)),
            };
            return resp;
        }
        "_nodes" => return Some(crate::nodes::handle_nodes(store, req, &segs[1..])),
        "_tasks" => return Some(crate::nodes::handle_tasks(store, req, &segs[1..])),
        "_remote" => {
            return Some(if segs.len() == 2 && segs[1] == "info" {
                crate::cluster::handle_remote_info(store, req)
            } else {
                no_handler(&req.method, &req.path)
            });
        }
        "_template" => return Some(crate::templates::handle_legacy_template(store, req, &segs[1..])),
        "_index_template" => {
            return Some(
                if segs.len() >= 2 && (segs[1] == "_simulate" || segs[1] == "_simulate_index") {
                    crate::templates::handle_index_template_simulate(store, req, &segs[1..])
                } else {
                    crate::templates::handle_composable_template(store, req, &segs[1..])
                },
            );
        }
        "_component_template" => {
            return Some(crate::templates::handle_component_template(store, req, &segs[1..]));
        }
        "_data_stream" => {
            return Some(match segs.len() {
                1 => crate::datastreams::handle_data_stream(store, req, "*"),
                2 => crate::datastreams::handle_data_stream(store, req, &segs[1]),
                3 if segs[2] == "_stats" => {
                    crate::datastreams::handle_data_stream_stats(store, req, &segs[1])
                }
                _ => no_handler(&req.method, &req.path),
            });
        }
        "_resolve" => {
            return Some(if segs.len() == 3 && segs[1] == "index" {
                crate::resolve_index::handle_resolve_index(store, req, &segs[2])
            } else {
                no_handler(&req.method, &req.path)
            });
        }
        "_reindex" => return Some(crate::reindex::handle_reindex(store, req)),
        _ => None,
    }
}
