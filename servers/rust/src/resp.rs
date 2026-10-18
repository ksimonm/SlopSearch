use serde_json::{Map, Number, Value};

pub fn format_number(f: f64) -> String {
    if f == (f.trunc()) && f < 1e15 && f > -1e15 {
        format!("{}", f as i64)
    } else if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else {
        let s = format!("{}", f);
        s
    }
}

pub fn pretty_json(v: &Value) -> String {
    let mut buf = String::new();
    write_pretty(&mut buf, v, 0);
    buf.push('\n');
    buf
}

fn write_indent(buf: &mut String, n: usize) {
    for _ in 0..n {
        buf.push_str("  ");
    }
}

pub(crate) fn encode_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_pretty(buf: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Object(m) => {
            if m.is_empty() {
                buf.push_str("{ }");
                return;
            }
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            buf.push_str("{\n");
            for (i, k) in keys.iter().enumerate() {
                write_indent(buf, indent + 1);
                buf.push_str(&encode_string(k));
                buf.push_str(" : ");
                write_pretty(buf, &m[*k], indent + 1);
                if i < keys.len() - 1 {
                    buf.push(',');
                }
                buf.push('\n');
            }
            write_indent(buf, indent);
            buf.push('}');
        }
        Value::Array(a) => {
            if a.is_empty() {
                buf.push_str("[ ]");
                return;
            }
            buf.push_str("[\n");
            for (i, e) in a.iter().enumerate() {
                write_indent(buf, indent + 1);
                write_pretty(buf, e, indent + 1);
                if i < a.len() - 1 {
                    buf.push(',');
                }
                buf.push('\n');
            }
            write_indent(buf, indent);
            buf.push(']');
        }
        Value::String(s) => buf.push_str(&encode_string(s)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                buf.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                buf.push_str(&u.to_string());
            } else if let Some(f) = n.as_f64() {
                buf.push_str(&format_number(f));
            } else {
                buf.push_str(&n.to_string());
            }
        }
        Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        Value::Null => buf.push_str("null"),
    }
}

pub fn write_compact(buf: &mut String, v: &Value) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            buf.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                buf.push_str(&encode_string(k));
                buf.push(':');
                write_compact(buf, &m[*k]);
            }
            buf.push('}');
        }
        Value::Array(a) => {
            buf.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                write_compact(buf, e);
            }
            buf.push(']');
        }
        Value::String(s) => buf.push_str(&encode_string(s)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                buf.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                buf.push_str(&u.to_string());
            } else if let Some(f) = n.as_f64() {
                buf.push_str(&format_number(f));
            } else {
                buf.push_str(&n.to_string());
            }
        }
        Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        Value::Null => buf.push_str("null"),
    }
}

pub fn render_ordered_json(pairs: &[(&str, &Value)], pretty: bool) -> String {
    let mut buf = String::new();
    if !pretty {
        buf.push('{');
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            buf.push_str(&encode_string(k));
            buf.push(':');
            write_compact(&mut buf, v);
        }
        buf.push('}');
        return buf;
    }
    buf.push_str("{\n");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            buf.push_str(",\n");
        }
        buf.push_str("  ");
        buf.push_str(&encode_string(k));
        buf.push_str(" : ");
        write_pretty(&mut buf, v, 1);
    }
    buf.push_str("\n}\n");
    buf
}

// ---------------------------------------------------------------------- YAML

pub fn to_yaml(v: &Value) -> String {
    let mut buf = String::new();
    buf.push_str("---\n");
    write_yaml(&mut buf, v, 0);
    buf
}

fn write_yaml(buf: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            for k in keys {
                for _ in 0..indent {
                    buf.push_str("  ");
                }
                buf.push_str(k);
                buf.push(':');
                match &m[k] {
                    Value::Object(child) => {
                        if child.is_empty() {
                            buf.push_str(" {}\n");
                        } else {
                            buf.push('\n');
                            write_yaml(buf, &m[k], indent + 1);
                        }
                    }
                    Value::Array(child) => {
                        if child.is_empty() {
                            buf.push_str(" []\n");
                        } else {
                            buf.push('\n');
                            write_yaml(buf, &m[k], indent);
                        }
                    }
                    other => {
                        buf.push(' ');
                        write_yaml_scalar(buf, other);
                        buf.push('\n');
                    }
                }
            }
        }
        Value::Array(a) => {
            for e in a {
                for _ in 0..indent {
                    buf.push_str("  ");
                }
                buf.push_str("- ");
                if let Value::Object(m) = e {
                    let mut keys: Vec<&String> = m.keys().collect();
                    keys.sort();
                    for (i, k) in keys.iter().enumerate() {
                        if i > 0 {
                            for _ in 0..indent + 1 {
                                buf.push_str("  ");
                            }
                        }
                        buf.push_str(k);
                        buf.push_str(": ");
                        write_yaml_scalar(buf, &m[*k]);
                        buf.push('\n');
                    }
                } else {
                    write_yaml_scalar(buf, e);
                    buf.push('\n');
                }
            }
        }
        other => {
            write_yaml_scalar(buf, other);
            buf.push('\n');
        }
    }
}

fn write_yaml_scalar(buf: &mut String, v: &Value) {
    match v {
        Value::Object(_) | Value::Array(_) => write_compact(buf, v),
        Value::String(s) => buf.push_str(&encode_string(s)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                buf.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                buf.push_str(&u.to_string());
            } else if let Some(f) = n.as_f64() {
                buf.push_str(&format_number(f));
            } else {
                buf.push_str(&n.to_string());
            }
        }
        Value::Bool(b) => buf.push_str(if *b { "true" } else { "false" }),
        Value::Null => buf.push_str("null"),
    }
}

pub fn parse_simple_yaml(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    let mut nesting = false;
    let mut prev_key = String::new();
    let mut child = Map::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && trimmed.ends_with(':') {
            if nesting {
                out.insert(prev_key.clone(), Value::Object(child.clone()));
                child.clear();
            }
            prev_key = trimmed.trim_end_matches(':').to_string();
            nesting = true;
            continue;
        }
        let target = if nesting { &mut child } else { &mut out };
        if let Some(i) = trimmed.find(':') {
            let key = &trimmed[..i];
            let val = trimmed[i + 1..].trim();
            let value = match val {
                "" | "null" | "~" => Value::Null,
                v => Value::String(v.trim_matches(|c| c == '"' || c == '\'').to_string()),
            };
            target.insert(key.to_string(), value);
        }
    }
    if nesting {
        out.insert(prev_key, Value::Object(child));
    }
    out
}

pub fn num(v: f64) -> Value {
    if v == v.trunc() && v.abs() < 9e15 {
        Value::Number(Number::from(v as i64))
    } else {
        Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

// ---------------------------------------------------------------- filter_path

pub struct PathPattern {
    pub exclude: bool,
    pub segs: Vec<String>,
}

pub fn parse_filter_path(fp: &str) -> Vec<PathPattern> {
    let mut pats = vec![];
    for raw in fp.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let mut exclude = false;
        let mut body = raw;
        if let Some(stripped) = raw.strip_prefix('-') {
            exclude = true;
            body = stripped;
        }
        pats.push(PathPattern {
            exclude,
            segs: body.split('.').map(|s| s.to_string()).collect(),
        });
    }
    pats
}

pub fn glob_match(pat: &str, s: &str) -> bool {
    if pat == "*" || pat == s {
        return true;
    }
    if !pat.contains('*') {
        return false;
    }
    let parts: Vec<&str> = pat.split('*').collect();
    if !s.starts_with(parts[0]) {
        return false;
    }
    let mut rest = &s[parts[0].len()..];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(idx) => rest = &rest[idx + p.len()..],
            None => return false,
        }
    }
    rest.ends_with(parts[parts.len() - 1])
}

fn expand_double_star(segs: &[String]) -> Vec<Vec<String>> {
    if segs.is_empty() || segs[0] != "**" {
        return vec![segs.to_vec()];
    }
    let rest = &segs[1..];
    let mut out = vec![];
    for depth in 1..=8 {
        let mut pat = vec!["*".to_string(); depth];
        pat.extend(rest.iter().cloned());
        out.push(pat);
    }
    out
}

fn filter_node(v: &Value, pats: &[Vec<String>]) -> (Value, bool) {
    for p in pats {
        if p.is_empty() {
            return (v.clone(), true);
        }
    }
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                let mut sub: Vec<Vec<String>> = vec![];
                for p in pats {
                    if !p.is_empty() && glob_match(&p[0], k) {
                        sub.push(p[1..].to_vec());
                    }
                }
                if sub.is_empty() {
                    continue;
                }
                let (res, matched) = filter_node(val, &sub);
                if matched {
                    out.insert(k.clone(), res);
                }
            }
            let nonempty = !out.is_empty();
            (Value::Object(out), nonempty)
        }
        Value::Array(a) => {
            let mut out = vec![];
            let mut matched_any = false;
            for e in a {
                let (res, matched) = filter_node(e, pats);
                if matched {
                    out.push(res);
                    matched_any = true;
                }
            }
            (Value::Array(out), matched_any)
        }
        _ => (Value::Null, false),
    }
}

fn match_path_pattern(segs: &[String], path: &[String]) -> bool {
    if segs.is_empty() {
        return path.is_empty();
    }
    if segs[0] == "**" {
        for i in 0..=path.len() {
            if match_path_pattern(&segs[1..], &path[i..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    if !glob_match(&segs[0], &path[0]) {
        return false;
    }
    match_path_pattern(&segs[1..], &path[1..])
}

fn prune_paths(v: &mut Value, excl: &[PathPattern], path: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            let keys: Vec<String> = m.keys().cloned().collect();
            for k in keys {
                path.push(k.clone());
                let excluded = excl.iter().any(|p| match_path_pattern(&p.segs, path));
                if excluded {
                    m.remove(&k);
                    path.pop();
                    continue;
                }
                if let Some(val) = m.get_mut(&k) {
                    prune_paths(val, excl, path);
                }
                path.pop();
            }
        }
        Value::Array(a) => {
            for e in a.iter_mut() {
                prune_paths(e, excl, path);
            }
        }
        _ => {}
    }
}

pub fn filter_path_value(v: &Value, fp: &str) -> Value {
    let pats = parse_filter_path(fp);
    let mut includes: Vec<Vec<String>> = vec![];
    let mut excludes: Vec<PathPattern> = vec![];
    for p in &pats {
        if p.exclude {
            for segs in expand_double_star(&p.segs) {
                excludes.push(PathPattern {
                    exclude: true,
                    segs,
                });
            }
        } else {
            for segs in expand_double_star(&p.segs) {
                includes.push(segs);
            }
        }
    }
    let mut out = if includes.is_empty() {
        v.clone()
    } else {
        let (res, matched) = filter_node(v, &includes);
        if !matched {
            Value::Object(Map::new())
        } else {
            res
        }
    };
    if !excludes.is_empty() {
        let mut path = vec![];
        prune_paths(&mut out, &excludes, &mut path);
    }
    out
}
