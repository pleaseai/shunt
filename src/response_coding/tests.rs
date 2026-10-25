#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{convert::Infallible, time::Duration};

use futures_util::TryStreamExt;

use super::*;

mod gzip;

fn compressed(coding: UpstreamCoding, bytes: &[u8]) -> Bytes {
    let mut encoder = Encoder::new(coding).unwrap();
    let mut out = encoder.push(bytes).unwrap().to_vec();
    out.extend_from_slice(&encoder.finish().unwrap());
    out.into()
}

async fn collected(stream: impl Stream<Item = io::Result<Bytes>>) -> io::Result<Vec<u8>> {
    stream
        .try_fold(Vec::new(), |mut out, bytes| async move {
            assert!(bytes.len() <= PIECE_BYTES);
            out.extend_from_slice(&bytes);
            Ok(out)
        })
        .await
}

#[tokio::test]
async fn arbitrary_byte_splits_round_trip_each_coding() {
    let expected = b"event: message_start\ndata: {}\n\nevent: message_stop\ndata: {}\n\n";
    for coding in [
        UpstreamCoding::Gzip,
        UpstreamCoding::Deflate,
        UpstreamCoding::Zstd,
    ] {
        let bytes = compressed(coding, expected);
        for split in 0..=bytes.len() {
            let input = stream::iter([
                Ok::<_, Infallible>(bytes.slice(..split)),
                Ok(bytes.slice(split..)),
            ]);
            assert_eq!(collected(decode(input, coding)).await.unwrap(), expected);
        }
    }
}

#[tokio::test]
async fn encoder_and_decoder_deliver_complete_events_before_eof() {
    for coding in [
        UpstreamCoding::Gzip,
        UpstreamCoding::Deflate,
        UpstreamCoding::Zstd,
    ] {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let event = Bytes::from_static(b"event: message_start\ndata: {}\n\n");
        let input = stream::iter([Ok::<_, Infallible>(event.clone())]).chain(stream::once(async {
            let _ = released.await;
            Ok(Bytes::from_static(b"event: message_stop\ndata: {}\n\n"))
        }));
        let output = decode(encode(input, coding), coding);
        futures_util::pin_mut!(output);
        let first = tokio::time::timeout(Duration::from_secs(3), output.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            first, event,
            "a coding header alone is not a complete event"
        );
        release.send(()).unwrap();
        assert_eq!(
            collected(output).await.unwrap(),
            b"event: message_stop\ndata: {}\n\n"
        );
    }
}

#[tokio::test]
async fn byte_split_flushed_prefix_is_decoded_before_the_tail() {
    for coding in [
        UpstreamCoding::Gzip,
        UpstreamCoding::Deflate,
        UpstreamCoding::Zstd,
    ] {
        let mut encoder = Encoder::new(coding).unwrap();
        let prefix = encoder.push(b"event: message_start\ndata: {}\n\n").unwrap();
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let input = stream::iter(
            prefix
                .iter()
                .map(|byte| Ok::<_, Infallible>(Bytes::from(vec![*byte])))
                .collect::<Vec<_>>(),
        )
        .chain(stream::once(async move {
            let _ = released.await;
            Ok(encoder.finish().unwrap())
        }));
        let output = decode(input, coding);
        futures_util::pin_mut!(output);
        let received = tokio::time::timeout(Duration::from_secs(3), async {
            let mut out = Vec::new();
            while !out.ends_with(b"\n\n") {
                out.extend_from_slice(&output.next().await.unwrap().unwrap());
            }
            out
        })
        .await
        .unwrap_or_else(|_| panic!("prefix stalled: {coding:?}"));
        assert_eq!(received, b"event: message_start\ndata: {}\n\n");
        release.send(()).unwrap();
        collected(output).await.unwrap();
    }
}

#[tokio::test]
async fn gzip_drains_a_large_prefix_before_requesting_more_input() {
    let expected = format!(
        "event: large\ndata: {}\n\nevent: small\ndata: {{}}\n\n",
        "x".repeat(48 * 1024)
    );
    let mut encoder = Encoder::new(UpstreamCoding::Gzip).unwrap();
    let prefix = encoder.push(expected.as_bytes()).unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let input = stream::iter([Ok::<_, Infallible>(prefix)]).chain(stream::once(async move {
        let _ = released.await;
        Ok(encoder.finish().unwrap())
    }));
    let output = decode(input, UpstreamCoding::Gzip);
    futures_util::pin_mut!(output);
    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let mut received = Vec::new();
        while received.len() < expected.len() {
            let chunk = output.next().await.unwrap().unwrap();
            assert!(chunk.len() <= PIECE_BYTES);
            received.extend_from_slice(&chunk);
        }
        received
    })
    .await
    .expect("pending gzip output must not wait for the trailer");
    assert_eq!(received, expected.as_bytes());
    release.send(()).unwrap();
    collected(output).await.unwrap();
}

#[tokio::test]
async fn expansion_emits_bounded_pieces() {
    let expected = vec![b'x'; 4 * 1024 * 1024];
    for coding in [
        UpstreamCoding::Gzip,
        UpstreamCoding::Deflate,
        UpstreamCoding::Zstd,
    ] {
        let bytes = compressed(coding, &expected);
        assert!(bytes.len() < PIECE_BYTES);
        let input = stream::once(async { Ok::<_, Infallible>(bytes) });
        assert_eq!(collected(decode(input, coding)).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn invalid_and_truncated_bodies_error_without_panicking() {
    for coding in [
        UpstreamCoding::Gzip,
        UpstreamCoding::Deflate,
        UpstreamCoding::Zstd,
    ] {
        let bytes = compressed(coding, b"event: message_start\ndata: {}\n\n");
        for bytes in [
            Bytes::from_static(b"invalid compressed data"),
            bytes.slice(..bytes.len() - 1),
            Bytes::new(),
        ] {
            let input = stream::once(async { Ok::<_, Infallible>(bytes) });
            assert!(
                collected(decode(input, coding)).await.is_err(),
                "{coding:?}"
            );
        }
    }
}

#[tokio::test]
async fn zstd_frames_of_unknown_size_needing_more_than_an_8_mib_window_fail() {
    use std::io::Write as _;
    let frame = |window_log: u32| {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder
            .set_parameter(zstd::stream::raw::CParameter::WindowLog(window_log))
            .unwrap();
        encoder
            .write_all(b"event: message_start\ndata: {}\n\n")
            .unwrap();
        Bytes::from(encoder.finish().unwrap())
    };
    for (window_log, decodes) in [(23, true), (24, false)] {
        let bytes = frame(window_log);
        let input = stream::once(async { Ok::<_, Infallible>(bytes) });
        assert_eq!(
            collected(decode(input, UpstreamCoding::Zstd)).await.is_ok(),
            decodes,
            "window log {window_log}"
        );
    }
}

#[tokio::test]
async fn upstream_body_errors_propagate() {
    let input = stream::iter([Err::<Bytes, _>(io::Error::other("upstream body failed"))]);
    let error = collected(decode(input, UpstreamCoding::Gzip))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("upstream body failed"));
}

#[test]
fn decoder_driver_coalesces_consecutive_again_transitions() {
    for ready in [
        Step::Input,
        Step::Done,
        Step::Output(Bytes::from_static(b"ready")),
    ] {
        let mut steps = vec![Step::Again, Step::Again, Step::Again, ready].into_iter();
        let mut calls = 0;
        let result = codec::drain_transitions(|| {
            calls += 1;
            Ok(steps.next().expect("driver must stop at the ready step"))
        })
        .unwrap();
        assert!(!matches!(result, Step::Again));
        assert_eq!(calls, 4);
    }
    let mut calls = 0;
    let result = codec::drain_transitions(|| {
        calls += 1;
        if calls < 4 {
            Ok(Step::Again)
        } else {
            Err(io::Error::other("codec failed"))
        }
    });
    assert_eq!(result.err().unwrap().to_string(), "codec failed");
    assert_eq!(calls, 4);
}

#[tokio::test]
async fn decoding_errors_end_the_stream_without_repolling_upstream() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    for transport_error in [true, false] {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let input = stream::poll_fn(move |_| {
            assert_eq!(observed.fetch_add(1, Ordering::SeqCst), 0);
            std::task::Poll::Ready(Some(if transport_error {
                Err(io::Error::other("transport failed"))
            } else {
                Ok(Bytes::from_static(b"invalid gzip data"))
            }))
        });
        let output = decode(input, UpstreamCoding::Gzip);
        futures_util::pin_mut!(output);
        let error = output.next().await.unwrap().unwrap_err();
        if transport_error {
            assert!(error.to_string().contains("transport failed"));
        }
        assert!(output.next().await.is_none());
        assert!(output.next().await.is_none());
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn unsupported_stacked_and_repeated_codings_are_passthrough() {
    for values in [
        vec!["br"],
        vec!["gzip, br"],
        vec!["gzip", "gzip"],
        vec!["identity"],
    ] {
        let mut response = Response::new(reqwest::Body::from("opaque bytes"));
        for value in values {
            response
                .headers_mut()
                .append(header::CONTENT_ENCODING, HeaderValue::from_static(value));
        }
        let response = normalize(response.into());
        assert!(response.extensions().get::<UpstreamCoding>().is_none());
        assert!(response.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(response.bytes().await.unwrap(), "opaque bytes");
    }
}

#[test]
fn accept_encoding_keeps_order_and_quality_values() {
    let mut headers = HeaderMap::new();
    headers.append(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("br, zstd;q=0.7, GZIP;q=1, mystery"),
    );
    headers.append(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("deflate;q=0"),
    );
    filter_accept_encoding(&mut headers);
    assert_eq!(
        headers[header::ACCEPT_ENCODING],
        "zstd;q=0.7, GZIP;q=1, deflate;q=0"
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity;q=0, gzip;q=0.5, br"),
    );
    filter_accept_encoding(&mut headers);
    assert_eq!(headers[header::ACCEPT_ENCODING], "identity;q=0, gzip;q=0.5");
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip;q=0, zstd;q=0.7, deflate;q=0, GZIP;q=1"),
    );
    filter_accept_encoding(&mut headers);
    assert_eq!(
        headers[header::ACCEPT_ENCODING],
        "gzip;q=0, zstd;q=0.7, deflate;q=0, GZIP;q=1"
    );
    // Without a supported coding the upstream is asked for identity, even when
    // the client excludes it.
    for value in [
        "identity",
        "br",
        "unknown, br",
        "*",
        "gzip;q=0",
        "br, identity;q=0",
        "br, *;q=0",
        "br, gzip;q=0, identity;q=0",
        "br, gzip;q=0, *;q=0",
        "identity;q=0",
        "*;q=0",
    ] {
        headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
        filter_accept_encoding(&mut headers);
        assert_eq!(headers[header::ACCEPT_ENCODING], "identity", "{value}");
    }
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, br, *;q=0"),
    );
    filter_accept_encoding(&mut headers);
    assert_eq!(headers[header::ACCEPT_ENCODING], "gzip, *;q=0");
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("br, *;q=0, identity;q=1"),
    );
    filter_accept_encoding(&mut headers);
    assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
    headers.remove(header::ACCEPT_ENCODING);
    filter_accept_encoding(&mut headers);
    assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
}

#[tokio::test]
async fn bodyless_statuses_stay_unmarked_but_empty_success_is_truncated() {
    for status in [100, 103, 204, 205, 304] {
        let response = Response::builder()
            .status(status)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(reqwest::Body::from(Vec::new()))
            .unwrap();
        let response = normalize(response.into());
        assert!(response.extensions().get::<UpstreamCoding>().is_none());
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    let response = Response::builder()
        .status(200)
        .header(header::CONTENT_ENCODING, "gzip")
        .body(reqwest::Body::from(Vec::new()))
        .unwrap();
    assert!(normalize(response.into()).bytes().await.is_err());
}

#[tokio::test]
async fn coding_headers_and_response_metadata_survive_the_pipeline() {
    let bytes = compressed(UpstreamCoding::Gzip, b"plain JSON");
    let upstream = Response::builder()
        .status(429)
        .header(header::CONTENT_ENCODING, "GZip")
        .header(header::CONTENT_LENGTH, bytes.len())
        .header(header::RETRY_AFTER, "17")
        .header("request-id", "req_test")
        .header(header::VARY, "Origin")
        .body(reqwest::Body::from(bytes))
        .unwrap();
    let upstream = normalize(upstream.into());
    assert_eq!(upstream.status(), 429);
    assert!(!upstream.headers().contains_key(header::CONTENT_LENGTH));
    assert!(!upstream.headers().contains_key(header::CONTENT_ENCODING));
    let upstream: Response<reqwest::Body> = upstream.into();
    let (parts, body) = upstream.into_parts();
    let input = reqwest::Response::from(Response::new(body)).bytes_stream();
    let response = encode_response(Response::from_parts(parts, Body::from_stream(input)));
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(response.headers()[header::RETRY_AFTER], "17");
    assert_eq!(response.headers()["request-id"], "req_test");
    let vary: Vec<_> = response
        .headers()
        .get_all(header::VARY)
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(vary, ["Origin", "Accept-Encoding"]);
    let decoded = collected(decode(
        response.into_body().into_data_stream(),
        UpstreamCoding::Gzip,
    ))
    .await
    .unwrap();
    assert_eq!(decoded, b"plain JSON");
}

#[test]
fn vary_is_not_duplicated_and_unmarked_responses_are_unchanged() {
    for vary in ["Origin, accept-ENCODING", "*"] {
        let mut response = Response::builder()
            .header(header::VARY, vary)
            .body(Body::empty())
            .unwrap();
        response.extensions_mut().insert(UpstreamCoding::Gzip);
        assert_eq!(
            encode_response(response)
                .headers()
                .get_all(header::VARY)
                .iter()
                .count(),
            1
        );
    }
    let response = Response::builder()
        .header(header::CONTENT_ENCODING, "br")
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .unwrap();
    let response = encode_response(response);
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
    assert!(!response.headers().contains_key(header::VARY));
}
