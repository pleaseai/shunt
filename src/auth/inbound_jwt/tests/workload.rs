use serde_json::{json, Value};

use super::*;

fn github_claims(issuer: &str) -> Value {
    json!({
        "iss": issuer,
        "aud": "shunt-clients",
        "exp": FUTURE_EXP,
        "iat": FUTURE_EXP - 300,
        "sub": "repo:pleaseai/shunt:ref:refs/heads/main",
        "repository": "pleaseai/shunt",
        "repository_owner": "pleaseai",
    })
}

fn workload_entry(issuer: &str, jwks_url: &str) -> InboundJwtConfig {
    let mut entry = entry(issuer, jwks_url);
    entry.email_domains = Vec::new();
    entry.require = [(
        "repository_owner".to_string(),
        StringList::Many(vec!["pleaseai".to_string()]),
    )]
    .into();
    entry.subject_prefix = StringList::Many(vec!["repo:pleaseai/shunt:".to_string()]);
    entry.identity_claim = Some("repository".to_string());
    entry
}

async fn outcome(
    mutate_entry: impl FnOnce(&mut InboundJwtConfig),
    mutate_claims: impl FnOnce(&mut Value),
) -> JwtOutcome {
    let server = issuer_serving(jwks()).await;
    let mut entry = workload_entry(&server.uri(), &format!("{}/jwks", server.uri()));
    mutate_entry(&mut entry);
    let auth = auth_with(vec![entry], None);
    let mut claims = github_claims(&server.uri());
    mutate_claims(&mut claims);
    verify_with(&auth, &token(&claims)).await
}

#[tokio::test]
async fn a_github_actions_token_authenticates_with_the_repository_as_identity() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![workload_entry(
            &server.uri(),
            &format!("{}/jwks", server.uri()),
        )],
        None,
    );

    let outcome = verify_with(&auth, &token(&github_claims(&server.uri()))).await;

    assert_eq!(
        outcome,
        JwtOutcome::Verified {
            identity: "pleaseai/shunt".to_string(),
            issuer: server.uri(),
        }
    );
}

#[tokio::test]
async fn another_repository_owner_rejects() {
    let result = outcome(|_| {}, |c| c["repository_owner"] = json!("evil")).await;
    assert_eq!(result, JwtOutcome::Rejected);
}

#[tokio::test]
async fn a_subject_outside_the_prefix_or_missing_rejects() {
    let other = outcome(|_| {}, |c| c["sub"] = json!("repo:pleaseai/other:ref")).await;
    assert_eq!(other, JwtOutcome::Rejected);
    let missing = outcome(
        |_| {},
        |c| {
            c.as_object_mut().unwrap().remove("sub");
        },
    )
    .await;
    assert_eq!(missing, JwtOutcome::Rejected);
}

#[tokio::test]
async fn an_identity_claim_that_is_missing_non_string_or_oversized_rejects() {
    let missing = outcome(
        |_| {},
        |c| {
            c.as_object_mut().unwrap().remove("repository");
        },
    )
    .await;
    let non_string = outcome(|_| {}, |c| c["repository"] = json!(42)).await;
    let oversized = outcome(|_| {}, |c| c["repository"] = json!("r".repeat(257))).await;

    for result in [missing, non_string, oversized] {
        assert_eq!(result, JwtOutcome::Rejected);
    }
}

#[tokio::test]
async fn a_required_claim_that_is_not_a_string_rejects() {
    let result = outcome(
        |e| {
            e.require = [(
                "repository_id".to_string(),
                StringList::One("1".to_string()),
            )]
            .into();
        },
        |c| c["repository_id"] = json!(1),
    )
    .await;
    assert_eq!(result, JwtOutcome::Rejected);
}

#[tokio::test]
async fn an_email_rule_and_require_must_both_pass() {
    let configure = |e: &mut InboundJwtConfig| {
        e.email_domains = vec!["example.com".to_string()];
        e.identity_claim = None;
    };
    let email = |c: &mut Value| {
        c["email"] = json!("dev@example.com");
        c["email_verified"] = json!(true);
    };
    let both = outcome(configure, email).await;
    let require_fails = outcome(configure, |c| {
        email(c);
        c["repository_owner"] = json!("evil");
    })
    .await;

    assert!(matches!(both, JwtOutcome::Verified { identity, .. } if identity == "dev@example.com"));
    assert_eq!(require_fails, JwtOutcome::Rejected);
}

#[tokio::test]
async fn an_unverified_email_never_becomes_an_identity() {
    let configure = |e: &mut InboundJwtConfig| e.identity_claim = Some("email".to_string());
    let unverified = outcome(configure, |c| {
        c["email"] = json!("dev@example.com");
        c["email_verified"] = json!(false);
    })
    .await;
    let verified = outcome(configure, |c| {
        c["email"] = json!("dev@example.com");
        c["email_verified"] = json!(true);
    })
    .await;

    assert_eq!(unverified, JwtOutcome::Rejected);
    assert!(
        matches!(verified, JwtOutcome::Verified { identity, .. } if identity == "dev@example.com")
    );
}

#[tokio::test]
async fn a_required_email_must_be_verified() {
    let configure = |e: &mut InboundJwtConfig| {
        e.require = [(
            "email".to_string(),
            StringList::One("ci@example.com".to_string()),
        )]
        .into();
    };
    let unverified = outcome(configure, |c| {
        c["email"] = json!("ci@example.com");
        c["email_verified"] = json!(false);
    })
    .await;
    let verified = outcome(configure, |c| {
        c["email"] = json!("ci@example.com");
        c["email_verified"] = json!(true);
    })
    .await;

    assert_eq!(unverified, JwtOutcome::Rejected);
    assert!(matches!(verified, JwtOutcome::Verified { .. }));
}

#[tokio::test]
async fn azp_is_readable_as_the_identity_claim() {
    let result = outcome(
        |e| {
            e.identity_claim = Some("azp".to_string());
            e.authorized_parties = vec!["ci-client".to_string()];
        },
        |c| c["azp"] = json!("ci-client"),
    )
    .await;
    assert!(matches!(result, JwtOutcome::Verified { identity, .. } if identity == "ci-client"));
}

#[tokio::test]
async fn every_required_claim_must_match_and_any_listed_value_satisfies_one() {
    let configure = |e: &mut InboundJwtConfig| {
        e.require = [
            (
                "repository_owner".to_string(),
                StringList::Many(vec!["other-org".to_string(), "pleaseai".to_string()]),
            ),
            (
                "repository".to_string(),
                StringList::One("pleaseai/shunt".to_string()),
            ),
        ]
        .into();
    };
    let both_match = outcome(configure, |_| {}).await;
    let one_mismatched = outcome(configure, |c| c["repository"] = json!("pleaseai/other")).await;

    assert!(matches!(both_match, JwtOutcome::Verified { .. }));
    assert_eq!(one_mismatched, JwtOutcome::Rejected);
}

#[tokio::test]
async fn a_null_azp_is_absent_but_a_non_string_azp_rejects() {
    let null = outcome(|_| {}, |c| c["azp"] = Value::Null).await;
    let numeric = outcome(|_| {}, |c| c["azp"] = json!(42)).await;

    assert!(matches!(null, JwtOutcome::Verified { .. }));
    assert_eq!(numeric, JwtOutcome::Rejected);
}
