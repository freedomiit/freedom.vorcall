//! The two URI list formats a file manager copies files as, decoded to paths.
//!
//! An RFC 2483 `text/uri-list` is CRLF-separated and may carry `#` comment
//! lines; GNOME's own `x-special/gnome-copied-files` prefixes the list with the
//! operation the user asked for — `copy` or `cut` — and separates the rest with
//! bare newlines. Both carry percent-encoded `file://` URIs.

use std::path::PathBuf;

/// Every local file an RFC 2483 URI list names, in the order it names them.
pub(crate) fn from_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    collect(text.lines())
}

/// Every local file GNOME's clipboard format names, past the operation line it
/// starts with.
pub(crate) fn from_gnome_copied_files(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().peekable();
    if lines.peek().is_some_and(|first| {
        let first = first.trim();
        first.eq_ignore_ascii_case("copy") || first.eq_ignore_ascii_case("cut")
    }) {
        lines.next();
    }
    collect(lines)
}

/// The path a single `file://` URI names, or `None` when it names something
/// this machine cannot simply open.
pub(crate) fn to_path(uri: &str) -> Option<PathBuf> {
    const SCHEME: &str = "file://";

    // The scheme is case-insensitive, unlike the rest of the URI.
    let rest = uri.get(SCHEME.len()..)?;
    if !uri[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let (host, path) = match rest.find('/') {
        Some(0) => ("", rest),
        Some(slash) => rest.split_at(slash),
        None => return None,
    };
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        return None;
    }
    path_from_bytes(percent_decode(path)?)
}

fn collect<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match to_path(line) {
            Some(path) => paths.push(path),
            None => tracing::debug!(
                uri = line,
                "skipping a clipboard URI that is not a local file"
            ),
        }
    }
    paths
}

/// The bytes a percent-encoded string stands for, or `None` when an escape is
/// malformed — a URI nobody can decode is one file skipped, never a panic.
fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let source = text.as_bytes();
    let mut decoded = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if source[index] == b'%' {
            let high = source.get(index + 1)?;
            let low = source.get(index + 2)?;
            decoded.push((hex(*high)? << 4) | hex(*low)?);
            index += 3;
        } else {
            decoded.push(source[index]);
            index += 1;
        }
    }
    Some(decoded)
}

fn hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

// A Unix path is a byte string that need not be UTF-8, and a file whose name is
// not decodes to a path that still opens.
#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &str) -> Vec<String> {
        from_uri_list(list.as_bytes())
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn reads_a_crlf_list_and_drops_its_comments() {
        let list = "# a comment\r\nfile:///tmp/one.txt\r\nfile:///tmp/two.txt\r\n";
        assert_eq!(paths(list), vec!["/tmp/one.txt", "/tmp/two.txt"]);
    }

    #[test]
    fn decodes_percent_escapes() {
        let list = "file:///tmp/a%20name%20with%20spaces.txt\r\n";
        assert_eq!(paths(list), vec!["/tmp/a name with spaces.txt"]);
    }

    #[test]
    fn decodes_a_utf8_escape() {
        // "café.txt", the way a file manager encodes it.
        let list = "file:///tmp/caf%C3%A9.txt\r\n";
        assert_eq!(paths(list), vec!["/tmp/café.txt"]);
    }

    #[test]
    fn accepts_an_empty_or_localhost_host() {
        assert_eq!(paths("file:///tmp/one.txt"), vec!["/tmp/one.txt"]);
        assert_eq!(paths("file://localhost/tmp/one.txt"), vec!["/tmp/one.txt"]);
    }

    #[test]
    fn rejects_a_remote_host() {
        assert!(paths("file://fileserver/share/one.txt").is_empty());
    }

    #[test]
    fn rejects_another_scheme() {
        assert!(paths("https://example.com/one.txt\r\nmailto:someone@example.com").is_empty());
    }

    #[test]
    fn skips_a_malformed_escape_without_panicking() {
        let list = "file:///tmp/50%.txt\r\nfile:///tmp/%\r\nfile:///tmp/%A\r\nfile:///tmp/%ZZ.txt\r\nfile:///tmp/good.txt";
        assert_eq!(paths(list), vec!["/tmp/good.txt"]);
    }

    #[test]
    fn an_empty_list_is_no_paths() {
        assert!(paths("").is_empty());
        assert!(paths("\r\n\r\n").is_empty());
    }

    #[test]
    fn reads_the_gnome_format_past_its_operation_line() {
        let list = b"copy\nfile:///tmp/one.txt\nfile:///tmp/two.txt";
        let paths: Vec<_> = from_gnome_copied_files(list)
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/tmp/one.txt", "/tmp/two.txt"]);

        let cut = b"cut\nfile:///tmp/one.txt";
        assert_eq!(from_gnome_copied_files(cut).len(), 1);
    }

    #[test]
    fn keeps_a_gnome_list_that_starts_with_a_uri() {
        let list = b"file:///tmp/one.txt\nfile:///tmp/two.txt";
        assert_eq!(from_gnome_copied_files(list).len(), 2);
    }

    #[test]
    fn a_uri_without_a_path_is_not_a_file() {
        assert_eq!(to_path("file://localhost"), None);
        assert_eq!(to_path("file://"), None);
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        assert_eq!(
            to_path("FILE:///tmp/one.txt"),
            Some(PathBuf::from("/tmp/one.txt"))
        );
    }
}
