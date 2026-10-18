use crate::errors::ApiError;
use crate::eval::{mapping_field_def, single_entry, wildcard_regex};
use crate::store::{analyze, field_value, simple_lower, value_to_string, Index};
use serde_json::{json, Map, Value};

#[derive(Clone)]
struct Cfg {
    typ: String,
    pre: Option<Vec<String>>,
    post: Option<Vec<String>>,
    schema: Option<String>,
    fragment_size: Option<usize>,
    nfrags: Option<usize>,
    order_score: bool,
    boundary: Option<String>,
    encoder_html: bool,
    require_field_match: Option<bool>,
    no_match_size: Option<usize>,
    highlight_query: Option<Value>,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg {
            typ: "unified".to_string(),
            pre: None,
            post: None,
            schema: None,
            fragment_size: None,
            nfrags: None,
            order_score: false,
            boundary: None,
            encoder_html: false,
            require_field_match: None,
            no_match_size: None,
            highlight_query: None,
        }
    }
}

fn xcontent_error(reason: &str) -> ApiError {
    let mut e = ApiError::new("x_content_parse_exception", reason, 400);
    e.root_cause = Some(Box::new(ApiError::new("parsing_exception", reason, 400)));
    e.caused_by = Some(Box::new(ApiError::new("parsing_exception", reason, 400)));
    e
}

fn search_error(reason: &str) -> ApiError {
    let mut e = ApiError::new(
        "search_phase_execution_exception",
        "all shards failed",
        400,
    );
    let cause = ApiError::new("illegal_argument_exception", reason, 400);
    e.root_cause = Some(Box::new(cause.clone()));
    e.caused_by = Some(Box::new(cause));
    e
}

fn apply_options(base: &Cfg, opts: &Map<String, Value>) -> Result<Cfg, ApiError> {
    let mut c = base.clone();
    if let Some(v) = opts.get("type").and_then(|v| v.as_str()) {
        c.typ = v.to_string();
    }
    if let Some(v) = opts.get("tags_schema").and_then(|v| v.as_str()) {
        c.schema = Some(v.to_string());
    }
    if let Some(arr) = opts.get("pre_tags").and_then(|v| v.as_array()) {
        c.pre = Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        );
    }
    if let Some(arr) = opts.get("post_tags").and_then(|v| v.as_array()) {
        c.post = Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        );
        if c.pre.as_ref().map(|p| p.is_empty()).unwrap_or(true) {
            c.pre = Some(vec!["<em>".to_string()]);
        }
    }
    if let Some(v) = opts.get("fragment_size").and_then(|v| v.as_u64()) {
        c.fragment_size = Some(v as usize);
    }
    if let Some(v) = opts.get("number_of_fragments").and_then(|v| v.as_u64()) {
        c.nfrags = Some(v as usize);
    }
    if let Some(v) = opts.get("order").and_then(|v| v.as_str()) {
        c.order_score = v == "score";
    }
    if let Some(v) = opts.get("boundary_scanner").and_then(|v| v.as_str()) {
        c.boundary = Some(v.to_string());
    }
    if let Some(v) = opts.get("encoder").and_then(|v| v.as_str()) {
        c.encoder_html = v == "html";
    }
    if let Some(v) = opts.get("require_field_match").and_then(|v| v.as_bool()) {
        c.require_field_match = Some(v);
    }
    if let Some(v) = opts.get("no_match_size").and_then(|v| v.as_u64()) {
        c.no_match_size = Some(v as usize);
    }
    if let Some(v) = opts.get("highlight_query") {
        c.highlight_query = Some(v.clone());
    }
    Ok(c)
}

fn parse_global(hl: &Value) -> Result<Cfg, ApiError> {
    let obj = hl.as_object().cloned().unwrap_or_default();
    if obj.contains_key("max_analyzed_offset") {
        return Err(xcontent_error(
            "[highlight] unknown field [max_analyzed_offset] did you mean [max_analyzer_offset]?",
        ));
    }
    let mut c = Cfg::default();
    c = apply_options(&c, &obj)?;
    if c.pre.is_some() && c.post.is_none() {
        return Err(xcontent_error("pre_tags are set but post_tags are not set"));
    }
    Ok(c)
}

fn tags(c: &Cfg) -> (String, String) {
    if let (Some(pre), Some(post)) = (&c.pre, &c.post) {
        return (
            pre.first().cloned().unwrap_or_else(|| "<em>".to_string()),
            post.first().cloned().unwrap_or_else(|| "</em>".to_string()),
        );
    }
    if c.schema.as_deref() == Some("styled") {
        return (
            "<em class=\"hlt1\">".to_string(),
            "</em>".to_string(),
        );
    }
    ("<em>".to_string(), "</em>".to_string())
}

fn field_specs(hl: &Value) -> Vec<(String, Map<String, Value>)> {
    let mut out = Vec::new();
    if let Some(fields) = hl.get("fields") {
        match fields {
            Value::Object(m) => {
                for (k, v) in m {
                    out.push((k.clone(), v.as_object().cloned().unwrap_or_default()));
                }
            }
            Value::Array(list) => {
                for item in list {
                    if let Some(m) = item.as_object() {
                        for (k, v) in m {
                            out.push((k.clone(), v.as_object().cloned().unwrap_or_default()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn collect_leaf_fields(doc: &Value, prefix: &str, out: &mut Vec<String>) {
    if let Some(m) = doc.as_object() {
        for (k, v) in m {
            let path = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{}.{}", prefix, k)
            };
            match v {
                Value::Object(_) => collect_leaf_fields(v, &path, out),
                Value::Array(arr) => {
                    let mut nested = false;
                    for item in arr {
                        if item.is_object() {
                            nested = true;
                            collect_leaf_fields(item, &path, out);
                        }
                    }
                    if !nested {
                        out.push(path);
                    }
                }
                _ => out.push(path),
            }
        }
    }
}

fn mapping_type(idx: &Index, field: &str) -> Option<String> {
    mapping_field_def(idx, field)
        .and_then(|d| d.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn is_text_type(t: &str) -> bool {
    matches!(
        t,
        "text" | "keyword" | "wildcard" | "match_only_text" | "search_as_you_type"
    )
}

fn collect_terms(
    q: &Map<String, Value>,
    target: Option<&str>,
    terms: &mut Vec<String>,
    touched: &mut bool,
) {
    for (clause, raw) in q {
        let clause = clause.as_str();
        if clause == "_name" {
            continue;
        }
        match clause {
            "match" | "match_phrase" | "match_phrase_prefix" | "match_bool_prefix" => {
                if let Some(m) = raw.as_object() {
                    if let Some((f, val)) = single_entry(m) {
                        if f == "_name" {
                            continue;
                        }
                        if target.map(|t| t == f).unwrap_or(true) {
                            let (text, _) = crate::eval::query_text(val);
                            terms.extend(analyze(&text));
                        }
                        if target == Some(f.as_str()) {
                            *touched = true;
                        }
                    }
                }
            }
            "term" | "terms" => {
                if let Some(m) = raw.as_object() {
                    for (f, val) in m {
                        if f == "_name" {
                            continue;
                        }
                        if target.map(|t| t == f).unwrap_or(true) {
                            match val {
                                Value::Array(arr) => {
                                    for v in arr {
                                        if let Some(s) = value_to_string(v) {
                                            terms.extend(analyze(&s));
                                        }
                                    }
                                }
                                _ => {
                                    if let Some(s) = value_to_string(val) {
                                        terms.extend(analyze(&s));
                                    }
                                }
                            }
                        }
                        if target == Some(f.as_str()) {
                            *touched = true;
                        }
                    }
                }
            }
            "multi_match" => {
                if let Some(m) = raw.as_object() {
                    let applies = match m.get("fields").and_then(|v| v.as_array()) {
                        Some(fs) => match target {
                            Some(t) => fs.iter().any(|f| {
                                f.as_str()
                                    .map(|p| p == t || wildcard_regex(p).is_match(t))
                                    .unwrap_or(false)
                            }),
                            None => true,
                        },
                        None => target.is_none(),
                    };
                    if applies {
                        if let Some(qv) = m.get("query") {
                            let (text, _) = crate::eval::query_text(qv);
                            terms.extend(analyze(&text));
                        }
                        if target.is_some() {
                            *touched = true;
                        }
                    }
                }
            }
            "query_string" | "simple_query_string" => {
                if let Some(m) = raw.as_object() {
                    let applies = match m.get("fields").and_then(|v| v.as_array()) {
                        Some(fs) => match target {
                            Some(t) => fs.iter().any(|f| {
                                f.as_str()
                                    .map(|p| p == t || wildcard_regex(p).is_match(t))
                                    .unwrap_or(false)
                            }),
                            None => true,
                        },
                        None => true,
                    };
                    if applies {
                        if let Some(qv) = m.get("query").and_then(|v| v.as_str()) {
                            terms.extend(analyze(qv));
                        }
                        if target.is_some() {
                            *touched = true;
                        }
                    }
                }
            }
            "bool" => {
                if let Some(bm) = raw.as_object() {
                    for key in ["must", "should", "filter"] {
                        for item in crate::eval::as_query_list(bm.get(key)) {
                            if let Some(qm) = item.as_object() {
                                collect_terms(qm, target, terms, touched);
                            }
                        }
                    }
                }
            }
            "constant_score" => {
                if let Some(m) = raw.as_object() {
                    if let Some(qv) = m.get("filter").and_then(|v| v.as_object()) {
                        collect_terms(qv, target, terms, touched);
                    }
                }
            }
            "function_score" => {
                if let Some(m) = raw.as_object() {
                    if let Some(qv) = m.get("query").and_then(|v| v.as_object()) {
                        collect_terms(qv, target, terms, touched);
                    }
                }
            }
            "dis_max" => {
                if let Some(m) = raw.as_object() {
                    for item in crate::eval::as_query_list(m.get("queries")) {
                        if let Some(qm) = item.as_object() {
                            collect_terms(qm, target, terms, touched);
                        }
                    }
                }
            }
            "boosting" => {
                if let Some(m) = raw.as_object() {
                    for key in ["positive", "negative"] {
                        if let Some(qm) = m.get(key).and_then(|v| v.as_object()) {
                            collect_terms(qm, target, terms, touched);
                        }
                    }
                }
            }
            "nested" => {
                if let Some(m) = raw.as_object() {
                    if let Some(qm) = m.get("query").and_then(|v| v.as_object()) {
                        collect_terms(qm, target, terms, touched);
                    }
                }
            }
            _ => {}
        }
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn mark_text(text: &str, terms: &[String], pre: &str, post: &str, html: bool, cutoff: Option<usize>) -> String {
    let termset: std::collections::HashSet<String> =
        terms.iter().map(|t| simple_lower(t)).collect();
    let mut out = String::new();
    let mut i = 0;
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let flush_plain = |plain: &mut String, out: &mut String, html: bool| {
        if !plain.is_empty() {
            if html {
                out.push_str(&escape_html(plain));
            } else {
                out.push_str(plain);
            }
            plain.clear();
        }
    };
    while i < chars.len() {
        if is_word_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let lower = simple_lower(&word);
            let within = cutoff.map(|c| start < c).unwrap_or(true);
            if within && termset.contains(&lower) {
                flush_plain(&mut plain, &mut out, html);
                out.push_str(pre);
                if html {
                    out.push_str(&escape_html(&word));
                } else {
                    out.push_str(&word);
                }
                out.push_str(post);
            } else {
                plain.push_str(&word);
            }
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush_plain(&mut plain, &mut out, html);
    out
}

struct Word {
    start: usize,
    end: usize,
    text: String,
}

fn words(text: &str) -> Vec<Word> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_word_char(chars[i].1) {
            let start = chars[i].0;
            let mut last = i;
            while i + 1 < chars.len() && is_word_char(chars[i + 1].1) {
                i += 1;
                last = i;
            }
            let end = chars[last].0 + chars[last].1.len_utf8();
            out.push(Word {
                start,
                end,
                text: text[start..end].to_string(),
            });
        }
        i += 1;
    }
    out
}

fn matching_words(ws: &[Word], terms: &[String]) -> Vec<usize> {
    let termset: std::collections::HashSet<String> =
        terms.iter().map(|t| simple_lower(t)).collect();
    ws.iter()
        .enumerate()
        .filter(|(_, w)| termset.contains(&simple_lower(&w.text)))
        .map(|(i, _)| i)
        .collect()
}

fn make_fragment(text: &str, start: usize, end: usize, terms: &[String], c: &Cfg, cutoff: Option<usize>) -> String {
    let (pre, post) = tags(c);
    let end = end.min(text.len());
    let start = start.min(end);
    mark_text(&text[start..end], terms, &pre, &post, c.encoder_html, cutoff)
}

fn build_fragments(text: &str, terms: &[String], c: &Cfg) -> Vec<String> {
    let ws = words(text);
    let matches = matching_words(&ws, terms);
    if matches.is_empty() {
        return vec![];
    }
    let size = c.fragment_size.unwrap_or(100);
    let nfrags = c.nfrags.unwrap_or(5);
    let (pre, post) = tags(c);
    if nfrags == 0 {
        return vec![mark_text(text, terms, &pre, &post, c.encoder_html, None)];
    }
    if c.fragment_size.is_none() && text.len() <= size {
        return vec![mark_text(text, terms, &pre, &post, c.encoder_html, None)];
    }

    let mut frags: Vec<(usize, usize, usize, bool)> = Vec::new();
    let boundary = c.boundary.as_deref();
    if boundary.is_some() && c.typ != "fvh" {
        for &mi in &matches {
            let (start, end) = if boundary == Some("word") {
                (ws[mi].start, ws[mi].end)
            } else {
                let prev = mi.saturating_sub(1);
                (ws[prev].start, ws[mi].end)
            };
            let count = matches
                .iter()
                .filter(|&&m| ws[m].start >= start && ws[m].end <= end)
                .count();
            frags.push((start, end, count, false));
        }
    } else if c.typ == "fvh" {
        let mut consumed_end = 0;
        for (pos, &mi) in matches.iter().enumerate() {
            if ws[mi].start < consumed_end {
                continue;
            }
            let start = if frags.is_empty() {
                0
            } else {
                let mut si = mi.saturating_sub(1);
                while si > 0 && ws[si].start < consumed_end {
                    si += 1;
                    break;
                }
                ws[si].start
            };
            let mut end = ws[mi].end;
            let mut cur = end - start;
            let mut j = mi + 1;
            while cur < size && j < ws.len() {
                cur = ws[j].end - start;
                end = ws[j].end;
                j += 1;
            }
            let termset: std::collections::HashSet<String> =
                terms.iter().map(|t| simple_lower(t)).collect();
            let mut extended = false;
            while j < ws.len() && termset.contains(&simple_lower(&ws[j].text)) {
                end = ws[j].end;
                j += 1;
                extended = true;
            }
            let count = matches
                .iter()
                .filter(|&&m| ws[m].start >= start && ws[m].end <= end)
                .count();
            frags.push((start, end, count, extended));
            consumed_end = end;
            let _ = pos;
        }
    } else {
        let mut consumed_end = 0;
        let mut i = 0;
        while i < matches.len() {
            let mi = matches[i];
            if ws[mi].start < consumed_end {
                i += 1;
                continue;
            }
            let mut run = 1;
            while i + run < matches.len() && matches[i + run] == mi + run {
                run += 1;
            }
            let anchor = if frags.is_empty() {
                mi
            } else if run >= 3 {
                mi + 1
            } else {
                mi
            };
            let start = if frags.is_empty() {
                ws[anchor.saturating_sub(2)].start
            } else if run >= 3 {
                ws[anchor].start
            } else {
                ws[anchor.saturating_sub(2)].start
            };
            let mut end = ws[anchor].end;
            let mut cur = end - start;
            let mut j = anchor + 1;
            while cur < size && j < ws.len() {
                cur = ws[j].end - start;
                end = ws[j].end;
                j += 1;
            }
            let count = matches
                .iter()
                .filter(|&&m| ws[m].start >= start && ws[m].end <= end)
                .count();
            frags.push((start, end, count, false));
            consumed_end = end;
            i += run.max(1);
        }
    }

    if c.typ == "plain" {
        frags.clear();
        let mut seen = std::collections::HashSet::new();
        for &mi in &matches {
            let end = ws[mi].end;
            let start = end.saturating_sub(size);
            if !seen.insert((start, end)) {
                continue;
            }
            let count = matches
                .iter()
                .filter(|&&m| ws[m].start >= start && ws[m].end <= end)
                .count();
            frags.push((start, end, count, false));
        }
    }

    if c.order_score {
        frags.sort_by(|a, b| b.2.cmp(&a.2));
    }
    if nfrags == 1 && frags.len() > 1 {
        if let Some(best) = frags.iter().max_by_key(|f| f.2) {
            let best = *best;
            frags = vec![best];
        }
    }
    frags.truncate(nfrags);
    frags
        .into_iter()
        .map(|(s, e, _, _)| {
            let cutoff = if c.typ == "fvh" {
                c.fragment_size
            } else {
                None
            };
            make_fragment(text, s, e, terms, c, cutoff)
        })
        .collect()
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().filter_map(text_of).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => value_to_string(v),
    }
}

pub fn render(
    hl: &Value,
    query: &Map<String, Value>,
    doc: &Value,
    idx: &Index,
) -> Result<Option<Value>, ApiError> {
    let global = parse_global(hl)?;
    let specs = field_specs(hl);
    if specs.is_empty() {
        return Ok(None);
    }
    let mut leaves = Vec::new();
    collect_leaf_fields(doc, "", &mut leaves);
    let mut out = Map::new();

    for (pattern, opts) in specs {
        let cfg = apply_options(&global, &opts)?;
        if cfg.pre.is_some() && cfg.post.is_none() {
            return Err(xcontent_error("pre_tags are set but post_tags are not set"));
        }
        let has_wild = pattern.contains('*') || pattern.contains('?');
        let mut names = Vec::new();
        if has_wild {
            let re = wildcard_regex(&pattern);
            for leaf in &leaves {
                if re.is_match(leaf) && is_text_type(&mapping_type(idx, leaf).unwrap_or_default()) {
                    names.push(leaf.clone());
                }
            }
        } else {
            names.push(pattern.clone());
        }
        for name in names {
            let ftype = mapping_type(idx, &name).unwrap_or_default();
            if !is_text_type(&ftype) {
                continue;
            }
            if cfg.typ == "fvh" {
                let def = mapping_field_def(idx, &name);
                let tv = def
                    .and_then(|d| d.get("term_vector"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !tv.contains("positions") || !tv.contains("offsets") {
                    return Err(search_error(&format!(
                        "the field [{}] should be indexed with term vector with position offsets to be used with fast vector highlighter",
                        name
                    )));
                }
                if cfg.boundary.as_deref() == Some("sentence")
                    && cfg.fragment_size.unwrap_or(100) < 18
                {
                    let s = cfg.fragment_size.unwrap_or(100);
                    return Err(search_error(&format!(
                        "fragCharSize({}) is too small. It must be 18 or higher.",
                        s
                    )));
                }
            }
            if !matches!(
                cfg.typ.as_str(),
                "unified" | "plain" | "fvh" | "experimental"
            ) {
                return Err(search_error(&format!(
                    "unknown highlighter type [{}] for the field [{}]",
                    cfg.typ, name
                )));
            }
            let text = match field_value(doc, &name).and_then(text_of) {
                Some(t) => t,
                None => continue,
            };
            let mut terms: Vec<String> = Vec::new();
            let mut touched = false;
            if let Some(hq) = &cfg.highlight_query {
                if let Some(qm) = hq.as_object() {
                    collect_terms(qm, Some(&name), &mut terms, &mut touched);
                }
            } else {
                collect_terms(query, Some(&name), &mut terms, &mut touched);
            }
            if !touched {
                if cfg.require_field_match != Some(false) {
                    terms.clear();
                } else if terms.is_empty() {
                    collect_terms(query, None, &mut terms, &mut touched);
                }
            } else if terms.is_empty() && cfg.require_field_match == Some(false) {
                collect_terms(query, None, &mut terms, &mut touched);
            }

            let frags = build_fragments(&text, &terms, &cfg);
            if !frags.is_empty() {
                out.insert(name, json!(frags));
            } else if let Some(n) = cfg.no_match_size {
                if !text.is_empty() {
                    let snippet: String = text.chars().take(n).collect();
                    out.insert(name, json!([snippet]));
                }
            }
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(out)))
    }
}
