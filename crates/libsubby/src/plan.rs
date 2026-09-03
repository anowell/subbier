//! Subscription tiers, and how much allowance one is worth relative to another.
//! One percentage stands for a whole pool, and a plain mean is wrong once the
//! accounts differ in size (see [`weighted_pct`]). The weights are estimates from
//! tier name and price, overridable per canonical id in a `plan-weights` block.

use std::collections::BTreeMap;

use crate::model::Provider;

/// One subscription tier, and the allowance it is taken to carry.
///
/// Data and not an enum: the same word means different things to the two
/// vendors (Claude "pro" is an entry tier, ChatGPT "pro" is the $200 one).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanTier {
    pub provider: Provider,
    /// Canonical lowercase id, e.g. `"max-5x"`. Stable: it is the config key and
    /// appears in the JSON snapshot.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Allowance relative to the provider's $20 entry tier.
    pub weight: f32,
}

impl PlanTier {
    /// The tier used when a provider names nothing recognisable. Weight 1, not
    /// 0: a zero would silently drop the account out of every aggregate.
    #[must_use]
    pub const fn unknown(provider: Provider) -> Self {
        Self {
            provider,
            id: "unknown",
            display_name: "Unknown",
            weight: 1.0,
        }
    }

    /// `"claude:max-5x"`.
    #[must_use]
    pub fn config_key(&self) -> String {
        format!("{}:{}", self.provider.id(), self.id)
    }

    /// Resolve a raw plan string (Codex's `plan_type`, Claude's
    /// `subscriptionType`). Case-insensitive, with `_` and ` ` folded to `-`.
    /// An unrecognised string is [`PlanTier::unknown`], never an error.
    #[must_use]
    pub fn resolve(provider: Provider, plan: Option<&str>) -> Self {
        let Some(plan) = plan else {
            return Self::unknown(provider);
        };
        let normalised: String = plan
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c == '_' || c == ' ' { '-' } else { c })
            .collect();
        TIERS
            .iter()
            .find(|t| t.provider == provider && t.id == normalised)
            .copied()
            .or_else(|| {
                ALIASES
                    .iter()
                    .find(|(p, alias, _)| *p == provider && *alias == normalised)
                    .and_then(|(p, _, id)| {
                        TIERS
                            .iter()
                            .find(|t| t.provider == *p && t.id == *id)
                            .copied()
                    })
            })
            .unwrap_or_else(|| Self::unknown(provider))
    }

    /// This tier's weight, after any `plan-weights` override.
    #[must_use]
    pub fn weight_with(&self, overrides: &BTreeMap<String, f32>) -> f32 {
        overrides
            .get(&self.config_key())
            .copied()
            .unwrap_or(self.weight)
            .max(0.0)
    }
}

const TIERS: &[PlanTier] = &[
    PlanTier {
        provider: Provider::Claude,
        id: "free",
        display_name: "Free",
        weight: 0.0,
    },
    PlanTier {
        provider: Provider::Claude,
        id: "pro",
        display_name: "Pro",
        weight: 1.0,
    },
    PlanTier {
        provider: Provider::Claude,
        id: "max-5x",
        display_name: "Max 5x",
        weight: 5.0,
    },
    PlanTier {
        provider: Provider::Claude,
        id: "max-20x",
        display_name: "Max 20x",
        weight: 20.0,
    },
    PlanTier {
        provider: Provider::Claude,
        id: "team",
        display_name: "Team",
        weight: 1.0,
    },
    PlanTier {
        provider: Provider::Claude,
        id: "enterprise",
        display_name: "Enterprise",
        weight: 5.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "free",
        display_name: "Free",
        weight: 0.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "plus",
        display_name: "Plus",
        weight: 1.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "pro",
        display_name: "Pro",
        weight: 10.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "team",
        display_name: "Team",
        weight: 1.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "business",
        display_name: "Business",
        weight: 1.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "enterprise",
        display_name: "Enterprise",
        weight: 5.0,
    },
    PlanTier {
        provider: Provider::Codex,
        id: "edu",
        display_name: "Edu",
        weight: 1.0,
    },
];

/// Alternate spellings of a canonical id.
///
/// Claude's credential blob spells both Max tiers `"max"`, and we resolve
/// *down*: under-weighting a large account pulls the aggregate up, and a
/// too-high reading surprises you now rather than when you run out.
const ALIASES: &[(Provider, &str, &str)] = &[
    (Provider::Claude, "max", "max-5x"),
    (Provider::Claude, "max5x", "max-5x"),
    (Provider::Claude, "max20x", "max-20x"),
    (Provider::Claude, "claude-pro", "pro"),
    (Provider::Claude, "claude-max", "max-5x"),
    (Provider::Codex, "chatgpt-plus", "plus"),
    (Provider::Codex, "chatgpt-pro", "pro"),
    (Provider::Codex, "chatgpt-team", "team"),
];

/// The human name of a canonical tier id. An unrecognised id reads `"Unknown"`,
/// never an empty cell — an account always has *some* plan.
#[must_use]
pub fn display_name(provider: Provider, tier_id: &str) -> &'static str {
    TIERS
        .iter()
        .find(|t| t.provider == provider && t.id == tier_id)
        .map_or(PlanTier::unknown(provider).display_name, |t| t.display_name)
}

/// A mean of `pct` weighted by allowance size, over `(pct, weight)` entries.
///
/// `None` when there is nothing to average or every weight is zero: the caller
/// must render "no number", never `0%`, which reads as "nothing used".
///
/// ```
/// # use libsubby::plan::weighted_pct;
/// let overall = weighted_pct([(10.0, 20.0), (90.0, 1.0)]).unwrap();
/// assert!((overall - 13.8).abs() < 0.1);
/// ```
#[must_use]
pub fn weighted_pct(entries: impl IntoIterator<Item = (f32, f32)>) -> Option<f32> {
    let (weighted, total) = entries
        .into_iter()
        .filter(|(_, w)| *w > 0.0)
        .fold((0.0f32, 0.0f32), |(sum, total), (pct, w)| {
            (sum + pct * w, total + w)
        });
    (total > 0.0).then(|| weighted / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_plan_name_means_different_things_to_the_two_vendors() {
        let claude = PlanTier::resolve(Provider::Claude, Some("pro"));
        let codex = PlanTier::resolve(Provider::Codex, Some("pro"));
        assert_eq!(claude.id, codex.id);
        assert_ne!(claude.weight, codex.weight);
        assert_eq!(display_name(Provider::Claude, "pro"), "Pro");
        assert_eq!(display_name(Provider::Codex, "pro"), "Pro");
    }

    #[test]
    fn spelling_variants_fold_together() {
        for spelling in ["max_20x", "Max 20x", "MAX-20X", "max20x"] {
            assert_eq!(
                PlanTier::resolve(Provider::Claude, Some(spelling)).id,
                "max-20x",
                "{spelling}"
            );
        }
        // The credential blob cannot tell 5x from 20x; guessing high would make
        // the aggregate read lower than reality.
        assert_eq!(
            PlanTier::resolve(Provider::Claude, Some("max")).id,
            "max-5x"
        );
    }

    #[test]
    fn an_unknown_plan_weighs_one_not_zero() {
        let tier = PlanTier::resolve(Provider::Codex, Some("plan-from-2029"));
        assert_eq!(tier.id, "unknown");
        assert_eq!(tier.weight, 1.0);
        assert_eq!(PlanTier::resolve(Provider::Codex, None).weight, 1.0);
        assert_eq!(display_name(Provider::Codex, "max-20x"), "Unknown");
    }

    #[test]
    fn overrides_are_keyed_by_provider_and_id_and_cannot_go_negative() {
        let mut overrides = BTreeMap::new();
        overrides.insert("codex:pro".to_string(), 6.0);
        assert_eq!(
            PlanTier::resolve(Provider::Codex, Some("pro")).weight_with(&overrides),
            6.0
        );
        assert_eq!(
            PlanTier::resolve(Provider::Claude, Some("pro")).weight_with(&overrides),
            1.0,
            "claude:pro is untouched"
        );

        overrides.insert("codex:pro".to_string(), -3.0);
        assert_eq!(
            PlanTier::resolve(Provider::Codex, Some("pro")).weight_with(&overrides),
            0.0,
            "a negative override would invert the mean"
        );
    }

    #[test]
    fn the_aggregate_is_weighted_not_arithmetic() {
        // The plain mean of these two is 50%.
        assert_eq!(
            weighted_pct([(10.0, 20.0), (90.0, 1.0)]).map(f32::round),
            Some(14.0)
        );
        assert_eq!(weighted_pct([(10.0, 1.0), (90.0, 1.0)]), Some(50.0));
    }

    #[test]
    fn nothing_to_average_is_none_not_zero() {
        assert_eq!(weighted_pct([]), None);
        assert_eq!(
            weighted_pct([(50.0, 0.0)]),
            None,
            "free tiers carry no allowance"
        );
    }

    #[test]
    fn the_table_is_addressable_and_every_alias_points_at_a_real_tier() {
        let mut keys: Vec<String> = TIERS.iter().map(PlanTier::config_key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate config key");

        for (provider, alias, id) in ALIASES {
            assert!(
                TIERS.iter().any(|t| t.provider == *provider && t.id == *id),
                "alias {alias} -> {id} has no tier"
            );
        }
        for tier in TIERS {
            assert_eq!(display_name(tier.provider, tier.id), tier.display_name);
        }
    }
}
