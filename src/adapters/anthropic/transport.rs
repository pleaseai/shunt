//! Allowlisted transport diagnostics: never format an error's source chain.

use std::{error::Error, io};

// Temporary Claude OAuth HTTP/1.1 trial. RequestBuilder::version(HTTP_11)
// cannot exclude a negotiated or pooled H2 connection in hyper-util 0.1.20.
// Keep an independent pool with H1-only ALPN and normal TLS verification.
pub(super) fn request_client(
    shared: &reqwest::Client,
    auth: crate::config::AuthMode,
    headers: &axum::http::HeaderMap,
) -> reqwest::Client {
    use crate::config::AuthMode;
    static CLAUDE_HTTP1: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if matches!(auth, AuthMode::ClaudeOauth | AuthMode::Passthrough)
        && super::bearer_is_subscription_oauth(headers)
    {
        CLAUDE_HTTP1
            .get_or_init(|| {
                reqwest::Client::builder()
                    .http1_only()
                    .redirect(crate::adapters::responses::request::codex_identity_redirect_policy())
                    .build()
                    .expect("build Claude OAuth HTTP/1.1 trial client")
            })
            .clone()
    } else {
        shared.clone()
    }
}

/// Only fixed classifications may be added here, not error text or request data.
/// Unexposed TLS/protocol causes remain unclassified; `None` is not evidence
/// that the failure was not TLS or HTTP/2 related.
#[derive(Debug)]
pub(super) struct Diagnostics {
    pub connect: bool,
    pub timeout: bool,
    pub request: bool,
    pub body: bool,
    pub io_kind: Option<io::ErrorKind>,
    pub tls_kind: Option<&'static str>,
}

impl Diagnostics {
    pub fn new(error: &reqwest::Error) -> Self {
        Self {
            connect: error.is_connect(),
            timeout: error.is_timeout(),
            request: error.is_request(),
            body: error.is_body(),
            io_kind: io_kind(error),
            tls_kind: tls_kind(error),
        }
    }
}

fn deepest<T>(
    error: &(dyn Error + 'static),
    classify: impl Fn(&(dyn Error + 'static)) -> Option<T>,
) -> Option<T> {
    let mut cause = Some(error);
    let mut kind = None;
    while let Some(error) = cause {
        if let Some(classification) = classify(error) {
            kind = Some(classification);
        }
        if let Some(error) = error.downcast_ref::<io::Error>() {
            // io::Error::source delegates to its payload's source, skipping the
            // payload itself. Inspect it explicitly so nested IO kinds survive.
            cause = error.get_ref().map(|inner| inner as &(dyn Error + 'static));
        } else {
            cause = error.source();
        }
    }
    kind
}

fn io_kind(error: &(dyn Error + 'static)) -> Option<io::ErrorKind> {
    deepest(error, |error| {
        error.downcast_ref::<io::Error>().map(io::Error::kind)
    })
}

fn tls_kind(error: &(dyn Error + 'static)) -> Option<&'static str> {
    deepest(error, |error| {
        error.downcast_ref::<rustls::Error>().map(tls_label)
    })
}

fn tls_label(error: &rustls::Error) -> &'static str {
    use rustls::{AlertDescription as Alert, Error as Tls};
    match error {
        Tls::InappropriateMessage { .. } => "InappropriateMessage",
        Tls::InappropriateHandshakeMessage { .. } => "InappropriateHandshakeMessage",
        Tls::InvalidEncryptedClientHello(_) => "InvalidEncryptedClientHello",
        Tls::InvalidMessage(_) => "InvalidMessage",
        Tls::NoCertificatesPresented => "NoCertificatesPresented",
        Tls::UnsupportedNameType => "UnsupportedNameType",
        Tls::DecryptError => "DecryptError",
        Tls::EncryptError => "EncryptError",
        Tls::PeerIncompatible(_) => "PeerIncompatible",
        Tls::PeerMisbehaved(_) => "PeerMisbehaved",
        Tls::AlertReceived(alert) => match alert {
            Alert::CloseNotify => "AlertReceived.CloseNotify",
            Alert::UnexpectedMessage => "AlertReceived.UnexpectedMessage",
            Alert::BadRecordMac => "AlertReceived.BadRecordMac",
            Alert::RecordOverflow => "AlertReceived.RecordOverflow",
            Alert::HandshakeFailure => "AlertReceived.HandshakeFailure",
            Alert::BadCertificate => "AlertReceived.BadCertificate",
            Alert::UnsupportedCertificate => "AlertReceived.UnsupportedCertificate",
            Alert::CertificateRevoked => "AlertReceived.CertificateRevoked",
            Alert::CertificateExpired => "AlertReceived.CertificateExpired",
            Alert::CertificateUnknown => "AlertReceived.CertificateUnknown",
            Alert::IllegalParameter => "AlertReceived.IllegalParameter",
            Alert::UnknownCA => "AlertReceived.UnknownCA",
            Alert::AccessDenied => "AlertReceived.AccessDenied",
            Alert::DecodeError => "AlertReceived.DecodeError",
            Alert::DecryptError => "AlertReceived.DecryptError",
            Alert::ProtocolVersion => "AlertReceived.ProtocolVersion",
            Alert::InsufficientSecurity => "AlertReceived.InsufficientSecurity",
            Alert::InternalError => "AlertReceived.InternalError",
            Alert::UserCanceled => "AlertReceived.UserCanceled",
            Alert::MissingExtension => "AlertReceived.MissingExtension",
            Alert::UnsupportedExtension => "AlertReceived.UnsupportedExtension",
            Alert::UnrecognisedName => "AlertReceived.UnrecognisedName",
            Alert::CertificateRequired => "AlertReceived.CertificateRequired",
            Alert::NoApplicationProtocol => "AlertReceived.NoApplicationProtocol",
            _ => "AlertReceived.Other",
        },
        Tls::InvalidCertificate(_) => "InvalidCertificate",
        Tls::InvalidCertRevocationList(_) => "InvalidCertRevocationList",
        Tls::General(_) => "General",
        Tls::FailedToGetCurrentTime => "FailedToGetCurrentTime",
        Tls::FailedToGetRandomBytes => "FailedToGetRandomBytes",
        Tls::HandshakeNotComplete => "HandshakeNotComplete",
        Tls::PeerSentOversizedRecord => "PeerSentOversizedRecord",
        Tls::NoApplicationProtocol => "NoApplicationProtocol",
        Tls::BadMaxFragmentSize => "BadMaxFragmentSize",
        Tls::InconsistentKeys(_) => "InconsistentKeys",
        Tls::Other(_) => "Other",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claude_http1_trial_is_scoped_and_applies_to_retries() {
        use crate::{
            config::{AuthMode, Config},
            routing::{AdapterKind, Route},
            server::AppState,
        };
        use axum::{
            http::{HeaderMap, HeaderValue, Version},
            routing::post,
            Router,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/", post(|| async { "ok" })))
                .await
                .unwrap();
        });
        let shared = reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        // A request version does not override the shared client's H2 transport.
        let response = shared
            .post(&url)
            .version(Version::HTTP_11)
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), Version::HTTP_2);
        response.bytes().await.unwrap();

        for (auth, token, expected) in [
            (
                AuthMode::ClaudeOauth,
                "Bearer sk-ant-oat-test",
                Version::HTTP_11,
            ),
            (
                AuthMode::ClaudeOauth,
                "Bearer sk-ant-api-test",
                Version::HTTP_2,
            ),
            (AuthMode::KimiOauth, "Bearer kimi-test", Version::HTTP_2),
            (AuthMode::ApiKey, "", Version::HTTP_2),
            (
                AuthMode::Passthrough,
                "Bearer sk-ant-oat-test",
                Version::HTTP_11,
            ),
            (
                AuthMode::Passthrough,
                "Bearer sk-ant-api-test",
                Version::HTTP_2,
            ),
            (
                AuthMode::KimiOauth,
                "Bearer sk-ant-oat-test",
                Version::HTTP_2,
            ),
        ] {
            let mut config = Config::default();
            let provider = config.providers.get_mut("anthropic").unwrap();
            provider.auth = auth;
            provider.base_url = url.clone();
            provider.api_key_env = Some("SHUNT_HTTP1_TRIAL_UNUSED_KEY".into());
            let state = AppState::new(config, shared.clone()).unwrap();
            let route = Route {
                provider: "anthropic".into(),
                adapter: AdapterKind::Anthropic,
                model: "test".into(),
                upstream_model: "test".into(),
                effort: None,
                service_tier: None,
            };
            let mut headers = HeaderMap::new();
            if !token.is_empty() {
                headers.insert("authorization", HeaderValue::from_str(token).unwrap());
            }
            let response =
                super::super::post_upstream(&state, &route, &url, headers.clone(), Vec::new())
                    .await
                    .unwrap();
            assert_eq!(response.version(), expected, "{auth:?}");
            response.bytes().await.unwrap();
            let response = super::super::retry_upstream(
                &state,
                &route,
                &crate::config::AccountConfig::default(),
                &url,
                headers,
                Vec::new(),
                "local trial retry failed",
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.version(), expected, "retry {auth:?}");
            response.bytes().await.unwrap();
        }
        // Neither the pool nor its settings were changed by Claude sends.
        assert_eq!(
            shared.post(&url).send().await.unwrap().version(),
            Version::HTTP_2
        );
        server.abort();
    }

    #[test]
    fn nested_invalid_data_exposes_tls_alert_classification() {
        let tls = rustls::Error::AlertReceived(rustls::AlertDescription::DecodeError);
        let inner = io::Error::new(io::ErrorKind::InvalidData, tls);
        let error = anyhow::Error::new(io::Error::other(inner)).context("sensitive-context");
        assert_eq!(io_kind(error.as_ref()), Some(io::ErrorKind::InvalidData));
        assert_eq!(tls_kind(error.as_ref()), Some("AlertReceived.DecodeError"));
    }

    #[test]
    fn tls_general_payload_is_not_disclosed() {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::General("sensitive-url?token=sensitive-secret".into()),
        );
        assert_eq!(tls_kind(&error), Some("General"));
        let logged = format!("{:?} {:?}", io_kind(&error), tls_kind(&error));
        assert_eq!(logged, "Some(InvalidData) Some(\"General\")");
        assert!(!logged.contains("sensitive-"));
    }

    #[test]
    fn tls_variants_and_alerts_use_static_labels() {
        for (error, expected) in [
            (rustls::Error::DecryptError, "DecryptError"),
            (
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding),
                "InvalidCertificate",
            ),
            (
                rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
                "AlertReceived.HandshakeFailure",
            ),
            (
                rustls::Error::AlertReceived(rustls::AlertDescription::Unknown(255)),
                "AlertReceived.Other",
            ),
        ] {
            assert_eq!(tls_kind(&error), Some(expected));
        }
    }

    #[test]
    fn nested_io_cause_is_classified_without_formatting_payload() {
        let inner = io::Error::new(io::ErrorKind::ConnectionReset, "sensitive-source-message");
        let error = anyhow::Error::new(io::Error::other(inner)).context("sensitive-context");
        assert_eq!(
            io_kind(error.as_ref()),
            Some(io::ErrorKind::ConnectionReset)
        );
        let diagnostic = format!("{:?}", io_kind(error.as_ref()));
        assert_eq!(diagnostic, "Some(ConnectionReset)");
        assert!(!diagnostic.contains("sensitive-"));
    }

    #[tokio::test]
    async fn transport_diagnostics_identify_connection_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://sensitive-user:sensitive-password@{}/sensitive-path?token=sensitive-query#sensitive-fragment",
            listener.local_addr().unwrap()
        );
        drop(listener);
        let error = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(url.clone())
            .send()
            .await
            .unwrap_err();
        let diagnostics = Diagnostics::new(&error);
        assert_eq!(diagnostics.tls_kind, None);
        assert!(diagnostics.connect);
        assert!(diagnostics.request);
        assert!(!diagnostics.timeout);
        assert!(!diagnostics.body);
        assert_eq!(diagnostics.io_kind, Some(io::ErrorKind::ConnectionRefused));
        // Preserve the reproduction: Display alone cannot identify the cause.
        let redacted = error.without_url().to_string();
        assert_eq!(redacted, "error sending request");
        let logged = format!("{redacted} {diagnostics:?}");
        assert!(logged.contains("ConnectionRefused"));
        assert!(!logged.contains("sensitive-"));
        assert!(!logged.contains(&url));
        assert!(!logged.contains("127.0.0.1"));
    }

    #[test]
    fn unknown_source_is_not_formatted() {
        #[derive(Debug)]
        struct Opaque;
        impl std::fmt::Display for Opaque {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("source text must never be formatted")
            }
        }
        impl Error for Opaque {}
        assert_eq!(io_kind(&Opaque), None);
        assert_eq!(tls_kind(&Opaque), None);
    }
}
