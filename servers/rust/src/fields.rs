use crate::resp::format_number;
use crate::store::*;
use serde_json::{json, Map, Number, Value};
use std::collections::{BTreeMap, HashSet};

// ---------------------------------------------------------------------------
// Index-time field value extraction (port of servers/go/fields.go).
// ---------------------------------------------------------------------------

pub struct IndexCtx {
    pub index_coerce: bool,
    pub index_ignore_mal: bool,
    pub doc_id: String,
    pub doc: Map<String, Value>,
    dyn_setting: String,
    ignored: Vec<String>,
    ignored_set: HashSet<String>,
    pub values: BTreeMap<String, Vec<Value>>,
    normalizer_filters: std::collections::HashMap<String, Vec<String>>,
    dynamic_templates: Vec<Value>,
    date_detection: bool,
    numeric_detection: bool,
    dynamic_date_formats: Vec<String>,
    analyzers: HashSet<String>,
    copy_targets: HashSet<String>,
}

pub fn mapper_err(typ: &str, reason: &str, caused_type: &str, caused_reason: &str) -> ApiError {
    let mut e = ApiError::new(typ, reason, 400);
    if !caused_type.is_empty() {
        e = e.caused_by(ApiError::new(caused_type, caused_reason, 400));
    } else if !caused_reason.is_empty() {
        e = e.caused_by(ApiError::new("illegal_argument_exception", caused_reason, 400));
    }
    e
}

pub fn preview_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(_) => value_to_string(v).unwrap_or_default(),
        Value::Object(_) => "{...}".to_string(),
        Value::Array(_) => "[...]".to_string(),
    }
}

impl IndexCtx {
    pub fn new(idx: &Index, doc_id: &str) -> Self {
        let mut c = IndexCtx {
            index_coerce: true,
            index_ignore_mal: false,
            doc_id: doc_id.to_string(),
            doc: Map::new(),
            dyn_setting: match idx.mappings.get("dynamic") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Bool(b)) => b.to_string(),
                _ => "true".to_string(),
            },
            ignored: Vec::new(),
            ignored_set: HashSet::new(),
            values: BTreeMap::new(),
            normalizer_filters: std::collections::HashMap::new(),
            dynamic_templates: Vec::new(),
            date_detection: true,
            numeric_detection: false,
            dynamic_date_formats: Vec::new(),
            analyzers: HashSet::new(),
            copy_targets: HashSet::new(),
        };
        for (k, v) in &idx.settings {
            if let Some(rest) = k.strip_prefix("index.analysis.normalizer.") {
                if let Some(name) = rest.strip_suffix(".filter") {
                    let mut filters = Vec::new();
                    match v {
                        Value::Array(a) => {
                            for x in a {
                                if let Some(f) = value_to_string(x) {
                                    filters.push(f);
                                }
                            }
                        }
                        Value::String(sv) => {
                            if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(sv) {
                                for x in &a {
                                    if let Some(f) = value_to_string(x) {
                                        filters.push(f);
                                    }
                                }
                            } else {
                                filters.push(sv.clone());
                            }
                        }
                        _ => {}
                    }
                    c.normalizer_filters.insert(name.to_string(), filters);
                }
            }
        }
        if let Some(a) = idx.mappings.get("dynamic_templates").and_then(|v| v.as_array()) {
            c.dynamic_templates = a.clone();
        }
        c.date_detection = idx
            .mappings
            .get("date_detection")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        c.numeric_detection = idx
            .mappings
            .get("numeric_detection")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(a) = idx.mappings.get("dynamic_date_formats").and_then(|v| v.as_array()) {
            c.dynamic_date_formats = a
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        for (k, _) in &idx.settings {
            if let Some(rest) = k.strip_prefix("index.analysis.analyzer.") {
                if let Some(name) = rest.strip_suffix(".type") {
                    c.analyzers.insert(name.to_string());
                }
            }
        }
        if let Some(v) = find_setting(&idx.settings, "mapping.coerce") {
            c.index_coerce = settings_value_string(v) != "false";
        }
        if let Some(v) = find_setting(&idx.settings, "mapping.ignore_malformed") {
            c.index_ignore_mal = settings_value_string(v) == "true";
        }
        c
    }

    pub fn query_ctx() -> Self {
        IndexCtx {
            index_coerce: true,
            index_ignore_mal: false,
            doc_id: String::new(),
            doc: Map::new(),
            dyn_setting: "true".to_string(),
            ignored: Vec::new(),
            ignored_set: HashSet::new(),
            values: BTreeMap::new(),
            normalizer_filters: std::collections::HashMap::new(),
            dynamic_templates: Vec::new(),
            date_detection: true,
            numeric_detection: false,
            dynamic_date_formats: Vec::new(),
            analyzers: HashSet::new(),
            copy_targets: HashSet::new(),
        }
    }

    fn field_value_error(
        &self,
        name: &str,
        typ: &str,
        v: &Value,
        cause_type: &str,
        cause_reason: &str,
    ) -> ApiError {
        let reason = format!(
            "failed to parse field [{}] of type [{}] in document with id '{}'. Preview of field's value: '{}'",
            name,
            typ,
            self.doc_id,
            preview_value(v)
        );
        mapper_err("mapper_parsing_exception", &reason, cause_type, cause_reason)
    }
}

fn geohash_decode(s: &str) -> Option<(f64, f64)> {
    const ALPHA: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    if s.is_empty() || s.len() > 12 || !s.is_ascii() {
        return None;
    }
    let (mut lon_lo, mut lon_hi) = (-180.0f64, 180.0f64);
    let (mut lat_lo, mut lat_hi) = (-90.0f64, 90.0f64);
    let mut lon_bit = true;
    for ch in s.bytes() {
        let idx = ALPHA.iter().position(|c| *c == ch.to_ascii_lowercase())? as u8;
        for shift in (0..5).rev() {
            let bit = (idx >> shift) & 1 == 1;
            if lon_bit {
                let mid = (lon_lo + lon_hi) / 2.0;
                if bit { lon_lo = mid } else { lon_hi = mid }
            } else {
                let mid = (lat_lo + lat_hi) / 2.0;
                if bit { lat_lo = mid } else { lat_hi = mid }
            }
            lon_bit = !lon_bit;
        }
    }
    Some(((lon_lo + lon_hi) / 2.0, (lat_lo + lat_hi) / 2.0))
}

fn is_integer_kind(t: &str) -> bool {
    matches!(t, "byte" | "short" | "integer" | "long" | "unsigned_long")
}

fn type_display_name(t: &str) -> String {
    match t {
        "unsigned_long" => "unsigned long".to_string(),
        other => other.to_string(),
    }
}

fn num_i64(i: i64) -> Value {
    Value::Number(Number::from(i))
}

fn num_u64(u: u64) -> Value {
    Value::Number(Number::from(u))
}

fn num_f64(f: f64) -> Value {
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

impl IndexCtx {
    pub(crate) fn parseIntValue(
        &self,
        name: &str,
        typ: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        let mut coerce = self.index_coerce;
        if let Some(b) = def.get("coerce").and_then(|v| v.as_bool()) {
            coerce = b;
        }
        match v {
            Value::Number(n) => {
                if typ == "unsigned_long" {
                    if let Some(u) = n.as_u64() {
                        return Ok(num_u64(u));
                    }
                }
                if !coerce && is_integer_kind(typ) {
                    if typ == "unsigned_long" {
                        if let Some(u) = n.as_u64() {
                            return Ok(num_u64(u));
                        }
                    }
                    if let Some(i) = n.as_i64() {
                        return self.checkIntRange(name, typ, i, &num_i64(i));
                    }
                    if let Some(f) = n.as_f64() {
                        if f.fract() != 0.0 {
                            return Err(self.field_value_error(
                                name,
                                typ,
                                v,
                                "illegal_argument_exception",
                                &format!(
                                    "{} cannot be converted to Integer without data loss",
                                    format_number(f)
                                ),
                            ));
                        }
                        return self.checkIntRange(name, typ, f as i64, &num_f64(f));
                    }
                }
                let s = number_string(n);
                if is_integer_kind(typ) && s.parse::<i64>().is_err() {
                    if let Ok(f) = s.parse::<f64>() {
                        if f > i64::MAX as f64 || f < i64::MIN as f64 {
                            return Err(self.field_value_error(
                                name,
                                typ,
                                v,
                                "input_coercion_exception",
                                &format!("Value [{}] is out of range for a {}", s, type_display_name(typ)),
                            ));
                        }
                    }
                }
                let res = self.parseIntString(name, typ, def, &s, coerce, false);
                if typ != "byte" && typ != "unsigned_long" {
                    if let Err(mut e) = res {
                        if let Some(cb) = e.caused_by.as_mut() {
                            if cb.typ == "illegal_argument_exception"
                                || cb.typ == "number_format_exception"
                            {
                                cb.typ = "input_coercion_exception".to_string();
                            }
                        }
                        return Err(e);
                    }
                    return res;
                }
                res
            }
            Value::Bool(b) => Err(self.field_value_error(
                name,
                typ,
                v,
                "input_coercion_exception",
                &format!("Cannot coerce value {} to {}", b, typ),
            )),
            Value::String(s) => {
                let trim = !matches!(typ, "long" | "unsigned_long");
                self.parseIntString(name, typ, def, s, coerce, trim)
            }
            _ => Err(self.field_value_error(
                name,
                typ,
                v,
                "input_coercion_exception",
                &format!("For input string: \"{}\"", preview_value(v)),
            )),
        }
    }

    fn parseIntFloat(
        &self,
        name: &str,
        typ: &str,
        f: f64,
        coerce: bool,
    ) -> Result<Value, ApiError> {
        if !coerce && f != f.trunc() {
            return Err(self.field_value_error(
                name,
                typ,
                &Value::String(format_number(f)),
                "illegal_argument_exception",
                &format!("{} cannot be converted to Integer without data loss", format_number(f)),
            ));
        }
        self.checkIntRange(name, typ, f as i64, &num_f64(f))
    }

    fn checkIntRange(
        &self,
        name: &str,
        typ: &str,
        iv: i64,
        orig: &Value,
    ) -> Result<Value, ApiError> {
        match typ {
            "byte" => {
                if iv > i8::MAX as i64 || iv < i8::MIN as i64 {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        orig,
                        "illegal_argument_exception",
                        &format!("Value [{}] is out of range for a byte", iv),
                    ));
                }
            }
            "short" => {
                if iv > i16::MAX as i64 || iv < i16::MIN as i64 {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        orig,
                        "illegal_argument_exception",
                        &format!("Value [{}] is out of range for a short", iv),
                    ));
                }
            }
            "integer" => {
                if iv > i32::MAX as i64 || iv < i32::MIN as i64 {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        orig,
                        "illegal_argument_exception",
                        &format!("Value [{}] is out of range for an integer", iv),
                    ));
                }
            }
            _ => {}
        }
        if typ == "unsigned_long" {
            if iv < 0 {
                return Err(self.field_value_error(
                    name,
                    typ,
                    orig,
                    "illegal_argument_exception",
                    &format!("Value [{}] is out of range for an unsigned long", iv),
                ));
            }
            return Ok(num_u64(iv as u64));
        }
        Ok(num_i64(iv))
    }

    fn parseIntString(
        &self,
        name: &str,
        typ: &str,
        def: &Map<String, Value>,
        s: &str,
        coerce: bool,
        trim: bool,
    ) -> Result<Value, ApiError> {
        let field_coerce = def.get("coerce").and_then(|v| v.as_bool());
        let coerce_false = field_coerce == Some(false) || (field_coerce.is_none() && !coerce);
        if coerce_false {
            let word = if is_integer_kind(typ) { "Integer" } else { "Float" };
            return Err(self.field_value_error(
                name,
                typ,
                &Value::String(s.to_string()),
                "illegal_argument_exception",
                &format!("{} value passed as String", word),
            ));
        }
        let orig = s.to_string();
        let mut st = if trim { s.trim().to_string() } else { s.to_string() };
        if typ == "unsigned_long" {
            match st.parse::<u64>() {
                Ok(u) => return Ok(num_u64(u)),
                Err(e) => {
                    if *e.kind() == std::num::IntErrorKind::PosOverflow {
                        return Err(self.field_value_error(
                            name,
                            typ,
                            &Value::String(orig.clone()),
                            "illegal_argument_exception",
                            &format!("Value [{}] is out of range for an unsigned long", orig),
                        ));
                    }
                    if let Ok(f) = st.parse::<f64>() {
                        if f < 0.0 {
                            return Err(self.field_value_error(
                                name,
                                typ,
                                &Value::String(orig.clone()),
                                "illegal_argument_exception",
                                &format!("Value [{}] is out of range for an unsigned long", orig),
                            ));
                        }
                        if coerce && f.is_finite() && f <= u64::MAX as f64 {
                            return Ok(num_u64(f as u64));
                        }
                    }
                    return Err(self.field_value_error(
                        name,
                        typ,
                        &Value::String(orig.clone()),
                        "illegal_argument_exception",
                        &format!("For input string: \"{}\"", orig),
                    ));
                }
            }
        }
        match st.parse::<i64>() {
            Ok(iv) => self.checkIntRange(name, typ, iv, &num_i64(iv)),
            Err(e) => {
                if *e.kind() == std::num::IntErrorKind::PosOverflow
                    || *e.kind() == std::num::IntErrorKind::NegOverflow
                {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        &Value::String(orig.clone()),
                        "illegal_argument_exception",
                        &format!(
                            "Value [{}] is out of range for a {}",
                            orig,
                            type_display_name(typ)
                        ),
                    ));
                }
                if is_integer_kind(typ) {
                    if let Ok(f) = st.parse::<f64>() {
                        let iv = f as i64;
                        return self.checkIntRange(name, typ, iv, &num_f64(f));
                    }
                    let cause_type = if typ == "long" {
                        "illegal_argument_exception"
                    } else {
                        "number_format_exception"
                    };
                    return Err(self.field_value_error(
                        name,
                        typ,
                        &Value::String(orig.clone()),
                        cause_type,
                        &format!("For input string: \"{}\"", orig),
                    ));
                }
                Err(self.field_value_error(
                    name,
                    typ,
                    &Value::String(orig.clone()),
                    "number_format_exception",
                    &format!("For input string: \"{}\"", orig),
                ))
            }
        }
    }

    pub(crate) fn parseFloatValue(
        &self,
        name: &str,
        typ: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        let mut coerce_false = false;
        if let Some(b) = def.get("coerce").and_then(|v| v.as_bool()) {
            coerce_false = !b;
        }
        let f: f64;
        match v {
            Value::Number(n) => {
                f = n.as_f64().unwrap_or(0.0);
            }
            Value::Bool(_) => {
                return Err(self.field_value_error(
                    name,
                    typ,
                    v,
                    "input_coercion_exception",
                    "Cannot coerce boolean",
                ))
            }
            Value::String(raw) => {
                if coerce_false {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        v,
                        "illegal_argument_exception",
                        "Float value passed as String",
                    ));
                }
                let lv = simple_lower(raw.trim());
                if matches!(lv.as_str(), "nan" | "infinity" | "-infinity" | "inf" | "-inf") {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        v,
                        "illegal_argument_exception",
                        &format!("[{}] supports only finite values, but got [{}]", typ, raw),
                    ));
                }
                if raw.contains(' ') {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        v,
                        "number_format_exception",
                        &format!("For input string: \"{}\"", raw),
                    ));
                }
                match raw.trim().parse::<f64>() {
                    Ok(fv) => {
                        if fv.is_infinite() {
                            return Err(self.field_value_error(
                                name,
                                typ,
                                v,
                                "illegal_argument_exception",
                                &format!("[{}] supports only finite values, but got [{}]", typ, raw),
                            ));
                        }
                        f = fv;
                    }
                    Err(_) => {
                        return Err(self.field_value_error(
                            name,
                            typ,
                            v,
                            "number_format_exception",
                            &format!("For input string: \"{}\"", raw),
                        ))
                    }
                }
            }
            _ => {
                return Err(self.field_value_error(
                    name,
                    typ,
                    v,
                    "input_coercion_exception",
                    "malformed",
                ))
            }
        }
        if f.is_nan() || f.is_infinite() {
            return Err(self.field_value_error(
                name,
                typ,
                v,
                "illegal_argument_exception",
                &format!("[{}] supports only finite values, but got [{}]", typ, preview_value(v)),
            ));
        }
        match typ {
            "float" => {
                if f > f32::MAX as f64 || f < -(f32::MAX as f64) {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        v,
                        "input_coercion_exception",
                        &format!("Value [{}] is out of range for a float", format_number(f)),
                    ));
                }
                Ok(num_f64(f as f32 as f64))
            }
            "half_float" => {
                if !(-65504.0..=65504.0).contains(&f) {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        v,
                        "input_coercion_exception",
                        &format!("Value [{}] is out of range for a half_float", format_number(f)),
                    ));
                }
                Ok(num_f64(float32_from_half(float32_half(f)) as f64))
            }
            "scaled_float" => {
                let scale = def
                    .get("scaling_factor")
                    .and_then(to_f64)
                    .unwrap_or(1.0);
                Ok(num_f64((f * scale).round() / scale))
            }
            _ => Ok(num_f64(f)),
        }
    }

    pub(crate) fn parseBooleanValue(
        &self,
        name: &str,
        v: &Value,
    ) -> Result<Value, ApiError> {
        match v {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::String(s) => {
                if s.is_empty() {
                    return Ok(Value::Bool(false));
                }
                if s == "true" || s == "false" {
                    return Ok(Value::Bool(s == "true"));
                }
                Err(self.field_value_error(
                    name,
                    "boolean",
                    v,
                    "illegal_argument_exception",
                    &format!(
                        "Failed to parse value [{}] as only [true] or [false] are allowed.",
                        s
                    ),
                ))
            }
            Value::Number(n) => {
                let s = number_string(n);
                if s.is_empty() {
                    return Ok(Value::Bool(false));
                }
                Err(self.field_value_error(
                    name,
                    "boolean",
                    v,
                    "input_coercion_exception",
                    &format!(
                        "Failed to parse value [{}] as only [true] or [false] are allowed.",
                        s
                    ),
                ))
            }
            _ => Err(self.field_value_error(
                name,
                "boolean",
                v,
                "illegal_argument_exception",
                &format!(
                    "Failed to parse value [{}] as only [true] or [false] are allowed.",
                    preview_value(v)
                ),
            )),
        }
    }

    pub(crate) fn parseDateValue(
        &self,
        name: &str,
        typ: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        if let Value::String(s) = v {
            if s.is_empty() {
                return Err(self.field_value_error(
                    name,
                    typ,
                    v,
                    "illegal_argument_exception",
                    "cannot parse empty date",
                ));
            }
        }
        let fmt = date_format_of(def);
        let has_epoch_ms = fmt.is_empty() || fmt.split("||").any(|p| p.trim() == "epoch_millis");
        let has_epoch_s = fmt.split("||").any(|p| p.trim() == "epoch_second");
        if let Value::Number(n) = v {
            let f = n.as_f64().unwrap_or(0.0);
            if has_epoch_s && !has_epoch_ms {
                let ms = (f * 1000.0) as i64;
                return Ok(num_i64(if typ == "date_nanos" { ms * 1_000_000 } else { ms }));
            }
            if has_epoch_ms {
                let is_default = def
                    .get("format")
                    .and_then(|v| v.as_str())
                    .map(|v| v.is_empty())
                    .unwrap_or(true);
                if is_default && f < 0.0 {
                    let y = f as i64;
                    let ms = days_from_civil(y, 1, 1) * 86_400_000;
                    return Ok(num_i64(if typ == "date_nanos" {
                        ms * 1_000_000
                    } else {
                        ms
                    }));
                }
                let ms = f as i64;
                return Ok(num_i64(if typ == "date_nanos" { ms * 1_000_000 } else { ms }));
            }
            let shown = number_string(n);
            return Err(self.field_value_error(
                name,
                typ,
                v,
                "illegal_argument_exception",
                &format!("failed to parse date field [{}] with format [{}]", shown, fmt),
            ));
        }
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                let shown = preview_value(v);
                return Err(self.field_value_error(
                    name,
                    typ,
                    v,
                    "illegal_argument_exception",
                    &format!("failed to parse date field [{}] with format [{}]", shown, fmt),
                ));
            }
        };
        match parse_date_any(def, &s) {
            Some(ms) => {
                if typ == "date_nanos" {
                    if ms < 0 {
                        return Err(self.field_value_error(
                            name,
                            typ,
                            v,
                            "illegal_argument_exception",
                            &format!(
                                "date[{}] is before the epoch in 1970 and cannot be stored in nanosecond resolution",
                                s
                            ),
                        ));
                    }
                    if ms > i64::MAX / 1_000_000 {
                        return Err(self.field_value_error(
                            name,
                            typ,
                            v,
                            "illegal_argument_exception",
                            &format!(
                                "date[{}] is after 2262-04-11T23:47:16.854775807 and cannot be stored in nanosecond resolution",
                                s
                            ),
                        ));
                    }
                    let ns = parse_date_nanos_any(def, &s).unwrap_or(ms * 1_000_000);
                    if ns < 0 {
                        return Err(self.field_value_error(
                            name,
                            typ,
                            v,
                            "illegal_argument_exception",
                            &format!(
                                "date[{}] is before the epoch in 1970 and cannot be stored in nanosecond resolution",
                                s
                            ),
                        ));
                    }
                    return Ok(num_i64(ns));
                }
                Ok(num_i64(ms))
            }
            None => Err(self.field_value_error(
                name,
                typ,
                v,
                "illegal_argument_exception",
                &format!(
                    "failed to parse date field [{}] with format [{}]",
                    s,
                    date_format_of(def)
                ),
            )),
        }
    }

    pub(crate) fn parseIPValue(&self, name: &str, v: &Value) -> Result<Value, ApiError> {
        let s = match value_to_string(v) {
            Some(s) => s,
            None => {
                return Err(self.field_value_error(
                    name,
                    "ip",
                    v,
                    "illegal_argument_exception",
                    "failed to parse ip",
                ))
            }
        };
        match canonical_ip(&s) {
            Ok(c) => Ok(Value::String(c)),
            Err(ones) => {
                let reason = if ones < 0 {
                    format!("'{}' is not an IP string literal.", s)
                } else {
                    format!(
                        "Illegal prefix length [{}] in [{}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
                        ones, s
                    )
                };
                Err(self.field_value_error(
                    name,
                    "ip",
                    &Value::String(s.clone()),
                    "illegal_argument_exception",
                    &reason,
                ))
            }
        }
    }
}

pub fn date_format_of(def: &Map<String, Value>) -> String {
    def.get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("strict_date_optional_time||epoch_millis")
        .to_string()
}

pub fn number_string(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else if let Some(f) = n.as_f64() {
        format_number(f)
    } else {
        n.to_string()
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parses the default ISO layouts, returning epoch millis (UTC).
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

pub fn parse_iso_millis(s: &str) -> Option<i64> {
    let (secs, nanos) = parse_iso_full(s)?;
    Some(secs * 1000 + nanos / 1_000_000)
}

/// Returns (epoch seconds, sub-second nanos).
pub fn parse_iso_full(s: &str) -> Option<(i64, i64)> {
    let b = s.as_bytes();
    let neg = b.first() == Some(&b'-');
    let ystart = if neg { 1usize } else { 0usize };
    if b.len() < ystart + 4 {
        return None;
    }
    if !b[ystart..ystart + 4].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut y: i64 = s[ystart..ystart + 4].parse().ok()?;
    if neg {
        y = -y;
    }
    let mut mo: i64 = 1;
    let mut d: i64 = 1;
    let mut pos = ystart + 4;
    if b.len() > pos {
        if b[pos] != b'-' {
            return None;
        }
        if b.len() < pos + 3 || !b[pos + 1..pos + 3].iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        mo = s[pos + 1..pos + 3].parse().ok()?;
        if !(1..=12).contains(&mo) {
            return None;
        }
        pos += 3;
        if b.len() > pos {
            if b[pos] != b'-' {
                return None;
            }
            if b.len() < pos + 3 || !b[pos + 1..pos + 3].iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            d = s[pos + 1..pos + 3].parse().ok()?;
            if d < 1 || d > days_in_month(y, mo) {
                return None;
            }
            pos += 3;
        }
    }
    let mut hh = 0i64;
    let mut mi = 0i64;
    let mut ss = 0i64;
    let mut nanos = 0i64;
    let mut offset_min = 0i64;
    if pos < b.len() {
        if b[pos] != b'T' {
            return None;
        }
        pos += 1;
        if b.len() < pos + 5 {
            return None;
        }
        if b[pos + 2] != b':' {
            return None;
        }
        for c in [b[pos], b[pos + 1], b[pos + 3], b[pos + 4]] {
            if !c.is_ascii_digit() {
                return None;
            }
        }
        hh = s[pos..pos + 2].parse().ok()?;
        mi = s[pos + 3..pos + 5].parse().ok()?;
        pos += 5;
        if pos + 2 < b.len() && b[pos] == b':' {
            if !b[pos + 1].is_ascii_digit() || !b[pos + 2].is_ascii_digit() {
                return None;
            }
            ss = s[pos + 1..pos + 3].parse().ok()?;
            pos += 3;
        }
        if hh > 23 || mi > 59 || ss > 60 {
            return None;
        }
        if pos < b.len() && b[pos] == b'.' {
            let start = pos + 1;
            let mut end = start;
            while end < b.len() && b[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                return None;
            }
            let digits = &s[start..end.min(start + 9)];
            let mut v: i64 = digits.parse().ok()?;
            for _ in digits.len()..9 {
                v *= 10;
            }
            nanos = v;
            pos = end;
        }
        if pos < b.len() {
            match b[pos] {
                b'Z' => {
                    pos += 1;
                }
                b'+' | b'-' => {
                    let sign: i64 = if b[pos] == b'+' { 1 } else { -1 };
                    pos += 1;
                    if b.len() < pos + 5 || b[pos + 2] != b':' {
                        return None;
                    }
                    if !b[pos].is_ascii_digit()
                        || !b[pos + 1].is_ascii_digit()
                        || !b[pos + 3].is_ascii_digit()
                        || !b[pos + 4].is_ascii_digit()
                    {
                        return None;
                    }
                    let oh: i64 = s[pos..pos + 2].parse().ok()?;
                    let om: i64 = s[pos + 3..pos + 5].parse().ok()?;
                    offset_min = sign * (oh * 60 + om);
                    pos += 5;
                }
                _ => return None,
            }
        }
    }
    if pos != b.len() {
        return None;
    }
    let secs =
        days_from_civil(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss - offset_min * 60;
    Some((secs, nanos))
}

fn java_format_to_parse(pattern: &str, s: &str) -> Option<i64> {
    let pb: Vec<char> = pattern.chars().collect();
    let sb: Vec<char> = s.chars().collect();
    let mut si = 0usize;
    let mut y = 1970i64;
    let mut mo = 1i64;
    let mut d = 1i64;
    let mut hh = 0i64;
    let mut mi = 0i64;
    let mut ss = 0i64;
    let mut nanos = 0i64;
    let mut pi = 0usize;
    while pi < pb.len() {
        let c = pb[pi];
        if c.is_ascii_alphabetic() {
            let start = pi;
            while pi < pb.len() && pb[pi] == c {
                pi += 1;
            }
            let n = pi - start;
            let take_digits = |si: &mut usize, min: usize, max: usize| -> Option<String> {
                let mut out = String::new();
                while *si < sb.len() && out.len() < max && sb[*si].is_ascii_digit() {
                    out.push(sb[*si]);
                    *si += 1;
                }
                if out.len() < min {
                    None
                } else {
                    Some(out)
                }
            };
            match c {
                'y' => {
                    let ds = take_digits(&mut si, n, n)?;
                    y = ds.parse().ok()?;
                    if n == 2 {
                        y += if y < 70 { 2000 } else { 1900 };
                    }
                }
                'M' => {
                    let ds = take_digits(&mut si, if n >= 2 { 2 } else { 1 }, n.max(2))?;
                    mo = ds.parse().ok()?;
                }
                'd' => {
                    let ds = take_digits(&mut si, if n >= 2 { 2 } else { 1 }, n.max(2))?;
                    d = ds.parse().ok()?;
                }
                'H' => {
                    let ds = take_digits(&mut si, 2, 2)?;
                    hh = ds.parse().ok()?;
                }
                'm' => {
                    let ds = take_digits(&mut si, 2, 2)?;
                    mi = ds.parse().ok()?;
                }
                's' => {
                    let ds = take_digits(&mut si, 2, 2)?;
                    ss = ds.parse().ok()?;
                }
                'S' => {
                    let ds = take_digits(&mut si, 1, n.min(9))?;
                    let mut v: i64 = ds.parse().ok()?;
                    for _ in ds.len()..9 {
                        v *= 10;
                    }
                    nanos = v;
                }
                _ => return None,
            }
        } else {
            pi += 1;
            if c == '\'' {
                continue;
            }
            if si >= sb.len() || sb[si] != c {
                return None;
            }
            si += 1;
        }
    }
    if si != sb.len() {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400_000 + hh * 3_600_000 + mi * 60_000 + ss * 1000 + nanos / 1_000_000)
}

pub fn parse_date_any(def: &Map<String, Value>, s: &str) -> Option<i64> {
    if let Some(f) = def.get("format").and_then(|v| v.as_str()) {
        if !f.is_empty() {
            for part in f.split("||") {
                let p = part.trim();
                match p {
                    "epoch_millis" => {
                        if let Ok(n) = s.parse::<f64>() {
                            return Some(n as i64);
                        }
                    }
                    "epoch_second" => {
                        if let Ok(n) = s.parse::<f64>() {
                            return Some((n * 1000.0) as i64);
                        }
                    }
                    "date_optional_time" | "strict_date_optional_time" | "date_time"
                    | "strict_date_optional_time_nanos" | "basic_date_time" => {
                        if let Some(ms) = parse_iso_millis(s) {
                            return Some(ms);
                        }
                    }
                    "strict_date" => {
                        if let Some(ms) = java_format_to_parse("yyyy-MM-dd", s) {
                            return Some(ms);
                        }
                    }
                    "date" => {
                        if let Some(ms) = java_format_to_parse("yyyy-M-d", s) {
                            return Some(ms);
                        }
                    }
                    "basic_date" => {
                        if let Some(ms) = java_format_to_parse("yyyyMMdd", s) {
                            return Some(ms);
                        }
                    }
                    _ => {
                        if let Some(ms) = java_format_to_parse(p, s) {
                            return Some(ms);
                        }
                    }
                }
            }
            return None;
        }
    }
    if let Some(ms) = parse_iso_millis(s) {
        return Some(ms);
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    None
}

pub fn parse_date_nanos_any(def: &Map<String, Value>, s: &str) -> Option<i64> {
    if let Some(f) = def.get("format").and_then(|v| v.as_str()) {
        if !f.is_empty() {
            for part in f.split("||") {
                if part.trim() == "epoch_millis" {
                    if let Ok(n) = s.parse::<i64>() {
                        return n.checked_mul(1_000_000);
                    }
                }
            }
        }
    }
    if let Some((secs, nanos)) = parse_iso_full(s) {
        return secs.checked_mul(1_000_000_000)?.checked_add(nanos);
    }
    parse_date_any(def, s).and_then(|ms| ms.checked_mul(1_000_000))
}

pub fn canonical_ip(s: &str) -> Result<String, i32> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    if let Some(idx) = s.find('/') {
        let addr_part = &s[..idx];
        let len_part = &s[idx + 1..];
        let ones: i32 = len_part.trim().parse().unwrap_or(0);
        match addr_part.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => {
                if !(0..=32).contains(&ones) {
                    return Err(ones);
                }
                let bits = u32::from(v4);
                let masked = if ones == 0 { 0 } else { bits & (!0u32 << (32 - ones)) };
                if masked != bits {
                    return Err(-1);
                }
                Ok(format!("{}/{}", Ipv4Addr::from(masked), ones))
            }
            Ok(IpAddr::V6(v6)) => {
                if !(0..=128).contains(&ones) {
                    return Err(ones);
                }
                let bits = u128::from(v6);
                let masked = if ones == 0 { 0 } else { bits & (!0u128 << (128 - ones)) };
                if masked != bits {
                    return Err(-1);
                }
                Ok(format!("{}/{}", Ipv6Addr::from(masked), ones))
            }
            Err(_) => Err(ones),
        }
    } else {
        match s.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => Ok(v4.to_string()),
            Ok(IpAddr::V6(v6)) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    Ok(v4.to_string())
                } else {
                    Ok(v6.to_string())
                }
            }
            Err(_) => Err(-1),
        }
    }
}

fn float32_half(f: f64) -> u16 {
    let bits = (f as f32).to_bits();
    let sign = ((bits >> 31) as u16) << 15;
    let exp = ((bits >> 23) & 0xff) as i32 - 127;
    let frac = bits & 0x7fffff;
    if exp > 15 {
        return sign | 0x7c00;
    }
    if exp < -24 {
        return sign;
    }
    if exp < -14 {
        let shift = (-exp - 14) as u32;
        let m = frac | 0x800000;
        let mut half = (m >> (13 + shift)) as u16;
        let rem = m & ((1u32 << (13 + shift)) - 1);
        let half_round = 1u32 << (12 + shift);
        if rem > half_round || (rem == half_round && half & 1 == 1) {
            half += 1;
        }
        return sign | half;
    }
    let mut half = ((exp + 15) as u16) << 10 | ((frac >> 13) as u16);
    let rem = frac & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && half & 1 == 1) {
        half += 1;
    }
    sign | half
}

fn float32_from_half(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mut frac = (h & 0x3ff) as u32;
    match exp {
        0 => {
            if frac == 0 {
                return f32::from_bits(sign);
            }
            let mut e = 127 - 15 + 1;
            while frac & 0x400 == 0 {
                frac <<= 1;
                e -= 1;
            }
            frac &= 0x3ff;
            f32::from_bits(sign | e << 23 | frac << 13)
        }
        0x1f => f32::from_bits(sign | 0x7f800000 | frac << 13),
        _ => f32::from_bits(sign | (exp + 127 - 15) << 23 | frac << 13),
    }
}

// ---------------------------------------------------------------------------
// Field indexing
// ---------------------------------------------------------------------------

impl IndexCtx {
    fn indexRawField(
        &mut self,
        path: &str,
        def: &mut Map<String, Value>,
        raw: &Value,
    ) -> Result<(), ApiError> {
        let typ = def
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("object")
            .to_string();
        let has_props = def
            .get("properties")
            .map(|p| p.is_object())
            .unwrap_or(false);
        if typ == "object" || typ == "nested" {
            return self.indexObject(path, def, raw);
        }
        if typ == "alias" {
            let reason = format!("Cannot write to a field alias [{}].", path);
            return Err(ApiError::new("mapper_parsing_exception", "failed to parse", 400)
                .caused_by(ApiError::new("illegal_argument_exception", &reason, 400)));
        }
        if typ == "flat_object" {
            if !raw.is_object() {
                return Err(self.field_value_error(
                    path,
                    &typ,
                    raw,
                    "illegal_argument_exception",
                    "malformed",
                ));
            }
            let mut leaves: Vec<(String, Value)> = Vec::new();
            flatten_flat_object("", raw, &mut leaves);
            for (rel, val) in leaves {
                self.values
                    .entry(format!("{}.{}", path, rel))
                    .or_default()
                    .push(val.clone());
                self.values
                    .entry(path.to_string())
                    .or_default()
                    .push(val);
            }
            return Ok(());
        }
        if typ == "constant_keyword" {
            if let Some(expected) = def.get("value").and_then(|v| value_scalar_string(v)) {
                for v in flatten_scalars(raw) {
                    if v.is_null() {
                        continue;
                    }
                    let got = value_scalar_string(&v).unwrap_or_else(|| preview_value(&v));
                    if got != expected {
                        return Err(self.field_value_error(
                            path,
                            &typ,
                            raw,
                            "illegal_argument_exception",
                            &format!(
                                "constant keyword field [{}] must have a value of [{}]",
                                path, expected
                            ),
                        ));
                    }
                }
            }
        }
        if typ == "rank_feature" && raw.is_array() {
            return Err(self.field_value_error(
                path,
                &typ,
                raw,
                "illegal_argument_exception",
                &format!(
                    "[rank_feature] fields do not support indexing multiple values for the same field [{}] in the same document",
                    path
                ),
            ));
        }
        let values = if typ == "geo_point" {
            flatten_geo_points(raw)
        } else {
            flatten_scalars(raw)
        };
        let mut out: Vec<Value> = Vec::new();
        let mut ignore_malformed = self.index_ignore_mal;
        if let Some(b) = def.get("ignore_malformed").and_then(|v| v.as_bool()) {
            ignore_malformed = b;
        }
        let null_value = def.get("null_value").cloned();
        let def_snapshot = def.clone();
        for v in &values {
            if v.is_null() {
                if let Some(nv) = &null_value {
                    let cv = self.coerce(path, &def_snapshot, &typ, nv)?;
                    if !cv.is_null() {
                        out.push(cv);
                    }
                }
                continue;
            }
            if typ == "object" || typ == "nested" {
                continue;
            }
            match self.coerce(path, &def_snapshot, &typ, v) {
                Ok(nv) => {
                    if !nv.is_null() {
                        if matches!(typ.as_str(), "keyword" | "constant_keyword" | "wildcard") {
                            if let Some(limit) = def_snapshot
                                .get("ignore_above")
                                .and_then(|x| x.as_i64())
                            {
                                if let Some(sv) = nv.as_str() {
                                    if limit >= 0 && sv.chars().count() as i64 > limit {
                                        continue;
                                    }
                                }
                            }
                        }
                        out.push(nv);
                    }
                }
                Err(e) => {
                    if ignore_malformed {
                        if !v.is_object() {
                            self.addIgnored(path);
                        }
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        if !out.is_empty() || null_value.is_some() {
            let entry = self.values.entry(path.to_string()).or_default();
            entry.extend(out.iter().cloned());
        }
        if typ == "text" {
            let mut prefixes: Vec<Value> = Vec::new();
            if let Some(ip) = def.get("index_prefixes").and_then(|v| v.as_object()) {
                let min_chars = ip.get("min_chars").and_then(|v| v.as_i64()).unwrap_or(2) as usize;
                let max_chars = ip.get("max_chars").and_then(|v| v.as_i64()).unwrap_or(5) as usize;
                for v in &out {
                    let sv = match value_scalar_string(v) {
                        Some(x) => x,
                        None => continue,
                    };
                    for tok in analyze(&sv) {
                        let chars: Vec<char> = tok.chars().collect();
                        for n in min_chars..=max_chars.min(chars.len()) {
                            prefixes.push(Value::String(chars[..n].iter().collect()));
                        }
                    }
                }
            }
            if !prefixes.is_empty() {
                self.values
                    .entry(format!("{}._index_prefix", path))
                    .or_default()
                    .extend(prefixes);
            }
            if def.get("index_phrases").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mut phrases: Vec<Value> = Vec::new();
                for v in &out {
                    let sv = match value_scalar_string(v) {
                        Some(x) => x,
                        None => continue,
                    };
                    let toks = analyze(&sv);
                    for w in toks.windows(2) {
                        phrases.push(Value::String(w.join(" ")));
                    }
                }
                if !phrases.is_empty() {
                    self.values
                        .entry(format!("{}._index_phrase", path))
                        .or_default()
                        .extend(phrases);
                }
            }
        }
        if typ == "search_as_you_type" {
            let max_shingle = def
                .get("max_shingle_size")
                .and_then(|x| x.as_i64())
                .unwrap_or(3)
                .clamp(2, 4) as usize;
            let mut grams: Vec<(String, Vec<Value>)> = Vec::new();
            for n in 2..=max_shingle {
                grams.push((format!("{}._{}gram", path, n), Vec::new()));
            }
            let mut prefixes: Vec<Value> = Vec::new();
            for v in &out {
                let sv = match value_scalar_string(v) {
                    Some(x) => x,
                    None => continue,
                };
                let toks = analyze(&sv);
                for n in 2..=max_shingle {
                    if toks.len() >= n {
                        for w in toks.windows(n) {
                            let g = w.join(" ");
                            if let Some((_, list)) = grams.iter_mut().find(|(k, _)| k.ends_with(&format!("_{}gram", n))) {
                                list.push(Value::String(g));
                            }
                        }
                    }
                }
                for i in 0..toks.len() {
                    let joined = toks[i..].join(" ");
                    let chars: Vec<char> = joined.chars().collect();
                    for end in 1..=chars.len() {
                        prefixes.push(Value::String(chars[..end].iter().collect()));
                    }
                }
            }
            for (k, list) in grams {
                if !list.is_empty() {
                    self.values.entry(k).or_default().extend(list);
                }
            }
            if !prefixes.is_empty() {
                self.values
                    .entry(format!("{}._index_prefix", path))
                    .or_default()
                    .extend(prefixes);
            }
        }
        if let Some(fields) = def.get("fields").and_then(|f| f.as_object()) {
            for (fname, fraw) in fields {
                let fdef = match fraw.as_object() {
                    Some(d) => d.clone(),
                    None => continue,
                };
                let sub = format!("{}.{}", path, fname);
                let mut sub_vals: Vec<Value> = Vec::new();
                let sub_typ = fdef
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                for v in &values {
                    if v.is_null() {
                        continue;
                    }
                    let nv = self.coerce(&sub, &fdef, &sub_typ, v)?;
                    if !nv.is_null() {
                        if matches!(sub_typ.as_str(), "keyword" | "constant_keyword" | "wildcard") {
                            if let Some(limit) = fdef.get("ignore_above").and_then(|x| x.as_i64()) {
                                if let Some(sv) = nv.as_str() {
                                    if limit >= 0 && sv.chars().count() as i64 > limit {
                                        continue;
                                    }
                                }
                            }
                        }
                        sub_vals.push(nv);
                    }
                }
                if !sub_vals.is_empty() {
                    let entry = self.values.entry(sub).or_default();
                    entry.extend(sub_vals);
                }
            }
        }
        Ok(())
    }

    pub fn copyTo(&mut self, target: &str, vals: &[Value]) {
        self.copy_targets.insert(target.to_string());
        let entry = self.values.entry(target.to_string()).or_default();
        entry.extend(vals.iter().cloned());
    }

    pub fn addIgnored(&mut self, path: &str) {
        if !self.ignored_set.contains(path) {
            self.ignored_set.insert(path.to_string());
            self.ignored.push(path.to_string());
        }
    }

    pub fn coerce(
        &self,
        path: &str,
        def: &Map<String, Value>,
        typ: &str,
        v: &Value,
    ) -> Result<Value, ApiError> {
        match typ {
            "text" | "match_only_text" | "search_as_you_type" | "flat_object" => {
                match value_scalar_string(v) {
                    Some(s) => Ok(Value::String(s)),
                    None => Err(self.field_value_error(
                        path,
                        typ,
                        v,
                        "illegal_state_exception",
                        "Can't get text on a START_OBJECT",
                    )),
                }
            }
            "keyword" | "constant_keyword" | "wildcard" => match value_scalar_string(v) {
                Some(mut s) => {
                    if let Some(norm) = def.get("normalizer").and_then(|v| v.as_str()) {
                        if !norm.is_empty() {
                            s = self.apply_norm(norm, &s);
                        }
                    }
                    Ok(Value::String(s))
                }
                None => Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "illegal_state_exception",
                    "Can't get text on a START_OBJECT",
                )),
            },
            "integer" | "long" | "short" | "byte" | "unsigned_long" => {
                self.parseIntValue(path, typ, def, v)
            }
            "float" | "double" | "half_float" | "scaled_float" => {
                self.parseFloatValue(path, typ, def, v)
            }
            "boolean" => self.parseBooleanValue(path, v),
            "date" | "date_nanos" => self.parseDateValue(path, typ, def, v),
            "ip" => self.parseIPValue(path, v),
            "binary" => match value_scalar_string(v) {
                Some(s) => {
                    let needs_check = def.get("store").and_then(|x| x.as_bool()) == Some(true)
                        || def.get("doc_values").and_then(|x| x.as_bool()) == Some(true);
                    let bad_b64 = s.len() % 4 != 0
                        || !s
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=');
                    if needs_check && bad_b64 {
                        return Err(self.field_value_error(
                            path,
                            typ,
                            v,
                            "json_parse_exception",
                            &format!("Failed to parse Base64 string: [{}]", s),
                        ));
                    }
                    Ok(Value::String(s))
                }
                None => Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "illegal_argument_exception",
                    "malformed",
                )),
            },
            "token_count" => self.parseTokenCount(path, def, v),
            "integer_range" | "float_range" | "long_range" | "double_range" | "date_range"
            | "ip_range" => self.parseRangeValue(path, typ, def, v),
            "geo_point" => self.parseGeoPoint(path, def, v),
            "geo_shape" => self.parseGeoShape(path, v),
            "completion" => self.parseCompletion(path, v),
            "rank_feature" => self.parseRankFeature(path, v),
            "rank_features" => self.parseRankFeatures(path, v),
            "constant_keyword_default" => Ok(Value::Null),
            "alias" => Ok(Value::Null),
            "join" => self.parseJoin(path, def, v),
            "percolator" => Ok(v.clone()),
            "derived" => Ok(Value::Null),
            _ => {
                if is_numeric_type_name(typ) {
                    return self.parseFloatValue(path, typ, def, v);
                }
                match value_scalar_string(v) {
                    Some(s) => Ok(Value::String(s)),
                    None => Err(self.field_value_error(
                        path,
                        typ,
                        v,
                        "illegal_argument_exception",
                        "malformed",
                    )),
                }
            }
        }
    }

    fn indexObject(
        &mut self,
        path: &str,
        def: &mut Map<String, Value>,
        raw: &Value,
    ) -> Result<(), ApiError> {
        let saved = self.dyn_setting.clone();
        if let Some(dv) = def.get("dynamic") {
            self.dyn_setting = match dv {
                Value::Bool(true) => "true".to_string(),
                Value::Bool(false) => "false".to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
        let result = self.indexObjectInner(path, def, raw);
        self.dyn_setting = saved;
        result
    }

    fn indexObjectInner(
        &mut self,
        path: &str,
        def: &mut Map<String, Value>,
        raw: &Value,
    ) -> Result<(), ApiError> {
        let enabled = def.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        if !enabled {
            return Ok(());
        }
        if raw.is_null() {
            return Ok(());
        }
        match raw {
            Value::Array(a) => {
                for e in a {
                    self.indexObject(path, def, e)?;
                }
                Ok(())
            }
            Value::Object(m) => {
                if m.is_empty() {
                    return Ok(());
                }
                let props = def
                    .entry("properties".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !props.is_object() {
                    *props = Value::Object(Map::new());
                }
                let props = props.as_object_mut().unwrap();
                self.indexObjectMap(path, props, m)
            }
            _ => Err(ApiError::new(
                "mapper_parsing_exception",
                &format!(
                    "object mapping for [{}] tried to parse field [{}] as object, but found a concrete value",
                    path, path
                ),
                400,
            )),
        }
    }

    fn indexObjectMap(
        &mut self,
        path: &str,
        props: &mut Map<String, Value>,
        src: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        for (raw_k, v) in src {
            let k_owned = raw_k.trim_end_matches('.').to_string();
            let k = if k_owned.is_empty() { raw_k.as_str() } else { k_owned.as_str() };
            if matches!(
                k,
                "_id" | "_index" | "_source" | "_routing" | "_field_names" | "_ignored"
            ) {
                self.rejectMetadataField(k)?;
                continue;
            }
            let child_path = if path.is_empty() {
                k.to_string()
            } else {
                format!("{}.{}", path, k)
            };
            let declared = props.get(k).map(|d| d.is_object()).unwrap_or(false);
            if !declared {
                let (template, matched) = self.dynamic_def_for(&child_path, k, v)?;
                let allow = match self.dyn_setting.as_str() {
                    "false" => false,
                    "strict" => false,
                    "strict_allow_templates" => matched,
                    "false_allow_templates" => matched,
                    _ => true,
                };
                if !allow {
                    if self.dyn_setting == "false" || self.dyn_setting == "false_allow_templates" {
                        continue;
                    }
                    let name = if path.is_empty() { "_doc" } else { path };
                    return Err(ApiError::new(
                        "strict_dynamic_mapping_exception",
                        &format!(
                            "mapping set to {}, dynamic introduction of [{}] within [{}] is not allowed",
                            self.dyn_setting, k, name
                        ),
                        400,
                    ));
                }
                match template {
                    Some(d) => {
                        if k.contains('.') {
                            let parts: Vec<&str> = k.split('.').collect();
                            insert_dynamic_prop(props, &parts, Value::Object(d)).map_err(|reason| {
                                ApiError::new("illegal_argument_exception", &reason, 400)
                            })?;
                        } else {
                            props.insert(k.to_string(), Value::Object(d));
                        }
                    }
                    None => continue,
                }
            }
            let mut def = if k.contains('.') {
                dotted_prop(props, k).cloned().unwrap_or_default()
            } else {
                props
                    .get(k)
                    .and_then(|d| d.as_object())
                    .cloned()
                    .unwrap()
            };
            let pre_len = self
                .values
                .get(&child_path)
                .map(|v| v.len())
                .unwrap_or(0);
            self.indexRawField(&child_path, &mut def, v)?;
            let all_vals = self.values.get(&child_path).cloned().unwrap_or_default();
            let fresh: Vec<Value> = all_vals.iter().skip(pre_len).cloned().collect();
            if !k.contains('.') && def.get("properties").is_some() {
                props.insert(k.to_string(), Value::Object(def.clone()));
            }
            let copy_targets: Vec<String> = match def.get("copy_to") {
                Some(Value::String(t)) => vec![t.clone()],
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => vec![],
            };
            if let Some(slot) = props.get_mut(k).and_then(|d| d.as_object_mut()) {
                *slot = def;
            }
            for t in copy_targets {
                if !props.contains_key(&t) {
                    if self.dyn_setting == "strict" || self.dyn_setting == "strict_allow_templates" {
                        return Err(ApiError::new(
                            "strict_dynamic_mapping_exception",
                            &format!(
                                "mapping set to strict, dynamic introduction of [{}] within [_doc] is not allowed",
                                t
                            ),
                            400,
                        ));
                    }
                    props.insert(t.clone(), json!({"type": "text"}));
                }
                if !fresh.is_empty() {
                    self.copyTo(&t, &fresh);
                }
            }
        }
        Ok(())
    }

    fn dynamic_def_for(
        &self,
        path: &str,
        name: &str,
        raw: &Value,
    ) -> Result<(Option<Map<String, Value>>, bool), ApiError> {
        if let Some(arr) = raw.as_array() {
            let mut first: Option<(&Value, &'static str)> = None;
            for el in arr {
                if el.is_null() {
                    continue;
                }
                let inf = match self.infer_dynamic_value(el) {
                    Some(v) => v,
                    None => continue,
                };
                let kind = mapping_type_placeholder(&inf);
                let is_obj = inf.get("properties").is_some()
                    || matches!(kind, "object");
                match first {
                    None => first = Some((el, if is_obj { "object" } else { kind })),
                    Some((_, fk)) => {
                        if is_obj != (fk == "object") {
                            return Err(ApiError::new(
                                "illegal_argument_exception",
                                &format!(
                                    "can't merge a non object mapping [{}] with an object mapping",
                                    name
                                ),
                                400,
                            ));
                        }
                        if !is_obj && kind != fk {
                            let from = if fk == "string" { "text" } else { fk };
                            let to = if kind == "string" { "text" } else { kind };
                            return Err(ApiError::new(
                                "illegal_argument_exception",
                                &format!(
                                    "mapper [{}] cannot be changed from type [{}] to [{}]",
                                    name, from, to
                                ),
                                400,
                            ));
                        }
                    }
                }
            }
        }
        let inferred = match self.infer_dynamic_value(raw) {
            Some(v) => v,
            None => return Ok((None, false)),
        };
        let dyn_type = dynamic_type_name(&inferred);
        let placeholder_type = mapping_type_placeholder(&inferred);
        let template = self.match_dynamic_template(path, name, &dyn_type);
        let matched = template.is_some();
        let has_dyn_placeholder = template
            .as_ref()
            .map(|m| contains_dynamic_type_placeholder(&Value::Object(m.clone())))
            .unwrap_or(false);
        let mut def = match template {
            Some(mut m) => {
                for v in m.values_mut() {
                    substitute_placeholders(v, name, &placeholder_type);
                }
                if !m.contains_key("type") {
                    if let Some(base) = inferred.as_object() {
                        for (k, v) in base {
                            m.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
                m
            }
            None => inferred.as_object().cloned().unwrap_or_default(),
        };
        if def.get("type").and_then(|v| v.as_str()) == Some("text") && has_dyn_placeholder {
            def.remove("doc_values");
        }
        if let Some(Value::String(a)) = def.get("analyzer").cloned() {
            if !builtin_analyzer(&a) && !self.analyzers.contains(&a) {
                let reason = format!("analyzer [{}] has not been configured in mappings", a);
                return Err(mapper_err(
                    "mapper_parsing_exception",
                    &reason,
                    "illegal_argument_exception",
                    &reason,
                ));
            }
        }
        if let Some(Value::String(t)) = def.get("copy_to").cloned() {
            def.insert("copy_to".to_string(), json!([t]));
        }
        let typ = def.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if raw.is_object() && (typ == "object" || typ == "nested" || typ.is_empty()) {
            let mut props = Map::new();
            if let Some(m) = raw.as_object() {
                for (ck, cv) in m {
                    let cp = if path.is_empty() {
                        ck.clone()
                    } else {
                        format!("{}.{}", path, ck)
                    };
                    if let Some(cd) = self.dynamic_def_for(&cp, ck, cv)?.0 {
                        props.insert(ck.clone(), Value::Object(cd));
                    }
                }
            }
            if (typ == "object" || typ == "nested") && !props.is_empty() {
                def.insert("properties".to_string(), Value::Object(props));
            } else if !props.is_empty() {
                def.insert("properties".to_string(), Value::Object(props));
            }
        }
        Ok((Some(def), matched))
    }

    fn infer_dynamic_value(&self, v: &Value) -> Option<Value> {
        match v {
            Value::Bool(_) => Some(json!({"type": "boolean"})),
            Value::Number(n) => {
                let f = n.as_f64()?;
                if f.fract() == 0.0 {
                    Some(json!({"type": "long"}))
                } else {
                    Some(json!({"type": "float"}))
                }
            }
            Value::String(s) => Some(self.infer_dynamic_string(s)),
            Value::Array(a) => a.iter().find_map(|e| {
                if e.is_null() {
                    None
                } else {
                    self.infer_dynamic_value(e)
                }
            }),
            Value::Object(m) => {
                let mut props = Map::new();
                for (k, val) in m {
                    if let Some(d) = self.dynamic_def_for("", k, val).ok().and_then(|(d, _)| d) {
                        props.insert(k.clone(), Value::Object(d));
                    }
                }
                if props.is_empty() {
                    Some(json!({"type": "object"}))
                } else {
                    Some(json!({"properties": Value::Object(props)}))
                }
            }
            _ => None,
        }
    }

    fn infer_dynamic_string(&self, s: &str) -> Value {
        if self.numeric_detection {
            if s.parse::<i64>().is_ok() {
                return json!({"type": "long"});
            }
            if s.parse::<f64>().is_ok() {
                return json!({"type": "float"});
            }
        }
        if self.date_detection {
            if !self.dynamic_date_formats.is_empty() {
                for fmt in &self.dynamic_date_formats {
                    if date_format_matches(fmt, s) {
                        let print = fmt.split("||").next().unwrap_or(fmt);
                        return json!({"type": "date", "format": fmt, "print_format": print});
                    }
                }
            } else if is_iso_date_like(s) {
                return json!({"type": "date"});
            } else if is_slash_date_like(s) {
                return json!({
                    "type": "date",
                    "format": "yyyy/MM/dd HH:mm:ss||yyyy/MM/dd||epoch_millis",
                    "print_format": "yyyy/MM/dd HH:mm:ss"
                });
            }
        }
        json!({
            "type": "text",
            "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}
        })
    }

    fn match_dynamic_template(
        &self,
        path: &str,
        name: &str,
        dyn_type: &str,
    ) -> Option<Map<String, Value>> {
        for t in &self.dynamic_templates {
            let obj = match t.as_object() {
                Some(o) => o,
                None => continue,
            };
            let spec = match obj.values().next().and_then(|v| v.as_object()) {
                Some(s) => s,
                None => continue,
            };
            let regex = spec
                .get("match_pattern")
                .and_then(|v| v.as_str())
                .map(|v| v == "regex")
                .unwrap_or(false);
            if let Some(p) = spec.get("match").and_then(|v| v.as_str()) {
                let hit = if regex {
                    regex::Regex::new(p).map(|r| r.is_match(name)).unwrap_or(false)
                } else {
                    crate::resp::glob_match(p, name)
                };
                if !hit {
                    continue;
                }
            }
            if let Some(p) = spec.get("unmatch").and_then(|v| v.as_str()) {
                let hit = if regex {
                    regex::Regex::new(p).map(|r| r.is_match(name)).unwrap_or(false)
                } else {
                    crate::resp::glob_match(p, name)
                };
                if hit {
                    continue;
                }
            }
            if let Some(p) = spec.get("path_match").and_then(|v| v.as_str()) {
                if !crate::resp::glob_match(p, path) {
                    continue;
                }
            }
            if let Some(p) = spec.get("path_unmatch").and_then(|v| v.as_str()) {
                if crate::resp::glob_match(p, path) {
                    continue;
                }
            }
            if let Some(mt) = spec.get("match_mapping_type").and_then(|v| v.as_str()) {
                if mt != dyn_type {
                    continue;
                }
            }
            if let Some(m) = spec.get("mapping").and_then(|v| v.as_object()) {
                return Some(m.clone());
            }
        }
        None
    }

    fn rejectMetadataField(&self, name: &str) -> Result<(), ApiError> {
        let reason = format!(
            "Field [{}] is a metadata field and cannot be added inside a document. Use the index API request parameters.",
            name
        );
        Err(mapper_err(
            "mapper_parsing_exception",
            &reason,
            "mapper_parsing_exception",
            &reason,
        ))
    }

    fn fillConstants(&mut self, props: &Map<String, Value>, src: &Map<String, Value>) {
        for (name, raw) in props {
            let def = match raw.as_object() {
                Some(d) => d,
                None => continue,
            };
            if def.get("type").and_then(|t| t.as_str()) == Some("constant_keyword") {
                if !src.contains_key(name) {
                    if let Some(v) = def.get("value") {
                        self.values
                            .insert(name.clone(), vec![Value::String(preview_value(v))]);
                    }
                }
            }
        }
    }
}

fn flatten_flat_object(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                flatten_flat_object(&p, child, out);
            }
        }
        Value::Array(a) => {
            for child in a {
                flatten_flat_object(prefix, child, out);
            }
        }
        Value::Null => {}
        other => {
            if let Some(s) = value_scalar_string(other) {
                out.push((prefix.to_string(), Value::String(s)));
            }
        }
    }
}

pub fn flat_object_root(idx: &Index, field: &str) -> Option<String> {
    let parts: Vec<&str> = field.split('.').collect();
    for i in (1..parts.len()).rev() {
        let prefix = parts[..i].join(".");
        if let Some(def) = crate::eval::mapping_field_def(idx, &prefix) {
            if def.get("type").and_then(|t| t.as_str()) == Some("flat_object") {
                return Some(prefix);
            }
        }
    }
    None
}

fn mapping_type_placeholder(def: &Value) -> &'static str {
    match def.get("type").and_then(|v| v.as_str()) {
        Some("text") => "text",
        Some("keyword") => "keyword",
        Some("boolean") => "boolean",
        Some("long") => "long",
        Some("integer") => "long",
        Some("float") | Some("double") => "float",
        Some("date") => "date",
        Some("object") | Some("nested") => "object",
        _ => {
            if def.get("properties").is_some() {
                "object"
            } else {
                "string"
            }
        }
    }
}

fn dynamic_type_name(def: &Value) -> &'static str {
    match def.get("type").and_then(|v| v.as_str()) {
        Some("boolean") => "boolean",
        Some("long") => "long",
        Some("integer") => "long",
        Some("float") | Some("double") => "float",
        Some("date") => "date",
        Some("object") => "object",
        Some("nested") => "object",
        _ => {
            if def.get("properties").is_some() {
                "object"
            } else {
                "string"
            }
        }
    }
}

fn contains_dynamic_type_placeholder(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("{dynamic_type}"),
        Value::Array(a) => a.iter().any(contains_dynamic_type_placeholder),
        Value::Object(m) => m.values().any(contains_dynamic_type_placeholder),
        _ => false,
    }
}

fn substitute_placeholders(v: &mut Value, name: &str, dyn_type: &str) {
    match v {
        Value::String(s) => {
            if s.contains("{name}") {
                *s = s.replace("{name}", name);
            }
            if s.contains("{dynamic_type}") {
                *s = s.replace("{dynamic_type}", dyn_type);
            }
        }
        Value::Array(a) => {
            for e in a {
                substitute_placeholders(e, name, dyn_type);
            }
        }
        Value::Object(m) => {
            for e in m.values_mut() {
                substitute_placeholders(e, name, dyn_type);
            }
        }
        _ => {}
    }
}

fn builtin_analyzer(name: &str) -> bool {
    matches!(
        name,
        "standard"
            | "simple"
            | "whitespace"
            | "stop"
            | "keyword"
            | "pattern"
            | "fingerprint"
            | "english"
            | "french"
            | "german"
            | "spanish"
            | "italian"
            | "portuguese"
            | "dutch"
            | "russian"
            | "arabic"
            | "cjk"
            | "chinese"
            | "thai"
            | "turkish"
            | "swedish"
            | "norwegian"
            | "danish"
            | "finnish"
            | "hungarian"
            | "romanian"
            | "czech"
            | "greek"
            | "hindi"
            | "indonesian"
            | "latvian"
            | "lithuanian"
            | "persian"
            | "vietnamese"
            | "irish"
            | "catalan"
            | "basque"
            | "galician"
            | "sorani"
            | "estonian"
            | "bulgarian"
            | "serbian"
            | "slovak"
            | "slovenian"
            | "ukrainian"
    )
}

fn is_iso_date_like(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 7 || b.len() > 30 {
        return false;
    }
    if !(b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[5..7].iter().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    if b.len() == 7 {
        return true;
    }
    if b[7] != b'-' || b.len() < 10 || !b[8..10].iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if b.len() == 10 {
        return true;
    }
    if b[10] != b'T' {
        return false;
    }
    let rest = &s[11..];
    rest.chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | 'Z' | '+' | '-'))
}

fn is_slash_date_like(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 8 {
        return false;
    }
    b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'/'
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[7] == b'/'
        && b.len() > 8
}

fn date_format_matches(fmt: &str, s: &str) -> bool {
    for alt in fmt.split("||") {
        let mut pat = String::from("^");
        let mut chars = alt.chars().peekable();
        while let Some(c) = chars.next() {
            let mut tok = String::new();
            tok.push(c);
            if c.is_ascii_alphabetic() {
                while let Some(n) = chars.peek() {
                    if n.is_ascii_alphabetic() {
                        tok.push(*n);
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            match tok.as_str() {
                "yyyy" => pat.push_str("\\d{4}"),
                "MM" | "dd" | "HH" | "mm" | "ss" => pat.push_str("\\d{2}"),
                "epoch_millis" => pat.push_str("\\d+"),
                "epoch_second" => pat.push_str("\\d+"),
                "T" => pat.push('T'),
                other => {
                    for ch in other.chars() {
                        if "\\.+*?()|[]{}^$".contains(ch) {
                            pat.push('\\');
                        }
                        pat.push(ch);
                    }
                }
            }
        }
        pat.push('$');
        if let Ok(re) = regex::Regex::new(&pat) {
            if re.is_match(s) {
                return true;
            }
        }
    }
    false
}

pub fn is_numeric_type_name(t: &str) -> bool {
    matches!(
        t,
        "integer" | "long" | "short" | "byte" | "float" | "double" | "half_float"
            | "scaled_float" | "unsigned_long"
    )
}

pub fn value_scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(number_string(n)),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn flatten_geo_points(raw: &Value) -> Vec<Value> {
    match raw {
        Value::Array(a) => {
            if a.len() == 2 || a.len() == 3 {
                let numlike = |v: &Value| {
                    v.is_number()
                        || v.as_str()
                            .map(|s| s.trim().parse::<f64>().is_ok())
                            .unwrap_or(false)
                };
                if a.iter().all(numlike) {
                    return vec![raw.clone()];
                }
            }
            let mut out = Vec::new();
            for e in a {
                out.extend(flatten_geo_points(e));
            }
            out
        }
        other => vec![other.clone()],
    }
}

pub fn flatten_scalars(raw: &Value) -> Vec<Value> {
    match raw {
        Value::Array(a) => {
            let mut out = Vec::new();
            for e in a {
                out.extend(flatten_scalars(e));
            }
            out
        }
        other => vec![other.clone()],
    }
}

pub fn apply_normalizer(name: &str, s: &str) -> String {
    match name {
        "lowercase" => simple_lower(s),
        _ => s.to_string(),
    }
}

pub fn apply_normalizer_idx(idx: &Index, name: &str, s: &str) -> String {
    if name == "lowercase" {
        return simple_lower(s);
    }
    let key = format!("index.analysis.normalizer.{}.filter", name);
    let v = match find_setting(&idx.settings, &key) {
        Some(v) => v,
        None => return s.to_string(),
    };
    let mut filters: Vec<String> = Vec::new();
    match v {
        Value::Array(a) => {
            for x in a {
                if let Some(f) = value_to_string(x) {
                    filters.push(f);
                }
            }
        }
        Value::String(sv) => {
            if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(sv) {
                for x in &a {
                    if let Some(f) = value_to_string(x) {
                        filters.push(f);
                    }
                }
            } else {
                filters.push(sv.clone());
            }
        }
        _ => {}
    }
    let mut out = s.to_string();
    for f in filters {
        match f.as_str() {
            "lowercase" => out = simple_lower(&out),
            "uppercase" => out = out.to_uppercase(),
            "asciifolding" => out = asciifold(&out),
            "trim" => out = out.trim().to_string(),
            _ => {}
        }
    }
    out
}

pub fn normalize_query_value(idx: &Index, field: &str, v: &Value) -> Value {
    let def = match crate::eval::mapping_field_def(idx, field) {
        Some(d) => d,
        None => return v.clone(),
    };
    let norm = match def.get("normalizer").and_then(|x| x.as_str()) {
        Some(n) if !n.is_empty() => n,
        _ => return v.clone(),
    };
    match v {
        Value::String(s) => Value::String(apply_normalizer_idx(idx, norm, s)),
        _ => v.clone(),
    }
}

impl IndexCtx {
    fn apply_norm(&self, name: &str, s: &str) -> String {
        if name == "lowercase" {
            return simple_lower(s);
        }
        let filters = match self.normalizer_filters.get(name) {
            Some(f) => f,
            None => return s.to_string(),
        };
        let mut out = s.to_string();
        for f in filters {
            match f.as_str() {
                "lowercase" => out = simple_lower(&out),
                "uppercase" => out = out.to_uppercase(),
                "asciifolding" => out = asciifold(&out),
                "trim" => out = out.trim().to_string(),
                _ => {}
            }
        }
        out
    }

    fn parseTokenCount(
        &self,
        path: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        match value_scalar_string(v) {
            Some(s) => {
                let analyzer = def
                    .get("analyzer")
                    .and_then(|a| a.as_str())
                    .unwrap_or("");
                let toks = if analyzer == "stop" {
                    let stops = [
                        "the", "a", "an", "and", "or", "of", "to", "in", "is", "are",
                        "was", "were", "be", "been", "being", "for", "on", "with", "as",
                        "at", "by", "from", "it", "this", "that", "these", "those",
                    ];
                    analyze(&s)
                        .into_iter()
                        .filter(|t| !stops.contains(&t.as_str()))
                        .collect::<Vec<_>>()
                } else {
                    analyze(&s)
                };
                let epi = def
                    .get("enable_position_increments")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let n = if analyzer == "stop" && epi {
                    analyze(&s).len()
                } else {
                    toks.len()
                };
                Ok(num_i64(n as i64))
            }
            None => Err(self.field_value_error(
                path,
                "token_count",
                v,
                "illegal_argument_exception",
                "malformed",
            )),
        }
    }

    fn parseGeoPoint(
        &self,
        path: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        let ignore_z = def
            .get("ignore_z_value")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let ignore_mal = def
            .get("ignore_malformed")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let err = |cause: &str, reason: &str| {
            mapper_err(
                "mapper_parsing_exception",
                &format!("failed to parse field [{}] of type [geo_point]", path),
                cause,
                reason,
            )
        };
        let normalize = |lat: f64, lon: f64| {
            if ignore_mal {
                let mut la = lat;
                let mut lo = lon;
                while la > 90.0 {
                    la = 180.0 - la;
                    lo += 180.0;
                }
                while la < -90.0 {
                    la = -180.0 - la;
                    lo += 180.0;
                }
                let mut lo = lo % 360.0;
                if lo > 180.0 {
                    lo -= 360.0;
                } else if lo < -180.0 {
                    lo += 360.0;
                }
                (la, lo)
            } else {
                (lat, lon)
            }
        };
        let check = |lat: f64, lon: f64, lon_first: bool| -> Result<Value, ApiError> {
            let (lat, lon) = normalize(lat, lon);
            if !ignore_mal && (lat > 90.0 || lat < -90.0) {
                return Err(err(
                    "illegal_argument_exception",
                    &format!("illegal latitude value [{:.1}] for {}", lat, path),
                ));
            }
            if !ignore_mal && (lon > 180.0 || lon < -180.0) {
                return Err(err(
                    "illegal_argument_exception",
                    &format!("illegal longitude value [{:.1}] for {}", lon, path),
                ));
            }
            let _ = lon_first;
            Ok(json!({"lat": lat, "lon": lon}))
        };
        match v {
            Value::String(s) => {
                let t = s.trim();
                let up = t.to_uppercase();
                if up.starts_with("POINT") {
                    let inner = t
                        .split_once('(')
                        .and_then(|(_, r)| r.rsplit_once(')'))
                        .map(|(l, _)| l)
                        .unwrap_or(t);
                    let nums: Vec<f64> = inner
                        .split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect();
                    if nums.len() >= 3 && !ignore_z {
                        return Err(err("parse_exception", "Invalid WKT format"));
                    }
                    if nums.len() >= 2 {
                        return check(nums[1], nums[0], true);
                    }
                    return Err(err("parse_exception", "Invalid WKT format"));
                }
                let parts: Vec<&str> = t.split(',').collect();
                if parts.len() >= 2 {
                    let nums: Vec<f64> = parts
                        .iter()
                        .filter_map(|p| p.trim().parse::<f64>().ok())
                        .collect();
                    if nums.len() >= 2 && nums.len() == parts.len() {
                        if nums.len() >= 3 && !ignore_z {
                            return Err(err(
                                "parse_exception",
                                &format!(
                                    "Exception parsing coordinates: found Z value [{:.1}] but [ignore_z_value] parameter is [false]",
                                    nums[2]
                                ),
                            ));
                        }
                        return check(nums[0], nums[1], false);
                    }
                }
                if let Some((lon, lat)) = geohash_decode(t) {
                    return check(lat, lon, false);
                }
                let bad = t
                    .chars()
                    .find(|c| !"0123456789bcdefghjkmnpqrstuvwxyzBCDEFGHJKMNPQRSTUVWXYZ".contains(*c));
                let ch = bad.unwrap_or(' ');
                Err(err(
                    "parse_exception",
                    &format!("unsupported symbol [{}] in geohash [{}]", ch, t),
                ))
            }
            Value::Object(m) => {
                if m.contains_key("lat") || m.contains_key("lon") {
                    let keys: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
                    if keys.iter().any(|k| *k != "lat" && *k != "lon") {
                        return Err(err(
                            "parse_exception",
                            "field must be either [lon|lat], [type|coordinates], or [geohash]",
                        ));
                    }
                    let lat = match m.get("lat").and_then(to_f64) {
                        Some(v) => v,
                        None => return Err(err("parse_exception", "field [lat] missing")),
                    };
                    let lon = match m.get("lon").and_then(to_f64) {
                        Some(v) => v,
                        None => return Err(err("parse_exception", "field [lon] missing")),
                    };
                    return check(lat, lon, false);
                }
                if m.get("type").and_then(|t| t.as_str()).map(|t| t.eq_ignore_ascii_case("point")).unwrap_or(false) {
                    if let Some(coords) = m.get("coordinates").and_then(|c| c.as_array()) {
                        if coords.len() >= 2 {
                            if let (Some(lon), Some(lat)) = (to_f64(&coords[0]), to_f64(&coords[1])) {
                                return check(lat, lon, true);
                            }
                        }
                    }
                }
                Err(err(
                    "parse_exception",
                    "field must be either [lon|lat], [type|coordinates], or [geohash]",
                ))
            }
            Value::Array(a) => {
                if a.len() >= 3 && !ignore_z {
                    let z = to_f64(&a[2]).unwrap_or(0.0);
                    return Err(err(
                        "parse_exception",
                        &format!(
                            "Exception parsing coordinates: found Z value [{:.1}] but [ignore_z_value] parameter is [false]",
                            z
                        ),
                    ));
                }
                if a.len() >= 2 {
                    if let (Some(lon), Some(lat)) = (to_f64(&a[0]), to_f64(&a[1])) {
                        return check(lat, lon, true);
                    }
                }
                Err(err(
                    "parse_exception",
                    "field must be either [lon|lat], [type|coordinates], or [geohash]",
                ))
            }
            _ => Err(err("parse_exception", "failed to parse geo_point")),
        }
    }

    fn parseGeoShape(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        let err = |cause: &str, reason: &str| {
            mapper_err(
                "mapper_parsing_exception",
                &format!("failed to parse field [{}] of type [geo_shape]", path),
                cause,
                reason,
            )
        };
        match v {
            Value::String(s) => {
                let up = s.trim().to_uppercase();
                let known = [
                    "POINT",
                    "POLYGON",
                    "LINESTRING",
                    "MULTIPOINT",
                    "MULTILINESTRING",
                    "MULTIPOLYGON",
                    "BBOX",
                    "ENVELOPE",
                    "GEOMETRYCOLLECTION",
                ];
                if known.iter().any(|k| up.starts_with(k)) {
                    if crate::eval_extra::parse_wkt_shape(s).is_some() {
                        Ok(v.clone())
                    } else {
                        Err(err("parse_exception", "Invalid WKT format"))
                    }
                } else {
                    Err(err(
                        "illegal_argument_exception",
                        &format!("Unknown geometry type [{}]", s),
                    ))
                }
            }
            Value::Object(m) => {
                let typ = m
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if typ == "circle" {
                    return Err(err(
                        "unsupported_operation_exception",
                        "circle is not supported",
                    ));
                }
                let known = [
                    "point",
                    "multipoint",
                    "linestring",
                    "multilinestring",
                    "polygon",
                    "multipolygon",
                    "envelope",
                    "geometrycollection",
                ];
                if !known.contains(&typ.as_str()) {
                    return Err(err(
                        "x_content_parse_exception",
                        &format!("Unknown geometry type [{}]", typ),
                    ));
                }
                if typ == "polygon" {
                    if let Some(rings) = m.get("coordinates").and_then(|c| c.as_array()) {
                        for ring in rings {
                            let pts = match ring.as_array() {
                                Some(p) => p,
                                None => {
                                    return Err(err("x_content_parse_exception", "Invalid polygon"))
                                }
                            };
                            if pts.len() < 4 {
                                return Err(err(
                                    "x_content_parse_exception",
                                    "Invalid polygon: ring has fewer than 4 points",
                                ));
                            }
                            if pts.first() != pts.last() {
                                return Err(err(
                                    "x_content_parse_exception",
                                    "Invalid polygon: ring is not closed",
                                ));
                            }
                        }
                    } else {
                        return Err(err("x_content_parse_exception", "Invalid polygon"));
                    }
                }
                if typ == "geometrycollection"
                    && !m.get("geometries").map(|g| g.is_array()).unwrap_or(false)
                {
                    return Err(err(
                        "x_content_parse_exception",
                        "Invalid geometry collection",
                    ));
                }
                Ok(v.clone())
            }
            _ => Err(err("x_content_parse_exception", "Invalid shape")),
        }
    }

    fn parseCompletion(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        fn json_kind(v: &Value) -> &'static str {
            match v {
                Value::Null => "VALUE_NULL",
                Value::Bool(_) => "VALUE_BOOLEAN",
                Value::Number(_) => "VALUE_NUMBER",
                Value::String(_) => "VALUE_STRING",
                Value::Array(_) => "START_ARRAY",
                Value::Object(_) => "START_OBJECT",
            }
        }
        fn check_reserved(path: &str, s: &str) -> Result<(), ApiError> {
            let mut pos = 0usize;
            for ch in s.chars() {
                let cp = ch as u32;
                if cp < 0x20 || cp == 0x7f {
                    let mut e = ApiError::new(
                        "mapper_parsing_exception",
                        &format!(
                            "failed to parse field [{}] of type [completion]",
                            path
                        ),
                        400,
                    );
                    e.caused_by = Some(Box::new(ApiError::new(
                        "illegal_argument_exception",
                        &format!(
                            "Illegal input [{}] UTF-16 codepoint [0x{:x}] at position {} is a reserved character",
                            s, cp, pos
                        ),
                        400,
                    )));
                    return Err(e);
                }
                pos += ch.len_utf16();
            }
            Ok(())
        }
        fn parse_one(
            ctx: &IndexCtx,
            path: &str,
            v: &Value,
            out: &mut Vec<Value>,
        ) -> Result<(), ApiError> {
            match v {
                Value::String(s) => {
                    check_reserved(path, s)?;
                    out.push(Value::String(s.clone()));
                }
                Value::Object(m) => {
                    for k in m.keys() {
                        if !matches!(k.as_str(), "input" | "weight" | "contexts") {
                            return Err(ctx.field_value_error(
                                path,
                                "completion",
                                v,
                                "illegal_argument_exception",
                                &format!(
                                    "unknown field name [{}], must be one of [input, weight, contexts]",
                                    k
                                ),
                            ));
                        }
                    }
                    if let Some(w) = m.get("weight") {
                        let wi = match w {
                            Value::Number(n) => n.as_i64(),
                            Value::String(s) => s.parse::<i64>().ok().filter(|_| s.trim() == *s),
                            _ => None,
                        };
                        match wi {
                            Some(n) if (0..=i32::MAX as i64).contains(&n) => {}
                            Some(n) => {
                                return Err(ctx.field_value_error(
                                    path,
                                    "completion",
                                    v,
                                    "illegal_argument_exception",
                                    &format!(
                                        "weight must be in the interval [0..2147483647], but was [{}]",
                                        n
                                    ),
                                ))
                            }
                            None => {
                                return Err(ctx.field_value_error(
                                    path,
                                    "completion",
                                    v,
                                    "illegal_argument_exception",
                                    &format!(
                                        "weight must be an integer, but was [{}]",
                                        preview_value(w)
                                    ),
                                ))
                            }
                        }
                    }
                    if let Some(inp) = m.get("input") {
                        match inp {
                            Value::String(s) => {
                                check_reserved(path, s)?;
                                out.push(Value::String(s.clone()));
                            }
                            Value::Array(a) => {
                                for e in a {
                                    match e {
                                        Value::String(s) => {
                                            check_reserved(path, s)?;
                                            out.push(Value::String(s.clone()));
                                        }
                                        Value::Object(em) => {
                                            let iv = em
                                                .get("input")
                                                .cloned()
                                                .unwrap_or_else(|| e.clone());
                                            match &iv {
                                                Value::String(s) => {
                                                    check_reserved(path, s)?;
                                                    out.push(Value::String(s.clone()));
                                                }
                                                Value::Array(a2) => {
                                                    for e2 in a2 {
                                                        if let Some(s) = value_scalar_string(e2) {
                                                            check_reserved(path, &s)?;
                                                            out.push(Value::String(s));
                                                        }
                                                    }
                                                }
                                                other => {
                                                    return Err(ctx.field_value_error(
                                                        path,
                                                        "completion",
                                                        other,
                                                        "parsing_exception",
                                                        &format!(
                                                            "failed to parse [{}]: expected text or object, but got {}",
                                                            path,
                                                            json_kind(other)
                                                        ),
                                                    ))
                                                }
                                            }
                                        }
                                        other => {
                                            return Err(ctx.field_value_error(
                                                path,
                                                "completion",
                                                other,
                                                "parsing_exception",
                                                &format!(
                                                    "failed to parse [{}]: expected text or object, but got {}",
                                                    path,
                                                    json_kind(other)
                                                ),
                                            ))
                                        }
                                    }
                                }
                            }
                            other => {
                                return Err(ctx.field_value_error(
                                    path,
                                    "completion",
                                    other,
                                    "parsing_exception",
                                    &format!(
                                        "failed to parse [{}]: expected text or object, but got {}",
                                        path,
                                        json_kind(other)
                                    ),
                                ))
                            }
                        }
                    }
                    if out.is_empty() {
                        out.push(v.clone());
                    }
                }
                Value::Array(a) => {
                    for e in a {
                        parse_one(ctx, path, e, out)?;
                    }
                }
                other => {
                    return Err(ctx.field_value_error(
                        path,
                        "completion",
                        other,
                        "parsing_exception",
                        &format!(
                            "failed to parse [{}]: expected text or object, but got {}",
                            path,
                            json_kind(other)
                        ),
                    ))
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        parse_one(self, path, v, &mut out)?;
        Ok(Value::Array(out))
    }

    fn parseRankFeature(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        let f = to_f64(v).or_else(|| {
            value_scalar_string(v).and_then(|s| s.parse::<f64>().ok())
        });
        let f = match f {
            Some(f) => f,
            None => {
                return Err(self.field_value_error(
                    path,
                    "rank_feature",
                    v,
                    "illegal_argument_exception",
                    "failed to parse rank_feature",
                ))
            }
        };
        let ff = f as f32;
        if !ff.is_normal() || ff <= 0.0 {
            return Err(self.field_value_error(
                path,
                "rank_feature",
                v,
                "illegal_argument_exception",
                &format!(
                    "featureValue must be a positive normal float, got: {:?} for feature {}",
                    ff, path
                ),
            ));
        }
        Ok(num_f64(f))
    }

    fn parseRankFeatures(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        let m = match v.as_object() {
            Some(m) => m,
            None => {
                let kind = match v {
                    Value::Null => "VALUE_NULL",
                    Value::Bool(_) => "VALUE_BOOLEAN",
                    Value::Number(_) => "VALUE_NUMBER",
                    Value::String(_) => "VALUE_STRING",
                    Value::Array(_) => "START_ARRAY",
                    Value::Object(_) => "START_OBJECT",
                };
                return Err(self.field_value_error(
                    path,
                    "rank_features",
                    v,
                    "illegal_argument_exception",
                    &format!(
                        "[rank_features] fields must be json objects, expected a START_OBJECT but got: {}",
                        kind
                    ),
                ));
            }
        };
        for (k, fv) in m {
            let f = to_f64(fv).or_else(|| {
                value_scalar_string(fv).and_then(|s| s.parse::<f64>().ok())
            });
            let ff = f.map(|x| x as f32);
            match ff {
                Some(ff) if ff.is_normal() && ff > 0.0 => {}
                _ => {
                    return Err(self.field_value_error(
                        path,
                        "rank_features",
                        fv,
                        "illegal_argument_exception",
                        &format!(
                            "featureValue must be a positive normal float, got: {:?} for feature {} on field {}",
                            ff.unwrap_or(0.0),
                            k,
                            path
                        ),
                    ))
                }
            }
        }
        Ok(v.clone())
    }

    fn parseJoin(
        &self,
        path: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        let relations = def.get("relations").and_then(|r| r.as_object());
        match v {
            Value::String(s) => {
                if let Some(rel) = relations {
                    if rel.contains_key(s) {
                        return Ok(Value::String(s.clone()));
                    }
                }
                Err(self.field_value_error(
                    path,
                    "join",
                    v,
                    "illegal_argument_exception",
                    &format!("unknown join name [{}] for field [{}]", s, path),
                ))
            }
            Value::Object(m) => {
                let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let parent = m.get("parent").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    return Err(self.field_value_error(
                        path,
                        "join",
                        v,
                        "illegal_argument_exception",
                        "join name is missing",
                    ));
                }
                let mut valid = false;
                if let Some(rel) = relations {
                    for (parent, children) in rel {
                        if parent == name || children.as_str() == Some(name) {
                            valid = true;
                            break;
                        }
                        if let Some(list) = children.as_array() {
                            if list.iter().any(|e| e.as_str() == Some(name)) {
                                valid = true;
                                break;
                            }
                        }
                    }
                }
                if !valid {
                    return Err(self.field_value_error(
                        path,
                        "join",
                        v,
                        "illegal_argument_exception",
                        &format!("unknown join name [{}] for field [{}]", name, path),
                    ));
                }
                let mut child = false;
                if let Some(rel) = relations {
                    for (_p, children) in rel {
                        if children.as_str() == Some(name)
                            || children
                                .as_array()
                                .map(|l| l.iter().any(|e| e.as_str() == Some(name)))
                                .unwrap_or(false)
                        {
                            child = true;
                        }
                    }
                }
                if child && parent.is_empty() {
                    return Err(self.field_value_error(
                        path,
                        "join",
                        v,
                        "illegal_argument_exception",
                        &format!("[parent] is missing for join field [{}]", path),
                    ));
                }
                Ok(json!({"name": name, "parent": parent}))
            }
            _ => Err(self.field_value_error(
                path,
                "join",
                v,
                "illegal_argument_exception",
                "join name is missing",
            )),
        }
    }

    fn parseRangeValue(
        &self,
        path: &str,
        typ: &str,
        def: &Map<String, Value>,
        v: &Value,
    ) -> Result<Value, ApiError> {
        if let Value::String(s) = v {
            if typ == "ip_range" {
                if let Some((addr, plen_s)) = s.split_once('/') {
                    if let (Ok(ip), Ok(plen)) = (addr.parse::<std::net::IpAddr>(), plen_s.parse::<u32>()) {
                        match ip {
                            std::net::IpAddr::V4(v4) => {
                                if plen <= 32 {
                                    let bits = u32::from(v4);
                                    let start = if plen == 0 { 0 } else { bits & (!0u32 << (32 - plen)) };
                                    let end = if plen == 0 { u32::MAX } else { start | (!0u32 >> plen) };
                                    return Ok(json!({
                                        "gte": std::net::Ipv4Addr::from(start).to_string(),
                                        "lte": std::net::Ipv4Addr::from(end).to_string(),
                                    }));
                                }
                            }
                            std::net::IpAddr::V6(v6) => {
                                if plen <= 128 {
                                    let bits = u128::from(v6);
                                    let start = if plen == 0 { 0 } else { bits & (!0u128 << (128 - plen)) };
                                    let end = if plen == 0 { u128::MAX } else { start | (!0u128 >> plen) };
                                    return Ok(json!({
                                        "gte": std::net::Ipv6Addr::from(start).to_string(),
                                        "lte": std::net::Ipv6Addr::from(end).to_string(),
                                    }));
                                }
                            }
                        }
                    }
                }
                let vals = self.parseIPValue(path, v)?;
                return Ok(json!({"gte": vals, "lte": vals}));
            }
            return Err(self.field_value_error(
                path,
                typ,
                v,
                "mapper_parsing_exception",
                &format!(
                    "error parsing field [{}], expected an object but got {}",
                    path, s
                ),
            ));
        }
        let m = match v.as_object() {
            Some(m) => m,
            None => {
                return Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "mapper_parsing_exception",
                    &format!(
                        "error parsing field [{}], expected an object but got {}",
                        path, path
                    ),
                ))
            }
        };
        let has_gt = m.get("gt").map(|v| !v.is_null()).unwrap_or(false);
        let has_gte = m.get("gte").map(|v| !v.is_null()).unwrap_or(false);
        if has_gt && has_gte {
            return Err(self.field_value_error(
                path,
                typ,
                v,
                "mapper_parsing_exception",
                &format!("error parsing field [{}], invalid lower bound (gt/gte)", path),
            ));
        }
        let has_lt = m.get("lt").map(|v| !v.is_null()).unwrap_or(false);
        let has_lte = m.get("lte").map(|v| !v.is_null()).unwrap_or(false);
        if has_lt && has_lte {
            return Err(self.field_value_error(
                path,
                typ,
                v,
                "mapper_parsing_exception",
                &format!("error parsing field [{}], invalid upper bound (lt/lte)", path),
            ));
        }
        let mut out = Map::new();
        for key in ["gt", "gte", "lt", "lte"] {
            let raw = match m.get(key) {
                Some(r) => r,
                None => continue,
            };
            let pv = match typ {
                "date_range" => match self.parseDateValue(path, "date", def, raw) {
                    Ok(v) => v,
                    Err(_) => {
                        return Err(self.field_value_error(
                            path,
                            typ,
                            raw,
                            "parse_exception",
                            "failed to parse date field",
                        ))
                    }
                },
                "ip_range" => self.parseIPValue(path, raw)?,
                "float_range" | "double_range" => {
                    self.parseFloatValue(path, typ.trim_end_matches("_range"), def, raw)?
                }
                _ => self.parseIntValue(path, typ.trim_end_matches("_range"), def, raw)?,
            };
            out.insert(key.to_string(), pv);
        }
        for k in m.keys() {
            if k != "gt" && k != "gte" && k != "lt" && k != "lte" {
                return Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "mapper_parsing_exception",
                    &format!("error parsing field [{}], with unknown parameter [{}]", path, k),
                ));
            }
        }
        let lower = out.get("gte").or_else(|| out.get("gt"));
        let upper = out.get("lte").or_else(|| out.get("lt"));
        if let (Some(l), Some(u)) = (lower, upper) {
            if compare_values_for_type(typ, l, u) > 0 {
                return Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "illegal_argument_exception",
                    &format!(
                        "min value ({}) is greater than max value ({})",
                        bounds_value(l),
                        bounds_value(u)
                    ),
                ));
            }
        }
        Ok(Value::Object(out))
    }
}

fn bounds_value(v: &Value) -> String {
    match v {
        Value::Number(_) => value_to_string(v).unwrap_or_default(),
        Value::String(s) => s.clone(),
        other => preview_value(other),
    }
}

fn compare_values_for_type(typ: &str, a: &Value, b: &Value) -> i32 {
    if typ == "ip_range" {
        let as_ = a.as_str().unwrap_or("");
        let bs = b.as_str().unwrap_or("");
        return match canonical_ip(as_) {
            Ok(x) => match canonical_ip(bs) {
                Ok(y) => x.cmp(&y) as i32,
                Err(_) => 0,
            },
            Err(_) => 0,
        };
    }
    let af = to_f64(a).unwrap_or(0.0);
    let bf = to_f64(b).unwrap_or(0.0);
    if af < bf {
        -1
    } else if af > bf {
        1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Entry points on Index
// ---------------------------------------------------------------------------

impl IndexCtx {
    pub fn buildDocValues(&mut self, idx: &mut Index) -> Result<(), ApiError> {
        if !idx.mappings.is_object() {
            idx.mappings = Value::Object(Map::new());
        }
        let obj = idx.mappings.as_object_mut().unwrap();
        if !obj.get("properties").map(|p| p.is_object()).unwrap_or(false) {
            obj.insert("properties".to_string(), Value::Object(Map::new()));
        }
        if obj.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Ok(());
        }
        let doc = self.doc.clone();
        let props = obj.get_mut("properties").unwrap().as_object_mut().unwrap();
        self.indexObjectMap("", props, &doc)?;
        let props_clone = props.clone();
        let targets: Vec<String> = self.copy_targets.iter().cloned().collect();
        for t in targets {
            let def = crate::eval::mapping_field_def(idx, &t).cloned().unwrap_or_default();
            let typ = def
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let vals = self.values.get(&t).cloned().unwrap_or_default();
            let mut out = Vec::with_capacity(vals.len());
            for v in &vals {
                out.push(self.coerce(&t, &def, &typ, v)?);
            }
            self.values.insert(t, out);
        }
        self.fillConstants(&props_clone, &doc);
        if !self.ignored.is_empty() {
            let list: Vec<Value> = self
                .ignored
                .iter()
                .map(|f| Value::String(f.clone()))
                .collect();
            self.values.insert("_ignored".to_string(), list);
        }
        Ok(())
    }

    pub fn alreadyIgnored(&self) -> &[String] {
        &self.ignored
    }
}

fn invalid_doc_field_name(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                if k.is_empty() {
                    return Some("field name cannot be an empty string".to_string());
                }
                if k == "." {
                    return Some("field name cannot contain only the character [.]".to_string());
                }
                if k.starts_with('.') || k.contains("..") {
                    return Some(format!(
                        "object field starting or ending with a [.] makes object resolution ambiguous: [{}]",
                        k
                    ));
                }
                if let Some(r) = invalid_doc_field_name(child) {
                    return Some(r);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(invalid_doc_field_name),
        _ => None,
    }
}

pub fn eval_derived(def: &Value, doc: &Value) -> Option<Value> {
    let script = def.get("script")?;
    let src = match script {
        Value::String(s) => s.as_str(),
        Value::Object(m) => m.get("source").and_then(|v| v.as_str())?,
        _ => return None,
    };
    let body = src.split_once("emit(").map(|(_, b)| b).unwrap_or(src);
    if body.contains("toUpperCase") {
        let key = quoted_key(body, "params._source[")?;
        let v = crate::store::field_value(doc, &key)?;
        return v.as_str().map(|s| Value::String(s.to_uppercase()));
    }
    let key = quoted_key(body, "doc[").or_else(|| quoted_key(body, "params._source["))?;
    let base = crate::store::field_value(doc, &key)?.clone();
    if body.contains(".value") {
        let rest = body.split_once(".value").map(|(_, r)| r).unwrap_or("");
        let rest = rest.trim().trim_end_matches(')').trim();
        if let Some(op) = rest.chars().next() {
            if matches!(op, '*' | '+' | '-' | '/') {
                let rhs: f64 = rest[1..].trim().parse().ok()?;
                let lhs = base
                    .as_f64()
                    .or_else(|| base.as_str().and_then(|s| s.parse().ok()))?;
                let out = match op {
                    '*' => lhs * rhs,
                    '+' => lhs + rhs,
                    '-' => lhs - rhs,
                    '/' => lhs / rhs,
                    _ => return None,
                };
                if out.fract() == 0.0 {
                    return Some(serde_json::json!(out as i64));
                }
                return Some(serde_json::json!(out));
            }
        }
    }
    Some(base)
}

fn quoted_key<'a>(src: &'a str, marker: &str) -> Option<String> {
    let rest = src.split_once(marker)?.1;
    let rest = rest.trim_start();
    let q = rest.chars().next()?;
    if q != '\'' && q != '"' {
        return None;
    }
    let end = rest[1..].find(q)?;
    Some(rest[1..1 + end].to_string())
}

fn add_derived_values(defs: &Map<String, Value>, doc: &Value, values: &mut BTreeMap<String, Vec<Value>>) {
    for (name, def) in defs {
        if let Some(v) = eval_derived(def, doc) {
            values.insert(name.clone(), vec![v]);
        }
    }
}

pub fn compute_doc_values(
    idx: &mut Index,
    id: &str,
    doc: &mut Map<String, Value>,
) -> Result<(), ApiError> {
    if let Some(reason) = invalid_doc_field_name(&Value::Object(doc.clone())) {
        return Err(mapper_err(
            "mapper_parsing_exception",
            &reason,
            "illegal_argument_exception",
            &reason,
        ));
    }
    let derived_defs = idx
        .mappings
        .get("derived")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut c = IndexCtx::new(idx, id);
    c.doc = doc.clone();
    c.buildDocValues(idx)?;
    add_derived_values(&derived_defs, &Value::Object(c.doc.clone()), &mut c.values);
    let values: Map<String, Value> = c
        .values
        .iter()
        .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
        .collect();
    idx.doc_values.insert(id.to_string(), values);
    if !c.ignored.is_empty() {
        idx.doc_ignored.insert(id.to_string(), c.ignored.clone());
    } else {
        idx.doc_ignored.remove(id);
    }
    doc.remove("_ignored");
    Ok(())
}

pub fn doc_values_for(
    idx: &Index,
    id: &str,
    doc: &Map<String, Value>,
) -> Result<(BTreeMap<String, Vec<Value>>, Vec<String>), ApiError> {
    let mut tmp = idx.clone();
    let derived_defs = tmp
        .mappings
        .get("derived")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut c = IndexCtx::new(&tmp, id);
    c.doc = doc.clone();
    c.buildDocValues(&mut tmp)?;
    add_derived_values(&derived_defs, &Value::Object(c.doc.clone()), &mut c.values);
    Ok((c.values.clone(), c.ignored.clone()))
}

pub fn remove_doc_values(idx: &mut Index, id: &str) {
    idx.doc_values.remove(id);
    idx.doc_ignored.remove(id);
}

pub fn apply_request_derived(idx: &mut Index, defs: &Value) -> Result<(), ApiError> {
    let src = defs
        .as_object()
        .ok_or_else(|| ApiError::new("parsing_exception", "expected derived to be an object", 400))?;
    let mut norm = Map::new();
    for (name, def) in src {
        let mut d = def.clone();
        if let Some(o) = d.as_object_mut() {
            if let Some(Value::String(source)) = o.get("script").cloned() {
                o.insert(
                    "script".to_string(),
                    serde_json::json!({"source": source, "lang": "painless"}),
                );
            }
        }
        norm.insert(name.clone(), d);
    }
    if let Some(m) = idx.mappings.as_object_mut() {
        m.insert("derived".to_string(), Value::Object(norm));
    }
    recompute_all_doc_values(idx);
    Ok(())
}

pub fn recompute_all_doc_values(idx: &mut Index) {
    idx.doc_values.clear();
    idx.doc_ignored.clear();
    let order = idx.order.clone();
    for id in order {
        let mut doc = match idx.docs.get(&id) {
            Some(d) => d.clone(),
            None => continue,
        };
        let mut doc_map = match doc.as_object_mut() {
            Some(m) => m.clone(),
            None => continue,
        };
        let _ = compute_doc_values(idx, &id, &mut doc_map);
        idx.docs.insert(id, Value::Object(doc_map));
    }
}

fn insert_dynamic_prop(
    props: &mut Map<String, Value>,
    parts: &[&str],
    def: Value,
) -> Result<(), String> {
    if parts.len() == 1 {
        if let Some(existing) = props.get(parts[0]) {
            if existing.get("properties").is_none() {
                let old_type = existing.get("type").and_then(|v| v.as_str());
                let new_type = def.get("type").and_then(|v| v.as_str());
                if old_type == new_type {
                    return Ok(());
                }
                return Err(format!(
                    "can't merge a non object mapping [{}] with an object mapping",
                    parts[0]
                ));
            }
        }
        props.insert(parts[0].to_string(), def);
        return Ok(());
    }
    let entry = props
        .entry(parts[0].to_string())
        .or_insert_with(|| json!({"type": "object", "properties": {}}));
    if !entry.is_object() || entry.get("properties").is_none() {
        return Err(format!(
            "can't merge a non object mapping [{}] with an object mapping",
            parts[0]
        ));
    }
    let obj = entry.as_object_mut().unwrap();
    if !obj.get("properties").map(|p| p.is_object()).unwrap_or(false) {
        obj.insert("properties".to_string(), json!({}));
    }
    let sub = obj.get_mut("properties").unwrap().as_object_mut().unwrap();
    insert_dynamic_prop(sub, &parts[1..], def)
}

fn dotted_prop<'a>(props: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = props;
    for (i, part) in parts.iter().enumerate() {
        let def = cur.get(*part).and_then(|v| v.as_object())?;
        if i + 1 == parts.len() {
            return Some(def);
        }
        cur = def.get("properties").and_then(|p| p.as_object())?;
    }
    None
}
