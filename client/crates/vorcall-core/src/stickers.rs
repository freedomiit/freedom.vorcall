//! The sticker library's two REST calls: the picture a manager adds, and the
//! bytes every member reads back to draw one in the message list.
//!
//! A sticker is one of the four picture types [`crate::attachments::sniff_image`]
//! knows, under this module's own [`MAX_BYTES`]: it is drawn at 160 pixels in a
//! row, so a megabyte is already generous.

use bytes::Bytes;
use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use vorcall_proto::v1::Sticker;

use crate::connection::Blob;
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// `PROTOCOL.md` § Limits: the largest body the sticker endpoint accepts.
pub const MAX_BYTES: u64 = 1 << 20;

/// The picture types a sticker may travel as, which is what the server sniffs
/// the bytes against.
pub const MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Whether the server will take a picture of this type. Matched
/// case-insensitively: the type is derived from a file name or a magic number,
/// and RFC 2045 does not make the case meaningful.
pub fn is_accepted(content_type: &str) -> bool {
    MEDIA_TYPES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(content_type))
}

/// Uploads the raw bytes, answering with the stored sticker.
///
/// The body is the bytes themselves rather than protobuf, so this builds its own
/// request instead of going through [`http::post_proto`]. `name` travels as a
/// query parameter, like a clip's.
pub async fn upload(
    endpoints: &Endpoints,
    access_token: &str,
    name: &str,
    content_type: &str,
    bytes: Blob,
) -> Result<Sticker, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/stickers")?;
    url.query_pairs_mut().append_pair("name", name);

    let response = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, content_type.to_owned())
        // The body reads the shared buffer rather than a copy of it: `Bytes`
        // keeps the blob alive for as long as the request needs it.
        .body(Bytes::from_owner(bytes))
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

    Sticker::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// Fetches the stored bytes of one sticker.
pub async fn download(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
) -> Result<Vec<u8>, ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/stickers/{id}"))?;

    let response = http::client()?
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
    fn a_sticker_is_capped_far_below_a_picture() {
        // `PROTOCOL.md` § Limits: 1 MiB.
        assert_eq!(MAX_BYTES, 1024 * 1024);
        const { assert!(MAX_BYTES < crate::images::MAX_BYTES) };
    }

    #[test]
    fn only_the_four_picture_types_are_taken() {
        for content_type in MEDIA_TYPES {
            assert!(is_accepted(content_type), "{content_type} should be taken");
        }
        assert!(is_accepted("IMAGE/PNG"));

        for content_type in ["image/svg+xml", "image/bmp", "text/plain", ""] {
            assert!(
                !is_accepted(content_type),
                "{content_type} should be refused"
            );
        }
    }
}
