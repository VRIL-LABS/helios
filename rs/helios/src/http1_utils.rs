use anyhow::{Context as _, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

// In an HTTP/1.x status line like `HTTP/1.1 200 OK`, the three status-code
// bytes occupy offsets 9..12.
const HTTP_STATUS_CODE_START: usize = 9;
const HTTP_STATUS_CODE_END: usize = 12;

pub(crate) async fn write_all_fast(stream: &mut TcpStream, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        match stream.try_write(bytes) {
            Ok(0) => stream.write_all(bytes).await.context("write socket")?,
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                stream.writable().await.context("wait writable socket")?;
            }
            Err(e) => return Err(e).context("write socket"),
        }
    }
    Ok(())
}

pub(crate) fn find_header_end_from(buf: &[u8], start: usize) -> Option<usize> {
    let max_start = buf.len().checked_sub(4)?;
    if start > max_start {
        return None;
    }

    for offset in memchr::memchr_iter(b'\r', &buf[start..=max_start]) {
        let i = start + offset;
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

pub(crate) fn content_length(headers: &[u8]) -> Option<usize> {
    let mut start = 0usize;
    while start < headers.len() {
        let line_end = memchr::memchr(b'\n', &headers[start..])
            .map(|n| start + n)
            .unwrap_or(headers.len());
        let line = headers[start..line_end]
            .strip_suffix(b"\r")
            .unwrap_or(&headers[start..line_end]);
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            let (name, value) = line.split_at(colon);
            if name.eq_ignore_ascii_case(b"content-length") {
                return parse_ascii_usize(&value[1..]);
            }
        }
        start = line_end.saturating_add(1);
    }
    None
}

fn parse_ascii_usize(value: &[u8]) -> Option<usize> {
    let mut value = value;
    while matches!(value.first(), Some(b' ' | b'\t')) {
        value = &value[1..];
    }
    while matches!(value.last(), Some(b' ' | b'\t' | b'\r')) {
        value = &value[..value.len() - 1];
    }
    if value.is_empty() {
        return None;
    }

    let mut n = 0usize;
    for b in value {
        match *b {
            b'0'..=b'9' => {
                n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
            }
            _ => return None,
        }
    }
    Some(n)
}

pub(crate) fn status_is_success(response_bytes: &[u8]) -> bool {
    matches!(
        response_bytes.get(HTTP_STATUS_CODE_START..HTTP_STATUS_CODE_END),
        Some([b'2', b'0'..=b'9', b'0'..=b'9'])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_header_end_from_offset() {
        let buf = b"GET / HTTP/1.1\r\nhost: example\r\n\r\nnext";

        assert_eq!(find_header_end_from(buf, 0), Some(29));
        assert_eq!(find_header_end_from(buf, 16), Some(29));
        assert_eq!(find_header_end_from(buf, 31), None);
    }

    #[test]
    fn parses_content_length_with_optional_whitespace() {
        let headers = b"GET / HTTP/1.1\r\nContent-Length: \t42 \r\n\r\n";

        assert_eq!(content_length(headers), Some(42));
    }

    #[test]
    fn rejects_malformed_content_length() {
        let headers = b"HTTP/1.1 200 OK\r\ncontent-length: 42 bytes\r\n\r\n";

        assert_eq!(content_length(headers), None);
    }
}
