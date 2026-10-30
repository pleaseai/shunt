//! The sign-in against a loopback token endpoint + issuer. The callback is
//! driven by an HTTP GET to the bound listener or by an injected paste.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{run_with, Behavior, Endpoints, Outcome};
use crate::auth::chatgpt_plan::id_token::tests::{sign, KID_ONE, MODULUS_ONE};
use crate::auth::chatgpt_plan::{record::AccountRecord, store};
use crate::auth::shared::EnvVarGuard;

const NAME: &str = "primary";
const GRANTED: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

struct Env {
    // Drop order: the env var is restored before the lock is released.
    _env: EnvVarGuard,
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
    server: MockServer,
}

impl Env {
    async fn new(tag: &str) -> Self {
        let lock = crate::config::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "shunt-siwc-login-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let env = EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"issuer": uri, "jwks_uri": format!("{uri}/keys")})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": KID_ONE, "n": MODULUS_ONE, "e": "AQAB",
            }]})))
            .mount(&server)
            .await;
        Self {
            _env: env,
            _lock: lock,
            dir,
            server,
        }
    }

    fn endpoints(&self, callback_port: u16) -> Endpoints {
        Endpoints {
            authorize_url: format!("{}/authorize", self.server.uri()),
            token_url: format!("{}/token", self.server.uri()),
            issuer: self.server.uri(),
            callback_port,
        }
    }

    /// Answer the token endpoint with an ID token for `aud`/`nonce`.
    async fn mount_token(&self, aud: &str, nonce: &str, scope: &str) {
        let iat = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let id_token = sign(
            &json!({
                "iss": self.server.uri(), "aud": [aud], "nonce": nonce,
                "sub": "auth0|user-1", "email": "dev@example.com",
                "iat": iat, "exp": iat + 3600,
            }),
            KID_ONE,
        );
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ACCESS-SECRET", "refresh_token": "REFRESH-SECRET",
                "id_token": id_token, "expires_in": 3600, "scope": scope,
                "earliest_refresh_at": iat + 3000, "token_type": "Bearer",
            })))
            .mount(&self.server)
            .await;
    }

    async fn token_posts(&self) -> Vec<wiremock::Request> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/token")
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Attempt {
    handle: JoinHandle<anyhow::Result<Outcome>>,
    url: reqwest::Url,
    paste: oneshot::Sender<anyhow::Result<String>>,
}

impl Attempt {
    async fn start(endpoints: Endpoints) -> Self {
        let (sink, mut urls) = mpsc::unbounded_channel();
        let (paste, receiver) = oneshot::channel();
        let behavior = Behavior {
            timeout: Duration::from_secs(20),
            open_browser: false,
            url_sink: Some(sink),
        };
        let handle =
            tokio::spawn(
                async move { run_with(NAME, &endpoints, behavior, move || receiver).await },
            );
        let url = urls.recv().await.expect("authorize URL announced");
        Self {
            handle,
            url: url.parse().unwrap(),
            paste,
        }
    }

    fn param(&self, key: &str) -> Option<String> {
        self.url
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    fn callback(&self, extra: &str) -> String {
        format!(
            "{}?code=auth-code&state={}{extra}",
            self.param("redirect_uri").unwrap(),
            self.param("state").unwrap()
        )
    }

    async fn deliver(&self, extra: &str) {
        let response = reqwest::get(self.callback(extra)).await.unwrap();
        assert!(response.status().is_success());
    }

    async fn finish(self) -> anyhow::Result<Outcome> {
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("sign-in finished promptly")
            .unwrap()
    }
}

fn saved(client_id: &str) -> AccountRecord {
    AccountRecord::signed_out(client_id)
}

#[tokio::test]
async fn first_sign_in_registers_and_stores_the_account() {
    let env = Env::new("first").await;
    let attempt = Attempt::start(env.endpoints(0)).await;

    assert_eq!(
        attempt.param("client_id").as_deref(),
        Some("dynamic_agent_client")
    );
    assert_eq!(attempt.param("agent_name_hint").as_deref(), Some("shunt"));
    assert_eq!(
        attempt.param("ext_agent_host_id"),
        Some(store::host_id().unwrap())
    );
    assert_eq!(attempt.param("response_type").as_deref(), Some("code"));
    assert_eq!(attempt.param("scope").as_deref(), Some(GRANTED));
    assert_eq!(
        attempt.param("resource").as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(
        attempt.param("code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(attempt
        .param("redirect_uri")
        .unwrap()
        .ends_with("/auth/callback"));

    let nonce = attempt.param("nonce").unwrap();
    env.mount_token("oaiapp_new", &nonce, GRANTED).await;
    attempt
        .deliver(&format!(
            "&client_id=oaiapp_new&scope={}",
            GRANTED.replace(' ', "%20")
        ))
        .await;
    let challenge = attempt.param("code_challenge").unwrap();
    let redirect = attempt.param("redirect_uri").unwrap();
    let outcome = attempt.finish().await.unwrap();

    assert_eq!(outcome.email.as_deref(), Some("dev@example.com"));
    let rendered = outcome.render();
    assert!(rendered.contains("dev@example.com") && rendered.contains("chatgpt.tokens.use.direct"));
    assert!(!rendered.contains("SECRET") && !rendered.contains("auth-code"));

    let record = store::read_account(NAME).unwrap().expect("stored");
    assert_eq!(record.client_id, "oaiapp_new");
    assert_eq!(record.sub.as_deref(), Some("auth0|user-1"));
    assert_eq!(record.scopes.join(" "), GRANTED);
    let tokens = record.tokens.expect("signed in");
    assert_eq!(tokens.access_token, "ACCESS-SECRET");
    assert_eq!(tokens.refresh_token, "REFRESH-SECRET");
    assert!(tokens.earliest_refresh_at.is_some() && tokens.expires_at > 0);

    // The exchange carried the form, and the verifier matches the challenge.
    let posts = env.token_posts().await;
    assert_eq!(posts.len(), 1);
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(&posts[0].body)
            .into_owned()
            .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "oaiapp_new");
    assert_eq!(form["code"], "auth-code");
    assert_eq!(form["redirect_uri"], redirect);
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes())),
        challenge
    );
}

#[tokio::test]
async fn re_sign_in_reuses_the_saved_client_id_without_a_name_hint() {
    let env = Env::new("resign").await;
    store::write_account(NAME, &saved("oaiapp_saved")).unwrap();
    let attempt = Attempt::start(env.endpoints(0)).await;
    assert_eq!(attempt.param("client_id").as_deref(), Some("oaiapp_saved"));
    assert_eq!(attempt.param("agent_name_hint"), None);
    assert_eq!(
        attempt.param("ext_agent_host_id"),
        Some(store::host_id().unwrap())
    );

    env.mount_token("oaiapp_saved", &attempt.param("nonce").unwrap(), GRANTED)
        .await;
    attempt.deliver("").await; // callback omits client_id
    attempt.finish().await.unwrap();
    assert_eq!(
        store::read_account(NAME).unwrap().unwrap().client_id,
        "oaiapp_saved"
    );
}

#[tokio::test]
async fn a_different_client_id_aborts_and_leaves_the_file_untouched() {
    let env = Env::new("mismatch").await;
    let mut before = saved("oaiapp_saved");
    before.email = Some("old@example.com".into());
    store::write_account(NAME, &before).unwrap();
    let bytes = std::fs::read(store::account_path(NAME)).unwrap();

    let attempt = Attempt::start(env.endpoints(0)).await;
    attempt.deliver("&client_id=oaiapp_other").await;
    let error = attempt.finish().await.unwrap_err();
    assert!(
        error.to_string().contains("different client id"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(store::account_path(NAME)).unwrap(), bytes);
    assert!(env.token_posts().await.is_empty());
}

#[tokio::test]
async fn first_sign_in_without_an_issued_client_id_stores_nothing() {
    for (tag, extra) in [("none", ""), ("dynamic", "&client_id=dynamic_agent_client")] {
        let env = Env::new(tag).await;
        let attempt = Attempt::start(env.endpoints(0)).await;
        attempt.deliver(extra).await;
        let error = attempt.finish().await.unwrap_err();
        assert!(
            error.to_string().contains("registration incomplete"),
            "{error:#}"
        );
        assert!(store::read_account(NAME).unwrap().is_none());
        assert!(env.token_posts().await.is_empty());
    }
}

#[tokio::test]
async fn a_pasted_url_with_whitespace_and_quotes_completes_the_sign_in() {
    let env = Env::new("paste").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    env.mount_token("oaiapp_new", &attempt.param("nonce").unwrap(), GRANTED)
        .await;
    let pasted = format!("  \"{}\"\n", attempt.callback("&client_id=oaiapp_new"));
    attempt.paste.send(Ok(pasted)).unwrap();
    attempt.handle.await.unwrap().unwrap();
    assert_eq!(
        store::read_account(NAME).unwrap().unwrap().client_id,
        "oaiapp_new"
    );
}

#[tokio::test]
async fn a_pasted_url_with_another_state_never_reaches_the_token_endpoint() {
    let env = Env::new("paste-state").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    let wrong = attempt
        .callback("&client_id=oaiapp_new")
        .replace(&attempt.param("state").unwrap(), "someone-elses-state");
    attempt.paste.send(Ok(wrong)).unwrap();
    let error = attempt.handle.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("state mismatch"), "{error:#}");
    assert!(env.token_posts().await.is_empty());
    assert!(store::read_account(NAME).unwrap().is_none());
}

#[tokio::test]
async fn a_pasted_error_callback_is_reported() {
    let env = Env::new("paste-denied").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    let url = format!(
        "{}?error=access_denied&state={}",
        attempt.param("redirect_uri").unwrap(),
        attempt.param("state").unwrap()
    );
    attempt.paste.send(Ok(url)).unwrap();
    let error = attempt.handle.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("access_denied"), "{error:#}");
    assert!(env.token_posts().await.is_empty());
}

#[tokio::test]
async fn an_occupied_callback_port_falls_back_to_paste_only() {
    let env = Env::new("port").await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let attempt = Attempt::start(env.endpoints(port)).await;
    assert!(attempt
        .param("redirect_uri")
        .unwrap()
        .starts_with(&format!("http://127.0.0.1:{port}/auth/callback")));
    env.mount_token("oaiapp_new", &attempt.param("nonce").unwrap(), GRANTED)
        .await;
    let pasted = attempt.callback("&client_id=oaiapp_new");
    attempt.paste.send(Ok(pasted)).unwrap();
    attempt.handle.await.unwrap().unwrap();
    assert!(store::read_account(NAME).unwrap().unwrap().is_signed_in());
}

#[tokio::test]
async fn an_id_token_failing_validation_stores_nothing() {
    let env = Env::new("badtoken").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    env.mount_token("oaiapp_new", "another-attempts-nonce", GRANTED)
        .await;
    attempt.deliver("&client_id=oaiapp_new").await;
    let error = attempt.finish().await.unwrap_err();
    assert!(error.to_string().contains("failed validation"), "{error:#}");
    assert!(!format!("{error:#}").contains("SECRET"));
    assert!(store::read_account(NAME).unwrap().is_none());
}

#[tokio::test]
async fn missing_plan_usage_scope_keeps_only_the_registration() {
    let env = Env::new("noscope").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    env.mount_token(
        "oaiapp_new",
        &attempt.param("nonce").unwrap(),
        "openid profile email",
    )
    .await;
    attempt.deliver("&client_id=oaiapp_new").await;
    let error = attempt.finish().await.unwrap_err();
    assert!(
        error.to_string().contains("plan usage was not granted"),
        "{error:#}"
    );
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store::account_path(NAME)).unwrap()).unwrap();
    assert_eq!(on_disk, json!({"version": 1, "client_id": "oaiapp_new"}));
}

#[tokio::test]
async fn an_account_changed_mid_sign_in_is_not_overwritten() {
    let env = Env::new("race").await;
    let attempt = Attempt::start(env.endpoints(0)).await;
    env.mount_token("oaiapp_new", &attempt.param("nonce").unwrap(), GRANTED)
        .await;
    store::write_account(NAME, &saved("oaiapp_concurrent")).unwrap();
    attempt.deliver("&client_id=oaiapp_new").await;
    let error = attempt.finish().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed while you were signing in"),
        "{error:#}"
    );
    assert_eq!(
        store::read_account(NAME).unwrap().unwrap().client_id,
        "oaiapp_concurrent"
    );
}

#[tokio::test]
async fn a_logout_mid_sign_in_is_not_undone() {
    use crate::auth::chatgpt_plan::record::TokenBlock;

    let env = Env::new("logout-race").await;
    let mut signed_in = saved("oaiapp_saved");
    signed_in.tokens = Some(TokenBlock {
        access_token: "OLD-ACCESS".into(),
        refresh_token: "OLD-REFRESH".into(),
        id_token: None,
        expires_at: 4_000_000_000,
        earliest_refresh_at: None,
        last_refresh: Some(1),
    });
    store::write_account(NAME, &signed_in).unwrap();

    let attempt = Attempt::start(env.endpoints(0)).await;
    env.mount_token("oaiapp_saved", &attempt.param("nonce").unwrap(), GRANTED)
        .await;
    store::write_account(NAME, &saved("oaiapp_saved")).unwrap();
    attempt.deliver("&client_id=oaiapp_saved").await;
    let error = attempt.finish().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed while you were signing in"),
        "{error:#}"
    );
    assert!(!store::read_account(NAME).unwrap().unwrap().is_signed_in());
}

mod pure {
    use crate::auth::chatgpt_plan::authorize::{bind_client_id, parse_pasted};

    #[test]
    fn bind_client_id_rules() {
        assert_eq!(bind_client_id(Some("a"), None).unwrap(), "a");
        assert_eq!(bind_client_id(Some("a"), Some("a")).unwrap(), "a");
        assert!(bind_client_id(Some("a"), Some("b")).is_err());
        assert_eq!(bind_client_id(None, Some("oaiapp_x")).unwrap(), "oaiapp_x");
        assert!(bind_client_id(None, None).is_err());
        assert!(bind_client_id(None, Some("")).is_err());
        assert!(bind_client_id(None, Some("dynamic_agent_client")).is_err());
    }

    #[test]
    fn parse_pasted_trims_quotes_and_checks_state() {
        let ok = parse_pasted(
            " '\"http://127.0.0.1:1455/auth/callback?code=c&state=s&scope=a%20b'\" \n",
            "s",
        );
        let params = ok.unwrap();
        assert_eq!(
            (params.code.as_str(), params.scope.as_deref()),
            ("c", Some("a b"))
        );
        assert!(parse_pasted("http://x/cb?code=c&state=other", "s").is_err());
        assert!(parse_pasted("http://x/cb?code=c", "s").is_err());
        assert!(parse_pasted("http://x/cb?state=s", "s").is_err());
        assert!(parse_pasted("not a url", "s").is_err());
    }
}
