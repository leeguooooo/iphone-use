//! Literal field selectors for a fresh accessibility tree. No expression evaluation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A literal filter over exposed snapshot metadata.
///
/// ```
/// let selector: core::element_selector::ElementSelector =
///     serde_json::from_str(r#"{"value_present": true}"#).unwrap();
/// let row = serde_json::from_str(r#"{"kind":"TextField","value":""}"#).unwrap();
/// assert!(selector.validate().is_ok());
/// assert!(selector.matches(&row));
/// ```
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ElementSelector {
    /// Exact accessibility kind, e.g. Button (XCUIElementTypeButton also accepted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Exact accessibility identifier; case-sensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Exact name: non-empty identifier when present, otherwise label; case-sensitive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Exact label, preserving case, whitespace and newlines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Case-sensitive substring of the label. Combined with other fields using AND.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_contains: Option<String>,
    /// Exact exposed accessibility value; known truncated previews never match.
    /// An empty string matches a present empty value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Whether the snapshot supplies a string value, including empty or truncated
    /// strings. False matches an omitted/null non-secure snapshot value, not an App's
    /// null AX value. Protected SecureTextField rows match neither boolean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_present: Option<bool>,
    /// Exact enabled state. A missing state never matches either boolean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl<'de> Deserialize<'de> for ElementSelector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
            type Value = ElementSelector;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a selector JSON object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                const FIELDS: &[&str] = &[
                    "kind",
                    "identifier",
                    "name",
                    "label",
                    "label_contains",
                    "value",
                    "value_present",
                    "enabled",
                ];
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if !FIELDS.contains(&key.as_str()) {
                        return Err(serde::de::Error::unknown_field(&key, FIELDS));
                    }
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate selector field"));
                    }
                }
                fn take<T: serde::de::DeserializeOwned, E: serde::de::Error>(
                    fields: &mut serde_json::Map<String, Value>,
                    key: &str,
                ) -> Result<Option<T>, E> {
                    match fields.remove(key) {
                        None | Some(Value::Null) => Ok(None),
                        Some(value) => serde_json::from_value(value).map(Some).map_err(E::custom),
                    }
                }
                Ok(ElementSelector {
                    kind: take(&mut fields, "kind")?,
                    identifier: take(&mut fields, "identifier")?,
                    name: take(&mut fields, "name")?,
                    label: take(&mut fields, "label")?,
                    label_contains: take(&mut fields, "label_contains")?,
                    value: take(&mut fields, "value")?,
                    value_present: take(&mut fields, "value_present")?,
                    enabled: take(&mut fields, "enabled")?,
                })
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

impl ElementSelector {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.kind.is_none()
            && self.identifier.is_none()
            && self.name.is_none()
            && self.label.is_none()
            && self.label_contains.is_none()
            && self.value.is_none()
            && self.value_present.is_none()
            && self.enabled.is_none()
        {
            return Err("selector needs at least one field: kind, identifier, name, label, label_contains, value, value_present or enabled");
        }
        if self.value.is_some() && self.value_present == Some(false) {
            return Err("value cannot be combined with value_present:false");
        }
        for text in [
            &self.kind,
            &self.identifier,
            &self.name,
            &self.label,
            &self.label_contains,
            &self.value,
        ]
        .into_iter()
        .flatten()
        {
            if text.chars().count() > 1000 {
                return Err("each selector string is at most 1000 characters");
            }
        }
        if self.identifier.as_deref() == Some("") || self.name.as_deref() == Some("") || self.label_contains.as_deref() == Some("") {
            return Err("identifier, name and label_contains must be non-empty when supplied");
        }
        if let Some(kind) = &self.kind {
            let kind = kind.strip_prefix("XCUIElementType").unwrap_or(kind);
            if kind.is_empty() || kind.len() > 40 || !kind.chars().all(|c| c.is_ascii_alphabetic())
            {
                return Err("kind must be an accessibility type such as Button");
            }
        }
        Ok(())
    }

    pub fn matches(&self, row: &Value) -> bool {
        let exact = |field: &str, expected: Option<&str>| {
            expected.is_none_or(|text| row.get(field).and_then(Value::as_str) == Some(text))
        };
        exact(
            "kind",
            self.kind
                .as_deref()
                .map(|k| k.strip_prefix("XCUIElementType").unwrap_or(k)),
        ) && exact("identifier", self.identifier.as_deref())
            && self.name.as_deref().is_none_or(|expected| {
                row.get("identifier").and_then(Value::as_str).filter(|id| !id.is_empty())
                    .or_else(|| row.get("label").and_then(Value::as_str)) == Some(expected)
            })
            && exact("label", self.label.as_deref())
            && exact("value", self.value.as_deref())
            && (self.value_present.is_none() || row.get("kind").and_then(Value::as_str)
                .is_none_or(|kind| kind.trim_start_matches("XCUIElementType") != "SecureTextField"))
            && self.value_present.is_none_or(|expected| {
                if row.get("snapshot_value_unavailable").is_some_and(|flag| flag != &Value::Bool(false)) { return false; }
                match row.get("value") {
                    Some(Value::String(_)) => expected,
                    None | Some(Value::Null) => !expected
                        && row.get("value_truncated").and_then(Value::as_bool) != Some(true),
                    _ => false,
                }
            })
            && (self.value.is_none()
                || row.get("value_truncated").and_then(Value::as_bool) != Some(true))
            && self.label_contains.as_deref().is_none_or(|text| {
                row.get("label")
                    .and_then(Value::as_str)
                    .is_some_and(|label| label.contains(text))
            })
            && self
                .enabled
                .is_none_or(|enabled| row.get("enabled").and_then(Value::as_bool) == Some(enabled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_prefers_nonempty_identifier_then_label_and_combines_literally() {
        let selector: ElementSelector = serde_json::from_value(json!({"name":"保存\n'O\\K'🙂"})).unwrap();
        assert!(selector.validate().is_ok());
        for row in [
            json!({"identifier":"保存\n'O\\K'🙂","label":"Different"}),
            json!({"label":"保存\n'O\\K'🙂"}),
            json!({"identifier":"","label":"保存\n'O\\K'🙂"}),
            json!({"identifier":null,"label":"保存\n'O\\K'🙂"}),
        ] { assert!(selector.matches(&row), "{row}"); }
        for row in [
            json!({"identifier":"other","label":"保存\n'O\\K'🙂"}),
            json!({"identifier":" 保存\n'O\\K'🙂","label":"保存\n'O\\K'🙂"}),
            json!({"label":"保存 'O\\K'🙂"}),
            json!({"identifier":"保存\n'o\\K'🙂"}), json!({}),
        ] { assert!(!selector.matches(&row), "{row}"); }
        let combined: ElementSelector = serde_json::from_value(json!({"name":"primary","label":"Save","enabled":false})).unwrap();
        assert!(combined.matches(&json!({"identifier":"primary","label":"Save","enabled":false})));
        assert!(!combined.matches(&json!({"identifier":"primary","label":"Other","enabled":false})));
        assert!(!combined.matches(&json!({"identifier":"primary","label":"Save","enabled":true})));
        assert_eq!(serde_json::to_value(&selector).unwrap(),json!({"name":"保存\n'O\\K'🙂"}));
        for value in [json!({"name":""}),json!({"name":"字".repeat(1001)})] {
            assert!(serde_json::from_value::<ElementSelector>(value).unwrap().validate().is_err());
        }
        for raw in [r#"{"name":false}"#,r#"{"name":"one","name":"two"}"#] {
            assert!(serde_json::from_str::<ElementSelector>(raw).is_err());
        }
    }

    #[test]
    fn exact_fields_and_substrings_are_literal_case_sensitive_and_combined() {
        let selector: ElementSelector = serde_json::from_value(json!({
            "kind":"XCUIElementTypeButton", "identifier":"save.primary", "label_contains":"保存\n", "enabled":false
        })).unwrap();
        assert!(selector.validate().is_ok());
        let row = json!({"kind":"Button","identifier":"save.primary","label":"保存\n'O\\K'", "enabled":false});
        assert!(selector.matches(&row));
        for (field, value) in [
            ("identifier", json!("save.secondary")),
            ("kind", json!("Cell")),
            ("enabled", json!(true)),
            ("label", json!("保存 'O\\K'")),
        ] {
            let mut wrong = row.clone();
            wrong[field] = value;
            assert!(!selector.matches(&wrong));
        }
    }

    #[test]
    fn empty_values_and_disabled_states_do_not_match_missing_attributes() {
        let selector: ElementSelector =
            serde_json::from_value(json!({"value":"", "enabled":false})).unwrap();
        assert!(selector.validate().is_ok());
        assert!(selector.matches(&json!({"value":"","enabled":false})));
        for row in [
            json!({"enabled":false}),
            json!({"value":""}),
            json!({"value":null,"enabled":false}),
            json!({"value":"","enabled":"false"}),
        ] {
            assert!(!selector.matches(&row));
        }
    }

    #[test]
    fn truncated_value_previews_cannot_satisfy_an_exact_value_filter() {
        let selector: ElementSelector = serde_json::from_value(json!({"value":"prefix"})).unwrap();
        assert!(!selector.matches(&json!({"value":"prefix","value_truncated":true})));
        assert!(selector.matches(&json!({"value":"prefix","value_truncated":false})));
    }

    #[test]
    fn snapshot_value_presence_is_distinct_from_empty_content_and_complete_ax_null() {
        let present:ElementSelector=serde_json::from_value(json!({"value_present":true})).unwrap();
        let absent:ElementSelector=serde_json::from_value(json!({"value_present":false})).unwrap();
        assert!(present.validate().is_ok());assert!(absent.validate().is_ok());
        for row in [json!({"value":""}),json!({"value":"text"}),json!({"value":"prefix","value_truncated":true})] {
            assert!(present.matches(&row));assert!(!absent.matches(&row));
        }
        // These are snapshot representation facts, not proof of an App's null value.
        for row in [json!({}),json!({"value":null})] {
            assert!(absent.matches(&row));assert!(!present.matches(&row));
        }
        for row in [json!({"value":false}),json!({"value":12}),json!({"value":{}}),json!({"snapshot_value_unavailable":true}),json!({"snapshot_value_unavailable":"true"}),json!({"value_truncated":true}),json!({"value":null,"value_truncated":true})] {
            assert!(!present.matches(&row));assert!(!absent.matches(&row));
        }
        for row in [json!({"kind":"SecureTextField"}),json!({"kind":"SecureTextField","value":""}),json!({"kind":"SecureTextField","value":"••••"})] {
            assert!(!present.matches(&row));assert!(!absent.matches(&row));
        }
        let contradictory:ElementSelector=serde_json::from_value(json!({"value":"","value_present":false})).unwrap();
        assert!(contradictory.validate().is_err());
    }

    #[test]
    fn position_arrays_and_duplicate_object_fields_are_never_selectors() {
        for value in [
            json!(["Button", null, null, null, null, null]),
            json!(["Button"]),
            json!([]),
        ] {
            assert!(serde_json::from_value::<ElementSelector>(value).is_err());
        }
        assert!(
            serde_json::from_str::<ElementSelector>(r#"{"enabled":true,"enabled":false}"#).is_err()
        );
    }

    #[test]
    fn invalid_and_unknown_selectors_are_refused() {
        for value in [
            json!({}),
            json!({"label_contains":""}),
            json!({"identifier":""}),
            json!({"kind":"XCUIElementType"}),
            json!({"value":"字".repeat(1001)}),
        ] {
            let selector: ElementSelector = serde_json::from_value(value).unwrap();
            assert!(selector.validate().is_err());
        }
        for value in [
            json!({"predicate":"TRUEPREDICATE"}),
            json!({"enabled":"false"}),
            json!({"enabled":0}),
        ] {
            assert!(serde_json::from_value::<ElementSelector>(value).is_err());
        }
    }
}
