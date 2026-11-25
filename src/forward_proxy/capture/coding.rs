//! HTTP content decoding for the capture copy.

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use tokio::io::{AsyncRead, BufReader};

type Reader = Box<dyn AsyncRead + Send + Unpin>;

/// Bound the number of decoder buffers and compression windows per body.
const MAX_LAYERS: usize = 4;

pub(super) enum Coding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl Coding {
    pub(super) fn parse(header: &str) -> Option<Vec<Self>> {
        let layers = header
            .rsplit(',')
            .map(|name| match name.trim().to_ascii_lowercase().as_str() {
                "gzip" | "x-gzip" => Some(Self::Gzip),
                "deflate" => Some(Self::Deflate),
                "br" => Some(Self::Brotli),
                "zstd" => Some(Self::Zstd),
                _ => None,
            })
            .take(MAX_LAYERS + 1)
            .collect::<Option<Vec<_>>>()?;
        (layers.len() <= MAX_LAYERS).then_some(layers)
    }

    /// Wrap in reverse header order, so the outermost applied coding is read first.
    pub(super) fn reader(layers: Vec<Self>, input: impl AsyncRead + Send + Unpin + 'static) -> Reader {
        layers.into_iter().fold(Box::new(input) as Reader, |input, layer| {
            let input = BufReader::new(input);
            match layer {
                Self::Gzip => {
                    let mut reader = GzipDecoder::new(input);
                    reader.multiple_members(true);
                    Box::new(reader)
                }
                Self::Deflate => {
                    let mut reader = ZlibDecoder::new(input);
                    reader.multiple_members(true);
                    Box::new(reader)
                }
                Self::Brotli => {
                    let mut reader = BrotliDecoder::new(input);
                    reader.multiple_members(true);
                    Box::new(reader)
                }
                Self::Zstd => {
                    let mut reader = ZstdDecoder::new_for_http(input);
                    reader.multiple_members(true);
                    Box::new(reader)
                }
            }
        })
    }
}
