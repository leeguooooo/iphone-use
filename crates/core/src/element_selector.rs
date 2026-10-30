//! Literal field selectors for a fresh accessibility tree. No expression evaluation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Exact label, preserving case, whitespace and newlines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Case-sensitive substring of the label. Combined with other fields using AND.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_contains: Option<String>,
    /// Exact accessibility value. An empty string matches a present empty value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
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
                    "label",
                    "label_contains",
                    "value",
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
                    label: take(&mut fields, "label")?,
                    label_contains: take(&mut fields, "label_contains")?,
                    value: take(&mut fields, "value")?,
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
            && self.label.is_none()
            && self.label_contains.is_none()
            && self.value.is_none()
            && self.enabled.is_none()
        {
            return Err("selector needs at least one field: kind, identifier, label, label_contains, value or enabled");
        }
        for text in [
            &self.kind,
            &self.identifier,
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
        if self.identifier.as_deref() == Some("") || self.label_contains.as_deref() == Some("") {
            return Err("identifier and label_contains must be non-empty when supplied");
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
            && exact("label", self.label.as_deref())
            && exact("value", self.value.as_deref())
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
