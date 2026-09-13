//! The soundpad's two REST calls: the clip a member uploads, and the bytes
//! another member reads back to play it.
//!
//! A clip travels as the VORCSND1 container under this module's own
//! [`MEDIA_TYPE`], and the server stores it whole — it never decodes a sample.

use bytes::Bytes;
use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use vorcall_proto::v1::Sound;

use crate::connection::Blob;
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// `PROTOCOL.md` § Limits: the largest clip the sound endpoint accepts.
pub const MAX_BYTES: u64 = 16 << 20;

/// The media type the VORCSND1 container travels under.
pub const MEDIA_TYPE: &str = "application/vnd.vorcall.sound";

/// Uploads a cropped clip, answering with the stored record.
///
/// The body is the bytes themselves rather than protobuf, so this builds its own
/// request instead of going through [`http::post_proto`]. `name` travels as a
/// query parameter, like an image's purpose.
pub async fn upload(
    endpoints: &Endpoints,
    access_token: &str,
    name: &str,
    bytes: Blob,
) -> Result<Sound, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/sounds")?;
    url.query_pairs_mut().append_pair("name", name);

    let response = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, MEDIA_TYPE)
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

    Sound::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// Fetches the stored bytes of one clip.
pub async fn download(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
) -> Result<Vec<u8>, ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/sounds/{id}"))?;

    // A clip does not fit the shared client's total timeout.
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
    fn a_clip_is_capped_well_below_a_file() {
        // `PROTOCOL.md` § Limits: 16 MiB.
        assert_eq!(MAX_BYTES, 16 * 1024 * 1024);
        const { assert!(MAX_BYTES < crate::attachments::MAX_BYTES) };
    }

    #[test]
    fn the_container_travels_under_its_own_media_type() {
        assert_eq!(MEDIA_TYPE, "application/vnd.vorcall.sound");
    }
}
