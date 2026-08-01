//! Model/effort capability discovery for the Antigravity CLI.
//!
//! `agy` rejects an invalid model+effort pair outright (`gemini-3.1-pro`
//! accepts only `low` and `high` — passing `medium` fails the run), and the
//! valid set differs per model and changes as Google ships new ones. Rather
//! than hardcode a table that silently rots, discover it from `agy models`,
//! whose output lists one `<model>-<effort>` line per usable combination:
//!
//! ```text
//! gemini-3.6-flash-high
//! gemini-3.6-flash-medium
//! gemini-3.6-flash-low
//! gemini-3.1-pro-high
//! gemini-3.1-pro-low
//! claude-sonnet-4-6
//! ```
//!
//! A line with no effort suffix denotes a model that takes no `--effort` flag.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Stdio,
    time::Duration,
};

use tokio::sync::OnceCell;

/// Cap on the one-off `agy models` discovery call. Generous because the CLI
/// is markedly slower as a gateway subprocess (~20s) than from a shell (~2s),
/// and this runs once per process on the first Antigravity turn.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Effort levels ordered weakest to strongest.
const LADDER: [&str; 3] = ["low", "medium", "high"];

/// Effort used when a route does not pin one. Clamped per model, so a model
/// without `medium` resolves to its nearest supported level.
const DEFAULT_EFFORT: &str = "medium";

pub type EffortMatrix = BTreeMap<String, BTreeSet<String>>;

static MATRIX: OnceCell<EffortMatrix> = OnceCell::const_new();

/// Parse `agy models` output into model → supported efforts.
pub fn parse_models(output: &str) -> EffortMatrix {
    let mut matrix: EffortMatrix = BTreeMap::new();
    for line in output.lines() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        match LADDER
            .iter()
            .find_map(|effort| entry.strip_suffix(&format!("-{effort}")).zip(Some(*effort)))
        {
            Some((model, effort)) => {
                matrix
                    .entry(model.to_string())
                    .or_default()
                    .insert(effort.to_string());
            }
            // No effort suffix: record the model with an empty effort set so
            // callers can distinguish "takes no effort flag" from "unknown".
            None => {
                matrix.entry(entry.to_string()).or_default();
            }
        }
    }
    matrix
}

/// Discover and cache the matrix by invoking `agy models` once per process.
///
/// A failed, timed-out or unparseable invocation caches an empty matrix rather
/// than retrying: [`resolve_effort`] then passes the requested effort through
/// untouched and the CLI reports any invalid pair authoritatively.
pub async fn effort_matrix(agy_bin: &Path) -> &'static EffortMatrix {
    MATRIX
        .get_or_init(|| async {
            let mut command = tokio::process::Command::new(agy_bin);
            command.arg("models");
            // `agy` blocks indefinitely if it inherits a live stdin, which as a
            // gateway subprocess would wedge the request that triggered
            // discovery — no response, no headers, no keepalive.
            command.stdin(Stdio::null());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            // Belt and braces: discovery must never be able to stall a turn.
            command.kill_on_drop(true);

            let started = std::time::Instant::now();
            let output = tokio::time::timeout(DISCOVERY_TIMEOUT, command.output()).await;
            let elapsed_ms = started.elapsed().as_millis();
            match output {
                Ok(Ok(out)) if out.status.success() => {
                    let matrix = parse_models(&String::from_utf8_lossy(&out.stdout));
                    tracing::debug!(
                        models = matrix.len(),
                        elapsed_ms,
                        "discovered agy model efforts"
                    );
                    matrix
                }
                Ok(Ok(out)) => {
                    tracing::warn!(
                        status = ?out.status.code(),
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "`agy models` failed; effort values will be passed through unvalidated"
                    );
                    EffortMatrix::new()
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "could not run `agy models`; effort passed through");
                    EffortMatrix::new()
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_s = DISCOVERY_TIMEOUT.as_secs(),
                        "`agy models` timed out; effort passed through"
                    );
                    EffortMatrix::new()
                }
            }
        })
        .await
}

/// Pick the `--effort` value to pass for `model`, or `None` to omit the flag.
///
/// When the requested level is unsupported it is clamped to the nearest
/// supported one, breaking ties upward: a route asking for `medium` on a model
/// offering only `low`/`high` gets `high`, preserving the caller's intent that
/// this is a reasoning-tier request rather than quietly downgrading it.
///
/// An unknown model yields the requested effort unchanged — better to let the
/// CLI report an authoritative error than to guess on its behalf.
pub fn resolve_effort(
    matrix: &EffortMatrix,
    model: &str,
    requested: Option<&str>,
) -> Option<String> {
    let Some(supported) = matrix.get(model) else {
        return requested.map(str::to_string);
    };
    if supported.is_empty() {
        return None;
    }
    let requested = requested.unwrap_or(DEFAULT_EFFORT);
    if supported.contains(requested) {
        return Some(requested.to_string());
    }
    let target = LADDER.iter().position(|level| *level == requested)?;
    LADDER
        .iter()
        .enumerate()
        .filter(|(_, level)| supported.contains(**level))
        .min_by_key(|(index, _)| {
            let distance = index.abs_diff(target);
            // Equal distance prefers the stronger level.
            (distance, usize::from(*index < target))
        })
        .map(|(_, level)| (*level).to_string())
}
