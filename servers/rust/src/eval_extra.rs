// Extra query evaluators: multi_match, nested/join, geo, span, intervals,
// query_string parser, function_score, more_like_this, knn.
// Port of servers/go/eval_extra.go.

use crate::eval::*;
use crate::fields::{doc_values_for, preview_value, value_scalar_string};
use crate::painless::{script_float, ScriptEnv};
use crate::store::*;
use regex::Regex;
use serde_json::{json, Map, Number, Value};

fn err_parsing(reason: &str) -> ApiError {
    ApiError::parsing(reason)
}

fn preview(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match &s[split..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        "M" => 30 * 86_400_000,
        "y" => 365 * 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

fn parse_distance_km(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')?;
    let n: f64 = s[..split].parse().ok()?;
    match &s[split..] {
        "km" => Some(n),
        "m" => Some(n / 1000.0),
        "mi" | "miles" => Some(n * 1.609344),
        "yd" => Some(n * 0.0009144),
        "ft" => Some(n * 0.0003048),
        "nmi" => Some(n * 1.852),
        _ => None,
    }
}

fn geo_pair(v: &Value) -> Option<(f64, f64)> {
    if let Some(m) = v.as_object() {
        let lat = m.get("lat").and_then(to_f64)?;
        let lon = m.get("lon").and_then(to_f64)?;
        return Some((lat, lon));
    }
    if let Value::Array(a) = v {
        if a.len() == 2 {
            return Some((a[1].as_f64()?, a[0].as_f64()?));
        }
    }
    if let Some(s) = v.as_str() {
        let parts: Vec<&str> = s.split(',').collect();
        if parts.len() == 2 {
            return Some((parts[0].trim().parse().ok()?, parts[1].trim().parse().ok()?));
        }
    }
    None
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0088;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub(crate) fn shard_error(status: u16, root_type: &str, reason: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", status);
    let cause = ApiError::new(root_type, reason, status);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e
}


fn strip_field_boost(f: &str) -> (String, f64) {
    if let Some(i) = f.find('^') {
        if let Ok(b) = f[i + 1..].parse::<f64>() {
            return (f[..i].to_string(), b);
        }
        return (f[..i].to_string(), 1.0);
    }
    (f.to_string(), 1.0)
}

fn fields_of(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(value_to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

impl<'a> QueryCtx<'a> {
    pub fn eval_multi_match(&self, bm: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        let query = bm
            .get("query")
            .and_then(value_to_string)
            .unwrap_or_default();
        if query.is_empty() {
            return Err(err_parsing("[multi_match] requires 'query'"));
        }
        let typ = bm
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("best_fields")
            .to_string();
        if !matches!(
            typ.as_str(),
            "best_fields" | "most_fields" | "cross_fields" | "phrase" | "phrase_prefix"
                | "bool_prefix"
        ) {
            return Err(ApiError::new(
                "parse_exception",
                &format!(
                    "failed to parse [multi_match] query type [{}]. unknown type.",
                    typ
                ),
                400,
            ));
        }
        if typ == "cross_fields" && bm.get("fuzziness").is_some() {
            return Err(err_parsing(
                "Fuzziness not allowed for type [cross_fields]",
            ));
        }
        let mut fields = fields_of(bm.get("fields"));
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let tie = bm.get("tie_breaker").and_then(to_f64).unwrap_or(0.0);
        if fields.is_empty() {
            fields = vec!["*".to_string()];
        }
        let mut expanded: Vec<String> = Vec::new();
        for f in &fields {
            let (name, _) = strip_field_boost(f);
            if name.contains('*') || name.contains('?') {
                let re = wildcard_regex(&name);
                if let Some(vals) = self.values_map() {
                    for (path, vals) in vals {
                        if re.is_match(path)
                            && !array_values(vals).is_empty()
                            && is_wildcard_searchable(self.idx, path)
                        {
                            expanded.push(path.clone());
                        }
                    }
                }
                continue;
            }
            expanded.push(f.clone());
        }
        if typ == "cross_fields" {
            let mut union: Vec<String> = Vec::new();
            for f in &expanded {
                let (name, _) = strip_field_boost(f);
                for v in self.must_values(&name) {
                    let s = value_scalar_string(&v).unwrap_or_default();
                    union.extend(analyze(&s));
                }
            }
            let qtokens = analyze(&query);
            if qtokens.is_empty() || union.is_empty() {
                return Ok((false, 0.0));
            }
            let operator = bm
                .get("operator")
                .and_then(|o| o.as_str())
                .unwrap_or("or")
                .to_lowercase();
            let mut required = if operator == "and" { qtokens.len() } else { 1 };
            if let Some(spec) = bm.get("minimum_should_match") {
                required = minimum_should_match_checked(spec, qtokens.len()).unwrap_or(required);
            }
            let mut matched = 0usize;
            for qt in &qtokens {
                if union.iter().any(|t| t == qt) {
                    matched += 1;
                }
            }
            if matched >= required {
                return Ok((true, boost * matched as f64));
            }
            return Ok((false, 0.0));
        }
        let mut best = 0.0;
        let mut sum = 0.0;
        let mut matched_any = false;
        match typ.as_str() {
            "phrase" | "phrase_prefix" => {
                let mm_slop = bm.get("slop").and_then(to_f64).unwrap_or(0.0);
                for f in &expanded {
                    let (name, fb) = strip_field_boost(f);
                    let ok = if typ == "phrase" {
                        let mut mv = Map::new();
                        mv.insert("query".to_string(), Value::String(query.clone()));
                        if let Some(sl) = bm.get("slop") {
                            mv.insert("slop".to_string(), sl.clone());
                        }
                        self.eval_match_phrase(&name, &Value::Object(mv), boost * fb, mm_slop)?
                            .0
                    } else {
                        let mut inner = Map::new();
                        inner.insert("query".to_string(), Value::String(query.clone()));
                        inner.insert("boost".to_string(), json!(boost * fb));
                        if let Some(sl) = bm.get("slop") {
                            inner.insert("slop".to_string(), sl.clone());
                        }
                        let mut bm2 = Map::new();
                        bm2.insert(name.clone(), Value::Object(inner));
                        let mut q = Map::new();
                        q.insert("match_phrase_prefix".to_string(), Value::Object(bm2));
                        self.eval(&q)?.0
                    };
                    if ok {
                        matched_any = true;
                        sum += fb;
                    }
                }
            }
            "bool_prefix" => {
                for f in &expanded {
                    let (name, fb) = strip_field_boost(f);
                    let (ok, sc) =
                        self.eval_match_bool_prefix(&name, &Value::String(query.clone()), boost * fb)?;
                    if ok {
                        matched_any = true;
                        sum += sc;
                    }
                }
            }
            _ => {
                let mut raw_map = Map::new();
                raw_map.insert("query".to_string(), Value::String(query.clone()));
                if let Some(op) = bm.get("operator") {
                    raw_map.insert("operator".to_string(), op.clone());
                }
                if let Some(op) = bm.get("default_operator") {
                    raw_map.insert("operator".to_string(), op.clone());
                }
                if let Some(fz) = bm.get("fuzziness") {
                    raw_map.insert("fuzziness".to_string(), fz.clone());
                }
                if let Some(l) = bm.get("lenient") {
                    raw_map.insert("lenient".to_string(), l.clone());
                }
                if let Some(m) = bm.get("minimum_should_match") {
                    raw_map.insert("minimum_should_match".to_string(), m.clone());
                }
                let raw = Value::Object(raw_map);
                for f in &expanded {
                    let (name, fb) = strip_field_boost(f);
                    let (ok, sc) = self.eval_match(&name, &raw, boost * fb)?;
                    if ok {
                        matched_any = true;
                        if sc > best {
                            best = sc;
                        }
                        sum += sc;
                    }
                }
            }
        }
        if !matched_any {
            return Ok((false, 0.0));
        }
        match typ.as_str() {
            "most_fields" | "cross_fields" | "bool_prefix" => Ok((true, sum)),
            _ => {
                if tie > 0.0 {
                    Ok((true, best + tie * (sum - best)))
                } else {
                    Ok((true, best))
                }
            }
        }
    }

    pub fn eval_combined_fields(&self, bm: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        let query = bm
            .get("query")
            .and_then(value_to_string)
            .unwrap_or_default();
        let raw_fields = fields_of(bm.get("fields"));
        if raw_fields.is_empty() {
            return Ok((false, 0.0));
        }
        let mut fields: Vec<(String, f64)> = Vec::new();
        for f in &raw_fields {
            let (name, fb) = strip_field_boost(f);
            if fb < 1.0 {
                let mut e = ApiError::new(
                    "search_phase_execution_exception",
                    "all shards failed",
                    400,
                );
                e.root_cause = Some(Box::new(ApiError::new(
                    "query_shard_exception",
                    "weight must be greater or equal to 1",
                    400,
                )));
                return Err(e);
            }
            let names = if name.contains('*') || name.contains('?') {
                expand_field_patterns(self.idx, &[name.clone()])
            } else {
                vec![name.clone()]
            };
            for n in names {
                let ftype = mapping_field_def(self.idx, &n)
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if ftype != "text" {
                    if ftype.is_empty() {
                        continue;
                    }
                    let reason = format!(
                        "Field [{}] of type [{}] does not support [combined_fields] queries",
                        n, ftype
                    );
                    let mut e = ApiError::new(
                        "search_phase_execution_exception",
                        "all shards failed",
                        400,
                    );
                    e.root_cause = Some(Box::new(ApiError::new(
                        "query_shard_exception",
                        &reason,
                        400,
                    )));
                    return Err(e);
                }
                fields.push((n, fb));
            }
        }
        if fields.is_empty() {
            return Ok((false, 0.0));
        }
        let qtokens = analyze(&query);
        if qtokens.is_empty() {
            return Ok((false, 0.0));
        }
        let operator = bm
            .get("operator")
            .and_then(|o| o.as_str())
            .unwrap_or("or")
            .to_lowercase();
        let mut required = if operator == "and" { qtokens.len() } else { 1 };
        if let Some(spec) = bm.get("minimum_should_match") {
            required = minimum_should_match_checked(spec, qtokens.len()).unwrap_or(required);
        }
        let mut tokens: Vec<(String, f64)> = Vec::new();
        for (name, fb) in &fields {
            for v in self.must_values(name) {
                let s = value_scalar_string(&v).unwrap_or_default();
                for t in analyze(&s) {
                    tokens.push((t, *fb));
                }
            }
        }
        if tokens.is_empty() {
            return Ok((false, 0.0));
        }
        let mut matched = 0usize;
        let mut score = 0.0;
        for qt in &qtokens {
            let mut term_score = 0.0;
            let mut found = false;
            for (t, fb) in &tokens {
                if t == qt {
                    found = true;
                    if *fb > term_score {
                        term_score = *fb;
                    }
                }
            }
            if found {
                matched += 1;
                score += term_score;
            }
        }
        if matched >= required {
            return Ok((true, score * boost_of(bm)));
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // nested / join
    // -----------------------------------------------------------------------

    fn nested_children(&self, path: &str) -> Vec<Value> {
        let base = if let Some(sp) = &self.scope_path {
            if path.starts_with(&format!("{}.", sp)) {
                let rel = &path[sp.len() + 1..];
                match &self.scope_obj {
                    Some(o) => collect_path(o, rel),
                    None => Vec::new(),
                }
            } else {
                collect_path(self.doc, path)
            }
        } else {
            collect_path(self.doc, path)
        };
        base
    }

    pub fn eval_nested(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[nested] query malformed")),
        };
        let path = match bm.get("path").and_then(|p| p.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Err(err_parsing("[nested] requires 'path'")),
        };
        let inner = match bm.get("query").and_then(|q| q.as_object()) {
            Some(q) => q,
            None => return Err(err_parsing("[nested] requires 'query'")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let def = crate::eval::mapping_field_def(self.idx, &path);
        let is_nested = def
            .map(|d| d.get("type").and_then(|t| t.as_str()) == Some("nested"))
            .unwrap_or(false);
        if !is_nested {
            if ignore_unmapped {
                return Ok((false, 0.0));
            }
            let reason = if def.is_none() {
                format!("failed to create query: [nested] failed to find nested object under path [{}]", path)
            } else {
                format!(
                    "failed to create query: [nested] nested object under path [{}] is not of nested type",
                    path
                )
            };
            return Err(shard_error(400, "query_shard_exception", &reason));
        }
        let mut score_mode = bm
            .get("score_mode")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if score_mode.is_empty() {
            score_mode = "avg".to_string();
        }
        let mut count = 0usize;
        let mut best = 0.0f64;
        let mut worst = f64::INFINITY;
        let mut sum = 0.0f64;
        for child in self.nested_children(&path) {
            if !child.is_object() {
                continue;
            }
            let mut vals = Map::new();
            flatten_scope(&path, &child, &mut vals);
            let mut cc = QueryCtx::with_values(self.idx, &self.id, self.doc, vals);
            cc.scope_path = Some(path.clone());
            cc.scope_obj = Some(child);
            let (ok, sc) = cc.eval(inner)?;
            if ok {
                count += 1;
                if sc > best {
                    best = sc;
                }
                if sc < worst {
                    worst = sc;
                }
                sum += sc;
            }
        }
        if count == 0 {
            return Ok((false, 0.0));
        }
        let score = match score_mode.as_str() {
            "none" => 0.0,
            "max" => best,
            "min" => worst,
            "sum" => sum,
            "avg" => sum / count as f64,
            _ => sum / count as f64,
        };
        Ok((true, score))
    }

    pub fn eval_has_child(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[has_child] query malformed")),
        };
        let child_type = bm
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let inner = match bm.get("query").and_then(|q| q.as_object()) {
            Some(q) => q,
            None => return Err(err_parsing("[has_child] requires 'query'")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let field = match find_join_field(self.idx) {
            Some(f) => f,
            None => {
                if ignore_unmapped {
                    return Ok((false, 0.0));
                }
                return Err(shard_error(
                    400,
                    "query_shard_exception",
                    "[has_child] no join field has been configured",
                ));
            }
        };
        let def = crate::eval::mapping_field_def(self.idx, &field).cloned().unwrap_or_default();
        if !child_type.is_empty() {
            let mut valid = false;
            if let Some(rels) = def.get("relations").and_then(|r| r.as_object()) {
                for (parent, children) in rels {
                    if parent == &child_type {
                        valid = true;
                    }
                    if let Some(arr) = children.as_array() {
                        if arr.iter().any(|c| c.as_str() == Some(child_type.as_str())) {
                            valid = true;
                        }
                    }
                    if children.as_str() == Some(child_type.as_str()) {
                        valid = true;
                    }
                }
            }
            if !valid {
                return Err(shard_error(
                    400,
                    "query_shard_exception",
                    &format!(
                        "[has_child] join field [{}] doesn't hold [{}] as a child",
                        field, child_type
                    ),
                ));
            }
        }
        let score_mode = bm
            .get("score_mode")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let min_children = bm.get("min_children").and_then(to_f64).unwrap_or(0.0) as usize;
        let max_children = bm
            .get("max_children")
            .and_then(to_f64)
            .map(|n| n as usize);
        let mut count = 0usize;
        let mut best = 0.0f64;
        let mut worst = f64::INFINITY;
        let mut sum = 0.0f64;
        for id in &self.idx.order {
            let doc = match self.idx.docs.get(id) {
                Some(d) => d,
                None => continue,
            };
            let (name, parent) = join_parts(doc.get(&field));
            if parent != self.id {
                continue;
            }
            if !child_type.is_empty() && name != child_type {
                continue;
            }
            let cc = QueryCtx::new(self.idx, id, doc);
            let (ok, sc) = cc.eval(inner)?;
            if ok {
                count += 1;
                if sc > best {
                    best = sc;
                }
                if sc < worst {
                    worst = sc;
                }
                sum += sc;
            }
        }
        if min_children > 0 && count < min_children {
            return Ok((false, 0.0));
        }
        if let Some(max) = max_children {
            if count > max {
                return Ok((false, 0.0));
            }
        }
        if count == 0 {
            return Ok((false, 0.0));
        }
        let score = match score_mode.as_str() {
            "max" => best,
            "min" => worst,
            "sum" => sum,
            "avg" => sum / count as f64,
            _ => 1.0,
        };
        Ok((true, score))
    }

    pub fn eval_parent_id(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[parent_id] query malformed")),
        };
        let typ = bm.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let want = bm.get("id").and_then(|t| t.as_str()).unwrap_or("");
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let field = match find_join_field(self.idx) {
            Some(f) => f,
            None => {
                if ignore_unmapped {
                    return Ok((false, 0.0));
                }
                return Err(shard_error(
                    400,
                    "query_shard_exception",
                    "[parent_id] no join field has been configured",
                ));
            }
        };
        if self.idx.routing.get(&self.id).map(|r| r.as_str()) != Some(want) {
            return Ok((false, 0.0));
        }
        let (name, _) = join_parts(self.doc.get(&field));
        if !typ.is_empty() && name != typ {
            return Ok((false, 0.0));
        }
        Ok((true, 1.0))
    }

    pub fn eval_has_parent(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[has_parent] query malformed")),
        };
        let mut parent_type = bm
            .get("parent_type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if parent_type.is_empty() {
            parent_type = bm
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
        }
        let inner = match bm.get("query").and_then(|q| q.as_object()) {
            Some(q) => q,
            None => return Err(err_parsing("[has_parent] requires 'query'")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let field = match find_join_field(self.idx) {
            Some(f) => f,
            None => {
                if ignore_unmapped {
                    return Ok((false, 0.0));
                }
                return Err(shard_error(
                    400,
                    "query_shard_exception",
                    "[has_parent] no join field has been configured",
                ));
            }
        };
        let parent_id = self.idx.routing.get(&self.id).cloned().unwrap_or_default();
        if parent_id.is_empty() {
            return Ok((false, 0.0));
        }
        let doc = match self.idx.docs.get(&parent_id) {
            Some(d) => d,
            None => return Ok((false, 0.0)),
        };
        let (name, _) = join_parts(doc.get(&field));
        if !parent_type.is_empty() && name != parent_type {
            return Ok((false, 0.0));
        }
        let cc = QueryCtx::new(self.idx, &parent_id, doc);
        let (ok, sc) = cc.eval(inner)?;
        if ok {
            let score = if bm.get("score").and_then(|s| s.as_bool()).unwrap_or(false) {
                sc
            } else {
                1.0
            };
            return Ok((true, score));
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // geo queries
    // -----------------------------------------------------------------------

    pub fn eval_geo_distance(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[geo_distance] query malformed")),
        };
        let distance = match bm.get("distance").and_then(parse_distance_meters) {
            Some(d) => d,
            None => return Err(err_parsing("[geo_distance] requires 'distance'")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut field = String::new();
        let mut point = Value::Null;
        for (k, v) in bm {
            if k == "distance"
                || k == "distance_type"
                || k == "boost"
                || k == "_name"
                || k == "validation_method"
                || k == "ignore_unmapped"
            {
                continue;
            }
            field = k.clone();
            point = v.clone();
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("[geo_distance] requires a field"));
        }
        let def = crate::eval::mapping_field_def(self.idx, &field).cloned();
        let ftype = def
            .as_ref()
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if def.is_none() {
            if ignore_unmapped {
                return Ok((false, 0.0));
            }
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!("failed to find geo field [{}]", field),
            ));
        }
        if ftype != "geo_point" && ftype != "geo_shape" {
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!(
                    "for field [{}] is not supported for [geo_distance] queries. Must be one of [geo_point] or [geo_shape]",
                    field
                ),
            ));
        }
        let (plat, plon) = match geo_point_of(&point) {
            Some(p) => p,
            None => return Err(err_parsing("failed to parse geo_distance point")),
        };
        if !valid_latlon(plat, plon) {
            let ignore_malformed = bm
                .get("validation_method")
                .and_then(|v| v.as_str())
                .map(|v| v.eq_ignore_ascii_case("IGNORE_MALFORMED"))
                .unwrap_or(false);
            let reason = if ignore_malformed {
                format!(
                    "invalid latitude {:.1}; must be between -90.0 and 90.0",
                    plat
                )
            } else {
                format!(
                    "couldn't validate latitude/ longitude values: [geo_distance] center point latitude is invalid: {:.1}",
                    plat
                )
            };
            return Err(shard_error(400, "query_shard_exception", &reason));
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            if let Some((lat, lon)) = geo_point_of(v) {
                if haversine_meters(plat, plon, lat, lon) <= distance {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_geo_bounding_box(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[geo_bounding_box] query malformed")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut field = String::new();
        let mut spec: Option<&Map<String, Value>> = None;
        for (k, v) in bm {
            if k == "boost" || k == "_name" || k == "validation_method" || k == "type"
                || k == "ignore_unmapped"
            {
                continue;
            }
            field = k.clone();
            spec = v.as_object();
            break;
        }
        let spec = match spec {
            Some(s) if !field.is_empty() => s,
            _ => return Err(err_parsing("[geo_bounding_box] requires a field")),
        };
        let def = crate::eval::mapping_field_def(self.idx, &field).cloned();
        if def.is_none() {
            if ignore_unmapped {
                return Ok((false, 0.0));
            }
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!("failed to find geo field [{}]", field),
            ));
        }
        let mut top = None;
        let mut left = None;
        let mut bottom = None;
        let mut right = None;
        if let Some(wkt) = spec.get("wkt").and_then(|v| v.as_str()) {
            if let Some(gs) = parse_wkt_shape(wkt) {
                top = Some(gs.top);
                left = Some(gs.left);
                bottom = Some(gs.bot);
                right = Some(gs.right);
            }
        }
        if top.is_none() {
            if let Some(tl) = spec.get("top_left").and_then(geo_point_of) {
                top = Some(tl.0);
                left = Some(tl.1);
            }
            if let Some(br) = spec.get("bottom_right").and_then(geo_point_of) {
                bottom = Some(br.0);
                right = Some(br.1);
            }
            if let Some(tr) = spec.get("top_right").and_then(geo_point_of) {
                top = Some(tr.0);
                right = Some(tr.1);
            }
            if let Some(bl) = spec.get("bottom_left").and_then(geo_point_of) {
                bottom = Some(bl.0);
                left = Some(bl.1);
            }
        }
        if top.is_none() {
            top = spec.get("top").and_then(to_f64);
            left = spec.get("left").and_then(to_f64);
            bottom = spec.get("bottom").and_then(to_f64);
            right = spec.get("right").and_then(to_f64);
        }
        let (top, left, bottom, right) = match (top, left, bottom, right) {
            (Some(t), Some(l), Some(b), Some(r)) => (t, l, b, r),
            _ => return Err(err_parsing("[geo_bounding_box] requires bounds")),
        };
        if top < bottom {
            return Err(ApiError::new(
                "search_phase_execution_exception",
                "failed to create query: top is below bottom corner",
                400,
            ));
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            if let Some((lat, lon)) = geo_point_of(v) {
                let lon_ok = if left <= right {
                    lon >= left && lon <= right
                } else {
                    lon >= left || lon <= right
                };
                if lat <= top && lat >= bottom && lon_ok {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_geo_polygon(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[geo_polygon] query malformed")),
        };
        let mut field = String::new();
        let mut points_raw: Vec<Value> = Vec::new();
        for (k, v) in bm {
            if k == "boost" || k == "_name" || k == "ignore_unmapped" || k == "validation_method" {
                continue;
            }
            field = k.clone();
            if let Some(p) = v.get("points").and_then(|p| p.as_array()) {
                points_raw = p.clone();
            }
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("[geo_polygon] requires a field"));
        }
        let poly: Vec<(f64, f64)> = points_raw.iter().filter_map(geo_point_of).collect();
        if poly.len() < 3 {
            return Err(err_parsing("failed to parse geo_polygon"));
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            if let Some((lat, lon)) = geo_point_of(v) {
                if point_in_polygon(lat, lon, &poly) {
                    return Ok((true, boost));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_geo_shape(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[geo_shape] query malformed")),
        };
        let ignore_unmapped = bm
            .get("ignore_unmapped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut field = String::new();
        let mut shape_raw = Value::Null;
        for (k, v) in bm {
            if k == "boost" || k == "_name" || k == "ignore_unmapped" || k == "relation" {
                continue;
            }
            field = k.clone();
            shape_raw = v.clone();
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("[geo_shape] requires a field"));
        }
        let def = crate::eval::mapping_field_def(self.idx, &field).cloned();
        if def.is_none() {
            if ignore_unmapped {
                return Ok((false, 0.0));
            }
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!("failed to find type for field [{}]", field),
            ));
        }
        let mut relation = bm
            .get("relation")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let mut resolved_shape = shape_raw.clone();
        if let Some(obj) = shape_raw.as_object() {
            if let Some(r) = obj.get("relation").and_then(|r| r.as_str()) {
                relation = r.to_string();
            }
            if let Some(inner) = obj.get("shape") {
                resolved_shape = inner.clone();
            }
            if let Some(ix) = obj.get("indexed_shape").and_then(|v| v.as_object()) {
                let id = ix.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let path = ix.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(doc) = self.idx.docs.get(id) {
                    let mut cur = doc.clone();
                    for part in path.split('.') {
                        cur = cur.get(part).cloned().unwrap_or(Value::Null);
                    }
                    resolved_shape = cur;
                }
            }
        }
        if let Some(obj) = resolved_shape.as_object() {
            if let Some(r) = obj.get("relation").and_then(|r| r.as_str()) {
                relation = r.to_string();
            }
            if let Some(inner) = obj.get("shape") {
                resolved_shape = inner.clone();
            }
        }
        let qs = match parse_geo_shape(&resolved_shape) {
            Some(s) => s,
            None => return Err(err_parsing("failed to parse geo_shape")),
        };
        if relation.is_empty() {
            relation = "INTERSECTS".to_string();
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let ds = if let Some(ds) = parse_geo_shape(v) {
                ds
            } else if let Some((lat, lon)) = geo_point_of(v) {
                GeoShape {
                    typ: "point".to_string(),
                    lat,
                    lon,
                    ..Default::default()
                }
            } else {
                continue;
            };
            let hit = match relation.to_uppercase().as_str() {
                "DISJOINT" => !shape_intersect_shape(&qs, &ds),
                "WITHIN" => shape_contains_shape(&qs, &ds),
                "CONTAINS" => shape_contains_shape(&ds, &qs),
                _ => shape_intersect_shape(&qs, &ds),
            };
            if hit {
                return Ok((true, boost));
            }
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // rank_feature / distance_feature
    // -----------------------------------------------------------------------

    pub fn eval_rank_feature(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[rank_feature] query malformed")),
        };
        let field = bm
            .get("field")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let exact = mapping_field_def(self.idx, &field);
        let exact_type = exact
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let mut positive = true;
        let mut is_feature = exact_type == "rank_feature";
        if let Some(def) = exact {
            if def.get("positive_score_impact").and_then(|v| v.as_bool()) == Some(false) {
                positive = false;
            }
        }
        if !is_feature {
            let parts: Vec<&str> = field.split('.').collect();
            for i in (1..parts.len()).rev() {
                let prefix = parts[..i].join(".");
                if let Some(def) = mapping_field_def(self.idx, &prefix) {
                    if def.get("type").and_then(|t| t.as_str()) == Some("rank_features") {
                        is_feature = true;
                        break;
                    }
                }
            }
        }
        if !is_feature {
            let ftype = if exact_type.is_empty() { "unmapped" } else { exact_type };
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!(
                    "failed to create query: [rank_feature] query only works on [rank_feature] fields and features of [rank_features] fields, not [{}]",
                    ftype
                ),
            ));
        }
        let funcs = ["saturation", "log", "sigmoid", "linear"]
            .iter()
            .filter(|f| bm.contains_key(**f))
            .count();
        if funcs > 1 {
            return Err(err_parsing(
                "[rank_feature] only one of [saturation, log, sigmoid, linear] may be specified",
            ));
        }
        let (vals, present) = self.values(&field);
        let raw = if present {
            vals.first().cloned()
        } else {
            field_value(self.doc, &field).cloned()
        };
        let f = match raw.as_ref().and_then(to_f64) {
            Some(f) => f,
            None => return Ok((false, 0.0)),
        };
        let f = if positive {
            f
        } else if f == 0.0 {
            0.0
        } else {
            1.0 / f
        };
        let is_subfeature = exact_type != "rank_feature";
        let mut sc;
        if let Some(sat) = bm.get("saturation").and_then(|s| s.as_object()) {
            let pivot = sat.get("pivot").and_then(to_f64).unwrap_or(1.0);
            sc = f / (f + pivot);
        } else if let Some(lg) = bm.get("log").and_then(|s| s.as_object()) {
            let scaling = lg.get("scaling_factor").and_then(to_f64).unwrap_or(1.0);
            sc = (1.0 + f).ln() * scaling;
        } else if let Some(sg) = bm.get("sigmoid").and_then(|s| s.as_object()) {
            let pivot = sg.get("pivot").and_then(to_f64).unwrap_or(1.0);
            let exp = sg.get("exponent").and_then(to_f64).unwrap_or(1.0);
            let a = f.powf(exp);
            let b = pivot.powf(exp);
            sc = a / (a + b);
        } else if let Some(ln) = bm.get("linear").and_then(|s| s.as_object()) {
            let scaling = ln.get("scaling_factor").and_then(to_f64).unwrap_or(1.0);
            sc = f * scaling;
        } else if is_subfeature {
            sc = f / (f + 5.0);
        } else {
            sc = f / (f + 1.0);
        }
        Ok((true, boost * sc))
    }

    pub fn eval_distance_feature(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[distance_feature] query malformed")),
        };
        let field = bm
            .get("field")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let def = mapping_field_def(self.idx, &field)
            .cloned()
            .unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !matches!(ftype.as_str(), "date" | "date_nanos" | "geo_point") {
            return Err(shard_error(
                400,
                "query_shard_exception",
                "[distance_feature] query can only be run on a date, date_nanos or geo_point field type!",
            ));
        }
        let pivot = if ftype == "geo_point" {
            bm.get("pivot")
                .and_then(parse_distance_meters)
                .unwrap_or(1.0)
        } else {
            match bm.get("pivot") {
                Some(Value::String(s)) => parse_duration_ms(s).map(|n| n as f64).unwrap_or(1.0),
                Some(v) => to_f64(v).unwrap_or(1.0),
                None => 1.0,
            }
        };
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        if ftype == "geo_point" {
            let origin = bm.get("origin").and_then(geo_point_of);
            let (olat, olon) = match origin {
                Some(o) => o,
                None => return Ok((false, 0.0)),
            };
            let dp = match vals.first().and_then(geo_point_of) {
                Some(p) => p,
                None => return Ok((false, 0.0)),
            };
            let dist = haversine_meters(olat, olon, dp.0, dp.1);
            return Ok((true, boost * pivot / (pivot + dist)));
        }
        let v = match vals.first().and_then(to_f64) {
            Some(v) => v,
            None => return Ok((false, 0.0)),
        };
        let origin = match parse_query_date(
            &def,
            bm.get("origin").unwrap_or(&Value::Null),
            "",
            true,
        ) {
            Ok(o) => to_f64(&o).unwrap_or(0.0),
            Err(_) => return Ok((false, 0.0)),
        };
        let dist = (v - origin).abs();
        if dist == 0.0 {
            return Ok((true, boost));
        }
        Ok((true, boost * pivot / (pivot + dist)))
    }

    // -----------------------------------------------------------------------
    // wrapper / pinned / percolate / terms_set
    // -----------------------------------------------------------------------

    pub fn eval_wrapper(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[wrapper] query malformed")),
        };
        let raw = match bm.get("query") {
            Some(q) => q,
            None => return Err(err_parsing("[wrapper] requires 'query'")),
        };
        match raw {
            Value::String(t) => {
                let decoded: Vec<u8> = match base64::decode(t) {
                    Ok(d) => d,
                    Err(_) => match t.parse::<Map<String, Value>>() {
                        Ok(_) => t.as_bytes().to_vec(),
                        Err(_) => return Err(err_parsing("failed to parse wrapper query")),
                    },
                };
                let inner: Map<String, Value> = match serde_json::from_slice(&decoded) {
                    Ok(i) => i,
                    Err(_) => return Err(err_parsing("failed to parse wrapper query")),
                };
                self.eval(&inner)
            }
            Value::Object(m) => self.eval(m),
            _ => Err(err_parsing("failed to parse wrapper query")),
        }
    }

    pub fn eval_pinned(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[pinned] query malformed")),
        };
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let mut pinned = false;
        if let Some(list) = bm.get("ids").and_then(|l| l.as_array()) {
            for e in list {
                if value_to_string(e).unwrap_or_default() == self.id {
                    pinned = true;
                }
            }
        }
        if let Some(docs) = bm.get("docs").and_then(|l| l.as_array()) {
            for e in docs {
                if let Some(m) = e.as_object() {
                    if m.get("_id").and_then(value_to_string).unwrap_or_default() == self.id {
                        pinned = true;
                    }
                }
            }
        }
        if pinned {
            return Ok((true, boost));
        }
        if let Some(organic) = bm.get("organic").and_then(|o| o.as_object()) {
            return self.eval(organic);
        }
        Ok((false, 0.0))
    }

    pub fn eval_percolate(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[percolate] query malformed")),
        };
        let mut field = bm
            .get("field")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        if field.is_empty() {
            field = "query".to_string();
        }
        let def = mapping_field_def(self.idx, &field);
        let ftype = def
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if ftype != "percolator" {
            return Err(shard_error(
                400,
                "query_shard_exception",
                &format!(
                    "expected field [{}] to be of type [percolator], but is of type [{}]",
                    field, ftype
                ),
            ));
        }
        let stored = match self.doc.as_object().and_then(|d| d.get(&field)) {
            Some(s) => s,
            None => return Ok((false, 0.0)),
        };
        let qm = match stored.as_object() {
            Some(q) => q,
            None => return Ok((false, 0.0)),
        };
        let inputs = match percolate_inputs(self.idx, bm)? {
            Some(v) => v,
            None => return Ok((false, 0.0)),
        };
        for (_, input) in inputs {
            let input_obj = Value::Object(input.clone());
            let vals = match doc_values_for(self.idx, "_percolate", &input) {
                Ok(v) => v.0,
                Err(_) => continue,
            };
            let input_qc =
                QueryCtx::with_values(self.idx, "_percolate", &input_obj, btree_to_map(vals));
            if let Ok((true, sc)) = input_qc.eval(qm) {
                return Ok((true, sc));
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_terms_set(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[terms_set] query malformed")),
        };
        let mut field = String::new();
        let mut spec = Map::new();
        for (k, v) in bm {
            if k == "boost" || k == "_name" {
                continue;
            }
            field = k.clone();
            spec = v.as_object().cloned().unwrap_or_default();
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("[terms_set] requires a field"));
        }
        let terms = spec
            .get("terms")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        let mut matched = 0usize;
        for item in &terms {
            for v in &vals {
                let vs = value_scalar_string(v).unwrap_or_default();
                let is = value_scalar_string(item).unwrap_or_default();
                if vs == is || numeric_equal(v, item) {
                    matched += 1;
                    break;
                }
            }
        }
        let mut required = 1usize;
        if let Some(name) = spec
            .get("minimum_should_match_field")
            .and_then(|n| n.as_str())
        {
            let (req_vals, _) = self.values(name);
            if let Some(v) = req_vals.first() {
                required = numeric_int(v);
            }
        }
        if let Some(script_spec) = spec
            .get("minimum_should_match_script")
            .and_then(|s| s.as_object())
        {
            if let Some(src) = script_spec.get("source").and_then(|s| s.as_str()) {
                if let Some(n) = script_minimum_should_match(src, terms.len(), matched) {
                    required = n;
                }
            }
        }
        if matched >= required {
            return Ok((true, 1.0));
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // span queries
    // -----------------------------------------------------------------------

    pub fn eval_span_term(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_term".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_near(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_near".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_or(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_or".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_first(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_first".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_not(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_not".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_containing(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_containing".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_within(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_within".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    pub fn eval_span_multi(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let mut m = Map::new();
        m.insert("span_multi".to_string(), body.clone());
        let (_, spans) = self.eval_span_query(&Value::Object(m))?;
        Ok((!spans.is_empty(), 1.0))
    }

    fn span_token_positions(&self, field: &str) -> Vec<(String, usize)> {
        let gap = mapping_field_def(self.idx, field)
            .and_then(|d| d.get("position_increment_gap"))
            .and_then(|v| v.as_i64())
            .unwrap_or(100) as usize;
        let mut out = Vec::new();
        let mut base = 0usize;
        for v in self.must_values(field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            let toks = self.analyze_field(field, None, &s);
            let mut max_pos = 0usize;
            for (i, tok) in toks.into_iter().enumerate() {
                out.push((tok, base + i));
                max_pos = base + i;
            }
            base = max_pos + gap + 1;
        }
        out
    }

    fn span_term_positions(&self, field: &str, term: &str) -> Vec<Span> {
        self.span_token_positions(field)
            .into_iter()
            .filter(|(t, _)| t == term)
            .map(|(_, p)| Span { start: p, end: p })
            .collect()
    }

    fn eval_span_query(&self, q: &Value) -> Result<(String, Vec<Span>), ApiError> {
        let obj = match q.as_object() {
            Some(o) => o,
            None => return Err(err_parsing("span query malformed")),
        };
        let (kind, body) = match single_entry(obj) {
            Some(kv) => kv,
            None => return Err(err_parsing("span query malformed")),
        };
        match kind.as_str() {
            "span_term" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_term malformed")),
                };
                if bm.len() > 1 {
                    let names: Vec<String> = bm.keys().cloned().collect();
                    return Err(err_parsing(&format!(
                        "[span_term] query doesn't support multiple fields, found [{}] and [{}]",
                        names[0], names[1]
                    )));
                }
                let (field, raw) = match single_entry(bm) {
                    Some(kv) => kv,
                    None => return Err(err_parsing("span_term malformed")),
                };
                let term = if let Some(m) = raw.as_object() {
                    m.get("value").and_then(value_to_string).unwrap_or_default()
                } else {
                    value_to_string(raw).unwrap_or_default()
                };
                let ftype = mapping_field_def(self.idx, field)
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if matches!(
                    ftype,
                    "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
                        | "scaled_float" | "unsigned_long" | "date" | "date_nanos" | "boolean"
                ) {
                    return Err(span_shard_error(
                        "Cannot extract a term from a query of type [span_term] on field with no terms",
                    ));
                }
                if ftype == "match_only_text" {
                    let reason = format!(
                        "field \"{}\" was indexed without position data; cannot run SpanTermQuery (term={})",
                        field, term
                    );
                    let mut e = ApiError::new(
                        "search_phase_execution_exception",
                        "all shards failed",
                        500,
                    );
                    let cause = ApiError::new("illegal_state_exception", &reason, 500);
                    e.root_cause = Some(Box::new(cause.clone()));
                    e.caused_by = Some(Box::new(cause));
                    return Err(e);
                }
                Ok((field.clone(), self.span_term_positions(field, &term)))
            }
            "span_near" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_near malformed")),
                };
                let clauses = match bm.get("clauses").and_then(|c| c.as_array()) {
                    Some(c) if !c.is_empty() => c.clone(),
                    _ => {
                        return Err(err_parsing("span_near must include [clauses]"));
                    }
                };
                let in_order = bm.get("in_order").and_then(|v| v.as_bool()).unwrap_or(true);
                let slop = bm.get("slop").and_then(to_f64).unwrap_or(0.0).max(0.0) as usize;
                for cl in &clauses {
                    if !is_span_query(cl) {
                        return Err(err_parsing(
                            "span_near [clauses] must be of type span query",
                        ));
                    }
                }
                let mut field = String::new();
                let mut cur: Vec<Span> = Vec::new();
                for cl in &clauses {
                    let (f, spans) = self.eval_span_query(cl)?;
                    if field.is_empty() {
                        field = f;
                        cur = spans;
                        continue;
                    }
                    if f != field {
                        return Err(span_shard_error(&format!(
                            "Cannot add clause {} to SpanNearQuery for field {}",
                            span_clause_label(cl),
                            field
                        )));
                    }
                    let mut next = Vec::new();
                    for a in &cur {
                        for b in &spans {
                            if in_order {
                                if b.start > a.end && b.start - a.end - 1 <= slop {
                                    next.push(Span {
                                        start: a.start,
                                        end: b.end,
                                    });
                                }
                            } else {
                                let gap = if b.start > a.end {
                                    b.start - a.end - 1
                                } else if a.start > b.end {
                                    a.start - b.end - 1
                                } else {
                                    0
                                };
                                if gap <= slop {
                                    next.push(Span {
                                        start: a.start.min(b.start),
                                        end: a.end.max(b.end),
                                    });
                                }
                            }
                        }
                    }
                    cur = next;
                    if cur.is_empty() {
                        break;
                    }
                }
                Ok((field, cur))
            }
            "span_or" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_or malformed")),
                };
                let clauses = bm
                    .get("clauses")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut field = String::new();
                let mut out = Vec::new();
                for cl in &clauses {
                    let (f, spans) = self.eval_span_query(cl)?;
                    if field.is_empty() {
                        field = f;
                    } else if f != field {
                        return Err(span_shard_error("Clauses must have same field."));
                    }
                    out.extend(spans);
                }
                Ok((field, out))
            }
            "span_first" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_first malformed")),
                };
                let end = bm.get("end").and_then(to_f64).unwrap_or(1.0) as usize;
                let inner = bm.get("match").cloned().unwrap_or(Value::Null);
                let (field, spans) = self.eval_span_query(&inner)?;
                Ok((
                    field,
                    spans.into_iter().filter(|s| s.start < end).collect(),
                ))
            }
            "span_not" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_not malformed")),
                };
                let include = bm.get("include").cloned().unwrap_or(Value::Null);
                let exclude = bm.get("exclude").cloned().unwrap_or(Value::Null);
                let (field, inc) = self.eval_span_query(&include)?;
                let (_, exc) = self.eval_span_query(&exclude)?;
                let pre = bm.get("pre").and_then(to_f64).unwrap_or(0.0) as isize;
                let post = bm.get("post").and_then(to_f64).unwrap_or(0.0) as isize;
                let dist = bm.get("dist").and_then(to_f64).unwrap_or(-1.0);
                let out = inc
                    .into_iter()
                    .filter(|a| {
                        !exc.iter().any(|b| {
                            if dist >= 0.0 {
                                (a.start as isize - b.start as isize).abs() <= dist as isize
                            } else if pre > 0 || post > 0 {
                                (pre > 0
                                    && b.end < a.start
                                    && a.start - b.end <= pre as usize)
                                    || (post > 0
                                        && b.start > a.end
                                        && b.start - a.end <= post as usize)
                            } else {
                                b.start <= a.end && b.end >= a.start
                            }
                        })
                    })
                    .collect();
                Ok((field, out))
            }
            "span_multi" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span_multi malformed")),
                };
                let inner = match bm.get("match").and_then(|m| m.as_object()) {
                    Some(m) => m,
                    None => return Err(err_parsing("span_multi requires match")),
                };
                let (kind, body) = match single_entry(inner) {
                    Some(kv) => kv,
                    None => return Err(err_parsing("span_multi requires match")),
                };
                if !matches!(kind.as_str(), "prefix" | "wildcard" | "fuzzy" | "regexp") {
                    return Err(err_parsing(
                        "[span_multi] [match] must be of type multi term query",
                    ));
                }
                let (field, raw) = match body.as_object().and_then(single_entry) {
                    Some(kv) => kv,
                    None => return Err(err_parsing("span_multi malformed")),
                };
                let (value, opts) = if let Some(m) = raw.as_object() {
                    (
                        m.get("value").and_then(value_to_string).unwrap_or_default(),
                        Some(m),
                    )
                } else {
                    (value_to_string(raw).unwrap_or_default(), None)
                };
                let re = match kind.as_str() {
                    "prefix" => Some(wildcard_regex(&format!("{}*", value.to_lowercase()))),
                    "wildcard" => Some(wildcard_regex(&value.to_lowercase())),
                    "regexp" => Regex::new(&format!("^(?:{})$", value)).ok(),
                    _ => None,
                };
                let fuzz = if kind == "fuzzy" {
                    let spec = opts.and_then(|m| m.get("fuzziness"));
                    let n = match spec {
                        Some(Value::Number(n)) => n.as_i64().unwrap_or(0).max(0) as usize,
                        Some(Value::String(s)) if s == "AUTO" => auto_fuzziness(&value),
                        Some(Value::String(s)) => s.parse::<usize>().unwrap_or(1),
                        _ => auto_fuzziness(&value),
                    };
                    Some(n)
                } else {
                    None
                };
                let spans = self
                    .span_token_positions(field)
                    .into_iter()
                    .filter(|(tok, _)| {
                        if let Some(re) = &re {
                            re.is_match(&tok.to_lowercase())
                        } else if let Some(n) = fuzz {
                            levenshtein(&value.to_lowercase(), &tok.to_lowercase()) <= n
                        } else {
                            false
                        }
                    })
                    .map(|(_, p)| Span { start: p, end: p })
                    .collect();
                Ok((field.clone(), spans))
            }
            "span_containing" | "span_within" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("span query malformed")),
                };
                let big = bm.get("big").cloned().unwrap_or(Value::Null);
                let little = bm.get("little").cloned().unwrap_or(Value::Null);
                let (field, bigs) = self.eval_span_query(&big)?;
                let (_, littles) = self.eval_span_query(&little)?;
                let out = bigs
                    .into_iter()
                    .filter(|b| {
                        littles
                            .iter()
                            .any(|l| l.start >= b.start && l.end <= b.end)
                    })
                    .collect();
                Ok((field, out))
            }
            "field_masking_span" => {
                let bm = match body.as_object() {
                    Some(m) => m,
                    None => return Err(err_parsing("field_masking_span malformed")),
                };
                let field = bm
                    .get("field")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let inner = bm.get("query").cloned().unwrap_or(Value::Null);
                let inner_obj = inner.as_object().cloned().unwrap_or_default();
                if let Some((_, term_body)) = single_entry(&inner_obj) {
                    if let Some(tb) = term_body.as_object() {
                        if let Some((inner_field, raw)) = single_entry(tb) {
                            let term = if let Some(m) = raw.as_object() {
                                m.get("value").and_then(value_to_string).unwrap_or_default()
                            } else {
                                value_to_string(raw).unwrap_or_default()
                            };
                            return Ok((
                                field.clone(),
                                self.span_term_positions(inner_field, &term),
                            ));
                        }
                    }
                }
                self.eval_span_query(&inner)
            }
            _ => Err(err_parsing("span query malformed")),
        }
    }

    pub fn eval_intervals(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("intervals malformed")),
        };
        let mut field = String::new();
        let mut rules = Value::Null;
        for (k, v) in bm {
            if k == "boost" || k == "_name" {
                continue;
            }
            field = k.clone();
            rules = v.clone();
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("intervals malformed"));
        }
        let ftype = mapping_field_def(self.idx, &field)
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if ftype == "match_only_text" {
            return Err(span_shard_error(&format!(
                "failed to create query: Cannot create intervals over field [{}] with no positions indexed",
                field
            )));
        }
        if ftype == "keyword" || ftype == "constant_keyword" {
            return Err(span_shard_error(&format!(
                "Can only use interval queries on text fields - not on [{}] which is of type [{}]",
                field, ftype
            )));
        }
        let rm = match rules.as_object() {
            Some(m) => m.clone(),
            None => return Err(err_parsing("intervals malformed")),
        };
        let spans = self.intervals_rule(&field, &rm)?;
        Ok((!spans.is_empty(), 1.0))
    }

    fn intervals_rule(
        &self,
        field: &str,
        rule: &Map<String, Value>,
    ) -> Result<Vec<Span>, ApiError> {
        let clean: Vec<(&String, &Value)> = rule
            .iter()
            .filter(|(k, _)| k.as_str() != "boost" && k.as_str() != "_name")
            .collect();
        if clean.is_empty() {
            return Err(err_parsing("intervals malformed"));
        }
        if clean.len() > 1 {
            return Err(err_parsing(&format!(
                "Only one interval rule can be specified, found [{}] and [{}]",
                clean[0].0, clean[1].0
            )));
        }
        let kind = clean[0].0;
        let body = clean[0].1;
        let bm = body.as_object().cloned().unwrap_or_default();
        match kind.as_str() {
            "match" => {
                let q = bm
                    .get("query")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                if q.is_empty() {
                    return Ok(Vec::new());
                }
                let analyzer = bm.get("analyzer").and_then(|a| a.as_str()).unwrap_or("");
                let terms: Vec<String> = if analyzer == "keyword" {
                    vec![q.clone()]
                } else {
                    self.analyze_field(field, None, &q)
                };
                if terms.is_empty() {
                    return Ok(Vec::new());
                }
                let ordered = bm.get("ordered").and_then(|v| v.as_bool()).unwrap_or(false);
                let max_gaps = bm
                    .get("max_gaps")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1);
                let positions = self.span_token_positions(field);
                let mut spans = self.intervals_sequence(&positions, &terms, ordered, max_gaps);
                if let Some(flt) = bm.get("filter").and_then(|f| f.as_object()) {
                    spans = self.intervals_filter(field, spans, flt)?;
                }
                Ok(spans)
            }
            "prefix" => {
                let q = bm
                    .get("prefix")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                Ok(self
                    .span_token_positions(field)
                    .into_iter()
                    .filter(|(t, _)| t.starts_with(&q))
                    .map(|(_, p)| Span { start: p, end: p })
                    .collect())
            }
            "wildcard" => {
                let q = bm
                    .get("pattern")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let re = wildcard_regex(&q);
                Ok(self
                    .span_token_positions(field)
                    .into_iter()
                    .filter(|(t, _)| re.is_match(t))
                    .map(|(_, p)| Span { start: p, end: p })
                    .collect())
            }
            "regexp" => {
                let q = bm
                    .get("pattern")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string();
                let re = Regex::new(&format!("^(?:{})$", q)).ok();
                Ok(self
                    .span_token_positions(field)
                    .into_iter()
                    .filter(|(t, _)| re.as_ref().map(|r| r.is_match(t)).unwrap_or(false))
                    .map(|(_, p)| Span { start: p, end: p })
                    .collect())
            }
            "fuzzy" => {
                let q = bm
                    .get("term")
                    .or_else(|| bm.get("value"))
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let fuzz = match bm.get("fuzziness") {
                    Some(Value::Number(n)) => n.as_i64().unwrap_or(0).max(0) as usize,
                    Some(Value::String(s)) if s == "AUTO" => auto_fuzziness(&q),
                    Some(Value::String(s)) => s.parse::<usize>().unwrap_or(1),
                    _ => auto_fuzziness(&q),
                };
                let prefix_len = bm
                    .get("prefix_length")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    .max(0) as usize;
                Ok(self
                    .span_token_positions(field)
                    .into_iter()
                    .filter(|(t, _)| {
                        if t.len() < prefix_len || q.len() < prefix_len {
                            return false;
                        }
                        if t[..prefix_len] != q[..prefix_len] {
                            return false;
                        }
                        levenshtein(&q, t) <= fuzz
                    })
                    .map(|(_, p)| Span { start: p, end: p })
                    .collect())
            }
            "all_of" => {
                let subs = bm
                    .get("intervals")
                    .and_then(|i| i.as_array())
                    .cloned()
                    .unwrap_or_default();
                let ordered = bm.get("ordered").and_then(|v| v.as_bool()).unwrap_or(false);
                let max_gaps = bm.get("max_gaps").and_then(|v| v.as_i64()).unwrap_or(-1);
                let mut sets: Vec<Vec<Span>> = Vec::new();
                for sub in &subs {
                    if let Some(sm) = sub.as_object() {
                        let spans = self.intervals_rule(field, sm)?;
                        if spans.is_empty() {
                            return Ok(Vec::new());
                        }
                        sets.push(spans);
                    }
                }
                Ok(self.intervals_combine(sets, ordered, max_gaps))
            }
            "any_of" => {
                let subs = bm
                    .get("intervals")
                    .and_then(|i| i.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut out = Vec::new();
                for sub in &subs {
                    if let Some(sm) = sub.as_object() {
                        out.extend(self.intervals_rule(field, sm)?);
                    }
                }
                Ok(out)
            }
            "containing" => {
                let inner = bm
                    .get("intervals")
                    .and_then(|i| i.as_object())
                    .cloned()
                    .unwrap_or_default();
                self.intervals_rule(field, &inner)
            }
            "not" => {
                let inner = bm
                    .get("intervals")
                    .and_then(|i| i.as_object())
                    .cloned()
                    .unwrap_or_default();
                if self.intervals_rule(field, &inner)?.is_empty() {
                    Ok(vec![Span {
                        start: 0,
                        end: usize::MAX,
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Err(err_parsing(&format!("Unknown interval type [{}]", kind))),
        }
    }

    fn intervals_sequence(
        &self,
        positions: &[(String, usize)],
        terms: &[String],
        ordered: bool,
        max_gaps: i64,
    ) -> Vec<Span> {
        let occ: Vec<Vec<Span>> = terms
            .iter()
            .map(|t| {
                positions
                    .iter()
                    .filter(|(tok, _)| tok == t)
                    .map(|(_, p)| Span { start: *p, end: *p })
                    .collect()
            })
            .collect();
        if occ.iter().any(|v| v.is_empty()) {
            return Vec::new();
        }
        if ordered {
            return self.intervals_combine(occ, true, max_gaps);
        }
        let mut out = Vec::new();
        let mut idx: Vec<usize> = (0..occ.len()).collect();
        self.permute_spans(&occ, &mut idx, 0, max_gaps, &mut out);
        out
    }

    fn permute_spans(
        &self,
        occ: &[Vec<Span>],
        idx: &mut Vec<usize>,
        k: usize,
        max_gaps: i64,
        out: &mut Vec<Span>,
    ) {
        if k == idx.len() {
            let sets: Vec<Vec<Span>> = idx.iter().map(|&i| occ[i].clone()).collect();
            out.extend(self.intervals_combine(sets, false, max_gaps));
            return;
        }
        for i in k..idx.len() {
            idx.swap(k, i);
            self.permute_spans(occ, idx, k + 1, max_gaps, out);
            idx.swap(k, i);
        }
    }

    fn intervals_combine(&self, sets: Vec<Vec<Span>>, ordered: bool, max_gaps: i64) -> Vec<Span> {
        if sets.is_empty() {
            return Vec::new();
        }
        let mut cur = sets[0].clone();
        for spans in sets.iter().skip(1) {
            let mut next = Vec::new();
            for a in &cur {
                for b in spans {
                    let gap = if b.start > a.end {
                        (b.start - a.end - 1) as i64
                    } else if a.start > b.end {
                        (a.start - b.end - 1) as i64
                    } else {
                        -1
                    };
                    let ok = if ordered {
                        b.start > a.end && (max_gaps < 0 || gap <= max_gaps)
                    } else if max_gaps >= 0 {
                        gap <= max_gaps
                    } else {
                        true
                    };
                    if ok {
                        next.push(Span {
                            start: a.start.min(b.start),
                            end: a.end.max(b.end),
                        });
                    }
                }
            }
            cur = next;
            if cur.is_empty() {
                break;
            }
        }
        cur
    }

    fn intervals_filter(
        &self,
        field: &str,
        spans: Vec<Span>,
        flt: &Map<String, Value>,
    ) -> Result<Vec<Span>, ApiError> {
        let (kind, body) = match single_entry(flt) {
            Some(kv) => kv,
            None => return Ok(spans),
        };
        let bm = body.as_object().cloned().unwrap_or_default();
        if kind == "script" {
            let src = bm.get("source").and_then(|s| s.as_str()).unwrap_or("");
            if let Some(rest) = src.split("interval.start >").nth(1) {
                let n: usize = rest.trim().parse().unwrap_or(0);
                return Ok(spans.into_iter().filter(|s| s.start > n).collect());
            }
            return Ok(spans);
        }
        let other = self.intervals_rule(field, &bm)?;
        let out = spans
            .into_iter()
            .filter(|a| {
                let any = |pred: &dyn Fn(&Span) -> bool| other.iter().any(pred);
                match kind.as_str() {
                    "before" => any(&|b| b.start > a.end),
                    "after" => any(&|b| b.end < a.start),
                    "containing" => any(&|b| b.start >= a.start && b.end <= a.end),
                    "contained_by" => any(&|b| a.start >= b.start && a.end <= b.end),
                    "not_containing" => !any(&|b| b.start >= a.start && b.end <= a.end),
                    "not_contained_by" => !any(&|b| a.start >= b.start && a.end <= b.end),
                    "not_overlapping" => !any(&|b| b.start <= a.end && b.end >= a.start),
                    _ => true,
                }
            })
            .collect();
        Ok(out)
    }

    fn eval_term_single(
        &self,
        field: &str,
        bm: &Map<String, Value>,
    ) -> Result<(bool, f64), ApiError> {
        let q = bm
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string();
        if field.is_empty() {
            let keys: Vec<String> = self
                .values_map()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            for f in keys {
                if self.eval_term(&f, &Value::String(q.clone()), 1.0)?.0 {
                    return Ok((true, 1.0));
                }
            }
            return Ok((false, 0.0));
        }
        let ok = self.eval_term(field, &Value::String(q), 1.0)?.0;
        Ok((ok, 1.0))
    }

    fn eval_intervals_simple(&self, pred: &dyn Fn(&str, &[String]) -> bool) -> bool {
        let map = match self.values_map() {
            Some(m) => m.clone(),
            None => return false,
        };
        for (f, vals) in &map {
            let mut tokens: Vec<String> = Vec::new();
            for v in array_values(vals) {
                if let Some(s) = value_scalar_string(&v) {
                    tokens.extend(analyze(&s));
                }
            }
            if pred(f, &tokens) {
                return true;
            }
        }
        false
    }

    // -----------------------------------------------------------------------
    // more_like_this
    // -----------------------------------------------------------------------

    pub fn eval_more_like_this(&self, bm: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        if !bm.contains_key("like") {
            return Err(err_parsing(
                "[more_like_this] requires 'like' to be specified",
            ));
        }
        let mut fields: Vec<String> = fields_of(bm.get("fields"))
            .into_iter()
            .map(|f| strip_field_boost(&f).0)
            .collect();
        if fields.is_empty() {
            fields = vec!["_all".to_string()];
        }
        let mut source_ids = std::collections::HashSet::new();
        let mut texts: Vec<String> = Vec::new();
        let mut like_entries: Vec<Value> = Vec::new();
        match bm.get("like") {
            Some(Value::String(s)) => texts.push(s.clone()),
            Some(Value::Object(m)) => like_entries.push(Value::Object(m.clone())),
            Some(Value::Array(a)) => like_entries.extend(a.iter().cloned()),
            _ => {}
        }
        for e in &like_entries {
            match e {
                Value::String(v) => texts.push(v.clone()),
                Value::Object(v) => {
                    if let Some(doc) = v.get("doc").and_then(|d| d.as_object()) {
                        texts.extend(collect_doc_texts(doc, &fields));
                    } else if let Some(id) = v.get("_id").and_then(value_to_string) {
                        if let Some(sd) = self.idx.docs.get(&id) {
                            if let Some(sdo) = sd.as_object() {
                                texts.extend(collect_doc_texts(sdo, &fields));
                                source_ids.insert(id.clone());
                            }
                        }
                    } else {
                        texts.extend(collect_doc_texts(v, &fields));
                    }
                }
                _ => {}
            }
        }
        let mut stop: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(sw) = bm.get("stop_words") {
            match sw {
                Value::Array(a) => {
                    for w in a {
                        if let Some(s) = w.as_str() {
                            if s != "_none_" {
                                stop.insert(s.to_lowercase());
                            }
                        }
                    }
                }
                Value::String(s) if s != "_none_" => {
                    for w in s.split(',') {
                        stop.insert(w.trim().to_lowercase());
                    }
                }
                _ => {}
            }
        }
        let min_tf = bm
            .get("min_term_freq")
            .and_then(to_f64)
            .map(|n| n as usize)
            .unwrap_or(2);
        let mut terms: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for t in &texts {
            for tok in analyze(t) {
                if stop.contains(&tok) {
                    continue;
                }
                *terms.entry(tok).or_insert(0) += 1;
            }
        }
        terms.retain(|_, tf| *tf >= min_tf);
        let mut unlike_texts: Vec<String> = Vec::new();
        if let Some(u) = bm.get("unlike") {
            match u {
                Value::String(s) => unlike_texts.push(s.clone()),
                Value::Array(a) => {
                    for e in a {
                        match e {
                            Value::String(v) => unlike_texts.push(v.clone()),
                            Value::Object(v) => {
                                if let Some(id) = v.get("_id").and_then(value_to_string) {
                                    if let Some(sd) = self.idx.docs.get(&id) {
                                        if let Some(sdo) = sd.as_object() {
                                            unlike_texts
                                                .extend(collect_doc_texts(sdo, &fields));
                                        }
                                    }
                                } else if let Some(doc) =
                                    v.get("doc").and_then(|d| d.as_object())
                                {
                                    unlike_texts.extend(collect_doc_texts(doc, &fields));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        for t in &unlike_texts {
            for tok in analyze(t) {
                terms.remove(&tok);
            }
        }
        if terms.is_empty() {
            return Ok((false, 0.0));
        }
        let min_df = bm
            .get("min_doc_freq")
            .and_then(to_f64)
            .map(|n| n as usize)
            .unwrap_or(5);
        let max_df = bm
            .get("max_doc_freq")
            .and_then(to_f64)
            .map(|n| n as usize)
            .unwrap_or(0);
        let mut doc_freq: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for id in &self.idx.order {
            let Some(d) = self.idx.docs.get(id) else { continue };
            let Some(dm) = d.as_object() else { continue };
            let mut seen = std::collections::HashSet::new();
            for txt in collect_doc_texts(dm, &fields) {
                for tok in analyze(&txt) {
                    seen.insert(tok);
                }
            }
            for tok in seen {
                if terms.contains_key(&tok) {
                    *doc_freq.entry(tok).or_insert(0) += 1;
                }
            }
        }
        terms.retain(|t, _| {
            let df = doc_freq.get(t).copied().unwrap_or(0);
            df >= min_df && (max_df == 0 || df <= max_df)
        });
        if terms.is_empty() {
            return Ok((false, 0.0));
        }
        let required = match bm.get("minimum_should_match") {
            Some(spec) => minimum_should_match(spec, terms.len(), 0),
            None => 1,
        };
        let include = bm.get("include").and_then(|v| v.as_bool()).unwrap_or(false);
        if !include && source_ids.contains(&self.id) {
            return Ok((false, 0.0));
        }
        let mut matched = 0usize;
        for txt in collect_doc_texts(
            self.doc.as_object().unwrap_or(&Map::new()),
            &fields,
        ) {
            let toks: std::collections::HashSet<String> =
                analyze(&txt).into_iter().collect();
            for t in terms.keys() {
                if toks.contains(t) {
                    matched += 1;
                }
            }
        }
        if matched >= required && matched > 0 {
            return Ok((true, matched as f64));
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // knn
    // -----------------------------------------------------------------------

    pub fn eval_knn(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[knn] query malformed")),
        };
        let field = bm
            .get("field")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        let qv: Vec<f64> = bm
            .get("query_vector")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(to_f64).collect())
            .unwrap_or_default();
        if field.is_empty() || qv.is_empty() {
            return Err(err_parsing("[knn] requires field and query_vector"));
        }
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        let dv: Vec<f64> = vals.iter().filter_map(to_f64).collect();
        if dv.len() != qv.len() {
            return Ok((false, 0.0));
        }
        let mut dot = 0.0;
        let mut na = 0.0;
        let mut nb = 0.0;
        for i in 0..qv.len() {
            dot += qv[i] * dv[i];
            na += qv[i] * qv[i];
            nb += dv[i] * dv[i];
        }
        if na == 0.0 || nb == 0.0 {
            return Ok((false, 0.0));
        }
        let cos = dot / (na.sqrt() * nb.sqrt());
        let sim = (cos + 1.0) / 2.0;
        Ok((true, sim))
    }

    // -----------------------------------------------------------------------
    // function_score
    // -----------------------------------------------------------------------

    pub fn eval_function_score(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[function_score] query malformed")),
        };
        let inner = bm
            .get("query")
            .and_then(|q| q.as_object())
            .cloned()
            .unwrap_or_else(|| {
                let mut m = Map::new();
                m.insert("match_all".to_string(), json!({}));
                m
            });
        let (matched, base) = self.eval(&inner)?;
        if !matched {
            return Ok((false, 0.0));
        }
        let mut functions = as_query_list(bm.get("functions"));
        if functions.is_empty() {
            if let Some(f) = bm.get("function").and_then(|f| f.as_object()) {
                functions = vec![Value::Object(f.clone())];
            }
        }
        let mut score_mode = bm
            .get("score_mode")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if score_mode.is_empty() {
            score_mode = "multiply".to_string();
        }
        if !matches!(
            score_mode.as_str(),
            "multiply" | "sum" | "avg" | "max" | "min" | "first"
        ) {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "No enum constant org.opensearch.index.query.functionscore.ScoreMode.{}",
                    score_mode.to_uppercase()
                ),
                400,
            ));
        }
        if let Some(w) = bm.get("weight").and_then(to_f64) {
            if w < 0.0 {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    "[weight] cannot be negative for a filtering function",
                    400,
                ));
            }
        }
        for f in &functions {
            if let Some(w) = f.get("weight").and_then(to_f64) {
                if w < 0.0 {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        "[weight] cannot be negative for a filtering function",
                        400,
                    ));
                }
            }
        }
        let mut weighted_sum = 0.0f64;
        let mut weight_sum = 0.0f64;
        let mut final_score = 0.0f64;
        let mut first = true;
        let mut any = false;
        let outer_boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let base = base * outer_boost;
        for fn_raw in &functions {
            let fno = match fn_raw.as_object() {
                Some(f) => f,
                None => continue,
            };
            if let Some(filter) = fno.get("filter").and_then(|f| f.as_object()) {
                match self.eval(filter) {
                    Ok((true, _)) => {}
                    Ok((false, _)) => continue,
                    Err(e) => return Err(e),
                }
            }
            let raw = self.apply_score_function(fno, base)?;
            let weight = fno.get("weight").and_then(to_f64).unwrap_or(1.0);
            let weighted = raw * weight;
            match score_mode.as_str() {
                "first" => {
                    if first {
                        final_score = weighted;
                    }
                }
                "max" => {
                    if first || weighted > final_score {
                        final_score = weighted;
                    }
                }
                "min" => {
                    if first || weighted < final_score {
                        final_score = weighted;
                    }
                }
                "sum" => final_score += weighted,
                "avg" => {
                    weighted_sum += weighted;
                    weight_sum += weight;
                }
                _ => {
                    if first {
                        final_score = weighted;
                    } else {
                        final_score *= weighted;
                    }
                }
            }
            any = true;
            first = false;
        }
        if !any {
            final_score = bm.get("weight").and_then(to_f64).unwrap_or(1.0);
        } else if score_mode == "avg" && weight_sum > 0.0 {
            final_score = weighted_sum / weight_sum;
        }
        let boost_mode = bm
            .get("boost_mode")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string();
        let mut combined = match boost_mode.as_str() {
            "replace" => final_score,
            "sum" => base + final_score,
            "avg" => (base + final_score) / 2.0,
            "max" => base.max(final_score),
            "min" => base.min(final_score),
            _ => base * final_score,
        };
        if let Some(mb) = bm.get("max_boost").and_then(to_f64) {
            if combined > mb {
                combined = mb;
            }
        }
        if let Some(ms) = bm.get("min_score").and_then(to_f64) {
            if combined < ms {
                return Ok((false, 0.0));
            }
        }
        Ok((true, combined))
    }

    fn apply_score_function(
        &self,
        fno: &Map<String, Value>,
        base: f64,
    ) -> Result<f64, ApiError> {
        if let Some(fvf) = fno.get("field_value_factor").and_then(|f| f.as_object()) {
            let field = fvf
                .get("field")
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string();
            let factor = fvf.get("factor").and_then(to_f64).unwrap_or(1.0);
            let modifier = fvf
                .get("modifier")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let mapped = crate::eval::mapping_field_def(self.idx, &field).is_some();
            let (vals, _) = self.values(&field);
            let mut val;
            if !vals.is_empty() {
                val = vals.first().and_then(to_f64).unwrap_or(0.0);
            } else if let Some(mv) = fvf.get("missing").and_then(to_f64) {
                val = mv;
            } else if !mapped {
                return Err(shard_error(
                    400,
                    "exception",
                    &format!(
                        "Unable to find a field mapper for field [{}]. No 'missing' value defined.",
                        field
                    ),
                ));
            } else {
                return Err(shard_error(
                    500,
                    "exception",
                    &format!("Missing value for field [{}]", field),
                ));
            }
            val *= factor;
            match modifier.as_str() {
                "log" => {
                    if val <= 0.0 {
                        return Err(shard_error(
                            400,
                            "illegal_argument_exception",
                            &format!(
                                "Unable to calculate logarithm of value [{}] for field [{}]. consider using log1p or log2p instead.",
                                val, field
                            ),
                        ));
                    }
                    val = val.log10();
                }
                "log1p" => val = (1.0 + val).log10(),
                "log2p" => val = (2.0 + val).log10(),
                "ln" => val = val.ln(),
                "ln1p" => val = val.ln_1p(),
                "ln2p" => val = (2.0 + val).ln(),
                "square" => val = val * val,
                "sqrt" => val = val.sqrt(),
                "reciprocal" => val = 1.0 / val,
                _ => {}
            }
            return Ok(val);
        }
        if let Some(rs) = fno.get("random_score").and_then(|r| r.as_object()) {
            let mut seed = rs.get("seed").and_then(integer_of).unwrap_or(0);
            if seed == 0 {
                seed = 42;
            }
            return Ok(pseudo_random(&self.id, seed));
        }
        if let Some(ss) = fno.get("script_score").and_then(|s| s.as_object()) {
            let sm = ss.get("script").and_then(|s| s.as_object());
            let src = sm
                .and_then(|s| s.get("source"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let mut params = crate::painless::script_params_of(sm.unwrap_or(&Map::new()));
            params.insert("__score".to_string(), json!(base));
            let env = ScriptEnv { c: self, params };
            if let Ok(v) = env.eval_script_expression(src) {
                return Ok(script_float(&v));
            }
            return Ok(1.0);
        }
        for fn_name in ["gauss", "linear", "exp"] {
            if let Some(dec) = fno.get(fn_name).and_then(|d| d.as_object()) {
                let field = dec
                    .get("field")
                    .and_then(|f| f.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        dec.keys()
                            .find(|k| {
                                !matches!(
                                    k.as_str(),
                                    "origin" | "scale" | "offset" | "decay" | "field"
                                )
                            })
                            .cloned()
                    })
                    .unwrap_or_default();
                if field.is_empty() {
                    return Ok(1.0);
                }
                let (vals, _) = self.values(&field);
                if vals.is_empty() {
                    return Ok(1.0);
                }
                let def = crate::eval::mapping_field_def(self.idx, &field).cloned().unwrap_or_default();
                let ftype = def
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let opts = dec
                    .get(&field)
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_else(|| dec.clone());
                let origin_raw = opts.get("origin").cloned().unwrap_or(Value::Null);
                let decay_v = opts.get("decay").and_then(to_f64).unwrap_or(0.5);
                let offset = opts.get("offset").and_then(to_f64).unwrap_or(0.0);
                let dist;
                let scale;
                if ftype == "geo_point" {
                    let (lat, lon) = geo_pair(&vals[0]).unwrap_or((0.0, 0.0));
                    let (olat, olon) = geo_pair(&origin_raw).unwrap_or((0.0, 0.0));
                    dist = haversine_km(olat, olon, lat, lon);
                    scale = parse_distance_km(
                        &opts.get("scale").map(preview).unwrap_or_default(),
                    )
                    .unwrap_or(1.0);
                } else if ftype == "date" || ftype == "date_nanos" {
                    let val = crate::fields::parse_date_any(&def, &value_to_string(&vals[0]).unwrap_or_default())
                        .unwrap_or(0) as f64;
                    let origin = if let Value::String(s) = &origin_raw {
                        crate::fields::parse_date_any(&def, s).unwrap_or(0) as f64
                    } else {
                        origin_raw.as_f64().unwrap_or(0.0)
                    };
                    dist = (val - origin).abs();
                    scale = parse_duration_ms(
                        &opts.get("scale").map(preview).unwrap_or_default(),
                    )
                    .unwrap_or(1) as f64;
                } else {
                    let val = vals.first().and_then(to_f64).unwrap_or(0.0);
                    let origin = origin_raw.as_f64().unwrap_or(0.0);
                    dist = (val - origin).abs();
                    scale = opts.get("scale").and_then(to_f64).unwrap_or(1.0);
                }
                if dist <= offset || scale <= 0.0 {
                    return Ok(1.0);
                }
                let x = (dist - offset) / scale;
                return Ok(match fn_name {
                    "gauss" => decay_v.powf(x * x),
                    "exp" => decay_v.powf(x),
                    "linear" => (1.0 - (1.0 - decay_v) * x).max(0.0),
                    _ => 1.0,
                });
            }
        }
        Ok(1.0)
    }

    // -----------------------------------------------------------------------
    // query_string
    // -----------------------------------------------------------------------

    pub fn eval_query_string(
        &self,
        body: &Value,
        simple: bool,
    ) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("query_string malformed")),
        };
        let mut query = bm
            .get("query")
            .and_then(value_to_string)
            .unwrap_or_default();
        if query.trim().is_empty() {
            return Ok((false, 0.0));
        }
        if !valid_query_string(&query) {
            return Err(query_string_parse_error(&query));
        }
        if bm
            .get("allow_leading_wildcard")
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            for tok in tokenize_query_string(&query) {
                let val = tok.split_once(':').map(|(_, v)| v).unwrap_or(&tok);
                if val.starts_with('*') || val.starts_with('?') {
                    return Err(query_string_parse_error(&query));
                }
            }
        }
        if query.is_empty() && simple {
            query = bm
                .get("query")
                .and_then(value_to_string)
                .unwrap_or_default();
        }
        let operator = bm
            .get("default_operator")
            .and_then(|o| o.as_str())
            .unwrap_or("OR")
            .to_uppercase();
        if operator != "AND" && operator != "OR" {
            return Err(ApiError::new(
                "illegal_argument_exception",
                &format!(
                    "No enum constant org.opensearch.index.query.Operator.{}",
                    operator
                ),
                400,
            ));
        }
        let mut default_field = bm
            .get("default_field")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if default_field.is_empty() {
            default_field = "*".to_string();
        }
        let raw_fields = fields_of(bm.get("fields"));
        let fields = expand_field_patterns(self.idx, &raw_fields);
        if !raw_fields.is_empty() && fields.is_empty() {
            return Ok((false, 0.0));
        }
        let phrase_slop = bm.get("phrase_slop").and_then(to_f64).unwrap_or(0.0);
        if fields.is_empty()
            && default_field != "*"
            && !query.chars().any(|c| {
                matches!(
                    c,
                    ':' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '~' | '*'
                        | '?' | '\\' | '+' | '-' | '!' | '&' | '|'
                )
            })
            && query.split_whitespace().count() > 1
        {
            if let Some(def) = crate::eval::mapping_field_def(self.idx, &default_field) {
                let ftype = def.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let split = def
                    .get("split_queries_on_whitespace")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if matches!(ftype, "keyword" | "constant_keyword" | "wildcard") && !split {
                    let mut inner = Map::new();
                    inner.insert(default_field.clone(), Value::String(query.clone()));
                    let mut q = Map::new();
                    q.insert("term".to_string(), Value::Object(inner));
                    return self.eval(&q);
                }
            }
        }
        let qtype = bm.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if qtype == "phrase" {
            let targets = if !fields.is_empty() {
                fields.clone()
            } else if default_field != "*" {
                vec![default_field.clone()]
            } else {
                expand_field_patterns(self.idx, &["*".to_string()])
            };
            let mut shoulds = Vec::new();
            for f in targets {
                let (name, _) = strip_field_boost(&f);
                let mut inner = Map::new();
                if phrase_slop > 0.0 {
                    let mut opt = Map::new();
                    opt.insert("query".to_string(), Value::String(query.clone()));
                    opt.insert("slop".to_string(), json!(phrase_slop as i64));
                    inner.insert(name, Value::Object(opt));
                } else {
                    inner.insert(name, Value::String(query.clone()));
                }
                let mut q = Map::new();
                q.insert("match_phrase".to_string(), Value::Object(inner));
                shoulds.push(Value::Object(q));
            }
            let q = if shoulds.len() == 1 {
                match shoulds.pop() {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                }
            } else {
                bool_query("should", shoulds, 0)
            };
            return self.eval(&q);
        }
        let lenient = bm.get("lenient").and_then(|v| v.as_bool()).unwrap_or(false);
        let fuzzy_transpositions = bm
            .get("fuzzy_transpositions")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut p = QsParser {
            tokens: tokenize_query_string(&query),
            pos: 0,
            default_: default_field.clone(),
            fields: fields.clone(),
            operator,
            phrase_slop,
            fuzzy_transpositions,
            lenient,
            raw_query: query.clone(),
            c: self,
        };
        let toks = tokenize_query_string(&query);
        if !toks.is_empty()
            && toks
                .iter()
                .all(|t| (t.starts_with('+') || t.starts_with('-')) && t.len() > 1)
        {
            let mut must = Vec::new();
            let mut must_not = Vec::new();
            for t in &toks {
                let sign = t.as_bytes()[0] as char;
                let q = p.value_query(&default_field, &t[1..])?;
                if sign == '+' {
                    must.push(Value::Object(q));
                } else {
                    must_not.push(Value::Object(q));
                }
            }
            let mut b = Map::new();
            if !must.is_empty() {
                b.insert("must".to_string(), Value::Array(must));
            }
            if !must_not.is_empty() {
                b.insert("must_not".to_string(), Value::Array(must_not));
            }
            let mut q = Map::new();
            q.insert("bool".to_string(), Value::Object(b));
            return self.eval(&q);
        }
        let mut q = p.parse()?;
        if default_field == "*" && fields.is_empty() {
            q = expand_multifield(&q, self);
        }
        if let Some(msm) = bm.get("minimum_should_match") {
            apply_query_msm(&mut q, msm);
        }
        self.eval(&q)
    }
    pub fn eval_simple_query_string(
        &self,
        body: &Value,
    ) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("simple_query_string malformed")),
        };
        for k in bm.keys() {
            if !matches!(
                k.as_str(),
                "query"
                    | "fields"
                    | "default_operator"
                    | "flags"
                    | "analyze_wildcard"
                    | "minimum_should_match"
                    | "fuzzy_transpositions"
                    | "quote_field_suffix"
                    | "lenient"
                    | "boost"
                    | "auto_generate_synonyms_phrase_query"
            ) {
                return Err(err_parsing(&format!(
                    "[simple_query_string] unsupported field [{}]",
                    k
                )));
            }
        }
        let query = bm
            .get("query")
            .and_then(value_to_string)
            .unwrap_or_default();
        if query.trim().is_empty() {
            return Ok((false, 0.0));
        }
        let flags = match SqsFlags::parse(bm.get("flags")) {
            Ok(f) => f,
            Err(bad) => {
                return Err(ApiError::new(
                    "illegal_argument_exception",
                    &format!("Unknown simple_query_string flag [{}]", bad),
                    400,
                ));
            }
        };
        let raw_fields = fields_of(bm.get("fields"));
        let explicit_fields = !raw_fields.is_empty();
        let fields = if explicit_fields {
            expand_field_patterns(self.idx, &raw_fields)
        } else {
            expand_field_patterns(self.idx, &["*".to_string()])
        };
        if fields.is_empty() {
            return Ok((false, 0.0));
        }
        let default_and = bm
            .get("default_operator")
            .and_then(|o| o.as_str())
            .map(|o| o.eq_ignore_ascii_case("and"))
            .unwrap_or(false);
        let msm = bm
            .get("minimum_should_match")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let lenient = bm.get("lenient").and_then(|v| v.as_bool()).unwrap_or(false);
        let fuzzy_transpositions = bm
            .get("fuzzy_transpositions")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let quote_suffix = bm
            .get("quote_field_suffix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let explicit: Vec<String> = raw_fields
            .iter()
            .map(|f| strip_field_boost(f).0)
            .filter(|f| !f.contains('*') && !f.contains('?'))
            .collect();
        let mut p = SqsParser {
            tokens: tokenize_query_string(&query),
            pos: 0,
            default_and,
            flags,
            fields,
            explicit,
            explicit_fields,
            lenient,
            quote_suffix,
            fuzzy_transpositions,
            msm,
            query: query.clone(),
            c: self,
        };
        match p.parse()? {
            Some(q) => self.eval(&q),
            None => Ok((false, 0.0)),
        }
    }
}

fn span_field_term(body: &Value) -> Result<(String, String), ApiError> {
    let bm = match body.as_object() {
        Some(m) => m,
        None => return Err(err_parsing("span query malformed")),
    };
    let (field, val) = match single_entry(bm) {
        Some((k, v)) => (k.clone(), v.clone()),
        None => return Err(err_parsing("span query malformed")),
    };
    let term = if let Some(m) = val.as_object() {
        m.get("value").and_then(value_to_string).unwrap_or_default()
    } else {
        value_to_string(&val).unwrap_or_default()
    };
    Ok((field, term))
}

impl<'a> QueryCtx<'a> {
    fn span_match(&self, field: &str, term: &str) -> Result<bool, ApiError> {
        Ok(self.eval_term(field, &Value::String(term.to_string()), 1.0)?.0)
    }
}

pub(crate) fn find_join_field(idx: &Index) -> Option<String> {
    let props = idx.mappings.get("properties")?.as_object()?;
    for (name, raw) in props {
        let def = match raw.as_object() {
            Some(d) => d,
            None => continue,
        };
        if def.get("type").and_then(|t| t.as_str()) == Some("join") {
            return Some(name.clone());
        }
    }
    None
}

fn collect_path(v: &Value, path: &str) -> Vec<Value> {
    if path.is_empty() {
        return match v {
            Value::Array(a) => a.clone(),
            other => vec![other.clone()],
        };
    }
    if let Some(arr) = v.as_array() {
        let mut out = Vec::new();
        for e in arr {
            out.extend(collect_path(e, path));
        }
        return out;
    }
    let (head, rest) = match path.split_once('.') {
        Some((h, r)) => (h, r),
        None => (path, ""),
    };
    match v.get(head) {
        Some(next) => collect_path(next, rest),
        None => Vec::new(),
    }
}

pub(crate) fn flatten_scope(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                flatten_scope(&format!("{}.{}", prefix, k), val, out);
            }
        }
        Value::Array(a) => {
            for e in a {
                flatten_scope(prefix, e, out);
            }
        }
        other => {
            let entry = out
                .entry(prefix.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(arr) = entry.as_array_mut() {
                arr.push(other.clone());
            }
        }
    }
}

pub(crate) fn join_parts(v: Option<&Value>) -> (String, String) {
    match v {
        Some(Value::String(s)) => (s.clone(), String::new()),
        Some(Value::Object(m)) => (
            m.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            m.get("parent")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        _ => (String::new(), String::new()),
    }
}

fn geo_point_of(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::Object(m) => {
            if let (Some(lat), Some(lon)) = (m.get("lat").and_then(to_f64), m.get("lon").and_then(to_f64)) {
                return Some((lat, lon));
            }
            let typ = m.get("type").and_then(|t| t.as_str()).unwrap_or("").to_ascii_lowercase();
            if typ == "point" {
                if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                    if coords.len() >= 2 {
                        return Some((to_f64(&coords[1])?, to_f64(&coords[0])?));
                    }
                }
            }
            None
        }
        Value::Array(a) => {
            if a.len() == 2 {
                let lon = to_f64(&a[0])?;
                let lat = to_f64(&a[1])?;
                return Some((lat, lon));
            }
            None
        }
        Value::String(s) => {
            let t = s.trim();
            if t.to_ascii_uppercase().starts_with("POINT") {
                let inner = t
                    .split_once('(')
                    .and_then(|(_, r)| r.rsplit_once(')'))
                    .map(|(l, _)| l)
                    .unwrap_or(t);
                let parts: Vec<&str> = inner.split_whitespace().collect();
                if parts.len() >= 2 {
                    let lon: f64 = parts[0].parse().ok()?;
                    let lat: f64 = parts[1].parse().ok()?;
                    return Some((lat, lon));
                }
                return None;
            }
            let parts: Vec<&str> = t.split(',').collect();
            if parts.len() == 2 {
                if let (Ok(lat), Ok(lon)) =
                    (parts[0].trim().parse::<f64>(), parts[1].trim().parse::<f64>())
                {
                    return Some((lat, lon));
                }
            }
            decode_geohash(t)
        }
        _ => None,
    }
}

fn decode_geohash(s: &str) -> Option<(f64, f64)> {
    const B32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let mut even = true;
    let mut lat_min = -90.0f64;
    let mut lat_max = 90.0f64;
    let mut lon_min = -180.0f64;
    let mut lon_max = 180.0f64;
    let mut seen = false;
    for c in s.chars() {
        let ci = B32.iter().position(|b| *b as char == c.to_ascii_lowercase())?;
        seen = true;
        for bit in (0..5).rev() {
            let one = (ci >> bit) & 1 == 1;
            if even {
                let mid = (lon_min + lon_max) / 2.0;
                if one {
                    lon_min = mid;
                } else {
                    lon_max = mid;
                }
            } else {
                let mid = (lat_min + lat_max) / 2.0;
                if one {
                    lat_min = mid;
                } else {
                    lat_max = mid;
                }
            }
            even = !even;
        }
    }
    if !seen {
        return None;
    }
    Some(((lat_min + lat_max) / 2.0, (lon_min + lon_max) / 2.0))
}

fn valid_latlon(lat: f64, lon: f64) -> bool {
    (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)
}

fn unit_to_meters(unit: &str) -> f64 {
    match unit.to_lowercase().as_str() {
        "mi" | "miles" => 1609.344,
        "yd" | "yards" => 0.9144,
        "ft" | "feet" => 0.3048,
        "in" | "inch" => 0.0254,
        "km" | "kilometers" => 1000.0,
        "cm" => 0.01,
        "mm" => 0.001,
        "nmi" | "nauticalmiles" => 1852.0,
        _ => 1.0,
    }
}

fn parse_distance_meters(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(t) => {
            let s = t.trim().to_lowercase();
            let bytes = s.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' {
                    i += 1;
                } else {
                    break;
                }
            }
            let num: f64 = s[..i].trim().parse().ok()?;
            let unit = s[i..].trim();
            Some(num * unit_to_meters(unit))
        }
        _ => None,
    }
}

fn haversine_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371008.8;
    let rad = std::f64::consts::PI / 180.0;
    let d_lat = (lat2 - lat1) * rad;
    let d_lon = (lon2 - lon1) * rad;
    let a = (d_lat / 2.0).sin() * (d_lat / 2.0).sin()
        + (lat1 * rad).cos() * (lat2 * rad).cos() * (d_lon / 2.0).sin() * (d_lon / 2.0).sin();
    2.0 * r * a.sqrt().asin()
}

#[derive(Default, Clone)]
pub(crate) struct GeoShape {
    typ: String,
    lat: f64,
    lon: f64,
    radius_m: f64,
    top: f64,
    left: f64,
    bot: f64,
    right: f64,
    points: Vec<(f64, f64)>,
}

fn parse_geo_shape(v: &Value) -> Option<GeoShape> {
    if let Some(s) = v.as_str() {
        return parse_wkt_shape(s);
    }
    let m = v.as_object()?;
    let typ = m
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut gs = GeoShape {
        typ: typ.clone(),
        ..Default::default()
    };
    match typ.as_str() {
        "point" => {
            match m.get("coordinates") {
                Some(Value::Array(coords)) => {
                    if coords.len() >= 2 {
                        gs.lon = to_f64(&coords[0]).unwrap_or(0.0);
                        gs.lat = to_f64(&coords[1]).unwrap_or(0.0);
                    }
                }
                Some(Value::String(s)) => {
                    if let Some((lat, lon)) = geo_point_of(&Value::String(s.clone())) {
                        gs.lat = lat;
                        gs.lon = lon;
                    }
                }
                _ => {}
            }
            Some(gs)
        }
        "circle" => {
            if let Some(Value::String(r)) = m.get("radius") {
                gs.radius_m = parse_distance_meters(&Value::String(r.clone())).unwrap_or(0.0);
            } else if let Some(r) = m.get("radius").and_then(to_f64) {
                gs.radius_m = r;
            }
            if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                if coords.len() >= 2 {
                    gs.lon = to_f64(&coords[0]).unwrap_or(0.0);
                    gs.lat = to_f64(&coords[1]).unwrap_or(0.0);
                }
            }
            Some(gs)
        }
        "envelope" => {
            if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                if coords.len() >= 2 {
                    if let Some(tl) = coords[0].as_array() {
                        if tl.len() >= 2 {
                            gs.left = to_f64(&tl[0]).unwrap_or(0.0);
                            gs.top = to_f64(&tl[1]).unwrap_or(0.0);
                        }
                    }
                    if let Some(br) = coords[1].as_array() {
                        if br.len() >= 2 {
                            gs.right = to_f64(&br[0]).unwrap_or(0.0);
                            gs.bot = to_f64(&br[1]).unwrap_or(0.0);
                        }
                    }
                }
            }
            Some(gs)
        }
        "polygon" => {
            if let Some(rings) = m.get("coordinates").and_then(|c| c.as_array()) {
                if !rings.is_empty() {
                    if let Some(ring) = rings[0].as_array() {
                        for pt in ring {
                            if let Some(arr) = pt.as_array() {
                                if arr.len() >= 2 {
                                    let lon = to_f64(&arr[0]).unwrap_or(0.0);
                                    let lat = to_f64(&arr[1]).unwrap_or(0.0);
                                    gs.points.push((lat, lon));
                                }
                            }
                        }
                    }
                }
            }
            Some(gs)
        }
        "linestring" => {
            if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                for pt in coords {
                    if let Some(arr) = pt.as_array() {
                        if arr.len() >= 2 {
                            let lon = to_f64(&arr[0]).unwrap_or(0.0);
                            let lat = to_f64(&arr[1]).unwrap_or(0.0);
                            gs.points.push((lat, lon));
                        }
                    }
                }
            }
            Some(gs)
        }
        "multipoint" => {
            if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                for pt in coords {
                    if let Some(arr) = pt.as_array() {
                        if arr.len() >= 2 {
                            gs.points.push((to_f64(&arr[1]).unwrap_or(0.0), to_f64(&arr[0]).unwrap_or(0.0)));
                        }
                    }
                }
            }
            Some(gs)
        }
        "multilinestring" | "multipolygon" => {
            fn collect(v: &Value, out: &mut Vec<(f64, f64)>) {
                if let Some(a) = v.as_array() {
                    if a.len() >= 2 && a.iter().all(|x| x.is_number() || x.is_string()) {
                        let lon = to_f64(&a[0]).unwrap_or(0.0);
                        let lat = to_f64(&a[1]).unwrap_or(0.0);
                        out.push((lat, lon));
                    } else {
                        for e in a {
                            collect(e, out);
                        }
                    }
                }
            }
            if let Some(coords) = m.get("coordinates") {
                collect(coords, &mut gs.points);
            }
            Some(gs)
        }
        "geometrycollection" => {
            let mut out = Vec::new();
            if let Some(geoms) = m.get("geometries").and_then(|g| g.as_array()) {
                for g in geoms {
                    if let Some(child) = parse_geo_shape(g) {
                        match child.typ.as_str() {
                            "point" => out.push((child.lat, child.lon)),
                            "envelope" => {
                                out.push((child.top, child.left));
                                out.push((child.bot, child.right));
                            }
                            _ => out.extend(child.points),
                        }
                    }
                }
            }
            gs.typ = "multipoint".to_string();
            gs.points = out;
            Some(gs)
        }
        _ => None,
    }
}

pub(crate) fn parse_wkt_shape(s: &str) -> Option<GeoShape> {
    let t = s.trim();
    let up = t.to_ascii_uppercase();
    let inner = t
        .split_once('(')
        .and_then(|(_, r)| r.rsplit_once(')'))
        .map(|(l, _)| l.trim())
        .unwrap_or(t);
    if up.starts_with("POINT") {
        let parts: Vec<&str> = inner.split_whitespace().collect();
        if parts.len() >= 2 {
            let lon: f64 = parts[0].parse().ok()?;
            let lat: f64 = parts[1].parse().ok()?;
            return Some(GeoShape {
                typ: "point".to_string(),
                lat,
                lon,
                ..Default::default()
            });
        }
        return None;
    }
    if up.starts_with("BBOX") {
        let nums: Vec<f64> = inner
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if nums.len() >= 4 {
            return Some(GeoShape {
                typ: "envelope".to_string(),
                left: nums[0],
                right: nums[1],
                top: nums[2],
                bot: nums[3],
                ..Default::default()
            });
        }
        return None;
    }
    if up.starts_with("MULTIPOINT") {
        let body = inner.trim().trim_start_matches('(').trim_end_matches(')');
        let mut points = Vec::new();
        for pt in body.split(',') {
            let nums: Vec<f64> = pt
                .trim()
                .trim_matches(|c| c == '(' || c == ')')
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if nums.len() >= 2 {
                points.push((nums[1], nums[0]));
            }
        }
        if points.is_empty() {
            return None;
        }
        return Some(GeoShape {
            typ: "multipoint".to_string(),
            points,
            ..Default::default()
        });
    }
    if up.starts_with("POLYGON") || up.starts_with("LINESTRING") {
        let body = inner.trim().trim_start_matches('(').trim_end_matches(')');
        let ring = body.split(')').next().unwrap_or(body);
        let mut points = Vec::new();
        for pt in ring.split(',') {
            let nums: Vec<f64> = pt
                .trim()
                .trim_matches(|c| c == '(' || c == ')')
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if nums.len() >= 2 {
                points.push((nums[1], nums[0]));
            }
        }
        if points.is_empty() {
            return None;
        }
        return Some(GeoShape {
            typ: if up.starts_with("LINESTRING") {
                "linestring".to_string()
            } else {
                "polygon".to_string()
            },
            points,
            ..Default::default()
        });
    }
    None
}

fn point_in_polygon(lat: f64, lon: f64, points: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = points.len();
    if n == 0 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = points[i];
        let (yj, xj) = points[j];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn shape_corners(gs: &GeoShape) -> Vec<(f64, f64)> {
    match gs.typ.as_str() {
        "point" => vec![(gs.lat, gs.lon)],
        "envelope" => vec![
            (gs.top, gs.left),
            (gs.top, gs.right),
            (gs.bot, gs.right),
            (gs.bot, gs.left),
        ],
        "circle" => vec![(gs.lat, gs.lon)],
        _ => gs.points.clone(),
    }
}

fn shape_contains_point(gs: &GeoShape, lat: f64, lon: f64) -> bool {
    match gs.typ.as_str() {
        "point" => (lat - gs.lat).abs() < 1e-9 && (lon - gs.lon).abs() < 1e-9,
        "envelope" => {
            lat <= gs.top && lat >= gs.bot && lon >= gs.left && lon <= gs.right
        }
        "circle" => haversine_meters(gs.lat, gs.lon, lat, lon) <= gs.radius_m,
        "polygon" => point_in_polygon(lat, lon, &gs.points),
        "linestring" => {
            gs.points.windows(2).any(|w| {
                point_on_segment((lat, lon), w[0], w[1])
            })
        }
        "multipoint" => gs.points.iter().any(|p| (p.0 - lat).abs() < 1e-9 && (p.1 - lon).abs() < 1e-9),
        "multilinestring" | "multipolygon" => {
            gs.points.windows(2).any(|w| point_on_segment((lat, lon), w[0], w[1]))
                || point_in_polygon(lat, lon, &gs.points)
        }
        _ => false,
    }
}

fn point_on_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> bool {
    let cross = (p.1 - a.1) * (b.0 - a.0) - (p.0 - a.0) * (b.1 - a.1);
    if cross.abs() > 1e-7 {
        return false;
    }
    p.0 >= a.0.min(b.0) - 1e-9
        && p.0 <= a.0.max(b.0) + 1e-9
        && p.1 >= a.1.min(b.1) - 1e-9
        && p.1 <= a.1.max(b.1) + 1e-9
}

fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let ccw = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
        (r.0 - p.0) * (q.1 - p.1) - (q.0 - p.0) * (r.1 - p.1)
    };
    let d1 = ccw(a, b, c);
    let d2 = ccw(a, b, d);
    let d3 = ccw(c, d, a);
    let d4 = ccw(c, d, b);
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
    {
        return true;
    }
    (d1.abs() < 1e-9 && point_on_segment(c, a, b))
        || (d2.abs() < 1e-9 && point_on_segment(d, a, b))
        || (d3.abs() < 1e-9 && point_on_segment(a, c, d))
        || (d4.abs() < 1e-9 && point_on_segment(b, c, d))
}

fn shape_edges(gs: &GeoShape) -> Vec<((f64, f64), (f64, f64))> {
    let pts = shape_corners(gs);
    let mut out = Vec::new();
    if pts.len() < 2 {
        return out;
    }
    if gs.typ == "polygon" {
        for i in 0..pts.len() {
            out.push((pts[i], pts[(i + 1) % pts.len()]));
        }
    } else if gs.typ == "linestring" {
        for w in pts.windows(2) {
            out.push((w[0], w[1]));
        }
    } else if gs.typ == "envelope" {
        for i in 0..4 {
            out.push((pts[i], pts[(i + 1) % 4]));
        }
    }
    out
}

fn shape_intersect_shape(a: &GeoShape, b: &GeoShape) -> bool {
    if a.typ == "point" {
        return shape_contains_point(b, a.lat, a.lon);
    }
    if b.typ == "point" {
        return shape_contains_point(a, b.lat, b.lon);
    }
    for p in shape_corners(a) {
        if shape_contains_point(b, p.0, p.1) {
            return true;
        }
    }
    for p in shape_corners(b) {
        if shape_contains_point(a, p.0, p.1) {
            return true;
        }
    }
    let ea = shape_edges(a);
    let eb = shape_edges(b);
    for (a1, a2) in &ea {
        for (b1, b2) in &eb {
            if segments_intersect(*a1, *a2, *b1, *b2) {
                return true;
            }
        }
    }
    false
}

fn shape_contains_shape(container: &GeoShape, inner: &GeoShape) -> bool {
    if inner.typ == "point" {
        return shape_contains_point(container, inner.lat, inner.lon);
    }
    for p in shape_corners(inner) {
        if !shape_contains_point(container, p.0, p.1) {
            return false;
        }
    }
    true
}

fn shape_intersects(gs: &GeoShape, lat: f64, lon: f64) -> bool {
    match gs.typ.as_str() {
        "point" => lat == gs.lat && lon == gs.lon,
        "envelope" => lat <= gs.top && lat >= gs.bot && lon >= gs.left && lon <= gs.right,
        "circle" => haversine_meters(gs.lat, gs.lon, lat, lon) <= gs.radius_m,
        "polygon" => {
            let mut inside = false;
            let n = gs.points.len();
            if n == 0 {
                return false;
            }
            let mut j = n - 1;
            for i in 0..n {
                let (yi, xi) = gs.points[i];
                let (yj, xj) = gs.points[j];
                if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
                    inside = !inside;
                }
                j = i;
            }
            inside
        }
        _ => false,
    }
}

fn shape_points(gs: &GeoShape) -> Vec<(f64, f64)> {
    match gs.typ.as_str() {
        "point" => vec![(gs.lat, gs.lon)],
        "envelope" => vec![(gs.top, gs.left), (gs.bot, gs.right)],
        "circle" => vec![(gs.lat, gs.lon)],
        "polygon" => gs.points.clone(),
        _ => Vec::new(),
    }
}

fn numeric_int(v: &Value) -> usize {
    if let Some(n) = integer_of(v) {
        return n.max(0) as usize;
    }
    to_f64(v).unwrap_or(0.0).max(0.0) as usize
}

fn script_minimum_should_match(src: &str, count: usize, _matched: usize) -> Option<usize> {
    let src: String = src.chars().filter(|c| *c != ' ').collect();
    if let Some(i) = src.find("Math.min(") {
        let rest = &src[i + "Math.min(".len()..];
        let inner = rest.split(')').next().unwrap_or(rest);
        let mut vals = Vec::new();
        for part in inner.split(',') {
            if part.contains("params.num_terms") {
                vals.push(count);
            } else if let Ok(n) = part.parse::<usize>() {
                vals.push(n);
            }
        }
        if !vals.is_empty() {
            return Some(*vals.iter().min().unwrap_or(&1));
        }
    }
    if let Some(i) = src.find("Math.max(") {
        let rest = &src[i + "Math.max(".len()..];
        let inner = rest.split(')').next().unwrap_or(rest);
        let mut vals = Vec::new();
        for part in inner.split(',') {
            if part.contains("params.num_terms") {
                vals.push(count);
            } else if let Ok(n) = part.parse::<usize>() {
                vals.push(n);
            }
        }
        if !vals.is_empty() {
            return Some(*vals.iter().max().unwrap_or(&1));
        }
    }
    if src.contains("params.num_terms") {
        return Some(count);
    }
    src.parse::<usize>().ok()
}

fn pseudo_random(id: &str, seed: i64) -> f64 {
    let mut h = (seed as u64)
        .wrapping_mul(1469598103934665603)
        .wrapping_add(14695981039346656037);
    for b in id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    (h >> 11) as f64 / (1u64 << 53) as f64
}

pub(crate) fn percolate_inputs(
    idx: &Index,
    bm: &Map<String, Value>,
) -> Result<Option<Vec<(usize, Map<String, Value>)>>, ApiError> {
    if let Some(doc) = bm.get("document").and_then(|d| d.as_object()) {
        return Ok(Some(vec![(0, doc.clone())]));
    }
    if let Some(docs) = bm.get("documents").and_then(|d| d.as_array()) {
        let mut out = Vec::new();
        for (i, d) in docs.iter().enumerate() {
            if let Some(o) = d.as_object() {
                out.push((i, o.clone()));
            }
        }
        return Ok(Some(out));
    }
    if let Some(id) = bm.get("id").and_then(value_to_string) {
        if let Some(doc) = idx.docs.get(&id).and_then(|d| d.as_object()) {
            return Ok(Some(vec![(0, doc.clone())]));
        }
        return Err(ApiError::new(
            "resource_not_found_exception",
            &format!("document missing [{}]", id),
            404,
        ));
    }
    Err(err_parsing("[percolate] requires 'document'"))
}

fn collect_doc_texts(doc: &Map<String, Value>, fields: &[String]) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => {
                for e in a {
                    walk(e, out);
                }
            }
            Value::Object(m) => {
                for e in m.values() {
                    walk(e, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if !fields.is_empty() {
        let docv = Value::Object(doc.clone());
        for f in fields {
            if let Some(v) = field_value(&docv, f) {
                walk(v, &mut out);
            }
        }
        return out;
    }
    for (k, v) in doc {
        if k.starts_with('_') {
            continue;
        }
        walk(v, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// query string parser
// ---------------------------------------------------------------------------


#[derive(Clone, Copy)]
struct SqsFlags {
    prefix: bool,
    fuzzy: bool,
    phrase: bool,
    slop: bool,
    not: bool,
    all: bool,
}

impl SqsFlags {
    fn parse(v: Option<&Value>) -> Result<Self, String> {
        let raw = match v {
            None => return Ok(SqsFlags::all()),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Ok(SqsFlags::all()),
        };
        let mut f = SqsFlags {
            prefix: false,
            fuzzy: false,
            phrase: false,
            slop: false,
            not: false,
            all: false,
        };
        for part in raw.split('|') {
            let part = part.trim().to_uppercase();
            match part.as_str() {
                "ALL" => f = SqsFlags::all(),
                "NONE" => f = SqsFlags::none(),
                "PREFIX" => f.prefix = true,
                "FUZZY" => f.fuzzy = true,
                "PHRASE" => f.phrase = true,
                "SLOP" => f.slop = true,
                "NOT" => f.not = true,
                "AND" | "OR" | "ESCAPE" | "WHITESPACE" => {}
                _ => return Err(part),
            }
        }
        Ok(f)
    }

    fn all() -> Self {
        SqsFlags {
            prefix: true,
            fuzzy: true,
            phrase: true,
            slop: true,
            not: true,
            all: true,
        }
    }

    fn none() -> Self {
        SqsFlags {
            prefix: false,
            fuzzy: false,
            phrase: false,
            slop: false,
            not: false,
            all: false,
        }
    }
}

struct SqsParser<'a, 'b> {
    tokens: Vec<String>,
    pos: usize,
    default_and: bool,
    flags: SqsFlags,
    fields: Vec<String>,
    explicit: Vec<String>,
    explicit_fields: bool,
    lenient: bool,
    quote_suffix: String,
    fuzzy_transpositions: bool,
    msm: i64,
    query: String,
    c: &'a QueryCtx<'b>,
}

impl<'a, 'b> SqsParser<'a, 'b> {
    fn parse(&mut self) -> Result<Option<Map<String, Value>>, ApiError> {
        self.parse_or(false)
    }

    fn parse_or(&mut self, in_group: bool) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut parts: Vec<Map<String, Value>> = Vec::new();
        loop {
            if let Some(q) = self.parse_seq(in_group)? {
                parts.push(q);
            }
            if self.pos < self.tokens.len() && self.tokens[self.pos] == "|" {
                self.pos += 1;
                continue;
            }
            break;
        }
        if parts.is_empty() {
            return Ok(None);
        }
        if parts.len() == 1 {
            return Ok(parts.pop());
        }
        let shoulds = parts.into_iter().map(Value::Object).collect();
        Ok(Some(bool_query("should", shoulds, 0)))
    }

    fn parse_seq(&mut self, _in_group: bool) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut clauses: Vec<(i8, Map<String, Value>)> = Vec::new();
        let mut pending: Option<i8> = None;
        while self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            if tok == ")" || tok == "|" {
                break;
            }
            if tok == "+" {
                pending = Some(1);
                self.pos += 1;
                continue;
            }
            if tok == "-" {
                pending = Some(-1);
                self.pos += 1;
                continue;
            }
            let mut sign = pending.take().unwrap_or(0);
            let mut text = tok.clone();
            if text.len() > 1 && (text.starts_with('+') || text.starts_with('-')) {
                sign = if text.starts_with('-') { -1 } else { 1 };
                text = text[1..].to_string();
            }
            self.pos += 1;
            let q = if text == "(" {
                let inner = self.parse_or(true)?;
                if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                    self.pos += 1;
                }
                inner
            } else {
                self.atom(&text)?
            };
            if let Some(q) = q {
                clauses.push((sign, q));
            }
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        if !self.flags.not {
            clauses.retain(|(s, _)| *s != -1);
            if clauses.is_empty() {
                return Ok(None);
            }
        }
        let has_plus = clauses.iter().any(|(s, _)| *s == 1);
        let has_positive = clauses.iter().any(|(s, _)| *s != -1);
        let mut must = Vec::new();
        let mut must_not = Vec::new();
        let mut should = Vec::new();
        let use_not = self.flags.not
            && (self.flags.prefix || self.flags.fuzzy || self.flags.phrase || self.flags.slop);
        if self.default_and {
            for (s, q) in clauses {
                if s == -1 {
                    must_not.push(Value::Object(q));
                } else {
                    must.push(Value::Object(q));
                }
            }
        } else if !has_positive || self.flags.all {
            for (s, q) in clauses {
                if s == -1 {
                    should.push(Value::Object(not_query(q)));
                } else if s == 1 || has_plus {
                    must.push(Value::Object(q));
                } else {
                    should.push(Value::Object(q));
                }
            }
        } else {
            for (s, q) in clauses {
                if s == -1 {
                    if use_not {
                        must_not.push(Value::Object(q));
                    }
                } else if s == 1 || has_plus {
                    must.push(Value::Object(q));
                } else {
                    should.push(Value::Object(q));
                }
            }
        }
        if must.is_empty() && must_not.is_empty() && should.len() == 1 {
            if let Some(Value::Object(m)) = should.pop() {
                return Ok(Some(m));
            }
        }
        if must.is_empty() && must_not.is_empty() && should.is_empty() {
            return Ok(None);
        }
        let mut b = Map::new();
        if !must.is_empty() {
            b.insert("must".to_string(), Value::Array(must));
        }
        if !must_not.is_empty() {
            b.insert("must_not".to_string(), Value::Array(must_not));
        }
        if !should.is_empty() {
            b.insert("should".to_string(), Value::Array(should));
            if self.msm > 0 {
                b.insert("minimum_should_match".to_string(), json!(self.msm));
            }
        }
        let mut q = Map::new();
        q.insert("bool".to_string(), Value::Object(b));
        Ok(Some(q))
    }

    fn atom(&self, tok: &str) -> Result<Option<Map<String, Value>>, ApiError> {
        if tok.starts_with('"') {
            if !self.flags.phrase {
                return Ok(None);
            }
            let body = tok.trim_start_matches('"');
            let end = match body.find('"') {
                Some(i) => i,
                None => return Ok(None),
            };
            let phrase = body[..end].to_string();
            if phrase.is_empty() {
                return Ok(None);
            }
            let rest = &body[end + 1..];
            let slop = if self.flags.slop {
                rest.strip_prefix('~')
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            return self.build_phrase(&phrase, slop);
        }
        if tok.contains(':') {
            return Ok(None);
        }
        if tok.contains('*') || tok.contains('?') {
            if !self.flags.prefix {
                return Ok(None);
            }
            return self.build_wildcard(tok);
        }
        if let Some(ti) = tok.rfind('~') {
            if ti > 0 {
                let suffix = &tok[ti + 1..];
                if suffix.is_empty() || suffix.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    if !self.flags.fuzzy {
                        return self.build_plain(&tok[..ti]);
                    }
                    let spec = if suffix.is_empty() {
                        json!(2)
                    } else {
                        json!(suffix.parse::<f64>().unwrap_or(1.0).trunc() as i64)
                    };
                    return self.build_fuzzy(&tok[..ti], spec);
                }
            }
        }
        self.build_plain(tok)
    }

    fn build_plain(&self, value: &str) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut shoulds = Vec::new();
        for f in &self.fields {
            let (name, _) = strip_field_boost(f);
            let def = mapping_field_def(self.c.idx, &name);
            let ftype = def
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if matches!(
                ftype,
                "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
                    | "scaled_float" | "unsigned_long"
            ) {
                let num = match value.parse::<f64>() {
                    Ok(n) => n,
                    Err(_) => {
                        if self.lenient || !self.explicit.contains(&name) {
                            continue;
                        }
                        return Err(query_string_parse_error(&self.query));
                    }
                };
                let mut inner = Map::new();
                inner.insert(name, json!(num));
                let mut q = Map::new();
                q.insert("term".to_string(), Value::Object(inner));
                shoulds.push(Value::Object(q));
                continue;
            }
            let mut inner = Map::new();
            if ftype == "keyword" || ftype == "constant_keyword" || ftype == "wildcard" {
                inner.insert(name, Value::String(value.to_string()));
                let mut q = Map::new();
                q.insert("term".to_string(), Value::Object(inner));
                shoulds.push(Value::Object(q));
            } else {
                inner.insert(name, Value::String(value.to_string()));
                let mut q = Map::new();
                q.insert("match".to_string(), Value::Object(inner));
                shoulds.push(Value::Object(q));
            }
        }
        Ok(sqs_combine(shoulds))
    }

    fn build_wildcard(&self, value: &str) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut shoulds = Vec::new();
        for f in &self.fields {
            let (name, _) = strip_field_boost(f);
            let mut inner = Map::new();
            inner.insert(name, Value::String(value.to_lowercase()));
            let mut q = Map::new();
            q.insert("wildcard".to_string(), Value::Object(inner));
            shoulds.push(Value::Object(q));
        }
        Ok(sqs_combine(shoulds))
    }

    fn build_fuzzy(
        &self,
        value: &str,
        spec: Value,
    ) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut shoulds = Vec::new();
        for f in &self.fields {
            let (name, _) = strip_field_boost(f);
            let mut opt = Map::new();
            opt.insert("query".to_string(), Value::String(value.to_string()));
            opt.insert("fuzziness".to_string(), spec.clone());
            opt.insert(
                "fuzzy_transpositions".to_string(),
                json!(self.fuzzy_transpositions),
            );
            let mut inner = Map::new();
            inner.insert(name, Value::Object(opt));
            let mut q = Map::new();
            q.insert("match".to_string(), Value::Object(inner));
            shoulds.push(Value::Object(q));
        }
        Ok(sqs_combine(shoulds))
    }

    fn build_phrase(&self, value: &str, slop: f64) -> Result<Option<Map<String, Value>>, ApiError> {
        let mut shoulds = Vec::new();
        for f in &self.fields {
            let (name, _) = strip_field_boost(f);
            let name = format!("{}{}", name, self.quote_suffix);
            let mut inner = Map::new();
            if slop > 0.0 {
                let mut opt = Map::new();
                opt.insert("query".to_string(), Value::String(value.to_string()));
                opt.insert("slop".to_string(), json!(slop as i64));
                inner.insert(name, Value::Object(opt));
            } else {
                inner.insert(name, Value::String(value.to_string()));
            }
            let mut q = Map::new();
            q.insert("match_phrase".to_string(), Value::Object(inner));
            shoulds.push(Value::Object(q));
        }
        Ok(sqs_combine(shoulds))
    }
}

fn sqs_combine(mut shoulds: Vec<Value>) -> Option<Map<String, Value>> {
    if shoulds.is_empty() {
        let mut q = Map::new();
        q.insert("match_none".to_string(), json!({}));
        return Some(q);
    }
    if shoulds.len() == 1 {
        if let Value::Object(m) = shoulds.remove(0) {
            return Some(m);
        }
    }
    Some(bool_query("should", shoulds, 0))
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

fn is_span_query(v: &Value) -> bool {
    if let Some(m) = v.as_object() {
        if m.len() == 1 {
            if let Some((k, _)) = single_entry(m) {
                return matches!(
                    k.as_str(),
                    "span_term"
                        | "span_near"
                        | "span_or"
                        | "span_not"
                        | "span_first"
                        | "span_multi"
                        | "span_containing"
                        | "span_within"
                        | "field_masking_span"
                );
            }
        }
    }
    false
}

fn span_shard_error(reason: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
    let cause = ApiError::new("query_shard_exception", reason, 400);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e
}

fn span_clause_label(q: &Value) -> String {
    if let Some(m) = q.as_object() {
        if let Some((kind, body)) = single_entry(m) {
            if kind == "span_term" {
                if let Some(bm) = body.as_object() {
                    if let Some((f, v)) = single_entry(bm) {
                        let term = if let Some(o) = v.as_object() {
                            o.get("value").and_then(value_to_string).unwrap_or_default()
                        } else {
                            value_to_string(v).unwrap_or_default()
                        };
                        return format!("{}:{}", f, term);
                    }
                }
            }
        }
    }
    "span query".to_string()
}

fn numeric_field_error(query: &str, value: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
    let reason = format!("For input string: \"{}\"", value);
    let cause = ApiError::new("query_shard_exception", &reason, 400);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    let _ = query;
    e
}

fn flatten_should(q: &Value, out: &mut Vec<Value>) {
    if let Some(m) = q.as_object() {
        if m.len() == 1 {
            if let Some(b) = m.get("bool").and_then(|b| b.as_object()) {
                if b.len() == 1 {
                    if let Some(arr) = b.get("should").and_then(|s| s.as_array()) {
                        for e in arr {
                            flatten_should(e, out);
                        }
                        return;
                    }
                }
            }
        }
    }
    out.push(q.clone());
}

fn apply_query_msm(q: &mut Map<String, Value>, spec: &Value) {
    let mut items = Vec::new();
    flatten_should(&Value::Object(q.clone()), &mut items);
    let n = items.len();
    if n <= 1 {
        return;
    }
    let required = minimum_should_match_checked(spec, n).unwrap_or(1);
    let mut b = Map::new();
    b.insert(
        "should".to_string(),
        Value::Array(items.into_iter().collect()),
    );
    b.insert("minimum_should_match".to_string(), json!(required));
    let mut out = Map::new();
    out.insert("bool".to_string(), Value::Object(b));
    *q = out;
}

fn query_string_parse_error(query: &str) -> ApiError {
    let mut e = ApiError::new("search_phase_execution_exception", "all shards failed", 400);
    let reason = format!("Failed to parse query [{}]", query);
    let mut qs = ApiError::new("query_shard_exception", &reason, 400);
    qs.caused_by = Some(Box::new(ApiError::new(
        "parse_exception",
        "Cannot parse query",
        400,
    )));
    e.root_cause = Some(Box::new(qs.clone()));
    e.caused_by = Some(Box::new(qs));
    e
}

fn valid_query_string(q: &str) -> bool {
    let bytes = q.as_bytes();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut in_regex = false;
    let mut escaped = false;
    let mut brackets = 0i32;
    for &c in bytes {
        if escaped {
            escaped = false;
            continue;
        }
        if c == b'\\' {
            escaped = true;
            continue;
        }
        if c == b'"' {
            in_quote = !in_quote;
            continue;
        }
        if in_quote {
            continue;
        }
        if c == b'/' {
            in_regex = !in_regex;
            continue;
        }
        if in_regex {
            continue;
        }
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            b'[' | b'{' => brackets += 1,
            b']' | b'}' => {
                brackets -= 1;
                if brackets < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    if in_quote || depth != 0 || brackets != 0 {
        return false;
    }
    let tokens = tokenize_query_string(q);
    for (i, tok) in tokens.iter().enumerate() {
        if tok == "AND" || tok == "OR" || tok == "NOT" {
            if i + 1 == tokens.len() || tokens[i + 1] == ")" {
                return false;
            }
        }
        if tok == "(" || tok == ")" {
            continue;
        }
        if tok.ends_with('^') {
            return false;
        }
        if let Some((field, _)) = tok.split_once(':') {
            if (field.contains('*') || field.contains('?'))
                && field != "*"
                && !q.contains("\\*:")
                && !q.contains("\\?:")
            {
                return false;
            }
        }
        let colons = tok.chars().filter(|&c| c == ':').count();
        if colons > 1 && !tok.starts_with('_') {
            return false;
        }
    }
    true
}

fn boost_of(bm: &Map<String, Value>) -> f64 {
    bm.get("boost").and_then(to_f64).unwrap_or(1.0)
}

fn field_type_name(idx: &Index, name: &str) -> String {
    mapping_field_def(idx, name)
        .and_then(|d| d.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string()
}

fn is_wildcard_searchable(idx: &Index, name: &str) -> bool {
    let t = field_type_name(idx, name);
    if t.is_empty() {
        return false;
    }
    !matches!(
        t.as_str(),
        "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
            | "scaled_float" | "unsigned_long" | "date" | "date_nanos" | "boolean" | "ip"
            | "object" | "nested"
    )
}

fn expand_field_patterns(idx: &Index, fields: &[String]) -> Vec<String> {
    if fields.is_empty() {
        return Vec::new();
    }
    let mut all: Vec<String> = Vec::new();
    fn walk(props: &Map<String, Value>, prefix: &str, out: &mut Vec<String>) {
        for (k, def) in props {
            let full = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{}.{}", prefix, k)
            };
            if !out.contains(&full) {
                out.push(full.clone());
            }
            if let Some(defm) = def.as_object() {
                if let Some(sub) = defm.get("properties").and_then(|p| p.as_object()) {
                    walk(sub, &full, out);
                }
                if let Some(subs) = defm.get("fields").and_then(|p| p.as_object()) {
                    for (sk, _) in subs {
                        let sf = format!("{}.{}", full, sk);
                        if !out.contains(&sf) {
                            out.push(sf);
                        }
                    }
                }
            }
        }
    }
    if let Some(props) = idx.mappings.get("properties").and_then(|p| p.as_object()) {
        walk(props, "", &mut all);
    }
    for dv in idx.doc_values.values() {
        for k in dv.keys() {
            if !all.contains(k) {
                all.push(k.clone());
            }
        }
    }
    all.sort();
    let wants_wild = fields.iter().any(|f| f.contains('*') || f.contains('?'));
    if wants_wild {
        all.retain(|f| is_wildcard_searchable(idx, f));
    }
    let mut out = Vec::new();
    for f in fields {
        let (name, boost) = strip_field_boost(f);
        if name.contains('*') || name.contains('?') {
            let re = wildcard_regex(&name);
            for path in &all {
                if re.is_match(path) {
                    let item = if (boost - 1.0).abs() > f64::EPSILON {
                        format!("{}^{}", path, boost)
                    } else {
                        path.clone()
                    };
                    if !out.contains(&item) {
                        out.push(item);
                    }
                }
            }
        } else if !out.contains(f) {
            out.push(f.clone());
        }
    }
    out
}

fn parse_phrase_token(tok: &str) -> (String, f64) {
    let body = tok.trim_start_matches('"');
    if let Some(end) = body.find('"') {
        let phrase = body[..end].to_string();
        let rest = &body[end + 1..];
        let slop = rest
            .strip_prefix('~')
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        (phrase, slop)
    } else {
        (body.to_string(), 0.0)
    }
}

fn tokenize_query_string(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if ch == b' ' || ch == b'\t' || ch == b'\n' {
            i += 1;
            continue;
        }
        if ch == b'(' || ch == b')' {
            out.push((ch as char).to_string());
            i += 1;
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut in_quote = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'"' {
                in_quote = !in_quote;
                buf.push(c);
                i += 1;
                continue;
            }
            if !in_quote && (c == b' ' || c == b'\t' || c == b'\n' || c == b'(' || c == b')') {
                break;
            }
            if !in_quote && c == b'\\' && i + 1 < bytes.len() {
                buf.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            buf.push(c);
            i += 1;
        }
        if !buf.is_empty() {
            out.push(String::from_utf8_lossy(&buf).to_string());
        }
    }
    out
}

struct QsParser<'a, 'b> {
    tokens: Vec<String>,
    pos: usize,
    default_: String,
    fields: Vec<String>,
    operator: String,
    phrase_slop: f64,
    fuzzy_transpositions: bool,
    lenient: bool,
    raw_query: String,
    c: &'a QueryCtx<'b>,
}

impl<'a, 'b> QsParser<'a, 'b> {
    fn parse(&mut self) -> Result<Map<String, Value>, ApiError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Map<String, Value>, ApiError> {
        let mut left = self.parse_and()?;
        while self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            if tok == "OR" || tok == "||" {
                self.pos += 1;
                let right = self.parse_and()?;
                left = bool_query("should", vec![Value::Object(left), Value::Object(right)], 0);
                continue;
            }
            break;
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Map<String, Value>, ApiError> {
        let mut left = self.parse_unary()?;
        while self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            if tok == "AND" || tok == "&&" {
                self.pos += 1;
                let right = self.parse_unary()?;
                left = bool_query("must", vec![Value::Object(left), Value::Object(right)], 0);
                continue;
            }
            if tok == "OR" || tok == "||" || tok == ")" {
                break;
            }
            if tok == "NOT" {
                self.pos += 1;
                let right = self.parse_unary()?;
                left = bool_query(
                    "must",
                    vec![Value::Object(left), Value::Object(not_query(right))],
                    0,
                );
                continue;
            }
            let right = self.parse_unary()?;
            if self.operator == "AND" {
                left = bool_query("must", vec![Value::Object(left), Value::Object(right)], 0);
            } else {
                left = bool_query("should", vec![Value::Object(left), Value::Object(right)], 0);
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Map<String, Value>, ApiError> {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            if tok == "NOT" || tok == "!" {
                self.pos += 1;
                let inner = self.parse_unary()?;
                return Ok(not_query(inner));
            }
            if tok == "-" {
                self.pos += 1;
                let inner = self.parse_unary()?;
                return Ok(not_query(inner));
            }
            if tok == "+" {
                self.pos += 1;
                return self.parse_unary();
            }
            if tok.len() > 1 && (tok.starts_with('-') || tok.starts_with('+') || tok.starts_with('!')) {
                let sign = tok.as_bytes()[0] as char;
                self.tokens[self.pos] = tok[1..].to_string();
                let inner = self.parse_unary()?;
                if sign == '-' || sign == '!' {
                    return Ok(not_query(inner));
                }
                return Ok(inner);
            }
        }
        self.parse_term()
    }

    fn group_boost(&mut self, q: Map<String, Value>) -> Map<String, Value> {
        if self.pos < self.tokens.len() {
            let tok = self.tokens[self.pos].clone();
            if let Some(rest) = tok.strip_prefix('^') {
                if let Ok(b) = rest.parse::<f64>() {
                    self.pos += 1;
                    let mut bm = Map::new();
                    bm.insert("must".to_string(), Value::Array(vec![Value::Object(q)]));
                    bm.insert("boost".to_string(), json!(b));
                    let mut out = Map::new();
                    out.insert("bool".to_string(), Value::Object(bm));
                    return out;
                }
            }
        }
        q
    }

    fn parse_term(&mut self) -> Result<Map<String, Value>, ApiError> {
        if self.pos >= self.tokens.len() {
            let mut m = Map::new();
            m.insert("match_all".to_string(), json!({}));
            return Ok(m);
        }
        let tok = self.tokens[self.pos].clone();
        self.pos += 1;
        if tok == "(" {
            let q = self.parse_or()?;
            if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                self.pos += 1;
            }
            return Ok(self.group_boost(q));
        }
        if let Some(field) = tok.strip_prefix("_exists_:") {
            let mut inner = Map::new();
            inner.insert("field".to_string(), Value::String(field.to_string()));
            let mut q = Map::new();
            q.insert("exists".to_string(), Value::Object(inner));
            return Ok(q);
        }
        let cond1 = match tok.find(':') {
            Some(i) if i > 0 && !tok.starts_with('_') => true,
            Some(_) => tok.starts_with('_') && tok.contains(':'),
            None => false,
        };
        if cond1 {
            let i = tok.find(':').unwrap_or(0);
            let field = tok[..i].to_string();
            let mut value = tok[i + 1..].to_string();
            if value.is_empty() && self.pos < self.tokens.len() {
                if self.tokens[self.pos] == "(" {
                    self.pos += 1;
                    let q = self.parse_or()?;
                    if self.pos < self.tokens.len() && self.tokens[self.pos] == ")" {
                        self.pos += 1;
                    }
                    return Ok(self.group_boost(rewrite_qs_fields(&q, &field)));
                }
            }
            if field == "*" || field.contains('*') || field.contains('?') {
                let expanded = expand_field_patterns(self.c.idx, &[field.clone()]);
                if expanded.is_empty() {
                    let mut q = Map::new();
                    q.insert("match_none".to_string(), json!({}));
                    return Ok(q);
                }
                let mut shoulds = Vec::new();
                for f in expanded {
                    let (name, _) = strip_field_boost(&f);
                    let ftype = mapping_field_def(self.c.idx, &name)
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if matches!(
                        ftype,
                        "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
                            | "scaled_float" | "unsigned_long" | "date" | "date_nanos" | "boolean"
                    ) {
                        continue;
                    }
                    if value.starts_with('"') {
                        let (phrase, slop) = parse_phrase_token(&value);
                        let mut inner = Map::new();
                        if slop > 0.0 {
                            let mut opt = Map::new();
                            opt.insert("query".to_string(), Value::String(phrase));
                            opt.insert("slop".to_string(), json!(slop as i64));
                            inner.insert(name, Value::Object(opt));
                        } else {
                            inner.insert(name, Value::String(phrase));
                        }
                        let mut q = Map::new();
                        q.insert("match_phrase".to_string(), Value::Object(inner));
                        shoulds.push(Value::Object(q));
                    } else {
                        shoulds.push(Value::Object(self.value_query(&name, &value)?));
                    }
                }
                return Ok(bool_query("should", shoulds, 0));
            }
            if value.starts_with('"') {
                let (phrase, mut slop) = parse_phrase_token(&value);
                if slop == 0.0 {
                    slop = self.phrase_slop;
                }
                let mut inner = Map::new();
                if slop > 0.0 {
                    let mut opt = Map::new();
                    opt.insert("query".to_string(), Value::String(phrase));
                    opt.insert("slop".to_string(), json!(slop as i64));
                    inner.insert(field, Value::Object(opt));
                } else {
                    inner.insert(field, Value::String(phrase));
                }
                let mut q = Map::new();
                q.insert("match_phrase".to_string(), Value::Object(inner));
                return Ok(q);
            }
            return self.value_query(&field, &value);
        }
        if !self.fields.is_empty() {
            let fields = self.fields.clone();
            let mut shoulds = Vec::new();
            for f in fields {
                let (name, _) = strip_field_boost(&f);
                let ftype = mapping_field_def(self.c.idx, &name)
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if matches!(
                    ftype,
                    "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
                        | "scaled_float" | "unsigned_long"
                ) && tok.parse::<f64>().is_err()
                {
                    if self.lenient {
                        continue;
                    }
                    return Err(numeric_field_error(&self.raw_query, &tok));
                }
                let q = self.value_query(&name, &tok)?;
                shoulds.push(Value::Object(q));
            }
            return Ok(bool_query("should", shoulds, 0));
        }
        if tok.starts_with('"') {
            let (phrase, mut slop) = parse_phrase_token(&tok);
            if slop == 0.0 {
                slop = self.phrase_slop;
            }
            let val = if slop > 0.0 {
                let mut opt = Map::new();
                opt.insert("query".to_string(), Value::String(phrase));
                opt.insert("slop".to_string(), json!(slop as i64));
                Value::Object(opt)
            } else {
                Value::String(phrase)
            };
            if !self.fields.is_empty() {
                let mut shoulds = Vec::new();
                for f in self.fields.clone() {
                    let (name, _) = strip_field_boost(&f);
                    let mut inner = Map::new();
                    inner.insert(name, val.clone());
                    let mut q = Map::new();
                    q.insert("match_phrase".to_string(), Value::Object(inner));
                    shoulds.push(Value::Object(q));
                }
                return Ok(bool_query("should", shoulds, 0));
            }
            let mut inner = Map::new();
            inner.insert(self.default_.clone(), val);
            let mut q = Map::new();
            q.insert("match_phrase".to_string(), Value::Object(inner));
            return Ok(q);
        }
        self.value_query(&self.default_.clone(), &tok)
    }

    fn value_query(&mut self, field: &str, value: &str) -> Result<Map<String, Value>, ApiError> {
        if value == "*" {
            let mut inner = Map::new();
            inner.insert("field".to_string(), Value::String(field.to_string()));
            let mut q = Map::new();
            q.insert("exists".to_string(), Value::Object(inner));
            return Ok(q);
        }
        let mut value = value.to_string();
        let mut boost: Option<f64> = None;
        if let Some(ci) = value.rfind('^') {
            if ci > 0 {
                if let Ok(b) = value[ci + 1..].parse::<f64>() {
                    boost = Some(b);
                    value.truncate(ci);
                }
            }
        }
        if value.len() >= 2 && value.starts_with('/') && value.ends_with('/') {
            let pattern = value[1..value.len() - 1].to_string();
            if regex::Regex::new(&format!("^(?:{})$", pattern)).is_err() {
                let mut q = Map::new();
                q.insert("match_none".to_string(), json!({}));
                return Ok(q);
            }
            let mut outer = Map::new();
            outer.insert(field.to_string(), Value::String(pattern));
            let mut q = Map::new();
            q.insert("regexp".to_string(), Value::Object(outer));
            return Ok(q);
        }
        let mut fuzz: Option<String> = None;
        if let Some(ti) = value.rfind('~') {
            if ti > 0 {
                let suffix = value[ti + 1..].to_string();
                if suffix.is_empty() {
                    fuzz = Some("AUTO".to_string());
                    value.truncate(ti);
                } else if suffix.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    fuzz = Some(suffix);
                    value.truncate(ti);
                }
            }
        }
        if let Some(f) = fuzz {
            let mut opt = Map::new();
            opt.insert("query".to_string(), Value::String(value));
            let fnum = if f == "AUTO" {
                json!("AUTO")
            } else {
                json!(f.parse::<f64>().unwrap_or(1.0).trunc() as i64)
            };
            opt.insert("fuzziness".to_string(), fnum);
            opt.insert(
                "fuzzy_transpositions".to_string(),
                json!(self.fuzzy_transpositions),
            );
            let mut inner = Map::new();
            inner.insert(field.to_string(), Value::Object(opt));
            let mut q = Map::new();
            q.insert("match".to_string(), Value::Object(inner));
            return Ok(q);
        }
        if value.starts_with('>') || value.starts_with('<') {
            let (op, rest): (&str, String) = if let Some(r) = value.strip_prefix(">=") {
                ("gte", r.to_string())
            } else if let Some(r) = value.strip_prefix("<=") {
                ("lte", r.to_string())
            } else if let Some(r) = value.strip_prefix('>') {
                ("gt", r.to_string())
            } else {
                ("lt", value[1..].to_string())
            };
            let mut cond = Map::new();
            cond.insert(op.to_string(), lint_value(&rest));
            let mut inner = Map::new();
            inner.insert(field.to_string(), Value::Object(cond));
            let mut q = Map::new();
            q.insert("range".to_string(), Value::Object(inner));
            return Ok(q);
        }
        if value.starts_with('[') || value.starts_with('{') {
            let inclusive_low = value.starts_with('[');
            let mut raw = value.clone();
            while !(raw.ends_with(']') || raw.ends_with('}')) && self.pos < self.tokens.len() {
                raw.push(' ');
                raw.push_str(&self.tokens[self.pos]);
                self.pos += 1;
            }
            let inclusive_high = raw.ends_with(']');
            let body = raw
                .trim_start_matches(['[', '{'])
                .trim_end_matches([']', '}'])
                .to_string();
            let upper = body.to_uppercase();
            let lo_to = match upper.find(" TO ") {
                Some(i) => i,
                None => return Err(err_parsing("failed to parse range in query string")),
            };
            let lo = body[..lo_to].trim().to_string();
            let hi = body[lo_to + 4..].trim().to_string();
            let mut cond = Map::new();
            if lo != "*" && !lo.is_empty() {
                if inclusive_low {
                    cond.insert("gte".to_string(), lint_value(&lo));
                } else {
                    cond.insert("gt".to_string(), lint_value(&lo));
                }
            }
            if hi != "*" && !hi.is_empty() {
                if inclusive_high {
                    cond.insert("lte".to_string(), lint_value(&hi));
                } else {
                    cond.insert("lt".to_string(), lint_value(&hi));
                }
            }
            let mut inner = Map::new();
            inner.insert(field.to_string(), Value::Object(cond));
            let mut q = Map::new();
            q.insert("range".to_string(), Value::Object(inner));
            return Ok(q);
        }
        if value.contains('*') || value.contains('?') {
            let mut outer = Map::new();
            outer.insert(field.to_string(), Value::String(value.to_lowercase()));
            let mut q = Map::new();
            q.insert("wildcard".to_string(), Value::Object(outer));
            return Ok(q);
        }
        let def = mapping_field_def(self.c.idx, field);
        let ftype = def
            .and_then(|d| d.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let is_texty = def.is_none()
            || ftype == "text"
            || ftype == "match_only_text"
            || ftype == "search_as_you_type"
            || ftype.is_empty();
        if matches!(ftype.as_str(), "keyword" | "constant_keyword")
            && def
                .and_then(|d| d.get("split_queries_on_whitespace"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            && value.split_whitespace().count() > 1
        {
            let toks: Vec<&str> = value.split_whitespace().collect();
            let mut clauses = Vec::new();
            for t in &toks {
                let mut inner = Map::new();
                inner.insert(field.to_string(), lint_value(t));
                let mut q = Map::new();
                q.insert("term".to_string(), Value::Object(inner));
                clauses.push(Value::Object(q));
            }
            let key = if self.operator == "AND" { "must" } else { "should" };
            let mut b = Map::new();
            b.insert(key.to_string(), Value::Array(clauses));
            let mut q = Map::new();
            q.insert("bool".to_string(), Value::Object(b));
            return Ok(q);
        }
        if is_texty {
            let mut inner = Map::new();
            if let Some(b) = boost {
                let mut opt = Map::new();
                opt.insert("query".to_string(), Value::String(value.to_string()));
                opt.insert("boost".to_string(), json!(b));
                inner.insert(field.to_string(), Value::Object(opt));
            } else {
                inner.insert(field.to_string(), Value::String(value.to_string()));
            }
            let mut q = Map::new();
            q.insert("match".to_string(), Value::Object(inner));
            return Ok(q);
        }
        if matches!(
            ftype.as_str(),
            "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
                | "scaled_float" | "unsigned_long"
        ) && value.parse::<f64>().is_err()
        {
            let mut q = Map::new();
            q.insert("match_none".to_string(), json!({}));
            return Ok(q);
        }
        let mut inner = Map::new();
        inner.insert(field.to_string(), lint_value(&value));
        let mut q = Map::new();
        q.insert("term".to_string(), Value::Object(inner));
        Ok(q)
    }
}

fn not_query(inner: Map<String, Value>) -> Map<String, Value> {
    let mut bm = Map::new();
    bm.insert("must_not".to_string(), Value::Array(vec![Value::Object(inner)]));
    let mut q = Map::new();
    q.insert("bool".to_string(), Value::Object(bm));
    q
}

fn bool_query(kind: &str, clauses: Vec<Value>, msm: i64) -> Map<String, Value> {
    let mut bm = Map::new();
    bm.insert(kind.to_string(), Value::Array(clauses));
    if msm > 0 {
        bm.insert("minimum_should_match".to_string(), json!(msm));
    }
    let mut q = Map::new();
    q.insert("bool".to_string(), Value::Object(bm));
    q
}

fn rewrite_qs_fields(q: &Map<String, Value>, field: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in q {
        match k.as_str() {
            "match" | "match_phrase" | "term" | "wildcard" | "prefix" => {
                let mut inner = Map::new();
                if let Some(vm) = v.as_object() {
                    if let Some((_, val)) = single_entry(vm) {
                        inner.insert(field.to_string(), val.clone());
                    }
                }
                out.insert(k.clone(), Value::Object(inner));
            }
            "bool" => {
                let mut bm = Map::new();
                if let Some(vm) = v.as_object() {
                    for (bk, bv) in vm {
                        match bk.as_str() {
                            "must" | "should" | "must_not" | "filter" => {
                                let mut list = Vec::new();
                                for e in as_query_list(Some(bv)) {
                                    if let Some(em) = e.as_object() {
                                        list.push(Value::Object(rewrite_qs_fields(em, field)));
                                    } else {
                                        list.push(e);
                                    }
                                }
                                bm.insert(bk.clone(), Value::Array(list));
                            }
                            _ => {
                                bm.insert(bk.clone(), bv.clone());
                            }
                        }
                    }
                }
                out.insert("bool".to_string(), Value::Object(bm));
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

fn multi_field_query(kind: &str, fields: &[String], value: &str) -> Map<String, Value> {
    let mut shoulds = Vec::new();
    for f in fields {
        let (name, _) = strip_field_boost(f);
        let mut inner = Map::new();
        inner.insert(name, Value::String(value.to_string()));
        let mut q = Map::new();
        q.insert(kind.to_string(), Value::Object(inner));
        shoulds.push(Value::Object(q));
    }
    bool_query("should", shoulds, 0)
}

fn lint_value(v: &str) -> Value {
    if let Ok(n) = v.parse::<i64>() {
        return Value::Number(Number::from(n));
    }
    if let Ok(f) = v.parse::<f64>() {
        if let Some(n) = Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    Value::String(v.to_string())
}

fn expand_multifield(q: &Map<String, Value>, c: &QueryCtx) -> Map<String, Value> {
    let mut q = q.clone();
    for (k, v) in q.clone() {
        match k.as_str() {
            "match" | "match_phrase" => {
                let bm = v.as_object().cloned().unwrap_or_default();
                let mut field = String::new();
                let mut val = Value::Null;
                for (f, vv) in &bm {
                    field = f.clone();
                    val = vv.clone();
                }
                if field == "*" {
                    let mut all: Vec<String> = c
                        .values_map()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    all.retain(|f| is_wildcard_searchable(c.idx, f));
                    all.sort();
                    let kind = if k == "match_phrase" {
                        "match_phrase"
                    } else {
                        "match"
                    };
                    let mut shoulds = Vec::new();
                    for f in all {
                        let mut inner = Map::new();
                        inner.insert(f, val.clone());
                        let mut o = Map::new();
                        o.insert(kind.to_string(), Value::Object(inner));
                        shoulds.push(Value::Object(o));
                    }
                    return bool_query("should", shoulds, 0);
                }
            }
            "bool" => {
                let bm = v.as_object().cloned().unwrap_or_default();
                let mut new_bm = bm.clone();
                for key in ["must", "should", "must_not", "filter"] {
                    if let Some(list) = bm.get(key).and_then(|l| l.as_array()) {
                        let mut out = Vec::new();
                        for e in list {
                            if let Some(em) = e.as_object() {
                                out.push(Value::Object(expand_multifield(em, c)));
                            } else {
                                out.push(e.clone());
                            }
                        }
                        new_bm.insert(key.to_string(), Value::Array(out));
                    }
                }
                q.insert(k.clone(), Value::Object(new_bm));
            }
            _ => {}
        }
    }
    q
}
