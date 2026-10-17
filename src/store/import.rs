//! One-time import of the JSON snapshots into the store (FR-003).
//!
//! Per subsystem the decision is keyed on `import_record`, never on table
//! emptiness: a present record means "already imported (or deliberately
//! skipped)" even when the subsystem's tables are empty, and a snapshot that
//! appears after a no-snapshot boot is ignored. A missing record triggers the
//! import: read the snapshot, decode it strictly, and write its rows and the
//! record in one transaction. Any failure rolls both back and the error names
//! the subsystem and the path, so boot can refuse to start. The snapshot is
//! only read; a sidecar next to it ([`super::marker`]) records the import.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::marker::{self, ImportMarker};
use super::{pool_repo, sessions_repo, spend_repo, to_i64, Db, Store, StoreError, Tx, Value};
use crate::accounts::{AccountKey, QuotaState};
use crate::config::Config;
use crate::gateway::spend::{persist as spend_persist, store::SpendState};
use crate::gateway::{persist as gateway_persist, refresh::RefreshRecord};

/// Why a snapshot's bytes cannot be used. The lenient store-less loaders turn
/// this into a warning and a cold start; the import turns it into a boot error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoadIssue {
    /// The bytes are not the expected JSON document.
    NotJson(String),
    /// The document's `version` is not one this binary reads.
    Version { found: u32, expected: String },
}

impl std::fmt::Display for LoadIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson(error) => write!(f, "not valid json: {error}"),
            Self::Version { found, expected } => {
                write!(f, "version mismatch: found {found}, expected {expected}")
            }
        }
    }
}

/// A subsystem that can be imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Pool,
    Gateway,
    Spend,
}

impl Subsystem {
    /// The `import_record.subsystem` key and the name errors print.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pool => "pool",
            Self::Gateway => "gateway",
            Self::Spend => "spend",
        }
    }
}

/// Whether a subsystem is store-backed, and where its snapshot would be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Source {
    /// Not store-backed in this deployment: nothing is imported or recorded.
    #[default]
    Off,
    /// Store-backed; `snapshot` is the configured JSON state file, if any.
    On { snapshot: Option<PathBuf> },
}

/// The three subsystems' import inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSources {
    pub pool: Source,
    pub gateway: Source,
    pub spend: Source,
}

impl ImportSources {
    /// The plan's rule: the pool is store-backed whenever `[store]` is set (its
    /// `state_path` is only an import source); gateway and spend are
    /// store-backed when `[server.gateway]` / `[server.spend]` exist.
    pub fn from_config(config: &Config) -> Self {
        let server = &config.server;
        Self {
            pool: Source::On {
                snapshot: server.pool.as_ref().and_then(|p| p.state_path.clone()),
            },
            gateway: server
                .gateway
                .as_ref()
                .map_or(Source::Off, |gateway| Source::On {
                    snapshot: gateway.session_state_path().map(Path::to_path_buf),
                }),
            spend: server
                .spend
                .as_ref()
                .map_or(Source::Off, |spend| Source::On {
                    snapshot: spend.state_path().map(Path::to_path_buf),
                }),
        }
    }
}

/// What happened to one subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not store-backed; untouched.
    Off,
    /// An `import_record` already existed; nothing was read or written.
    AlreadyImported,
    /// No snapshot (unset or absent file): a `source_path NULL` record was written.
    NoSnapshot,
    /// The snapshot's rows and the record were committed.
    Imported { rows: u64 },
}

/// The per-subsystem outcomes of one [`import_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub pool: Outcome,
    pub gateway: Outcome,
    pub spend: Outcome,
}

/// Import every store-backed subsystem that has no `import_record` yet.
/// Call after the store is open (hold acquired, migrated) and before the
/// subsystems restore from it. The first failure stops the sequence; the
/// failing subsystem has no rows and no record, earlier ones keep theirs.
pub async fn import_all(
    store: &Store,
    sources: &ImportSources,
) -> Result<ImportReport, StoreError> {
    Ok(ImportReport {
        pool: import_one(store, Subsystem::Pool, &sources.pool).await?,
        gateway: import_one(store, Subsystem::Gateway, &sources.gateway).await?,
        spend: import_one(store, Subsystem::Spend, &sources.spend).await?,
    })
}

/// Rows decoded from a snapshot, ready to write.
enum Prepared {
    Pool(Vec<(AccountKey, QuotaState)>),
    Gateway(Vec<RefreshRecord>),
    Spend(Box<SpendState>),
}

impl Prepared {
    fn decode(subsystem: Subsystem, bytes: &[u8], path: &Path) -> Result<Self, String> {
        let issue = |issue: LoadIssue| issue.to_string();
        Ok(match subsystem {
            Subsystem::Pool => {
                Self::Pool(crate::state_persist::normalized_quotas(bytes).map_err(issue)?)
            }
            Subsystem::Gateway => Self::Gateway(unique_tokens(
                gateway_persist::parse_records(bytes).map_err(issue)?,
            )?),
            Subsystem::Spend => Self::Spend(Box::new(
                spend_persist::parse_state(bytes, path).map_err(issue)?,
            )),
        })
    }

    fn row_count(&self) -> usize {
        match self {
            Self::Pool(quotas) => quotas.len(),
            Self::Gateway(records) => records.len(),
            Self::Spend(state) => {
                state.limits.len()
                    + state.opaque_limits.len()
                    + state.audit.len()
                    + state.opaque_audit.len()
            }
        }
    }

    async fn write(&self, tx: &mut Tx) -> Result<(), StoreError> {
        match self {
            Self::Pool(quotas) => pool_repo::write_pool(tx, quotas).await,
            Self::Gateway(records) => {
                let none = std::collections::HashMap::new();
                let diff = sessions_repo::diff(&none, &sessions_repo::index(records.clone()));
                sessions_repo::apply_diff_in(tx, &diff).await
            }
            Self::Spend(state) => {
                let empty = SpendState {
                    next_audit_id: 1,
                    ..SpendState::default()
                };
                spend_repo::write_state(tx, &empty, state).await
            }
        }
    }
}

/// The write keys records by token hash ([`sessions_repo::index`]), so a
/// repeated hash would keep whichever copy came last (an active session or its
/// tombstone, by JSON order) and the recorded row count would overstate the
/// rows written. A repeat is a malformed snapshot, not one to collapse.
fn unique_tokens(records: Vec<RefreshRecord>) -> Result<Vec<RefreshRecord>, String> {
    let mut seen = std::collections::HashSet::with_capacity(records.len());
    match records
        .iter()
        .position(|record| !seen.insert(record.token_sha256.as_str()))
    {
        Some(index) => Err(format!(
            "duplicate token_sha256: refresh_tokens[{index}] repeats an earlier record"
        )),
        None => Ok(records),
    }
}

fn failure(
    subsystem: Subsystem,
    path: Option<&Path>,
    reason: impl std::fmt::Display,
) -> StoreError {
    StoreError::Import {
        subsystem: subsystem.name(),
        path: path.map_or_else(|| "(no snapshot)".to_string(), |p| p.display().to_string()),
        reason: reason.to_string(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

async fn has_record(store: &Store, subsystem: Subsystem) -> Result<bool, StoreError> {
    let mut tx = store.db().begin().await?;
    let found = tx
        .fetch_optional(
            "SELECT subsystem FROM import_record WHERE subsystem = $1",
            &[Value::from(subsystem.name())],
        )
        .await;
    tx.rollback().await?;
    Ok(found?.is_some())
}

/// The snapshot's bytes, or `None` when no path is set or the file is absent.
async fn read_snapshot(
    subsystem: Subsystem,
    path: Option<&Path>,
) -> Result<Option<Vec<u8>>, StoreError> {
    let Some(path) = path else {
        return Ok(None);
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failure(
            subsystem,
            Some(path),
            format!("unreadable: {error}"),
        )),
    }
}

async fn import_one(
    store: &Store,
    subsystem: Subsystem,
    source: &Source,
) -> Result<Outcome, StoreError> {
    let Source::On { snapshot } = source else {
        return Ok(Outcome::Off);
    };
    let path = snapshot.as_deref();
    let wrap = |error: StoreError| match error {
        error @ StoreError::Import { .. } => error,
        other => failure(subsystem, path, other),
    };
    if has_record(store, subsystem).await.map_err(wrap)? {
        return Ok(Outcome::AlreadyImported);
    }
    let bytes = read_snapshot(subsystem, path).await?;
    let prepared = match (&bytes, path) {
        (Some(bytes), Some(path)) => Some(
            Prepared::decode(subsystem, bytes, path)
                .map_err(|issue| failure(subsystem, Some(path), issue))?,
        ),
        _ => None,
    };
    let rows = prepared.as_ref().map_or(0, Prepared::row_count);
    let sha256 = bytes.as_deref().map(marker::sha256_hex);
    let imported_at = now_secs();

    let mut tx = store.begin_durable().await.map_err(wrap)?;
    let written = write_with_record(
        &mut tx,
        subsystem,
        prepared.as_ref(),
        path.filter(|_| bytes.is_some()),
        sha256.clone(),
        rows,
        imported_at,
    )
    .await;
    match written {
        Ok(()) => tx.commit().await.map_err(wrap)?,
        Err(error) => {
            let _ = tx.rollback().await;
            return Err(wrap(error));
        }
    }

    let (Some(path), Some(sha256)) = (path.filter(|_| bytes.is_some()), sha256) else {
        tracing::info!(
            subsystem = subsystem.name(),
            "no state snapshot to import; recorded"
        );
        return Ok(Outcome::NoSnapshot);
    };
    let marker = ImportMarker {
        backend: backend_name(store.db()).to_string(),
        imported_at,
        source_sha256: sha256,
    };
    // The data is already committed; a sidecar failure only costs the AC-031
    // warning on a later store-less boot, so it must not fail the boot.
    if let Err(error) = marker::write(path, &marker) {
        tracing::warn!(
            subsystem = subsystem.name(),
            path = %path.display(),
            %error,
            "imported the snapshot but could not write its .imported sidecar"
        );
    }
    tracing::info!(subsystem = subsystem.name(), path = %path.display(), rows, "imported state snapshot into the store");
    Ok(Outcome::Imported { rows: rows as u64 })
}

async fn write_with_record(
    tx: &mut Tx,
    subsystem: Subsystem,
    prepared: Option<&Prepared>,
    source_path: Option<&Path>,
    sha256: Option<String>,
    rows: usize,
    imported_at: u64,
) -> Result<(), StoreError> {
    if let Some(prepared) = prepared {
        prepared.write(tx).await?;
    }
    tx.execute(
        "INSERT INTO import_record (subsystem, imported_at, source_path, source_sha256, row_count) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            Value::from(subsystem.name()),
            to_i64(imported_at)?.into(),
            source_path.map(|p| p.display().to_string()).into(),
            sha256.into(),
            to_i64(rows as u64)?.into(),
        ],
    )
    .await?;
    Ok(())
}

fn backend_name(db: &Db) -> &'static str {
    match db {
        Db::Sqlite(_) => "sqlite",
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => "postgres",
    }
}

#[cfg(test)]
mod tests;
