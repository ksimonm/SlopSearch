use crate::errors::ApiError;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(f64, bool),
    Str(String),
    Op(String),
    LP,
    RP,
    LB,
    RB,
    Comma,
    Q,
    Colon,
    Dot,
    SafeDot,
    Semi,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            out.push(Tok::LP);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RP);
            i += 1;
        } else if c == '[' {
            out.push(Tok::LB);
            i += 1;
        } else if c == ']' {
            out.push(Tok::RB);
            i += 1;
        } else if c == ',' {
            out.push(Tok::Comma);
            i += 1;
        } else if c == '?' {
            if i + 1 < b.len() && b[i + 1] == '.' {
                out.push(Tok::SafeDot);
                i += 2;
            } else {
                out.push(Tok::Q);
                i += 1;
            }
        } else if c == ':' {
            out.push(Tok::Colon);
            i += 1;
        } else if c == '.' {
            out.push(Tok::Dot);
            i += 1;
        } else if c == ';' {
            out.push(Tok::Semi);
            i += 1;
        } else if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != quote {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                    match b[i] {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        other => s.push(other),
                    }
                } else {
                    s.push(b[i]);
                }
                i += 1;
            }
            if i >= b.len() {
                return Err("unterminated string literal".to_string());
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == '.') {
                i += 1;
            }
            let text: String = b[start..i].iter().collect();
            let is_int = !text.contains('.');
            let n: f64 = text
                .parse()
                .map_err(|_| format!("invalid number [{}]", text))?;
            out.push(Tok::Num(n, is_int));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < b.len()
                && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$')
            {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
        } else {
            let two: String = b[i..(i + 2).min(b.len())].iter().collect();
            if ["==", "!=", ">=", "<=", "&&", "||"].contains(&two.as_str()) {
                out.push(Tok::Op(two));
                i += 2;
            } else if "+-*/%<>!".contains(c) {
                out.push(Tok::Op(c.to_string()));
                i += 1;
            } else {
                return Err(format!("unexpected character [{}]", c));
            }
        }
    }
    Ok(out)
}

fn split_statements(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(c);
            }
            ';' if depth == 0 => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

pub fn compile_check(src: &str) -> Result<(), ApiError> {
    let src = src.trim();
    if src.is_empty() {
        return Ok(());
    }
    for st in split_statements(src) {
        let st = st.trim();
        if st.starts_with("if") {
            continue;
        }
        if st.starts_with("throw ") {
            let toks = tokenize(st).map_err(compile_err)?;
            if toks.len() < 3 {
                return Err(compile_err("invalid throw statement".to_string()));
            }
            if last_is_operator(&toks) {
                return Err(compile_err("unexpected end of expression".to_string()));
            }
            continue;
        }
        let (_, rhs) = match split_assignment(st) {
            Some((l, r)) => (l, r),
            None => (String::new(), st.to_string()),
        };
        let expr = if rhs.trim().is_empty() { st } else { rhs.as_str() };
        if expr.trim().is_empty() {
            return Err(compile_err("unexpected end of expression".to_string()));
        }
        let toks = tokenize(expr).map_err(compile_err)?;
        if toks.is_empty() {
            return Err(compile_err("unexpected end of expression".to_string()));
        }
        if last_is_operator(&toks) {
            return Err(compile_err("unexpected end of expression".to_string()));
        }
        let mut depth = 0i32;
        for t in &toks {
            match t {
                Tok::LP | Tok::LB => depth += 1,
                Tok::RP | Tok::RB => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return Err(compile_err("unbalanced parentheses".to_string()));
            }
        }
        if depth != 0 {
            return Err(compile_err("unbalanced parentheses".to_string()));
        }
    }
    Ok(())
}

fn last_is_operator(toks: &[Tok]) -> bool {
    matches!(
        toks.last(),
        Some(Tok::Op(op)) if ["+", "-", "*", "/", "%", "==", "!=", ">=", "<=", "<", ">", "&&", "||"].contains(&op.as_str())
    )
}

fn split_assignment(st: &str) -> Option<(String, String)> {
    let chars: Vec<char> = st.chars().collect();
    let mut quote: Option<char> = None;
    let mut depth = 0i32;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '=' if depth == 0 => {
                let prev = if i > 0 { Some(chars[i - 1]) } else { None };
                let next = chars.get(i + 1).copied();
                let eq_op = matches!(prev, Some('=') | Some('!') | Some('<') | Some('>'))
                    || next == Some('=');
                if !eq_op {
                    let lhs: String = chars[..i].iter().collect();
                    let rhs: String = chars[i + 1..].iter().collect();
                    return Some((lhs.trim().to_string(), rhs.trim().to_string()));
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn compile_err(msg: String) -> ApiError {
    ApiError::new("script_exception", "compile error", 400)
        .field("script_stack", serde_json::json!([msg.clone()]))
        .caused_by(ApiError::new("illegal_argument_exception", &msg, 400))
}

fn runtime_error(cause_type: &str, cause_reason: &str) -> ApiError {
    ApiError::new("script_exception", "runtime error", 400)
        .caused_by(ApiError::new(cause_type, cause_reason, 400))
}

fn npe(msg: &str) -> ApiError {
    runtime_error("null_pointer_exception", msg)
}

pub fn eval_condition(
    src: &str,
    params: &Value,
    meta: &Map<String, Value>,
    source: &Map<String, Value>,
) -> Result<bool, ApiError> {
    compile_check(src)?;
    let mut meta = meta.clone();
    let mut source = source.clone();
    let mut p = Parser::new(src, params, &mut meta, &mut source);
    let v = p.parse_expression().map_err(|e| e)?;
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(runtime_error(
            "class_cast_exception",
            &format!("cannot cast def [{}] to boolean", java_type(&other)),
        )),
    }
}

pub fn run_source(
    src: &str,
    params: &Value,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<(), ApiError> {
    compile_check(src)?;
    let statements = split_statements(src);
    for st in statements {
        execute_statement(&st, params, meta, source)?;
    }
    Ok(())
}

fn execute_statement(
    st: &str,
    params: &Value,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<(), ApiError> {
    let st = st.trim();
    if st.is_empty() {
        return Ok(());
    }
    if st.starts_with("if") && st.len() > 2 {
        let rest = st[2..].trim_start();
        if rest.starts_with('(') {
            let mut depth = 0i32;
            let chars: Vec<char> = rest.chars().collect();
            let mut quote: Option<char> = None;
            let mut close = None;
            for (i, c) in chars.iter().enumerate() {
                if let Some(q) = quote {
                    if *c == q {
                        quote = None;
                    }
                    continue;
                }
                match c {
                    '\'' | '"' => quote = Some(*c),
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(ci) = close {
                let cond: String = chars[1..ci].iter().collect();
                let block: String = chars[ci + 1..].iter().collect();
                let block = block.trim();
                let block = block
                    .strip_prefix('{')
                    .and_then(|x| x.strip_suffix('}'))
                    .unwrap_or(block);
                let val = {
                    let mut p = Parser::new(&cond, params, meta, source);
                    p.parse_expression()?
                };
                let cond_val = match val {
                    Value::Bool(b) => b,
                    _ => {
                        return Err(runtime_error(
                            "class_cast_exception",
                            "cannot cast def [def] to boolean",
                        ))
                    }
                };
                if cond_val {
                    for inner in split_statements(block) {
                        execute_statement(&inner, params, meta, source)?;
                    }
                }
                return Ok(());
            }
        }
        return Err(compile_err("invalid if statement".to_string()));
    }
    if let Some(rest) = st.strip_prefix("throw ") {
        let rest = rest.trim();
        let args = rest
            .strip_prefix("new ")
            .and_then(|r| {
                let idx = r.find('(')?;
                Some((r[..idx].to_string(), r[idx + 1..].trim_end_matches(')').to_string()))
            })
            .unwrap_or_else(|| ("IllegalArgumentException".to_string(), rest.to_string()));
        let (cls, arg) = args;
        let msg = if cls.contains("IllegalState") {
            let mut p = Parser::new(&arg, params, meta, source);
            match p.parse_expression()? {
                Value::String(s) => s,
                v => java_value(&v),
            }
        } else {
            let mut p = Parser::new(&arg, params, meta, source);
            match p.parse_expression()? {
                Value::String(s) => s,
                v => java_value(&v),
            }
        };
        let ctype = if cls.contains("IllegalState") {
            "illegal_state_exception"
        } else {
            "illegal_argument_exception"
        };
        return Err(runtime_error(ctype, &msg));
    }
    if let Some((lhs, rhs)) = split_assignment(st) {
        let mut p = Parser::new(&rhs, params, meta, source);
        let v = p.parse_expression()?;
        set_ctx_path(&lhs, v, meta, source)?;
        return Ok(());
    }
    let mut p = Parser::new(st, params, meta, source);
    let _ = p.parse_expression()?;
    Ok(())
}

fn set_ctx_path(
    path: &str,
    val: Value,
    meta: &mut Map<String, Value>,
    source: &mut Map<String, Value>,
) -> Result<(), ApiError> {
    let p = path.trim();
    if p == "ctx._source" || p.starts_with("ctx._source.") {
        return Err(npe(
            "Cannot invoke \"Object.getClass()\" because \"callArgs[0]\" is null",
        ));
    }
    let rest = p.strip_prefix("ctx.").unwrap_or(p);
    let segs: Vec<String> = rest.split('.').map(|s| s.to_string()).collect();
    if segs.is_empty() {
        return Ok(());
    }
    if segs[0].starts_with('_') {
        let mut cur = Value::Object(std::mem::take(meta));
        set_nested(&mut cur, &segs, val);
        *meta = match cur {
            Value::Object(m) => m,
            _ => Map::new(),
        };
    } else {
        let mut cur = Value::Object(std::mem::take(source));
        set_nested(&mut cur, &segs, val);
        *source = match cur {
            Value::Object(m) => m,
            _ => Map::new(),
        };
    }
    Ok(())
}

fn set_nested(root: &mut Value, segs: &[String], val: Value) {
    if segs.is_empty() {
        *root = val;
        return;
    }
    if segs.len() == 1 {
        if let Value::Object(m) = root {
            m.insert(segs[0].clone(), val);
        } else {
            *root = val;
        }
        return;
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let obj = root.as_object_mut().unwrap();
    let entry = obj
        .entry(segs[0].clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    set_nested(entry, &segs[1..], val);
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    params: &'a Value,
    meta: &'a mut Map<String, Value>,
    source: &'a mut Map<String, Value>,
}

impl<'a> Parser<'a> {
    fn new(
        src: &str,
        params: &'a Value,
        meta: &'a mut Map<String, Value>,
        source: &'a mut Map<String, Value>,
    ) -> Self {
        let toks = tokenize(src).unwrap_or_default();
        Parser {
            toks,
            pos: 0,
            params,
            meta,
            source,
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if let Some(Tok::Op(o)) = self.peek() {
            if o == op {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn parse_expression(&mut self) -> Result<Value, ApiError> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<Value, ApiError> {
        let cond = self.parse_or()?;
        if matches!(self.peek(), Some(Tok::Q)) {
            self.pos += 1;
            let a = self.parse_ternary()?;
            if matches!(self.peek(), Some(Tok::Colon)) {
                self.pos += 1;
            }
            let b = self.parse_ternary()?;
            return Ok(match cond {
                Value::Bool(true) => a,
                Value::Bool(false) => b,
                other => {
                    return Err(runtime_error(
                        "class_cast_exception",
                        &format!("cannot cast def [{}] to boolean", java_type(&other)),
                    ))
                }
            });
        }
        Ok(cond)
    }

    fn parse_or(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_and()?;
        while self.eat_op("||") {
            let right = self.parse_and()?;
            left = Value::Bool(as_bool(&left)? || as_bool(&right)?);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_equality()?;
        while self.eat_op("&&") {
            let right = self.parse_equality()?;
            left = Value::Bool(as_bool(&left)? && as_bool(&right)?);
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_comparison()?;
        loop {
            if self.eat_op("==") || self.eat_op("===") {
                let right = self.parse_comparison()?;
                left = Value::Bool(values_equal(&left, &right));
            } else if self.eat_op("!=") || self.eat_op("!==") {
                let right = self.parse_comparison()?;
                left = Value::Bool(!values_equal(&left, &right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o)) if [">", ">=", "<", "<="].contains(&o.as_str()) => o.clone(),
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_additive()?;
            let res = match (as_f64(&left), as_f64(&right)) {
                (Some(a), Some(b)) => match op.as_str() {
                    ">" => a > b,
                    ">=" => a >= b,
                    "<" => a < b,
                    _ => a <= b,
                },
                _ => match (&left, &right) {
                    (Value::String(a), Value::String(b)) => match op.as_str() {
                        ">" => a > b,
                        ">=" => a >= b,
                        "<" => a < b,
                        _ => a <= b,
                    },
                    _ => false,
                },
            };
            left = Value::Bool(res);
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o)) if o == "+" || o == "-" => o.clone(),
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_multiplicative()?;
            left = if op == "+" && (left.is_string() || right.is_string()) {
                Value::String(format!("{}{}", java_value(&left), java_value(&right)))
            } else {
                arith(&left, &right, &op)?
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Value, ApiError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(o)) if ["*", "/", "%"].contains(&o.as_str()) => o.clone(),
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = arith(&left, &right, &op)?;
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Value, ApiError> {
        if self.eat_op("!") {
            let v = self.parse_unary()?;
            return Ok(Value::Bool(!as_bool(&v)?));
        }
        if self.eat_op("-") {
            let v = self.parse_unary()?;
            return match &v {
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        Ok(Value::Number((-i).into()))
                    } else {
                        Ok(Value::Number(
                            serde_json::Number::from_f64(-n.as_f64().unwrap_or(0.0))
                                .unwrap_or_else(|| serde_json::Number::from(0)),
                        ))
                    }
                }
                _ => Err(runtime_error(
                    "class_cast_exception",
                    "cannot cast def to double",
                )),
            };
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Value, ApiError> {
        let mut v = self.parse_primary()?;
        loop {
            let safe = match self.peek() {
                Some(Tok::Dot) => false,
                Some(Tok::SafeDot) => true,
                _ => break,
            };
            self.pos += 1;
            let prop = match self.next() {
                Some(Tok::Ident(p)) => p,
                _ => break,
            };
            if matches!(self.peek(), Some(Tok::LP)) {
                self.pos += 1;
                let mut args = Vec::new();
                if !matches!(self.peek(), Some(Tok::RP)) {
                    loop {
                        args.push(self.parse_expression()?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                if matches!(self.peek(), Some(Tok::RP)) {
                    self.pos += 1;
                }
                v = self.call_method(v, &prop, args, safe)?;
            } else {
                v = self.property(v, &prop, safe)?;
            }
        }
        Ok(v)
    }

    fn parse_primary(&mut self) -> Result<Value, ApiError> {
        let tok = self
            .next()
            .ok_or_else(|| compile_err("unexpected end of expression".to_string()))?;
        match tok {
            Tok::Num(n, is_int) => {
                if is_int {
                    Ok(Value::Number(serde_json::Number::from(n as i64)))
                } else {
                    Ok(Value::Number(
                        serde_json::Number::from_f64(n)
                            .unwrap_or_else(|| serde_json::Number::from(0)),
                    ))
                }
            }
            Tok::Str(s) => Ok(Value::String(s)),
            Tok::LP => {
                let v = self.parse_expression()?;
                if matches!(self.peek(), Some(Tok::RP)) {
                    self.pos += 1;
                }
                Ok(v)
            }
            Tok::LB => {
                let mut arr = Vec::new();
                if !matches!(self.peek(), Some(Tok::RB)) {
                    loop {
                        arr.push(self.parse_expression()?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                if matches!(self.peek(), Some(Tok::RB)) {
                    self.pos += 1;
                }
                Ok(Value::Array(arr))
            }
            Tok::Ident(id) => {
                if id == "true" {
                    return Ok(Value::Bool(true));
                }
                if id == "false" {
                    return Ok(Value::Bool(false));
                }
                if id == "null" {
                    return Ok(Value::Null);
                }
                if id == "new" {
                    let _typ = match self.next() {
                        Some(Tok::Ident(t)) => t,
                        _ => String::new(),
                    };
                    let mut args = Vec::new();
                    if matches!(self.peek(), Some(Tok::LP)) {
                        self.pos += 1;
                        if !matches!(self.peek(), Some(Tok::RP)) {
                            loop {
                                args.push(self.parse_expression()?);
                                if matches!(self.peek(), Some(Tok::Comma)) {
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                        if matches!(self.peek(), Some(Tok::RP)) {
                            self.pos += 1;
                        }
                    }
                    return Ok(args.into_iter().next().unwrap_or(Value::Array(vec![])));
                }
                let mut segs = vec![id];
                let mut safe = false;
                loop {
                    match self.peek() {
                        Some(Tok::Dot) => {
                            self.pos += 1;
                            if let Some(Tok::Ident(s)) = self.next() {
                                segs.push(s);
                            } else {
                                break;
                            }
                        }
                        Some(Tok::SafeDot) => {
                            self.pos += 1;
                            safe = true;
                            if let Some(Tok::Ident(s)) = self.next() {
                                segs.push(s);
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                if matches!(self.peek(), Some(Tok::LP)) {
                    self.pos += 1;
                    if segs.len() == 2 && segs[0] == "Collections" && segs[1] == "sort" {
                        let mut path: Vec<String> = Vec::new();
                        while let Some(Tok::Ident(p)) = self.peek() {
                            path.push(p.clone());
                            self.pos += 1;
                            if matches!(self.peek(), Some(Tok::Dot)) {
                                self.pos += 1;
                            } else {
                                break;
                            }
                        }
                        while matches!(self.peek(), Some(Tok::RP)) {
                            self.pos += 1;
                        }
                        if path.first().map(|p| p == "ctx").unwrap_or(false) {
                            let subset = &path[1..];
                            if let Some(Value::Array(a)) = source_path_mut(self.source, subset) {
                                a.sort_by(|x, y| java_value(x).cmp(&java_value(y)));
                            }
                        }
                        return Ok(Value::Null);
                    }
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::RP)) {
                        loop {
                            args.push(self.parse_expression()?);
                            if matches!(self.peek(), Some(Tok::Comma)) {
                                self.pos += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    if matches!(self.peek(), Some(Tok::RP)) {
                        self.pos += 1;
                    }
                    let method = segs.pop().unwrap_or_default();
                    let recv = segs.join(".");
                    return self.call_path(&recv, &method, args, safe);
                }
                self.lookup(&segs, safe)
            }
            other => Err(compile_err(format!("unexpected token [{:?}]", other))),
        }
    }

    fn lookup(&mut self, segs: &[String], safe: bool) -> Result<Value, ApiError> {
        if segs.is_empty() {
            return Ok(Value::Null);
        }
        match segs[0].as_str() {
            "params" => {
                let mut cur = self.params.clone();
                for s in &segs[1..] {
                    cur = match cur {
                        Value::Object(m) => m.get(s).cloned().unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                    if cur.is_null() && safe {
                        return Ok(Value::Null);
                    }
                }
                Ok(cur)
            }
            "ctx" => {
                let rest = &segs[1..];
                if rest.is_empty() {
                    return Ok(Value::Object(self.source.clone()));
                }
                if rest[0] == "_source" || rest[0] == "_ingest" {
                    return Ok(Value::Null);
                }
                let mut cur: Value = if rest[0].starts_with('_') {
                    self.meta
                        .get(&rest[0])
                        .cloned()
                        .unwrap_or(Value::Null)
                } else {
                    self.source
                        .get(&rest[0])
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                for (idx, s) in rest.iter().enumerate().skip(1) {
                    if cur.is_null() {
                        if safe {
                            return Ok(Value::Null);
                        }
                        let _ = idx;
                        return Err(npe(
                            "Cannot invoke \"java.util.Map.get(Object)\" because value is null",
                        ));
                    }
                    cur = match cur {
                        Value::Object(m) => m.get(s).cloned().unwrap_or(Value::Null),
                        Value::Array(a) => s
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| a.get(i).cloned())
                            .unwrap_or(Value::Null),
                        _ => {
                            if safe {
                                return Ok(Value::Null);
                            }
                            Value::Null
                        }
                    };
                }
                Ok(cur)
            }
            "doc" => Ok(Value::Null),
            other => Ok(Value::String(other.to_string())),
        }
    }

    fn call_path(
        &mut self,
        recv: &str,
        method: &str,
        args: Vec<Value>,
        safe: bool,
    ) -> Result<Value, ApiError> {
        if recv == "Collections" && method == "sort" {
            return Ok(Value::Null);
        }
        if recv == "ctx" && method == "keySet" {
            let mut keys: Vec<String> = self.source.keys().cloned().collect();
            keys.push("_id".to_string());
            keys.push("_index".to_string());
            return Ok(Value::Array(keys.into_iter().map(Value::String).collect()));
        }
        if recv == "ctx" && method == "containsKey" {
            if let Some(Value::String(k)) = args.first() {
                return Ok(Value::Bool(self.source.contains_key(k)));
            }
            return Ok(Value::Bool(false));
        }
        if recv.starts_with("ctx.") && method == "add" {
            let path = &recv[4..];
            let segs: Vec<String> = path.split('.').map(|x| x.to_string()).collect();
            if let Some(Value::Array(a)) = source_path_mut(self.source, &segs) {
                if let Some(v) = args.into_iter().next() {
                    a.push(v);
                }
            }
            return Ok(Value::Null);
        }
        if recv == "ctx" && method == "remove" {
            if let Some(Value::String(k)) = args.first() {
                return Ok(self.source.remove(k).unwrap_or(Value::Null));
            }
            return Ok(Value::Null);
        }
        let recv_val = {
            let segs: Vec<String> = recv.split('.').map(|s| s.to_string()).collect();
            self.lookup(&segs, safe)?
        };
        self.call_method(recv_val, method, args, safe)
    }

    fn call_method(
        &mut self,
        recv: Value,
        method: &str,
        args: Vec<Value>,
        safe: bool,
    ) -> Result<Value, ApiError> {
        if recv.is_null() {
            if safe {
                return Ok(Value::Null);
            }
            return Err(npe(
                "Cannot invoke method because receiver is null",
            ));
        }
        match method {
            "toLowerCase" => Ok(Value::String(
                recv.as_str().unwrap_or_default().to_lowercase(),
            )),
            "toUpperCase" => Ok(Value::String(
                recv.as_str().unwrap_or_default().to_uppercase(),
            )),
            "trim" => Ok(Value::String(
                recv.as_str().unwrap_or_default().trim().to_string(),
            )),
            "length" => Ok(Value::Number(match &recv {
                Value::String(s) => (s.chars().count() as i64).into(),
                Value::Array(a) => (a.len() as i64).into(),
                Value::Object(o) => (o.len() as i64).into(),
                _ => 0.into(),
            })),
            "splitOnToken" | "split" => {
                let sep = args
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let s = recv.as_str().unwrap_or_default();
                let mut parts: Vec<Value> = s
                    .split(sep)
                    .map(|p| Value::String(p.to_string()))
                    .collect();
                if let Some(Value::Number(n)) = args.get(1) {
                    if let Some(limit) = n.as_i64() {
                        if limit > 0 && parts.len() > limit as usize {
                            parts.truncate(limit as usize);
                        }
                    }
                }
                Ok(Value::Array(parts))
            }
            "add" => Ok(recv),
            "get" => {
                if let (Value::Array(a), Some(Value::Number(n))) = (&recv, args.first()) {
                    if let Some(i) = n.as_i64() {
                        return Ok(a.get(i as usize).cloned().unwrap_or(Value::Null));
                    }
                }
                Ok(Value::Null)
            }
            "toString" => Ok(Value::String(java_value(&recv))),
            _ => Ok(Value::Null),
        }
    }

    fn property(&mut self, recv: Value, prop: &str, safe: bool) -> Result<Value, ApiError> {
        if recv.is_null() {
            if safe {
                return Ok(Value::Null);
            }
            return Err(npe("cannot access property of null value"));
        }
        match prop {
            "length" => self.call_method(recv, "length", vec![], safe),
            _ => Ok(Value::Null),
        }
    }
}

fn source_path_mut<'a>(
    source: &'a mut Map<String, Value>,
    segs: &[String],
) -> Option<&'a mut Value> {
    if segs.is_empty() {
        return None;
    }
    let mut cur = source.get_mut(&segs[0])?;
    for s in &segs[1..] {
        cur = match cur {
            Value::Object(m) => m.get_mut(s)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Number(x), Value::Number(y)) => {
            x.as_f64().unwrap_or(0.0) == y.as_f64().unwrap_or(0.0)
        }
        _ => a == b,
    }
}

fn arith(a: &Value, b: &Value, op: &str) -> Result<Value, ApiError> {
    let ai = a.as_i64();
    let bi = b.as_i64();
    if let (Some(x), Some(y)) = (ai, bi) {
        return match op {
            "+" => Ok(Value::Number((x + y).into())),
            "-" => Ok(Value::Number((x - y).into())),
            "*" => Ok(Value::Number((x * y).into())),
            "/" => {
                if y == 0 {
                    Err(runtime_error("arithmetic_exception", "/ by zero"))
                } else {
                    Ok(Value::Number((x / y).into()))
                }
            }
            _ => {
                if y == 0 {
                    Err(runtime_error("arithmetic_exception", "/ by zero"))
                } else {
                    Ok(Value::Number((x % y).into()))
                }
            }
        };
    }
    let x = as_f64(a).ok_or_else(|| {
        runtime_error("class_cast_exception", "cannot cast def to double")
    })?;
    let y = as_f64(b).ok_or_else(|| {
        runtime_error("class_cast_exception", "cannot cast def to double")
    })?;
    let r = match op {
        "+" => x + y,
        "-" => x - y,
        "*" => x * y,
        "/" => {
            if y == 0.0 {
                return Err(runtime_error("arithmetic_exception", "/ by zero"));
            }
            x / y
        }
        _ => x % y,
    };
    Ok(Value::Number(
        serde_json::Number::from_f64(r).unwrap_or_else(|| serde_json::Number::from(0)),
    ))
}

fn as_bool(v: &Value) -> Result<bool, ApiError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(runtime_error(
            "class_cast_exception",
            &format!("cannot cast def [{}] to boolean", java_type(other)),
        )),
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

pub fn java_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "double",
        Value::String(_) => "java.lang.String",
        Value::Array(_) => "java.util.ArrayList",
        Value::Object(_) => "java.util.LinkedHashMap",
    }
}

pub fn java_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else {
                let f = n.as_f64().unwrap_or(0.0);
                if f == f.trunc() && f.abs() < 1e15 {
                    format!("{}.0", f as i64)
                } else {
                    f.to_string()
                }
            }
        }
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(java_value).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}={}", k, java_value(&o[k])))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}
