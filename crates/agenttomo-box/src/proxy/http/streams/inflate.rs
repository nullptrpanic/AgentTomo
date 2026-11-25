//! Bounded output chunks for observation only; forwarded bytes stay untouched.
use flate2::{Decompress, FlushDecompress, Status};

pub(super) fn feed(
    decoder: &mut Decompress,
    bytes: &[u8],
    emit: &mut impl FnMut(&[u8]),
) -> Result<(), String> {
    let mut produced = 0;
    let (consumed, _) = chunk(decoder, bytes, &mut produced, emit)?;
    if consumed != bytes.len() {
        return Err("unexpected trailing compressed data".into());
    }
    Ok(())
}

pub(super) fn chunk(
    decoder: &mut Decompress,
    input: &[u8],
    produced: &mut usize,
    emit: &mut impl FnMut(&[u8]),
) -> Result<(usize, bool), String> {
    let mut bytes = input;
    let mut output = [0; 8192];
    // Bound work across the entire input chunk, including concatenated members.
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(bytes, &mut output, FlushDecompress::Sync)
            .map_err(|error| error.to_string())?;
        let consumed = (decoder.total_in() - before_in) as usize;
        let count = (decoder.total_out() - before_out) as usize;
        *produced += count;
        if *produced > 8 * 1024 * 1024 {
            return Err("decompression work limit exceeded; forwarding continues".into());
        }
        emit(&output[..count]);
        bytes = &bytes[consumed..];
        if status == Status::StreamEnd {
            return Ok((input.len() - bytes.len(), true));
        }
        if consumed == 0 && count == 0 {
            return Ok((input.len() - bytes.len(), false));
        }
        if bytes.is_empty() && count < output.len() {
            return Ok((input.len() - bytes.len(), false));
        }
    }
}

pub(super) enum SseDecoder {
    Gzip(Decompress),
    Deflate(Decompress),
    Zstd(zstd::stream::raw::Decoder<'static>),
}

impl SseDecoder {
    pub fn new(
        encoding: crate::content_encoding::ContentEncoding,
    ) -> std::io::Result<Option<Self>> {
        use crate::content_encoding::ContentEncoding;
        Ok(match encoding {
            ContentEncoding::Identity => None,
            ContentEncoding::Gzip => Some(Self::Gzip(Decompress::new_gzip(15))),
            ContentEncoding::Deflate => Some(Self::Deflate(Decompress::new(true))),
            ContentEncoding::Zstd => {
                let mut decoder = zstd::stream::raw::Decoder::new()?;
                decoder.set_parameter(zstd::stream::raw::DParameter::WindowLogMax(23))?;
                Some(Self::Zstd(decoder))
            }
        })
    }

    pub fn feed(
        &mut self,
        mut bytes: &[u8],
        ended: &mut bool,
        mut emit: impl FnMut(&[u8]),
    ) -> Result<(), String> {
        let mut produced = 0;
        while !bytes.is_empty() {
            if *ended && let Self::Gzip(decoder) = self {
                *decoder = Decompress::new_gzip(15);
            }
            let (read, complete) = match self {
                Self::Gzip(decoder) | Self::Deflate(decoder) => {
                    chunk(decoder, bytes, &mut produced, &mut emit)?
                }
                Self::Zstd(decoder) => {
                    use zstd::stream::raw::Operation;
                    let mut output = [0; 8192];
                    let mut read = 0;
                    let complete = loop {
                        let status = decoder
                            .run_on_buffers(&bytes[read..], &mut output)
                            .map_err(|e| e.to_string())?;
                        read += status.bytes_read;
                        produced += status.bytes_written;
                        if produced > 8 * 1024 * 1024 {
                            return Err("SSE decompression work limit exceeded".into());
                        }
                        emit(&output[..status.bytes_written]);
                        if status.remaining == 0 || status.bytes_written < output.len() {
                            break status.remaining == 0;
                        }
                    };
                    (read, complete)
                }
            };
            if read == 0 {
                return Err("invalid trailing SSE compression data".into());
            }
            bytes = &bytes[read..];
            *ended = complete;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_encoding::ContentEncoding;
    use std::io::Write;
    #[test]
    fn split_gzip_and_zstd_members_preserve_all_events_and_frame_boundaries() {
        for encoding in [ContentEncoding::Gzip, ContentEncoding::Zstd] {
            let pack = |data: &[u8]| match encoding {
                ContentEncoding::Gzip => {
                    let mut out = flate2::write::GzEncoder::new(Vec::new(), Default::default());
                    out.write_all(data).unwrap();
                    out.finish().unwrap()
                }
                _ => zstd::stream::encode_all(data, 1).unwrap(),
            };
            let mut wire = pack(b"data: first\n\n");
            wire.extend(pack(b"data: second\n\n"));
            let mut decoder = SseDecoder::new(encoding).unwrap().unwrap();
            let mut ended = false;
            let mut values = Vec::new();
            let (head, last) = wire.split_at(wire.len() - 1);
            for part in head.chunks(3) {
                decoder
                    .feed(part, &mut ended, |bytes| values.extend_from_slice(bytes))
                    .unwrap();
            }
            assert!(!ended);
            decoder
                .feed(last, &mut ended, |bytes| values.extend_from_slice(bytes))
                .unwrap();
            assert!(ended);
            assert_eq!(values, b"data: first\n\ndata: second\n\n");
        }
    }

    #[test]
    fn concatenated_gzip_members_share_one_decompression_budget() {
        let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        out.write_all(&vec![b'a'; 256 * 1024]).unwrap();
        let member = out.finish().unwrap();
        let mut wire = Vec::new();
        for _ in 0..33 {
            wire.extend_from_slice(&member);
        }
        let mut decoder = SseDecoder::new(ContentEncoding::Gzip).unwrap().unwrap();
        let mut ended = false;
        let err = decoder.feed(&wire, &mut ended, |_| {}).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }
}
