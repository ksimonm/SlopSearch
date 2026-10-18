use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Map, Value};

use crate::resp::pretty_json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub typ: String,
    pub reason: String,
    pub status: u16,
    pub extra: Option<Value>,
    pub fields: Map<String, Value>,
    pub caused_by: Option<Box<ApiError>>,
    pub top_type: Option<String>,
    pub top_reason: Option<String>,
    pub root_cause: Option<Box<ApiError>>,
}

impl ApiError {
    pub fn new(typ: &str, reason: &str, status: u16) -> Self {
        ApiError {
            typ: typ.to_string(),
            reason: reason.to_string(),
            status,
            extra: None,
            fields: Map::new(),
            caused_by: None,
            top_type: None,
            top_reason: None,
            root_cause: None,
        }
    }

    pub fn with_fields(mut self, fields: Map<String, Value>) -> Self {
        self.fields = fields;
        self
    }

    pub fn field(mut self, k: &str, v: Value) -> Self {
        self.fields.insert(k.to_string(), v);
        self
    }

    pub fn caused_by(mut self, e: ApiError) -> Self {
        self.caused_by = Some(Box::new(e));
        self
    }

    pub fn not_found(index: &str) -> Self {
        ApiError::new(
            "index_not_found_exception",
            &format!("no such index [{}]", index),
            404,
        )
    }

    pub fn bad(reason: &str) -> Self {
        ApiError::new("illegal_argument_exception", reason, 400)
    }

    pub fn parsing(reason: &str) -> Self {
        ApiError::new("parsing_exception", reason, 400)
    }

    pub fn validation(reason: &str) -> Self {
        ApiError::new("action_request_validation_exception", reason, 400)
    }

    pub fn to_value(&self) -> Value {
        let (rc_type, rc_reason) = match &self.root_cause {
            Some(rc) => (rc.typ.clone(), rc.reason.clone()),
            None => (self.typ.clone(), self.reason.clone()),
        };
        let mut root_cause = json!({"type": rc_type, "reason": rc_reason});
        let mut err = json!({
            "root_cause": [root_cause.clone()],
            "type": self.typ.clone(),
            "reason": self.reason.clone(),
        });
        let typ = self.top_type.clone().unwrap_or_else(|| self.typ.clone());
        let reason = self
            .top_reason
            .clone()
            .unwrap_or_else(|| self.reason.clone());
        err["type"] = json!(typ);
        err["reason"] = json!(reason);

        let mut fields = self.fields.clone();
        if self.typ == "index_not_found_exception" && !fields.contains_key("index") {
            let mut name = self.reason.clone();
            if let Some(i) = name.find('[') {
                let tail = &name[i..];
                if let Some(end) = tail.find(']') {
                    name = tail[1..end].to_string();
                } else {
                    name = tail[1..].to_string();
                }
            }
            fields.insert("resource.type".to_string(), json!("index_or_alias"));
            fields.insert("resource.id".to_string(), json!(name));
            fields.insert("index_uuid".to_string(), json!("_na_"));
            fields.insert("index".to_string(), json!(name));
        }
        if self.typ == "shard_not_found_exception" {
            if let Some(extra) = &self.extra {
                fields.insert("shard".to_string(), extra.clone());
            }
        }
        if let Some(obj) = err.as_object_mut() {
            if let Some(rc_obj) = root_cause.as_object_mut() {
                for (k, v) in &fields {
                    obj.insert(k.clone(), v.clone());
                    rc_obj.insert(k.clone(), v.clone());
                }
            }
            if let Some(rc_obj) = root_cause.as_object() {
                obj.insert("root_cause".to_string(), json!([rc_obj.clone()]));
            }
            if let Some(cb) = &self.caused_by {
                obj.insert("caused_by".to_string(), nested_error_value(cb));
            }
        }
        json!({"error": err, "status": self.status})
    }
}

pub(crate) fn nested_error_value(e: &ApiError) -> Value {
    let mut v = json!({"type": e.typ, "reason": e.reason});
    if let Some(o) = v.as_object_mut() {
        for (k, val) in &e.fields {
            o.insert(k.clone(), val.clone());
        }
        if let Some(cb) = &e.caused_by {
            o.insert("caused_by".to_string(), nested_error_value(cb));
        }
    }
    v
}

pub fn json_value_response(status: u16, v: &Value) -> Response {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string());
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        body,
    )
        .into_response()
}

pub fn json_response(status: u16, v: Value) -> Response {
    json_value_response(status, &v)
}

pub fn pretty_response(status: u16, v: &Value) -> Response {
    let body = pretty_json(v);
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, "application/json; charset=UTF-8")],
        body,
    )
        .into_response()
}

pub fn text_response(status: u16, content_type: &str, body: String) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, content_type.to_string())],
        body,
    )
        .into_response()
}

pub fn err_response(e: ApiError) -> Response {
    let status = e.status;
    json_response(status, e.to_value())
}

pub fn write_simple_error(status: u16, msg: &str) -> Response {
    json_response(status, json!({"error": msg, "status": status}))
}

pub fn method_not_allowed(req: &crate::Req, allowed: &[&str]) -> Response {
    let allow = if allowed.is_empty() {
        "GET,PUT,POST,DELETE,HEAD,OPTIONS".to_string()
    } else {
        allowed.join(",")
    };
    let v = json!({
        "error": format!(
            "Incorrect HTTP method for uri [{}] and method [{}], allowed: [{}]",
            req.path, req.method, allow
        ),
        "status": 405,
    });
    let mut resp = json_response(405, v);
    resp.headers_mut().insert(
        header::ALLOW,
        allow.parse().unwrap(),
    );
    resp
}

pub fn no_handler(method: &str, path: &str) -> Response {
    json_response(
        400,
        json!({
            "error": format!("no handler found for uri [{}] and method [{}]", path, method),
        }),
    )
}
