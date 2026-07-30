use super::*;

#[test]
fn pro_defaults_to_high_because_it_has_no_medium_effort() {
    // The regression this table exists for: `gemini-3.1-pro` offers only
    // `low`/`high`, so the previous unconditional `medium` default made
    // every effort-less request to that model fail with an `agy` error.
    assert_eq!(
        resolve_effort("gemini-3.1-pro", None),
        Ok(Some("high".to_string()))
    );
}

#[test]
fn flash_models_keep_a_medium_default() {
    for model in ["gemini-3.6-flash", "gemini-3.5-flash"] {
        assert_eq!(
            resolve_effort(model, None),
            Ok(Some("medium".to_string())),
            "{model} should default to medium"
        );
    }
}

#[test]
fn dotless_flash_model_still_receives_an_effort() {
    // The previous gate was `upstream_model.contains("3.")`, which matches
    // `gemini-3.1-pro` and `gemini-3.6-flash` but NOT `gemini-3-flash` —
    // that model silently never received an `--effort` flag at all.
    assert_eq!(
        resolve_effort("gemini-3-flash", None),
        Ok(Some("medium".to_string()))
    );
}

#[test]
fn a_configured_effort_the_model_offers_is_passed_through() {
    assert_eq!(
        resolve_effort("gemini-3.1-pro", Some("low")),
        Ok(Some("low".to_string()))
    );
}

#[test]
fn a_configured_effort_the_model_rejects_is_refused_locally() {
    // Caught here rather than at `agy`, so the operator gets a 400 naming
    // the valid values instead of an opaque upstream failure.
    let error = resolve_effort("gemini-3.1-pro", Some("medium"))
        .expect_err("medium is not offered by gemini-3.1-pro");
    assert!(
        error.contains("does not support effort \"medium\""),
        "{error}"
    );
    assert!(error.contains("low, high"), "{error}");
}

#[test]
fn an_unknown_model_defers_to_the_cli_instead_of_guessing() {
    // Guessing a default for a model we have not verified is exactly what
    // broke `gemini-3.1-pro`; an unrecognised model sends no flag, and a
    // configured value passes through for `agy` itself to validate.
    assert_eq!(resolve_effort("gemini-9-experimental", None), Ok(None));
    assert_eq!(
        resolve_effort("gemini-9-experimental", Some("medium")),
        Ok(Some("medium".to_string()))
    );
}

#[test]
fn every_table_default_is_one_of_that_models_supported_values() {
    for (model, supported, default) in ANTIGRAVITY_EFFORTS {
        assert!(
            supported.contains(default),
            "{model} defaults to {default}, which is not in {supported:?}"
        );
    }
}

#[test]
fn a_failed_invocation_carries_the_cli_diagnosis_in_anthropic_shape() {
    let error = agy_failure("  gemini-3.1-pro has no \"medium\" effort  ");
    assert!(
        error.message.contains("has no \"medium\" effort"),
        "the CLI's own diagnosis must survive to the client: {}",
        error.message
    );
}

#[test]
fn an_empty_stderr_still_produces_a_usable_message() {
    assert!(agy_failure("   ").message.contains("no output"));
}

#[test]
fn an_oversized_stderr_is_truncated() {
    let message = agy_failure(&"x".repeat(AGY_STDERR_LIMIT * 2)).message;
    assert!(
        message.len() < AGY_STDERR_LIMIT * 2,
        "len {}",
        message.len()
    );
}

#[test]
fn a_missing_binary_names_agy_bin_and_the_restricted_path_trap() {
    // Encountered live: under `brew services` the unit's PATH excludes
    // ~/.local/bin, so a provider that works in a shell returns 503 with
    // no indication why. The message must point at the actual remedy.
    let message = agy_not_found().message;
    assert!(message.contains("AGY_BIN"), "{message}");
    assert!(message.contains("PATH"), "{message}");
}
