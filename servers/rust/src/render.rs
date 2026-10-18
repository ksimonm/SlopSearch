use crate::errors::{err_response, json_response, text_response, ApiError};
use crate::{Req, Store};
use axum::response::Response;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct MustacheError {
    pub tag: String,
}

#[derive(Debug, Clone)]
enum Node {
    Text(String),
    Var { name: String, escaped: bool },
    Section {
        name: String,
        params: String,
        inverted: bool,
        children: Vec<Node>,
    },
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

fn parse_template(src: &str) -> Result<Vec<Node>, MustacheError> {
    let mut root: Vec<Node> = Vec::new();
    let mut stack: Vec<(String, String, bool, Vec<Node>)> = Vec::new();
    let mut open = "{{".to_string();
    let mut close = "}}".to_string();
    let mut pos = 0usize;
    let bytes = src.as_bytes();
    while pos < src.len() {
        let Some(rel) = src[pos..].find(open.as_str()) else {
            let text = &src[pos..];
            if !text.is_empty() {
                push_node(&mut root, &mut stack, Node::Text(text.to_string()));
            }
            break;
        };
        let start = pos + rel;
        if start > pos {
            push_node(
                &mut root,
                &mut stack,
                Node::Text(src[pos..start].to_string()),
            );
        }
        if open == "{{" && bytes.get(start + 2) == Some(&b'{') {
            let body_start = start + 3;
            match src[body_start..].find("}}}") {
                Some(end_rel) => {
                    let name = src[body_start..body_start + end_rel].trim().to_string();
                    push_node(&mut root, &mut stack, Node::Var { name, escaped: false });
                    pos = body_start + end_rel + 3;
                    continue;
                }
                None => {
                    return Err(MustacheError {
                        tag: String::new(),
                    })
                }
            }
        }
        let body_start = start + open.len();
        let Some(end_rel) = src[body_start..].find(close.as_str()) else {
            let text = &src[start..];
            push_node(&mut root, &mut stack, Node::Text(text.to_string()));
            break;
        };
        let tag = src[body_start..body_start + end_rel].trim().to_string();
        let _ = (&open, &close);
        pos = body_start + end_rel + close.len();
        if tag.starts_with('=') && tag.ends_with('=') {
            let inner = tag.trim_matches('=').trim();
            let parts: Vec<&str> = inner.split_whitespace().collect();
            if parts.len() == 2 {
                open = parts[0].to_string();
                close = parts[1].to_string();
            }
            continue;
        }
        if tag.starts_with('!') {
            continue;
        }
        if let Some(rest) = tag.strip_prefix('#') {
            let name = first_word(rest).to_string();
            let params = rest[name.len()..].trim().to_string();
            stack.push((name, params, false, Vec::new()));
            continue;
        }
        if let Some(rest) = tag.strip_prefix('^') {
            let name = first_word(rest).to_string();
            let params = rest[name.len()..].trim().to_string();
            stack.push((name, params, true, Vec::new()));
            continue;
        }
        if let Some(rest) = tag.strip_prefix('/') {
            let name = first_word(rest);
            if let Some((n, params, inverted, children)) = stack.pop() {
                if n != name {
                    return Err(MustacheError { tag: n });
                }
                let node = Node::Section {
                    name: n,
                    params,
                    inverted,
                    children,
                };
                push_node(&mut root, &mut stack, node);
            }
            continue;
        }
        if let Some(rest) = tag.strip_prefix('&') {
            push_node(
                &mut root,
                &mut stack,
                Node::Var {
                    name: rest.trim().to_string(),
                    escaped: false,
                },
            );
            continue;
        }
        if tag.starts_with('>') {
            continue;
        }
        push_node(
            &mut root,
            &mut stack,
            Node::Var {
                name: tag,
                escaped: true,
            },
        );
    }
    if let Some((name, _, _, _)) = stack.last() {
        return Err(MustacheError { tag: name.clone() });
    }
    Ok(root)
}

fn push_node(root: &mut Vec<Node>, stack: &mut [(String, String, bool, Vec<Node>)], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.3.push(node);
    } else {
        root.push(node);
    }
}

fn descend<'a>(val: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = val;
    for seg in path.split('.') {
        match cur {
            Value::Object(m) => cur = m.get(seg)?,
            Value::Array(a) => cur = a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        }
    }
    Some(cur)
}

fn resolve<'a>(ctx: &'a [Value], name: &str) -> Option<&'a Value> {
    if name == "." {
        return ctx.last();
    }
    for frame in ctx.iter().rev() {
        if let Some(v) = descend(frame, name) {
            return Some(v);
        }
    }
    None
}

fn json_escape(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    quoted[1..quoted.len() - 1].to_string()
}

fn java_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => {
            let parts: Vec<String> = a
                .iter()
                .enumerate()
                .map(|(i, x)| format!("{}={}", i, java_scalar(x)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, x)| format!("{}={}", k, java_scalar(x)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn interpolation(v: Option<&Value>, escaped: bool) -> String {
    let s = match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => java_scalar(other),
    };
    if escaped {
        json_escape(&s)
    } else {
        s
    }
}

fn is_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    }
}

fn java_url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn raw_text(children: &[Node]) -> String {
    let mut out = String::new();
    for n in children {
        if let Node::Text(t) = n {
            out.push_str(t);
        }
    }
    out
}

fn render_nodes(nodes: &[Node], ctx: &mut Vec<Value>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Var { name, escaped } => {
                let v = resolve(ctx, name);
                out.push_str(&interpolation(v, *escaped));
            }
            Node::Section {
                name,
                params,
                inverted,
                children,
            } => {
                if name == "toJson" || name == "join" || name == "url" {
                    let body = raw_text(children).trim().to_string();
                    let mut rendered_body = String::new();
                    if body.is_empty() {
                        render_nodes(children, ctx, &mut rendered_body);
                    }
                    let resolved = resolve(ctx, &body);
                    let direct = rendered_body.trim().to_string();
                    match name.as_str() {
                        "toJson" => {
                            if let Some(val) = resolved {
                                if !val.is_null() {
                                    out.push_str(&serde_json::to_string(val).unwrap_or_default());
                                }
                            }
                        }
                        "join" => {
                            let delim = params
                                .split("delimiter=")
                                .nth(1)
                                .map(|s| s.trim().trim_matches('\'').trim_matches('"').to_string())
                                .unwrap_or_else(|| ",".to_string());
                            if let Some(Value::Array(a)) = resolved {
                                let parts: Vec<String> = a.iter().map(java_scalar).collect();
                                out.push_str(&parts.join(&delim));
                            }
                        }
                        "url" => {
                            if !direct.is_empty() {
                                out.push_str(&java_url_encode(&direct));
                            } else if let Some(val) = resolved {
                                out.push_str(&java_url_encode(&java_scalar(val)));
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                let v = resolve(ctx, name);
                if *inverted {
                    if !is_truthy(v) {
                        render_nodes(children, ctx, out);
                    }
                } else if let Some(Value::Array(a)) = v {
                    for item in a.clone() {
                        ctx.push(item);
                        render_nodes(children, ctx, out);
                        ctx.pop();
                    }
                } else if is_truthy(v) {
                    ctx.push(v.cloned().unwrap_or(Value::Null));
                    render_nodes(children, ctx, out);
                    ctx.pop();
                }
            }
        }
    }
}

pub fn render_mustache(src: &str, params: &Value) -> Result<String, MustacheError> {
    let nodes = parse_template(src)?;
    let mut ctx = vec![params.clone()];
    let mut out = String::new();
    render_nodes(&nodes, &mut ctx, &mut out);
    Ok(out)
}

fn render_source(src: &Value, params: &Value) -> Result<String, MustacheError> {
    let text = match src {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    render_mustache(&text, params)
}

fn mustache_script_error(src: &str, tag: &str) -> ApiError {
    let reason = format!("Failed to close '{}' tag @[query-template:1]", tag);
    ApiError::new("script_exception", &reason, 400)
        .field("lang", json!("mustache"))
        .field("script", json!(src))
        .field("script_stack", json!([]))
        .caused_by(ApiError::new("mustache_exception", &reason, 400))
}

fn script_not_found(id: &str) -> ApiError {
    ApiError::new(
        "resource_not_found_exception",
        &format!("unable to find script [{}] in cluster state", id),
        404,
    )
}

fn stored_template_source(store: &Store, id: &str) -> Result<Value, ApiError> {
    let stored = store.scripts.get(id).ok_or_else(|| script_not_found(id))?;
    let script = stored.get("script").cloned().unwrap_or(Value::Null);
    let lang = script
        .get("lang")
        .and_then(|v| v.as_str())
        .unwrap_or("mustache");
    if lang != "mustache" {
        let e = ApiError::new("script_exception", "compile error", 400).caused_by(
            ApiError::new(
                "class_cast_exception",
                "Cannot cast from [int] to [java.lang.String].",
                400,
            ),
        );
        return Err(e);
    }
    Ok(script.get("source").cloned().unwrap_or(Value::Null))
}

pub fn handle_render(store: &mut Store, req: &Req, path_id: Option<&str>) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let inline = body.get("source").or_else(|| body.get("inline")).cloned();
    let body_id = body.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    let sid = path_id.map(|s| s.to_string()).or(body_id);
    let source = match inline {
        Some(s) => s,
        None => match sid {
            Some(id) => match stored_template_source(store, &id) {
                Ok(s) => s,
                Err(e) => return err_response(e),
            },
            None => {
                return err_response(ApiError::validation(
                    "Validation Failed: 1: template is missing;2: template's script type is missing;",
                ))
            }
        },
    };
    let src_text = match &source {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    match render_source(&source, &params) {
        Ok(rendered) => text_response(
            200,
            "application/json; charset=UTF-8",
            format!("{{\"template_output\":{}}}", rendered),
        ),
        Err(e) => err_response(mustache_script_error(&src_text, &e.tag)),
    }
}

fn template_source(
    store: &Store,
    body: &Value,
) -> Result<Value, ApiError> {
    if let Some(src) = body.get("source") {
        return Ok(src.clone());
    }
    if let Some(src) = body.get("inline") {
        return Ok(src.clone());
    }
    if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
        return stored_template_source(store, id);
    }
    Ok(json!({}))
}

fn apply_search_option_fixes(body: &mut Value) -> Result<(), ApiError> {
    let Some(m) = body.as_object_mut() else {
        return Ok(());
    };
    for key in ["size", "from"] {
        if let Some(v) = m.get(key).cloned() {
            if let Some(s) = v.as_str() {
                if s.is_empty() {
                    continue;
                }
                match s.parse::<i64>() {
                    Ok(n) => {
                        m.insert(key.to_string(), json!(n));
                    }
                    Err(_) => {
                        return Err(ApiError::new(
                            "number_format_exception",
                            &format!("For input string: \"{}\"", s),
                            400,
                        ))
                    }
                }
            }
        }
    }
    Ok(())
}

fn finish_search_result(mut v: Value, req: &Req, explain: bool) -> Value {
    if let Some(hits) = v
        .get_mut("hits")
        .and_then(|h| h.get_mut("hits"))
        .and_then(|h| h.as_array_mut())
    {
        for hit in hits.iter_mut() {
            if explain {
                if let Some(o) = hit.as_object_mut() {
                    o.insert(
                        "_explanation".to_string(),
                        json!({"value": 1.0, "description": "match", "details": []}),
                    );
                }
            }
        }
    }
    if req.param("rest_total_hits_as_int") == Some("true") {
        if let Some(total) = v
            .get_mut("hits")
            .and_then(|h| h.get_mut("total"))
            .and_then(|t| t.get_mut("value"))
            .cloned()
        {
            if let Some(hits) = v.get_mut("hits").and_then(|h| h.as_object_mut()) {
                hits.insert("total".to_string(), total);
            }
        }
    }
    v
}

fn resolve_for_search(
    store: &Store,
    req: &Req,
    name: &str,
) -> Result<Vec<String>, ApiError> {
    let o = crate::shared::parse_resolve_params(
        req,
        crate::shared::ResolveOpts2 {
            closed_error: true,
            ..Default::default()
        },
    );
    crate::shared::resolve_spec2(store, name, &o)
}

pub fn handle_search_template(store: &mut Store, req: &Req, name: &str) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(b) => Value::Object(b),
        Err(e) => return err_response(e),
    };
    let allowed = ["source", "inline", "id", "params", "explain", "profile"];
    if let Some(m) = body.as_object() {
        for k in m.keys() {
            if !allowed.contains(&k.as_str()) {
                let compact = serde_json::to_string(&body).unwrap_or_default();
                let col = compact
                    .find(&format!("\"{}\"", k))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    &format!("[1:{}] [search_template] unknown field [{}]", col, k),
                    400,
                ));
            }
        }
    }
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let source = match template_source(store, &body) {
        Ok(s) => s,
        Err(e) => return err_response(e),
    };
    let src_text = match &source {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    if let Some(pos) = src_text.rfind("{{") {
        if !src_text[pos + 2..].contains("}}") {
            let tag: String = src_text[pos + 2..]
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
                .to_string();
            return err_response(mustache_script_error(&src_text, &tag));
        }
    }
    let rendered = match render_source(&source, &params) {
        Ok(r) => r,
        Err(e) => return err_response(mustache_script_error(&src_text, &e.tag)),
    };
    let mut rendered_body: Value = match serde_json::from_str(&rendered) {
        Ok(v) => v,
        Err(e) => {
            return err_response(ApiError::new(
                "json_parse_exception",
                &format!("{}", e),
                400,
            ))
        }
    };
    if let Err(e) = apply_search_option_fixes(&mut rendered_body) {
        return err_response(e);
    }
    let explain = body.get("explain").and_then(|v| v.as_bool()).unwrap_or(false);
    let names = match resolve_for_search(store, req, name) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    match crate::search::run_search(store, name, &names, &rendered_body, req) {
        Ok(v) => {
            let mut v = finish_search_result(v, req, explain);
            if body.get("profile").and_then(|p| p.as_bool()).unwrap_or(false) {
                if let Some(o) = v.as_object_mut() {
                    o.insert("profile".to_string(), json!({"shards": []}));
                }
            }
            if let Some(o) = v.as_object_mut() {
                o.insert("status".to_string(), json!(200));
            }
            json_response(200, v)
        }
        Err(e) => err_response(e),
    }
}

fn error_item(e: &ApiError) -> Value {
    let inner = e
        .to_value()
        .get("error")
        .cloned()
        .unwrap_or_else(|| json!({"type": e.typ, "reason": e.reason}));
    json!({"status": e.status, "error": inner})
}

fn body_has_negative_param(body: &Value) -> Option<ApiError> {
    for key in ["from", "size"] {
        if let Some(n) = body.get(key).and_then(|v| v.as_i64()) {
            if n < 0 {
                return Some(ApiError::new(
                    "illegal_argument_exception",
                    &format!("[{}] parameter cannot be negative, found [{}]", key, n),
                    400,
                ));
            }
        }
    }
    None
}

fn resolve_header_names(
    store: &Store,
    req: &Req,
    header: &Value,
    path: &str,
) -> Result<Vec<String>, ApiError> {
    let ignore = header
        .get("ignore_unavailable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut specs: Vec<String> = Vec::new();
    if let Some(v) = header.get("index") {
        match v {
            Value::String(s) => specs.push(s.clone()),
            Value::Array(a) => {
                for x in a {
                    if let Some(s) = x.as_str() {
                        specs.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if specs.is_empty() {
        specs.push(if path.is_empty() {
            "_all".to_string()
        } else {
            path.to_string()
        });
    }
    let mut names: Vec<String> = Vec::new();
    for spec in specs {
        match resolve_for_search(store, req, &spec) {
            Ok(n) => names.extend(n),
            Err(e) => {
                if !ignore {
                    return Err(e);
                }
            }
        }
    }
    if names.is_empty() && !ignore {
        return Err(ApiError::not_found("_all"));
    }
    Ok(names)
}

pub fn handle_msearch(store: &mut Store, req: &Req, path: &str, template: bool) -> Response {
    if req.method != "POST" && req.method != "GET" {
        return crate::errors::method_not_allowed(req, &["GET", "POST"]);
    }
    let raw = String::from_utf8_lossy(&req.body).to_string();
    if !raw.is_empty() && !raw.ends_with('\n') {
        return err_response(ApiError::new(
            "illegal_argument_exception",
            "The msearch request must be terminated by a newline [\n]",
            400,
        ));
    }
    let mut lines: Vec<&str> = raw
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: no requests added;",
        ));
    }
    if let Ok(header0) = serde_json::from_str::<Value>(lines[0]) {
        if let Some(o) = header0.as_object() {
            let allowed = [
                "index",
                "search_type",
                "ignore_unavailable",
                "preference",
                "routing",
                "expand_wildcards",
                "allow_no_indices",
            ];
            for k in o.keys() {
                if !allowed.contains(&k.as_str()) {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("key [{}] is not supported in the metadata section", k),
                        400,
                    ));
                }
            }
        }
    }
    if lines.len() < 2 {
        return err_response(ApiError::validation(
            "Validation Failed: 1: no requests added;",
        ));
    }
    if lines.len() % 2 == 1 {
        return err_response(ApiError::validation(
            "Validation Failed: 1: no requests added;",
        ));
    }
    let mut responses: Vec<Value> = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        let header: Value = match serde_json::from_str(lines[i]) {
            Ok(v) => v,
            Err(e) => {
                return err_response(ApiError::new(
                    "json_parse_exception",
                    &format!("{}", e),
                    400,
                ))
            }
        };
        let mut body: Value = match serde_json::from_str(lines[i + 1]) {
            Ok(v) => v,
            Err(e) => {
                return err_response(ApiError::new(
                    "json_parse_exception",
                    &format!("{}", e),
                    400,
                ))
            }
        };
        if let Some(o) = header.as_object() {
            let allowed = [
                "index",
                "search_type",
                "ignore_unavailable",
                "preference",
                "routing",
                "expand_wildcards",
                "allow_no_indices",
            ];
            for k in o.keys() {
                if !allowed.contains(&k.as_str()) {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!("key [{}] is not supported in the metadata section", k),
                        400,
                    ));
                }
            }
        }
        i += 2;
        if template {
            let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
            let source = match template_source(store, &body) {
                Ok(s) => s,
                Err(e) => {
                    responses.push(error_item(&e));
                    continue;
                }
            };
            let rendered = match render_source(&source, &params) {
                Ok(r) => r,
                Err(e) => {
                    let src_text = match &source {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    responses.push(error_item(&mustache_script_error(&src_text, &e.tag)));
                    continue;
                }
            };
            body = match serde_json::from_str(&rendered) {
                Ok(v) => v,
                Err(e) => {
                    responses.push(error_item(&ApiError::new(
                        "json_parse_exception",
                        &format!("{}", e),
                        400,
                    )));
                    continue;
                }
            };
            if let Err(e) = apply_search_option_fixes(&mut body) {
                return err_response(e);
            }
        }
        if let Some(e) = body_has_negative_param(&body) {
            return err_response(e);
        }
        let names = match resolve_header_names(store, req, &header, path) {
            Ok(n) => n,
            Err(e) => {
                responses.push(error_item(&e));
                continue;
            }
        };
        let spec = if path.is_empty() { "_all".to_string() } else { path.to_string() };
        match crate::search::run_search(store, &spec, &names, &body, req) {
            Ok(v) => {
                let mut v = finish_search_result(v, req, false);
                if let Some(o) = v.as_object_mut() {
                    o.insert("status".to_string(), json!(200));
                }
                responses.push(v);
            }
            Err(e) => {
                if e.typ == "parsing_exception" {
                    return err_response(e);
                }
                responses.push(error_item(&e));
            }
        }
    }
    if responses.is_empty() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: no requests added;",
        ));
    }
    lines.clear();
    json_response(200, json!({"took": 1, "responses": responses}))
}
