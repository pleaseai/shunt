//! Drives the real `shunt run` binary against a store, a Wiremock upstream,
//! and a scratch directory used as `HOME`, so no test touches `~/.shunt`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const ADMIN_TOKEN: &str = "store-boot-admin-token-0123456789abcdef";
pub const POOL_TOKEN: &str = "store-boot-pool-oauth-token";
pub const HOSTNAME: &str = "store-boot-test";
/// The refresh token whose hash the seeded gateway snapshot holds.
pub const SEEDED_REFRESH: &str = "store-boot-seeded-refresh-token-0123456789";
pub const UTILIZATION: f64 = 0.42;
const JWT_SECRET: &str = "store-boot-jwt-secret-0123456789abcdef0123456789";

static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

/// A loopback port no earlier call in this binary has handed out.
///
/// The probe listener is dropped before the child binds, and the OS may hand
/// the same ephemeral port to the next probe, so two fixtures could otherwise
/// be given one port. Remembering every port handed out rules that out within
/// the binary; cargo runs test binaries one after another.
pub fn free_port() -> u16 {
    static HANDED_OUT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    let mut handed_out = HANDED_OUT.lock().unwrap_or_else(|error| error.into_inner());
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        if !handed_out.contains(&port) {
            handed_out.push(port);
            return port;
        }
    }
}

/// Which store engine a case runs against.
pub enum Engine {
    Sqlite,
    #[cfg(feature = "store-postgres")]
    Postgres(postgres::Scratch),
}

/// One test's world: scratch dir, upstream, store, and config.
pub struct Fixture {
    pub dir: PathBuf,
    pub port: u16,
    pub upstream: MockServer,
    pub engine: Engine,
    spawned: AtomicU64,
    /// The environment guard, held until the fixture drops (last field, so it
    /// is released after everything else).
    _env: crate::common::EnvVars,
}

/// Which `state_path`s the config sets.
#[derive(Clone, Copy)]
pub struct Sections {
    pub gateway: bool,
    pub spend: bool,
}

impl Fixture {
    pub async fn new(tag: &str, engine: Engine) -> Self {
        // Every test builds its fixture first, and `spawn` reads `PATH`, so
        // the fixture holds the environment guard for the rest of the test.
        let env = crate::common::env_lock().await;
        let dir = std::env::temp_dir().join(format!(
            "shunt-store-boot-{tag}-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let upstream = MockServer::start().await;
        let reset = (now_secs() + 3_600).to_string();
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "anthropic-ratelimit-unified-5h-utilization",
                        UTILIZATION.to_string(),
                    )
                    .insert_header("anthropic-ratelimit-unified-5h-reset", reset)
                    .set_body_string(r#"{"type":"message"}"#),
            )
            .mount(&upstream)
            .await;
        Self {
            dir,
            port: free_port(),
            upstream,
            engine,
            spawned: AtomicU64::new(0),
            _env: env,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A gateway access token: with `[server.gateway]`, a pooled
    /// (credential-injecting) route requires one.
    pub fn bearer(&self) -> String {
        let identity = shunt::gateway::approval::Identity {
            sub: "dev@example.com".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        };
        shunt::gateway::jwt::mint(&identity, &self.base_url(), JWT_SECRET.as_bytes(), 3_600)
    }

    pub fn pool_snapshot(&self) -> PathBuf {
        self.path("pool-state.json")
    }

    pub fn gateway_snapshot(&self) -> PathBuf {
        self.path("gateway-sessions.json")
    }

    pub fn spend_snapshot(&self) -> PathBuf {
        self.path("gateway-spend.json")
    }

    fn store_toml(&self) -> String {
        match &self.engine {
            Engine::Sqlite => format!(
                "backend = \"sqlite\"\nurl = \"{}\"\n",
                self.path("store.db").display()
            ),
            #[cfg(feature = "store-postgres")]
            Engine::Postgres(scratch) => {
                format!("backend = \"postgres\"\nurl = \"{}\"\n", scratch.url())
            }
        }
    }

    /// Write a config listening on `port` and return its path.
    pub fn config(&self, port: u16, sections: Sections) -> PathBuf {
        let mut toml = format!(
            "[server]\nbind = \"127.0.0.1:{port}\"\ndefault_provider = \"anthropic\"\n\
             shutdown_timeout_seconds = 5\n\n\
             [server.admin]\ntokens_env = \"SHUNT_TEST_ADMIN_TOKENS\"\n\n\
             [server.pool]\nstate_path = \"{pool}\"\n\n\
             [providers.anthropic]\nbase_url = \"{upstream}\"\nauth = \"claude_oauth\"\n\n\
             [[providers.anthropic.accounts]]\nname = \"store-a\"\n\
             token_env = \"SHUNT_TEST_POOL_TOKEN\"\nuuid = \"store-a-uuid\"\n\n\
             [[routes]]\nmodel = \"pooled-model\"\nprovider = \"anthropic\"\n\n",
            pool = self.pool_snapshot().display(),
            upstream = self.upstream.uri(),
        );
        if sections.gateway {
            toml.push_str(&format!(
                "[server.gateway]\npublic_url = \"http://127.0.0.1:{port}\"\nstate_path = \"{}\"\n\n",
                self.gateway_snapshot().display()
            ));
        }
        if sections.spend {
            toml.push_str(&format!(
                "[server.spend]\nstate_path = \"{}\"\n\n",
                self.spend_snapshot().display()
            ));
        }
        toml.push_str("[store]\n");
        toml.push_str(&self.store_toml());
        let path = self.path(&format!("shunt-{port}.toml"));
        std::fs::write(&path, toml).expect("write config");
        path
    }

    /// Spawn `shunt run` with `config`; it is not yet ready.
    pub fn spawn(&self, config: &Path) -> Shunt {
        let n = self.spawned.fetch_add(1, Ordering::Relaxed);
        let log = self.path(&format!("shunt-{n}.log"));
        let stderr = std::fs::File::create(&log).expect("create log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_shunt"));
        command
            .arg("run")
            .arg("--config")
            .arg(config)
            // A clean environment: no developer `SHUNT_*` override or
            // `RUST_LOG` can change what the child does.
            .env_clear()
            .env("HOME", &self.dir)
            .env("HOSTNAME", HOSTNAME)
            .env("NO_COLOR", "1")
            .env("SHUNT_TEST_ADMIN_TOKENS", format!("ops:{ADMIN_TOKEN}"))
            .env("SHUNT_TEST_POOL_TOKEN", POOL_TOKEN)
            .env("SHUNT_GATEWAY_JWT_SECRET", JWT_SECRET)
            .env("SHUNT_GATEWAY_USERS", "dev@example.com:password")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        let child = command.spawn().expect("spawn shunt");
        Shunt {
            child: Some(child),
            log,
            base_url: self.base_url(),
        }
    }

    /// Spawn on the fixture's port and wait until `/health` answers.
    pub async fn boot(&self, sections: Sections) -> Shunt {
        let config = self.config(self.port, sections);
        let mut shunt = self.spawn(&config);
        shunt.wait_ready().await;
        shunt
    }

    /// Seed the three snapshots (`pool` v3 with no accounts, one gateway
    /// session for [`SEEDED_REFRESH`], an empty spend state).
    pub fn seed_snapshots(&self) {
        std::fs::write(self.pool_snapshot(), r#"{"version":3,"accounts":[]}"#).unwrap();
        let hash = hex::encode(Sha256::digest(SEEDED_REFRESH.as_bytes()));
        let sessions = serde_json::json!({
            "version": 1,
            "refresh_tokens": [{
                "token_sha256": hash,
                "identity": {"sub": "dev@example.com", "email": "dev@example.com", "name": "Dev"},
                "family": "store-boot-family",
                "inactive_since": null,
                "issued_at": now_secs(),
            }]
        });
        std::fs::write(self.gateway_snapshot(), sessions.to_string()).unwrap();
        std::fs::write(
            self.spend_snapshot(),
            r#"{"version":1,"limits":[],"audit":[],"next_audit_id":1}"#,
        )
        .unwrap();
    }

    /// Number of rows in `table`, read straight from the store.
    pub async fn rows(&self, table: &str) -> i64 {
        match &self.engine {
            Engine::Sqlite => {
                let conn = rusqlite::Connection::open(self.path("store.db")).unwrap();
                conn.busy_timeout(Duration::from_secs(10)).unwrap();
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
            }
            #[cfg(feature = "store-postgres")]
            Engine::Postgres(scratch) => scratch.count(table).await,
        }
    }

    pub async fn finish(self) {
        #[cfg(feature = "store-postgres")]
        if let Engine::Postgres(scratch) = &self.engine {
            scratch.drop_schema().await;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A running (or exited) `shunt run` child.
pub struct Shunt {
    child: Option<Child>,
    log: PathBuf,
    pub base_url: String,
}

impl Shunt {
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("running child").id()
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// When the first log line containing `needle` was written, as an
    /// `Instant` (log timestamps are UTC `YYYY-MM-DDTHH:MM:SS.ffffffZ`).
    pub fn logged_at(&self, needle: &str) -> Instant {
        let log = self.log();
        let line = log
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no log line contains {needle:?}:\n{log}"));
        let stamp = line.split_whitespace().next().expect("timestamp");
        let logged = parse_utc(stamp).unwrap_or_else(|| panic!("unparsed timestamp {stamp:?}"));
        let age = SystemTime::now().duration_since(logged).unwrap_or_default();
        Instant::now() - age
    }

    pub async fn wait_ready(&mut self) {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("shunt exited during boot ({status}):\n{}", self.log());
            }
            let health = client
                .get(format!("{}/health", self.base_url))
                .timeout(Duration::from_secs(1))
                .send()
                .await;
            if health.is_ok_and(|r| r.status().is_success()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shunt never became ready:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for the child to exit on its own.
    pub async fn wait_exit(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "shunt did not exit:\n{}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// SIGTERM (graceful shutdown) and wait for the exit.
    pub async fn terminate(&mut self) -> ExitStatus {
        let pid = libc::pid_t::try_from(self.pid()).expect("pid fits pid_t");
        // SAFETY: plain kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        self.wait_exit(Duration::from_secs(30)).await
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac]Z` to a `SystemTime`.
fn parse_utc(stamp: &str) -> Option<SystemTime> {
    let stamp = stamp.strip_suffix('Z')?;
    let (date, time) = stamp.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()?;
    let nanos: u32 = format!("{fraction:0<9}").get(..9)?.parse().ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

impl Drop for Shunt {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One proxied `POST /v1/messages` to the pooled account.
pub async fn post_message(client: &reqwest::Client, fixture: &Fixture) -> reqwest::Response {
    client
        .post(format!("{}/v1/messages", fixture.base_url()))
        .bearer_auth(fixture.bearer())
        .header("content-type", "application/json")
        .body(r#"{"model":"pooled-model","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("proxied request")
}

/// Holds a SQLite write lock from another connection until released.
pub struct SqliteLock {
    release: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SqliteLock {
    /// `BEGIN <mode>` on `path`; returns once the lock is held.
    pub fn hold(path: &Path, mode: &'static str) -> Self {
        let path = path.to_path_buf();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (held, ready) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.busy_timeout(Duration::from_secs(30)).unwrap();
            conn.execute_batch(&format!("BEGIN {mode}")).unwrap();
            held.send(()).unwrap();
            let _ = released.recv();
            conn.execute_batch("ROLLBACK").unwrap();
        });
        ready.recv().expect("lock thread started");
        Self {
            release,
            thread: Some(thread),
        }
    }

    pub fn release(mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[cfg(feature = "store-postgres")]
#[path = "postgres.rs"]
pub mod postgres;
