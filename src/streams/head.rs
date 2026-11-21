//! Fetches the next stream's first bytes ahead, so its download need not wait
//! on googlevideo's slow first answer.

use super::ResolvedStream;

/// How much of the next stream `warm` fetches.
const HEAD_LEN: usize = 64 * 1024;

/// Fetches the likely next stream's first bytes, as googlevideo's first answer
/// for a rarely played song can take over half a second, and keeps them for
/// its download to start from. Blocking, and on the download's client so the
/// connection is pooled for it.
pub fn warm(stream: &ResolvedStream) {
    if !crate::http::allowed(&stream.url) {
        return;
    }
    match first_bytes(&stream.url, &stream.headers) {
        Ok(bytes) if head_fits(bytes.len(), stream.size) => {
            let _ = stream.head.set(bytes);
        }
        Ok(_) => log::debug!("the next stream's first bytes do not fit its length"),
        Err(e) => log::debug!("could not warm the next stream: {e}"),
    }
}

/// Whether `len` bytes are the whole first range of a stream of `size`, as
/// the download carries on after them. One of unknown size is downloaded
/// without ranges, so from its start.
fn head_fits(len: usize, size: u64) -> bool {
    size > 0 && len as u64 == size.min(HEAD_LEN as u64)
}

/// A stream's first bytes, from an answer that starts at its first byte.
/// Blocking, so never call it on Tokio.
pub(super) fn first_bytes(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
    let mut request = crate::http::blocking_client()
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes=0-{}", HEAD_LEN - 1));
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.without_url().to_string())?;
    // Anything else may be the whole song, too much to read for a warm-up.
    let from_start = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|r| r.starts_with("bytes 0-"));
    let short = response
        .content_length()
        .is_some_and(|n| n <= HEAD_LEN as u64);
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT || !from_start || !short {
        return Err("the answer is not the stream's first bytes".into());
    }
    let bytes = response.bytes().map_err(|e| e.without_url().to_string())?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_a_whole_first_range() {
        assert!(head_fits(HEAD_LEN, 4171552));
        assert!(head_fits(1000, 1000), "a stream shorter than the range");
        assert!(!head_fits(HEAD_LEN - 1, 4171552), "cut short");
        assert!(!head_fits(1001, 1000), "longer than the stream");
        assert!(!head_fits(HEAD_LEN, 0), "unknown size");
    }
}
