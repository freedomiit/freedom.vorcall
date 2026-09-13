//! Avatars, banners, the server icon and role icons: what each is for, how large
//! it should be, and the two REST calls that put one on the server and read one
//! back.
//!
//! The accepted content types are the four [`crate::attachments::sniff_image`]
//! knows, which the server sniffs the same way. The ceiling is this module's
//! own [`MAX_BYTES`]: an attachment may run to gigabytes now, an avatar may
//! not.

use bytes::Bytes;
use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use vorcall_proto::v1::Image;

use crate::connection::Blob;
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// `PROTOCOL.md` § Limits: the largest body the image endpoint accepts. A
/// picture is drawn from memory and drawn small, so this is the attachments'
/// old ceiling rather than their new one.
pub const MAX_BYTES: u64 = 8 << 20;

/// What an image is for. The server checks the caller's permissions against the
/// declared purpose — `server_icon` needs `MANAGE_SERVER`, `role_icon` needs
/// `MANAGE_ROLES`, an avatar or a banner needs nothing but an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePurpose {
    Avatar,
    Banner,
    ServerIcon,
    RoleIcon,
}

impl ImagePurpose {
    /// The spelling the `purpose` query parameter takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Banner => "banner",
            Self::ServerIcon => "server_icon",
            Self::RoleIcon => "role_icon",
        }
    }

    /// How large, in pixels, the client scales an image to before uploading it:
    /// nothing ever draws one bigger than this.
    pub fn max_size(self) -> (u32, u32) {
        match self {
            Self::Avatar => (512, 512),
            Self::Banner => (1600, 600),
            Self::ServerIcon => (512, 512),
            Self::RoleIcon => (128, 128),
        }
    }
}

/// Uploads the raw bytes, answering with the stored image.
///
/// The body is the bytes themselves rather than protobuf, so this builds its own
/// request instead of going through [`http::post_proto`].
pub async fn upload(
    endpoints: &Endpoints,
    access_token: &str,
    purpose: ImagePurpose,
    content_type: &'static str,
    bytes: Blob,
) -> Result<Image, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/images")?;
    url.query_pairs_mut()
        .append_pair("purpose", purpose.as_str());

    let response = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, content_type)
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

    Image::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// Fetches the stored bytes of one image.
pub async fn download(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
) -> Result<Vec<u8>, ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/images/{id}"))?;

    // A banner does not fit the shared client's total timeout.
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
    fn every_purpose_spells_itself_as_the_endpoint_expects() {
        assert_eq!(ImagePurpose::Avatar.as_str(), "avatar");
        assert_eq!(ImagePurpose::Banner.as_str(), "banner");
        assert_eq!(ImagePurpose::ServerIcon.as_str(), "server_icon");
        assert_eq!(ImagePurpose::RoleIcon.as_str(), "role_icon");
    }

    #[test]
    fn a_picture_is_capped_far_below_a_file() {
        // `PROTOCOL.md` § Limits: 8 MiB, which is no longer the attachments'.
        assert_eq!(MAX_BYTES, 8 * 1024 * 1024);
        const { assert!(MAX_BYTES < crate::attachments::MAX_BYTES) };
    }

    #[test]
    fn a_banner_is_the_only_wide_purpose() {
        assert_eq!(ImagePurpose::Avatar.max_size(), (512, 512));
        assert_eq!(ImagePurpose::Banner.max_size(), (1600, 600));
        assert_eq!(ImagePurpose::ServerIcon.max_size(), (512, 512));
        assert_eq!(ImagePurpose::RoleIcon.max_size(), (128, 128));
    }
}
