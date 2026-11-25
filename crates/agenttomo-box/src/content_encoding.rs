//! HTTP entity encodings understood by the capture readers.
use std::io::{self, Read};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    Identity,
    Gzip,
    Deflate,
    Zstd,
}

pub fn header(headers: &axum::http::HeaderMap) -> String {
    headers
        .get_all("content-encoding")
        .iter()
        .map(|v| v.to_str().unwrap_or("invalid"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl ContentEncoding {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(Self::Identity),
            "gzip" | "x-gzip" => Some(Self::Gzip),
            "deflate" => Some(Self::Deflate),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    pub fn read(self, bytes: &[u8], limit: usize, output: &mut Vec<u8>) -> io::Result<usize> {
        let reader: Box<dyn Read + '_> = match self {
            Self::Identity => Box::new(bytes),
            Self::Gzip => Box::new(flate2::read::MultiGzDecoder::new(bytes)),
            Self::Deflate => Box::new(flate2::read::ZlibDecoder::new(bytes)),
            Self::Zstd => {
                let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
                decoder.window_log_max(
                    limit
                        .saturating_sub(1)
                        .max(1 << 20)
                        .next_power_of_two()
                        .ilog2()
                        .min(26),
                )?;
                Box::new(decoder)
            }
        };
        reader.take(limit as u64).read_to_end(output)
    }
}
