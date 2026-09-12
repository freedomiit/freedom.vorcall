//! Image attachments: what the server accepts, told from the bytes themselves,
//! and the two REST calls that put one on the server and get it back.

use bytes::Bytes;
use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use vorcall_proto::v1::Attachment;

use crate::connection::Blob;
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// `PROTOCOL.md` § Limits: the largest body the upload endpoint accepts.
pub const MAX_BYTES: usize = 8 << 20;

/// `PROTOCOL.md` § Limits: attachment ids a single `SendMessage` may carry.
pub const MAX_PER_MESSAGE: usize = 4;

/// The optional header carrying the original file name on an upload.
pub const FILENAME_HEADER: &str = "X-Vorcall-Filename";

/// The content type the server accepts for these bytes, read from their magic
/// number. `None` when they are not one of the four accepted image types.
///
/// The server sniffs the same way and aborts an upload whose declared type does
/// not match the first bytes, so the client must declare what it finds here.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF container: four length bytes sit between the two tags.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// The file extension for a content type the server accepts.
pub fn extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Uploads the raw bytes to `channel_id`, answering with the stored attachment.
///
/// The body is the bytes themselves rather than protobuf, so this builds its own
/// request instead of going through [`http::post_proto`].
pub async fn upload(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    file_name: &str,
    content_type: &'static str,
    bytes: Blob,
) -> Result<Attachment, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/attachments")?;
    url.query_pairs_mut()
        .append_pair("channel", &channel_id.to_string());

    let mut request = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, content_type)
        // The body reads the shared buffer rather than a copy of it: `Bytes`
        // keeps the blob alive for as long as the request needs it.
        .body(Bytes::from_owner(bytes));
    // A control character cannot go in a header value; the name is optional, so
    // drop it rather than fail the upload over it.
    if !file_name.is_empty() && !file_name.chars().any(char::is_control) {
        request = request.header(FILENAME_HEADER, file_name);
    }

    let response = request
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    Attachment::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// Fetches the stored bytes of one attachment.
pub async fn download(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
) -> Result<Vec<u8>, ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/attachments/{id}"))?;

    // An 8 MiB image does not fit the shared client's total timeout.
    let response = http::download_client()?
        .get(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| ApiFailure::Transport(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_a_png_header() {
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(sniff(&bytes), Some("image/png"));
    }

    #[test]
    fn sniffs_a_jpeg_header() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(sniff(&bytes), Some("image/jpeg"));
    }

    #[test]
    fn sniffs_both_gif_headers() {
        assert_eq!(sniff(b"GIF87a...."), Some("image/gif"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
    }

    #[test]
    fn sniffs_a_webp_header() {
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff(&bytes), Some("image/webp"));
    }

    #[test]
    fn webp_needs_both_tags() {
        let mut riff_only = Vec::from(*b"RIFF");
        riff_only.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        riff_only.extend_from_slice(b"AVI LIST");
        assert_eq!(sniff(&riff_only), None);

        // The WEBP tag alone, without the RIFF container, is not a webp file.
        assert_eq!(sniff(b"XXXX\0\0\0\0WEBP"), None);
    }

    #[test]
    fn rejects_a_bmp_header_and_empty_input() {
        assert_eq!(sniff(b"BM\x36\x00\x00\x00"), None);
        assert_eq!(sniff(&[]), None);
    }

    #[test]
    fn maps_every_accepted_type_to_an_extension() {
        assert_eq!(extension("image/png"), Some("png"));
        assert_eq!(extension("image/jpeg"), Some("jpg"));
        assert_eq!(extension("image/gif"), Some("gif"));
        assert_eq!(extension("image/webp"), Some("webp"));
    }

    #[test]
    fn rejects_an_unaccepted_content_type() {
        assert_eq!(extension("image/bmp"), None);
        assert_eq!(extension(""), None);
    }
}
