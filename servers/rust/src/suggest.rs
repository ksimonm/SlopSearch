use crate::store::*;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

fn collect_field_terms(idx: &Index, field: &str) -> (HashMap<String, usize>, Vec<String>) {
    let mut freq: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = vec![];
    for id in &idx.order {
        let doc = match idx.docs.get(id) { Some(d) => d, None => continue };
        if let Some(v) = field_value(doc, field) {
            for s in doc_strings(v) {
                for tok in analyze(&s) {
                    if !freq.contains_key(&tok) {
                        order.push(tok.clone());
                    }
                    *freq.entry(tok).or_insert(0) += 1;
                }
            }
        }
    }
    (freq, order)
}

fn suggest_text(spec: &Map<String, Value>) -> String {
    if let Some(v) = spec.get("text") {
        return value_to_string(v).unwrap_or_default();
    }
    if let Some(v) = spec.get("prefix") {
        return value_to_string(v).unwrap_or_default();
    }
    String::new()
}

pub fn eval_suggest(suggest: &Map<String, Value>, idx: &Index) -> Value {
    let mut out = Map::new();
    for (name, raw) in suggest {
        let spec = match raw.as_object() {
            Some(s) => s,
            None => continue,
        };
        if let Some(t) = spec.get("term").and_then(|v| v.as_object()) {
            out.insert(name.clone(), json!([term_suggester(spec, t, idx)]));
        } else if let Some(t) = spec.get("phrase").and_then(|v| v.as_object()) {
            out.insert(name.clone(), json!([phrase_suggester(spec, t, idx)]));
        } else if let Some(t) = spec.get("completion").and_then(|v| v.as_object()) {
            out.insert(name.clone(), json!([completion_suggester(spec, t, idx)]));
        }
    }
    Value::Object(out)
}

fn term_suggester(spec: &Map<String, Value>, body: &Map<String, Value>, idx: &Index) -> Value {
    let field = body.get("field").and_then(value_to_string).unwrap_or_default();
    let input = suggest_text(spec);
    let (freq, order) = collect_field_terms(idx, &field);

    let mut options: Vec<Value> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for tok in analyze(&input) {
        let fuzz = auto_fuzziness(&tok);
        let mut cands: Vec<(String, f64)> = vec![];
        for c in &order {
            let d = levenshtein(&tok, c);
            if d == 0 || d > fuzz {
                continue;
            }
            let ml = tok.chars().count().max(c.chars().count()) as f64;
            cands.push((c.clone(), 1.0 - d as f64 / ml));
        }
        cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (text, score) in cands {
            if !seen.insert(text.clone()) {
                continue;
            }
            options.push(json!({
                "text": text,
                "score": score,
                "freq": freq.get(&text).copied().unwrap_or(0),
            }));
        }
    }
    json!({"text": input, "offset": 0, "length": input.len(), "options": options})
}

fn phrase_suggester(spec: &Map<String, Value>, body: &Map<String, Value>, idx: &Index) -> Value {
    let field = body.get("field").and_then(value_to_string).unwrap_or_default();
    let input = suggest_text(spec);
    let (_, order) = collect_field_terms(idx, &field);

    let tokens = analyze(&input);
    let mut corrected: Vec<String> = vec![];
    let mut changed = false;
    for tok in &tokens {
        let mut best = tok.clone();
        let mut best_dist = usize::MAX;
        for c in &order {
            let d = levenshtein(tok, c);
            if d == 0 {
                best = tok.clone();
                break;
            }
            if d <= 2 && d < best_dist {
                best_dist = d;
                best = c.clone();
            }
        }
        if &best != tok {
            changed = true;
        }
        corrected.push(best);
    }

    let mut options: Vec<Value> = vec![];
    if changed {
        options.push(json!({"text": corrected.join(" "), "score": 0.9}));
    }
    json!({"text": input, "offset": 0, "length": input.len(), "options": options})
}

fn completion_inputs(v: &Value) -> Vec<(String, f64)> {
    let mut out = vec![];
    match v {
        Value::String(s) => out.push((s.clone(), 0.0)),
        Value::Object(m) => {
            let w = m.get("weight").and_then(to_f64).unwrap_or(0.0);
            if let Some(inp) = m.get("input") {
                match inp {
                    Value::String(s) => out.push((s.clone(), w)),
                    Value::Array(a) => {
                        for e in a {
                            if let Some(s) = value_to_string(e) {
                                out.push((s, w));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Array(a) => {
            for e in a {
                match e {
                    Value::String(s) => out.push((s.clone(), 0.0)),
                    Value::Object(m) => {
                        let w = m.get("weight").and_then(to_f64).unwrap_or(0.0);
                        if let Some(inp) = m.get("input") {
                            match inp {
                                Value::String(s) => out.push((s.clone(), w)),
                                Value::Array(ia) => {
                                    for x in ia {
                                        if let Some(s) = value_to_string(x) {
                                            out.push((s, w));
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

fn completion_suggester(spec: &Map<String, Value>, body: &Map<String, Value>, idx: &Index) -> Value {
    let field = body.get("field").and_then(value_to_string).unwrap_or_default();
    let prefix = suggest_text(spec);
    let lower = prefix.to_lowercase();

    let mut entries: Vec<(String, f64, String)> = vec![];
    for id in &idx.order {
        let doc = match idx.docs.get(id) { Some(d) => d, None => continue };
        if let Some(v) = field_value(doc, &field) {
            for (text, weight) in completion_inputs(v) {
                if text.to_lowercase().starts_with(&lower) {
                    entries.push((text, weight, id.clone()));
                }
            }
        }
    }
    entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let options: Vec<Value> = entries
        .iter()
        .map(|(text, weight, id)| json!({"text": text, "_score": weight, "_id": id}))
        .collect();
    json!({"text": prefix, "offset": 0, "length": prefix.len(), "options": options})
}
