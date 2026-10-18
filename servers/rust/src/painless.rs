// Scripting (minimal painless/expression subset). Port of servers/go/painless.go.

use crate::eval::{compare_any, integer_of, QueryCtx};
use crate::resp::format_number;
use crate::store::{to_f64, value_to_string};
use serde_json::{Map, Number, Value};

pub struct ScriptEnv<'a, 'b> {
    pub c: &'a QueryCtx<'b>,
    pub params: serde_json::Map<String, Value>,
}

impl<'a, 'b> ScriptEnv<'a, 'b> {
    pub fn resolve_path(&self, path: &str) -> Value {
        if path.starts_with("doc[") {
            let i = match path.find(']') {
                Some(i) => i,
                None => return Value::Null,
            };
            let field = path[4..i].trim_matches(|c| c == '\'' || c == '"');
            let (vals, _) = self.c.values(field);
            if vals.is_empty() {
                return Value::Null;
            }
            return vals[0].clone();
        }
        if let Some(rest) = path.strip_prefix("doc.") {
            let field = rest
                .strip_suffix(".value")
                .or_else(|| rest.strip_suffix(".size()"))
                .unwrap_or(rest);
            let (vals, _) = self.c.values(field);
            if vals.is_empty() {
                return Value::Null;
            }
            return vals[0].clone();
        }
        if path == "ctx._index" {
            return self.params.get("__ctx_index").cloned().unwrap_or(Value::Null);
        }
        if path == "ctx._id" {
            return self.params.get("__ctx_id").cloned().unwrap_or(Value::Null);
        }
        if path == "ctx._version" {
            return self.params.get("__ctx_version").cloned().unwrap_or(Value::Null);
        }
        if path == "ctx._source" {
            return self.params.get("__ctx_source").cloned().unwrap_or(Value::Null);
        }
        if let Some(rest) = path.strip_prefix("ctx._source.") {
            let mut cur = match self.params.get("__ctx_source") {
                Some(v) => v,
                None => return Value::Null,
            };
            for seg in rest.split('.') {
                match cur.get(seg) {
                    Some(v) => cur = v,
                    None => return Value::Null,
                }
            }
            return cur.clone();
        }
        if let Some(key) = path.strip_prefix("params.") {
            let mut cur = match self.params.get(key) {
                Some(v) => v,
                None => return Value::Null,
            };
            let mut rest = key;
            for seg in rest.split('.').skip(1) {
                rest = seg;
                match cur.get(seg) {
                    Some(v) => cur = v,
                    None => return Value::Null,
                }
            }
            let _ = rest;
            return cur.clone();
        }
        if path == "params" {
            return Value::Object(
                self.params
                    .iter()
                    .filter(|(k, _)| !k.starts_with("__"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            );
        }
        if let Some(v) = self.params.get(path) {
            return v.clone();
        }
        Value::Null
    }

    pub fn eval_script_expression(&self, src: &str) -> Result<Value, String> {
        let mut src = src.trim().to_string();
        if src.ends_with(';') {
            src = src.trim_end_matches(';').trim().to_string();
        }
        if let Some(rest) = src.strip_prefix("return ") {
            src = rest.trim().to_string();
        }
        if src.ends_with(';') {
            src = src.trim_end_matches(';').trim().to_string();
        }
        let mut e2 = ExprEval {
            src: src.into_bytes(),
            pos: 0,
            env: self,
        };
        let v = e2.parse_ternary()?;
        e2.skip_space();
        if e2.pos < e2.src.len() {
            return Err(format!("unexpected token at {}", e2.pos));
        }
        Ok(v)
    }
}

pub fn script_float(v: &Value) -> f64 {
    if let Some(n) = integer_of(v) {
        return n as f64;
    }
    to_f64(v).unwrap_or(0.0)
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn int_or_zero(v: &Value) -> i64 {
    if let Some(n) = integer_of(v) {
        return n;
    }
    script_float(v) as i64
}

fn value_to_script_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else if let Some(u) = n.as_u64() {
                Some(u.to_string())
            } else {
                Some(format_number(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    if a.is_null() || b.is_null() {
        return a.is_null() && b.is_null();
    }
    if let Value::String(sa) = a {
        let bs = value_to_script_string(b).unwrap_or_default();
        return *sa == bs;
    }
    script_float(a) == script_float(b)
}

fn num_f64(f: f64) -> Value {
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

struct ExprEval<'a, 'b, 'c> {
    src: Vec<u8>,
    pos: usize,
    env: &'c ScriptEnv<'a, 'b>,
}

impl<'a, 'b, 'c> ExprEval<'a, 'b, 'c> {
    fn skip_space(&mut self) {
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self, prefix: &str) -> bool {
        self.skip_space();
        self.src[self.pos..].starts_with(prefix.as_bytes())
    }

    fn eat(&mut self, prefix: &str) -> bool {
        if self.peek(prefix) {
            self.pos += prefix.len();
            true
        } else {
            false
        }
    }

    fn parse_ternary(&mut self) -> Result<Value, String> {
        let cond = self.parse_or()?;
        if self.eat("?") {
            let a = self.parse_ternary()?;
            if !self.eat(":") {
                return Err("expected ':'".to_string());
            }
            let b = self.parse_ternary()?;
            if truthy(&cond) {
                return Ok(a);
            }
            return Ok(b);
        }
        Ok(cond)
    }

    fn parse_or(&mut self) -> Result<Value, String> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_space();
            if self.peek("||") {
                self.pos += 2;
                let right = self.parse_and()?;
                left = Value::Bool(truthy(&left) || truthy(&right));
                continue;
            }
            if self.peek("|") && !self.peek("||") {
                self.pos += 1;
                let right = self.parse_and()?;
                left = Value::Number(Number::from(int_or_zero(&left) | int_or_zero(&right)));
                continue;
            }
            return Ok(left);
        }
    }

    fn parse_and(&mut self) -> Result<Value, String> {
        let mut left = self.parse_equality()?;
        loop {
            self.skip_space();
            if self.peek("&&") {
                self.pos += 2;
                let right = self.parse_equality()?;
                left = Value::Bool(truthy(&left) && truthy(&right));
                continue;
            }
            return Ok(left);
        }
    }

    fn parse_equality(&mut self) -> Result<Value, String> {
        let mut left = self.parse_comparison()?;
        loop {
            self.skip_space();
            if self.peek("==") {
                self.pos += 2;
                let right = self.parse_comparison()?;
                left = Value::Bool(values_equal(&left, &right));
            } else if self.peek("!=") {
                self.pos += 2;
                let right = self.parse_comparison()?;
                left = Value::Bool(!values_equal(&left, &right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_comparison(&mut self) -> Result<Value, String> {
        let mut left = self.parse_additive()?;
        loop {
            self.skip_space();
            if self.peek("<=") {
                self.pos += 2;
                let right = self.parse_additive()?;
                left = Value::Bool(compare_any(&left, &right) <= 0);
            } else if self.peek(">=") {
                self.pos += 2;
                let right = self.parse_additive()?;
                left = Value::Bool(compare_any(&left, &right) >= 0);
            } else if self.peek("<") {
                self.pos += 1;
                let right = self.parse_additive()?;
                left = Value::Bool(compare_any(&left, &right) < 0);
            } else if self.peek(">") {
                self.pos += 1;
                let right = self.parse_additive()?;
                left = Value::Bool(compare_any(&left, &right) > 0);
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_additive(&mut self) -> Result<Value, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            self.skip_space();
            if self.peek("+") {
                self.pos += 1;
                let right = self.parse_multiplicative()?;
                if let Value::String(ls) = &left {
                    let rs = value_to_script_string(&right).unwrap_or_default();
                    left = Value::String(format!("{}{}", ls, rs));
                } else {
                    left = num_f64(script_float(&left) + script_float(&right));
                }
            } else if self.peek("-") {
                self.pos += 1;
                let right = self.parse_multiplicative()?;
                left = num_f64(script_float(&left) - script_float(&right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Value, String> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_space();
            if self.peek("*") {
                self.pos += 1;
                let right = self.parse_unary()?;
                left = num_f64(script_float(&left) * script_float(&right));
            } else if self.peek("/") {
                self.pos += 1;
                let right = self.parse_unary()?;
                let d = script_float(&right);
                if d == 0.0 {
                    left = num_f64(f64::INFINITY);
                } else {
                    left = num_f64(script_float(&left) / d);
                }
            } else if self.peek("%") {
                self.pos += 1;
                let right = self.parse_unary()?;
                left = num_f64(script_float(&left) % script_float(&right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> Result<Value, String> {
        self.skip_space();
        if self.eat("!") && !self.peek("=") {
            let v = self.parse_unary()?;
            return Ok(Value::Bool(!truthy(&v)));
        }
        if self.peek("-") {
            self.pos += 1;
            let v = self.parse_unary()?;
            return Ok(num_f64(-script_float(&v)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Value, String> {
        self.skip_space();
        if self.pos >= self.src.len() {
            return Err("unexpected end of expression".to_string());
        }
        if self.src[self.pos] == b'(' {
            self.pos += 1;
            let v = self.parse_ternary()?;
            if !self.eat(")") {
                return Err("expected ')'".to_string());
            }
            return Ok(v);
        }
        let c = self.src[self.pos];
        if c == b'\'' || c == b'"' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos] != c {
                self.pos += 1;
            }
            let s = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
            if self.pos < self.src.len() {
                self.pos += 1;
            }
            return Ok(Value::String(s));
        }
        if c.is_ascii_digit() {
            let start = self.pos;
            while self.pos < self.src.len()
                && (self.src[self.pos].is_ascii_digit() || self.src[self.pos] == b'.')
            {
                self.pos += 1;
            }
            if self.pos < self.src.len()
                && (self.src[self.pos] == b'L' || self.src[self.pos] == b'l')
            {
                self.pos += 1;
                let text = String::from_utf8_lossy(&self.src[start..self.pos - 1]).to_string();
                let n: i64 = text.parse().unwrap_or(0);
                return Ok(Value::Number(Number::from(n)));
            }
            let text = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
            let f: f64 = text.parse().unwrap_or(0.0);
            return Ok(num_f64(f));
        }
        let start = self.pos;
        let mut depth: i32 = 0;
        while self.pos < self.src.len() {
            let ch = self.src[self.pos];
            if ch == b'[' {
                depth += 1;
                self.pos += 1;
                continue;
            }
            if ch == b']' {
                depth -= 1;
                self.pos += 1;
                continue;
            }
            if ch == b'.' && depth == 0 {
                break;
            }
            if ch == b'.'
                || ch == b'_'
                || ch == b'\''
                || ch == b'"'
                || ch.is_ascii_alphabetic()
                || ch.is_ascii_digit()
            {
                self.pos += 1;
                continue;
            }
            break;
        }
        let mut token = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
        loop {
            let save = self.pos;
            self.skip_space();
            if self.pos < self.src.len() && self.src[self.pos] == b'.' {
                self.pos += 1;
                let fstart = self.pos;
                while self.pos < self.src.len()
                    && (self.src[self.pos].is_ascii_alphabetic()
                        || self.src[self.pos].is_ascii_digit()
                        || self.src[self.pos] == b'_')
                {
                    self.pos += 1;
                }
                let member = String::from_utf8_lossy(&self.src[fstart..self.pos]).to_string();
                token = format!("{}.{}", token, member);
                if self.pos < self.src.len() && self.src[self.pos] == b'(' {
                    self.pos += 1;
                    if !self.eat(")") {
                        return Err("method args unsupported".to_string());
                    }
                    token.push_str("()");
                }
                continue;
            }
            self.pos = save;
            break;
        }
        if token == "true" {
            return Ok(Value::Bool(true));
        }
        if token == "false" {
            return Ok(Value::Bool(false));
        }
        if token == "null" {
            return Ok(Value::Null);
        }
        if self.pos < self.src.len() && self.src[self.pos] == b'(' {
            return self.eval_call(&token);
        }
        let v = self.env.resolve_path(&token);
        if let Some(base) = token.strip_suffix(".size()") {
            let field = Self::doc_field_of(base);
            let (vals, _) = self.env.c.values(&field);
            return Ok(Value::Number(Number::from(vals.len() as i64)));
        }
        if let Some(base) = token.strip_suffix(".toUpperCase()") {
            if let Some(s) = value_to_script_string(&self.env.resolve_path(base)) {
                return Ok(Value::String(s.to_uppercase()));
            }
        }
        if let Some(base) = token.strip_suffix(".toLowerCase()") {
            if let Some(s) = value_to_script_string(&self.env.resolve_path(base)) {
                return Ok(Value::String(s.to_lowercase()));
            }
        }
        Ok(v)
    }

    fn doc_field_of(path: &str) -> String {
        let p = path.trim();
        if let Some(rest) = p.strip_prefix("doc[") {
            if let Some(i) = rest.find(']') {
                return rest[..i].trim_matches(|c| c == '\'' || c == '"').to_string();
            }
        }
        if let Some(rest) = p.strip_prefix("doc.") {
            let f = rest
                .strip_suffix(".value")
                .or_else(|| rest.strip_suffix(".size()"))
                .unwrap_or(rest);
            return f.to_string();
        }
        p.to_string()
    }

    fn eval_call(&mut self, fn_name: &str) -> Result<Value, String> {
        self.skip_space();
        if self.pos >= self.src.len() || self.src[self.pos] != b'(' {
            return Err("expected '('".to_string());
        }
        self.pos += 1;
        let mut args: Vec<Value> = Vec::new();
        self.skip_space();
        if self.pos < self.src.len() && self.src[self.pos] == b')' {
            self.pos += 1;
        } else {
            loop {
                let v = self.parse_ternary()?;
                args.push(v);
                self.skip_space();
                if self.pos < self.src.len() && self.src[self.pos] == b',' {
                    self.pos += 1;
                    continue;
                }
                if self.pos < self.src.len() && self.src[self.pos] == b')' {
                    self.pos += 1;
                    break;
                }
                return Err("bad call args".to_string());
            }
        }
        match fn_name {
            "Math.max" if args.len() == 2 => {
                Ok(num_f64(script_float(&args[0]).max(script_float(&args[1]))))
            }
            "Math.min" if args.len() == 2 => {
                Ok(num_f64(script_float(&args[0]).min(script_float(&args[1]))))
            }
            "Math.abs" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).abs())),
            "Math.sqrt" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).sqrt())),
            "Math.floor" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).floor())),
            "Math.ceil" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).ceil())),
            "Math.round" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).round())),
            "Math.log" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).ln())),
            "Math.exp" if args.len() == 1 => Ok(num_f64(script_float(&args[0]).exp())),
            "Math.pow" if args.len() == 2 => Ok(num_f64(
                script_float(&args[0]).powf(script_float(&args[1])),
            )),
            _ => Err(format!("unknown function [{}]", fn_name)),
        }
    }
}

pub fn format_script_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_infinite() {
                    if f.is_sign_negative() { "-Infinity".to_string() } else { "Infinity".to_string() }
                } else if f.is_nan() {
                    "NaN".to_string()
                } else {
                    format_number(f)
                }
            }
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(format_script_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}={}", k, format_script_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn script_params_of(bm: &serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    bm.get("params")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default()
}

impl<'a> QueryCtx<'a> {
    pub fn eval_script_query(&self, body: &Value) -> Result<(bool, f64), crate::errors::ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(crate::errors::ApiError::parsing("[script] query malformed")),
        };
        let mut src = bm
            .get("script")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if src.is_empty() {
            if let Some(sm) = bm.get("script").and_then(|s| s.as_object()) {
                src = sm
                    .get("source")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                if src.is_empty() {
                    src = sm
                        .get("script")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        if src.is_empty() {
            if let Some(sm) = bm.get("script").and_then(|s| s.as_object()) {
                src = sm
                    .get("source")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
            }
        }
        let mut params = script_params_of(bm);
        if let Some(sm) = bm.get("script").and_then(|s| s.as_object()) {
            params = script_params_of(sm);
        }
        let env = ScriptEnv { c: self, params };
        match env.eval_script_expression(&src) {
            Ok(v) => {
                if truthy(&v) {
                    Ok((true, 1.0))
                } else {
                    Ok((false, 0.0))
                }
            }
            Err(e) => Err(crate::errors::ApiError::new(
                "search_phase_execution_exception",
                &format!("failed to create query: {}", e),
                400,
            )
            .caused_by(crate::errors::ApiError::new(
                "illegal_argument_exception",
                &e,
                400,
            ))),
        }
    }

    pub fn eval_script_score(&self, body: &Value) -> Result<(bool, f64), crate::errors::ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(crate::errors::ApiError::parsing("[script_score] query malformed")),
        };
        let inner = match bm.get("query").and_then(|q| q.as_object()) {
            Some(q) => q,
            None => {
                return Err(crate::errors::ApiError::parsing(
                    "[script_score] requires 'query'",
                ))
            }
        };
        let (matched, _) = self.eval(inner)?;
        if !matched {
            return Ok((false, 0.0));
        }
        let sm = match bm.get("script").and_then(|s| s.as_object()) {
            Some(s) => s,
            None => return Ok((true, 1.0)),
        };
        let src = sm.get("source").and_then(|s| s.as_str()).unwrap_or("");
        let env = ScriptEnv {
            c: self,
            params: script_params_of(sm),
        };
        match env.eval_script_expression(src) {
            Ok(v) => Ok((true, script_float(&v))),
            Err(e) => Err(crate::errors::ApiError::new(
                "search_phase_execution_exception",
                &format!("failed to create query: {}", e),
                400,
            )
            .caused_by(crate::errors::ApiError::new(
                "illegal_argument_exception",
                &e,
                400,
            ))),
        }
    }
}

pub fn _value_to_script_string_public(v: &Value) -> Option<String> {
    value_to_script_string(v)
}

// ---------------------------------------------- scripted_metric mini-executor

enum SmExpr {
    Val(Value),
    Array(Vec<SmExpr>),
    MapLit(Vec<(String, SmExpr)>),
    Path(String),
    Call {
        recv: String,
        method: String,
        args: Vec<SmExpr>,
    },
    Binary {
        op: u8,
        lhs: Box<SmExpr>,
        rhs: Box<SmExpr>,
    },
}

fn sm_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn sm_len(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Number(Number::from(a.len() as i64)),
        Value::String(s) => Value::Number(Number::from(s.chars().count() as i64)),
        Value::Object(m) => Value::Number(Number::from(m.len() as i64)),
        _ => Value::Number(Number::from(0)),
    }
}

fn sm_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    if let (Some(x), Some(y)) = (to_f64(a), to_f64(b)) {
        return x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal);
    }
    value_to_string(a)
        .unwrap_or_default()
        .cmp(&value_to_string(b).unwrap_or_default())
}

fn sm_get_path(path: &str, locals: &Map<String, Value>, env: &ScriptEnv) -> Value {
    if path.starts_with("doc") || path.starts_with("params") || path.starts_with("_value") {
        return env.resolve_path(path);
    }
    let mut segs = path.split('.');
    let head = segs.next().unwrap_or("");
    let mut cur = match locals.get(head) {
        Some(v) => v.clone(),
        None => env.resolve_path(path),
    };
    for seg in segs {
        cur = match cur {
            Value::Object(m) => m.get(seg).cloned().unwrap_or(Value::Null),
            Value::Array(a) => {
                if seg == "length" {
                    Value::Number(Number::from(a.len() as i64))
                } else {
                    Value::Null
                }
            }
            _ => Value::Null,
        };
    }
    cur
}

fn sm_path_slot<'a>(path: &str, locals: &'a mut Map<String, Value>) -> Option<&'a mut Value> {
    let mut segs: Vec<&str> = path.split('.').collect();
    if segs.is_empty() {
        return None;
    }
    let head = segs.remove(0);
    if !locals.contains_key(head) {
        locals.insert(head.to_string(), Value::Object(Map::new()));
    }
    let mut cur = locals.get_mut(head)?;
    for seg in segs {
        match cur {
            Value::Object(m) => {
                if !m.contains_key(seg) {
                    m.insert(seg.to_string(), Value::Object(Map::new()));
                }
                cur = m.get_mut(seg)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

fn sm_eval(e: &SmExpr, locals: &Map<String, Value>, env: &ScriptEnv) -> Value {
    match e {
        SmExpr::Val(v) => v.clone(),
        SmExpr::Array(items) => {
            Value::Array(items.iter().map(|i| sm_eval(i, locals, env)).collect())
        }
        SmExpr::MapLit(entries) => {
            let mut m = Map::new();
            for (k, v) in entries {
                m.insert(k.clone(), sm_eval(v, locals, env));
            }
            Value::Object(m)
        }
        SmExpr::Path(p) => sm_get_path(p, locals, env),
        SmExpr::Call { recv, method, .. } => match method.as_str() {
            "size" => sm_len(&sm_get_path(recv, locals, env)),
            _ => Value::Null,
        },
        SmExpr::Binary { op, lhs, rhs } => {
            let a = sm_eval(lhs, locals, env);
            let b = sm_eval(rhs, locals, env);
            match op {
                b'+' => {
                    if let Value::String(s) = &a {
                        Value::String(format!(
                            "{}{}",
                            s,
                            value_to_script_string(&b).unwrap_or_default()
                        ))
                    } else {
                        num_f64(script_float(&a) + script_float(&b))
                    }
                }
                b'-' => num_f64(script_float(&a) - script_float(&b)),
                b'*' => num_f64(script_float(&a) * script_float(&b)),
                b'/' => {
                    let d = script_float(&b);
                    if d == 0.0 {
                        num_f64(f64::INFINITY)
                    } else {
                        num_f64(script_float(&a) / d)
                    }
                }
                _ => Value::Null,
            }
        }
    }
}

fn sm_exec_expr_stmt(e: &SmExpr, locals: &mut Map<String, Value>, env: &ScriptEnv) {
    if let SmExpr::Call { recv, method, args } = e {
        match method.as_str() {
            "add" => {
                let v = args
                    .first()
                    .map(|a| sm_eval(a, locals, env))
                    .unwrap_or(Value::Null);
                if let Some(slot) = sm_path_slot(recv, locals) {
                    if let Value::Array(a) = slot {
                        a.push(v);
                    }
                }
            }
            "addAll" => {
                let v = args
                    .first()
                    .map(|a| sm_eval(a, locals, env))
                    .unwrap_or(Value::Null);
                if let Some(slot) = sm_path_slot(recv, locals) {
                    if let Value::Array(a) = slot {
                        if let Value::Array(src) = v {
                            a.extend(src);
                        }
                    }
                }
            }
            "sort" => {
                if let Some(SmExpr::Path(p)) = args.first() {
                    if let Some(slot) = sm_path_slot(p, locals) {
                        if let Value::Array(a) = slot {
                            a.sort_by(sm_cmp);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

struct SmParser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> SmParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && (self.src[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> u8 {
        if self.pos < self.src.len() {
            self.src[self.pos]
        } else {
            0
        }
    }

    fn word_at(&self, kw: &str) -> bool {
        let k = kw.as_bytes();
        if self.pos + k.len() > self.src.len() || &self.src[self.pos..self.pos + k.len()] != k {
            return false;
        }
        let after = self.pos + k.len();
        after >= self.src.len() || !sm_ident_char(self.src[after])
    }

    fn eat(&mut self, ch: u8) -> bool {
        self.skip_ws();
        if self.peek() == ch {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn exec_block(
        &mut self,
        locals: &mut Map<String, Value>,
        env: &ScriptEnv,
        in_brace: bool,
    ) -> Result<Value, String> {
        loop {
            self.skip_ws();
            if self.pos >= self.src.len() {
                return Ok(Value::Null);
            }
            if self.peek() == b';' {
                self.pos += 1;
                continue;
            }
            if in_brace && self.peek() == b'}' {
                self.pos += 1;
                return Ok(Value::Null);
            }
            if self.word_at("for") {
                self.pos += 3;
                self.skip_ws();
                if !self.eat(b'(') {
                    return Err("expected '(' after for".to_string());
                }
                let hstart = self.pos;
                while self.pos < self.src.len() && self.peek() != b')' {
                    self.pos += 1;
                }
                let header = String::from_utf8_lossy(&self.src[hstart..self.pos]).to_string();
                if self.pos < self.src.len() {
                    self.pos += 1;
                }
                let mut parts = header.split_whitespace();
                let var = parts.next().unwrap_or("").to_string();
                let _in = parts.next().unwrap_or("");
                let iter_src: String = parts.collect::<Vec<_>>().join(" ");
                if var.is_empty() || iter_src.is_empty() {
                    return Err(format!("bad for header [{}]", header));
                }
                let iter_expr = SmParser {
                    src: iter_src.as_bytes(),
                    pos: 0,
                }
                .parse_expr()?;
                self.skip_ws();
                if !self.eat(b'{') {
                    return Err("expected '{' after for".to_string());
                }
                let bstart = self.pos;
                let mut depth = 1i32;
                while self.pos < self.src.len() && depth > 0 {
                    match self.src[self.pos] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    self.pos += 1;
                }
                let bend = if self.pos > bstart {
                    self.pos - 1
                } else {
                    self.pos
                };
                let body = String::from_utf8_lossy(&self.src[bstart..bend]).to_string();
                let items = sm_eval(&iter_expr, locals, env);
                let arr = match items {
                    Value::Array(a) => a,
                    other => vec![other],
                };
                for it in arr {
                    locals.insert(var.clone(), it);
                    exec_statements(&body, locals, env)?;
                }
                continue;
            }
            let start = self.pos;
            let mut depth = 0i32;
            while self.pos < self.src.len() {
                match self.src[self.pos] {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b';' if depth == 0 => break,
                    b'}' if depth == 0 && in_brace => break,
                    _ => {}
                }
                self.pos += 1;
            }
            let text = String::from_utf8_lossy(&self.src[start..self.pos])
                .trim()
                .to_string();
            if self.peek() == b';' {
                self.pos += 1;
            }
            if text.is_empty() {
                continue;
            }
            if let Some(v) = exec_stmt(&text, locals, env)? {
                return Ok(v);
            }
        }
    }

    fn parse_expr(&mut self) -> Result<SmExpr, String> {
        let mut l = self.parse_multiplicative()?;
        loop {
            self.skip_ws();
            let c = self.peek();
            if c == b'+' || c == b'-' {
                if self.pos + 1 < self.src.len() && self.src[self.pos + 1] == b'=' {
                    break;
                }
                self.pos += 1;
                let r = self.parse_multiplicative()?;
                l = SmExpr::Binary {
                    op: c,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                };
            } else {
                break;
            }
        }
        Ok(l)
    }

    fn parse_multiplicative(&mut self) -> Result<SmExpr, String> {
        let mut l = self.parse_primary()?;
        loop {
            self.skip_ws();
            let c = self.peek();
            if c == b'*' || c == b'/' || c == b'%' {
                self.pos += 1;
                let r = self.parse_primary()?;
                l = SmExpr::Binary {
                    op: c,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                };
            } else {
                break;
            }
        }
        Ok(l)
    }

    fn parse_primary(&mut self) -> Result<SmExpr, String> {
        self.skip_ws();
        if self.pos >= self.src.len() {
            return Err("unexpected end of expression".to_string());
        }
        let c = self.src[self.pos];
        if c == b'(' {
            self.pos += 1;
            let e = self.parse_expr()?;
            if !self.eat(b')') {
                return Err("expected ')'".to_string());
            }
            return Ok(e);
        }
        if c == b'\'' || c == b'"' {
            self.pos += 1;
            let start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos] != c {
                self.pos += 1;
            }
            let s = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
            if self.pos < self.src.len() {
                self.pos += 1;
            }
            return Ok(SmExpr::Val(Value::String(s)));
        }
        if c.is_ascii_digit() {
            let start = self.pos;
            while self.pos < self.src.len()
                && (self.src[self.pos].is_ascii_digit() || self.src[self.pos] == b'.')
            {
                self.pos += 1;
            }
            if self.pos < self.src.len()
                && (self.src[self.pos] == b'L' || self.src[self.pos] == b'l')
            {
                self.pos += 1;
                let text = String::from_utf8_lossy(&self.src[start..self.pos - 1]).to_string();
                let n: i64 = text.trim_end_matches('.').parse().unwrap_or(0);
                return Ok(SmExpr::Val(Value::Number(Number::from(n))));
            }
            let text = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
            let f: f64 = text.parse().unwrap_or(0.0);
            return Ok(SmExpr::Val(num_f64(f)));
        }
        if c == b'[' {
            return self.parse_bracket();
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            return self.parse_ident();
        }
        Err(format!("unexpected char [{}]", c as char))
    }

    fn parse_bracket(&mut self) -> Result<SmExpr, String> {
        self.pos += 1;
        self.skip_ws();
        if self.peek() == b']' {
            self.pos += 1;
            return Ok(SmExpr::Array(vec![]));
        }
        let first = self.parse_expr()?;
        self.skip_ws();
        if self.peek() == b':' {
            self.pos += 1;
            let key = match &first {
                SmExpr::Val(Value::String(s)) => s.clone(),
                _ => return Err("map key must be a string literal".to_string()),
            };
            let mut entries = vec![(key, self.parse_expr()?)];
            loop {
                self.skip_ws();
                if self.peek() == b',' {
                    self.pos += 1;
                    let ke = self.parse_expr()?;
                    let k = match ke {
                        SmExpr::Val(Value::String(s)) => s,
                        _ => return Err("map key must be a string literal".to_string()),
                    };
                    if !self.eat(b':') {
                        return Err("expected ':' in map literal".to_string());
                    }
                    entries.push((k, self.parse_expr()?));
                    continue;
                }
                if self.peek() == b']' {
                    self.pos += 1;
                    break;
                }
                return Err("bad map literal".to_string());
            }
            return Ok(SmExpr::MapLit(entries));
        }
        let mut items = vec![first];
        loop {
            self.skip_ws();
            if self.peek() == b',' {
                self.pos += 1;
                items.push(self.parse_expr()?);
                continue;
            }
            if self.peek() == b']' {
                self.pos += 1;
                break;
            }
            return Err("bad array literal".to_string());
        }
        Ok(SmExpr::Array(items))
    }

    fn parse_ident(&mut self) -> Result<SmExpr, String> {
        let start = self.pos;
        while self.pos < self.src.len()
            && (sm_ident_char(self.src[self.pos]) || self.src[self.pos] == b'.')
        {
            self.pos += 1;
        }
        let name = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
        self.skip_ws();
        if self.peek() != b'(' {
            return Ok(SmExpr::Path(name));
        }
        self.pos += 1;
        let mut args = Vec::new();
        self.skip_ws();
        if self.peek() != b')' {
            loop {
                args.push(self.parse_expr()?);
                self.skip_ws();
                if self.peek() == b',' {
                    self.pos += 1;
                    continue;
                }
                break;
            }
        }
        self.skip_ws();
        if !self.eat(b')') {
            return Err("expected ')' after call args".to_string());
        }
        let (recv, method) = match name.rsplit_once('.') {
            Some((r, m)) => (r.to_string(), m.to_string()),
            None => (String::new(), name),
        };
        Ok(SmExpr::Call { recv, method, args })
    }
}

fn sm_find_assign(text: &str) -> Option<(usize, u8)> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut quote = 0u8;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = c,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'=' if depth == 0 && i + 1 < b.len() && b[i + 1] != b'=' => {
                let prev = if i > 0 { b[i - 1] } else { 0 };
                let op = if prev == b'+' {
                    b'+'
                } else if prev == b'-' {
                    b'-'
                } else if prev == b'=' || prev == b'!' || prev == b'<' || prev == b'>' {
                    i += 1;
                    continue;
                } else {
                    b'='
                };
                return Some((i, op));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn exec_stmt(
    text: &str,
    locals: &mut Map<String, Value>,
    env: &ScriptEnv,
) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Some(rest) = text.strip_prefix("return") {
        let boundary = rest
            .as_bytes()
            .first()
            .map(|c| !sm_ident_char(*c))
            .unwrap_or(true);
        if boundary {
            let e = SmParser {
                src: rest.trim().as_bytes(),
                pos: 0,
            }
            .parse_expr()?;
            return Ok(Some(sm_eval(&e, locals, env)));
        }
    }
    for kw in [
        "double ", "int ", "long ", "float ", "short ", "byte ", "boolean ", "char ", "String ",
        "List ", "Map ", "ArrayList ", "HashMap ", "def ", "var ",
    ] {
        if let Some(rest) = text.strip_prefix(kw) {
            return exec_stmt(rest, locals, env);
        }
    }
    if let Some((i, op)) = sm_find_assign(text) {
        let (lhs, rhs) = if op == b'=' {
            (text[..i].trim(), text[i + 1..].trim())
        } else {
            (text[..i - 1].trim(), text[i + 1..].trim())
        };
        let rexpr = SmParser {
            src: rhs.as_bytes(),
            pos: 0,
        }
        .parse_expr()?;
        let val = sm_eval(&rexpr, locals, env);
        let newv = if op == b'+' {
            let cur = sm_get_path(lhs, locals, env);
            if let Value::String(s) = &cur {
                Value::String(format!(
                    "{}{}",
                    s,
                    value_to_script_string(&val).unwrap_or_default()
                ))
            } else {
                num_f64(script_float(&cur) + script_float(&val))
            }
        } else {
            val
        };
        if let Some(slot) = sm_path_slot(lhs, locals) {
            *slot = newv;
        }
        return Ok(None);
    }
    let e = SmParser {
        src: text.as_bytes(),
        pos: 0,
    }
    .parse_expr()?;
    sm_exec_expr_stmt(&e, locals, env);
    Ok(None)
}

pub fn exec_statements(
    src: &str,
    locals: &mut Map<String, Value>,
    env: &ScriptEnv,
) -> Result<Value, String> {
    let mut p = SmParser {
        src: src.as_bytes(),
        pos: 0,
    };
    p.exec_block(locals, env, false)
}
