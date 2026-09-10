//! The two requests the updater makes: the signed manifest, and the release
//! build for this platform.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use reqwest::header::AUTHORIZATION;
use ring::digest::{Context, SHA256};

use super::{Asset, UpdateError, Version, hash, manifest};
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// Carries the detached signature of the manifest body.
pub const SIGNATURE_HEADER: &str = "X-Vorcall-Manifest-Signature";

/// A manifest for a handful of platforms is a few hundred bytes; the cap only
/// stops a broken server from streaming into memory forever.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// The manifest bytes exactly as the server sent them — the signature covers
/// those bytes, so nothing may normalise them — and the signature hex.
pub async fn fetch_manifest(
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<(Vec<u8>, String), UpdateError> {
    let url = endpoints
        .http_base
        .join("/api/updates/manifest")
        .map_err(|e| UpdateError::Api(ApiFailure::Malformed(e.to_string())))?;

    let mut response = http::client()?
        .get(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| UpdateError::Api(ApiFailure::Transport(e.to_string())))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(UpdateError::Manifest("no manifest published".to_owned()));
    }
    if !response.status().is_success() {
        return Err(UpdateError::Api(http::failure_from(response).await));
    }

    let signature = response
        .headers()
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .ok_or(UpdateError::Signature)?;

    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| UpdateError::Api(ApiFailure::Transport(e.to_string())))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(UpdateError::Manifest("manifest too large".to_owned()));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok((bytes, signature))
}

/// Streams the release into `dest`, which the caller has already created, and
/// verifies its size and digest before returning. On any failure `dest` is
/// removed: a partial file must never look like a finished download.
pub async fn download_asset(
    endpoints: &Endpoints,
    access_token: &str,
    version: &Version,
    asset: &Asset,
    dest: &Path,
    mut progress: impl FnMut(u64, u64) + Send,
) -> Result<(), UpdateError> {
    let outcome = stream_asset(endpoints, access_token, version, asset, dest, &mut progress).await;
    if outcome.is_err() {
        let _ = fs::remove_file(dest);
    }
    outcome
}

async fn stream_asset<P: FnMut(u64, u64) + Send>(
    endpoints: &Endpoints,
    access_token: &str,
    version: &Version,
    asset: &Asset,
    dest: &Path,
    progress: &mut P,
) -> Result<(), UpdateError> {
    // A manifest that never went through `validate()` must not steer the URL.
    if !manifest::is_bare_filename(&asset.path) {
        return Err(UpdateError::Manifest(format!(
            "{:?} is not a bare filename",
            asset.path
        )));
    }

    let url = endpoints
        .http_base
        .join(&format!("/api/updates/{version}/{}", asset.path))
        .map_err(|e| UpdateError::Api(ApiFailure::Malformed(e.to_string())))?;

    let mut response = http::download_client()?
        .get(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| UpdateError::Api(ApiFailure::Transport(e.to_string())))?;

    if !response.status().is_success() {
        return Err(UpdateError::Api(http::failure_from(response).await));
    }

    let mut file = OpenOptions::new().write(true).truncate(true).open(dest)?;
    let mut streamed = Context::new(&SHA256);
    let mut received: u64 = 0;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| UpdateError::Api(ApiFailure::Transport(e.to_string())))?
    {
        received += chunk.len() as u64;
        if received > asset.size {
            return Err(UpdateError::Size {
                expected: asset.size,
                actual: received,
            });
        }

        file.write_all(&chunk)?;
        streamed.update(&chunk);
        progress(received, asset.size);
    }

    if received != asset.size {
        return Err(UpdateError::Size {
            expected: asset.size,
            actual: received,
        });
    }
    file.sync_all()?;
    drop(file);

    // Hashed twice on purpose: once as the bytes arrived, once from the file
    // that will be renamed over the running binary, so a short write is caught
    // as well as a corrupted transfer.
    let streamed = hex::encode(streamed.finish());
    let stored = hash::sha256_file_hex(dest)?;
    if stored != streamed || !stored.eq_ignore_ascii_case(&asset.sha256) {
        return Err(UpdateError::Hash {
            expected: asset.sha256.clone(),
            actual: stored,
        });
    }

    Ok(())
}
