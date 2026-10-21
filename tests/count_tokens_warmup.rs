//! The tiktoken vocab loads at gateway startup, not on the first turn (#642).
//!
//! The encoder is a process-wide `OnceLock`, so this file holds exactly one
//! test: any other test in the same binary could load it first and make the
//! "still cold" assertions meaningless.

use shunt::{
    config::{Config, CountTokens, ProviderKind},
    count_tokens::encoder_is_loaded,
    server,
};

#[tokio::test]
async fn build_router_loads_the_encoder_only_when_a_provider_counts_locally() {
    assert!(
        !encoder_is_loaded(),
        "the encoder must start cold, or the assertions below prove nothing"
    );

    // Every non-Anthropic provider opted out: nothing counts locally, so
    // startup must not pay for the vocab. The Anthropic provider stays at the
    // `tiktoken` default, which it ignores.
    let mut opted_out = Config::default();
    for provider in opted_out.providers.values_mut() {
        if provider.kind != ProviderKind::Anthropic {
            provider.count_tokens = CountTokens::Estimate;
        }
    }
    let _ = server::build_router(opted_out).unwrap();
    assert!(
        !encoder_is_loaded(),
        "no provider counts locally, so startup must not load the vocab"
    );

    // The default config counts codex/openai turns locally, so the first
    // turn's one-second estimate must not include the cold load.
    let _ = server::build_router(Config::default()).unwrap();
    assert!(
        encoder_is_loaded(),
        "build_router must load the vocab when a provider counts tokens locally"
    );
}
