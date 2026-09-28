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

/// The token buckets a single response can bill, kept apart because cache
/// reads and cache writes price very differently from fresh input.
///
/// The buckets are **disjoint**: `input` is uncached input only, as
/// Anthropic reports it. OpenAI's totals include the cached tokens, so its
/// parser subtracts them before building this — pricing a cached token as
/// both fresh and cached input bills it twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    /// Every prompt-cache write, whatever its lifetime.
    pub cache_creation: u64,
    pub cache_read: u64,
    /// The part of `cache_creation` written to the 1-hour cache, which
    /// Anthropic bills at 2× input rather than the 5-minute 1.25×. Claude
    /// Code writes 1-hour entries, so pricing them all at the 5-minute rate
    /// understates what a session costs.
    pub cache_creation_1h: u64,
    /// The response ran in fast mode (`usage.speed == "fast"`), which bills
    /// every bucket at [`FAST_MODE_MULTIPLIER`] × the standard rate.
    pub fast: bool,
}

/// Fast mode's premium over standard pricing, on every bucket: Opus 5.5 is
/// $8/$40 against $4/$20, Opus 5 and 4.8 $10/$50 against $5/$25, and the
/// cache multipliers apply on top of the fast rates.
pub const FAST_MODE_MULTIPLIER: f64 = 2.0;

/// The 1-hour cache write premium over base input, for every Claude model.
pub const ONE_HOUR_CACHE_WRITE_MULTIPLIER: f64 = 2.0;

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
    /// Writing a 1-hour prompt-cache entry. `None` means the published rule
    /// for every Claude model, [`ONE_HOUR_CACHE_WRITE_MULTIPLIER`] × input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

impl ModelPrice {
    /// Cost of `counts` under this rate card, in micro-USD (rounded).
    pub fn cost_micros(&self, counts: &TokenCounts) -> u64 {
        // dollars-per-Mtok × tokens == micro-USD directly:
        //   cost_usd      = tokens / 1e6 × price
        //   cost_micros   = cost_usd × 1e6 = tokens × price
        let dollars_per_mtok = |tokens: u64, price: f64| tokens as f64 * price;
        let one_hour = counts.cache_creation_1h.min(counts.cache_creation);
        let cache_write_1h = self
            .cache_write_1h
            .unwrap_or(self.input * ONE_HOUR_CACHE_WRITE_MULTIPLIER);
        let standard = dollars_per_mtok(counts.input, self.input)
            + dollars_per_mtok(counts.output, self.output)
            + dollars_per_mtok(counts.cache_creation - one_hour, self.cache_write)
            + dollars_per_mtok(one_hour, cache_write_1h)
            + dollars_per_mtok(counts.cache_read, self.cache_read);
        let total = if counts.fast {
            standard * FAST_MODE_MULTIPLIER
        } else {
            standard
        };
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
        // Current-generation list prices (Fable 5/5.1 and Mythos 5/5.1 share
        // the Fable tier; Opus 4.5/4.6/4.7/4.8/5 the Opus tier, Opus 5.5 its
        // own cheaper key; Sonnet 5 is cheaper than Sonnet 4.6 and gets its
        // own longer key; Haiku 4.5). Checked against
        // platform.claude.com/docs/en/about-claude/pricing on 2026-09-27.
        // 1-hour cache writes are 2× input for every model (derived, see
        // `ModelPrice::cache_write_1h`).
        // Cache-write is Anthropic's 1.25× input premium, cache-read the 0.1×
        // discount (Fable 5.1 reads at $0.25, a deeper discount, so it too
        // gets a longer key). The retired dotted models (Opus 3, Haiku 3.5)
        // keep their historical rates below, reachable via their longer
        // `claude-3-*` prefixes.
        (
            "claude-fable",
            ModelPrice {
                input: 10.0,
                output: 50.0,
                cache_write: 12.5,
                cache_read: 1.0,
                cache_write_1h: None,
            },
        ),
        (
            "claude-fable-5-1",
            ModelPrice {
                input: 10.0,
                output: 50.0,
                cache_write: 12.5,
                cache_read: 0.25,
                cache_write_1h: None,
            },
        ),
        (
            "claude-mythos",
            ModelPrice {
                input: 10.0,
                output: 50.0,
                cache_write: 12.5,
                cache_read: 1.0,
                cache_write_1h: None,
            },
        ),
        // Mythos 5.1 reads at 0.025× ($0.25), like Fable 5.1.
        (
            "claude-mythos-5-1",
            ModelPrice {
                input: 10.0,
                output: 50.0,
                cache_write: 12.5,
                cache_read: 0.25,
                cache_write_1h: None,
            },
        ),
        (
            "claude-opus",
            ModelPrice {
                input: 5.0,
                output: 25.0,
                cache_write: 6.25,
                cache_read: 0.50,
                cache_write_1h: None,
            },
        ),
        // Opus 5.5 is 20% under Opus 5, and its cache reads are 0.05× input
        // ($0.20), not the family's 0.1×. Without this key it priced at the
        // family's $5/$25/$0.50: 25% high on input and output and 2.5× on
        // the cache reads that dominate an agent session.
        (
            "claude-opus-5-5",
            ModelPrice {
                input: 4.0,
                output: 20.0,
                cache_write: 5.0,
                cache_read: 0.20,
                cache_write_1h: None,
            },
        ),
        // Opus 4 and 4.1 (retired except on Bedrock / Google Cloud) kept the
        // old $15/$75 rate; the family key would price them at a third.
        (
            "claude-opus-4-1",
            ModelPrice {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.50,
                cache_write_1h: None,
            },
        ),
        (
            "claude-opus-4-20",
            ModelPrice {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.50,
                cache_write_1h: None,
            },
        ),
        (
            "claude-sonnet",
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_write: 3.75,
                cache_read: 0.30,
                cache_write_1h: None,
            },
        ),
        // Sonnet 5 dropped to $2/$10; the longer key beats the family rate.
        (
            "claude-sonnet-5",
            ModelPrice {
                input: 2.0,
                output: 10.0,
                cache_write: 2.5,
                cache_read: 0.20,
                cache_write_1h: None,
            },
        ),
        (
            "claude-haiku",
            ModelPrice {
                input: 1.0,
                output: 5.0,
                cache_write: 1.25,
                cache_read: 0.10,
                cache_write_1h: None,
            },
        ),
        // Older dotted ids (`claude-3-opus…`, `claude-3-5-haiku…`) so a pinned
        // legacy model still prices at its own historical list rate — the
        // longer prefix wins over the generic family key above.
        (
            "claude-3-opus",
            ModelPrice {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.50,
                cache_write_1h: None,
            },
        ),
        (
            "claude-3-5-haiku",
            ModelPrice {
                input: 0.80,
                output: 4.0,
                cache_write: 1.0,
                cache_read: 0.08,
                cache_write_1h: None,
            },
        ),
        // ── OpenAI (Codex / GPT) ───────────────────────────────────────
        // OpenAI has no separate cache-write charge; cache_write == input.
        // The Codex CLI runs the GPT-5 family (`gpt-5.5` is its current
        // default; `gpt-5.3-codex` the coding-tuned variant) — the models the
        // proxy actually sees on `response.model`. GPT-5 cached input is a
        // 0.1× discount; the 4.1 / o-series 0.25×; 4o 0.5×.
        (
            "gpt-5",
            ModelPrice {
                input: 1.25,
                output: 10.0,
                cache_write: 1.25,
                cache_read: 0.125,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5-mini",
            ModelPrice {
                input: 0.25,
                output: 2.0,
                cache_write: 0.25,
                cache_read: 0.025,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5-nano",
            ModelPrice {
                input: 0.05,
                output: 0.40,
                cache_write: 0.05,
                cache_read: 0.005,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5.1",
            ModelPrice {
                input: 1.25,
                output: 10.0,
                cache_write: 1.25,
                cache_read: 0.125,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5.2",
            ModelPrice {
                input: 1.75,
                output: 14.0,
                cache_write: 1.75,
                cache_read: 0.175,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5.3-codex",
            ModelPrice {
                input: 1.75,
                output: 14.0,
                cache_write: 1.75,
                cache_read: 0.175,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-5.5",
            ModelPrice {
                input: 5.0,
                output: 30.0,
                cache_write: 5.0,
                cache_read: 0.50,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-4.1",
            ModelPrice {
                input: 2.0,
                output: 8.0,
                cache_write: 2.0,
                cache_read: 0.50,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-4.1-mini",
            ModelPrice {
                input: 0.40,
                output: 1.60,
                cache_write: 0.40,
                cache_read: 0.10,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-4.1-nano",
            ModelPrice {
                input: 0.10,
                output: 0.40,
                cache_write: 0.10,
                cache_read: 0.025,
                cache_write_1h: None,
            },
        ),
        (
            "o3",
            ModelPrice {
                input: 2.0,
                output: 8.0,
                cache_write: 2.0,
                cache_read: 0.50,
                cache_write_1h: None,
            },
        ),
        (
            "o4-mini",
            ModelPrice {
                input: 1.10,
                output: 4.40,
                cache_write: 1.10,
                cache_read: 0.275,
                cache_write_1h: None,
            },
        ),
        // The `-mini` reasoning tiers must out-prefix their bare parents
        // (`o3`, `o1`) or they'd bill at the full-size rate.
        (
            "o3-mini",
            ModelPrice {
                input: 1.10,
                output: 4.40,
                cache_write: 1.10,
                cache_read: 0.55,
                cache_write_1h: None,
            },
        ),
        (
            "o1-mini",
            ModelPrice {
                input: 1.10,
                output: 4.40,
                cache_write: 1.10,
                cache_read: 0.55,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-4o-mini",
            ModelPrice {
                input: 0.15,
                output: 0.60,
                cache_write: 0.15,
                cache_read: 0.075,
                cache_write_1h: None,
            },
        ),
        (
            "gpt-4o",
            ModelPrice {
                input: 2.50,
                output: 10.0,
                cache_write: 2.50,
                cache_read: 1.25,
                cache_write_1h: None,
            },
        ),
        (
            "o1",
            ModelPrice {
                input: 15.0,
                output: 60.0,
                cache_write: 15.0,
                cache_read: 7.50,
                cache_write_1h: None,
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
        // Opus: 5 in / 25 out / 6.25 write / 0.50 read, 1M of each.
        let counts = TokenCounts {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        // (5 + 25 + 6.25 + 0.50) == $36.75 == 36_750_000 micros.
        assert_eq!(
            cost_micros("claude-opus-4-8", &counts, &no_overrides()),
            Some(36_750_000)
        );
    }

    #[test]
    fn current_opus_prices_at_5_and_25_not_15_and_75() {
        // Regression for #1388: the default agent model `claude-opus-4-8` was
        // metered at the retired Opus-3 rate ($15/$75), a 3× overcount. It must
        // price at the current Opus-tier list rate.
        let input = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-opus-4-8", &input, &no_overrides()),
            Some(5_000_000),
            "1M Opus input tokens must cost $5.00, not $15.00"
        );
        let output = TokenCounts {
            output: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-opus-4-8", &output, &no_overrides()),
            Some(25_000_000),
            "1M Opus output tokens must cost $25.00, not $75.00"
        );
        // Opus 5 (undated id) shares the same current rate.
        assert_eq!(
            cost_micros("claude-opus-5", &input, &no_overrides()),
            Some(5_000_000)
        );
    }

    /// Current Anthropic list rates by family: Fable / Mythos ($10/$50) are
    /// priced at all (they were absent → cost silently missing), Sonnet 5
    /// dropped to $2/$10 and must beat the $3/$15 `claude-sonnet` family key,
    /// Fable 5.1's deeper $0.25 cache-read discount beats the Fable family.
    #[test]
    fn current_anthropic_families_price_at_list_rate() {
        let input = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        let output = TokenCounts {
            output: 1_000_000,
            ..Default::default()
        };
        let cache_read = TokenCounts {
            cache_read: 1_000_000,
            ..Default::default()
        };
        let no = no_overrides();

        assert_eq!(cost_micros("claude-fable-5", &input, &no), Some(10_000_000));
        assert_eq!(
            cost_micros("claude-fable-5", &output, &no),
            Some(50_000_000)
        );
        assert_eq!(
            cost_micros("claude-fable-5", &cache_read, &no),
            Some(1_000_000)
        );
        assert_eq!(
            cost_micros("claude-fable-5-1", &cache_read, &no),
            Some(250_000),
            "Fable 5.1 reads cache at $0.25"
        );
        assert_eq!(
            cost_micros("claude-mythos-5-1", &input, &no),
            Some(10_000_000)
        );

        assert_eq!(
            cost_micros("claude-sonnet-5", &input, &no),
            Some(2_000_000),
            "Sonnet 5 is $2 in, not the $3 family rate"
        );
        assert_eq!(
            cost_micros("claude-sonnet-5", &output, &no),
            Some(10_000_000)
        );
        assert_eq!(
            cost_micros("claude-sonnet-4-6", &input, &no),
            Some(3_000_000),
            "Sonnet 4.6 keeps $3"
        );
        assert_eq!(
            cost_micros("claude-haiku-4-5", &input, &no),
            Some(1_000_000)
        );
    }

    /// Opus 5.5 is $4/$20 with 0.05× cache reads ($0.20) — it used to fall
    /// to the Opus family's $5/$25/$0.50.
    #[test]
    fn opus_5_5_prices_at_its_own_rate() {
        let no = no_overrides();
        let one = |counts: TokenCounts| cost_micros("claude-opus-5-5", &counts, &no);
        let m = 1_000_000;
        assert_eq!(
            one(TokenCounts {
                input: m,
                ..Default::default()
            }),
            Some(4_000_000)
        );
        assert_eq!(
            one(TokenCounts {
                output: m,
                ..Default::default()
            }),
            Some(20_000_000)
        );
        assert_eq!(
            one(TokenCounts {
                cache_read: m,
                ..Default::default()
            }),
            Some(200_000)
        );
        assert_eq!(
            one(TokenCounts {
                cache_creation: m,
                ..Default::default()
            }),
            Some(5_000_000),
            "5-minute writes are 1.25× input"
        );
        assert_eq!(
            cost_micros(
                "claude-opus-5",
                &TokenCounts {
                    input: m,
                    ..Default::default()
                },
                &no
            ),
            Some(5_000_000),
            "Opus 5 keeps the family rate"
        );
    }

    /// A 1-hour cache write is 2× input on every model, not the 5-minute
    /// 1.25×; only the 1-hour part of the writes takes the higher rate.
    #[test]
    fn one_hour_cache_writes_price_at_twice_input() {
        let no = no_overrides();
        let counts = TokenCounts {
            cache_creation: 3_000_000,
            cache_creation_1h: 1_000_000,
            ..Default::default()
        };
        // Opus 5: 2M × $6.25 + 1M × $10 = $22.50.
        assert_eq!(cost_micros("claude-opus-5", &counts, &no), Some(22_500_000));
        // Opus 5.5: 2M × $5 + 1M × $8 = $18.
        assert_eq!(
            cost_micros("claude-opus-5-5", &counts, &no),
            Some(18_000_000)
        );
    }

    /// Fast mode doubles every bucket, cache included.
    #[test]
    fn fast_mode_doubles_the_rate() {
        let counts = TokenCounts {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            fast: true,
            ..Default::default()
        };
        // Opus 5.5 fast: $8 + $40 + $0.40.
        assert_eq!(
            cost_micros("claude-opus-5-5", &counts, &no_overrides()),
            Some(48_400_000)
        );
    }

    /// Mythos 5.1 reads cache at $0.25 like Fable 5.1, and the retired
    /// Opus 4 / 4.1 keep $15/$75 rather than the family's $5/$25.
    #[test]
    fn mythos_5_1_and_retired_opus_4_price_at_their_own_rates() {
        let no = no_overrides();
        let m = 1_000_000;
        assert_eq!(
            cost_micros(
                "claude-mythos-5-1",
                &TokenCounts {
                    cache_read: m,
                    ..Default::default()
                },
                &no
            ),
            Some(250_000)
        );
        let input = TokenCounts {
            input: m,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-opus-4-1-20250805", &input, &no),
            Some(15_000_000)
        );
        assert_eq!(
            cost_micros("claude-opus-4-20250514", &input, &no),
            Some(15_000_000)
        );
        assert_eq!(
            cost_micros("claude-opus-4-8", &input, &no),
            Some(5_000_000),
            "Opus 4.8 is not caught by the Opus 4 key"
        );
    }

    /// The models the Codex CLI actually runs (GPT-5 family) are priced; the
    /// dotted point-releases and the coding variant each beat the bare
    /// `gpt-5` key, and the `-mini` / `-nano` tiers beat their parent.
    #[test]
    fn codex_gpt5_family_prices_at_list_rate() {
        let input = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        let output = TokenCounts {
            output: 1_000_000,
            ..Default::default()
        };
        let cache_read = TokenCounts {
            cache_read: 1_000_000,
            ..Default::default()
        };
        let no = no_overrides();

        assert_eq!(cost_micros("gpt-5", &input, &no), Some(1_250_000));
        assert_eq!(cost_micros("gpt-5", &output, &no), Some(10_000_000));
        assert_eq!(cost_micros("gpt-5", &cache_read, &no), Some(125_000));
        assert_eq!(cost_micros("gpt-5-mini", &input, &no), Some(250_000));
        assert_eq!(cost_micros("gpt-5-nano", &input, &no), Some(50_000));
        assert_eq!(cost_micros("gpt-5.2", &input, &no), Some(1_750_000));
        assert_eq!(cost_micros("gpt-5.3-codex", &input, &no), Some(1_750_000));
        assert_eq!(cost_micros("gpt-5.3-codex", &output, &no), Some(14_000_000));
        assert_eq!(
            cost_micros("gpt-5.5", &input, &no),
            Some(5_000_000),
            "Codex's current default model"
        );
        assert_eq!(cost_micros("gpt-5.5", &output, &no), Some(30_000_000));
        assert_eq!(cost_micros("o3", &input, &no), Some(2_000_000));
        assert_eq!(cost_micros("o4-mini", &input, &no), Some(1_100_000));
        assert_eq!(
            cost_micros("o3-mini", &input, &no),
            Some(1_100_000),
            "o3-mini must not be shadowed by the bare `o3` key"
        );
        assert_eq!(cost_micros("o1-mini", &input, &no), Some(1_100_000));
        assert_eq!(cost_micros("gpt-4.1-mini", &input, &no), Some(400_000));
    }

    #[test]
    fn retired_dotted_opus_keeps_its_historical_rate() {
        // `claude-3-opus-*` was genuinely $15/$75; its longer prefix must still
        // win over the corrected generic `claude-opus` family key.
        let input = TokenCounts {
            input: 1_000_000,
            ..Default::default()
        };
        assert_eq!(
            cost_micros("claude-3-opus-20240229", &input, &no_overrides()),
            Some(15_000_000)
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
                cache_write_1h: None,
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
                cache_write_1h: None,
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
                cache_write_1h: None,
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
