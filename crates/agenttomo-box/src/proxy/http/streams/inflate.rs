//! Bounded output chunks for observation only; forwarded bytes stay untouched.
use flate2::{Decompress, FlushDecompress, Status};

pub(super) fn feed(
    decoder: &mut Decompress,
    mut bytes: &[u8],
    emit: &mut impl FnMut(&[u8]),
) -> Result<(), String> {
    let mut output = [0; 8192];
    // Bound work for a single input chunk, including a decompression bomb.
    let mut produced = 0;
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(bytes, &mut output, FlushDecompress::Sync)
            .map_err(|error| error.to_string())?;
        let consumed = (decoder.total_in() - before_in) as usize;
        let count = (decoder.total_out() - before_out) as usize;
        produced += count;
        if produced > 8 * 1024 * 1024 {
            return Err("decompression work limit exceeded; forwarding continues".into());
        }
        emit(&output[..count]);
        bytes = &bytes[consumed..];
        if status == Status::StreamEnd {
            if !bytes.is_empty() {
                return Err("trailing or concatenated compressed stream is not inspected".into());
            }
            return Ok(());
        }
        if consumed == 0 && count == 0 {
            return Ok(());
        }
        if bytes.is_empty() && count < output.len() {
            return Ok(());
        }
    }
}
