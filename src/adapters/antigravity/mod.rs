//! Antigravity CLI adapter (`agy`).
//!
//! `agy` is not a text-completion endpoint — it is a full agent with its own
//! tool set (file edits, shell, search, browser) and its own loop. This adapter
//! therefore runs it in agentic print mode and translates its
//! `--output-format stream-json` events back into Anthropic Messages SSE, so a
//! Gemini-routed turn can actually do work rather than only describe it.
//!
//! Consequences worth knowing before routing traffic here:
//!
//! - Tools are `agy`'s, not the caller's. `tools` supplied on the Messages
//!   request are not forwarded, and no `tool_use` block is ever returned; the
//!   CLI resolves its own tool calls internally and returns finished work.
//! - It runs with `--dangerously-skip-permissions`, because a print-mode run
//!   has no interactive channel to approve a permission prompt on. `--add-dir`
//!   is not a sandbox: the agent still has shell access and can act outside
//!   that directory. Treat this provider as arbitrary code execution as the
//!   user running shunt, and see [`resolve_workspace`] for the trust boundary
//!   on where it starts. Each run is isolated in its own process group so every
//!   cancellation path also kills tool subprocesses spawned by `agy`.

mod child;
pub mod models;
pub mod stream;

use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    error::ShuntError,
    request::RequestBody,
    routing::Route,
    server::AppState,
};

use self::{
    child::AgyChild,
    models::EffortChoice,
    stream::{AgyEnd, Translator},
};

/// Wall-clock cap handed to `agy --print-timeout`.
///
/// The CLI's own default is 5 minutes, which truncates genuine multi-step
/// agent runs and surfaces to the caller as a turn that delivered nothing.
const PRINT_TIMEOUT: &str = "30m";

/// Outer cap shunt enforces itself, independent of `--print-timeout`.
///
/// `--print-timeout` is delegated to the process being supervised, so it does
/// not protect against a wedged CLI or a descendant holding stdout open. Kept
/// above `PRINT_TIMEOUT` so the CLI's own limit reports first when it works.
const HARD_TIMEOUT: Duration = Duration::from_secs(35 * 60);

/// Cap on `agy` stderr retained for diagnostics, in bytes.
const STDERR_LIMIT: usize = 2000;

/// Grace period for the child to exit once the turn is over, before it is killed.
///
/// The turn is over by this point — either the terminal result arrived or
/// stdout reached EOF — and neither says the process ended. Short, because the
/// only thing left is reaping; non-zero, so a normally-exiting child still
/// reports its own status instead of being killed out from under it.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Cap on waiting for the stderr drain to publish before a failure is reported.
///
/// The child is already reaped or killed wherever this is awaited, so the task
/// is finishing regardless; the bound only stops a descendant that inherited
/// the stderr pipe from holding the response open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Environment override for the directory `agy` is allowed to work in.
const WORKSPACE_ENV: &str = "SHUNT_AGY_WORKSPACE";

/// Kill every `agy` process group this process still has running.
///
/// Shutdown's counterpart to the per-turn cancellation paths. Each run is
/// isolated in its own process group (see [`child::AgyChild`]), which is what
/// lets a cancelled turn take the CLI's tool subprocesses with it — but it also
/// means a signal sent to shunt's own process group no longer reaches the
/// agent. Without this, ctrl-c would leave a permission-skipping agent holding
/// the graceful drain open for up to [`HARD_TIMEOUT`], and the second-signal
/// `std::process::exit` would skip [`child::AgyChild`]'s destructor entirely and
/// orphan it. Called from `shutdown.rs` on both paths.
pub fn terminate_all_agy_groups() {
    child::terminate_all_groups();
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityAdapter;

impl Adapter for AntigravityAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        _uri: &'a Uri,
        _headers: &'a HeaderMap,
        body: RequestBody,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            let request = body.json();
            let prompt = extract_antigravity_prompt(request);
            let is_streaming = request
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let agy_bin = find_agy_binary().ok_or_else(agy_not_found)?;

            let provider = state.config.providers.get(&route.provider);
            let roots = provider
                .map(|provider| provider.workspace_roots.clone())
                .unwrap_or_default();
            let sandbox = provider.is_none_or(|provider| provider.sandbox);
            // Runtime half of Config::validate's boot check: `server.bind` is
            // restart-only, so a reload cannot make the socket already serving
            // public traffic safe for an unsandboxed agent.
            if !sandbox && !state.boot_is_loopback {
                return Err(adapter_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Antigravity is unsandboxed while the running listener is non-loopback. A reload cannot move the listener; restart shunt on a loopback bind or re-enable the sandbox."
                        .to_string(),
                ));
            }
            let workspace = resolve_workspace(request, &roots)?;

            let matrix = models::effort_matrix(&agy_bin, &route.upstream_model).await;
            let effort = match models::resolve_effort(
                &matrix,
                &route.upstream_model,
                route.effort.as_deref(),
            ) {
                EffortChoice::Use(effort) => Some(effort),
                EffortChoice::Omit => None,
                // Operator intent shunt cannot satisfy. Say so instead of
                // substituting a neighbouring level behind their back.
                EffortChoice::Unsupported {
                    model,
                    requested,
                    supported,
                } => {
                    let detail = if supported.is_empty() {
                        format!(
                            "model {model} takes no effort level, but effort \"{requested}\" was configured"
                        )
                    } else {
                        format!(
                            "model {model} does not support effort \"{requested}\" (supported: {})",
                            supported.join(", ")
                        )
                    };
                    return Err(adapter_error(StatusCode::BAD_REQUEST, detail));
                }
            };

            let mut cmd = Command::new(&agy_bin);
            cmd.arg("-p").arg(&prompt);
            cmd.arg("--model").arg(&route.upstream_model);
            if let Some(effort) = effort {
                cmd.arg("--effort").arg(effort);
            }
            cmd.arg("--output-format").arg("stream-json");
            // Print mode cannot service an interactive approval prompt.
            cmd.arg("--dangerously-skip-permissions");
            // ...which is why the sandbox matters. Skipping permissions without
            // it leaves an agent with shell access and no workspace boundary:
            // refusing an unlisted directory only moves where it starts, and a
            // path named in the prompt is still reachable from anywhere.
            if sandbox {
                cmd.arg("--sandbox");
            }
            cmd.arg("--print-timeout").arg(PRINT_TIMEOUT);
            cmd.arg("--add-dir").arg(&workspace);
            // Without this the agent inherits the gateway process's directory
            // and operates on whatever tree shunt happened to be started in.
            cmd.current_dir(&workspace);
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            // Isolate the agent and every tool it spawns. It no longer receives
            // Ctrl-C sent to shunt's process group, so per-turn cancellation and
            // gateway shutdown explicitly terminate its process group.
            #[cfg(unix)]
            cmd.process_group(0);
            // Without this, a client disconnect drops the body stream and
            // leaves a permission-skipping agent running unsupervised for up
            // to PRINT_TIMEOUT, still editing files and burning quota.
            cmd.kill_on_drop(true);

            let mut child = AgyChild::new(
                cmd.spawn()
                    .map_err(|err| agy_failure(&format!("could not start the CLI: {err}")))?,
            );

            let stdout = child
                .inner_mut()
                .stdout
                .take()
                .ok_or_else(|| agy_failure("the CLI produced no stdout pipe"))?;
            // stderr must be drained concurrently: left unread, a verbose
            // failure fills the pipe buffer, blocks the child's write, and
            // wedges the run behind keepalives that make it look healthy.
            let mut stderr_log = drain_stderr(child.inner_mut());

            let message_id = format!("msg_agy_{:016x}", rand::random::<u64>());
            let mut translator = Translator::new(&route.model, message_id);
            let mut lines = BufReader::new(stdout).lines();

            if is_streaming {
                // One deadline for the whole turn, not a fresh allowance per
                // line. Timing out each `next_line()` individually means any
                // CLI that keeps emitting tool progress resets the cap forever,
                // so a wedged permission-skipping child would never be reaped.
                let deadline = tokio::time::Instant::now() + HARD_TIMEOUT;
                // `timeout_at` below reports the deadline to an active reader,
                // but it is itself only polled with the response body. Keep a
                // separate supervisor alive in the stream state so backpressure
                // cannot suspend the wall-clock containment guarantee. The
                // child is shared with it rather than moved, because the
                // supervisor must be able to reap as well as signal: a client
                // that stalls without disconnecting never polls this stream, so
                // its `terminate` below is the one thing that would not run.
                let child = std::sync::Arc::new(tokio::sync::Mutex::new(child));
                let deadline_guard = AgyChild::arm_deadline(child.clone(), deadline);
                let stream_state = (lines, translator, child, stderr_log, false, deadline_guard);
                let sse_stream = futures_util::stream::unfold(
                    stream_state,
                    move |(
                        mut lines,
                        mut translator,
                        child,
                        mut stderr_log,
                        mut finished,
                        deadline_guard,
                    )| async move {
                        if finished {
                            return None;
                        }
                        loop {
                            let next = tokio::time::timeout_at(deadline, lines.next_line()).await;
                            match next {
                                Ok(Ok(Some(line))) => {
                                    let chunk = translator.on_line(&line);
                                    // Terminal event: emit whatever it produced
                                    // and finish, rather than reading to EOF. A
                                    // tool descendant holding the inherited
                                    // stdout pipe open would otherwise stall a
                                    // finished turn until the deadline.
                                    //
                                    // Checked before the emptiness test, not
                                    // after: a *failed* result records `end` and
                                    // returns an empty chunk, so an end-check at
                                    // the return site below would never see it.
                                    if translator.end().is_some() {
                                        finished = true;
                                        let mut tail = chunk;
                                        // Match `end` directly rather than going
                                        // through `terminal_failure`. Both of the
                                        // reachable outcomes here carry their own
                                        // message, so stderr is not consulted —
                                        // and routing through a helper whose
                                        // `None` arm *does* read stderr would
                                        // silently re-arm the reap-before-publish
                                        // race the moment that helper changed,
                                        // since this log has not been settled.
                                        match translator.end() {
                                            Some(AgyEnd::Failed(message)) => {
                                                let message = message.clone();
                                                tail.push_str(&translator.on_text(&format!(
                                                    "\n\n[agy error] {message}"
                                                )));
                                                tail.push_str(&translator.finish_with_error(None));
                                            }
                                            _ => tail.push_str(&translator.finish()),
                                        }
                                        // The turn is over and streaming never
                                        // reports an exit status, so there is
                                        // nothing to wait for — unlike the
                                        // non-streaming path, which grants
                                        // EXIT_GRACE precisely to read one.
                                        child.lock().await.terminate().await;
                                        return Some((
                                            Ok::<_, Infallible>(axum::body::Bytes::from(tail)),
                                            (
                                                lines,
                                                translator,
                                                child,
                                                stderr_log,
                                                finished,
                                                deadline_guard,
                                            ),
                                        ));
                                    }
                                    if chunk.is_empty() {
                                        continue;
                                    }
                                    return Some((
                                        Ok::<_, Infallible>(axum::body::Bytes::from(chunk)),
                                        (
                                            lines,
                                            translator,
                                            child,
                                            stderr_log,
                                            finished,
                                            deadline_guard,
                                        ),
                                    ));
                                }
                                // Every remaining case ends the turn. Headers
                                // are already committed, so the only place a
                                // failure can still be reported is the message
                                // body — closing silently would report a crash
                                // as a successful empty answer.
                                outcome => {
                                    finished = true;
                                    let timed_out = outcome.is_err();
                                    // Kill and reap before reading stderr, on
                                    // every terminal path rather than only the
                                    // timeout: a premature stdout EOF leaves
                                    // the agent alive, and an unbounded `wait`
                                    // would hang the response behind it. On a
                                    // clean exit this is a no-op.
                                    child.lock().await.terminate().await;
                                    // Only then wait for the drain to publish.
                                    // Reading the buffer first would report "no
                                    // stderr output" for a CLI that wrote a
                                    // diagnostic and exited immediately.
                                    stderr_log.settle().await;
                                    let mut tail = String::new();
                                    let failure = terminal_failure(
                                        translator.end(),
                                        timed_out,
                                        &stderr_log.log,
                                    );
                                    if let Some(message) = failure {
                                        tail.push_str(
                                            &translator
                                                .on_text(&format!("\n\n[agy error] {message}")),
                                        );
                                        tail.push_str(
                                            &translator.finish_with_error(Some(&message)),
                                        );
                                    } else {
                                        tail.push_str(&translator.finish());
                                    }
                                    return Some((
                                        Ok::<_, Infallible>(axum::body::Bytes::from(tail)),
                                        (
                                            lines,
                                            translator,
                                            child,
                                            stderr_log,
                                            finished,
                                            deadline_guard,
                                        ),
                                    ));
                                }
                            }
                        }
                    },
                );

                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream; charset=utf-8")
                    .header("Cache-Control", "no-cache")
                    // `agy` can be silent for tens of seconds before its first
                    // event while the CLI boots and the model takes its first
                    // turn. The shared keepalive covers that gap; the ping
                    // frames the translator emits per tool step then report
                    // genuine progress on top of it.
                    .body(Body::from_stream(crate::keepalive::with_pings(
                        sse_stream,
                        Duration::from_secs(state.config.server.sse_keepalive_seconds),
                    )))
                    .map_err(|err| {
                        adapter_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("failed to build SSE response: {err}"),
                        )
                    })?;
                return Ok((StatusCode::OK, response));
            }

            let drained = tokio::time::timeout(HARD_TIMEOUT, async {
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = translator.on_line(&line);
                    // Stop at the terminal event rather than reading to EOF.
                    // `agy` spawns tool descendants that inherit stdout; one
                    // holding the pipe open after the result would otherwise
                    // stall a finished turn until HARD_TIMEOUT and then report
                    // it as a failure. Checked after `on_line` regardless of
                    // what it returned: a failed result records `end` and
                    // returns nothing to emit.
                    if translator.end().is_some() {
                        break;
                    }
                }
            })
            .await;
            if drained.is_err() {
                child.terminate().await;
                stderr_log.settle().await;
                return Err(agy_failure(&format!(
                    "no result after {}s; {}",
                    HARD_TIMEOUT.as_secs(),
                    stderr_log.text()
                )));
            }
            // The loop above stops at the terminal result, so the common way
            // here is with the pipe still open and the child still alive; the
            // other way is a genuine stdout EOF, which likewise does not imply
            // the process exited. Either way an unbounded wait would hang the
            // response behind a permission-skipping agent. Give it a short
            // grace period to exit on its own — a normally-exiting child then
            // reports its own status, which the failure path below uses — then
            // kill if it overstays.
            let status = match tokio::time::timeout(EXIT_GRACE, child.inner_mut().wait()).await {
                Ok(status) => status,
                Err(_) => {
                    child.terminate().await;
                    child.inner_mut().wait().await
                }
            };

            // Not `terminal_failure` here: its `None` arm reports "stopped
            // without reporting a result" with no exit status, and because it
            // never returns `None` for a missing result, chaining `.or_else`
            // onto it left the status message unreachable. On this path the
            // process has been reaped, so the status is available and is the
            // only signal left about *why* nothing came back — report it.
            let failure = match translator.end() {
                Some(AgyEnd::Success) => None,
                Some(AgyEnd::Failed(message)) => Some(message.clone()),
                None => {
                    // Only this arm reads stderr, so only this arm waits for the
                    // drain. Settling unconditionally cost every *successful*
                    // turn the full DRAIN_GRACE whenever a descendant still held
                    // the pipe — which is precisely the case the early break was
                    // added to make fast.
                    stderr_log.settle().await;
                    let code = status.ok().and_then(|status| status.code()).unwrap_or(-1);
                    Some(format!(
                        "the CLI exited without a result (status {code}); {}",
                        stderr_log.text()
                    ))
                }
            };
            // Unlike streaming and hard-timeout paths, a natural non-streaming
            // exit settles a missing-result diagnostic before sweeping. Linux
            // keeps the struct pid backing a process-group id alive while the
            // group has members, so the pgid cannot be recycled while a
            // descendant remains; an already-empty group simply yields ESRCH.
            child.sweep_descendants();
            if let Some(message) = failure {
                return Err(agy_failure(&message));
            }

            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                axum::http::HeaderValue::from_static("application/json"),
            );
            let response =
                (StatusCode::OK, headers, axum::Json(translator.to_message())).into_response();
            Ok((StatusCode::OK, response))
        })
    }
}

/// Shared buffer holding a bounded prefix of the child's stderr.
///
/// Raw bytes, decoded only when read out. `agy` and its descendants are
/// arbitrary programs whose stderr is not guaranteed to be UTF-8, and deferring
/// the lossy decode to one contiguous prefix means no multi-byte character can
/// be split across two conversions — which is what a chunked decode would do.
type StderrLog = Arc<Mutex<Vec<u8>>>;

/// `agy` stderr plus the task draining it.
///
/// The handle is kept so a caller can wait for the drain to publish before
/// reading the buffer. Without it, a CLI that writes a diagnostic and exits
/// immediately is reaped before the task runs, and the failure surfaces as
/// "no stderr output" instead of what actually went wrong.
struct StderrDrain {
    log: StderrLog,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl StderrDrain {
    /// Wait for the drain to finish before its buffer is read.
    ///
    /// Bounded: by every call site the child is already reaped or killed, so
    /// the task is finishing anyway — but a descendant holding the inherited
    /// stderr pipe open must not be able to hang the response.
    async fn settle(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(DRAIN_GRACE, handle).await;
        }
    }

    fn text(&self) -> String {
        stderr_text(&self.log)
    }
}

/// Read the child's stderr concurrently into a bounded buffer.
///
/// Draining must happen alongside the run: left unread, a verbose failure fills
/// the pipe buffer and blocks the child's writes.
///
/// Reads fixed-size chunks rather than lines. Line-oriented reads — whether
/// `lines()` or `read_until` — accumulate an entire line before anything can
/// check its size, so a child emitting newline-free output (a progress bar
/// redrawing with `\r`, or binary noise) grows the buffer without bound. Only
/// `STDERR_LIMIT` bytes are ever retained, and reading continues past that so
/// the child still never blocks on a full pipe.
fn drain_stderr(child: &mut tokio::process::Child) -> StderrDrain {
    let log: StderrLog = Arc::new(Mutex::new(Vec::new()));
    let Some(stderr) = child.stderr.take() else {
        return StderrDrain { log, handle: None };
    };
    let sink = Arc::clone(&log);
    let handle = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut chunk = [0u8; 4096];
        loop {
            // A genuine I/O error means the pipe is gone, so stop. Notably a
            // non-UTF-8 byte is no longer an error at all: `lines()` reported
            // one as `Err(InvalidData)`, which ended the drain and left the
            // child free to block forever on a pipe nobody was reading.
            let read = match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if let Ok(mut buffer) = sink.lock() {
                let room = STDERR_LIMIT.saturating_sub(buffer.len());
                if room > 0 {
                    buffer.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    });
    StderrDrain {
        log,
        handle: Some(handle),
    }
}

fn stderr_text(log: &StderrLog) -> String {
    let text = log
        .lock()
        .map(|buffer| String::from_utf8_lossy(&buffer).trim().to_string())
        .unwrap_or_default();
    if text.is_empty() {
        "no stderr output".to_string()
    } else {
        truncate(&text, STDERR_LIMIT)
    }
}

/// Describe how a run ended, or `None` when it genuinely succeeded.
///
/// A run that reaches EOF without a terminal `result` event has crashed, been
/// killed, or emitted output we could not parse. Reporting that as `end_turn`
/// would turn every one of those into a silent empty success.
fn terminal_failure(end: Option<&AgyEnd>, timed_out: bool, stderr: &StderrLog) -> Option<String> {
    if timed_out {
        return Some(format!(
            "no output for {}s; {}",
            HARD_TIMEOUT.as_secs(),
            stderr_text(stderr)
        ));
    }
    match end {
        Some(AgyEnd::Success) => None,
        Some(AgyEnd::Failed(message)) => Some(message.clone()),
        None => Some(format!(
            "the CLI stopped without reporting a result; {}",
            stderr_text(stderr)
        )),
    }
}

/// Shape a missing `agy` binary as an Anthropic-form error.
///
/// Worth naming the search path explicitly: a service manager commonly runs
/// shunt under a restricted `PATH`. Homebrew's `brew services` unit sets
/// `PATH=/opt/homebrew/bin:/opt/homebrew/sbin:/usr/bin:/bin:/usr/sbin:/sbin`,
/// which excludes `~/.local/bin` — the default install location for `agy` — so
/// a provider that works in a shell returns 503 under the service with no
/// indication why. `AGY_BIN` is the fix, and the message has to say so.
fn agy_not_found() -> AdapterError {
    let message = "Antigravity CLI (agy) not found on PATH, in ~/.gemini/antigravity-cli/bin, or at $AGY_BIN. \
         Install agy, or set AGY_BIN to its absolute path — a service manager \
         (for example `brew services`) may run shunt with a PATH that excludes it."
        .to_string();
    adapter_error(StatusCode::SERVICE_UNAVAILABLE, message)
}

/// Shape a failed `agy` invocation as an Anthropic-form error carrying the
/// CLI's own diagnosis, which is often a precise, actionable line such as
/// `gemini-3.1-pro has no "medium" effort`.
fn agy_failure(detail: &str) -> AdapterError {
    let detail = truncate(detail.trim(), STDERR_LIMIT);
    let detail = if detail.is_empty() {
        "no output".to_string()
    } else {
        detail
    };
    adapter_error(
        StatusCode::BAD_GATEWAY,
        format!("Antigravity CLI (agy) failed: {detail}"),
    )
}

/// Build an [`AdapterError`] whose body is the Anthropic error shape, as
/// AGENTS.md requires for gateway-owned errors.
fn adapter_error(status: StatusCode, message: String) -> AdapterError {
    AdapterError {
        response: Box::new(ShuntError::new(status, "api_error", message.clone()).into_response()),
        message,
        failure: None,
    }
}

/// Truncate to at most `limit` bytes, without splitting a UTF-8 character.
///
/// Slices at the nearest char boundary rather than rebuilding the string one
/// `char` at a time, which allocated per character for a value that is usually
/// returned whole.
pub fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let end = (0..=limit)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(0);
    text[..end].to_string()
}

/// Directory `agy` is launched in and granted via `--add-dir`.
///
/// This is a trust boundary, not a convenience. `agy` runs with
/// `--dangerously-skip-permissions`, so this directory is one an unattended
/// agent can read, write, and run shell commands in. Resolution order:
///
/// 1. `SHUNT_AGY_WORKSPACE` — operator-set, therefore authoritative. If it is
///    set but not a directory the request fails rather than falling back, so a
///    typo cannot silently downgrade to a wider directory.
/// 2. A `Working directory:` line in the request's system prompt, but *only*
///    when it canonicalizes to a path inside one of the provider's configured
///    `workspace_roots`. System-prompt text is client-controlled and routinely
///    quotes fetched documents and tool output, so it is prompt-injectable;
///    canonicalizing first resolves `..` and symlinks that would otherwise
///    escape a naive prefix check. With no roots configured, prompt-derived
///    paths are ignored entirely — the safe default.
/// 3. The gateway's own directory.
pub fn resolve_workspace(request: &Value, roots: &[String]) -> Result<PathBuf, AdapterError> {
    if let Some(raw) = std::env::var_os(WORKSPACE_ENV) {
        let path = PathBuf::from(&raw);
        // Canonicalized, not just checked. The same value becomes both
        // `--add-dir` and `current_dir`, so a relative one is resolved twice
        // against different bases: `SHUNT_AGY_WORKSPACE=repo` passes `is_dir()`
        // against the gateway's directory, then the child — already moved into
        // `repo` — reads `--add-dir repo` as `repo/repo`. The granted directory
        // would not be the one that was vetted. Canonicalizing also folds the
        // existence check in, since it fails on a path that is not there.
        return match path.canonicalize() {
            Ok(canonical) if canonical.is_dir() => Ok(canonical),
            _ => Err(adapter_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "{WORKSPACE_ENV} is set to {}, which is not a directory",
                    path.display()
                ),
            )),
        };
    }

    if let Some(requested) = system_prompt_text(request)
        .as_deref()
        .and_then(parse_working_directory)
    {
        if let Some(allowed) = permitted_workspace(&requested, roots) {
            return Ok(allowed);
        }
        tracing::debug!(
            requested = %requested.display(),
            "ignoring prompt-derived workspace outside the configured workspace_roots"
        );
    }

    Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Canonicalize `requested` and accept it only if it lands inside a root.
///
/// Both sides are canonicalized so `..` segments and symlinks cannot smuggle a
/// path out of an allowed root while still passing a textual prefix test.
fn permitted_workspace(requested: &Path, roots: &[String]) -> Option<PathBuf> {
    if roots.is_empty() {
        return None;
    }
    let canonical = requested.canonicalize().ok()?;
    if !canonical.is_dir() {
        return None;
    }
    roots
        .iter()
        .filter_map(|root| PathBuf::from(root).canonicalize().ok())
        .any(|root| canonical.starts_with(&root))
        .then_some(canonical)
}

fn system_prompt_text(request: &Value) -> Option<String> {
    let system = request.get("system")?;
    if let Some(text) = system.as_str() {
        return Some(text.to_string());
    }
    let blocks = system.as_array()?;
    let joined = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// Find a `Working directory: <path>` line in the system prompt.
///
/// Anchored to the start of a line (after leading whitespace and any bullet
/// marker) rather than matched anywhere in the text: an unanchored search hits
/// the phrase inside quoted prose, which widens the injection surface for no
/// benefit. Harnesses state the value on its own line.
fn parse_working_directory(system: &str) -> Option<PathBuf> {
    const NEEDLE: &str = "working directory:";
    system.lines().find_map(|line| {
        let trimmed = line.trim_start().trim_start_matches(['-', '*', '#', ' ']);
        let lowered = trimmed.to_ascii_lowercase();
        // Allow a short qualifier such as "Primary working directory:".
        let start = lowered.find(NEEDLE).filter(|index| *index <= 16)? + NEEDLE.len();
        let value = trimmed[start..].trim().trim_matches('`');
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

pub fn extract_antigravity_prompt(request: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(sys) = request.get("system") {
        if let Some(s) = sys.as_str() {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        } else if let Some(arr) = sys.as_array() {
            for b in arr {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }

    if let Some(msgs) = request.get("messages").and_then(Value::as_array) {
        for msg in msgs {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            if let Some(content) = msg.get("content") {
                if let Some(t) = content.as_str() {
                    parts.push(format!("{role}: {t}"));
                } else if let Some(arr) = content.as_array() {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    parts.push(format!("{role}: {t}"));
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                                parts.push(format!("{role} tool_use {name}: {input}"));
                            }
                            Some("tool_result") => {
                                let content = b
                                    .get("content")
                                    .map(ToString::to_string)
                                    .unwrap_or_default();
                                parts.push(format!("{role} tool_result: {content}"));
                            }
                            Some("image") => parts.push(format!("{role}: [image omitted]")),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    parts.join("\n\n")
}

pub fn find_agy_binary() -> Option<PathBuf> {
    static CACHE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHE.get_or_init(find_agy_binary_uncached).clone()
}

fn find_agy_binary_uncached() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("AGY_BIN") {
        let p = PathBuf::from(env_path);
        if p.exists() {
            return Some(p);
        }
    }

    if let Some(home) = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
        })
    {
        let p = home.join(".gemini/antigravity-cli/bin/agy");
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let p = dir.join("agy");
            if p.is_file() {
                return Some(p);
            }
        }
    }

    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `kill(-0, ...)` is `kill(0, ...)`: every process in shunt's own group.
    #[test]
    fn kill_group_refuses_process_group_zero() {
        // Reaching the syscall would SIGKILL this test process and its group,
        // so surviving the call *is* the assertion.
        child::kill_group(0);
    }

    /// The retained buffer, not the response, is what the bound protects.
    ///
    /// `stderr_text` truncates on the way out, so an end-to-end assertion on the
    /// response body passes whether or not the drain itself is bounded — it
    /// cannot distinguish the two. This reads the buffer directly.
    #[tokio::test]
    async fn drain_stderr_bounds_newline_free_output() {
        // 75 bytes, repeated 1000 times, with no newline anywhere: the shape
        // that made a line-oriented drain accumulate the whole run before any
        // size check could see it. POSIX sh only — no `seq`, no bashisms.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(
                "s=PADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPADPAD; \
                 i=0; while [ $i -lt 1000 ]; do printf '%s' \"$s\" >&2; i=$((i+1)); done",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stderr producer");

        let mut drain = drain_stderr(&mut child);
        child.wait().await.expect("producer exits");
        drain.settle().await;

        let retained = drain
            .log
            .lock()
            .expect("stderr buffer is not poisoned")
            .len();
        assert!(
            retained <= STDERR_LIMIT,
            "75000 newline-free bytes must not be retained whole: kept {retained}"
        );
        assert!(
            drain.text().contains("PAD"),
            "the bounded prefix must still carry the diagnostic"
        );
    }
}
