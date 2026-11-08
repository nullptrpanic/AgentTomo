//! Decode retained HTTP entity bytes only for inspection; never alter forwarding.

use flate2::read::{MultiGzDecoder, ZlibDecoder};
use std::io::{self, Read};

pub const MAX_DECODED_BYTES: usize = 1024 * 1024;

pub struct Decoded {
    pub bytes: Vec<u8>,
    pub status: &'static str,
}

pub fn decode(bytes: Vec<u8>, encoding: &str) -> Decoded {
    let mut output = Vec::new();
    let limit = MAX_DECODED_BYTES as u64 + 1;
    let result = match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => {
            return Decoded {
                bytes,
                status: "identity",
            };
        }
        "gzip" | "x-gzip" => MultiGzDecoder::new(bytes.as_slice())
            .take(limit)
            .read_to_end(&mut output),
        "deflate" => ZlibDecoder::new(bytes.as_slice())
            .take(limit)
            .read_to_end(&mut output),
        _ => {
            return Decoded {
                bytes,
                status: "unsupported",
            };
        }
    };
    let status = match result {
        Ok(_) if output.len() > MAX_DECODED_BYTES => "output_limit",
        Ok(_) => "complete",
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => "incomplete",
        Err(_) => {
            return Decoded {
                bytes,
                status: "invalid",
            };
        }
    };
    output.truncate(MAX_DECODED_BYTES);
    Decoded {
        bytes: output,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{
        Compression,
        write::{GzEncoder, ZlibEncoder},
    };
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gzip_deflate_and_concatenated_gzip_decode_without_changing_originals() {
        let original = gzip(b"<html>hello</html>");
        let decoded = decode(original.clone(), "gzip");
        assert_eq!(decoded.bytes, b"<html>hello</html>");
        assert_eq!(decoded.status, "complete");
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"{\"ok\":true}").unwrap();
        assert_eq!(
            decode(encoder.finish().unwrap(), "deflate").bytes,
            b"{\"ok\":true}"
        );
        let mut joined = original;
        joined.extend(gzip(b"second member"));
        assert_eq!(
            decode(joined, "gzip").bytes,
            b"<html>hello</html>second member"
        );
    }

    #[test]
    fn incomplete_gzip_keeps_the_recoverable_prefix_and_output_is_bounded() {
        let mut bytes = gzip(b"<html>recoverable prefix</html>");
        bytes.truncate(bytes.len() - 8);
        let decoded = decode(bytes, "gzip");
        assert_eq!(decoded.bytes, b"<html>recoverable prefix</html>");
        assert_eq!(decoded.status, "incomplete");
        let bomb = decode(gzip(&vec![b'a'; MAX_DECODED_BYTES + 10000]), "gzip");
        assert_eq!(bomb.bytes.len(), MAX_DECODED_BYTES);
        assert_eq!(bomb.status, "output_limit");
    }

    #[test]
    fn incomplete_deflate_is_not_reported_as_a_complete_stream() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"deflate prefix").unwrap();
        let mut bytes = encoder.finish().unwrap();
        bytes.truncate(bytes.len() - 4);
        let decoded = decode(bytes, "deflate");
        assert_eq!(decoded.bytes, b"deflate prefix");
        assert_eq!(decoded.status, "incomplete");
    }

    #[test]
    fn unsupported_or_corrupt_data_returns_the_original_bytes_explicitly() {
        for (encoding, status) in [
            ("br", "unsupported"),
            ("gzip", "invalid"),
            ("gzip, deflate", "unsupported"),
        ] {
            let original = b"not compressed data".to_vec();
            let result = decode(original.clone(), encoding);
            assert_eq!(result.bytes, original);
            assert_eq!(result.status, status);
        }
        let mut corrupt = gzip(b"hello");
        let trailer = corrupt.len() - 8;
        corrupt[trailer] ^= 0xff;
        assert_eq!(decode(corrupt, "gzip").status, "invalid");
    }
}
