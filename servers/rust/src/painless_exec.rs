use crate::errors::{err_response, ApiError};
use crate::eval::QueryCtx;
use crate::painless::ScriptEnv;
use crate::painless::format_script_value;
use crate::store::Index;
use crate::{Req, Store};
use axum::response::Response;
use serde_json::{json, Value};

fn xcontent(reason: &str) -> ApiError {
    let mut e = ApiError::new("x_content_parse_exception", reason, 400);
    e.root_cause = Some(Box::new(ApiError::new("x_content_parse_exception", reason, 400)));
    e
}

fn bad_request(reason: &str) -> ApiError {
    ApiError::new("illegal_argument_exception", reason, 400)
}

pub fn handle_execute(_store: &mut Store, req: &Req, _lang: &str) -> Response {
    if req.method != "POST" {
        return crate::errors::method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(v) => v,
        Err(_) => {
            return err_response(ApiError::new(
                "parse_exception",
                "request body or source parameter is required",
                400,
            ))
        }
    };
    let bm = &body;
    if let Some(ctx) = bm.get("context").and_then(|v| v.as_str()) {
        if !matches!(ctx, "painless_test" | "filter" | "score") {
            return err_response(xcontent(&format!(
                "Failed to parse context: '{}'",
                ctx
            )));
        }
    }
    let script = match bm.get("script") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::String(_)) => {
            return err_response(xcontent(
                "script doesn't support values of type: VALUE_STRING",
            ))
        }
        Some(_) => return err_response(xcontent("failed to parse script")),
        None => return err_response(bad_request("Required [script]")),
    };
    if script.get("id").is_some() {
        return err_response(bad_request("stored scripts are not supported in this context"));
    }
    let src = script
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = script
        .get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let idx = Index::skeleton("");
    let empty = Value::Null;
    let c = QueryCtx::new(&idx, "", &empty);
    let env = ScriptEnv { c: &c, params };
    match env.eval_script_expression(&src) {
        Ok(v) => json_response_ok(json!({"result": format_script_value(&v)})),
        Err(msg) => err_response(ApiError::new("script_exception", &msg, 400)),
    }
}

fn json_response_ok(v: Value) -> Response {
    crate::errors::json_response(200, v)
}
