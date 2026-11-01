//! Selection of one match in tree order, distinct from match cardinality.
use serde::{Deserialize, Serialize};

/// Zero-based ordinal among matches, never a returned snapshot row index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct MatchOrdinal(#[cfg_attr(feature = "schema", schemars(range(min = 0, max = 199)))] u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinalSelection {
    /// Position in the original assessed row sequence, including non-matches.
    Selected(usize),
    Missing,
    Unproven,
}

impl<'de> Deserialize<'de> for MatchOrdinal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let ordinal = u16::deserialize(deserializer)?;
        if ordinal > 199 {
            return Err(serde::de::Error::custom("match index must be an integer 0..199"));
        }
        Ok(Self(ordinal))
    }
}

impl MatchOrdinal {
    pub fn get(self) -> usize {
        usize::from(self.0)
    }

    /// An unknown row before the requested known match may change its rank.
    /// Unknown rows after it cannot. A structurally incomplete tree gives no
    /// guarantee about where omitted matches belong, so it proves no rank.
    pub fn select(self, rows: &[Option<bool>], complete: bool) -> OrdinalSelection {
        if !complete {
            return OrdinalSelection::Unproven;
        }
        let mut known = 0usize;
        let mut unknown = 0usize;
        for (position, assessment) in rows.iter().enumerate() {
            match assessment {
                Some(true) => {
                    if known == self.get() {
                        return if unknown == 0 {
                            OrdinalSelection::Selected(position)
                        } else {
                            OrdinalSelection::Unproven
                        };
                    }
                    known += 1;
                }
                None => unknown += 1,
                Some(false) => {}
            }
        }
        if known.saturating_add(unknown) <= self.get() {
            OrdinalSelection::Missing
        } else {
            OrdinalSelection::Unproven
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordinal(value: usize) -> MatchOrdinal {
        serde_json::from_value(serde_json::json!(value)).unwrap()
    }

    #[test]
    fn retains_original_positions_and_distinguishes_unknown_prefix_from_suffix() {
        let rows = [Some(false), Some(true), None, Some(true), Some(false)];
        assert_eq!(ordinal(0).select(&rows, true), OrdinalSelection::Selected(1));
        assert_eq!(ordinal(1).select(&rows, true), OrdinalSelection::Unproven);
        assert_eq!(ordinal(3).select(&rows, true), OrdinalSelection::Missing);
        assert_eq!(ordinal(0).select(&rows, false), OrdinalSelection::Unproven);
    }

    #[test]
    fn selection_agrees_with_every_possible_completion_of_unknown_rows() {
        // This independent oracle expands every unknown to both booleans and
        // asks whether all completed worlds select the same observed row.
        for length in 0..=7u32 {
            for encoded in 0..3usize.pow(length) {
                let mut code = encoded;
                let rows: Vec<_> = (0..length).map(|_| {
                    let value = match code % 3 { 0 => Some(false), 1 => Some(true), _ => None };
                    code /= 3;
                    value
                }).collect();
                let unknown = rows.iter().filter(|row| row.is_none()).count();
                for index in 0..=7 {
                    let outcomes: Vec<Option<usize>> = (0..1usize << unknown).map(|world| {
                        let mut bit = 0;
                        rows.iter().enumerate().filter_map(|(position, state)| {
                            let matched = match state {
                                Some(value) => *value,
                                None => { let value = world & (1 << bit) != 0; bit += 1; value }
                            };
                            matched.then_some(position)
                        }).nth(index)
                    }).collect();
                    let expected = if outcomes.iter().all(|outcome| outcome == &outcomes[0]) {
                        outcomes[0].map(OrdinalSelection::Selected).unwrap_or(OrdinalSelection::Missing)
                    } else { OrdinalSelection::Unproven };
                    assert_eq!(ordinal(index).select(&rows, true), expected, "{rows:?}, index={index}");
                    assert_eq!(ordinal(index).select(&rows, false), OrdinalSelection::Unproven);
                }
            }
        }
    }

    #[test]
    fn serialized_index_is_strict_and_bounded() {
        for value in [0, 199] {
            let parsed = ordinal(value);
            assert_eq!(parsed.get(), value);
            assert_eq!(serde_json::to_value(parsed).unwrap(), serde_json::json!(value));
        }
        for raw in ["true", "false", "null", "-1", "200", "1.5", "\"1\"", "[]", "{}"] {
            assert!(serde_json::from_str::<MatchOrdinal>(raw).is_err(), "{raw}");
        }
    }
}
