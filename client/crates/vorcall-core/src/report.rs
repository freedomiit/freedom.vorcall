//! Sending the client's own diagnostics to the server: the log files and the
//! crash reports a friend can hand over from the Settings page, without having
//! to find them on disk first.

use bytes::Bytes;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// `PROTOCOL.md` § Limits: the largest body the diagnostics endpoint accepts.
pub const MAX_BYTES: usize = 4 << 20;

/// The header carrying the file's own name, which is how the server files it.
pub const FILENAME_HEADER: &str = "X-Vorcall-Filename";

/// The only content type the endpoint accepts: a report is text, never a dump.
const TEXT: &str = "text/plain; charset=utf-8";

/// Uploads one file as a report of `kind` — `log` or `crash`.
pub async fn upload(
    endpoints: &Endpoints,
    access_token: &str,
    kind: &str,
    file_name: &str,
    body: Vec<u8>,
) -> Result<(), ApiFailure> {
    let name = header_name(file_name)?;

    let mut url = endpoints
        .http_base
        .join("/api/diagnostics")
        .map_err(|e| ApiFailure::Malformed(e.to_string()))?;
    url.query_pairs_mut().append_pair("kind", kind);

    // A log file does not fit the shared client's total timeout any better than
    // a release download does.
    let mut request = http::download_client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, TEXT)
        .body(Bytes::from_owner(body));
    if let Some(name) = name {
        request = request.header(FILENAME_HEADER, name);
    }

    let response = request
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    Ok(())
}

/// The name to send, or nothing when there is none. Unlike an attachment's
/// name, which is decoration, this one is how the report is filed, so a name a
/// header cannot carry fails the upload instead of being dropped.
fn header_name(file_name: &str) -> Result<Option<&str>, ApiFailure> {
    if file_name.is_empty() {
        return Ok(None);
    }
    if file_name.chars().any(char::is_control) {
        return Err(ApiFailure::Malformed("invalid file name".to_owned()));
    }
    Ok(Some(file_name))
}

/// The last `max` bytes at most, starting on a line boundary: the end of a log
/// is what says what went wrong, and half a first line only confuses the read.
pub fn tail(bytes: Vec<u8>, max: usize) -> Vec<u8> {
    if bytes.len() <= max {
        return bytes;
    }

    let cut = bytes.len() - max;
    let start = match bytes[cut..].iter().position(|&byte| byte == b'\n') {
        Some(offset) => cut + offset + 1,
        // Nothing in the kept part is a line break; there is no boundary to
        // move to and dropping it all would send an empty report.
        None => cut,
    };
    bytes[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_under_the_cap_is_sent_whole() {
        let log = b"first\nsecond\nthird\n".to_vec();
        assert_eq!(tail(log.clone(), MAX_BYTES), log);
        assert_eq!(tail(log.clone(), log.len()), log);
    }

    #[test]
    fn a_long_file_keeps_its_tail_from_a_line_boundary() {
        let log = b"first\nsecond\nthird\n".to_vec();

        // The last 10 bytes are "nd\nthird\n", cut inside "second"; the report
        // starts at the line after it.
        assert_eq!(tail(log.clone(), 10), b"third\n".to_vec());
        // A cut that already lands on a line start keeps that whole line.
        assert_eq!(tail(log, 7), b"third\n".to_vec());
    }

    #[test]
    fn a_tail_without_a_line_break_is_kept_as_it_is() {
        assert_eq!(tail(b"abcdefgh".to_vec(), 3), b"fgh".to_vec());
        assert_eq!(tail(Vec::new(), 4), Vec::<u8>::new());
    }

    #[test]
    fn a_file_name_a_header_cannot_carry_is_refused() {
        assert!(matches!(
            header_name("crash\n20250904.txt"),
            Err(ApiFailure::Malformed(detail)) if detail == "invalid file name"
        ));
        assert!(matches!(
            header_name("vorcall\u{7f}.log"),
            Err(ApiFailure::Malformed(_))
        ));
    }

    #[test]
    fn a_plain_file_name_is_sent_and_an_empty_one_is_left_out() {
        assert!(matches!(
            header_name("crash-20250904T153320Z.txt"),
            Ok(Some("crash-20250904T153320Z.txt"))
        ));
        assert!(matches!(header_name(""), Ok(None)));
    }
}
