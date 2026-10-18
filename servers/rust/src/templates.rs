use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::errors::{err_response, json_response, method_not_allowed, ApiError};
use crate::resp::glob_match;
use crate::shared::{
    flatten_settings, is_known_index_setting, validate_setting_value,
};
use crate::store::{settings_value_int, settings_value_string, Store};
use crate::Req;

/// Result of composing matching index templates for a concrete index name.
/// Mirrors Go's composedTemplate. `ds_def` is Some when the winning template
/// declares a `data_stream` block.
pub struct ComposedTemplate {
    pub template: String,
    pub settings: Map<String, Value>,
    pub mappings: Value,
    pub aliases: Map<String, Value>,
    pub ds_def: Option<Value>,
    pub overlapping: Vec<Value>,
}

impl ComposedTemplate {
    pub fn empty() -> Self {
        ComposedTemplate {
            template: String::new(),
            settings: Map::new(),
            mappings: Value::Object(Map::new()),
            aliases: Map::new(),
            ds_def: None,
            overlapping: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------- helpers

/// Converts a raw template "settings" object into canonical flat form
/// (keys prefixed with "index.", values stringified).
pub fn template_settings_canonical(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(m) = raw.as_object() {
        let mut flat = Map::new();
        flatten_settings(m, &mut flat);
        for (k, v) in flat {
            out.insert(k, Value::String(settings_value_string(&v)));
        }
    }
    out
}

/// Renders settings for GET responses: {"index": {...}}.
pub fn template_settings_view(raw: &Value) -> Value {
    match raw.as_object() {
        Some(m) if !m.is_empty() => {
            let canon = template_settings_canonical(raw);
            let mut inner = Map::new();
            for (k, v) in canon {
                inner.insert(
                    k.strip_prefix("index.").unwrap_or(&k).to_string(),
                    Value::String(settings_value_string(&v)),
                );
            }
            json!({"index": inner})
        }
        _ => json!({}),
    }
}

pub fn normalize_patterns(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    }
}

fn pattern_prefix(p: &str) -> &str {
    match p.find(|c| c == '*' || c == '?') {
        Some(i) => &p[..i],
        None => p,
    }
}

pub fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if !a.contains('*') && !a.contains('?') && !b.contains('*') && !b.contains('?') {
        return false;
    }
    let pa = pattern_prefix(a);
    let pb = pattern_prefix(b);
    if pa.is_empty() || pb.is_empty() {
        return true;
    }
    pa.starts_with(pb) || pb.starts_with(pa)
}

pub fn template_matches(tpl: &Map<String, Value>, name: &str) -> bool {
    for pat in normalize_patterns(tpl.get("index_patterns")) {
        if let Some(ps) = pat.as_str() {
            if glob_match(ps, name) {
                return true;
            }
        }
    }
    false
}

pub fn priority_of(raw: &Map<String, Value>) -> i64 {
    raw.get("priority")
        .and_then(settings_value_int)
        .unwrap_or(0)
}

fn sorted_template_names(m: &std::collections::HashMap<String, Map<String, Value>>) -> Vec<String> {
    let mut names: Vec<String> = m.keys().cloned().collect();
    names.sort();
    names
}

// ------------------------------------------------------- template composing

/// Deep-merge mappings (used by create_index_internal and simulate).
pub fn merge_mappings_deep(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        if k == "properties" {
            let sv = match v.as_object() {
                Some(s) => s,
                None => {
                    dst.insert(k.clone(), v.clone());
                    continue;
                }
            };
            if !dst.get("properties").map(|p| p.is_object()).unwrap_or(false) {
                dst.insert("properties".to_string(), Value::Object(Map::new()));
            }
            let dv = dst.get_mut("properties").unwrap().as_object_mut().unwrap();
            for (fk, fv) in sv {
                let merge = dv.get(fk).and_then(|d| d.as_object()).is_some()
                    && fv.as_object().is_some();
                if merge {
                    let dfv = dv.get_mut(fk).unwrap().as_object_mut().unwrap();
                    merge_mappings_deep(dfv, fv.as_object().unwrap());
                } else {
                    dv.insert(fk.clone(), fv.clone());
                }
            }
            continue;
        }
        dst.insert(k.clone(), v.clone());
    }
}

fn apply_template_block(out: &mut ComposedTemplate, tpl: &Map<String, Value>) {
    if let Some(st) = tpl.get("settings") {
        for (k, v) in template_settings_canonical(st) {
            out.settings.insert(k, v);
        }
    }
    if let Some(m) = tpl.get("mappings").and_then(|m| m.as_object()) {
        merge_mappings_deep(out.mappings.as_object_mut().unwrap(), m);
    }
    if let Some(a) = tpl.get("aliases").and_then(|a| a.as_object()) {
        for (k, v) in a {
            out.aliases.insert(k.clone(), v.clone());
        }
    }
}

/// Compose all matching templates for an index name.
pub fn compose_for_index(store: &Store, name: &str) -> ComposedTemplate {
    let mut out = ComposedTemplate::empty();
    let mut best_name = String::new();
    let mut best_priority = i32::MIN as i64;

    for n in sorted_template_names(&store.composable_templates) {
        let tpl = &store.composable_templates[&n];
        if !template_matches(tpl, name) {
            continue;
        }
        let p = priority_of(tpl);
        if p > best_priority || (p == best_priority && (best_name.is_empty() || n < best_name)) {
            if !best_name.is_empty() {
                let prev = &store.composable_templates[&best_name];
                out.overlapping.push(json!({
                    "name": best_name,
                    "index_patterns": normalize_patterns(prev.get("index_patterns")),
                }));
            }
            best_name = n;
            best_priority = p;
        } else {
            out.overlapping.push(json!({
                "name": n,
                "index_patterns": normalize_patterns(tpl.get("index_patterns")),
            }));
        }
    }

    if !best_name.is_empty() {
        let tpl = &store.composable_templates[&best_name];
        out.template = best_name.clone();
        if let Some(comps) = tpl.get("composed_of").and_then(|c| c.as_array()) {
            for c in comps {
                let cs = c.as_str().unwrap_or("");
                let comp = match store.component_templates.get(cs) {
                    Some(c) => c,
                    None => continue,
                };
                if let Some(ctpl) = comp.get("template").and_then(|t| t.as_object()) {
                    apply_template_block(&mut out, ctpl);
                }
            }
        }
        if let Some(tpl0) = tpl.get("template").and_then(|t| t.as_object()) {
            apply_template_block(&mut out, tpl0);
        }
        if let Some(ds) = tpl.get("data_stream") {
            out.ds_def = Some(ds.clone());
        }
        for n in sorted_template_names(&store.legacy_templates) {
            let ltpl = &store.legacy_templates[&n];
            if template_matches(ltpl, name) {
                out.overlapping.push(json!({
                    "name": n,
                    "index_patterns": normalize_patterns(ltpl.get("index_patterns")),
                }));
            }
        }
        out.overlapping
            .sort_by_key(|v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
        return out;
    }

    // legacy templates: merge by ascending order (then name)
    let mut names: Vec<String> = store
        .legacy_templates
        .iter()
        .filter(|(_, tpl)| template_matches(tpl, name))
        .map(|(n, _)| n.clone())
        .collect();
    for n in &names {
        out.overlapping.push(json!({
            "name": n,
            "index_patterns": normalize_patterns(store.legacy_templates[n].get("index_patterns")),
        }));
    }
    names.sort_by(|a, b| {
        let oa = store.legacy_templates[a]
            .get("order")
            .and_then(settings_value_int)
            .unwrap_or(0);
        let ob = store.legacy_templates[b]
            .get("order")
            .and_then(settings_value_int)
            .unwrap_or(0);
        oa.cmp(&ob).then_with(|| a.cmp(b))
    });
    out.overlapping
        .sort_by_key(|v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
    for n in names {
        let tpl = &store.legacy_templates[&n];
        if let Some(st) = tpl.get("settings") {
            for (k, v) in template_settings_canonical(st) {
                out.settings.insert(k, v);
            }
        }
        if let Some(m) = tpl.get("mappings").and_then(|m| m.as_object()) {
            merge_mappings_deep(out.mappings.as_object_mut().unwrap(), m);
        }
        if let Some(a) = tpl.get("aliases").and_then(|a| a.as_object()) {
            for (k, v) in a {
                let resolved = k.replace("{index}", name);
                out.aliases.insert(resolved, v.clone());
            }
        }
    }
    out
}

/// Other templates (composable + legacy) whose patterns overlap any given pattern.
pub fn overlapping_for(store: &Store, patterns: &[Value], exclude: &str) -> Vec<Value> {
    let mut seen: Vec<String> = vec![];
    let mut out: Vec<Value> = vec![];
    let overlap = |pats: &[Value]| -> bool {
        for p1 in patterns {
            let p1s = p1.as_str().unwrap_or("");
            for p2 in pats {
                let p2s = p2.as_str().unwrap_or("");
                if patterns_overlap(p1s, p2s) {
                    return true;
                }
            }
        }
        false
    };
    for (n, tpl) in store.composable_templates.iter() {
        if n == exclude || seen.contains(n) {
            continue;
        }
        let pats = normalize_patterns(tpl.get("index_patterns"));
        if overlap(&pats) {
            seen.push(n.clone());
            out.push(json!({"name": n, "index_patterns": pats}));
        }
    }
    for (n, tpl) in store.legacy_templates.iter() {
        if n == exclude || seen.contains(n) {
            continue;
        }
        let pats = normalize_patterns(tpl.get("index_patterns"));
        if overlap(&pats) {
            seen.push(n.clone());
            out.push(json!({"name": n, "index_patterns": pats}));
        }
    }
    out.sort_by_key(|v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
    out
}

// ---------------------------------------------------------- index templates

fn known_index_template_field(k: &str) -> bool {
    matches!(
        k,
        "index_patterns" | "template" | "composed_of" | "priority" | "version" | "_meta"
            | "data_stream"
    )
}

fn validate_pattern_list(patterns: &[Value]) -> Result<(), ApiError> {
    for p in patterns {
        let ps = p.as_str().unwrap_or("");
        if ps.is_empty() {
            return Err(ApiError::new(
                "invalid_index_template_exception",
                &format!(
                    "index_pattern [{}] must not be empty",
                    settings_value_string(p)
                ),
                400,
            ));
        }
        if ps.starts_with('_') || ps.starts_with('-') || ps.starts_with('+') {
            return Err(ApiError::new(
                "invalid_index_template_exception",
                &format!(
                    "index_pattern [{}] must not start with '_', '-', or '+'. Found your index pattern [{}]",
                    ps, ps
                ),
                400,
            ));
        }
    }
    Ok(())
}

fn index_template_get_view(raw: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert(
        "index_patterns".to_string(),
        Value::Array(normalize_patterns(raw.get("index_patterns"))),
    );
    let mut tv = Map::new();
    if let Some(tpl) = raw.get("template").and_then(|t| t.as_object()) {
        if let Some(s) = tpl.get("settings") {
            tv.insert("settings".to_string(), template_settings_view(s));
        }
        if let Some(m) = tpl.get("mappings") {
            if m.as_object().map(|m| !m.is_empty()).unwrap_or(true) && !m.is_null() {
                tv.insert("mappings".to_string(), m.clone());
            }
        }
        if let Some(a) = tpl.get("aliases") {
            if a.as_object().map(|a| !a.is_empty()).unwrap_or(false) {
                tv.insert("aliases".to_string(), a.clone());
            }
        }
    }
    if !tv.is_empty() {
        out.insert("template".to_string(), Value::Object(tv));
    }
    for k in ["composed_of", "priority", "version", "_meta", "data_stream"] {
        if let Some(v) = raw.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

fn validate_and_store_index_template(
    store: &mut Store,
    name: &str,
    body: &Map<String, Value>,
    create_only: bool,
) -> Result<(), ApiError> {
    if name != name.to_lowercase() {
        return Err(ApiError::new(
            "invalid_index_template_exception",
            &format!(
                "index_template [{}] invalid, cause [Validation Failed: 1: name must be lower cased;]",
                name
            ),
            400,
        ));
    }
    for k in body.keys() {
        if !known_index_template_field(k) {
            return Err(ApiError::new(
                "x_content_parse_exception",
                &format!("unknown field [{}]", k),
                400,
            ));
        }
    }
    let patterns = normalize_patterns(body.get("index_patterns"));
    if patterns.is_empty() {
        return Err(ApiError::new(
            "illegal_argument_exception",
            "Required [index_patterns]",
            400,
        ));
    }
    validate_pattern_list(&patterns)?;
    if let Some(p) = body.get("priority") {
        if !p.is_null() {
            match settings_value_int(p) {
                Some(n) if n >= 0 => {}
                _ => {
                    return Err(ApiError::new(
                        "action_request_validation_exception",
                        "Validation Failed: 1: index template priority must be >= 0;",
                        400,
                    ))
                }
            }
        }
    }
    if let Some(comps) = body.get("composed_of").and_then(|c| c.as_array()) {
        let mut missing = vec![];
        for c in comps {
            let cs = c.as_str().unwrap_or("");
            if !store.component_templates.contains_key(cs) {
                missing.push(cs.to_string());
            }
        }
        if !missing.is_empty() {
            return Err(ApiError::new(
                "invalid_index_template_exception",
                &format!(
                    "index template [{}] specifies component templates [{}] that do not exist",
                    name,
                    missing.join(", ")
                ),
                400,
            ));
        }
    }
    if let Some(tpl) = body.get("template").and_then(|t| t.as_object()) {
        if let Some(st) = tpl.get("settings").and_then(|s| s.as_object()) {
            let canon = template_settings_canonical(&Value::Object(st.clone()));
            for (k, v) in canon {
                if !is_known_index_setting(&k) {
                    return Err(ApiError::new(
                        "settings_exception",
                        &format!("unknown setting [{}] please check that any required plugins are installed, or check the breaking changes documentation for removed settings", k),
                        400,
                    ));
                }
                if let Err(e) = validate_setting_value(&k, &v, true) {
                    return Err(ApiError::new(
                        "invalid_index_template_exception",
                        &e.reason,
                        400,
                    ));
                }
            }
        }
    }
    if create_only && store.composable_templates.contains_key(name) {
        return Err(ApiError::new(
            "illegal_argument_exception",
            &format!("index template [{}] already exists", name),
            400,
        ));
    }
    let prio = if body.contains_key("priority") {
        priority_of(body)
    } else {
        0
    };
    for (other_name, other) in store.composable_templates.iter() {
        if other_name == name {
            continue;
        }
        if priority_of(other) != prio {
            continue;
        }
        let other_patterns = normalize_patterns(other.get("index_patterns"));
        for p1 in &patterns {
            let p1s = p1.as_str().unwrap_or("");
            for p2 in &other_patterns {
                let p2s = p2.as_str().unwrap_or("");
                if patterns_overlap(p1s, p2s) {
                    return Err(ApiError::new(
                        "illegal_argument_exception",
                        &format!(
                            "index template [{}] has index patterns [{}] matching patterns [{}] from existing templates [{}] that have the same priority [{}], multiple templates can be configured with different priority values or use it is recommended to use a greater priority",
                            name, p1s, p2s, other_name, prio
                        ),
                        400,
                    ));
                }
            }
        }
    }
    store
        .composable_templates
        .insert(name.to_string(), body.clone());
    Ok(())
}

pub fn handle_composable_template(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    let list_view = |store: &Store| -> Value {
        let mut out = vec![];
        let mut names: Vec<String> = store.composable_templates.keys().cloned().collect();
        names.sort();
        for n in names {
            out.push(json!({
                "name": n,
                "index_template": index_template_get_view(&store.composable_templates[&n]),
            }));
        }
        json!({ "index_templates": out })
    };
    if rest.is_empty() {
        if req.method != "GET" && req.method != "HEAD" {
            return method_not_allowed(req, &["GET", "HEAD"]);
        }
        return json_response(200, list_view(store));
    }
    let name = &rest[0];
    match req.method.as_str() {
        "PUT" | "POST" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            let create_only = req.bool_param("create");
            if let Err(e) = validate_and_store_index_template(store, name, &body, create_only) {
                return err_response(e);
            }
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "DELETE" => {
            if !store.composable_templates.contains_key(name) {
                return err_response(ApiError::new(
                    "index_template_missing_exception",
                    &format!("index_template [{}] missing", name),
                    404,
                ));
            }
            for ds in store.data_streams.values() {
                if ds.template == *name {
                    return err_response(ApiError::new(
                        "illegal_argument_exception",
                        &format!(
                            "index templates [{}] cannot be removed as they are in use by a data streams [{}]",
                            name, ds.name
                        ),
                        400,
                    ));
                }
            }
            store.composable_templates.remove(name);
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "GET" | "HEAD" => {
            let mut out = vec![];
            let mut names: Vec<String> = store.composable_templates.keys().cloned().collect();
            names.sort();
            for n in names {
                if n == *name || glob_match(name, &n) {
                    out.push(json!({
                        "name": n,
                        "index_template": index_template_get_view(&store.composable_templates[&n]),
                    }));
                }
            }
            if out.is_empty() {
                return err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("index template matching [{}] not found", name),
                    404,
                ));
            }
            json_response(200, json!({"index_templates": out}))
        }
        _ => method_not_allowed(req, &["GET", "PUT", "POST", "DELETE"]),
    }
}

// ------------------------------------------------------- component templates

pub fn handle_component_template(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    let view = |n: &str, raw: &Map<String, Value>| -> Value {
        let mut inner = Map::new();
        if let Some(tpl) = raw.get("template").and_then(|t| t.as_object()) {
            let mut tv = Map::new();
            if let Some(st) = tpl.get("settings") {
                if !st.is_null()
                    && st.as_object().map(|m| !m.is_empty()).unwrap_or(true)
                {
                    tv.insert("settings".to_string(), template_settings_view(st));
                }
            }
            if let Some(m) = tpl.get("mappings") {
                if !m.is_null() && m.as_object().map(|m| !m.is_empty()).unwrap_or(true) {
                    tv.insert("mappings".to_string(), m.clone());
                }
            }
            if let Some(a) = tpl.get("aliases") {
                if a.as_object().map(|a| !a.is_empty()).unwrap_or(false) {
                    tv.insert("aliases".to_string(), a.clone());
                }
            }
            inner.insert("template".to_string(), Value::Object(tv));
        }
        for k in ["version", "_meta"] {
            if let Some(v) = raw.get(k) {
                inner.insert(k.to_string(), v.clone());
            }
        }
        json!({"name": n, "component_template": inner})
    };
    if rest.is_empty() {
        if req.method != "GET" && req.method != "HEAD" {
            return method_not_allowed(req, &["GET", "HEAD"]);
        }
        let mut out = vec![];
        let mut names: Vec<String> = store.component_templates.keys().cloned().collect();
        names.sort();
        for n in names {
            out.push(view(&n, &store.component_templates[&n]));
        }
        return json_response(200, json!({"component_templates": out}));
    }
    let name = &rest[0];
    match req.method.as_str() {
        "PUT" | "POST" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            store.component_templates.insert(name.clone(), body);
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "DELETE" => {
            if !store.component_templates.contains_key(name) {
                return err_response(ApiError::new(
                    "index_template_missing_exception",
                    &format!("component template [{}] missing", name),
                    404,
                ));
            }
            for (other_name, tpl) in store.composable_templates.iter() {
                if let Some(comps) = tpl.get("composed_of").and_then(|c| c.as_array()) {
                    for c in comps {
                        if c.as_str() == Some(name.as_str()) {
                            return err_response(ApiError::new(
                                "illegal_argument_exception",
                                &format!(
                                    "component templates [{}] cannot be removed as they are still in use by index templates [{}]",
                                    name, other_name
                                ),
                                400,
                            ));
                        }
                    }
                }
            }
            store.component_templates.remove(name);
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "GET" | "HEAD" => {
            let mut out = vec![];
            let mut names: Vec<String> = store.component_templates.keys().cloned().collect();
            names.sort();
            for n in names {
                if n == *name || glob_match(name, &n) {
                    out.push(view(&n, &store.component_templates[&n]));
                }
            }
            if out.is_empty() {
                return err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("component template matching [{}] not found", name),
                    404,
                ));
            }
            json_response(200, json!({"component_templates": out}))
        }
        _ => method_not_allowed(req, &["GET", "PUT", "POST", "DELETE"]),
    }
}

// --------------------------------------------------------- legacy templates

fn legacy_get_view(tpl: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    let order = tpl.get("order").and_then(settings_value_int).unwrap_or(0);
    out.insert("order".to_string(), json!(order));
    out.insert(
        "index_patterns".to_string(),
        Value::Array(normalize_patterns(tpl.get("index_patterns"))),
    );
    out.insert(
        "settings".to_string(),
        match tpl.get("settings") {
            Some(s) if !s.is_null() => template_settings_view(s),
            _ => json!({}),
        },
    );
    out.insert(
        "mappings".to_string(),
        match tpl.get("mappings") {
            Some(m) if !m.is_null() => m.clone(),
            _ => json!({}),
        },
    );
    out.insert(
        "aliases".to_string(),
        match tpl.get("aliases") {
            Some(a) if !a.is_null() => a.clone(),
            _ => json!({}),
        },
    );
    if let Some(v) = tpl.get("version") {
        out.insert("version".to_string(), v.clone());
    }
    Value::Object(out)
}

pub fn handle_legacy_template(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if rest.is_empty() {
        if req.method != "GET" && req.method != "HEAD" {
            return method_not_allowed(req, &["GET", "HEAD"]);
        }
        let mut out = Map::new();
        let mut names: Vec<String> = store.legacy_templates.keys().cloned().collect();
        names.sort();
        for n in names {
            out.insert(n.clone(), legacy_get_view(&store.legacy_templates[&n]));
        }
        return json_response(200, Value::Object(out));
    }
    let name = &rest[0];
    match req.method.as_str() {
        "PUT" | "POST" => {
            let body = match req.json_body() {
                Ok(b) => b,
                Err(e) => return err_response(e),
            };
            if normalize_patterns(body.get("index_patterns")).is_empty() {
                return err_response(ApiError::new(
                    "action_request_validation_exception",
                    "Validation Failed: 1: index patterns are missing;",
                    400,
                ));
            }
            if req.bool_param("create") && store.legacy_templates.contains_key(name) {
                return err_response(ApiError::new(
                    "illegal_argument_exception",
                    &format!("index_template [{}] already exists", name),
                    400,
                ));
            }
            store.legacy_templates.insert(name.clone(), body);
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "DELETE" => {
            if !store.legacy_templates.contains_key(name) {
                return err_response(ApiError::new(
                    "index_template_missing_exception",
                    &format!("index_template [{}] missing", name),
                    404,
                ));
            }
            store.legacy_templates.remove(name);
            store.bump_state();
            json_response(200, json!({"acknowledged": true}))
        }
        "GET" | "HEAD" => {
            let mut out = Map::new();
            let mut names: Vec<String> = store.legacy_templates.keys().cloned().collect();
            names.sort();
            for n in names {
                if n == *name || glob_match(name, &n) {
                    out.insert(n.clone(), legacy_get_view(&store.legacy_templates[&n]));
                }
            }
            if out.is_empty() {
                return json_response(404, json!({}));
            }
            json_response(200, Value::Object(out))
        }
        _ => method_not_allowed(req, &["GET", "PUT", "POST", "DELETE"]),
    }
}

// ------------------------------------------------------- simulate endpoint

fn simulate_template(store: &Store, name: &str, body: Option<&Map<String, Value>>) -> Response {
    let mut composed = ComposedTemplate::empty();
    if let Some(body) = body {
        let patterns = normalize_patterns(body.get("index_patterns"));
        if let Some(tpl) = body.get("template").and_then(|t| t.as_object()) {
            apply_template_block(&mut composed, tpl);
        }
        composed.overlapping = overlapping_for(store, &patterns, "");
    } else if !name.is_empty() {
        let tpl = match store.composable_templates.get(name) {
            Some(t) => t,
            None => {
                return err_response(ApiError::new(
                    "resource_not_found_exception",
                    &format!("index template matching [{}] not found", name),
                    404,
                ))
            }
        };
        composed.overlapping =
            overlapping_for(store, &normalize_patterns(tpl.get("index_patterns")), name);
        if let Some(comps) = tpl.get("composed_of").and_then(|c| c.as_array()) {
            for c in comps {
                let cs = c.as_str().unwrap_or("");
                let comp = match store.component_templates.get(cs) {
                    Some(c) => c,
                    None => continue,
                };
                if let Some(ctpl) = comp.get("template").and_then(|t| t.as_object()) {
                    apply_template_block(&mut composed, ctpl);
                }
            }
        }
        if let Some(tpl0) = tpl.get("template").and_then(|t| t.as_object()) {
            apply_template_block(&mut composed, tpl0);
        }
    }
    composed
        .overlapping
        .sort_by_key(|v| v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string());
    let mut inner = Map::new();
    for (k, v) in &composed.settings {
        inner.insert(
            k.strip_prefix("index.").unwrap_or(k).to_string(),
            Value::String(settings_value_string(v)),
        );
    }
    let mut tpl_view = Map::new();
    tpl_view.insert("aliases".to_string(), Value::Object(composed.aliases.clone()));
    if !composed.settings.is_empty() {
        tpl_view.insert("settings".to_string(), json!({"index": inner}));
    }
    if composed
        .mappings
        .as_object()
        .map(|m| !m.is_empty())
        .unwrap_or(false)
    {
        tpl_view.insert("mappings".to_string(), composed.mappings.clone());
    }
    json_response(
        200,
        json!({"template": tpl_view, "overlapping": composed.overlapping}),
    )
}

pub fn handle_index_template_simulate(store: &mut Store, req: &Req, rest: &[String]) -> Response {
    if req.method != "POST" {
        return method_not_allowed(req, &["POST"]);
    }
    if rest.len() >= 2 && rest[0] == "_simulate_index" {
        let composed = compose_for_index(store, &rest[1]);
        return render_simulate(&composed);
    }
    if !rest.is_empty() && rest[0] == "_simulate" {
        if rest.len() >= 2 {
            return simulate_template(store, &rest[1], None);
        }
        let body = match req.json_body() {
            Ok(b) => b,
            Err(e) => return err_response(e),
        };
        return simulate_template(store, "", Some(&body));
    }
    simulate_template(store, "", None)
}

fn render_simulate(composed: &ComposedTemplate) -> Response {
    let mut overlapping = composed.overlapping.clone();
    overlapping.sort_by_key(|v| {
        v.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    });
    let mut inner = Map::new();
    for (k, v) in &composed.settings {
        inner.insert(
            k.strip_prefix("index.").unwrap_or(k).to_string(),
            Value::String(settings_value_string(v)),
        );
    }
    let mut tpl_view = Map::new();
    tpl_view.insert("aliases".to_string(), Value::Object(composed.aliases.clone()));
    if !composed.settings.is_empty() {
        tpl_view.insert("settings".to_string(), json!({"index": inner}));
    }
    if composed
        .mappings
        .as_object()
        .map(|m| !m.is_empty())
        .unwrap_or(false)
    {
        tpl_view.insert("mappings".to_string(), composed.mappings.clone());
    }
    json_response(200, json!({"template": tpl_view, "overlapping": overlapping}))
}
