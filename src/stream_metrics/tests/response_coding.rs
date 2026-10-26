use std::io::Write;

use super::*;

#[tokio::test]
async fn normalized_compressed_stream_matches_identity_usage_and_outcome() {
    let body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let mut identity = state(Protocol::Anthropic);
    identity.observe_chunk(body.as_bytes());
    assert_eq!(identity.outcome(true), Outcome::Completed);
    for coding in ["gzip", "deflate", "zstd"] {
        let bytes = match coding {
            "gzip" => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body.as_bytes()).unwrap();
                encoder.finish().unwrap()
            }
            "deflate" => {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(body.as_bytes()).unwrap();
                encoder.finish().unwrap()
            }
            "zstd" => zstd::stream::encode_all(body.as_bytes(), 3).unwrap(),
            _ => unreachable!(),
        };
        let upstream = Response::builder()
            .header("content-encoding", coding)
            .body(reqwest::Body::wrap_stream(stream::iter(
                bytes
                    .into_iter()
                    .map(|byte| Ok::<_, Infallible>(Bytes::from(vec![byte]))),
            )))
            .unwrap();
        let normalized = crate::response_coding::normalize(upstream.into());
        let mut stream = normalized.bytes_stream();
        let mut observer = state(Protocol::Anthropic);
        while let Some(chunk) = stream.next().await {
            observer.observe_chunk(&chunk.unwrap());
        }
        assert_eq!(observer.outcome(true), identity.outcome(true), "{coding}");
        assert_eq!(observer.tokens.input, identity.tokens.input);
        assert_eq!(observer.tokens.output, identity.tokens.output);
        assert_eq!(observer.tokens.cache_read, identity.tokens.cache_read);
        assert_eq!(
            observer.tokens.cache_creation,
            identity.tokens.cache_creation
        );
        assert_eq!(observer.sse_events, identity.sse_events);
        assert_eq!(observer.bytes_forwarded, identity.bytes_forwarded);
    }
}
