//! # Dynamic Configuration with Hot-Reload and Validation
//!
//! ## Responsibility
//! Provide an in-process, schema-validated, hot-reloadable configuration store
//! with atomic swap semantics and change-notification via a
//! [`tokio::sync::watch`] channel.
//!
//! ## Design
//! - All reads are lock-free (short `RwLock` read guard).
//! - Writes validate the new value against the optional [`ConfigSchema`] before
//!   acquiring the write lock, minimising lock-hold time.
//! - [`DynamicConfig::reload_from_map`] performs an atomic full-map swap and
//!   returns the set of changed keys.
//! - [`DynamicConfig::subscribe`] returns a [`tokio::sync::watch::Receiver<u64>`]
//!   that is bumped on every successful write or reload.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::watch;

// ── ConfigValue ───────────────────────────────────────────────────────────────

/// A dynamically-typed configuration value.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    /// A UTF-8 string.
    String(String),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit floating-point number.
    Float(f64),
    /// A boolean flag.
    Bool(bool),
    /// An ordered list of values.
    Array(Vec<ConfigValue>),
}

impl ConfigValue {
    /// Return the [`ValueType`] of this value.
    pub fn value_type(&self) -> ValueType {
        match self {
            Self::String(_) => ValueType::String,
            Self::Int(_) => ValueType::Int,
            Self::Float(_) => ValueType::Float,
            Self::Bool(_) => ValueType::Bool,
            Self::Array(_) => ValueType::Array,
        }
    }

    /// Return the numeric representation if applicable (for range checks).
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }
}

// ── ValueType ────────────────────────────────────────────────────────────────

/// The expected Rust type for a schema field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    /// UTF-8 string.
    String,
    /// 64-bit signed integer.
    Int,
    /// 64-bit floating-point.
    Float,
    /// Boolean.
    Bool,
    /// Homogeneous array.
    Array,
}

impl std::fmt::Display for ValueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String => write!(f, "String"),
            Self::Int => write!(f, "Int"),
            Self::Float => write!(f, "Float"),
            Self::Bool => write!(f, "Bool"),
            Self::Array => write!(f, "Array"),
        }
    }
}

// ── SchemaField / ConfigSchema ────────────────────────────────────────────────

/// Validation rules for a single configuration key.
#[derive(Debug, Clone)]
pub struct SchemaField {
    /// Whether the key must be present in the configuration.
    pub required: bool,
    /// Expected type of the value.
    pub value_type: ValueType,
    /// Minimum numeric value (inclusive). Only meaningful for `Int` / `Float`.
    pub min: Option<f64>,
    /// Maximum numeric value (inclusive). Only meaningful for `Int` / `Float`.
    pub max: Option<f64>,
    /// A regular-expression pattern the string value must match (string fields only).
    pub regex: Option<String>,
}

/// Schema describing all known configuration keys and their constraints.
#[derive(Debug, Default, Clone)]
pub struct ConfigSchema {
    /// Field descriptors, keyed by configuration key name.
    pub fields: HashMap<String, SchemaField>,
}

impl ConfigSchema {
    /// Create an empty schema.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a field in the schema.
    pub fn add_field(&mut self, key: impl Into<String>, field: SchemaField) {
        self.fields.insert(key.into(), field);
    }
}

// ── ConfigError ───────────────────────────────────────────────────────────────

/// Errors produced when configuration validation fails.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// A required key is absent.
    MissingRequired(String),
    /// The value's type does not match the schema.
    TypeMismatch {
        /// The key whose value has the wrong type.
        key: String,
        /// The type the schema expects.
        expected: String,
        /// The type that was supplied.
        got: String,
    },
    /// A numeric value falls outside the allowed range.
    OutOfRange {
        /// The key whose value is out of range.
        key: String,
        /// The supplied value.
        value: f64,
        /// The minimum allowed value.
        min: f64,
        /// The maximum allowed value.
        max: f64,
    },
    /// A string value does not match the required pattern.
    ValidationFailed(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRequired(k) => write!(f, "required key '{k}' is missing"),
            Self::TypeMismatch { key, expected, got } => {
                write!(f, "key '{key}': expected {expected}, got {got}")
            }
            Self::OutOfRange { key, value, min, max } => {
                write!(f, "key '{key}': value {value} is outside [{min}, {max}]")
            }
            Self::ValidationFailed(msg) => write!(f, "validation failed: {msg}"),
        }
    }
}

// ── DynamicConfig ─────────────────────────────────────────────────────────────

/// A hot-reloadable, schema-validated, concurrency-safe configuration store.
///
/// # Clone behaviour
/// Cloning produces a handle that shares the same underlying data — all clones
/// see the same values and receive the same change notifications.
#[derive(Clone)]
pub struct DynamicConfig {
    values: Arc<RwLock<HashMap<String, ConfigValue>>>,
    schema: Arc<Option<ConfigSchema>>,
    version_tx: watch::Sender<u64>,
    version: Arc<RwLock<u64>>,
}

impl DynamicConfig {
    /// Create an empty config without a schema.
    pub fn new() -> Self {
        let (tx, _) = watch::channel(0u64);
        Self {
            values: Arc::new(RwLock::new(HashMap::new())),
            schema: Arc::new(None),
            version_tx: tx,
            version: Arc::new(RwLock::new(0)),
        }
    }

    /// Create an empty config with the given validation schema.
    pub fn with_schema(schema: ConfigSchema) -> Self {
        let (tx, _) = watch::channel(0u64);
        Self {
            values: Arc::new(RwLock::new(HashMap::new())),
            schema: Arc::new(Some(schema)),
            version_tx: tx,
            version: Arc::new(RwLock::new(0)),
        }
    }

    /// Retrieve a value by key. Returns `None` if the key is absent.
    pub fn get(&self, key: &str) -> Option<ConfigValue> {
        self.values
            .read()
            .ok()
            .and_then(|guard| guard.get(key).cloned())
    }

    /// Set a single key. Validates the value against the schema (if any) before
    /// writing. On success the version counter is bumped.
    pub fn set(&self, key: &str, value: ConfigValue) -> Result<(), ConfigError> {
        if let Some(schema) = self.schema.as_ref() {
            if let Some(field) = schema.fields.get(key) {
                validate_value(key, &value, field)?;
            }
        }
        let mut guard = self.values.write().map_err(|_| {
            ConfigError::ValidationFailed("lock poisoned".to_owned())
        })?;
        guard.insert(key.to_owned(), value);
        drop(guard);
        self.bump_version();
        Ok(())
    }

    /// Atomically swap the entire configuration map with `new_values`.
    ///
    /// Validates all values against the schema before swapping. On success:
    /// - The internal map is replaced atomically.
    /// - The version counter is bumped.
    /// - Returns the list of keys that were added, removed, or changed.
    pub fn reload_from_map(
        &self,
        new_values: HashMap<String, ConfigValue>,
    ) -> Result<Vec<String>, ConfigError> {
        // Validate all required fields and value types.
        if let Some(schema) = self.schema.as_ref() {
            for (key, field) in &schema.fields {
                if field.required && !new_values.contains_key(key) {
                    return Err(ConfigError::MissingRequired(key.clone()));
                }
            }
            for (key, value) in &new_values {
                if let Some(field) = schema.fields.get(key) {
                    validate_value(key, value, field)?;
                }
            }
        }

        let mut guard = self.values.write().map_err(|_| {
            ConfigError::ValidationFailed("lock poisoned".to_owned())
        })?;

        let old = &*guard;
        let mut changed = Vec::new();

        // Keys present in new but not in old, or whose value changed.
        for (k, v) in &new_values {
            if old.get(k) != Some(v) {
                changed.push(k.clone());
            }
        }
        // Keys present in old but removed from new.
        for k in old.keys() {
            if !new_values.contains_key(k) {
                changed.push(k.clone());
            }
        }

        *guard = new_values;
        drop(guard);
        if !changed.is_empty() {
            self.bump_version();
        }
        Ok(changed)
    }

    /// Subscribe to configuration changes. The returned receiver yields the
    /// current version and is notified every time a `set` or `reload_from_map`
    /// succeeds.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version_tx.subscribe()
    }

    /// Validate all currently stored values against `schema` and return any
    /// errors found.
    pub fn validate_all(&self, schema: &ConfigSchema) -> Vec<ConfigError> {
        let mut errors = Vec::new();
        let guard = match self.values.read() {
            Ok(g) => g,
            Err(_) => {
                errors.push(ConfigError::ValidationFailed("lock poisoned".to_owned()));
                return errors;
            }
        };

        // Check required fields.
        for (key, field) in &schema.fields {
            if field.required && !guard.contains_key(key) {
                errors.push(ConfigError::MissingRequired(key.clone()));
            }
        }
        // Validate existing values.
        for (key, value) in guard.iter() {
            if let Some(field) = schema.fields.get(key) {
                if let Err(e) = validate_value(key, value, field) {
                    errors.push(e);
                }
            }
        }
        errors
    }

    // ── private ───────────────────────────────────────────────────────────

    fn bump_version(&self) {
        if let Ok(mut v) = self.version.write() {
            *v += 1;
            let _ = self.version_tx.send(*v);
        }
    }
}

impl Default for DynamicConfig {
    fn default() -> Self {
        Self::new()
    }
}

// ── Validation helper ─────────────────────────────────────────────────────────

fn validate_value(key: &str, value: &ConfigValue, field: &SchemaField) -> Result<(), ConfigError> {
    // Type check.
    if value.value_type() != field.value_type {
        return Err(ConfigError::TypeMismatch {
            key: key.to_owned(),
            expected: field.value_type.to_string(),
            got: value.value_type().to_string(),
        });
    }
    // Range check.
    if field.min.is_some() || field.max.is_some() {
        if let Some(n) = value.as_f64() {
            let min = field.min.unwrap_or(f64::NEG_INFINITY);
            let max = field.max.unwrap_or(f64::INFINITY);
            if n < min || n > max {
                return Err(ConfigError::OutOfRange {
                    key: key.to_owned(),
                    value: n,
                    min,
                    max,
                });
            }
        }
    }
    // Regex check.
    if let (ConfigValue::String(s), Some(pattern)) = (value, &field.regex) {
        if !simple_regex_match(pattern, s) {
            return Err(ConfigError::ValidationFailed(format!(
                "key '{key}': value '{s}' does not match pattern '{pattern}'"
            )));
        }
    }
    Ok(())
}

/// A minimal regex check that only supports anchored prefix/suffix and `.*`.
/// For production use, replace with the `regex` crate.
fn simple_regex_match(pattern: &str, text: &str) -> bool {
    // Handle `^...$`, `^...`, `...$`, and `.*` anywhere.
    let anchored_start = pattern.starts_with('^');
    let anchored_end = pattern.ends_with('$');
    let core = pattern
        .trim_start_matches('^')
        .trim_end_matches('$');

    if core.contains(".*") {
        // Split on `.*` and check that each segment appears in order.
        let segments: Vec<&str> = core.split(".*").collect();
        let mut pos = 0usize;
        for (i, seg) in segments.iter().enumerate() {
            if seg.is_empty() {
                continue;
            }
            if i == 0 && anchored_start {
                if !text.starts_with(seg) {
                    return false;
                }
                pos = seg.len();
            } else if i == segments.len() - 1 && anchored_end {
                if !text.ends_with(seg) {
                    return false;
                }
            } else if let Some(found) = text[pos..].find(seg) {
                pos += found + seg.len();
            } else {
                return false;
            }
        }
        return true;
    }

    // No `.*` — literal match.
    match (anchored_start, anchored_end) {
        (true, true) => text == core,
        (true, false) => text.starts_with(core),
        (false, true) => text.ends_with(core),
        (false, false) => text.contains(core),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn int_field(required: bool, min: Option<f64>, max: Option<f64>) -> SchemaField {
        SchemaField {
            required,
            value_type: ValueType::Int,
            min,
            max,
            regex: None,
        }
    }

    #[test]
    fn get_set_roundtrip() {
        let cfg = DynamicConfig::new();
        cfg.set("port", ConfigValue::Int(8080)).unwrap();
        assert_eq!(cfg.get("port"), Some(ConfigValue::Int(8080)));
    }

    #[test]
    fn get_missing_returns_none() {
        let cfg = DynamicConfig::new();
        assert_eq!(cfg.get("nonexistent"), None);
    }

    #[test]
    fn schema_rejects_wrong_type() {
        let mut schema = ConfigSchema::new();
        schema.add_field("port", int_field(false, None, None));
        let cfg = DynamicConfig::with_schema(schema);
        let result = cfg.set("port", ConfigValue::String("8080".to_owned()));
        assert!(matches!(result, Err(ConfigError::TypeMismatch { .. })));
    }

    #[test]
    fn schema_rejects_out_of_range() {
        let mut schema = ConfigSchema::new();
        schema.add_field("workers", int_field(false, Some(1.0), Some(64.0)));
        let cfg = DynamicConfig::with_schema(schema);
        let result = cfg.set("workers", ConfigValue::Int(128));
        assert!(matches!(result, Err(ConfigError::OutOfRange { .. })));
        cfg.set("workers", ConfigValue::Int(8)).unwrap();
    }

    #[test]
    fn schema_accepts_valid_value() {
        let mut schema = ConfigSchema::new();
        schema.add_field("workers", int_field(false, Some(1.0), Some(64.0)));
        let cfg = DynamicConfig::with_schema(schema);
        cfg.set("workers", ConfigValue::Int(16)).unwrap();
        assert_eq!(cfg.get("workers"), Some(ConfigValue::Int(16)));
    }

    #[test]
    fn reload_returns_changed_keys() {
        let cfg = DynamicConfig::new();
        cfg.set("a", ConfigValue::Int(1)).unwrap();
        cfg.set("b", ConfigValue::Bool(true)).unwrap();

        let mut new_map = HashMap::new();
        new_map.insert("a".to_owned(), ConfigValue::Int(2)); // changed
        new_map.insert("c".to_owned(), ConfigValue::Int(3)); // added

        let changed = cfg.reload_from_map(new_map).unwrap();
        assert!(changed.contains(&"a".to_owned()), "a was changed");
        assert!(changed.contains(&"b".to_owned()), "b was removed");
        assert!(changed.contains(&"c".to_owned()), "c was added");
        assert!(!changed.contains(&"d".to_owned()));
    }

    #[test]
    fn reload_rejects_missing_required() {
        let mut schema = ConfigSchema::new();
        schema.add_field("host", SchemaField {
            required: true,
            value_type: ValueType::String,
            min: None,
            max: None,
            regex: None,
        });
        let cfg = DynamicConfig::with_schema(schema);
        let result = cfg.reload_from_map(HashMap::new());
        assert!(matches!(result, Err(ConfigError::MissingRequired(_))));
    }

    #[tokio::test]
    async fn subscribe_bumps_on_change() {
        let cfg = DynamicConfig::new();
        let mut rx = cfg.subscribe();

        let v0 = *rx.borrow();
        cfg.set("x", ConfigValue::Bool(false)).unwrap();
        rx.changed().await.unwrap();
        let v1 = *rx.borrow();
        assert!(v1 > v0, "version should increase after set");
    }

    #[tokio::test]
    async fn subscribe_bumps_on_reload() {
        let cfg = DynamicConfig::new();
        let mut rx = cfg.subscribe();

        let v0 = *rx.borrow();
        let mut m = HashMap::new();
        m.insert("key".to_owned(), ConfigValue::Int(42));
        cfg.reload_from_map(m).unwrap();
        rx.changed().await.unwrap();
        let v1 = *rx.borrow();
        assert!(v1 > v0);
    }

    #[test]
    fn validate_all_finds_missing_required() {
        let mut schema = ConfigSchema::new();
        schema.add_field("host", SchemaField {
            required: true,
            value_type: ValueType::String,
            min: None,
            max: None,
            regex: None,
        });
        let cfg = DynamicConfig::new();
        let errors = cfg.validate_all(&schema);
        assert!(!errors.is_empty());
        assert!(matches!(&errors[0], ConfigError::MissingRequired(k) if k == "host"));
    }

    #[test]
    fn regex_field_validation() {
        // Use a simple anchored pattern that the built-in matcher supports.
        // The pattern "^prod" matches strings starting with "prod".
        let mut schema = ConfigSchema::new();
        schema.add_field("env", SchemaField {
            required: false,
            value_type: ValueType::String,
            min: None,
            max: None,
            regex: Some("^prod$".to_owned()),
        });
        let cfg = DynamicConfig::with_schema(schema);
        // Exact match succeeds.
        cfg.set("env", ConfigValue::String("prod".to_owned())).unwrap();
        // Non-matching value is rejected.
        let bad = cfg.set("env", ConfigValue::String("production".to_owned()));
        assert!(matches!(bad, Err(ConfigError::ValidationFailed(_))));
        // Another non-matching value is also rejected.
        let bad2 = cfg.set("env", ConfigValue::String("staging".to_owned()));
        assert!(matches!(bad2, Err(ConfigError::ValidationFailed(_))));
    }
}
