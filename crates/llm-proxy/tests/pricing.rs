//! Pricing table tests. The guard we care about: unknown models return
//! None (never lie about cost), known model families resolve to
//! reasonable rates, and changes to the rate table get reviewed.

use lazybox_llm_proxy::{ApiProvider, pricing};

#[test]
fn anthropic_known_families_have_rates() {
    for model in [
        "claude-opus-4-7",
        "claude-opus-4-7-20260101",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-20251001",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
    ] {
        let rate = pricing::rate_per_mtok(ApiProvider::Anthropic, model);
        assert!(rate.is_some(), "{model} should have a known rate");
    }
}

#[test]
fn anthropic_rates_are_ordered_by_tier() {
    let opus = pricing::rate_per_mtok(ApiProvider::Anthropic, "claude-opus-4-7").unwrap();
    let sonnet = pricing::rate_per_mtok(ApiProvider::Anthropic, "claude-sonnet-4-6").unwrap();
    let haiku = pricing::rate_per_mtok(ApiProvider::Anthropic, "claude-haiku-4-5").unwrap();
    // Sanity: Opus > Sonnet > Haiku on both input and output rates.
    assert!(opus.0 > sonnet.0 && sonnet.0 > haiku.0);
    assert!(opus.1 > sonnet.1 && sonnet.1 > haiku.1);
}

#[test]
fn openai_known_models_have_rates() {
    for model in ["gpt-4o", "gpt-4o-mini", "o1", "o1-mini"] {
        assert!(
            pricing::rate_per_mtok(ApiProvider::OpenAI, model).is_some(),
            "{model} should have a known rate"
        );
    }
}

#[test]
fn unknown_models_return_none() {
    // Critical: we must NOT fabricate a number for a model we don't know.
    // Callers use `None` as "don't display cost"; a bogus fallback
    // would lie to users.
    assert!(pricing::rate_per_mtok(ApiProvider::Anthropic, "claude-3-sonnet").is_none());
    assert!(pricing::rate_per_mtok(ApiProvider::OpenAI, "gpt-3.5-turbo").is_none());
    assert!(pricing::rate_per_mtok(ApiProvider::Unknown, "anything").is_none());
}

#[test]
fn estimate_cost_zero_tokens_is_zero() {
    let cost = pricing::estimate_cost(ApiProvider::Anthropic, "claude-sonnet-4-6", 0, 0);
    assert_eq!(cost, Some(0.0));
}

#[test]
fn estimate_cost_unknown_model_is_none() {
    let cost = pricing::estimate_cost(ApiProvider::Anthropic, "unknown-model", 1000, 1000);
    assert!(cost.is_none());
}

#[test]
fn estimate_cost_sonnet_million_tokens() {
    // 1M input + 1M output on Sonnet = input_rate + output_rate USD.
    let (input_rate, output_rate) =
        pricing::rate_per_mtok(ApiProvider::Anthropic, "claude-sonnet-4-6").unwrap();
    let cost = pricing::estimate_cost(
        ApiProvider::Anthropic,
        "claude-sonnet-4-6",
        1_000_000,
        1_000_000,
    )
    .unwrap();
    assert!(
        (cost - (input_rate + output_rate)).abs() < 1e-6,
        "1M in + 1M out should equal (in_rate + out_rate); got {cost}"
    );
}
