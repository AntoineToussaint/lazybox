//! Token → USD pricing for agent LLM usage.
//!
//! The metering proxy parses token counts off an agent's response stream but
//! has no way to price them — the vendor's rate card lives here. Prices are
//! expressed in **US dollars per million tokens** (the unit every provider
//! publishes), and the one public entry point,
//! [`cost_micros`], returns cost in **millionths of a USD** (`u64`) to match
//! the `cost_usd_micros` wire field — integer money, no float drift across the
//! IPC boundary.
//!
//! Matching is **longest-prefix over the model id** (`claude-opus-4-8` matches
//! the `claude-opus` entry), so a new point-release of a known family is priced
//! without a table edit. A caller-supplied override map (from
//! `agent.pricing` in the YAML config) is consulted first, so a brand-new model
//! — or a negotiated rate — needs no recompile. An unknown model returns
//! `None`: cost stays absent rather than silently wrong.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The four token buckets a single response can bill, kept apart because cache
/// reads and cache writes price very differently from fresh input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// A model's rate card, in **USD per million tokens**. `f64` is fine here: it
/// is human-facing config and an intermediate for the integer-micros result,
/// never itself serialized as money.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Fresh (uncached) input tokens.
    pub input: f64,
    /// Output/completion tokens.
    pub output: f64,
    /// Writing a prompt-cache entry (Anthropic bills ~1.25× input; providers
    /// without a separate cache-write charge set this equal to `input`).
    #[serde(default)]
    pub cache_write: f64,
    /// Reading from the prompt cache (the discounted rate).
    #[serde(default)]
    pub cache_read: f64,
}

impl ModelPrice {
    /// Cost of `counts` under this rate card, in micro-USD (rounded).
    pub fn cost_micros(&self, counts: &TokenCounts) -> u64 {
        // dollars-per-Mtok × tokens == micro-USD directly:
        //   cost_usd      = tokens / 1e6 × price
        //   cost_micros   = cost_usd × 1e6 = tokens × price
        let dollars_per_mtok = |tokens: u64, price: f64| tokens as f64 * price;
        let total = dollars_per_mtok(counts.input, self.input)
            + dollars_per_mtok(counts.output, self.output)
            + dollars_per_mtok(counts.cache_creation, self.cache_write)
            + dollars_per_mtok(counts.cache_read, self.cache_read);
        total.round().max(0.0) as u64
    }
}

/// Built-in rate cards keyed by model-id prefix, most-specific first is NOT
/// required — [`price_for`] always picks the longest matching key. Values are
/// the published list prices (USD / million tokens) at the time of writing;
/// override via `agent.pricing` for negotiated rates or new models.
fn builtin_prices() -> &'static [(&'static str, ModelPrice)] {
    &[
        // ── Anthropic (Claude) ─────────────────────────────────────────
        (
            "claude-opus",
            ModelPrice {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.50,
            },
        ),
        (
            "claude-sonnet",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_write: 3.75,
                cache_read: 0.30,
            },
        ),
        (
            "claude-haiku",
            ModelPrice {
                input: 0.80,
                output: 4.0,
                cache_write: 1.0,
                cache_read: 0.08,
            },
        ),
        // Older dotted ids (`claude-3-5-haiku…`, `claude-3-opus…`) so a
        // pinned legacy model still prices.
        (
            "claude-3-opus",
            ModelPrice {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.50,
            },
        ),
        (
            "claude-3-5-haiku",
            ModelPrice {
                input: 0.80,
                output: 4.0,
                cache_write: 1.0,
                cache_read: 0.08,
            },
        ),
        // ── OpenAI (Codex / GPT) ───────────────────────────────────────
        // OpenAI has no separate cache-write charge; cache_write == input.
        (
            "gpt-4o-mini",
            ModelPrice {
                input: 0.15,
                output: 0.60,
                cache_write: 0.15,
                cache_read: 0.075,
            },
        ),
        (
            "gpt-4o",
            ModelPrice {
                input: 2.50,
                output: 10.0,
                cache_write: 2.50,
                cache_read: 1.25,
            },
        ),
        (
            "o1",
            ModelPrice {
                input: 15.0,
                output: 60.0,
                cache_write: 15.0,
                cache_read: 7.50,
            },
        ),
    ]
}

/// The rate card for `model`: the longest-prefix match in `overrides` first,
/// then the built-in table. `None` when nothing matches.
pub fn price_for(model: &str, overrides: &BTreeMap<String, ModelPrice>) -> Option<ModelPrice> {
    let longest =
        |pairs: &mut dyn Iterator<Item = (&str, ModelPrice)>| -> Option<(usize, ModelPrice)> {
            pairs
                .filter(|(prefix, _)| model.starts_with(*prefix))
                .map(|(prefix, price)| (prefix.len(), price))
                .max_by_key(|(len, _)| *len)
        };

    let override_hit = longest(&mut overrides.iter().map(|(k, v)| (k.as_str(), *v)));
    let builtin_hit = longest(&mut builtin_prices().iter().map(|(k, v)| (*k, *v)));

    match (override_hit, builtin_hit) {
        // An override always wins when it matches at all — even a shorter
        // prefix than the built-in — so a user can retune one family without
        // out-prefixing our keys.
        (Some((_, price)), _) => Some(price),
        (None, Some((_, price))) => Some(price),
        (None, None) => None,
    }
}

/// Cost of `counts` for `model` in micro-USD, or `None` when the model is not
/// priced (unknown to both the overrides and the built-in table).
pub fn cost_micros(
    model: &str,
    counts: &TokenCounts,
    overrides: &BTreeMap<String, ModelPrice>,
) -> Option<u64> {
    price_for(model, overrides).map(|price| price.cost_micros(counts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> BTreeMap<String, ModelPrice> {
        BTreeMap::new()
    }

    #[test]
    fn prices_a_million_input_tokens_at_list_rate() {
        // 1M Sonnet input tokens == $3.00 == 3_000_000 micros.
        let counts = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-sonnet-4-5", &counts, &no_overrides()),
            Some(3_000_000)
        );
    }

    #[test]
    fn sums_every_token_bucket() {
        // Opus: 15 in / 75 out / 18.75 write / 1.50 read, 1M of each.
        let counts = TokenCounts {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
        };
        // (15 + 75 + 18.75 + 1.50) == $110.25 == 110_250_000 micros.
        assert_eq!(
            cost_micros("claude-opus-4-8", &counts, &no_overrides()),
            Some(110_250_000)
        );
    }

    #[test]
    fn longest_prefix_wins_over_shorter() {
        // `claude-3-5-haiku` and `claude-haiku` both price at the same rate;
        // the point is the longer key is selected, proven with a distinct
        // override on the longer prefix.
        let mut overrides = no_overrides();
        overrides.insert(
            "claude-3-5-haiku".into(),
            ModelPrice {
                input: 1.0,
                output: 1.0,
                cache_write: 1.0,
                cache_read: 1.0,
            },
        );
        let counts = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-3-5-haiku-20241022", &counts, &overrides),
            Some(1_000_000)
        );
    }

    #[test]
    fn override_beats_builtin() {
        let mut overrides = no_overrides();
        overrides.insert(
            "claude-sonnet".into(),
            ModelPrice {
                input: 99.0,
                output: 99.0,
                cache_write: 99.0,
                cache_read: 99.0,
            },
        );
        let counts = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-sonnet-4-5", &counts, &overrides),
            Some(99_000_000)
        );
    }

    #[test]
    fn unknown_model_is_unpriced() {
        let counts = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("some-future-model", &counts, &no_overrides()),
            None
        );
    }

    #[test]
    fn override_prices_an_otherwise_unknown_model() {
        let mut overrides = no_overrides();
        overrides.insert(
            "some-future-model".into(),
            ModelPrice {
                input: 10.0,
                output: 10.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let counts = TokenCounts {
            output: 2_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("some-future-model-v2", &counts, &overrides),
            Some(20_000_000)
        );
    }

    #[test]
    fn openai_cached_read_is_discounted() {
        // gpt-4o: cache_read 1.25/Mtok.
        let counts = TokenCounts {
            cache_read: 4_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("gpt-4o-2024-08-06", &counts, &no_overrides()),
            Some(5_000_000)
        );
    }
}
