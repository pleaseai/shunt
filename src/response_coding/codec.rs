use std::io::{self, Write};

use bytes::{Buf, Bytes};
use zstd::stream::raw::Operation;

use super::UpstreamCoding;

pub(super) const PIECE_BYTES: usize = 16 * 1024;

/// RFC 9659 §3: an encoder of the "zstd" content coding MUST NOT require a
/// window above 8 MB. libzstd's streaming decoder does not allocate a window
/// buffer above this limit; its own default limit is 128 MiB, which would let
/// one upstream reply hold that much history.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

pub(super) enum Step {
    Output(Bytes),
    Input,
    Again,
    Done,
}

pub(super) fn drain_transitions(mut step: impl FnMut() -> io::Result<Step>) -> io::Result<Step> {
    loop {
        match step()? {
            Step::Again => continue,
            ready => return Ok(ready),
        }
    }
}

#[derive(Default)]
struct Input {
    bytes: Bytes,
    eof: bool,
}

enum DecodeKind {
    Gzip(flate2::Decompress, Input, bool),
    Deflate(flate2::Decompress, Input, bool),
    Zstd(zstd::stream::raw::Decoder<'static>, Input, bool),
}

pub(super) struct Decoder(DecodeKind);

impl Decoder {
    pub(super) fn new(coding: UpstreamCoding) -> io::Result<Self> {
        Ok(Self(match coding {
            UpstreamCoding::Gzip => {
                DecodeKind::Gzip(flate2::Decompress::new_gzip(15), Input::default(), false)
            }
            UpstreamCoding::Deflate => {
                DecodeKind::Deflate(flate2::Decompress::new(true), Input::default(), false)
            }
            UpstreamCoding::Zstd => {
                let mut decoder = zstd::stream::raw::Decoder::new()?;
                decoder.set_parameter(zstd::stream::raw::DParameter::WindowLogMax(
                    ZSTD_WINDOW_LOG_MAX,
                ))?;
                DecodeKind::Zstd(decoder, Input::default(), false)
            }
        }))
    }

    pub(super) fn feed(&mut self, bytes: Bytes, eof: bool) {
        let input = match &mut self.0 {
            DecodeKind::Gzip(_, input, _)
            | DecodeKind::Deflate(_, input, _)
            | DecodeKind::Zstd(_, input, _) => input,
        };
        input.bytes = bytes;
        input.eof = eof;
    }

    pub(super) fn step(&mut self) -> io::Result<Step> {
        let mut output = [0u8; PIECE_BYTES];
        let is_gzip = matches!(&self.0, DecodeKind::Gzip(..));
        let (read, written, finished, input) = match &mut self.0 {
            DecodeKind::Gzip(decoder, input, finished)
            | DecodeKind::Deflate(decoder, input, finished) => {
                if *finished {
                    if input.bytes.is_empty() {
                        return Ok(if input.eof { Step::Done } else { Step::Input });
                    }
                    if is_gzip {
                        *decoder = flate2::Decompress::new_gzip(15);
                        *finished = false;
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "trailing deflate data",
                        ));
                    }
                }
                let before_in = decoder.total_in();
                let before_out = decoder.total_out();
                let status = decoder
                    .decompress(&input.bytes, &mut output, flate2::FlushDecompress::None)
                    .map_err(io::Error::other)?;
                *finished = status == flate2::Status::StreamEnd;
                (
                    (decoder.total_in() - before_in) as usize,
                    (decoder.total_out() - before_out) as usize,
                    *finished,
                    input,
                )
            }
            DecodeKind::Zstd(decoder, input, finished) => {
                if *finished && input.bytes.is_empty() {
                    return Ok(if input.eof { Step::Done } else { Step::Input });
                }
                let status = decoder.run_on_buffers(&input.bytes, &mut output)?;
                *finished = status.remaining == 0;
                (status.bytes_read, status.bytes_written, *finished, input)
            }
        };
        input.bytes.advance(read);
        if written > 0 {
            return Ok(Step::Output(Bytes::copy_from_slice(&output[..written])));
        }
        if read > 0 || finished {
            return Ok(Step::Again);
        }
        if input.eof {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated compressed response",
            ));
        }
        if !input.bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response decoder made no progress",
            ));
        }
        Ok(Step::Input)
    }
}

pub(super) enum Encoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Deflate(flate2::write::ZlibEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl Encoder {
    pub(super) fn new(coding: UpstreamCoding) -> io::Result<Self> {
        Ok(match coding {
            UpstreamCoding::Gzip => Self::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            )),
            UpstreamCoding::Deflate => Self::Deflate(flate2::write::ZlibEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            )),
            UpstreamCoding::Zstd => Self::Zstd(zstd::stream::write::Encoder::new(
                Vec::new(),
                zstd::DEFAULT_COMPRESSION_LEVEL,
            )?),
        })
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> io::Result<Bytes> {
        match self {
            Self::Gzip(encoder) => {
                encoder.write_all(bytes)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()).into())
            }
            Self::Deflate(encoder) => {
                encoder.write_all(bytes)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()).into())
            }
            Self::Zstd(encoder) => {
                encoder.write_all(bytes)?;
                encoder.flush()?;
                Ok(std::mem::take(encoder.get_mut()).into())
            }
        }
    }

    pub(super) fn finish(self) -> io::Result<Bytes> {
        match self {
            Self::Gzip(encoder) => encoder.finish().map(Bytes::from),
            Self::Deflate(encoder) => encoder.finish().map(Bytes::from),
            Self::Zstd(encoder) => encoder.finish().map(Bytes::from),
        }
    }
}
