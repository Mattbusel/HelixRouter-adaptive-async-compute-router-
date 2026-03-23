//! JSON schema validation, size limits, and header requirements.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Supported JSON schema types.
#[derive(Clone)]
pub enum SchemaType {
    /// UTF-8 string with optional length and pattern constraints.
    String {
        /// Minimum string length.
        min_len: Option<usize>,
        /// Maximum string length.
        max_len: Option<usize>,
        /// Basic pattern (supports `*` as wildcard matching anything).
        pattern: Option<String>,
    },
    /// 64-bit signed integer.
    Integer {
        /// Minimum value (inclusive).
        min: Option<i64>,
        /// Maximum value (inclusive).
        max: Option<i64>,
    },
    /// 64-bit floating-point number.
    Float {
        /// Minimum value (inclusive).
        min: Option<f64>,
        /// Maximum value (inclusive).
        max: Option<f64>,
    },
    /// Boolean.
    Boolean,
    /// Homogeneous array of items.
    Array {
        /// Schema for each item.
        item_type: Box<SchemaType>,
        /// Maximum number of items.
        max_items: Option<usize>,
    },
    /// JSON object with named fields.
    Object {
        /// Field definitions.
        fields: Vec<SchemaField>,
    },
    /// A value that may be null.
    Nullable(Box<SchemaType>),
}

/// One field within an Object schema.
#[derive(Clone)]
pub struct SchemaField {
    /// Field name.
    pub name: String,
    /// Schema for the field value.
    pub schema: SchemaType,
    /// Whether the field must be present.
    pub required: bool,
}

/// Minimal recursive JSON value representation for validation.
#[derive(Debug)]
enum JsonVal<'a> {
    Null,
    Bool(bool),
    Number(f64),
    Str(&'a str),
    Array(Vec<JsonVal<'a>>),
    Object(Vec<(&'a str, JsonVal<'a>)>),
}

/// Attempt to parse `input` as a JSON value, returning the value and the
/// number of bytes consumed.
fn parse_json(input: &str) -> Option<(JsonVal<'_>, usize)> {
    let s = input.trim_start();
    let offset = input.len() - s.len();

    if s.starts_with("null") {
        return Some((JsonVal::Null, offset + 4));
    }
    if s.starts_with("true") {
        return Some((JsonVal::Bool(true), offset + 4));
    }
    if s.starts_with("false") {
        return Some((JsonVal::Bool(false), offset + 5));
    }
    if s.starts_with('"') {
        // String — find closing quote, handling simple escapes
        let inner = &s[1..];
        let mut chars = inner.char_indices();
        let mut escaped = false;
        while let Some((i, c)) = chars.next() {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == '"' {
                return Some((JsonVal::Str(&inner[..i]), offset + 1 + i + 1));
            }
        }
        return None; // unterminated string
    }
    if s.starts_with('[') {
        let mut items = Vec::new();
        let mut pos = 1usize;
        let rest = &s[pos..];
        let r = rest.trim_start();
        if r.starts_with(']') {
            let skip = rest.len() - r.len() + 1;
            return Some((JsonVal::Array(items), offset + pos + skip));
        }
        loop {
            let sub = &s[pos..];
            let (val, consumed) = parse_json(sub)?;
            items.push(val);
            pos += consumed;
            let after = s[pos..].trim_start();
            let skip = s[pos..].len() - after.len();
            pos += skip;
            if after.starts_with(',') {
                pos += 1;
            } else if after.starts_with(']') {
                pos += 1;
                break;
            } else {
                return None;
            }
        }
        return Some((JsonVal::Array(items), offset + pos));
    }
    if s.starts_with('{') {
        let mut fields: Vec<(&str, JsonVal)> = Vec::new();
        let mut pos = 1usize;
        let rest = &s[pos..];
        let r = rest.trim_start();
        if r.starts_with('}') {
            let skip = rest.len() - r.len() + 1;
            return Some((JsonVal::Object(fields), offset + pos + skip));
        }
        loop {
            // Parse key
            let sub = s[pos..].trim_start();
            let skip_ws = s[pos..].len() - sub.len();
            pos += skip_ws;
            let (key_val, k_consumed) = parse_json(&s[pos..])?;
            let key = match key_val {
                JsonVal::Str(k) => k,
                _ => return None,
            };
            pos += k_consumed;
            // Expect ':'
            let after_key = s[pos..].trim_start();
            let skip2 = s[pos..].len() - after_key.len();
            pos += skip2;
            if !after_key.starts_with(':') {
                return None;
            }
            pos += 1;
            // Parse value
            let (val, v_consumed) = parse_json(&s[pos..])?;
            fields.push((key, val));
            pos += v_consumed;
            let after_val = s[pos..].trim_start();
            let skip3 = s[pos..].len() - after_val.len();
            pos += skip3;
            if after_val.starts_with(',') {
                pos += 1;
            } else if after_val.starts_with('}') {
                pos += 1;
                break;
            } else {
                return None;
            }
        }
        return Some((JsonVal::Object(fields), offset + pos));
    }
    // Number
    let end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E')
        .unwrap_or(s.len());
    if end > 0 {
        if let Ok(n) = s[..end].parse::<f64>() {
            return Some((JsonVal::Number(n), offset + end));
        }
    }
    None
}

/// Simple wildcard match: `*` matches any substring.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First part must match at start
            if !value.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else {
            match value[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    // If pattern ends with `*`, any suffix is fine
    if pattern.ends_with('*') {
        return true;
    }
    // Otherwise the last part must align with end
    pos == value.len()
}

fn validate_value<'a>(val: &JsonVal<'a>, schema: &SchemaType, path: &str, errors: &mut Vec<String>) {
    match schema {
        SchemaType::Nullable(inner) => {
            if matches!(val, JsonVal::Null) {
                return;
            }
            validate_value(val, inner, path, errors);
        }
        SchemaType::Boolean => {
            if !matches!(val, JsonVal::Bool(_)) {
                errors.push(format!("{path}: expected boolean"));
            }
        }
        SchemaType::Integer { min, max } => {
            if let JsonVal::Number(n) = val {
                let n_i = *n as i64;
                if let Some(lo) = min {
                    if n_i < *lo {
                        errors.push(format!("{path}: {n_i} < min {lo}"));
                    }
                }
                if let Some(hi) = max {
                    if n_i > *hi {
                        errors.push(format!("{path}: {n_i} > max {hi}"));
                    }
                }
            } else {
                errors.push(format!("{path}: expected integer"));
            }
        }
        SchemaType::Float { min, max } => {
            if let JsonVal::Number(n) = val {
                if let Some(lo) = min {
                    if n < lo {
                        errors.push(format!("{path}: {n} < min {lo}"));
                    }
                }
                if let Some(hi) = max {
                    if n > hi {
                        errors.push(format!("{path}: {n} > max {hi}"));
                    }
                }
            } else {
                errors.push(format!("{path}: expected float"));
            }
        }
        SchemaType::String { min_len, max_len, pattern } => {
            if let JsonVal::Str(s) = val {
                if let Some(min) = min_len {
                    if s.len() < *min {
                        errors.push(format!("{path}: string too short ({} < {})", s.len(), min));
                    }
                }
                if let Some(max) = max_len {
                    if s.len() > *max {
                        errors.push(format!("{path}: string too long ({} > {})", s.len(), max));
                    }
                }
                if let Some(pat) = pattern {
                    if !wildcard_match(pat, s) {
                        errors.push(format!("{path}: string '{s}' does not match pattern '{pat}'"));
                    }
                }
            } else {
                errors.push(format!("{path}: expected string"));
            }
        }
        SchemaType::Array { item_type, max_items } => {
            if let JsonVal::Array(items) = val {
                if let Some(max) = max_items {
                    if items.len() > *max {
                        errors.push(format!("{path}: array length {} > max {}", items.len(), max));
                    }
                }
                for (i, item) in items.iter().enumerate() {
                    validate_value(item, item_type, &format!("{path}[{i}]"), errors);
                }
            } else {
                errors.push(format!("{path}: expected array"));
            }
        }
        SchemaType::Object { fields } => {
            if let JsonVal::Object(obj_fields) = val {
                let field_map: HashMap<&str, &JsonVal> =
                    obj_fields.iter().map(|(k, v)| (*k, v)).collect();
                for field in fields {
                    match field_map.get(field.name.as_str()) {
                        Some(fval) => {
                            validate_value(
                                fval,
                                &field.schema,
                                &format!("{}.{}", path, field.name),
                                errors,
                            );
                        }
                        None if field.required => {
                            errors.push(format!("{path}: missing required field '{}'", field.name));
                        }
                        None => {}
                    }
                }
            } else {
                errors.push(format!("{path}: expected object"));
            }
        }
    }
}

/// Validate a JSON string against the given schema.
/// Returns `Ok(())` on success, or a list of error messages.
pub fn validate_json(value: &str, schema: &SchemaType) -> Result<(), Vec<String>> {
    match parse_json(value) {
        Some((json_val, _)) => {
            let mut errors = Vec::new();
            validate_value(&json_val, schema, "$", &mut errors);
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors)
            }
        }
        None => Err(vec![format!("invalid JSON: could not parse '{value}'")]),
    }
}

/// Byte-size and count limits for incoming requests.
#[derive(Clone)]
pub struct SizeLimits {
    /// Maximum request body size in bytes.
    pub max_body_bytes: usize,
    /// Maximum number of HTTP headers.
    pub max_header_count: usize,
    /// Maximum length of any single header value.
    pub max_header_value_len: usize,
    /// Maximum URL length.
    pub max_url_len: usize,
}

/// Constraint on a specific HTTP header.
#[derive(Clone)]
pub struct HeaderRequirement {
    /// Header name (case-insensitive comparison).
    pub name: String,
    /// Whether this header must be present.
    pub required: bool,
    /// If non-empty, the header value must be one of these.
    pub allowed_values: Vec<String>,
}

/// Full validation configuration for a route prefix.
#[derive(Clone)]
pub struct RequestValidationConfig {
    /// Body and header size limits.
    pub size_limits: SizeLimits,
    /// Header constraints.
    pub required_headers: Vec<HeaderRequirement>,
    /// Optional JSON body schema.
    pub body_schema: Option<SchemaType>,
    /// HTTP methods that are allowed.
    pub allowed_methods: Vec<String>,
}

/// Outcome of a validation check.
pub struct ValidationResult {
    /// Whether the request is valid.
    pub valid: bool,
    /// Human-readable error list (empty when valid).
    pub errors: Vec<String>,
}

/// Validates incoming requests against registered route configurations.
pub struct RequestValidator {
    configs: RwLock<HashMap<String, RequestValidationConfig>>,
    total_validated: AtomicU64,
    total_rejected: AtomicU64,
}

impl Default for RequestValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestValidator {
    /// Create a new validator with no configurations.
    pub fn new() -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
            total_validated: AtomicU64::new(0),
            total_rejected: AtomicU64::new(0),
        }
    }

    /// Register a validation config for a route prefix.
    pub fn add_config(&self, route_prefix: &str, config: RequestValidationConfig) {
        let mut configs = self.configs.write().unwrap();
        configs.insert(route_prefix.to_string(), config);
    }

    /// Find the most specific matching config for `route`.
    fn find_config<'a>(
        configs: &'a HashMap<String, RequestValidationConfig>,
        route: &str,
    ) -> Option<&'a RequestValidationConfig> {
        let mut best: Option<(&str, &RequestValidationConfig)> = None;
        for (prefix, cfg) in configs.iter() {
            if route.starts_with(prefix.as_str()) {
                match best {
                    None => best = Some((prefix, cfg)),
                    Some((best_prefix, _)) if prefix.len() > best_prefix.len() => {
                        best = Some((prefix, cfg));
                    }
                    _ => {}
                }
            }
        }
        best.map(|(_, c)| c)
    }

    /// Validate a request. Returns a `ValidationResult`.
    pub fn validate(
        &self,
        route: &str,
        method: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> ValidationResult {
        self.total_validated.fetch_add(1, Ordering::Relaxed);
        let configs = self.configs.read().unwrap();
        let Some(cfg) = Self::find_config(&configs, route) else {
            // No config → pass through
            return ValidationResult { valid: true, errors: vec![] };
        };

        let mut errors = Vec::new();

        // Method check
        if !cfg.allowed_methods.is_empty()
            && !cfg.allowed_methods.iter().any(|m| m.eq_ignore_ascii_case(method))
        {
            errors.push(format!("method '{}' not allowed", method));
        }

        // Size limits
        if body.len() > cfg.size_limits.max_body_bytes {
            errors.push(format!(
                "body too large: {} > {}",
                body.len(),
                cfg.size_limits.max_body_bytes
            ));
        }
        if headers.len() > cfg.size_limits.max_header_count {
            errors.push(format!(
                "too many headers: {} > {}",
                headers.len(),
                cfg.size_limits.max_header_count
            ));
        }
        for (name, value) in headers {
            if value.len() > cfg.size_limits.max_header_value_len {
                errors.push(format!(
                    "header '{}' value too long: {} > {}",
                    name,
                    value.len(),
                    cfg.size_limits.max_header_value_len
                ));
            }
        }
        if route.len() > cfg.size_limits.max_url_len {
            errors.push(format!(
                "URL too long: {} > {}",
                route.len(),
                cfg.size_limits.max_url_len
            ));
        }

        // Header requirements
        let header_map: HashMap<String, &str> = headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.as_str()))
            .collect();
        for req in &cfg.required_headers {
            let key = req.name.to_lowercase();
            match header_map.get(&key) {
                None if req.required => {
                    errors.push(format!("missing required header '{}'", req.name));
                }
                Some(val) if !req.allowed_values.is_empty() => {
                    if !req.allowed_values.iter().any(|av| av.as_str() == *val) {
                        errors.push(format!(
                            "header '{}' value '{}' not in allowed list",
                            req.name, val
                        ));
                    }
                }
                _ => {}
            }
        }

        // Body schema
        if let Some(schema) = &cfg.body_schema {
            if let Err(schema_errors) = validate_json(body, schema) {
                errors.extend(schema_errors);
            }
        }

        let valid = errors.is_empty();
        if !valid {
            self.total_rejected.fetch_add(1, Ordering::Relaxed);
        }
        ValidationResult { valid, errors }
    }

    /// Return `(total_validated, total_rejected)`.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.total_validated.load(Ordering::Relaxed),
            self.total_rejected.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_string_schema() {
        let schema = SchemaType::String {
            min_len: Some(2),
            max_len: Some(10),
            pattern: None,
        };
        assert!(validate_json(r#""hello""#, &schema).is_ok());
        assert!(validate_json(r#""a""#, &schema).is_err()); // too short
        assert!(validate_json(r#""hello world!""#, &schema).is_err()); // too long
    }

    #[test]
    fn validate_object_schema() {
        let schema = SchemaType::Object {
            fields: vec![
                SchemaField {
                    name: "name".to_string(),
                    schema: SchemaType::String { min_len: None, max_len: None, pattern: None },
                    required: true,
                },
                SchemaField {
                    name: "age".to_string(),
                    schema: SchemaType::Integer { min: Some(0), max: Some(150) },
                    required: false,
                },
            ],
        };
        assert!(validate_json(r#"{"name": "Alice", "age": 30}"#, &schema).is_ok());
        assert!(validate_json(r#"{"age": 30}"#, &schema).is_err()); // missing required
    }

    #[test]
    fn wildcard_pattern() {
        assert!(wildcard_match("application/*", "application/json"));
        assert!(!wildcard_match("text/*", "application/json"));
        assert!(wildcard_match("*", "anything"));
    }

    #[test]
    fn request_validator_integration() {
        let validator = RequestValidator::new();
        let config = RequestValidationConfig {
            size_limits: SizeLimits {
                max_body_bytes: 1024,
                max_header_count: 20,
                max_header_value_len: 256,
                max_url_len: 512,
            },
            required_headers: vec![HeaderRequirement {
                name: "Content-Type".to_string(),
                required: true,
                allowed_values: vec!["application/json".to_string()],
            }],
            body_schema: None,
            allowed_methods: vec!["GET".to_string(), "POST".to_string()],
        };
        validator.add_config("/api", config);

        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let result = validator.validate("/api/users", "POST", &headers, "{}");
        assert!(result.valid);

        let bad = validator.validate("/api/users", "DELETE", &headers, "{}");
        assert!(!bad.valid);

        let (validated, rejected) = validator.stats();
        assert_eq!(validated, 2);
        assert_eq!(rejected, 1);
    }
}
