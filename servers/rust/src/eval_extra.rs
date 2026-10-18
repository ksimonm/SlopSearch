// Extra query evaluators: multi_match, nested/join, geo, span, intervals,
// query_string parser, function_score, more_like_this, knn.
// Port of servers/go/eval_extra.go.

use crate::eval::*;
use crate::fields::{doc_values_for, preview_value, value_scalar_string};
use crate::painless::{script_float, ScriptEnv};
use crate::store::*;
use serde_json::{json, Map, Number, Value};

fn err_parsing(reason: &str) -> ApiError {
    ApiError::parsing(reason)
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
                        if re.is_match(path) && !array_values(vals).is_empty() {
                            expanded.push(path.clone());
                        }
                    }
                }
                continue;
            }
            expanded.push(f.clone());
        }
        let mut best = 0.0;
        let mut sum = 0.0;
        let mut matched_any = false;
        match typ.as_str() {
            "phrase" | "phrase_prefix" => {
                for f in &expanded {
                    let (name, fb) = strip_field_boost(f);
                    let ok = if typ == "phrase" {
                        self.eval_match_phrase(&name, &Value::String(query.clone()), boost * fb, 0.0)?
                            .0
                    } else {
                        let mut inner = Map::new();
                        inner.insert("query".to_string(), Value::String(query.clone()));
                        inner.insert("boost".to_string(), json!(boost * fb));
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
                let raw = if let Some(op) = bm.get("default_operator") {
                    json!({"query": query.clone(), "operator": op})
                } else {
                    Value::String(query.clone())
                };
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
        let fields = fields_of(bm.get("fields"));
        if fields.is_empty() {
            return Err(err_parsing("[combined_fields] requires 'fields'"));
        }
        let operator = bm
            .get("operator")
            .and_then(|o| o.as_str())
            .unwrap_or("")
            .to_string();
        let mut best = 0.0;
        for f in &fields {
            let mut inner = Map::new();
            inner.insert("query".to_string(), Value::String(query.clone()));
            inner.insert("operator".to_string(), Value::String(operator.clone()));
            let (ok, sc) = self.eval_match(f, &Value::Object(inner), 1.0)?;
            if ok && sc > best {
                best = sc;
            }
        }
        if best > 0.0 {
            return Ok((true, best));
        }
        Ok((false, 0.0))
    }

    // -----------------------------------------------------------------------
    // nested / join
    // -----------------------------------------------------------------------

    pub fn eval_nested(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("[nested] query malformed")),
        };
        let inner = match bm.get("query").and_then(|q| q.as_object()) {
            Some(q) => q,
            None => return Err(err_parsing("[nested] requires 'query'")),
        };
        let mut score_mode = bm
            .get("score_mode")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if score_mode.is_empty() {
            score_mode = "avg".to_string();
        }
        let (matched, sc) = self.eval(inner)?;
        if !matched {
            return Ok((false, 0.0));
        }
        if score_mode == "none" {
            return Ok((true, 0.0));
        }
        Ok((true, sc))
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
        let field = match find_join_field(self.idx) {
            Some(f) => f,
            None => {
                return Err(err_parsing(
                    "failed to create query: join field is not configured in mappings",
                ))
            }
        };
        let mut best = 0.0;
        let mut found = false;
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
                found = true;
                if sc > best {
                    best = sc;
                }
            }
        }
        if found {
            if bm.contains_key("score_mode") {
                return Ok((true, best));
            }
            return Ok((true, 0.0));
        }
        Ok((false, 0.0))
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
        let field = match find_join_field(self.idx) {
            Some(f) => f,
            None => {
                return Err(err_parsing(
                    "failed to create query: join field is not configured in mappings",
                ))
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
        let (ok, _) = cc.eval(inner)?;
        if ok {
            return Ok((true, 1.0));
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
        let (plat, plon) = match geo_point_of(&point) {
            Some(p) => p,
            None => return Err(err_parsing("failed to parse geo_distance point")),
        };
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
        let mut top = 0.0;
        let mut left = 0.0;
        let mut bottom = 0.0;
        let mut right = 0.0;
        let mut have = false;
        if let Some(tl) = spec.get("top_left").and_then(|t| t.as_object()) {
            top = tl.get("lat").and_then(to_f64).unwrap_or(0.0);
            left = tl.get("lon").and_then(to_f64).unwrap_or(0.0);
            have = true;
        }
        if let Some(br) = spec.get("bottom_right").and_then(|t| t.as_object()) {
            bottom = br.get("lat").and_then(to_f64).unwrap_or(0.0);
            right = br.get("lon").and_then(to_f64).unwrap_or(0.0);
            have = true;
        }
        if !have {
            top = spec.get("top").and_then(to_f64).unwrap_or(0.0);
            left = spec.get("left").and_then(to_f64).unwrap_or(0.0);
            bottom = spec.get("bottom").and_then(to_f64).unwrap_or(0.0);
            right = spec.get("right").and_then(to_f64).unwrap_or(0.0);
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            if let Some((lat, lon)) = geo_point_of(v) {
                if lat <= top && lat >= bottom && lon >= left && lon <= right {
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
        let qs = match parse_geo_shape(&shape_raw) {
            Some(s) => s,
            None => return Err(err_parsing("failed to parse geo_shape")),
        };
        let mut relation = bm
            .get("relation")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if relation.is_empty() {
            relation = "INTERSECTS".to_string();
        }
        let boost = bm.get("boost").and_then(to_f64).unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        for v in &vals {
            let mut lat = 0.0;
            let mut lon = 0.0;
            let mut valid = false;
            if let Some(ds) = parse_geo_shape(v) {
                match ds.typ.as_str() {
                    "point" => {
                        lat = ds.lat;
                        lon = ds.lon;
                        valid = true;
                    }
                    "envelope" | "polygon" | "circle" => {
                        for p in shape_points(&ds) {
                            if shape_intersects(&qs, p.0, p.1) {
                                let rel = relation.to_uppercase();
                                if rel != "DISJOINT" && rel != "WITHIN" {
                                    return Ok((true, boost));
                                }
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            } else if let Some((la, lo)) = geo_point_of(v) {
                lat = la;
                lon = lo;
                valid = true;
            }
            if !valid {
                continue;
            }
            match relation.to_uppercase().as_str() {
                "DISJOINT" => {
                    if !shape_intersects(&qs, lat, lon) {
                        return Ok((true, boost));
                    }
                }
                _ => {
                    if shape_intersects(&qs, lat, lon) {
                        return Ok((true, boost));
                    }
                }
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
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        let f = match vals.first().and_then(to_f64) {
            Some(f) => f,
            None => return Ok((false, 0.0)),
        };
        let mut sc = f;
        if let Some(sat) = bm.get("saturation").and_then(|s| s.as_object()) {
            let pivot = sat.get("pivot").and_then(to_f64).unwrap_or(1.0);
            sc = f / (f + pivot);
        } else if let Some(lg) = bm.get("log").and_then(|s| s.as_object()) {
            sc = (1.0 + f).ln();
            if let Some(s) = lg.get("scaling_factor").and_then(to_f64) {
                sc *= s;
            }
        } else if let Some(ln) = bm.get("linear").and_then(|s| s.as_object()) {
            sc = f;
            if let Some(s) = ln.get("scaling_factor").and_then(to_f64) {
                sc *= s;
            }
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
        let pivot = bm
            .get("pivot")
            .and_then(parse_distance_meters)
            .unwrap_or(1.0);
        let (vals, present) = self.values(&field);
        if !present {
            return Ok((false, 0.0));
        }
        let v = match vals.first().and_then(to_f64) {
            Some(v) => v,
            None => return Ok((false, 0.0)),
        };
        let def = mapping_field_def(self.idx, &field)
            .cloned()
            .unwrap_or_default();
        let ftype = def
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
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
        let mut origin = 0.0;
        if let Ok(o) = parse_query_date(&def, bm.get("origin").unwrap_or(&Value::Null), "") {
            origin = to_f64(&o).unwrap_or(0.0);
        }
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
        let doc_raw = match bm.get("document").and_then(|d| d.as_object()) {
            Some(d) => d,
            None => {
                if bm.contains_key("id") {
                    return Err(err_parsing("percolate by id is not supported"));
                }
                return Err(err_parsing("[percolate] requires 'document'"));
            }
        };
        let mut field = bm
            .get("field")
            .and_then(|f| f.as_str())
            .unwrap_or("")
            .to_string();
        if field.is_empty() {
            field = "query".to_string();
        }
        let vals = match doc_values_for(self.idx, "_percolate", doc_raw) {
            Ok(v) => v.0,
            Err(_) => return Ok((true, 0.0)),
        };
        let doc_value = Value::Object(doc_raw.clone());
        let input_qc = QueryCtx::with_values(
            self.idx,
            "_percolate",
            &doc_value,
            btree_to_map(vals),
        );
        let mut score = 0.0;
        for id in &self.idx.order {
            let doc = match self.idx.docs.get(id) {
                Some(d) => d,
                None => continue,
            };
            let stored = match doc.as_object().and_then(|d| d.get(&field)) {
                Some(s) => s,
                None => continue,
            };
            if *id == self.id {
                continue;
            }
            let qm = match stored.as_object() {
                Some(q) => q,
                None => continue,
            };
            let (matched, sc) = match input_qc.eval(qm) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if matched {
                if sc > score {
                    score = sc;
                }
                if *id == self.id {
                    return Ok((true, sc));
                }
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
        let mut terms: Vec<Value> = Vec::new();
        for (k, v) in bm {
            if k == "boost" || k == "_name" {
                continue;
            }
            field = k.clone();
            terms = v.as_array().cloned().unwrap_or_default();
            break;
        }
        if field.is_empty() {
            return Err(err_parsing("[terms_set] requires a field"));
        }
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
        if let Some(name) = bm.get("minimum_should_match_field").and_then(|n| n.as_str()) {
            let (req_vals, _) = self.values(name);
            if let Some(v) = req_vals.first() {
                required = numeric_int(v);
            }
        }
        if let Some(script_spec) = bm
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
        let (field, term) = span_field_term(body)?;
        if self.span_match(&field, &term)? {
            return Ok((true, 1.0));
        }
        Ok((false, 0.0))
    }

    pub fn eval_span_near(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_near malformed")),
        };
        let clauses = bm
            .get("clauses")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let in_order = bm.get("in_order").and_then(|i| i.as_bool()).unwrap_or(false);
        if clauses.is_empty() {
            return Ok((false, 0.0));
        }
        let mut phrase_parts: Vec<String> = Vec::new();
        let mut field = String::new();
        for cl in &clauses {
            let (f, term) = span_field_term(cl)?;
            if field.is_empty() {
                field = f;
            }
            phrase_parts.push(term);
        }
        for v in self.must_values(&field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            let tokens = analyze(&s);
            if contains_subsequence(&tokens, &phrase_parts) {
                return Ok((true, 1.0));
            }
            if !in_order {
                let mut all_present = true;
                for p in &phrase_parts {
                    let mut found = false;
                    for t in &tokens {
                        if t == p {
                            found = true;
                        }
                    }
                    if !found {
                        all_present = false;
                    }
                }
                if all_present {
                    return Ok((true, 1.0));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_span_or(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_or malformed")),
        };
        let clauses = bm
            .get("clauses")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        for cl in &clauses {
            if let Some(m) = cl.as_object() {
                if self.eval(m)?.0 {
                    return Ok((true, 1.0));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_span_first(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_first malformed")),
        };
        let match_raw = bm.get("match").cloned().unwrap_or(Value::Null);
        let end = bm.get("end").and_then(to_f64).unwrap_or(1.0) as usize;
        let (field, term) = span_field_term(&match_raw)?;
        for v in self.must_values(&field) {
            let s = value_scalar_string(&v).unwrap_or_default();
            let tokens = analyze(&s);
            for i in 0..end.min(tokens.len()) {
                if tokens[i] == term {
                    return Ok((true, 1.0));
                }
            }
        }
        Ok((false, 0.0))
    }

    pub fn eval_span_not(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_not malformed")),
        };
        let include = bm.get("include").and_then(|i| i.as_object());
        let exclude = bm.get("exclude").and_then(|i| i.as_object());
        let ok1 = match include {
            Some(i) => self.eval(i)?.0,
            None => false,
        };
        if !ok1 {
            return Ok((false, 0.0));
        }
        if let Some(e) = exclude {
            if self.eval(e)?.0 {
                return Ok((false, 0.0));
            }
        }
        Ok((true, 1.0))
    }

    pub fn eval_span_containing(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_containing malformed")),
        };
        let big = bm.get("big").and_then(|b| b.as_object());
        let little = bm.get("little").and_then(|l| l.as_object());
        let ok1 = match big {
            Some(b) => self.eval(b)?.0,
            None => false,
        };
        if !ok1 {
            return Ok((false, 0.0));
        }
        let ok2 = match little {
            Some(l) => self.eval(l)?.0,
            None => false,
        };
        if !ok2 {
            return Ok((false, 0.0));
        }
        Ok((true, 1.0))
    }

    pub fn eval_span_within(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_within malformed")),
        };
        let big = bm.get("big").and_then(|b| b.as_object());
        let little = bm.get("little").and_then(|l| l.as_object());
        let ok1 = match little {
            Some(l) => self.eval(l)?.0,
            None => false,
        };
        if !ok1 {
            return Ok((false, 0.0));
        }
        let ok2 = match big {
            Some(b) => self.eval(b)?.0,
            None => false,
        };
        if !ok2 {
            return Ok((false, 0.0));
        }
        Ok((true, 1.0))
    }

    pub fn eval_span_multi(&self, body: &Value) -> Result<(bool, f64), ApiError> {
        let bm = match body.as_object() {
            Some(m) => m,
            None => return Err(err_parsing("span_multi malformed")),
        };
        let match_raw = match bm.get("match").and_then(|m| m.as_object()) {
            Some(m) => m,
            None => return Err(err_parsing("span_multi requires match")),
        };
        self.eval(match_raw)
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
        let mut all_fields = Map::new();
        all_fields.insert(field, rules);
        self.eval_intervals_rule(&all_fields)
    }

    fn eval_intervals_rule(&self, rule: &Map<String, Value>) -> Result<(bool, f64), ApiError> {
        for (kind, body) in rule {
            let bm = body.as_object().cloned().unwrap_or_default();
            match kind.as_str() {
                "match" => {
                    let q = bm
                        .get("query")
                        .and_then(|q| q.as_str())
                        .unwrap_or("")
                        .to_string();
                    if q.is_empty() {
                        return Ok((false, 0.0));
                    }
                    let patterns: Vec<String> =
                        q.split_whitespace().map(|s| s.to_string()).collect();
                    if patterns.len() > 1 {
                        let mut phrase = Map::new();
                        phrase.insert(
                            "terms".to_string(),
                            Value::Array(
                                patterns.iter().map(|p| Value::String(p.clone())).collect(),
                            ),
                        );
                        let mut r = Map::new();
                        r.insert("phrase".to_string(), Value::Object(phrase));
                        return self.eval_intervals_rule(&r);
                    }
                    return self.eval_term_single("", &bm);
                }
                "prefix" => {
                    let q = bm
                        .get("prefix")
                        .and_then(|q| q.as_str())
                        .unwrap_or("")
                        .to_string();
                    let ok = self.eval_intervals_simple(&move |_f: &str, tokens: &[String]| {
                        tokens.iter().any(|t| t.starts_with(&q))
                    });
                    return Ok((ok, 1.0));
                }
                "wildcard" => {
                    let q = bm
                        .get("pattern")
                        .and_then(|q| q.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let re = wildcard_regex(&q);
                    let ok = self.eval_intervals_simple(&move |_f: &str, tokens: &[String]| {
                        tokens.iter().any(|t| re.is_match(t))
                    });
                    return Ok((ok, 1.0));
                }
                "fuzzy" => {
                    let q = bm
                        .get("value")
                        .and_then(|q| q.as_str())
                        .unwrap_or("")
                        .to_string();
                    let fuzz = auto_fuzziness(&q);
                    let ok = self.eval_intervals_simple(&move |_f: &str, tokens: &[String]| {
                        tokens.iter().any(|t| levenshtein(t, &q) <= fuzz)
                    });
                    return Ok((ok, 1.0));
                }
                "all_of" => {
                    let subs = bm
                        .get("intervals")
                        .and_then(|i| i.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for sub in &subs {
                        if let Some(sm) = sub.as_object() {
                            if !self.eval_intervals_rule(sm)?.0 {
                                return Ok((false, 0.0));
                            }
                        }
                    }
                    return Ok((true, 1.0));
                }
                "any_of" => {
                    let subs = bm
                        .get("intervals")
                        .and_then(|i| i.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for sub in &subs {
                        if let Some(sm) = sub.as_object() {
                            if self.eval_intervals_rule(sm)?.0 {
                                return Ok((true, 1.0));
                            }
                        }
                    }
                    return Ok((false, 0.0));
                }
                "phrase" => {
                    let terms = bm
                        .get("terms")
                        .and_then(|t| t.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let parts: Vec<String> =
                        terms.iter().filter_map(value_to_string).collect();
                    let ok = self.eval_intervals_simple(&move |_f: &str, tokens: &[String]| {
                        contains_subsequence(tokens, &parts)
                    });
                    return Ok((ok, 1.0));
                }
                "containing" => {
                    let big = bm.get("big").and_then(|b| b.as_object());
                    if let Some(b) = big {
                        let ok = self.eval_intervals_rule(b)?.0;
                        return Ok((ok, 1.0));
                    }
                    return Ok((false, 0.0));
                }
                "not" => {
                    let inner = bm.get("intervals").and_then(|i| i.as_object());
                    if let Some(i) = inner {
                        let ok = self.eval_intervals_rule(i)?.0;
                        if ok {
                            return Ok((false, 0.0));
                        }
                        return Ok((true, 1.0));
                    }
                    return Ok((false, 0.0));
                }
                _ => return Err(err_parsing(&format!("unknown intervals rule [{}]", kind))),
            }
        }
        Ok((false, 0.0))
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
        let fields = fields_of(bm.get("fields"));
        let mut texts: Vec<String> = Vec::new();
        match bm.get("like") {
            Some(Value::String(s)) => texts.push(s.clone()),
            Some(Value::Object(m)) => texts.extend(collect_doc_texts(m, &[])),
            Some(Value::Array(a)) => {
                for e in a {
                    match e {
                        Value::String(v) => texts.push(v.clone()),
                        Value::Object(v) => {
                            if let Some(doc) = v.get("doc").and_then(|d| d.as_object()) {
                                texts.extend(collect_doc_texts(doc, &fields));
                            } else if let Some(id) = v.get("_id").and_then(value_to_string) {
                                if let Some(sd) = self.idx.docs.get(&id) {
                                    if let Some(sdo) = sd.as_object() {
                                        texts.extend(collect_doc_texts(sdo, &fields));
                                    }
                                }
                            } else {
                                texts.extend(collect_doc_texts(v, &fields));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let mut terms: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for t in &texts {
            for tok in analyze(t) {
                *terms.entry(tok).or_insert(0) += 1;
            }
        }
        if terms.is_empty() {
            return Ok((false, 0.0));
        }
        let mut targets = fields.clone();
        if targets.is_empty() {
            if let Some(m) = self.values_map() {
                targets = m.keys().cloned().collect();
            }
        }
        let min_match = match bm.get("minimum_should_match") {
            Some(msm) => minimum_should_match(msm, terms.len(), 0),
            None => 1,
        };
        let mut score = 0.0;
        for f in &targets {
            for v in self.must_values(f) {
                let s = match value_scalar_string(&v) {
                    Some(s) => s,
                    None => continue,
                };
                let doc_terms: std::collections::BTreeSet<String> =
                    analyze(&s).into_iter().collect();
                let matched = terms.keys().filter(|t| doc_terms.contains(*t)).count();
                if matched >= min_match {
                    score += matched as f64;
                }
            }
        }
        if score > 0.0 {
            return Ok((true, score));
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
        let mut final_score = 0.0;
        let mut first = true;
        for fn_raw in &functions {
            let fno = match fn_raw.as_object() {
                Some(f) => f,
                None => continue,
            };
            let mut fv = self.apply_score_function(fno);
            if let Some(b) = fno.get("weight").and_then(to_f64) {
                fv *= b;
            }
            match score_mode.as_str() {
                "first" | "" => {
                    if first {
                        final_score = fv;
                    }
                }
                "max" => {
                    if first || fv > final_score {
                        final_score = fv;
                    }
                }
                "min" => {
                    if first || fv < final_score {
                        final_score = fv;
                    }
                }
                "sum" => final_score += fv,
                "avg" => final_score += fv,
                "replace" => final_score = fv,
                _ => {
                    if first {
                        final_score = fv;
                    } else {
                        final_score *= fv;
                    }
                }
            }
            first = false;
        }
        if functions.is_empty() {
            final_score = 1.0;
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

    fn apply_score_function(&self, fno: &Map<String, Value>) -> f64 {
        if let Some(v) = fno.get("weight").and_then(to_f64) {
            return v;
        }
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
            let mut val;
            let (vals, ok) = self.values(&field);
            if ok && !vals.is_empty() {
                val = vals.first().and_then(to_f64).unwrap_or(0.0);
            } else if let Some(mv) = fvf.get("missing").and_then(to_f64) {
                val = mv;
            } else {
                val = 0.0;
            }
            val *= factor;
            match modifier.as_str() {
                "log1p" => val = val.ln_1p(),
                "log2p" => val = (1.0 + val).log2(),
                "ln" => val = val.ln(),
                "ln1p" => val = val.ln_1p(),
                "ln2p" => val = (1.0 + val).ln() / 2f64.ln(),
                "square" => val = val * val,
                "sqrt" => val = val.sqrt(),
                "reciprocal" => val = 1.0 / val,
                _ => {}
            }
            return val;
        }
        if let Some(rs) = fno.get("random_score").and_then(|r| r.as_object()) {
            let mut seed = rs.get("seed").and_then(integer_of).unwrap_or(0);
            if seed == 0 {
                seed = 42;
            }
            return pseudo_random(&self.id, seed);
        }
        if let Some(ss) = fno.get("script_score").and_then(|s| s.as_object()) {
            let sm = ss.get("script").and_then(|s| s.as_object());
            let src = sm
                .and_then(|s| s.get("source"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let env = ScriptEnv {
                c: self,
                params: crate::painless::script_params_of(sm.unwrap_or(&Map::new())),
            };
            if let Ok(v) = env.eval_script_expression(src) {
                return script_float(&v);
            }
            return 1.0;
        }
        for fn_name in ["gauss", "linear", "exp"] {
            if let Some(dec) = fno.get(fn_name).and_then(|d| d.as_object()) {
                let field = dec
                    .get("field")
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .to_string();
                let (vals, _) = self.values(&field);
                if vals.is_empty() {
                    return 1.0;
                }
                let val = vals.first().and_then(to_f64).unwrap_or(0.0);
                let origin = dec.get("origin").and_then(to_f64).unwrap_or(0.0);
                let scale = dec.get("scale").and_then(to_f64).unwrap_or(1.0);
                let offset = dec.get("offset").and_then(to_f64).unwrap_or(0.0);
                let decay_v = dec.get("decay").and_then(to_f64).unwrap_or(0.5);
                let dist = (val - origin).abs();
                if dist <= offset {
                    return 1.0;
                }
                let x = (dist - offset) / scale;
                match fn_name {
                    "gauss" => {
                        return (-0.5 * x * x * decay_v.ln() / 0.5f64.ln() * -1.0).exp()
                    }
                    "exp" => return decay_v.powf(x),
                    "linear" => {
                        if 1.0 - x < 0.0 {
                            return 0.0;
                        }
                        return 1.0 - x;
                    }
                    _ => {}
                }
            }
        }
        1.0
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
        let mut default_field = bm
            .get("default_field")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if default_field.is_empty() {
            default_field = "*".to_string();
        }
        let fields = fields_of(bm.get("fields"));
        let mut p = QsParser {
            tokens: tokenize_query_string(&query),
            pos: 0,
            default_: default_field.clone(),
            fields: fields.clone(),
            operator,
            c: self,
        };
        let mut q = p.parse()?;
        if default_field == "*" && fields.is_empty() {
            q = expand_multifield(&q, self);
        }
        self.eval(&q)
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

fn find_join_field(idx: &Index) -> Option<String> {
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

fn join_parts(v: Option<&Value>) -> (String, String) {
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
            let lat = m.get("lat").and_then(to_f64)?;
            let lon = m.get("lon").and_then(to_f64)?;
            Some((lat, lon))
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
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() == 2 {
                let lat: f64 = parts[0].trim().parse().ok()?;
                let lon: f64 = parts[1].trim().parse().ok()?;
                return Some((lat, lon));
            }
            None
        }
        _ => None,
    }
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
struct GeoShape {
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
    let m = v.as_object()?;
    let typ = m
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
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
        _ => None,
    }
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
    if src.contains("Math.min") {
        return Some(count);
    }
    if src.contains("Math.max") {
        let mut lower = 1usize;
        if let Some(i) = src.find("Math.max(") {
            let rest = &src[i + "Math.max(".len()..];
            if let Some(j) = rest.find(',') {
                if let Ok(n) = rest[..j].trim().parse::<usize>() {
                    lower = n;
                }
            }
        }
        return Some(lower);
    }
    None
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
        if ch == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            let end = (j + 1).min(bytes.len());
            out.push(String::from_utf8_lossy(&bytes[i..end]).to_string());
            i = j + 1;
            continue;
        }
        if ch == b'(' || ch == b')' {
            out.push((ch as char).to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j] != b' ' && bytes[j] != b'\t' && bytes[j] != b'\n' {
            j += 1;
        }
        out.push(String::from_utf8_lossy(&bytes[i..j]).to_string());
        i = j;
    }
    out
}

struct QsParser<'a, 'b> {
    tokens: Vec<String>,
    pos: usize,
    default_: String,
    fields: Vec<String>,
    operator: String,
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
            if tok.eq_ignore_ascii_case("OR") || tok == "||" {
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
            if tok.eq_ignore_ascii_case("AND") || tok == "&&" {
                self.pos += 1;
                let right = self.parse_unary()?;
                left = bool_query("must", vec![Value::Object(left), Value::Object(right)], 0);
                continue;
            }
            if tok.eq_ignore_ascii_case("OR")
                || tok == "||"
                || tok == ")"
                || tok.eq_ignore_ascii_case("NOT")
            {
                break;
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
            if tok == "-" || (tok.starts_with('-') && tok.len() > 1 && !tok.contains(':')) {
                if tok == "-" {
                    self.pos += 1;
                    let inner = self.parse_unary()?;
                    return Ok(not_query(inner));
                }
                self.tokens[self.pos] = tok[1..].to_string();
                let inner = self.parse_unary()?;
                return Ok(not_query(inner));
            }
        }
        self.parse_term()
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
                    return Ok(rewrite_qs_fields(&q, &field));
                }
            }
            if value.starts_with('"') {
                if !value.ends_with('"') && self.pos < self.tokens.len() {
                    while self.pos < self.tokens.len() && !value.ends_with('"') {
                        value.push(' ');
                        value.push_str(&self.tokens[self.pos]);
                        self.pos += 1;
                    }
                }
                let phrase = value.trim_matches('"');
                let mut inner = Map::new();
                inner.insert(field, Value::String(phrase.to_string()));
                let mut q = Map::new();
                q.insert("match_phrase".to_string(), Value::Object(inner));
                return Ok(q);
            }
            return self.value_query(&field, &value);
        }
        if let Some(field) = tok.strip_prefix("_exists_:") {
            let mut inner = Map::new();
            inner.insert("field".to_string(), Value::String(field.to_string()));
            let mut q = Map::new();
            q.insert("exists".to_string(), Value::Object(inner));
            return Ok(q);
        }
        if tok.starts_with('"') {
            let phrase = tok.trim_matches('"');
            if !self.fields.is_empty() {
                return Ok(multi_field_query(
                    "match_phrase",
                    &self.fields.clone(),
                    phrase,
                ));
            }
            let mut inner = Map::new();
            inner.insert(self.default_.clone(), Value::String(phrase.to_string()));
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
            let mut body = value
                .trim_start_matches(['[', '{'])
                .trim_end_matches([']', '}'])
                .to_string();
            if !body.contains("TO") && self.pos < self.tokens.len() {
                while self.pos < self.tokens.len() && !body.contains("TO") {
                    body.push(' ');
                    body.push_str(&self.tokens[self.pos]);
                    self.pos += 1;
                }
                body = body
                    .trim_start_matches(['[', '{'])
                    .trim_end_matches([']', '}'])
                    .to_string();
            }
            let lo_to = match body.find(" TO ") {
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
                if value.ends_with(']') {
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
            let mut inner = Map::new();
            inner.insert(
                "value".to_string(),
                Value::String(value.to_lowercase()),
            );
            let mut outer = Map::new();
            outer.insert(field.to_string(), Value::Object(inner));
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
        if is_texty {
            let mut inner = Map::new();
            inner.insert(field.to_string(), Value::String(value.to_string()));
            let mut q = Map::new();
            q.insert("match".to_string(), Value::Object(inner));
            return Ok(q);
        }
        let mut inner = Map::new();
        inner.insert(field.to_string(), lint_value(value));
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
