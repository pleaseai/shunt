use super::*;

fn sqlite(url: &str) -> StoreConfig {
    StoreConfig {
        backend: StoreBackend::Sqlite,
        url: url.into(),
        max_connections: None,
        password_env: None,
        allow_insecure_remote_tls: false,
        hold_stale_seconds: DEFAULT_HOLD_STALE_SECONDS,
    }
}

fn postgres(url: &str) -> StoreConfig {
    StoreConfig {
        backend: StoreBackend::Postgres,
        ..sqlite(url)
    }
}

#[test]
fn toml_keys_deserialize_with_defaults() {
    let config: StoreConfig = toml::from_str(
        r#"
backend = "sqlite"
url = "/var/lib/shunt/store.db"
"#,
    )
    .unwrap();
    assert_eq!(config, sqlite("/var/lib/shunt/store.db"));
    assert!(!config.allow_insecure_remote_tls);
    assert_eq!(config.hold_stale_seconds, 30);

    let full: StoreConfig = toml::from_str(
        r#"
backend = "postgres"
url = "postgres://u@db/shunt"
max_connections = 8
password_env = "SHUNT_PG_PASSWORD"
allow_insecure_remote_tls = true
hold_stale_seconds = 60
"#,
    )
    .unwrap();
    assert_eq!(full.backend, StoreBackend::Postgres);
    assert_eq!(full.max_connections, Some(8));
    assert_eq!(full.password_env.as_deref(), Some("SHUNT_PG_PASSWORD"));
    assert!(full.allow_insecure_remote_tls);
    assert_eq!(full.hold_stale_seconds, 60);
}

#[test]
fn sqlite_accepts_paths_and_sqlite_urls() {
    for url in [
        "/var/lib/shunt/store.db",
        "store.db",
        "sqlite:store.db",
        "sqlite:///var/lib/shunt/store.db",
        "sqlite:my%20store.db",
        "my%20store.db",
    ] {
        sqlite(url)
            .validate()
            .unwrap_or_else(|e| panic!("{url}: {e}"));
    }
}

#[test]
fn sqlite_url_paths_are_percent_decoded_and_plain_paths_are_literal() {
    let encoded = sqlite("sqlite:///data/my%20store.db");
    assert_eq!(
        encoded.sqlite_file(),
        std::path::PathBuf::from("/data/my store.db")
    );
    assert_eq!(encoded.sanitized_target(), "sqlite:/data/my store.db");
    assert_eq!(
        sqlite("my%20store.db").sqlite_file(),
        std::path::PathBuf::from("my%20store.db")
    );
}

#[test]
fn sqlite_rejects_file_uri_names() {
    for url in [
        "file:store.db",
        "FILE:/data/store.db",
        "sqlite:file%3Astore.db",
    ] {
        let error = sqlite(url).validate().unwrap_err();
        assert!(
            error.contains("file: URI name is not supported"),
            "{url}: {error}"
        );
    }
}

#[test]
fn sqlite_rejects_query_options() {
    for url in [
        "sqlite:store.db?mode=rwc",
        "sqlite:store.db?mode%3Drwc",
        "sqlite:///var/lib/shunt/store.db?cache=private",
    ] {
        let error = sqlite(url).validate().unwrap_err();
        assert!(
            error.contains("query options are not supported"),
            "{url}: {error}"
        );
    }
}

#[test]
fn sqlite_rejects_in_memory_forms() {
    for url in [
        ":memory:",
        "sqlite::memory:",
        "sqlite://:memory:",
        "SQLITE::MEMORY:",
        "file::memory:?cache=shared",
        "sqlite:store.db?mode=memory",
        "sqlite:store.db?cache=shared&mode=memory",
        "file:%3Amemory%3A",
        "sqlite:file:%3amemory%3a",
        "sqlite:store.db?mode%3Dmemory",
        "",
        "   ",
    ] {
        assert!(sqlite(url).validate().is_err(), "{url:?} must be rejected");
    }
}

#[test]
fn sqlite_rejects_max_connections() {
    let mut config = sqlite("/tmp/a.db");
    config.max_connections = Some(4);
    let error = config.validate().unwrap_err();
    assert!(error.contains("max_connections"), "{error}");
}

#[test]
fn hold_stale_seconds_has_a_floor() {
    let mut config = sqlite("/tmp/a.db");
    config.hold_stale_seconds = MIN_HOLD_STALE_SECONDS - 1;
    assert!(config
        .validate()
        .unwrap_err()
        .contains("hold_stale_seconds"));
    config.hold_stale_seconds = 0;
    assert!(config.validate().is_err());
    config.hold_stale_seconds = MIN_HOLD_STALE_SECONDS;
    config.validate().unwrap();
}

#[test]
fn debug_and_serialize_never_print_the_url() {
    let config = postgres("postgres://user:hunter2@db.example/shunt");
    let debug = format!("{config:?}");
    assert!(!debug.contains("hunter2"), "{debug}");
    assert!(!debug.contains("db.example"), "{debug}");
    assert!(debug.contains("[redacted]"), "{debug}");
    let json = serde_json::to_string(&config).unwrap();
    assert!(
        !json.contains("hunter2") && !json.contains("db.example"),
        "{json}"
    );
    let toml = toml::to_string(&config).unwrap();
    assert!(
        !toml.contains("hunter2") && !toml.contains("db.example"),
        "{toml}"
    );
}

#[test]
fn sanitized_target_drops_credentials_and_query() {
    assert_eq!(
        postgres("postgres://user:hunter2@db.example:6543/shunt?password=pw2&sslmode=verify-full")
            .sanitized_target(),
        "postgres://db.example:6543/shunt"
    );
    assert_eq!(
        postgres("postgres://127.0.0.1/shunt?host=evil.example").sanitized_target(),
        "postgres://evil.example/shunt"
    );
    assert_eq!(
        sqlite("sqlite:///var/lib/shunt/store.db?mode=rwc").sanitized_target(),
        "sqlite:/var/lib/shunt/store.db"
    );
    assert_eq!(sqlite("store.db").sanitized_target(), "sqlite:store.db");
}

#[test]
fn inline_password_covers_userinfo_and_query() {
    assert!(postgres("postgres://u:pw@h/db").has_inline_password());
    assert!(postgres("postgres://u@h/db?password=pw").has_inline_password());
    assert!(postgres("postgres://u@h/db?sslmode=require&password=pw").has_inline_password());
    assert!(!postgres("postgres://u@h/db").has_inline_password());
    assert!(!postgres("postgres://u:@h/db").has_inline_password());
    assert!(!postgres("postgres://u@h/db?password=").has_inline_password());
}

#[test]
fn config_default_has_no_store_and_round_trips() {
    use crate::config::Config;
    let config = Config::default();
    assert!(config.store.is_none());
    let rendered = toml::to_string(&config).unwrap();
    assert!(!rendered.contains("[store]"), "{rendered}");
}

#[test]
fn config_validate_hooks_the_store_section() {
    use crate::config::{Config, ConfigError};
    let config = Config {
        store: Some(Box::new(sqlite(":memory:"))),
        ..Config::default()
    };
    assert!(matches!(
        config.validate(),
        Err(ConfigError::InvalidStore { .. })
    ));
    let config = Config {
        store: Some(Box::new(sqlite("/tmp/shunt-store.db"))),
        ..Config::default()
    };
    config.validate().unwrap();
}

#[cfg(not(feature = "store-postgres"))]
#[test]
fn postgres_without_the_feature_names_it() {
    let error = postgres("postgres://u@127.0.0.1/db")
        .validate()
        .unwrap_err();
    assert!(error.contains("--features store-postgres"), "{error}");
}

#[cfg(feature = "store-postgres")]
mod postgres_feature {
    use std::str::FromStr;

    use sqlx::postgres::PgConnectOptions;

    use super::*;
    use crate::config::CONFIG_ENV_LOCK;

    fn violation(url: &str) -> Option<String> {
        tls_violation(&PgConnectOptions::from_str(url).unwrap())
    }

    #[test]
    fn loopback_and_socket_hosts_need_no_tls() {
        for url in [
            "postgres://u@localhost/db",
            "postgres://u@LOCALHOST/db?sslmode=disable",
            "postgres://u@127.0.0.1/db",
            "postgres://u@127.5.6.7/db?sslmode=prefer",
            "postgres://u@[::1]/db",
            "postgres://u@%2Fvar%2Frun%2Fpostgresql/db",
            "postgres:///db?host=/var/run/postgresql",
            "postgres://u@127.0.0.1/db?hostaddr=::1",
        ] {
            assert_eq!(violation(url), None, "{url}");
        }
    }

    #[test]
    fn remote_hosts_require_verify_full() {
        for url in [
            "postgres://u@db.example/shunt",
            "postgres://u@db.example/shunt?sslmode=prefer",
            "postgres://u@db.example/shunt?sslmode=require",
            "postgres://u@db.example/shunt?sslmode=verify-ca",
            "postgres://u@db.example/shunt?sslmode=disable",
            "postgres://u@10.0.0.5/shunt?sslmode=require",
            "postgres://u@[2001:db8::1]/shunt",
        ] {
            assert!(violation(url).is_some(), "{url} must be rejected");
        }
        assert_eq!(
            violation("postgres://u@db.example/shunt?sslmode=verify-full"),
            None
        );
    }

    #[test]
    fn a_loopback_url_whose_host_override_is_remote_is_rejected() {
        for url in [
            "postgres://u@localhost/db?host=db.example",
            "postgres://u@127.0.0.1/db?hostaddr=203.0.113.9",
            "postgres://u@[::1]/db?hostaddr=203.0.113.9&sslmode=require",
        ] {
            assert!(violation(url).is_some(), "{url} must be rejected");
        }
    }

    #[test]
    fn validate_rejects_remote_without_verify_full_unless_opted_out() {
        let url = "postgres://u@db.example/shunt?sslmode=require";
        let error = postgres(url).validate().unwrap_err();
        assert!(error.contains("verify-full"), "{error}");
        assert!(error.contains("allow_insecure_remote_tls"), "{error}");
        assert!(!error.contains("u@"), "{error}");

        let mut opted_out = postgres(url);
        opted_out.allow_insecure_remote_tls = true;
        opted_out.validate().unwrap();

        postgres("postgres://u@db.example/shunt?sslmode=verify-full")
            .validate()
            .unwrap();
        postgres("postgres://u@localhost/shunt").validate().unwrap();
    }

    #[test]
    fn validate_rejects_garbage_url_and_zero_pool() {
        assert!(postgres("not a url").validate().is_err());
        let mut config = postgres("postgres://u@localhost/db");
        config.max_connections = Some(0);
        assert!(config.validate().is_err());
        config.max_connections = Some(4);
        config.validate().unwrap();
    }

    #[test]
    fn opt_out_logs_a_warning_without_credentials() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        let mut config = postgres("postgres://u:hunter2@db.example/shunt");
        config.allow_insecure_remote_tls = true;
        tracing::subscriber::with_default(subscriber, || config.validate().unwrap());
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("allow_insecure_remote_tls"), "{logs}");
        assert!(!logs.contains("hunter2"), "{logs}");
    }

    fn env_password(
        url: &str,
        name: Option<&str>,
        value: Option<&str>,
    ) -> Result<Option<String>, String> {
        let mut config = postgres(url);
        config.password_env = name.map(str::to_string);
        config.env_password(|var| {
            assert_eq!(Some(var), name);
            value.map(str::to_string)
        })
    }

    #[test]
    fn password_env_applies_only_without_an_inline_password() {
        let plain = "postgres://u@localhost/db";
        let used = env_password(plain, Some("X"), Some("from-env"));
        assert_eq!(used, Ok(Some("from-env".to_string())));
        // Inline password, in either place: the variable is ignored, and an
        // unset variable is not an error.
        for url in [
            "postgres://u:pw@localhost/db",
            "postgres://u@localhost/db?password=pw",
        ] {
            assert_eq!(
                env_password(url, Some("X"), Some("from-env")),
                Ok(None),
                "{url}"
            );
            assert_eq!(env_password(url, Some("X"), None), Ok(None), "{url}");
        }
        // No password_env configured: nothing to apply.
        assert_eq!(env_password(plain, None, Some("ignored")), Ok(None));
    }

    #[test]
    fn an_inline_password_never_reads_the_variable() {
        let mut config = postgres("postgres://u:pw@localhost/db");
        config.password_env = Some("X".to_string());
        assert_eq!(
            config.env_password(|_| panic!("must not read the env")),
            Ok(None)
        );
    }

    #[test]
    fn password_env_missing_or_empty_is_an_error() {
        let plain = "postgres://u@localhost/db";
        assert!(env_password(plain, Some("X"), None)
            .unwrap_err()
            .contains("unset or empty"));
        assert!(env_password(plain, Some("X"), Some("")).is_err());
    }

    #[test]
    fn pg_connect_options_reads_the_process_environment_under_the_env_lock() {
        let _guard = CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let name = format!("SHUNT_STORE_TEST_PW_{}", std::process::id());
        // Declared after the lock so it drops first: whatever happens below,
        // the variable gets its prior value back while the lock is held.
        let _restore = RestoreEnv {
            prior: std::env::var_os(&name),
            name: name.clone(),
        };
        let mut config = postgres("postgres://u@localhost/db");
        config.password_env = Some(name.clone());

        std::env::remove_var(&name);
        assert!(config.pg_connect_options().is_err());
        std::env::set_var(&name, "from-env");
        config.pg_connect_options().unwrap();
    }

    /// Puts `name` back to `prior` (set or unset) on drop, panics included.
    struct RestoreEnv {
        name: String,
        prior: Option<std::ffi::OsString>,
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match &self.prior {
                Some(value) => std::env::set_var(&self.name, value),
                None => std::env::remove_var(&self.name),
            }
        }
    }
}
