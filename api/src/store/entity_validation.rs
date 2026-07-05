// Registry validation for custom entities — the enforcement boundary.
// Postgres holds `entities.fields` as JSONB but does not check it (the same
// deal tasks.status has: TEXT column, Rust enum). Everything here is pure and
// DB-free so it unit-tests without a pool; ref-existence is the one async
// step and lives in custom_entities.rs (`ref_target_exists`), driven by the
// RefCheck list this module returns.

use std::collections::HashSet;

use hive_shared::{EntityField, FieldType, RESERVED_KIND_SLUGS};
use serde_json::{Map, Value};

/// One structured validation failure; routes serialize the whole list as
/// `{"error": "validation failed", "issues": [...]}` and MCP renders it as text.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldIssue {
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

impl FieldIssue {
    fn new(field: &str, code: &'static str, message: String) -> Self {
        Self {
            field: field.to_string(),
            code,
            message,
        }
    }
}

/// A ref-typed value that still needs its existence checked against the DB.
#[derive(Debug, Clone)]
pub struct RefCheck {
    pub field: String,
    pub kind: String,
    pub id: String,
}

/// Both type and field slugs: lowercase, starts with a letter, 2-32 chars.
fn slug_shape_ok(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    (2..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Type slugs also stay off the reserved list (built-ins, planned corpora,
/// infra nouns) — a collision there would mislabel seam rows forever.
pub fn validate_type_slug(slug: &str) -> Result<(), FieldIssue> {
    if !slug_shape_ok(slug) {
        return Err(FieldIssue::new(
            "slug",
            "bad_slug",
            format!("type slug '{slug}' must match ^[a-z][a-z0-9_-]{{1,31}}$"),
        ));
    }
    if RESERVED_KIND_SLUGS.contains(&slug) {
        return Err(FieldIssue::new(
            "slug",
            "bad_slug",
            format!("'{slug}' is a reserved kind name"),
        ));
    }
    Ok(())
}

pub fn validate_field_slug(slug: &str) -> Result<(), FieldIssue> {
    if !slug_shape_ok(slug) {
        return Err(FieldIssue::new(
            slug,
            "bad_slug",
            format!("field slug '{slug}' must match ^[a-z][a-z0-9_-]{{1,31}}$"),
        ));
    }
    Ok(())
}

/// The Node slugify convention, reused for defaulting a slug from a label.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true; // suppress leading dashes
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Shallow-merge a patch into the current fields map. Serde has already
/// dropped absent keys, so every key present in the patch was deliberately
/// sent: a JSON null clears the key, anything else replaces it. Returns the
/// merged map plus the set of touched keys (validation only shape-checks
/// touched keys, so an instance holding a since-removed choice option stays
/// readable and updatable until someone touches that specific field).
pub fn merge_fields(
    current: &Map<String, Value>,
    patch: &Map<String, Value>,
) -> (Map<String, Value>, HashSet<String>) {
    let mut merged = current.clone();
    let mut touched = HashSet::new();
    for (k, v) in patch {
        touched.insert(k.clone());
        if v.is_null() {
            merged.remove(k);
        } else {
            merged.insert(k.clone(), v.clone());
        }
    }
    (merged, touched)
}

/// A date value: YYYY-MM-DD, or a full ISO-8601 UTC timestamp. Lexicographic
/// ordering is the house sort, so shapes must stay comparable per-field; both
/// accepted forms are.
fn date_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let ymd_ok = |b: &[u8]| {
        b.len() == 10
            && b[..4].iter().all(u8::is_ascii_digit)
            && b[4] == b'-'
            && b[5..7].iter().all(u8::is_ascii_digit)
            && b[7] == b'-'
            && b[8..10].iter().all(u8::is_ascii_digit)
    };
    if ymd_ok(b) {
        return true;
    }
    // Full timestamp: date, 'T', time, trailing 'Z'.
    b.len() >= 20 && ymd_ok(&b[..10]) && b[10] == b'T' && b[b.len() - 1] == b'Z'
}

/// Pure checks against the registry: unknown keys rejected, per-type shape on
/// TOUCHED keys only, required-presence on the MERGED result. Archived fields
/// are accepted if present (stale clients shouldn't 400) but never required.
/// Ref fields get shape-checked here and returned for async existence checks.
pub fn validate_fields(
    specs: &[EntityField],
    merged: &Map<String, Value>,
    touched: &HashSet<String>,
) -> Result<Vec<RefCheck>, Vec<FieldIssue>> {
    let mut issues = Vec::new();
    let mut refs = Vec::new();

    for key in merged.keys() {
        if !specs.iter().any(|f| f.slug == *key) {
            issues.push(FieldIssue::new(
                key,
                "unknown_field",
                format!("'{key}' is not a field of this type"),
            ));
        }
    }

    for spec in specs {
        let value = merged.get(&spec.slug);
        if spec.required && !spec.archived && value.is_none() {
            issues.push(FieldIssue::new(
                &spec.slug,
                "required",
                format!("'{}' is required", spec.slug),
            ));
            continue;
        }
        let Some(v) = value else { continue };
        if !touched.contains(&spec.slug) {
            continue; // stored values are re-checked only when touched
        }
        match spec.field_type {
            FieldType::Text => {
                if !v.is_string() {
                    issues.push(FieldIssue::new(
                        &spec.slug,
                        "wrong_type",
                        format!("'{}' must be a string", spec.slug),
                    ));
                }
            }
            FieldType::Number => {
                if !v.is_number() {
                    issues.push(FieldIssue::new(
                        &spec.slug,
                        "wrong_type",
                        format!("'{}' must be a number", spec.slug),
                    ));
                }
            }
            FieldType::Bool => {
                if !v.is_boolean() {
                    issues.push(FieldIssue::new(
                        &spec.slug,
                        "wrong_type",
                        format!("'{}' must be true or false", spec.slug),
                    ));
                }
            }
            FieldType::Date => match v.as_str() {
                Some(s) if date_ok(s) => {}
                _ => issues.push(FieldIssue::new(
                    &spec.slug,
                    "bad_date",
                    format!("'{}' must be YYYY-MM-DD or an ISO-8601 UTC timestamp", spec.slug),
                )),
            },
            FieldType::Choice => match v.as_str() {
                Some(s) if spec.options.iter().any(|o| o == s) => {}
                _ => issues.push(FieldIssue::new(
                    &spec.slug,
                    "bad_choice",
                    format!("'{}' must be one of: {}", spec.slug, spec.options.join(", ")),
                )),
            },
            FieldType::Ref => match (v.as_str(), spec.ref_kind.as_deref()) {
                (Some(id), Some(kind)) if !id.is_empty() => refs.push(RefCheck {
                    field: spec.slug.clone(),
                    kind: kind.to_string(),
                    id: id.to_string(),
                }),
                _ => issues.push(FieldIssue::new(
                    &spec.slug,
                    "wrong_type",
                    format!("'{}' must be an id string", spec.slug),
                )),
            },
        }
    }

    if issues.is_empty() {
        Ok(refs)
    } else {
        Err(issues)
    }
}

/// Text worth feeding the FTS row: text/choice/date values (numbers and bools
/// aren't useful tokens), newline-joined in field order.
pub fn searchable_text(specs: &[EntityField], fields: &Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for spec in specs {
        if matches!(
            spec.field_type,
            FieldType::Text | FieldType::Choice | FieldType::Date
        ) {
            if let Some(s) = fields.get(&spec.slug).and_then(Value::as_str) {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(slug: &str, ft: FieldType) -> EntityField {
        EntityField {
            id: format!("efield_{slug}"),
            slug: slug.to_string(),
            label: slug.to_string(),
            field_type: ft,
            required: false,
            position: 0,
            options: if ft == FieldType::Choice {
                vec!["a".into(), "b".into()]
            } else {
                Vec::new()
            },
            ref_kind: if ft == FieldType::Ref {
                Some("person".to_string())
            } else {
                None
            },
            archived: false,
        }
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn merge_clears_on_null_and_tracks_touched() {
        let current = map(json!({"a": 1, "b": "x"}));
        let patch = map(json!({"b": null, "c": true}));
        let (merged, touched) = merge_fields(&current, &patch);
        assert_eq!(merged.get("a"), Some(&json!(1)));
        assert!(merged.get("b").is_none());
        assert_eq!(merged.get("c"), Some(&json!(true)));
        assert!(touched.contains("b") && touched.contains("c") && !touched.contains("a"));
    }

    #[test]
    fn unknown_keys_rejected() {
        let specs = [spec("n", FieldType::Number)];
        let (merged, touched) = merge_fields(&Map::new(), &map(json!({"nope": 1})));
        let err = validate_fields(&specs, &merged, &touched).unwrap_err();
        assert_eq!(err[0].code, "unknown_field");
    }

    #[test]
    fn typed_checks_fire_only_on_touched() {
        let specs = [spec("n", FieldType::Number)];
        // Stored garbage untouched by this patch passes...
        let stored = map(json!({"n": "not a number"}));
        let (merged, touched) = merge_fields(&stored, &Map::new());
        assert!(validate_fields(&specs, &merged, &touched).is_ok());
        // ...but touching it re-validates.
        let (merged, touched) = merge_fields(&stored, &map(json!({"n": "still not"})));
        let err = validate_fields(&specs, &merged, &touched).unwrap_err();
        assert_eq!(err[0].code, "wrong_type");
    }

    #[test]
    fn required_checked_on_merged_result() {
        let mut required = spec("t", FieldType::Text);
        required.required = true;
        let (merged, touched) = merge_fields(&Map::new(), &Map::new());
        let err = validate_fields(&[required.clone()], &merged, &touched).unwrap_err();
        assert_eq!(err[0].code, "required");
        // Archived fields are never required.
        required.archived = true;
        assert!(validate_fields(&[required], &merged, &touched).is_ok());
    }

    #[test]
    fn choice_and_date_shapes() {
        let specs = [spec("c", FieldType::Choice), spec("d", FieldType::Date)];
        let patch = map(json!({"c": "b", "d": "2026-07-04"}));
        let (merged, touched) = merge_fields(&Map::new(), &patch);
        assert!(validate_fields(&specs, &merged, &touched).is_ok());
        let patch = map(json!({"c": "nope", "d": "July 4th"}));
        let (merged, touched) = merge_fields(&Map::new(), &patch);
        let err = validate_fields(&specs, &merged, &touched).unwrap_err();
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn refs_deferred_for_async_check() {
        let specs = [spec("owner", FieldType::Ref)];
        let (merged, touched) = merge_fields(&Map::new(), &map(json!({"owner": "person_abc"})));
        let refs = validate_fields(&specs, &merged, &touched).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "person");
    }

    #[test]
    fn slug_rules() {
        assert!(validate_type_slug("recipe").is_ok());
        assert!(validate_type_slug("mail").is_err()); // reserved
        assert!(validate_type_slug("task").is_err()); // built-in
        assert!(validate_type_slug("Recipe").is_err()); // uppercase
        assert!(validate_type_slug("x").is_err()); // too short
        assert_eq!(slugify("Household Gear!"), "household-gear");
    }
}
