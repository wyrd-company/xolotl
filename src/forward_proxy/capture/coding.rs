//! HTTP content decoding for the capture copy.

use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
use tokio::io::{AsyncBufRead, AsyncRead};

type Reader = Box<dyn AsyncRead + Send + Unpin>;

#[derive(Debug)]
pub(super) enum Coding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl Coding {
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Some(Self::Gzip),
            "deflate" => Some(Self::Deflate),
            "br" => Some(Self::Brotli),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    pub(super) fn reader(self, input: impl AsyncBufRead + Send + Unpin + 'static) -> Reader {
        match self {
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
    }
}
