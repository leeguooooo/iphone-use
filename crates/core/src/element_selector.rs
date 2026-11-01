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
    /// Zero-based ordinal among matching AX rows, 0..199; not a snapshot row index.
    /// Requires other criteria and a proven tree-order rank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "crate::match_ordinal::MatchOrdinal"))]
    pub index: Option<crate::match_ordinal::MatchOrdinal>,
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
    /// Exact exposed accessibility value; truncated, unavailable and secure values are unknown, never matches.
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
                    "index",
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
                let index = fields.remove("index").map(serde_json::from_value)
                    .transpose().map_err(serde::de::Error::custom)?;
                Ok(ElementSelector {
                    index,
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

    /// Matching an observed field is separate from proving an ordinal's rank.
    pub fn matches(&self, row: &Value) -> bool {
        self.assess(row) == Some(true)
    }

    pub fn assess(&self, row: &Value) -> Option<bool> {
        let exact = |field: &str, expected: Option<&str>| {
            expected.is_none_or(|text| row.get(field).and_then(Value::as_str) == Some(text))
        };
        if self.kind.as_deref().is_some_and(|expected| row.get("kind").and_then(Value::as_str).map(|kind|kind.strip_prefix("XCUIElementType").unwrap_or(kind)) != Some(expected.strip_prefix("XCUIElementType").unwrap_or(expected)))
            || !exact("identifier", self.identifier.as_deref())
            || !exact("label", self.label.as_deref())
            || self.name.as_deref().is_some_and(|expected| {
                row.get("identifier").and_then(Value::as_str).filter(|id|!id.is_empty())
                    .or_else(||row.get("label").and_then(Value::as_str)) != Some(expected)
            })
            || self.label_contains.as_deref().is_some_and(|expected| {
                !row.get("label").and_then(Value::as_str).is_some_and(|label|label.contains(expected))
            }) { return Some(false); }
        let mut unknown = false;
        let unavailable = row.get("snapshot_value_unavailable").is_some_and(|flag|flag != &Value::Bool(false));
        let truncated = row.get("value_truncated").is_some_and(|flag|flag != &Value::Bool(false));
        let malformed_value = row.get("value").is_some_and(|value|!matches!(value,Value::Null|Value::String(_)));
        let secure = row.get("kind").and_then(Value::as_str).is_some_and(|kind|kind.strip_prefix("XCUIElementType").unwrap_or(kind)=="SecureTextField");
        if let Some(expected) = self.value.as_deref() {
            if unavailable || secure || truncated || malformed_value {
                unknown = true;
            } else if !exact("value",Some(expected)) { return Some(false); }
        }
        if let Some(expected) = self.value_present {
            if unavailable || secure { unknown = true; }
            else {
                match row.get("value") {
                    Some(Value::String(_)) if !expected => return Some(false),
                    Some(Value::String(_)) => {},
                    None | Some(Value::Null) if !truncated => {
                        if expected { return Some(false); }
                    },
                    _ => unknown = true,
                }
            }
        }
        if let Some(expected) = self.enabled {
            match row.get("enabled").and_then(Value::as_bool) {
                Some(value) if value!=expected => return Some(false),
                None => unknown=true,
                _ => {},
            }
        }
        if unknown { None } else { Some(true) }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordinal_requires_criteria_and_strict_nonnullable_bounded_index() {
        for raw in [r#"{"label":"Save","index":0}"#,r#"{"label":"Save","index":199}"#] {
            let selector:ElementSelector=serde_json::from_str(raw).unwrap();assert!(selector.validate().is_ok());
            assert!(serde_json::to_value(selector).unwrap().get("index").is_some());
        }
        for raw in [r#"{"index":0}"#,r#"{"index":199}"#] {
            assert!(serde_json::from_str::<ElementSelector>(raw).unwrap().validate().is_err());
        }
        for raw in [r#"{"label":"Save","index":null}"#,r#"{"label":"Save","index":true}"#,r#"{"label":"Save","index":200}"#,r#"{"label":"Save","index":-1}"#,r#"{"label":"Save","index":1.5}"#,r#"{"label":"Save","index":0,"index":1}"#] {
            assert!(serde_json::from_str::<ElementSelector>(raw).is_err(),"{raw}");
        }
    }

    #[cfg(feature="schema")]
    #[test]
    fn ordinal_schema_is_optional_bounded_and_not_nullable() {
        let schema=serde_json::to_value(schemars::schema_for!(ElementSelector)).unwrap();
        let mut index=&schema["properties"]["index"];
        if let Some(reference)=index.get("$ref").and_then(Value::as_str) {index=schema.pointer(reference.strip_prefix('#').unwrap()).unwrap();}
        assert_eq!(index["type"],"integer");assert_eq!(index["minimum"],0);assert_eq!(index["maximum"],199);
        assert!(!schema["required"].as_array().is_some_and(|fields|fields.iter().any(|field|field=="index")));
    }

    #[test]
    fn ordinal_assessment_retains_unknown_state_and_known_mismatch() {
        let selector:ElementSelector=serde_json::from_value(json!({"label":"Save","enabled":false,"index":1})).unwrap();
        assert_eq!(selector.assess(&json!({"label":"Save"})),None);
        assert_eq!(selector.assess(&json!({"label":"Other"})),Some(false));
        assert_eq!(selector.assess(&json!({"label":"Save","enabled":false})),Some(true));
        assert_eq!(selector.assess(&json!({"kind":"XCUIElementTypeButton","label":"Other"})),Some(false));
        let value:ElementSelector=serde_json::from_value(json!({"value":"prefix","index":0})).unwrap();
        for row in [json!({"value":"prefix","value_truncated":true}),json!({"value":"prefix","kind":"SecureTextField"}),json!({"snapshot_value_unavailable":true}),json!({"value":{}}),json!({"value":"prefix","value_truncated":"unknown"})] {assert_eq!(value.assess(&row),None);}
    }

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
