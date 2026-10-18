use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::datastreams::{backing_index_name, create_backing_index};
use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::shared::sorted_index_names;
use crate::store::{settings_value_int, to_f64, value_to_string, DataStreamInfo, Store};
use crate::Req;

fn parse_duration_millis(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            let unit = &s[digits.len()..];
            let n: i64 = digits.parse().ok()?;
            match unit {
                "ms" => Some(n),
                "s" => Some(n * 1000),
                "m" => Some(n * 60 * 1000),
                "h" => Some(n * 3600 * 1000),
                "d" => Some(n * 86400 * 1000),
                "w" => Some(n * 7 * 86400 * 1000),
                _ => None,
            }
        }
        other => to_f64(other).map(|f| f as i64),
    }
}

fn condition_key(name: &str, v: &Value) -> String {
    match v {
        Value::String(s) => format!("[{}: {}]", name, s),
        Value::Bool(b) => format!("[{}: {}]", name, b),
        Value::Null => format!("[{}: <nil>]", name),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                format!("[{}: {}]", name, i)
            } else if let Some(f) = n.as_f64() {
                format!("[{}: {}]", name, f)
            } else {
                format!("[{}: {}]", name, n)
            }
        }
        other => format!("[{}: {}]", name, value_to_string(other).unwrap_or_default()),
    }
}

fn evaluate_rollover_conditions(
    raw: &Map<String, Value>,
    names: &[String],
    idx: &crate::store::Index,
) -> Map<String, Value> {
    let mut out = Map::new();
    for name in names {
        let v = raw.get(name).cloned().unwrap_or(Value::Null);
        let met = match name.as_str() {
            "max_docs" | "max_primary_shard_docs" => match settings_value_int(&v) {
                Some(n) => idx.order.len() as i64 >= n,
                None => false,
            },
            "max_age" => match parse_duration_millis(&v) {
                Some(ms) => crate::store::now_millis() - idx.creation_date >= ms,
                None => false,
            },
            "max_size" | "max_primary_shard_size" => false,
            _ => false,
        };
        out.insert(condition_key(name, &v), json!(met));
    }
    out
}

fn next_backing_name(old: &str) -> Option<String> {
    let idx = old.rfind('-')?;
    let (prefix, suffix) = (&old[..idx], &old[idx + 1..]);
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = suffix.parse().ok()?;
    let width = suffix.len();
    Some(format!("{}-{:0width$}", prefix, n + 1, width = width))
}

pub fn handle_rollover(store: &mut Store, req: &Req, target: &str, new_index_name: &str) -> Response {
    if req.method != "POST" {
        return method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let conditions_raw: Map<String, Value> = body
        .get("conditions")
        .and_then(|c| c.as_object())
        .cloned()
        .unwrap_or_default();

    let mut cond_names: Vec<String> = vec![];
    for name in conditions_raw.keys() {
        match name.as_str() {
            "max_docs" | "max_age" | "max_size" | "max_primary_shard_size"
            | "max_primary_shard_docs" => cond_names.push(name.clone()),
            _ => {
                return err_response(
                    ApiError::new(
                        "x_content_parse_exception",
                        &format!("unknown field [{}]", name),
                        400,
                    )
                    .field(
                        &format!("rollover.conditions.{}", name),
                        json!("unknown field"),
                    ),
                );
            }
        }
    }

    let dry_run = req.bool_param("dry_run");

    if store.indices.contains_key(target) {
        return err_response(ApiError::bad(
            "rollover target is a [concrete index] but one of [alias,data_stream] was expected",
        ));
    }

    if store.data_streams.contains_key(target) {
        let ds = store.data_streams.get(target).unwrap().clone();
        if ds.backing_indices.is_empty() {
            return err_response(ApiError::bad(&format!(
                "rollover target [{}] does not exist",
                target
            )));
        }
        let last = ds.backing_indices.last().unwrap().clone();
        let old_idx = match store.indices.get(&last) {
            Some(i) => i.clone(),
            None => {
                return err_response(ApiError::bad(&format!(
                    "rollover target [{}] does not exist",
                    target
                )))
            }
        };
        let conds = evaluate_rollover_conditions(&conditions_raw, &cond_names, &old_idx);
        let rolled_over = conditions_raw.is_empty()
            || conds.values().any(|v| v.as_bool().unwrap_or(false));
        let mut new_name = backing_index_name(target, ds.generation + 1);
        if !new_index_name.is_empty() {
            new_name = new_index_name.to_string();
        }
        let mut ack = false;
        let do_roll = rolled_over && !dry_run;
        if do_roll {
            let info = DataStreamInfo {
                name: ds.name.clone(),
                backing_indices: vec![],
                generation: ds.generation + 1,
                timestamp_field: ds.timestamp_field.clone(),
                template: ds.template.clone(),
            };
            if let Err(e) = create_backing_index(store, &info) {
                return err_response(e);
            }
            let ds_mut = store.data_streams.get_mut(target).unwrap();
            ds_mut.generation += 1;
            ds_mut.backing_indices.push(new_name.clone());
            ack = true;
            store.bump_state();
        }
        return json_response(
            200,
            json!({
                "acknowledged": ack,
                "shards_acknowledged": ack,
                "old_index": last,
                "new_index": new_name,
                "rolled_over": do_roll,
                "dry_run": dry_run,
                "conditions": conds,
            }),
        );
    }

    // Alias path.
    let mut with_alias: Vec<String> = vec![];
    let mut write_idx: Option<String> = None;
    for name in sorted_index_names(store) {
        let idx = &store.indices[&name];
        if let Some(def) = idx.aliases.get(target) {
            with_alias.push(name.clone());
            if def.get("is_write_index").and_then(|v| v.as_bool()).unwrap_or(false) {
                write_idx = Some(name);
            }
        }
    }
    if with_alias.is_empty() {
        return err_response(ApiError::bad(&format!(
            "rollover target [{}] does not exist",
            target
        )));
    }
    let old_name = if let Some(w) = write_idx {
        w
    } else if with_alias.len() == 1 {
        with_alias[0].clone()
    } else {
        return err_response(ApiError::bad(&format!(
            "rollover target [{}] does not point to a write index",
            target
        )));
    };

    let mut new_name = new_index_name.to_string();
    if new_name.is_empty() {
        match next_backing_name(&old_name) {
            Some(n) => new_name = n,
            None => {
                return err_response(ApiError::bad(&format!(
                    "index name [{}] does not match pattern '^.*-\\d+$'",
                    old_name
                )))
            }
        }
    }

    let old_idx = store.indices.get(&old_name).unwrap().clone();
    let conds = evaluate_rollover_conditions(&conditions_raw, &cond_names, &old_idx);
    let rolled_over =
        conditions_raw.is_empty() || conds.values().any(|v| v.as_bool().unwrap_or(false));

    let mut ack = false;
    let do_roll = rolled_over && !dry_run;
    if do_roll {
        let mut body = Map::new();
        body.insert(
            "settings".to_string(),
            Value::Object(old_idx.settings.clone()),
        );
        body.insert("mappings".to_string(), old_idx.mappings.clone());
        if let Err(e) = crate::admin::create_index_internal(store, &new_name, &body, false) {
            return err_response(e);
        }
        let old_def = old_idx
            .aliases
            .get(target)
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        let mut new_def = Map::new();
        if old_def.get("is_write_index").is_some() {
            let mut old_copy = old_def.as_object().cloned().unwrap_or_default();
            old_copy.insert("is_write_index".to_string(), json!(false));
            if let Some(idx) = store.indices.get_mut(&old_name) {
                idx.aliases.insert(target.to_string(), Value::Object(old_copy));
            }
            new_def.insert("is_write_index".to_string(), json!(true));
        } else if let Some(idx) = store.indices.get_mut(&old_name) {
            idx.aliases.remove(target);
        }
        if let Some(idx) = store.indices.get_mut(&new_name) {
            idx.aliases.insert(target.to_string(), Value::Object(new_def));
        }
        ack = true;
        store.bump_state();
    }

    json_response(
        200,
        json!({
            "acknowledged": ack,
            "shards_acknowledged": ack,
            "old_index": old_name,
            "new_index": new_name,
            "rolled_over": do_roll,
            "dry_run": dry_run,
            "conditions": conds,
        }),
    )
}
