use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::shared::{resolve_spec2, validate_index_name, ResolveOpts2};
use crate::store::{
    ensure_properties, field_value, merge_mapping, settings_value_int, settings_value_string,
    to_f64, Store,
};
use crate::Req;

struct ReindexFailure {
    index: String,
    id: String,
    typ: String,
    rex: String,
    code: u16,
}

impl ReindexFailure {
    fn json(&self, dest_uuid: &str) -> Value {
        json!({
            "index": self.index,
            "id": self.id,
            "cause": {
                "type": self.typ,
                "reason": self.rex,
                "index": self.index,
                "shard": "0",
                "index_uuid": dest_uuid,
            },
            "status": self.code,
        })
    }
}

pub fn handle_reindex(store: &mut Store, req: &Req) -> Response {
    if req.method != "POST" {
        return method_not_allowed(req, &["POST"]);
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };
    for k in body.keys() {
        match k.as_str() {
            "source" | "dest" | "script" | "max_docs" | "conflicts" | "wait_for_completion"
            | "refresh" | "timeout" | "scroll" | "size" | "slices" => {}
            _ => {
                return err_response(ApiError::new(
                    "x_content_parse_exception",
                    &format!("[1:0] [reindex] unknown field [{}]", k),
                    400,
                ))
            }
        }
    }
    let src_raw = body.get("source").and_then(|s| s.as_object());
    let dest_raw = body.get("dest").and_then(|d| d.as_object());
    let dest_raw = match dest_raw {
        Some(d) => d,
        None => {
            return err_response(ApiError::validation(
                "Validation Failed: 1: index must be specified;",
            ))
        }
    };
    let dest_name = dest_raw
        .get("index")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    let dest_pipeline = dest_raw
        .get("pipeline")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if dest_name.is_empty() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: index must be specified;",
        ));
    }
    let src_raw = match src_raw {
        Some(s) if s.get("index").is_some() => s,
        _ => {
            return err_response(ApiError::validation(
                "Validation Failed: 1: use _all if you really want to copy from all existing indexes;",
            ))
        }
    };
    if let Some(remote) = src_raw.get("remote").and_then(|r| r.as_object()) {
        let host = remote
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string();
        return err_response(ApiError::bad(&format!(
            "[{}] not allowlisted in reindex.remote.allowlist",
            host
        )));
    }
    let mut src_specs: Vec<String> = vec![];
    match src_raw.get("index") {
        Some(Value::String(s)) => src_specs.push(s.clone()),
        Some(Value::Array(a)) => {
            for v in a {
                if let Some(s) = v.as_str() {
                    src_specs.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    let src_spec = src_specs.join(",");
    if src_spec.is_empty() {
        return err_response(ApiError::validation(
            "Validation Failed: 1: use _all if you really want to copy from all existing indexes;",
        ));
    }
    if src_spec == dest_name {
        return err_response(ApiError::validation(&format!(
            "Validation Failed: 1: reindex cannot write into an index its reading from [{}];",
            src_spec
        )));
    }
    let mut max_docs: i64 = -1;
    if let Some(v) = body.get("max_docs") {
        if !v.is_null() {
            match settings_value_int(v) {
                None => {
                    return err_response(
                        ApiError::new(
                            "x_content_parse_exception",
                            "[1:0] [reindex] failed to parse field [max_docs]",
                            400,
                        )
                        .caused_by(ApiError::bad("[max_docs] should be >= [slices]")),
                    )
                }
                Some(n) => {
                    if n < 0 {
                        return err_response(
                            ApiError::new(
                                "x_content_parse_exception",
                                "[1:0] [reindex] failed to parse field [max_docs]",
                                400,
                            )
                            .caused_by(ApiError::bad(&format!(
                                "[max_docs] parameter cannot be negative, found [{}]",
                                n
                            ))),
                        );
                    }
                    if n < 1 {
                        return err_response(
                            ApiError::new(
                                "x_content_parse_exception",
                                "[1:0] [reindex] failed to parse field [max_docs]",
                                400,
                            )
                            .caused_by(ApiError::bad("[max_docs] should be >= [slices]")),
                        );
                    }
                    max_docs = n;
                }
            }
        }
    }
    let conflicts_proceed = body.get("conflicts").and_then(|c| c.as_str()) == Some("proceed");
    let mut size: i64 = 1000;
    if let Some(v) = src_raw.get("size") {
        if let Some(n) = settings_value_int(v) {
            if n > 0 {
                size = n;
            }
        }
    }
    let op_type = dest_raw
        .get("op_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version_type = dest_raw
        .get("version_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut script_src = String::new();
    if let Some(sc) = body.get("script").and_then(|s| s.as_object()) {
        script_src = sc
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
    }
    let query = src_raw.get("query").cloned();
    let mut source_include: Option<Vec<String>> = None;
    if let Some(Value::Array(si)) = src_raw.get("_source") {
        let mut out = vec![];
        for f in si {
            if let Some(fs) = f.as_str() {
                out.push(fs.to_string());
            }
        }
        source_include = Some(out);
    }

    let mut o = ResolveOpts2::default();
    o.allow_no_indices = Some(false);
    let src_indices = match resolve_spec2(store, &src_spec, &o) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    let mut items: Vec<(String, String, Value)> = vec![];
    for sidx in &src_indices {
        let idx = &store.indices[sidx];
        let extra = crate::search::precompute_doc_values(idx);
        for id in &idx.order {
            let doc = match idx.docs.get(id) {
                Some(d) => d,
                None => continue,
            };
            if let Some(q) = &query {
                if let Some(qm) = q.as_object() {
                    match crate::search::eval_doc(idx, id, doc, &extra, qm) {
                        Ok((ok, _)) => {
                            if !ok {
                                continue;
                            }
                        }
                        Err(e) => return err_response(e),
                    }
                }
            }
            items.push((sidx.clone(), id.clone(), doc.clone()));
        }
    }
    if max_docs >= 0 && items.len() as i64 > max_docs {
        items.truncate(max_docs as usize);
    }
    let total = items.len();
    let mut batches = 0;
    if total > 0 {
        batches = (total as i64 + size - 1) / size;
    }

    let (mut created, mut updated, mut deleted, mut noops, mut version_conflicts) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut failures: Vec<Value> = vec![];
    let mut dest_name_err = validate_index_name(&dest_name).err();
    let mut dest_uuid = match store.indices.get(&dest_name) {
        Some(i) => i.uuid.clone(),
        None => "_na_".to_string(),
    };
    let mut dest_created = store.indices.contains_key(&dest_name);

    for (_it_idx, it_id, it_doc) in items {
        let mut doc = match it_doc.as_object() {
            Some(m) => m.clone(),
            None => Map::new(),
        };
        let mut id = it_id.clone();
        if !script_src.is_empty() {
            let mut op = String::new();
            run_reindex_script(&script_src, &mut doc, &mut id, &mut op);
            if op == "noop" {
                noops += 1;
                continue;
            }
            if op == "delete" {
                deleted += 1;
                continue;
            }
        }
        if let Some(pname) = &dest_pipeline {
            let mut pm = Map::new();
            pm.insert("_index".to_string(), json!(dest_name));
            pm.insert("_id".to_string(), json!(id));
            match crate::ingest::run_ingest_pipeline(store, pname, doc.clone(), pm) {
                Ok(out) => {
                    if out.dropped {
                        noops += 1;
                        continue;
                    }
                    doc = out.source;
                    if let Some(nid) = out.meta.get("_id").and_then(|v| v.as_str()) {
                        if !nid.is_empty() {
                            id = nid.to_string();
                        }
                    }
                }
                Err(e) => {
                    failures.push(
                        ReindexFailure {
                            index: dest_name.clone(),
                            id: it_id.clone(),
                            typ: e.typ.clone(),
                            rex: e.reason.clone(),
                            code: e.status,
                        }
                        .json("_na_"),
                    );
                    continue;
                }
            }
        }
        if !dest_created {
            if let Some(e) = &dest_name_err {
                failures.push(
                    ReindexFailure {
                        index: dest_name.clone(),
                        id: it_id.clone(),
                        typ: e.typ.clone(),
                        rex: e.reason.clone(),
                        code: e.status,
                    }
                    .json("_na_"),
                );
                continue;
            }
            match crate::admin::create_index_internal(store, &dest_name, &Map::new(), false) {
                Ok(()) => {
                    dest_created = true;
                    dest_uuid = store.indices[&dest_name].uuid.clone();
                    dest_name_err = None;
                }
                Err(e) => {
                    failures.push(
                        ReindexFailure {
                            index: dest_name.clone(),
                            id: it_id.clone(),
                            typ: e.typ.clone(),
                            rex: e.reason.clone(),
                            code: e.status,
                        }
                        .json("_na_"),
                    );
                    continue;
                }
            }
        }
        let existing = store.indices[&dest_name].docs.contains_key(&id);
        if existing && (op_type == "create" || version_type == "external" || version_type == "external_gte")
        {
            version_conflicts += 1;
            if !conflicts_proceed {
                let reason = if op_type == "create" {
                    format!(
                        "[{}]: version conflict, document already exists (current version [1])",
                        id
                    )
                } else {
                    format!(
                        "[{}]: version conflict, current version [1] is higher or equal to the one provided [1]",
                        id
                    )
                };
                failures.push(
                    ReindexFailure {
                        index: dest_name.clone(),
                        id: it_id.clone(),
                        typ: "version_conflict_engine_exception".to_string(),
                        rex: reason,
                        code: 409,
                    }
                    .json(&dest_uuid),
                );
            }
            continue;
        }
        if let Some(include) = &source_include {
            let mut filtered = Map::new();
            for f in include {
                if let Some(v) = doc.get(f) {
                    filtered.insert(f.clone(), v.clone());
                }
            }
            doc = filtered;
        }
        if existing {
            updated += 1;
        } else {
            created += 1;
        }
        let vres = {
            let idx = store.indices.get_mut(&dest_name).unwrap();
            {
                let props = ensure_properties(&mut idx.mappings);
                merge_mapping(props, &doc);
            }
            idx.ensure_field_types();
            let mut doc_mut = doc.clone();
            let res = crate::fields::compute_doc_values(idx, &id, &mut doc_mut);
            if res.is_ok() {
                doc = doc_mut;
            }
            res
        };
        if let Err(e) = vres {
            if existing {
                updated -= 1;
            } else {
                created -= 1;
            }
            failures.push(
                ReindexFailure {
                    index: dest_name.clone(),
                    id: id.clone(),
                    typ: e.typ.clone(),
                    rex: e.reason.clone(),
                    code: e.status,
                }
                .json(&dest_uuid),
            );
            continue;
        }
        let idx = store.indices.get_mut(&dest_name).unwrap();
        if !existing {
            idx.order.push(id.clone());
        }
        idx.docs.insert(id.clone(), Value::Object(doc));
        idx.index_total += 1;
    }
    if dest_created {
        store.bump_state();
    }
    let status = if version_conflicts > 0 && !conflicts_proceed {
        409
    } else if !failures.is_empty() {
        400
    } else {
        200
    };
    json_response(
        status,
        json!({
            "took": 1,
            "timed_out": false,
            "total": total,
            "updated": updated,
            "created": created,
            "deleted": deleted,
            "batches": batches,
            "version_conflicts": version_conflicts,
            "noops": noops,
            "retries": {"bulk": 0, "search": 0},
            "throttled_millis": 0,
            "requests_per_second": -1.0,
            "throttled_until_millis": 0,
            "failures": failures,
        }),
    )
}

// ---------------------------------------------------------------- script subset

fn split_reindex_statements(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b';' if depth == 0 => {
                let st = s[start..i].trim();
                if !st.is_empty() {
                    out.push(st.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let st = s[start..].trim();
    if !st.is_empty() {
        out.push(st.to_string());
    }
    out
}

fn match_close_brace(s: &str, open: usize) -> usize {
    let mut depth = 0i32;
    let bytes = s.as_bytes();
    for i in open..bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    s.len().saturating_sub(1)
}

fn match_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote = 0u8;
    let bytes = s.as_bytes();
    for i in open..bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = c,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn reindex_source_get(doc: &Map<String, Value>, reference: &str) -> Option<Value> {
    let mut r = reference.trim();
    r = r.strip_prefix("ctx._source.").unwrap_or(r);
    r = r.strip_prefix("ctx._source").unwrap_or(r);
    let r = r.trim_matches(|c| c == '[' || c == ']' || c == '\'' || c == '"');
    if r.is_empty() {
        return None;
    }
    field_value(&Value::Object(doc.clone()), r).cloned()
}

fn reindex_literal(tok: &str) -> Option<Value> {
    let tok = tok.trim();
    let bytes = tok.as_bytes();
    if tok.len() >= 2 && (bytes[0] == b'\'' || bytes[0] == b'"') && bytes[tok.len() - 1] == bytes[0] {
        return Some(json!(&tok[1..tok.len() - 1]));
    }
    if let Ok(f) = tok.parse::<f64>() {
        return Some(json!(f));
    }
    None
}

fn split_top_level_expr(expr: &str, op: u8) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0i32;
    let mut quote = 0u8;
    let mut start = 0usize;
    let bytes = expr.as_bytes();
    for i in 0..bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = c,
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {
                if c == op && depth == 0 {
                    out.push(expr[start..i].to_string());
                    start = i + 1;
                }
            }
        }
    }
    out.push(expr[start..].to_string());
    out
}

fn eval_reindex_expr(expr: &str, doc: &Map<String, Value>, id: &str) -> Value {
    let expr = expr.trim();
    if expr == "ctx._id" || expr == "ctx.id" {
        return json!(id);
    }
    if let Some(v) = reindex_literal(expr) {
        return v;
    }
    if let Some(v) = reindex_source_get(doc, expr) {
        if expr.trim_start().starts_with("ctx._source") {
            return v;
        }
    }
    let parts = split_top_level_expr(expr, b'+');
    if parts.len() > 1 {
        let mut out = String::new();
        for p in &parts {
            let v = eval_reindex_expr(p, doc, id);
            out.push_str(&settings_value_string(&v));
        }
        return json!(out);
    }
    for op_ch in [b'*', b'/', b'-', b'+'] {
        let sub = split_top_level_expr(expr, op_ch);
        if sub.len() == 2 {
            let lv = eval_reindex_expr(&sub[0], doc, id);
            let rv = eval_reindex_expr(&sub[1], doc, id);
            let lf = to_f64(&lv);
            let rf = to_f64(&rv);
            match (lf, rf) {
                (Some(lf), Some(rf)) => match op_ch {
                    b'*' => return json!(lf * rf),
                    b'/' => {
                        if rf != 0.0 {
                            return json!(lf / rf);
                        }
                        return json!(lf);
                    }
                    b'-' => return json!(lf - rf),
                    b'+' => return json!(lf + rf),
                    _ => {}
                },
                _ => {
                    if op_ch == b'+' {
                        return json!(format!(
                            "{}{}",
                            settings_value_string(&lv),
                            settings_value_string(&rv)
                        ));
                    }
                    return lv;
                }
            }
        }
    }
    json!(expr)
}

fn index_outside_quotes(s: &str, sub: &str) -> Option<usize> {
    let mut quote = 0u8;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + sub.len() <= bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        if c == b'\'' || c == b'"' {
            quote = c;
            i += 1;
            continue;
        }
        if s[i..].starts_with(sub) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn eval_reindex_cond(cond: &str, doc: &Map<String, Value>) -> bool {
    for op in ["==", "!=", ">=", "<=", ">", "<"] {
        let i = match index_outside_quotes(cond, op) {
            Some(i) => i,
            None => continue,
        };
        let lhs = cond[..i].trim();
        let rhs = cond[i + op.len()..].trim();
        let lv = reindex_source_get(doc, lhs);
        let mut rv = reindex_literal(rhs);
        if rv.is_none() {
            rv = reindex_source_get(doc, rhs);
        }
        let (lv, rv) = match (lv, rv) {
            (Some(l), Some(r)) => (l, r),
            _ => return false,
        };
        if let (Value::String(ls), Value::String(rs)) = (&lv, &rv) {
            return match op {
                "==" => ls == rs,
                "!=" => ls != rs,
                _ => false,
            };
        }
        let lf = to_f64(&lv).unwrap_or(0.0);
        let rf = to_f64(&rv).unwrap_or(0.0);
        return match op {
            "==" => lf == rf,
            "!=" => lf != rf,
            ">" => lf > rf,
            "<" => lf < rf,
            ">=" => lf >= rf,
            "<=" => lf <= rf,
            _ => false,
        };
    }
    false
}

fn run_reindex_script(code: &str, doc: &mut Map<String, Value>, id: &mut String, op: &mut String) {
    for st in split_reindex_statements(code) {
        exec_reindex_stmt(&st, doc, id, op);
    }
}

fn exec_reindex_stmt(st: &str, doc: &mut Map<String, Value>, id: &mut String, op: &mut String) {
    if let Some(rest) = st.strip_prefix("if") {
        if !st.starts_with("if") {
            return;
        }
        let rest = rest;
        let open = match rest.find('(') {
            Some(o) => o,
            None => return,
        };
        let close = match match_paren(rest, open) {
            Some(c) => c,
            None => return,
        };
        let cond = &rest[open + 1..close];
        let after = rest[close + 1..].trim();
        let (body, remainder) = if after.strip_prefix('{').is_some() {
            let end = match_close_brace(after, 0);
            let body = &after[1..end];
            let rem = after[end + 1..].trim();
            (body.to_string(), rem.to_string())
        } else {
            (after.to_string(), String::new())
        };
        if eval_reindex_cond(cond, doc) {
            run_reindex_script(&body, doc, id, op);
        }
        if !remainder.is_empty() {
            run_reindex_script(&remainder, doc, id, op);
        }
        return;
    }
    let eq = match index_outside_quotes(st, "=") {
        Some(e) => e,
        None => return,
    };
    let lhs = st[..eq].trim();
    let rhs = st[eq + 1..].trim();
    let lhs = lhs.strip_prefix("ctx.").unwrap_or(lhs);
    match lhs {
        "op" => {
            if let Some(v) = reindex_literal(rhs) {
                *op = settings_value_string(&v);
            }
        }
        "_id" => {
            let v = eval_reindex_expr(rhs, doc, id);
            *id = settings_value_string(&v);
        }
        _ => {
            if let Some(rest) = lhs.strip_prefix("_source.") {
                let field = rest.trim_matches(|c| c == '[' || c == ']' || c == '\'' || c == '"');
                let v = eval_reindex_expr(rhs, doc, id);
                doc.insert(field.to_string(), v);
            } else if lhs.starts_with("_source[") {
                let field = lhs.trim_start_matches("_source").trim_matches(|c| c == '[' || c == ']' || c == '\'' || c == '"');
                let v = eval_reindex_expr(rhs, doc, id);
                doc.insert(field.to_string(), v);
            }
        }
    }
}
