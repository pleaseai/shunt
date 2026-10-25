//! Native Anthropic response decoding before plaintext processing, and encoding
//! at the client-facing Messages boundary.

use std::{io, sync::OnceLock};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response},
};
use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};

mod codec;
use codec::{Decoder, Encoder, Step, PIECE_BYTES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpstreamCoding {
    Gzip,
    Deflate,
    Zstd,
}

impl UpstreamCoding {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gzip" => Some(Self::Gzip),
            "deflate" => Some(Self::Deflate),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Zstd => "zstd",
        }
    }
}

/// Keep supported offers and identity exclusions in client order. Without a
/// supported coding, ask for identity even when the client excludes it: a reply
/// in a coding shunt cannot decode would reach the plaintext layers, the spend
/// meter among them, still encoded.
pub(crate) fn filter_accept_encoding(headers: &mut HeaderMap) {
    let mut has_coding = false;
    let mut offers = Vec::new();
    for offer in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let offer = offer.trim();
        let mut parts = offer.split(';');
        let token = parts.next().unwrap_or_default().trim();
        let quality = parts
            .filter_map(|part| part.trim().split_once('='))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
            .and_then(|(_, value)| value.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        let supported = UpstreamCoding::parse(token).is_some();
        let is_identity = token.eq_ignore_ascii_case("identity");
        let wildcard_exclusion = token == "*" && quality == 0.0;
        has_coding |= supported && quality > 0.0;
        if supported || is_identity || wildcard_exclusion {
            offers.push(offer.to_owned());
        }
    }
    let value = if has_coding {
        HeaderValue::from_str(&offers.join(", ")).expect("filtered header entries remain valid")
    } else {
        HeaderValue::from_static("identity")
    };
    headers.insert(header::ACCEPT_ENCODING, value);
}

fn decode_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(crate::offload::cpu_sized_semaphore)
}

fn encode_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SLOTS.get_or_init(crate::offload::cpu_sized_semaphore)
}

/// Leave unsupported and stacked codings on the existing passthrough path.
pub(crate) fn normalize(upstream: reqwest::Response) -> reqwest::Response {
    if bodyless(upstream.status()) {
        return upstream;
    }
    let mut values = upstream.headers().get_all(header::CONTENT_ENCODING).iter();
    let coding = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(UpstreamCoding::parse);
    let Some(coding) = coding.filter(|_| values.next().is_none()) else {
        return upstream;
    };
    let response: Response<reqwest::Body> = upstream.into();
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.extensions.insert(coding);
    // Reuse reqwest's body stream without changing the adapter's error type.
    let stream = reqwest::Response::from(Response::new(body)).bytes_stream();
    Response::from_parts(parts, reqwest::Body::wrap_stream(decode(stream, coding))).into()
}

fn bodyless(status: axum::http::StatusCode) -> bool {
    status.is_informational() || matches!(status.as_u16(), 204 | 205 | 304)
}

pub(crate) fn encode_response(response: axum::response::Response) -> axum::response::Response {
    if bodyless(response.status()) {
        return response;
    }
    let Some(coding) = response.extensions().get::<UpstreamCoding>().copied() else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    parts.extensions.remove::<UpstreamCoding>();
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(coding.name()),
    );
    let varies = parts
        .headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == "*" || value.trim().eq_ignore_ascii_case("accept-encoding"));
    if !varies {
        parts
            .headers
            .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    Response::from_parts(
        parts,
        Body::from_stream(encode(body.into_data_stream(), coding)),
    )
}

pub(super) fn decode<S, E>(
    input: S,
    coding: UpstreamCoding,
) -> impl Stream<Item = io::Result<Bytes>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let input = Box::pin(input.map(|item| item.map_err(io::Error::other)));
    let state = (input, None::<Decoder>, Bytes::new(), false);
    stream::try_unfold(
        state,
        move |(mut input, mut decoder, mut pending, mut eof)| async move {
            loop {
                let (mut next, step) = crate::offload::spawn_bounded(decode_slots(), move || {
                    let mut decoder = match decoder {
                        Some(decoder) => decoder,
                        None => Decoder::new(coding)?,
                    };
                    let step = codec::drain_transitions(|| decoder.step())?;
                    Ok::<_, io::Error>((decoder, step))
                })
                .await??;
                match step {
                    Step::Output(bytes) => {
                        return Ok(Some((bytes, (input, Some(next), pending, eof))));
                    }
                    Step::Done => return Ok(None),
                    Step::Again => unreachable!("drain_transitions consumes Again steps"),
                    Step::Input => {
                        if pending.is_empty() && !eof {
                            match input.next().await {
                                Some(chunk) => pending = chunk?,
                                None => eof = true,
                            }
                        }
                        let len = pending.len().min(PIECE_BYTES);
                        next.feed(pending.split_to(len), eof);
                        decoder = Some(next);
                    }
                }
            }
        },
    )
}

fn encode<S, E>(input: S, coding: UpstreamCoding) -> impl Stream<Item = io::Result<Bytes>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    let input = Box::pin(input.map(|item| item.map_err(io::Error::other)));
    let state = (input, None::<Encoder>, Bytes::new(), false);
    stream::unfold(
        state,
        move |(mut input, encoder, mut pending, done)| async move {
            if done {
                return None;
            }
            let result = async {
                let mut encoder = match encoder {
                    Some(encoder) => encoder,
                    None => {
                        crate::offload::spawn_bounded(encode_slots(), move || Encoder::new(coding))
                            .await??
                    }
                };
                while pending.is_empty() {
                    match input.next().await {
                        Some(chunk) => pending = chunk?,
                        None => {
                            let bytes = crate::offload::spawn_bounded(encode_slots(), move || {
                                encoder.finish()
                            })
                            .await??;
                            return Ok((bytes, None, true));
                        }
                    }
                }
                let piece = pending.split_to(pending.len().min(PIECE_BYTES));
                let (encoder, bytes) = crate::offload::spawn_bounded(encode_slots(), move || {
                    // Bounded input keeps the encoder's output allocation bounded.
                    // Flush here, not at HTTP chunk boundaries, so SSE cannot wait for EOF.
                    let bytes = encoder.push(&piece);
                    (encoder, bytes)
                })
                .await?;
                Ok((bytes?, Some(encoder), false))
            }
            .await;
            match result {
                Ok((bytes, encoder, done)) => Some((Ok(bytes), (input, encoder, pending, done))),
                Err(error) => Some((Err(error), (input, None, pending, true))),
            }
        },
    )
}

#[cfg(test)]
mod tests;
