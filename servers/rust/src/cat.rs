use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};

use crate::cluster::{compute_index_health, format_utc_millis, worst_status, NODE_HOST};
use crate::errors::{err_response, no_handler, text_response, ApiError};
use crate::resp::encode_string;
use crate::shared::{parse_expand_wildcards, resolve_indices, resolve_spec2, ResolveOpts2};
use crate::store::*;
use crate::{Req, Store};

const CAT_ROOT_LISTING: &str = "=^.^=\n/_cat/allocation\n/_cat/segment_replication\n/_cat/segment_replication/{index}\n/_cat/shards\n/_cat/shards/{index}\n/_cat/cluster_manager\n/_cat/nodes\n/_cat/tasks\n/_cat/indices\n/_cat/indices/{index}\n/_cat/segments\n/_cat/segments/{index}\n/_cat/count\n/_cat/count/{index}\n/_cat/recovery\n/_cat/recovery/{index}\n/_cat/health\n/_cat/pending_tasks\n/_cat/aliases\n/_cat/aliases/{alias}\n/_cat/thread_pool\n/_cat/thread_pool/{thread_pools}\n/_cat/plugins\n/_cat/fielddata\n/_cat/fielddata/{fields}\n/_cat/nodeattrs\n/_cat/repositories\n/_cat/snapshots/{repository}\n/_cat/templates\n/_cat/pit_segments\n/_cat/pit_segments/{pit_id}\n";

fn cat_base_params() -> Vec<&'static str> {
    vec![
        "v", "help", "h", "s", "format", "pretty", "bytes", "local",
        "cluster_manager_timeout", "master_timeout", "expand_wildcards", "ignore_unavailable",
        "ts", "full_id", "label", "index", "actions", "detailed", "group_by", "node_id",
        "pri", "name", "health", "include_unloaded_segments", "error_trace", "filter_path",
        "source",
    ]
}

struct CatCol {
    key: &'static str,
    aliases: &'static [&'static str],
    desc: &'static str,
    numeric: bool,
    no_default: bool,
}

fn col(key: &'static str, aliases: &'static [&'static str], desc: &'static str) -> CatCol {
    CatCol { key, aliases, desc, numeric: false, no_default: false }
}

fn ncol(key: &'static str, aliases: &'static [&'static str], desc: &'static str) -> CatCol {
    CatCol { key, aliases, desc, numeric: true, no_default: false }
}

fn ncol_nd(key: &'static str, aliases: &'static [&'static str], desc: &'static str) -> CatCol {
    CatCol { key, aliases, desc, numeric: true, no_default: true }
}

fn col_nd(key: &'static str, aliases: &'static [&'static str], desc: &'static str) -> CatCol {
    CatCol { key, aliases, desc, numeric: false, no_default: true }
}

struct CatRow {
    values: Map<String, Value>,
}

struct ReqCol {
    key: String,
    display: String,
    numeric: bool,
}

fn cell_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                format_number_go(f)
            } else {
                n.to_string()
            }
        }
        other => other.to_string(),
    }
}

fn format_number_go(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

// ------------------------------------------------------------------ render

fn resolve_cat_columns(h: &str, cols: &[CatCol]) -> Vec<ReqCol> {
    if h.is_empty() {
        return cols
            .iter()
            .filter(|c| !c.no_default)
            .map(|c| ReqCol {
                key: c.key.to_string(),
                display: c.key.to_string(),
                numeric: c.numeric,
            })
            .collect();
    }
    let mut out = vec![];
    for tok in h.split(',') {
        let token = tok.trim();
        if token.is_empty() {
            continue;
        }
        if token.contains('*') || token.contains('?') {
            for c in cols {
                if glob_match(token, c.key) {
                    out.push(ReqCol {
                        key: c.key.to_string(),
                        display: c.key.to_string(),
                        numeric: c.numeric,
                    });
                }
            }
            continue;
        }
        let mut found = false;
        for c in cols {
            if c.key == token {
                out.push(ReqCol {
                    key: c.key.to_string(),
                    display: token.to_string(),
                    numeric: c.numeric,
                });
                found = true;
                break;
            }
            if c.aliases.contains(&token) {
                out.push(ReqCol {
                    key: c.key.to_string(),
                    display: token.to_string(),
                    numeric: c.numeric,
                });
                found = true;
                break;
            }
        }
        let _ = found;
    }
    out
}

fn cat_alias_to_key(token: &str, cols: &[CatCol]) -> String {
    for c in cols {
        if c.key == token {
            return token.to_string();
        }
        if c.aliases.contains(&token) {
            return c.key.to_string();
        }
    }
    String::new()
}

fn sort_cat_rows(
    s: &str,
    cols: &[CatCol],
    rows: &mut Vec<CatRow>,
) -> Result<(), ApiError> {
    if s.is_empty() {
        return Ok(());
    }
    let mut keys: Vec<(String, bool)> = vec![];
    for tok in s.split(',') {
        let token = tok.trim();
        if token.is_empty() {
            continue;
        }
        let mut key = token.to_string();
        let mut desc = false;
        if let Some(i) = token.rfind(':') {
            let dir = &token[i + 1..];
            if dir != "asc" && dir != "desc" {
                return Err(ApiError::new(
                    "unsupported_operation_exception",
                    &format!("Unable to sort by unknown sort key `{}`", token),
                    500,
                ));
            }
            desc = dir == "desc";
            key = token[..i].to_string();
        }
        let canonical = cat_alias_to_key(&key, cols);
        if canonical.is_empty() {
            return Err(ApiError::new(
                "unsupported_operation_exception",
                &format!("Unable to sort by unknown sort key `{}`", token),
                500,
            ));
        }
        keys.push((canonical, desc));
    }
    let numeric_cols: std::collections::HashMap<&str, bool> =
        cols.iter().map(|c| (c.key, c.numeric)).collect();
    rows.sort_by(|r1, r2| {
        for (k, desc) in &keys {
            let a = r1.values.get(k);
            let b = r2.values.get(k);
            let (an, bn) = (a.map_or(true, |v| v.is_null()), b.map_or(true, |v| v.is_null()));
            if an && bn {
                continue;
            }
            if an {
                return if *desc {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            if bn {
                return if *desc {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
            let a = a.unwrap();
            let b = b.unwrap();
            let mut less = false;
            let mut eq;
            if numeric_cols.get(k.as_str()).copied().unwrap_or(false) {
                let af = to_f64(a);
                let bf = to_f64(b);
                match (af, bf) {
                    (Some(x), Some(y)) => {
                        less = x < y;
                        eq = x == y;
                    }
                    _ => {
                        let (x, y) = (cell_string(a), cell_string(b));
                        less = x < y;
                        eq = x == y;
                    }
                }
            } else {
                let (x, y) = (cell_string(a), cell_string(b));
                less = x < y;
                eq = x == y;
            }
            if less {
                return if *desc {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
            if !eq {
                return if *desc {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            eq = true;
            let _ = eq;
        }
        std::cmp::Ordering::Equal
    });
    Ok(())
}

fn render_cat(req: &Req, cols: &[ReqCol], rows: &[CatRow], v_flag: bool) -> Response {
    let format = req.param_or("format", "").to_lowercase();
    let pretty = req.bool_param("pretty");
    match format.as_str() {
        "json" => render_cat_json(cols, rows, pretty),
        "yaml" => {
            let mut buf = String::from("---\n");
            for row in rows {
                for (i, c) in cols.iter().enumerate() {
                    let prefix = if i == 0 { "- " } else { "  " };
                    buf.push_str(prefix);
                    buf.push_str(&c.display);
                    buf.push_str(": ");
                    match row.values.get(&c.key) {
                        None | Some(Value::Null) => buf.push_str("null"),
                        Some(v) => buf.push_str(&encode_string(&cell_string(v))),
                    }
                    buf.push('\n');
                }
            }
            text_response(200, "application/yaml", buf)
        }
        _ => {
            let mut widths: Vec<usize> = vec![0; cols.len()];
            for (i, c) in cols.iter().enumerate() {
                if v_flag {
                    widths[i] = c.display.len();
                }
            }
            for row in rows {
                for (i, c) in cols.iter().enumerate() {
                    let v = cell_string(row.values.get(&c.key).unwrap_or(&Value::Null));
                    if v.len() > widths[i] {
                        widths[i] = v.len();
                    }
                }
            }
            let mut lines: Vec<String> = vec![];
            if v_flag {
                let parts: Vec<String> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        if c.numeric {
                            pad_left(&c.display, widths[i])
                        } else {
                            pad_right(&c.display, widths[i])
                        }
                    })
                    .collect();
                lines.push(parts.join(" ").trim_end().to_string());
            }
            for row in rows {
                let parts: Vec<String> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let v = cell_string(row.values.get(&c.key).unwrap_or(&Value::Null));
                        if c.numeric {
                            pad_left(&v, widths[i])
                        } else {
                            pad_right(&v, widths[i])
                        }
                    })
                    .collect();
                lines.push(parts.join(" ").trim_end().to_string());
            }
            if lines.is_empty() {
                return text_response(200, "text/plain; charset=UTF-8", String::new());
            }
            text_response(200, "text/plain; charset=UTF-8", lines.join("\n") + "\n")
        }
    }
}

fn pad_right(s: &str, n: usize) -> String {
    let mut out = s.to_string();
    while out.len() < n {
        out.push(' ');
    }
    out
}

fn pad_left(s: &str, n: usize) -> String {
    let mut out = s.to_string();
    while out.len() < n {
        out.insert(0, ' ');
    }
    out
}

fn write_cat_json_object(buf: &mut String, cols: &[ReqCol], row: &CatRow, indent: usize, pretty: bool) {
    buf.push('{');
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        if pretty {
            buf.push('\n');
            for _ in 0..indent + 1 {
                buf.push_str("  ");
            }
        }
        buf.push_str(&encode_string(&c.display));
        if pretty {
            buf.push_str(" : ");
        } else {
            buf.push(':');
        }
        match row.values.get(&c.key) {
            None | Some(Value::Null) => buf.push_str("null"),
            Some(v) => buf.push_str(&encode_string(&cell_string(v))),
        }
    }
    if pretty && !cols.is_empty() {
        buf.push('\n');
        for _ in 0..indent {
            buf.push_str("  ");
        }
    }
    buf.push('}');
}

fn render_cat_json(cols: &[ReqCol], rows: &[CatRow], pretty: bool) -> Response {
    let mut buf = String::new();
    if pretty {
        buf.push_str("[\n");
        for (ri, row) in rows.iter().enumerate() {
            buf.push_str("  ");
            write_cat_json_object(&mut buf, cols, row, 1, true);
            if ri < rows.len() - 1 {
                buf.push(',');
            }
            buf.push('\n');
        }
        buf.push_str("]\n");
    } else {
        buf.push('[');
        for (ri, row) in rows.iter().enumerate() {
            if ri > 0 {
                buf.push(',');
            }
            write_cat_json_object(&mut buf, cols, row, 0, false);
        }
        buf.push(']');
    }
    let mut resp = (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        buf,
    )
        .into_response();
    resp.headers_mut().insert("x-lite-ordered", "1".parse().unwrap());
    resp
}

// ------------------------------------------------------------------ helpers

fn format_cat_bytes(n: i64, unit: &str) -> String {
    match unit {
        "b" => return n.to_string(),
        "kb" => return (n / 1024).to_string(),
        "mb" => return (n / (1024 * 1024)).to_string(),
        "gb" => return (n / (1024 * 1024 * 1024)).to_string(),
        _ => {}
    }
    if n < 1024 {
        return format!("{}b", n);
    }
    if n < 1024 * 1024 {
        return format!("{}kb", trim_float(n as f64 / 1024.0));
    }
    if n < 1024 * 1024 * 1024 {
        return format!("{}mb", trim_float(n as f64 / (1024.0 * 1024.0)));
    }
    format!("{}gb", trim_float(n as f64 / (1024.0 * 1024.0 * 1024.0)))
}

fn trim_float(f: f64) -> String {
    let s = format!("{:.1}", f);
    s.strip_suffix(".0").map(|x| x.to_string()).unwrap_or(s)
}

fn distribute_docs(total: usize, shards: i64) -> Vec<usize> {
    let shards = if shards <= 0 { 1 } else { shards as usize };
    let mut out = vec![0usize; shards];
    for i in 0..total {
        out[i % shards] += 1;
    }
    out
}

fn time_of_day() -> String {
    let ts = format_utc_millis(now_millis());
    ts.get(11..19).unwrap_or("00:00:00").to_string()
}

fn int_or_default(v: Option<&Value>, def: i64) -> i64 {
    match v {
        None | Some(Value::Null) => def,
        Some(val) => match to_f64(val) {
            Some(f) => f as i64,
            None => def,
        },
    }
}

fn int_or_nil(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(val) => match to_f64(val) {
            Some(f) => json!((f as i64).to_string()),
            None => Value::Null,
        },
    }
}

fn format_patterns(v: Option<&Value>) -> String {
    let mut items: Vec<String> = vec![];
    match v {
        Some(Value::Array(a)) => {
            for p in a {
                items.push(cell_string(p));
            }
        }
        Some(Value::String(s)) => items.push(s.clone()),
        _ => {}
    }
    format!("[{}]", items.join(", "))
}

// ------------------------------------------------------------------- entry

pub fn handle_cat(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if req.method != "GET" && req.method != "HEAD" {
        return crate::errors::method_not_allowed(req, &["GET"]);
    }
    if rest.is_empty() {
        return text_response(200, "text/plain; charset=UTF-8", CAT_ROOT_LISTING.to_string());
    }
    let endpoint = rest[0].as_str();
    let spec = if rest.len() >= 2 { rest[1].clone() } else { String::new() };
    if rest.len() > 2 {
        return no_handler(&req.method, &req.path);
    }
    let mut known: Vec<&str> = cat_base_params();
    match endpoint {
        "indices" => known.push("creation_date"),
        "health" => {
            known.push("epoch");
            known.push("timestamp");
        }
        "thread_pool" => known.push("size"),
        "shards" | "nodes" | "cluster_manager" | "master" | "aliases" | "count" | "segments"
        | "allocation" | "recovery" | "templates" | "nodeattrs" | "plugins" | "pending_tasks"
        | "repositories" | "fielddata" | "tasks" => {}
        _ => return no_handler(&req.method, &req.path),
    }
    if let Some(val) = req.param("v") {
        if !val.is_empty() && val != "true" && val != "false" {
            return err_response(ApiError::new(
                "options_parse_exception",
                &format!(
                    "Failed to parse value [{}] as only [true] or [false] are allowed.",
                    val
                ),
                400,
            ));
        }
    }
    if req.bool_param("help") {
        if !spec.is_empty() {
            return err_response(ApiError::bad(&format!(
                "request [{}] contains unrecognized parameter: [index]",
                req.path
            )));
        }
        return write_cat_help(endpoint);
    }
    for (name, _) in &req.query {
        if !known.contains(&name.as_str()) {
            return err_response(ApiError::bad(&format!(
                "request [{}] contains unrecognized parameter: [{}]",
                req.path, name
            )));
        }
    }

    let mut cols: Vec<CatCol>;
    let mut rows: Vec<CatRow>;
    let api_err: Option<ApiError>;
    match endpoint {
        "indices" => {
            let (c, r, e) = cat_indices(store, req, &spec);
            cols = c;
            rows = r;
            api_err = e;
        }
        "health" => {
            let (c, r) = cat_health(store);
            cols = c;
            rows = r;
            api_err = None;
        }
        "count" => {
            let (c, r, e) = cat_count(store, &spec);
            cols = c;
            rows = r;
            api_err = e;
        }
        "aliases" => {
            let (c, r) = cat_aliases(store, &spec);
            cols = c;
            rows = r;
            api_err = None;
        }
        "shards" => {
            let (c, r, e) = cat_shards(store, req, &spec);
            cols = c;
            rows = r;
            api_err = e;
        }
        "nodes" => {
            let (c, r) = cat_nodes(req);
            cols = c;
            rows = r;
            api_err = None;
        }
        "cluster_manager" | "master" => {
            let (c, r) = cat_cluster_manager();
            cols = c;
            rows = r;
            api_err = None;
        }
        "nodeattrs" => {
            let (c, r) = cat_node_attrs();
            cols = c;
            rows = r;
            api_err = None;
        }
        "plugins" => {
            let (c, r) = cat_plugins();
            cols = c;
            rows = r;
            api_err = None;
        }
        "pending_tasks" => {
            let (c, r) = cat_pending_tasks();
            cols = c;
            rows = r;
            api_err = None;
        }
        "thread_pool" => {
            let (c, r) = cat_thread_pool(&spec);
            cols = c;
            rows = r;
            api_err = None;
        }
        "repositories" => {
            let (c, r) = cat_repositories();
            cols = c;
            rows = r;
            api_err = None;
        }
        "fielddata" => {
            let (c, r) = cat_fielddata();
            cols = c;
            rows = r;
            api_err = None;
        }
        "allocation" => {
            let (c, r, e) = cat_allocation(store, req);
            cols = c;
            rows = r;
            api_err = e;
        }
        "recovery" => {
            let (c, r, e) = cat_recovery(store, &spec);
            cols = c;
            rows = r;
            api_err = e;
        }
        "segments" => {
            let (c, r, e) = cat_segments(store, req, &spec);
            cols = c;
            rows = r;
            api_err = e;
        }
        "tasks" => {
            let (c, r) = cat_tasks(req);
            cols = c;
            rows = r;
            api_err = None;
        }
        "templates" => {
            let (c, r) = cat_templates(store, &spec);
            cols = c;
            rows = r;
            api_err = None;
        }
        _ => return no_handler(&req.method, &req.path),
    }
    if let Some(e) = api_err {
        return err_response(e);
    }
    let mut resp_extra: Vec<(&str, String)> = vec![];
    if endpoint == "master" {
        resp_extra.push((
            "warning",
            "299 OpenSearch-3.9.0-00000000 \"[GET /_cat/master] is deprecated! Use [GET /_cat/cluster_manager] instead.\"".to_string(),
        ));
    }
    if endpoint == "health" && req.param_or("ts", "").to_lowercase() == "false" {
        cols = cols.split_off(2);
    }
    if let Some(e) = filter_cat_health(req, endpoint, &mut rows) {
        return err_response(e);
    }
    let requested = resolve_cat_columns(&req.param_or("h", ""), &cols);
    if let Err(e) = sort_cat_rows(&req.param_or("s", ""), &cols, &mut rows) {
        return err_response(e);
    }
    let mut resp = render_cat(req, &requested, &rows, req.bool_param("v"));
    for (k, v) in resp_extra {
        resp.headers_mut().insert(k, v.parse().unwrap());
    }
    resp
}

fn filter_cat_health(req: &Req, endpoint: &str, rows: &mut Vec<CatRow>) -> Option<ApiError> {
    if endpoint != "indices" {
        return None;
    }
    let health = req.param_or("health", "");
    if health.is_empty() {
        return None;
    }
    if health != "green" && health != "yellow" && health != "red" {
        return Some(ApiError::new(
            "illegal_argument_exception",
            &format!("unknown cluster health status [{}]", health),
            400,
        ));
    }
    rows.retain(|row| row.values.get("health").and_then(|v| v.as_str()) == Some(health.as_str()));
    None
}

// -------------------------------------------------------------------- help

fn write_cat_help(endpoint: &str) -> Response {
    let cols: Vec<CatCol> = match endpoint {
        "indices" => {
            let all = cat_indices_columns();
            let mut out: Vec<CatCol> = all[..8].iter().map(clone_col).collect();
            out.push(clone_col(&all[10]));
            out.push(clone_col(&all[11]));
            out.push(clone_col(&all[8]));
            out.push(clone_col(&all[9]));
            out
        }
        "health" => cat_health_columns(),
        "aliases" => cat_aliases_columns(),
        "shards" => cat_shards_columns(),
        _ => {
            let mut buf = String::new();
            buf.push_str(&pad_right("help", 22));
            buf.push_str(" | ");
            buf.push_str(&pad_right("help", 22));
            buf.push_str(" | ");
            buf.push_str(endpoint);
            buf.push_str(" help\n");
            return text_response(200, "text/plain; charset=UTF-8", buf);
        }
    };
    let mut buf = String::new();
    for c in &cols {
        let alias = if c.aliases.is_empty() {
            c.key.to_string()
        } else {
            c.aliases.join(",")
        };
        buf.push_str(&pad_right(c.key, 22));
        buf.push_str(" | ");
        buf.push_str(&pad_right(&alias, 22));
        buf.push_str(" | ");
        buf.push_str(c.desc);
        buf.push('\n');
    }
    text_response(200, "text/plain; charset=UTF-8", buf)
}

fn clone_col(c: &CatCol) -> CatCol {
    CatCol {
        key: c.key,
        aliases: c.aliases,
        desc: c.desc,
        numeric: c.numeric,
        no_default: c.no_default,
    }
}

// ---------------------------------------------------------------- indices

fn cat_indices_columns() -> Vec<CatCol> {
    vec![
        col("health", &["h"], "current health status"),
        col("status", &["s"], "open/close status"),
        col("index", &["i", "idx"], "index name"),
        col("uuid", &["id"], "index uuid"),
        ncol("pri", &["p", "shardsPrimary"], "number of primary shards"),
        ncol("rep", &["r", "shardsReplica"], "number of replica shards"),
        ncol("docs.count", &["dc", "docsCount"], "available docs"),
        ncol("docs.deleted", &["dd", "docsDeleted"], "deleted docs"),
        ncol("store.size", &["ss", "storeSize"], "store size of primaries & replicas"),
        ncol("pri.store.size", &["pss", "priStoreSize"], "store size of primaries"),
        ncol_nd("creation.date", &["cd"], "creation date"),
        col_nd("creation.date.string", &["cds"], "creation date string"),
    ]
}

fn cat_indices(store: &Store, req: &Req, spec: &str) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let all = cat_indices_columns();
    let expand_raw = req.param_or("expand_wildcards", "");
    let mut expand: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    expand.insert("open".to_string(), true);
    expand.insert("closed".to_string(), true);
    if !expand_raw.is_empty() {
        match parse_expand_wildcards(&expand_raw, None) {
            Ok(parsed) => expand = parsed,
            Err(e) => return (all, vec![], Some(e)),
        }
    }
    let mut o = ResolveOpts2::default();
    o.expand = Some(expand);
    o.ignore_unavailable = req.bool_param("ignore_unavailable");
    let names = match resolve_spec2(store, spec, &o) {
        Ok(n) => n,
        Err(e) => return (all, vec![], Some(e)),
    };
    let bytes_unit = req.param_or("bytes", "");
    let mut rows: Vec<CatRow> = vec![];
    for name in &names {
        let idx = &store.indices[name];
        let h = compute_index_health(idx);
        let mut values = Map::new();
        values.insert("health".into(), json!(h.status));
        values.insert(
            "status".into(),
            json!(if idx.closed { "close" } else { "open" }),
        );
        values.insert("index".into(), json!(idx.name));
        values.insert("uuid".into(), json!(idx.uuid));
        values.insert("pri".into(), json!(idx.num_shards().to_string()));
        values.insert("rep".into(), json!(idx.num_replicas().to_string()));
        if idx.closed {
            values.insert("docs.count".into(), Value::Null);
            values.insert("docs.deleted".into(), Value::Null);
            values.insert("store.size".into(), Value::Null);
            values.insert("pri.store.size".into(), Value::Null);
        } else {
            values.insert(
                "docs.count".into(),
                json!(idx.docs.len().saturating_add(nested_object_count(idx)).to_string()),
            );
            values.insert("docs.deleted".into(), json!("0"));
            values.insert(
                "store.size".into(),
                json!(format_cat_bytes(idx.store_bytes(), &bytes_unit)),
            );
            values.insert(
                "pri.store.size".into(),
                json!(format_cat_bytes(idx.store_bytes(), &bytes_unit)),
            );
        }
        values.insert("creation.date".into(), json!(idx.creation_date.to_string()));
        values.insert(
            "creation.date.string".into(),
            json!(format_utc_millis(idx.creation_date)),
        );
        rows.push(CatRow { values });
    }
    if !req.has_param("h") {
        rows.sort_by(|a, b| {
            cell_string(a.values.get("index").unwrap_or(&Value::Null))
                .cmp(&cell_string(b.values.get("index").unwrap_or(&Value::Null)))
        });
    }
    (all, rows, None)
}

// ------------------------------------------------------------------ health

fn cat_health_columns() -> Vec<CatCol> {
    vec![
        ncol("epoch", &["e"], "seconds since 1970"),
        col("timestamp", &["ts"], "time of day"),
        col("cluster", &["cl"], "cluster name"),
        col("status", &["st"], "health status"),
        ncol("node.total", &["nt"], "total number of nodes"),
        ncol("node.data", &["nd"], "number of data nodes"),
        col("discovered_cluster_manager", &["dcm"], "cluster manager node discovered"),
        ncol("shards", &["t", "sh", "total"], "total number of shards"),
        ncol("pri", &["p"], "number of primary shards"),
        ncol("relo", &["r"], "number of relocating nodes"),
        ncol("init", &["i"], "number of initializing nodes"),
        ncol("unassign", &["u"], "number of unassigned shards"),
        ncol("pending_tasks", &["pt"], "number of pending tasks"),
        col("max_task_wait_time", &["mtwt"], "max time of longest tasks"),
        col("active_shards_percent", &["asp"], "percent of active shards"),
    ]
}

fn cat_health(store: &Store) -> (Vec<CatCol>, Vec<CatRow>) {
    let indices = resolve_indices(store, "*", false, true, false).unwrap_or_default();
    let mut status = "green".to_string();
    let mut pri = 0;
    let mut active = 0;
    let mut unassigned = 0;
    for idx in &indices {
        let h = compute_index_health(idx);
        status = worst_status(&status, &h.status);
        pri += h.active_primary;
        active += h.active;
        unassigned += h.unassigned;
    }
    let total = active + unassigned;
    let pct = if total > 0 {
        active as f64 / total as f64 * 100.0
    } else {
        100.0
    };
    let now = now_millis();
    let mut values = Map::new();
    values.insert("epoch".into(), json!((now / 1000).to_string()));
    values.insert("timestamp".into(), json!(time_of_day()));
    values.insert("cluster".into(), json!(CLUSTER_NAME));
    values.insert("status".into(), json!(status));
    values.insert("node.total".into(), json!("1"));
    values.insert("node.data".into(), json!("1"));
    values.insert("discovered_cluster_manager".into(), json!("true"));
    values.insert("shards".into(), json!(total.to_string()));
    values.insert("pri".into(), json!(pri.to_string()));
    values.insert("relo".into(), json!("0"));
    values.insert("init".into(), json!("0"));
    values.insert("unassign".into(), json!(unassigned.to_string()));
    values.insert("pending_tasks".into(), json!("0"));
    values.insert("max_task_wait_time".into(), json!("-"));
    values.insert("active_shards_percent".into(), json!(format!("{:.1}%", pct)));
    (cat_health_columns(), vec![CatRow { values }])
}

// ------------------------------------------------------------------- count

fn cat_count(store: &Store, spec: &str) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let cols = vec![
        ncol("epoch", &["e"], "seconds since 1970"),
        col("timestamp", &["ts"], "time of day"),
        ncol("count", &["c"], "document count"),
    ];
    let indices = match resolve_indices(store, spec, true, false, true) {
        Ok(i) => i,
        Err(e) => return (cols, vec![], Some(e)),
    };
    for idx in &indices {
        if idx.closed {
            return (
                cols,
                vec![],
                Some(ApiError::new("index_closed_exception", "closed", 400)),
            );
        }
    }
    let mut count = 0usize;
    for idx in &indices {
        count += idx.docs.len();
    }
    if !spec.is_empty() && !spec.contains('*') && !spec.contains('?') {
        for idx in &indices {
            for (alias_name, def) in &idx.aliases {
                if alias_name == spec {
                    let mut filtered = 0;
                    for (_, doc) in &idx.docs {
                        if matches_alias_filter(doc, def) {
                            filtered += 1;
                        }
                    }
                    if def.as_object().map_or(false, |m| !m.is_empty()) {
                        count = filtered;
                    }
                }
            }
        }
    }
    let now = now_millis();
    let mut values = Map::new();
    values.insert("epoch".into(), json!((now / 1000).to_string()));
    values.insert("timestamp".into(), json!(time_of_day()));
    values.insert("count".into(), json!(count.to_string()));
    (cols, vec![CatRow { values }], None)
}

fn matches_alias_filter(doc: &Value, def: &Value) -> bool {
    let filter = match def.get("filter").and_then(|f| f.as_object()) {
        Some(f) => f,
        None => return true,
    };
    let term = match filter.get("term").and_then(|t| t.as_object()) {
        Some(t) => t,
        None => return true,
    };
    for (field, want) in term {
        let got = match doc.get(field) {
            Some(g) => g,
            None => return false,
        };
        if cell_string(got) != cell_string(want) {
            return false;
        }
    }
    true
}

// ----------------------------------------------------------------- aliases

fn cat_aliases_columns() -> Vec<CatCol> {
    vec![
        col("alias", &["a"], "alias name"),
        col("index", &["i", "idx"], "index alias points to"),
        col("filter", &["f", "fi"], "filter"),
        col("routing.index", &["ri", "routingIndex"], "index routing"),
        col("routing.search", &["rs", "routingSearch"], "search routing"),
        col("is_write_index", &["w", "isWriteIndex"], "write index"),
    ]
}

fn cat_aliases(store: &Store, spec: &str) -> (Vec<CatCol>, Vec<CatRow>) {
    let mut rows: Vec<CatRow> = vec![];
    let spec = if spec.is_empty() { "*" } else { spec };
    for name in sorted_index_names(store) {
        let idx = &store.indices[&name];
        for (alias, def) in &idx.aliases {
            if !glob_match(spec, alias) {
                continue;
            }
            let mut values = Map::new();
            values.insert("alias".into(), json!(alias));
            values.insert("index".into(), json!(idx.name));
            values.insert(
                "filter".into(),
                json!(if def.get("filter").is_some() { "*" } else { "-" }),
            );
            values.insert(
                "routing.index".into(),
                match def.get("index_routing") {
                    Some(v) => json!(cell_string(v)),
                    None => json!("-"),
                },
            );
            values.insert(
                "routing.search".into(),
                match def.get("search_routing") {
                    Some(v) => json!(cell_string(v)),
                    None => json!("-"),
                },
            );
            values.insert(
                "is_write_index".into(),
                match def.get("is_write_index") {
                    Some(v) => json!(cell_string(v)),
                    None => json!("-"),
                },
            );
            rows.push(CatRow { values });
        }
    }
    (cat_aliases_columns(), rows)
}

// ------------------------------------------------------------------ shards

fn cat_shards_columns() -> Vec<CatCol> {
    vec![
        col("index", &["i", "idx"], "index name"),
        ncol("shard", &["s", "sh"], "shard name"),
        col("prirep", &["p", "pr", "primaryOrReplica"], "primary or replica"),
        col("state", &["st"], "shard state"),
        ncol("docs", &["d", "dc", "docsCount"], "number of docs in shard"),
        ncol("store", &["sto", "storeSize"], "store size of shard"),
        col("ip", &["p", "publishAddress"], "ip of node"),
        col("node", &["n", "nodeName"], "name of node"),
        col_nd("unassigned.reason", &["ur"], "unassigned reason"),
    ]
}

fn cat_shards(store: &Store, req: &Req, spec: &str) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let cols = cat_shards_columns();
    let missing404 = !req.bool_param("ignore_unavailable");
    let indices = match resolve_indices(store, spec, missing404, false, false) {
        Ok(i) => i,
        Err(e) => return (cols, vec![], Some(e)),
    };
    for idx in &indices {
        if idx.closed {
            return (
                cols,
                vec![],
                Some(ApiError::new(
                    "index_closed_exception",
                    &format!("closed ({})", idx.name),
                    400,
                )),
            );
        }
    }
    let bytes_unit = req.param_or("bytes", "");
    let mut rows: Vec<CatRow> = vec![];
    for idx in &indices {
        let per_shard = distribute_docs(idx.docs.len(), idx.num_shards());
        for i in 0..idx.num_shards() as usize {
            let mut values = Map::new();
            values.insert("index".into(), json!(idx.name));
            values.insert("shard".into(), json!(i.to_string()));
            values.insert("prirep".into(), json!("p"));
            values.insert("state".into(), json!("STARTED"));
            values.insert("docs".into(), json!(per_shard[i].to_string()));
            values.insert(
                "store".into(),
                json!(format_cat_bytes(per_shard[i] as i64 * 64, &bytes_unit)),
            );
            values.insert("ip".into(), json!(NODE_HOST));
            values.insert("node".into(), json!(NODE_NAME));
            values.insert("unassigned.reason".into(), Value::Null);
            rows.push(CatRow { values });
            for _ in 0..idx.num_replicas() {
                let mut replica = Map::new();
                replica.insert("index".into(), json!(idx.name));
                replica.insert("shard".into(), json!(i.to_string()));
                replica.insert("prirep".into(), json!("r"));
                replica.insert("state".into(), json!("UNASSIGNED"));
                replica.insert("docs".into(), Value::Null);
                replica.insert("store".into(), Value::Null);
                replica.insert("ip".into(), Value::Null);
                replica.insert("node".into(), Value::Null);
                replica.insert("unassigned.reason".into(), json!("INDEX_CREATED"));
                rows.push(CatRow { values: replica });
            }
        }
    }
    if !req.has_param("s") {
        rows.sort_by(|a, b| {
            let ai = cell_string(a.values.get("index").unwrap_or(&Value::Null));
            let bi = cell_string(b.values.get("index").unwrap_or(&Value::Null));
            if ai != bi {
                return ai.cmp(&bi);
            }
            let asv = cell_string(a.values.get("shard").unwrap_or(&Value::Null));
            let bsv = cell_string(b.values.get("shard").unwrap_or(&Value::Null));
            if asv != bsv {
                return asv.cmp(&bsv);
            }
            let ap = a.values.get("prirep").and_then(|v| v.as_str()) == Some("p");
            let bp = b.values.get("prirep").and_then(|v| v.as_str()) == Some("p");
            match (ap, bp) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });
    }
    (cols, rows, None)
}

// ------------------------------------------------------------------- nodes

fn cat_nodes_columns() -> Vec<CatCol> {
    vec![
        ncol("ip", &["i"], "ip address"),
        ncol("heap.percent", &["hp", "heapPercent"], "heap percent"),
        ncol("ram.percent", &["rp", "ramPercent"], "ram percent"),
        ncol("cpu", &["c"], "cpu"),
        ncol("load_1m", &["l"], "load 1m"),
        ncol("load_5m", &["l"], "load 5m"),
        ncol("load_15m", &["l"], "load 15m"),
        col("node.role", &["r", "role", "nodeRole"], "node role"),
        col("node.roles", &["r", "roles", "nodeRoles"], "node roles"),
        col("cluster_manager", &["m", "clusterManager"], "cluster manager node"),
        col_nd("master", &["m", "master"], "master node"),
        col("name", &["n"], "node name"),
        col_nd("id", &["id"], "node id"),
        col_nd("version", &["v"], "node version"),
    ]
}

fn cat_nodes(req: &Req) -> (Vec<CatCol>, Vec<CatRow>) {
    let full_id = req.bool_param("full_id");
    let mut values = Map::new();
    values.insert("ip".into(), json!(NODE_HOST));
    values.insert("heap.percent".into(), json!("12"));
    values.insert("ram.percent".into(), json!("40"));
    values.insert("cpu".into(), json!("5"));
    values.insert("load_1m".into(), json!("0.50"));
    values.insert("load_5m".into(), json!("0.40"));
    values.insert("load_15m".into(), json!("0.30"));
    values.insert("node.role".into(), json!("dimr"));
    values.insert(
        "node.roles".into(),
        json!("cluster_manager,data,ingest,remote_cluster_client"),
    );
    values.insert("cluster_manager".into(), json!("*"));
    values.insert("master".into(), json!("*"));
    values.insert("name".into(), json!(NODE_NAME));
    if full_id {
        values.insert("id".into(), json!(NODE_ID));
    } else {
        let id = if NODE_ID.len() > 4 { &NODE_ID[..4] } else { NODE_ID };
        values.insert("id".into(), json!(id));
    }
    values.insert("version".into(), json!(NODE_VERSION));
    (cat_nodes_columns(), vec![CatRow { values }])
}

fn cat_cluster_manager() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("id", &["nodeId"], "node id"),
        col("host", &["h"], "host name"),
        col("ip", &["i"], "ip address"),
        col("node", &["n"], "node name"),
    ];
    let mut values = Map::new();
    values.insert("id".into(), json!(NODE_ID));
    values.insert("host".into(), json!(NODE_HOST));
    values.insert("ip".into(), json!(NODE_HOST));
    values.insert("node".into(), json!(NODE_NAME));
    (cols, vec![CatRow { values }])
}

fn cat_node_attrs() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("node", &["n"], "node name"),
        col("host", &["h"], "host name"),
        col("ip", &["i"], "ip address"),
        col("attr", &["a"], "attribute name"),
        col("value", &["v"], "attribute value"),
    ];
    let mut values = Map::new();
    values.insert("node".into(), json!(NODE_NAME));
    values.insert("host".into(), json!(NODE_HOST));
    values.insert("ip".into(), json!(NODE_HOST));
    values.insert("attr".into(), json!("shard_indexing_pressure_enabled"));
    values.insert("value".into(), json!("true"));
    (cols, vec![CatRow { values }])
}

fn cat_plugins() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("name", &["n"], "node name"),
        col("component", &["c"], "component"),
        col("version", &["v"], "version"),
    ];
    let rows = crate::nodes::PLUGIN_NAMES
        .iter()
        .map(|name| {
            let mut values = Map::new();
            values.insert("name".into(), json!(NODE_NAME));
            values.insert("component".into(), json!(name));
            values.insert("version".into(), json!("3.9.0.0"));
            CatRow { values }
        })
        .collect();
    (cols, rows)
}

fn cat_pending_tasks() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        ncol("insertOrder", &["o"], "insert order"),
        col("timeInQueue", &["t"], "time in queue"),
        col("priority", &["p"], "priority"),
        col("source", &["s"], "source"),
    ];
    (cols, vec![])
}

fn cat_thread_pool(spec: &str) -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("node_name", &["nn"], "node name"),
        col("name", &["n"], "thread pool name"),
        ncol("active", &["a"], "active threads"),
        ncol("queue", &["q"], "queue size"),
        ncol("rejected", &["r"], "rejected threads"),
        col_nd("type", &["t"], "pool type"),
        ncol_nd("queue_size", &["qs"], "queue size limit"),
    ];
    let pools = crate::nodes::node_thread_pools();
    let mut names: Vec<&String> = pools.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    names.sort();
    let mut rows: Vec<CatRow> = vec![];
    for name in names {
        if !spec.is_empty() {
            let matched = spec.split(',').any(|pat| glob_match(pat, name));
            if !matched {
                continue;
            }
        }
        let mut values = Map::new();
        values.insert("node_name".into(), json!(NODE_NAME));
        values.insert("name".into(), json!(name));
        values.insert("active".into(), json!("0"));
        values.insert("queue".into(), json!("0"));
        values.insert("rejected".into(), json!("0"));
        if let Some(pool) = pools.get(name).and_then(|p| p.as_object()) {
            if let Some(t) = pool.get("type") {
                values.insert("type".into(), json!(cell_string(t)));
            }
            if let Some(qs) = pool.get("queue_size") {
                values.insert("queue_size".into(), json!(cell_string(qs)));
            }
        }
        rows.push(CatRow { values });
    }
    (cols, rows)
}

fn cat_repositories() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("id", &["i"], "repository id"),
        col("type", &["t"], "repository type"),
    ];
    (cols, vec![])
}

fn cat_fielddata() -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("id", &["i"], "node id"),
        col("host", &["h"], "host name"),
        col("ip", &["ip"], "ip address"),
        col("node", &["n"], "node name"),
        col("field", &["f"], "field name"),
        ncol("size", &["s"], "field size"),
    ];
    (cols, vec![])
}

fn cat_allocation(store: &Store, req: &Req) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let cols = vec![
        ncol("shards", &["s"], "number of shards"),
        ncol("disk.indices", &["di"], "disk used by indices"),
        ncol("disk.used", &["du"], "disk used"),
        ncol("disk.avail", &["da"], "disk available"),
        ncol("disk.total", &["dt"], "disk total"),
        ncol("disk.percent", &["dp"], "disk percent"),
        col("host", &["h"], "host name"),
        col("ip", &["ip"], "ip address"),
        col("node", &["n"], "node name"),
    ];
    let indices = resolve_indices(store, "*", false, true, false).unwrap_or_default();
    let mut shards = 0;
    let mut assigned_bytes = 0i64;
    let mut unassigned = 0;
    for idx in &indices {
        if idx.closed {
            continue;
        }
        let h = compute_index_health(idx);
        shards += h.active;
        unassigned += h.unassigned;
        assigned_bytes += idx.store_bytes();
    }
    let bytes_unit = req.param_or("bytes", "");
    let disk_total: i64 = 500107862016;
    let gb: i64 = 1024 * 1024 * 1024;
    let mut values = Map::new();
    values.insert("shards".into(), json!(shards.to_string()));
    values.insert("disk.indices".into(), json!(format_cat_bytes(assigned_bytes, &bytes_unit)));
    values.insert("disk.used".into(), json!(format_cat_bytes(gb, &bytes_unit)));
    values.insert("disk.avail".into(), json!(format_cat_bytes(disk_total - gb, &bytes_unit)));
    values.insert("disk.total".into(), json!(format_cat_bytes(disk_total, &bytes_unit)));
    values.insert("disk.percent".into(), json!("1"));
    values.insert("host".into(), json!(NODE_HOST));
    values.insert("ip".into(), json!(NODE_HOST));
    values.insert("node".into(), json!(NODE_NAME));
    let mut rows = vec![CatRow { values }];
    if unassigned > 0 {
        let mut un = Map::new();
        un.insert("shards".into(), json!(unassigned.to_string()));
        un.insert("disk.indices".into(), Value::Null);
        un.insert("disk.used".into(), Value::Null);
        un.insert("disk.avail".into(), Value::Null);
        un.insert("disk.total".into(), Value::Null);
        un.insert("disk.percent".into(), Value::Null);
        un.insert("host".into(), Value::Null);
        un.insert("ip".into(), Value::Null);
        un.insert("node".into(), json!("UNASSIGNED"));
        rows.push(CatRow { values: un });
    }
    (cols, rows, None)
}

fn cat_recovery(store: &Store, spec: &str) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let cols = vec![
        col("index", &["i", "idx"], "index name"),
        ncol("shard", &["s", "sh"], "shard name"),
        col("start_time", &["st"], "start time"),
        col("time", &["t"], "recovery time"),
        col("type", &["ty"], "recovery type"),
        col("stage", &["sg"], "recovery stage"),
        col("source_host", &["shost"], "source host"),
        col("source_node", &["snode"], "source node"),
        col("target_host", &["thost"], "target host"),
        col("target_node", &["tnode"], "target node"),
        col("repository", &["rep"], "repository"),
        col("snapshot", &["snap"], "snapshot"),
        ncol("files", &["f"], "files"),
        ncol("files_recovered", &["fr"], "files recovered"),
        col("files_percent", &["fp"], "files percent"),
        ncol("files_total", &["ft"], "files total"),
        ncol("bytes", &["b"], "bytes"),
        ncol("bytes_recovered", &["br"], "bytes recovered"),
        col("bytes_percent", &["bp"], "bytes percent"),
        ncol("bytes_total", &["bt"], "bytes total"),
        ncol("translog_ops", &["to"], "translog ops"),
        ncol("translog_ops_recovered", &["tor"], "translog ops recovered"),
        col("translog_ops_percent", &["top"], "translog ops percent"),
    ];
    let indices = match resolve_indices(store, spec, true, false, false) {
        Ok(i) => i,
        Err(e) => return (cols, vec![], Some(e)),
    };
    let mut rows: Vec<CatRow> = vec![];
    for idx in &indices {
        if idx.closed {
            continue;
        }
        for i in 0..idx.num_shards() {
            let mut values = Map::new();
            values.insert("index".into(), json!(idx.name));
            values.insert("shard".into(), json!(i.to_string()));
            values.insert("start_time".into(), json!("2026-01-01T00:00:00.000Z"));
            values.insert("time".into(), json!("0ms"));
            values.insert("type".into(), json!("empty_store"));
            values.insert("stage".into(), json!("done"));
            values.insert("source_host".into(), json!("-"));
            values.insert("source_node".into(), json!("-"));
            values.insert("target_host".into(), json!(NODE_HOST));
            values.insert("target_node".into(), json!(NODE_NAME));
            values.insert("repository".into(), json!("-"));
            values.insert("snapshot".into(), json!("-"));
            values.insert("files".into(), json!("0"));
            values.insert("files_recovered".into(), json!("0"));
            values.insert("files_percent".into(), json!("100.0%"));
            values.insert("files_total".into(), json!("0"));
            values.insert("bytes".into(), json!("0"));
            values.insert("bytes_recovered".into(), json!("0"));
            values.insert("bytes_percent".into(), json!("100.0%"));
            values.insert("bytes_total".into(), json!("0"));
            values.insert("translog_ops".into(), json!("0"));
            values.insert("translog_ops_recovered".into(), json!("0"));
            values.insert("translog_ops_percent".into(), json!("100.0%"));
            rows.push(CatRow { values });
        }
    }
    (cols, rows, None)
}

fn cat_segments(store: &Store, req: &Req, spec: &str) -> (Vec<CatCol>, Vec<CatRow>, Option<ApiError>) {
    let cols = vec![
        col("index", &["i", "idx"], "index name"),
        ncol("shard", &["s", "sh"], "shard name"),
        col("prirep", &["p", "pr", "primaryOrReplica"], "primary or replica"),
        col("ip", &["ip"], "ip address"),
        col("segment", &["seg"], "segment name"),
        ncol("generation", &["g", "gen"], "segment generation"),
        ncol("docs.count", &["dc", "docsCount"], "number of docs"),
        ncol("docs.deleted", &["dd", "docsDeleted"], "deleted docs"),
        ncol("size", &["si"], "segment size"),
        ncol("size.memory", &["sm"], "segment memory"),
        col("committed", &["ic", "isCommitted"], "committed"),
        col("searchable", &["is", "isSearchable"], "searchable"),
        col("version", &["v"], "version"),
        col("compound", &["c"], "compound"),
    ];
    let indices = match resolve_indices(store, spec, true, false, false) {
        Ok(i) => i,
        Err(e) => return (cols, vec![], Some(e)),
    };
    let bytes_unit = req.param_or("bytes", "");
    let mut rows: Vec<CatRow> = vec![];
    for idx in &indices {
        if idx.closed {
            continue;
        }
        let per_shard = distribute_docs(idx.docs.len(), idx.num_shards());
        for i in 0..idx.num_shards() as usize {
            if per_shard[i] == 0 {
                continue;
            }
            let mut values = Map::new();
            values.insert("index".into(), json!(idx.name));
            values.insert("shard".into(), json!(i.to_string()));
            values.insert("prirep".into(), json!("p"));
            values.insert("ip".into(), json!(NODE_HOST));
            values.insert("segment".into(), json!(format!("_{}", i)));
            values.insert("generation".into(), json!("0"));
            values.insert("docs.count".into(), json!(per_shard[i].to_string()));
            values.insert("docs.deleted".into(), json!("0"));
            values.insert(
                "size".into(),
                json!(format_cat_bytes(per_shard[i] as i64 * 64, &bytes_unit)),
            );
            values.insert("size.memory".into(), json!("0b"));
            values.insert("committed".into(), json!("true"));
            values.insert("searchable".into(), json!("true"));
            values.insert("version".into(), json!(LUCENE_VERSION));
            values.insert("compound".into(), json!("true"));
            rows.push(CatRow { values });
        }
    }
    (cols, rows, None)
}

fn cat_tasks(req: &Req) -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("action", &["ac"], "action"),
        col("task_id", &["ti"], "task id"),
        col("parent_task_id", &["pti"], "parent task id"),
        col("type", &["ty"], "task type"),
        col("start_time", &["st"], "start time"),
        col("timestamp", &["ts"], "timestamp"),
        col("running_time", &["rt"], "running time"),
        col("ip", &["i"], "ip"),
        col("node", &["n"], "node name"),
    ];
    let actions = req.param_or("actions", "");
    let mut rows: Vec<CatRow> = vec![];
    for t in crate::nodes::synthetic_tasks() {
        if !crate::nodes::task_action_matches(&t, &actions) {
            continue;
        }
        let now = now_millis();
        let mut values = Map::new();
        values.insert("action".into(), t.get("action").cloned().unwrap_or(Value::Null));
        values.insert(
            "task_id".into(),
            json!(format!(
                "{}:{}",
                cell_string(t.get("node").unwrap_or(&Value::Null)),
                cell_string(t.get("id").unwrap_or(&Value::Null))
            )),
        );
        values.insert("type".into(), t.get("type").cloned().unwrap_or(Value::Null));
        values.insert("start_time".into(), json!(time_of_day()));
        values.insert("timestamp".into(), json!(now.to_string()));
        values.insert("running_time".into(), json!("1ms"));
        values.insert("ip".into(), json!(NODE_HOST));
        values.insert("node".into(), json!(NODE_NAME));
        values.insert(
            "parent_task_id".into(),
            t.get("parent_task_id").cloned().unwrap_or(Value::Null),
        );
        rows.push(CatRow { values });
    }
    (cols, rows)
}

fn cat_templates(store: &Store, spec: &str) -> (Vec<CatCol>, Vec<CatRow>) {
    let cols = vec![
        col("name", &["n"], "template name"),
        col("index_patterns", &["ip"], "index patterns"),
        ncol("order", &["o"], "order"),
        ncol("version", &["v"], "version"),
        col("composed_of", &["co"], "composed of"),
    ];
    let mut rows: Vec<CatRow> = vec![];
    let matches = |name: &str| -> bool {
        if spec.is_empty() || spec == "*" {
            return true;
        }
        spec.split(',').any(|pat| glob_match(pat, name))
    };
    let mut legacy: Vec<&String> = store.legacy_templates.keys().collect();
    legacy.sort();
    for name in legacy {
        if !matches(name) {
            continue;
        }
        let tpl = &store.legacy_templates[name];
        let mut values = Map::new();
        values.insert("name".into(), json!(name));
        values.insert("index_patterns".into(), json!(format_patterns(tpl.get("index_patterns"))));
        values.insert(
            "order".into(),
            json!(int_or_default(tpl.get("order"), 0).to_string()),
        );
        values.insert("version".into(), int_or_nil(tpl.get("version")));
        values.insert("composed_of".into(), json!(""));
        rows.push(CatRow { values });
    }
    let mut composable: Vec<&String> = store.composable_templates.keys().collect();
    composable.sort();
    for name in composable {
        if !matches(name) {
            continue;
        }
        let tpl = &store.composable_templates[name];
        let mut composed: Vec<String> = vec![];
        if let Some(Value::Array(a)) = tpl.get("composed_of") {
            for c in a {
                composed.push(cell_string(c));
            }
        }
        let mut values = Map::new();
        values.insert("name".into(), json!(name));
        values.insert("index_patterns".into(), json!(format_patterns(tpl.get("index_patterns"))));
        values.insert(
            "order".into(),
            json!(int_or_default(tpl.get("priority"), 0).to_string()),
        );
        values.insert("version".into(), int_or_nil(tpl.get("version")));
        values.insert("composed_of".into(), json!(format!("[{}]", composed.join(", "))));
        rows.push(CatRow { values });
    }
    (cols, rows)
}

fn sorted_index_names(store: &Store) -> Vec<String> {
    let mut names: Vec<String> = store.indices.keys().cloned().collect();
    names.sort();
    names
}

fn glob_match(pattern: &str, value: &str) -> bool {
    crate::resp::glob_match(pattern, value)
}

fn nested_object_count(idx: &crate::store::Index) -> usize {
    fn collect(props: &serde_json::Map<String, serde_json::Value>, prefix: &str, out: &mut Vec<String>) {
        for (name, raw) in props {
            let Some(def) = raw.as_object() else { continue };
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}.{}", prefix, name)
            };
            if def.get("type").and_then(|v| v.as_str()) == Some("nested") {
                out.push(path.clone());
            }
            if let Some(sub) = def.get("properties").and_then(|p| p.as_object()) {
                collect(sub, &path, out);
            }
        }
    }
    let mut paths = Vec::new();
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        collect(props, "", &mut paths);
    }
    let mut total = 0usize;
    for doc in idx.docs.values() {
        for p in &paths {
            if let Some(v) = crate::store::field_value(doc, p) {
                total += match v {
                    serde_json::Value::Array(a) => a.len(),
                    serde_json::Value::Null => 0,
                    _ => 1,
                };
            }
        }
    }
    total
}
