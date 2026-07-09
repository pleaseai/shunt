//! SSE keepalive pings (M5): keep a byte flowing while the upstream is quiet
//! so middlebox idle timers (e.g. Cloudflare's 100s → 524) never expire.
//! Injected pings are the Anthropic protocol's own `ping` event, which clients
//! ignore. See `docs/m5-sse-keepalive.md`.

use std::time::Duration;

use axum::body::Bytes;
use futures_util::{stream, Stream, StreamExt};

const PING_EVENT: &str = "event: ping\ndata: {\"type\": \"ping\"}\n\n";

/// Tracks whether the bytes forwarded so far end at an SSE event boundary
/// (`\n\n` or `\r\n\r\n`), across arbitrary chunk splits. Injecting anywhere
/// else would corrupt a partially-sent upstream event.
#[derive(Debug, Clone, Copy)]
struct BoundaryTracker {
    /// Last (up to) four forwarded bytes, oldest first.
    tail: [u8; 4],
}

impl BoundaryTracker {
    /// Start of stream counts as a boundary: nothing is half-sent yet.
    fn new() -> Self {
        Self { tail: *b"\n\n\n\n" }
    }

    fn push(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            self.tail.rotate_left(1);
            self.tail[3] = byte;
        }
    }

    fn at_boundary(&self) -> bool {
        self.tail.ends_with(b"\n\n") || self.tail == *b"\r\n\r\n"
    }
}

/// Wrap an outgoing SSE byte stream: forward upstream items unchanged, and
/// while the upstream is idle for `interval` at an event boundary, emit a
/// `ping` event and keep waiting. `interval` of zero disables injection.
pub fn with_pings<S, E>(
    upstream: S,
    interval: Duration,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    let enabled = !interval.is_zero();
    let state = (Box::pin(upstream), BoundaryTracker::new(), false);
    stream::unfold(state, move |(mut upstream, mut boundary, done)| {
        Box::pin(async move {
            if done {
                return None;
            }
            loop {
                if !enabled {
                    return match upstream.next().await {
                        Some(Ok(chunk)) => Some((Ok(chunk), (upstream, boundary, false))),
                        Some(Err(error)) => Some((Err(error), (upstream, boundary, true))),
                        None => None,
                    };
                }
                match tokio::time::timeout(interval, upstream.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        boundary.push(&chunk);
                        return Some((Ok(chunk), (upstream, boundary, false)));
                    }
                    Ok(Some(Err(error))) => return Some((Err(error), (upstream, boundary, true))),
                    Ok(None) => return None,
                    Err(_idle) => {
                        // Only inject between complete events; a stalled
                        // half-sent frame must stay untouched.
                        if boundary.at_boundary() {
                            return Some((
                                Ok(Bytes::from_static(PING_EVENT.as_bytes())),
                                (upstream, boundary, false),
                            ));
                        }
                    }
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;
    use futures_util::{stream, StreamExt};

    use super::{with_pings, BoundaryTracker, PING_EVENT};

    type Item = Result<Bytes, std::convert::Infallible>;

    fn chunk(text: &'static str) -> Item {
        Ok(Bytes::from_static(text.as_bytes()))
    }

    #[test]
    fn boundary_tracking_handles_chunk_splits() {
        let mut tracker = BoundaryTracker::new();
        assert!(tracker.at_boundary(), "start of stream is a boundary");

        tracker.push(b"event: message_start\ndata: {}\n");
        assert!(!tracker.at_boundary(), "mid-event is not a boundary");

        tracker.push(b"\n");
        assert!(tracker.at_boundary(), "\\n\\n split across chunks");

        tracker.push(b"data: {}\r\n");
        assert!(!tracker.at_boundary());
        tracker.push(b"\r\n");
        assert!(tracker.at_boundary(), "\\r\\n\\r\\n counts as a boundary");
    }

    #[tokio::test(start_paused = true)]
    async fn active_stream_passes_through_without_pings() {
        let upstream = stream::iter(vec![
            chunk("event: a\ndata: {}\n\n"),
            chunk("event: b\ndata: {}\n\n"),
        ]);
        let collected: Vec<_> = with_pings(upstream, Duration::from_secs(30))
            .collect()
            .await;
        let text = collected
            .into_iter()
            .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
            .collect::<String>();
        assert_eq!(text, "event: a\ndata: {}\n\nevent: b\ndata: {}\n\n");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_stream_at_boundary_emits_pings_until_upstream_resumes() {
        let upstream =
            stream::iter(vec![chunk("event: a\ndata: {}\n\n")]).chain(stream::pending::<Item>());
        let mut wrapped = Box::pin(with_pings(upstream, Duration::from_secs(30)));

        let first = wrapped.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"event: a\ndata: {}\n\n");
        // Paused clock: next() blocks on the timeout; tokio auto-advances.
        let ping = wrapped.next().await.unwrap().unwrap();
        assert_eq!(&ping[..], PING_EVENT.as_bytes());
        let ping = wrapped.next().await.unwrap().unwrap();
        assert_eq!(&ping[..], PING_EVENT.as_bytes(), "pings repeat while idle");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_mid_event_suppresses_pings() {
        let upstream = stream::iter(vec![chunk("event: a\ndata: {\"partial\":")])
            .chain(stream::pending::<Item>());
        let mut wrapped = Box::pin(with_pings(upstream, Duration::from_millis(10)));

        let first = wrapped.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"event: a\ndata: {\"partial\":");
        // No ping may ever be injected mid-frame: give the wrapper many idle
        // intervals and assert nothing arrives.
        let next = tokio::time::timeout(Duration::from_secs(60), wrapped.next()).await;
        assert!(next.is_err(), "no bytes while stalled mid-event");
    }

    #[tokio::test(start_paused = true)]
    async fn upstream_end_terminates_without_trailing_pings() {
        let upstream = stream::iter(vec![chunk("event: a\ndata: {}\n\n")]);
        let collected: Vec<_> = with_pings(upstream, Duration::from_secs(30))
            .collect()
            .await;
        assert_eq!(collected.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_interval_disables_injection() {
        let upstream =
            stream::iter(vec![chunk("event: a\ndata: {}\n\n")]).chain(stream::pending::<Item>());
        let mut wrapped = Box::pin(with_pings(upstream, Duration::ZERO));

        let first = wrapped.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"event: a\ndata: {}\n\n");
        let next = tokio::time::timeout(Duration::from_secs(600), wrapped.next()).await;
        assert!(next.is_err(), "disabled wrapper never injects");
    }
}
