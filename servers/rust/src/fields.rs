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
        };
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
                self.parseIntString(name, typ, def, &s, coerce, false)
            }
            Value::Bool(b) => Err(self.field_value_error(
                name,
                typ,
                v,
                "input_coercion_exception",
                &format!("Cannot coerce value {} to {}", b, typ),
            )),
            Value::String(s) => self.parseIntString(name, typ, def, s, coerce, true),
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
        let mut coerce_false = false;
        if let Some(b) = def.get("coerce").and_then(|v| v.as_bool()) {
            coerce_false = !b;
        }
        let mut coerce = coerce;
        if !coerce && !coerce_false {
            coerce = false;
        }
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
                if coerce_false {
                    return Err(self.field_value_error(
                        name,
                        typ,
                        &Value::String(number_string(n)),
                        "illegal_argument_exception",
                        "Float value passed as String",
                    ));
                }
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
                    "illegal_argument_exception",
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
                    "parse_exception",
                    "failed to parse date field [] with format",
                ));
            }
        }
        if let Value::Number(n) = v {
            let f = n.as_f64().unwrap_or(0.0);
            return Ok(num_i64(f as i64));
        }
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                return Err(self.field_value_error(
                    name,
                    typ,
                    v,
                    "illegal_argument_exception",
                    "failed to parse date field",
                ))
            }
        };
        match parse_date_any(def, &s) {
            Some(ms) => {
                if typ == "date_nanos" {
                    if let Some(ns) = parse_date_nanos_any(def, &s) {
                        return Ok(num_i64(ns));
                    }
                    return Ok(num_i64(ms * 1_000_000));
                }
                Ok(num_i64(ms))
            }
            None => Err(self.field_value_error(
                name,
                typ,
                v,
                "parse_exception",
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
pub fn parse_iso_millis(s: &str) -> Option<i64> {
    let (secs, nanos) = parse_iso_full(s)?;
    Some(secs * 1000 + nanos / 1_000_000)
}

/// Returns (epoch seconds, sub-second nanos).
pub fn parse_iso_full(s: &str) -> Option<(i64, i64)> {
    let b = s.as_bytes();
    if b.len() < 4 {
        return None;
    }
    if !b[..4].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let mut mo: i64 = 1;
    let mut d: i64 = 1;
    let mut pos = 4;
    if b.len() > 4 {
        if b[4] != b'-' {
            return None;
        }
        if b.len() < 7 || !b[5..7].iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        mo = s[5..7].parse().ok()?;
        if !(1..=12).contains(&mo) {
            return None;
        }
        pos = 7;
        if b.len() > 7 {
            if b[7] != b'-' {
                return None;
            }
            if b.len() < 10 || !b[8..10].iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            d = s[8..10].parse().ok()?;
            if !(1..=31).contains(&d) {
                return None;
            }
            pos = 10;
        }
    }
    let mut hh = 0i64;
    let mut mi = 0i64;
    let mut ss = 0i64;
    let mut nanos = 0i64;
    let mut offset_min = 0i64;
    if pos < b.len() {
        if b[pos] != b'T' && b[pos] != b' ' {
            return None;
        }
        pos += 1;
        if b.len() < pos + 8 {
            return None;
        }
        if b[pos + 2] != b':' || b[pos + 5] != b':' {
            return None;
        }
        for (i, c) in [(pos, b[pos]), (pos + 1, b[pos + 1]), (pos + 3, b[pos + 3]), (pos + 4, b[pos + 4]), (pos + 6, b[pos + 6]), (pos + 7, b[pos + 7])] {
            if !c.is_ascii_digit() {
                let _ = i;
                return None;
            }
        }
        hh = s[pos..pos + 2].parse().ok()?;
        mi = s[pos + 3..pos + 5].parse().ok()?;
        ss = s[pos + 6..pos + 8].parse().ok()?;
        if hh > 23 || mi > 59 || ss > 60 {
            return None;
        }
        pos += 8;
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
                        if let Ok(n) = s.parse::<i64>() {
                            return Some(n);
                        }
                    }
                    "epoch_second" => {
                        if let Ok(n) = s.parse::<i64>() {
                            return Some(n * 1000);
                        }
                    }
                    "date_optional_time" | "strict_date_optional_time" | "date_time"
                    | "strict_date_optional_time_nanos" | "basic_date_time" => {
                        if let Some(ms) = parse_iso_millis(s) {
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
                        return Some(n * 1_000_000);
                    }
                }
            }
        }
    }
    if s.contains('.') {
        let chars: Vec<char> = s.chars().collect();
        let mut cut = None;
        for (i, c) in chars.iter().enumerate() {
            if i == 0 {
                continue;
            }
            if *c == '.' || *c == 'Z' || *c == '+' || *c == '-' {
                cut = Some(i);
                break;
            }
        }
        if let Some(i) = cut {
            let base: String = chars[..i].iter().collect();
            let rest: String = chars[i..].iter().collect();
            let mut frac = String::new();
            if let Some(stripped) = rest.strip_prefix('.') {
                frac = stripped
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
            }
            if let Some(ms) = parse_iso_millis(&base) {
                let mut pad = frac;
                while pad.len() < 9 {
                    pad.push('0');
                }
                let ns: i64 = pad[..9].parse().unwrap_or(0);
                return Some(ms * 1_000_000 + ns);
            }
        }
    }
    parse_iso_millis(s).map(|ms| ms * 1_000_000)
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
        if has_props && (typ == "object" || typ == "nested") {
            return self.indexObject(path, def, raw);
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
                        sub_vals.push(nv);
                    }
                }
                if !sub_vals.is_empty() {
                    let entry = self.values.entry(sub).or_default();
                    entry.extend(sub_vals);
                }
            }
        }
        if let Some(ct) = def.get("copy_to").and_then(|v| v.as_str()) {
            if !ct.is_empty() {
                let vals = self.values.get(path).cloned().unwrap_or_default();
                self.copyTo(ct, &vals);
            }
        } else if let Some(cts) = def.get("copy_to").and_then(|v| v.as_array()) {
            for e in cts {
                if let Some(s) = e.as_str() {
                    let vals = self.values.get(path).cloned().unwrap_or_default();
                    self.copyTo(s, &vals);
                }
            }
        }
        Ok(())
    }

    pub fn copyTo(&mut self, target: &str, vals: &[Value]) {
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
            "text" | "match_only_text" | "search_as_you_type" | "wildcard" | "flat_object" => {
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
            "keyword" | "constant_keyword" => match value_scalar_string(v) {
                Some(mut s) => {
                    if let Some(norm) = def.get("normalizer").and_then(|v| v.as_str()) {
                        if !norm.is_empty() {
                            s = apply_normalizer(norm, &s);
                        }
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
                Some(s) => Ok(Value::String(s)),
                None => Err(self.field_value_error(
                    path,
                    typ,
                    v,
                    "illegal_argument_exception",
                    "malformed",
                )),
            },
            "token_count" => self.parseTokenCount(path, v),
            "integer_range" | "float_range" | "long_range" | "double_range" | "date_range"
            | "ip_range" => self.parseRangeValue(path, typ, def, v),
            "geo_point" => self.parseGeoPoint(path, v),
            "geo_shape" => Ok(v.clone()),
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
                let props = def
                    .entry("properties".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if !props.is_object() {
                    *props = Value::Object(Map::new());
                }
                let props = props.as_object_mut().unwrap();
                self.indexObjectMap(path, props, m)
            }
            _ => Ok(()),
        }
    }

    fn indexObjectMap(
        &mut self,
        path: &str,
        props: &mut Map<String, Value>,
        src: &Map<String, Value>,
    ) -> Result<(), ApiError> {
        for (k, v) in src {
            if k.starts_with('_') {
                self.rejectMetadataField(k)?;
                continue;
            }
            let child_path = if path.is_empty() {
                k.clone()
            } else {
                format!("{}.{}", path, k)
            };
            let declared = props.get(k).map(|d| d.is_object()).unwrap_or(false);
            if !declared {
                if self.dyn_setting == "strict" {
                    let name = if path.is_empty() { "root" } else { path };
                    return Err(ApiError::new(
                        "strict_dynamic_mapping_exception",
                        &format!(
                            "mapping set to strict, dynamic introduction of [{}] within [{}] is not allowed",
                            k, name
                        ),
                        400,
                    ));
                }
                if self.dyn_setting == "false" {
                    continue;
                }
                let inferred = match infer_type(v) {
                    Some(t) => t,
                    None => continue,
                };
                props.insert(k.clone(), inferred);
            }
            let def = props
                .get_mut(k)
                .and_then(|d| d.as_object_mut())
                .map(|o| {
                    // take a snapshot to satisfy the borrow checker; nested
                    // dynamic additions are written back below
                    o.clone()
                })
                .unwrap();
            let mut def = def;
            self.indexRawField(&child_path, &mut def, v)?;
            if let Some(slot) = props.get_mut(k).and_then(|d| d.as_object_mut()) {
                *slot = def;
            }
        }
        Ok(())
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
            if a.len() == 2 {
                let numlike = |v: &Value| {
                    v.is_number()
                        || v.as_str()
                            .map(|s| s.trim().parse::<f64>().is_ok())
                            .unwrap_or(false)
                };
                if numlike(&a[0]) && numlike(&a[1]) {
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

impl IndexCtx {
    fn parseTokenCount(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        match value_scalar_string(v) {
            Some(s) => Ok(num_i64(analyze(&s).len() as i64)),
            None => Err(self.field_value_error(
                path,
                "token_count",
                v,
                "illegal_argument_exception",
                "malformed",
            )),
        }
    }

    fn parseGeoPoint(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        match v {
            Value::String(s) => {
                let parts: Vec<&str> = s.split(',').collect();
                if parts.len() == 2 {
                    if let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f64>(),
                        parts[1].trim().parse::<f64>(),
                    ) {
                        return Ok(json!({"lat": lat, "lon": lon}));
                    }
                }
                let up = s.trim().to_uppercase();
                if let Some(rest) = up.strip_prefix("POINT") {
                    let nums: Vec<f64> = rest
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect();
                    if nums.len() >= 2 {
                        return Ok(json!({"lat": nums[1], "lon": nums[0]}));
                    }
                }
                if let Some((lon, lat)) = geohash_decode(s.trim()) {
                    return Ok(json!({"lat": lat, "lon": lon}));
                }
                Err(self.field_value_error(
                    path,
                    "geo_point",
                    v,
                    "illegal_argument_exception",
                    "failed to parse geo_point",
                ))
            }
            Value::Object(m) => {
                if let (Some(lat), Some(lon)) = (m.get("lat").and_then(to_f64), m.get("lon").and_then(to_f64)) {
                    return Ok(json!({"lat": lat, "lon": lon}));
                }
                Err(self.field_value_error(
                    path,
                    "geo_point",
                    v,
                    "illegal_argument_exception",
                    "failed to parse geo_point",
                ))
            }
            Value::Array(a) => {
                if a.len() == 2 {
                    if let (Some(lon), Some(lat)) = (to_f64(&a[0]), to_f64(&a[1])) {
                        return Ok(json!({"lat": lat, "lon": lon}));
                    }
                }
                Err(self.field_value_error(
                    path,
                    "geo_point",
                    v,
                    "illegal_argument_exception",
                    "failed to parse geo_point",
                ))
            }
            _ => Err(self.field_value_error(
                path,
                "geo_point",
                v,
                "illegal_argument_exception",
                "failed to parse geo_point",
            )),
        }
    }

    fn parseCompletion(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        if let Value::Object(m) = v {
            let mut out: Vec<Value> = Vec::new();
            if let Some(inp) = m.get("input").and_then(|i| i.as_array()) {
                for e in inp {
                    if let Some(s) = value_scalar_string(e) {
                        out.push(Value::String(s));
                    }
                }
            }
            if out.is_empty() {
                out.push(v.clone());
            }
            return Ok(Value::Array(out));
        }
        if let Some(s) = value_scalar_string(v) {
            return Ok(Value::String(s));
        }
        Err(self.field_value_error(
            path,
            "completion",
            v,
            "illegal_argument_exception",
            "failed to parse completion",
        ))
    }

    fn parseRankFeature(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        let mut f = to_f64(v);
        if f.is_none() {
            if let Some(s) = value_scalar_string(v) {
                if let Ok(fv) = s.parse::<f64>() {
                    f = Some(fv);
                }
            }
        }
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
        if f < 0.0 {
            return Err(self.field_value_error(
                path,
                "rank_feature",
                v,
                "illegal_argument_exception",
                "Field value must be >= 0",
            ));
        }
        Ok(num_f64(f))
    }

    fn parseRankFeatures(&self, path: &str, v: &Value) -> Result<Value, ApiError> {
        let m = match v.as_object() {
            Some(m) => m,
            None => {
                return Err(self.field_value_error(
                    path,
                    "rank_features",
                    v,
                    "illegal_argument_exception",
                    "failed to parse rank_features",
                ))
            }
        };
        for (k, fv) in m {
            let f = to_f64(fv).or_else(|| {
                value_scalar_string(fv).and_then(|s| s.parse::<f64>().ok())
            });
            match f {
                Some(f) if f >= 0.0 => {}
                _ => {
                    return Err(self.field_value_error(
                        path,
                        "rank_features",
                        fv,
                        "illegal_argument_exception",
                        &format!("failed to parse rank feature [{}]", k),
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
                    &format!("[{}] is not a valid join name", s),
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
                let _ = parent;
                if !valid {
                    return Err(self.field_value_error(
                        path,
                        "join",
                        v,
                        "illegal_argument_exception",
                        &format!("[{}] is not a valid join name", name),
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
                        path,
                        preview_value(v)
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
                "date_range" => self.parseDateValue(path, "date", def, raw)?,
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
        let doc = self.doc.clone();
        let props = obj.get_mut("properties").unwrap().as_object_mut().unwrap();
        self.indexObjectMap("", props, &doc)?;
        let props_clone = props.clone();
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

pub fn compute_doc_values(
    idx: &mut Index,
    id: &str,
    doc: &mut Map<String, Value>,
) -> Result<(), ApiError> {
    let mut c = IndexCtx::new(idx, id);
    c.doc = doc.clone();
    c.buildDocValues(idx)?;
    let values: Map<String, Value> = c
        .values
        .iter()
        .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
        .collect();
    idx.doc_values.insert(id.to_string(), values);
    if !c.ignored.is_empty() {
        idx.doc_ignored.insert(id.to_string(), c.ignored.clone());
        let arr: Vec<Value> = c
            .ignored
            .iter()
            .map(|f| Value::String(f.clone()))
            .collect();
        doc.insert("_ignored".to_string(), Value::Array(arr));
    } else {
        idx.doc_ignored.remove(id);
        doc.remove("_ignored");
    }
    Ok(())
}

pub fn doc_values_for(
    idx: &Index,
    id: &str,
    doc: &Map<String, Value>,
) -> Result<(BTreeMap<String, Vec<Value>>, Vec<String>), ApiError> {
    let mut tmp = idx.clone();
    let mut c = IndexCtx::new(&tmp, id);
    c.doc = doc.clone();
    c.buildDocValues(&mut tmp)?;
    Ok((c.values.clone(), c.ignored.clone()))
}

pub fn remove_doc_values(idx: &mut Index, id: &str) {
    idx.doc_values.remove(id);
    idx.doc_ignored.remove(id);
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
