//! Wait-only match cardinality. An ordinal is not an action target or snapshot index.
use serde::{Deserialize, Serialize};
use std::{marker::PhantomData, ops::Deref};

/// Zero-based ordinal among matches; index 2 requires at least three matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct WaitIndex(#[cfg_attr(feature = "schema", schemars(range(min = 0, max = 199)))] u16);
impl WaitIndex {
    pub fn required_matches(self) -> usize {
        usize::from(self.0) + 1
    }
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
    /// Known matches prove presence even in a partial tree. Unknown or omitted
    /// candidates prevent proving shortage; a known unrelated row does not.
    pub fn assess(self, known: usize, unknown: usize, complete: bool) -> Option<bool> {
        let required = self.required_matches();
        if known >= required {
            Some(true)
        } else if !complete || known.saturating_add(unknown) >= required {
            None
        } else {
            Some(false)
        }
    }
}
impl<'de> Deserialize<'de> for WaitIndex {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let index = u16::deserialize(deserializer)?;
        if index > 199 {
            return Err(serde::de::Error::custom(
                "wait index must be an integer 0..199",
            ));
        }
        Ok(Self(index))
    }
}

/// Flat legacy locator fields plus a wait-only bounded ordinal. Custom parsing
/// preserves strict unknown/duplicate-field rejection with flattened schema.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WaitSelector<L> {
    #[serde(flatten)]
    pub locator: L,
    #[serde(default, skip_serializing_if = "WaitIndex::is_zero")]
    pub index: WaitIndex,
}
impl<L> Deref for WaitSelector<L> {
    type Target = L;
    fn deref(&self) -> &L {
        &self.locator
    }
}
impl<L> From<L> for WaitSelector<L> {
    fn from(locator: L) -> Self {
        Self {
            locator,
            index: WaitIndex::default(),
        }
    }
}
impl<'de, L: serde::de::DeserializeOwned> Deserialize<'de> for WaitSelector<L> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<L>(PhantomData<L>);
        impl<'de, L: serde::de::DeserializeOwned> serde::de::Visitor<'de> for ObjectVisitor<L> {
            type Value = WaitSelector<L>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a wait selector object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate wait selector field"));
                    }
                }
                let index = match fields.remove("index") {
                    Some(value) => {
                        serde_json::from_value(value).map_err(serde::de::Error::custom)?
                    }
                    None => WaitIndex::default(),
                };
                let locator = serde_json::from_value(serde_json::Value::Object(fields))
                    .map_err(serde::de::Error::custom)?;
                Ok(WaitSelector { locator, index })
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Locator {
        label: String,
    }
    #[test]
    fn ordinal_threshold_uses_known_unknown_and_complete_counts() {
        let index: WaitIndex = serde_json::from_str("2").unwrap();
        for (known, unknown, complete, expected) in [
            (3, 0, true, Some(true)),
            (3, 8, false, Some(true)),
            (2, 0, true, Some(false)),
            (2, 1, true, None),
            (0, 2, true, Some(false)),
            (2, 0, false, None),
            (0, 0, false, None),
        ] {
            assert_eq!(index.assess(known, unknown, complete), expected);
        }
    }
    #[test]
    fn flat_selector_is_strict_and_legacy_serialization_stays_flat() {
        let plain: WaitSelector<Locator> = serde_json::from_str(r#"{"label":"Save"}"#).unwrap();
        assert_eq!(
            serde_json::to_value(plain).unwrap(),
            serde_json::json!({"label":"Save"})
        );
        let chosen: WaitSelector<Locator> =
            serde_json::from_str(r#"{"label":"Save","index":199}"#).unwrap();
        assert_eq!(chosen.index.required_matches(), 200);
        assert_eq!(
            serde_json::to_value(chosen).unwrap(),
            serde_json::json!({"label":"Save","index":199})
        );
        for bad in [
            r#"{"label":"Save","index":true}"#,
            r#"{"label":"Save","index":null}"#,
            r#"{"label":"Save","index":-1}"#,
            r#"{"label":"Save","index":200}"#,
            r#"{"label":"Save","index":1.5}"#,
            r#"{"label":"Save","index":"1"}"#,
            r#"{"label":"Save","index":1,"index":2}"#,
            r#"{"label":"Save","label":"Other"}"#,
            r#"{"label":"Save","predicate":"TRUEPREDICATE"}"#,
            r#"["Save",1]"#,
        ] {
            assert!(
                serde_json::from_str::<WaitSelector<Locator>>(bad).is_err(),
                "{bad}"
            );
        }
    }
}
