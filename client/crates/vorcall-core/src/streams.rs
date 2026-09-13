//! Streamed files: the ones too large to be stored, which the sender's own
//! client serves on demand while the server only proxies the bytes.
//!
//! The offer and the decline are small protobuf calls; the two transfers are
//! not. A read may run for hours, so it goes through the client with no total
//! timeout and straight onto the disk, and a range this client serves streams
//! off the disk the same way. Nothing here ever holds a whole file in memory: a
//! streamed file starts where an attachment stops, and can be tens of
//! gigabytes.
//!
//! Which local file a stream id stands for is [`crate::registry`]'s business,
//! not this module's.

use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use prost::Message as _;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use tokio::sync::mpsc;
use vorcall_proto::v1::{StreamOffer, StreamRequest, StreamedFile};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};
use crate::transfer::{self, Steps};

/// How far the disk may run ahead of the socket on a range this client serves,
/// and how much of the file one block of it holds.
const READ_QUEUE: usize = 4;
const READ_BLOCK: u64 = 1 << 18;

/// What a served range travels as: the server proxies the bytes on to whoever
/// asked, and only the [`StreamedFile`] record says what they really are.
const CHUNK_TYPE: &str = "application/octet-stream";

/// Why a streamed file did not move.
///
/// The three answers a read can get that are nothing to do with this client are
/// their own variants: a sender who is offline, one who no longer has the file
/// and one who never answered are three different sentences in the interface,
/// not one HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Api(#[from] ApiFailure),
    #[error("the sender is offline")]
    OwnerOffline,
    #[error("the sender no longer has this file")]
    Declined,
    #[error("the sender did not answer")]
    NoAnswer,
    #[error("the transfer ended after {actual} of {expected} bytes")]
    Short { expected: u64, actual: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl transfer::DownloadError for StreamError {
    fn from_status(status: StatusCode) -> Option<Self> {
        match status {
            StatusCode::CONFLICT => Some(StreamError::OwnerOffline),
            StatusCode::GONE => Some(StreamError::Declined),
            StatusCode::GATEWAY_TIMEOUT => Some(StreamError::NoAnswer),
            _ => None,
        }
    }

    fn disk(_action: &str, _path: &Path, error: std::io::Error) -> Self {
        StreamError::Io(error)
    }

    fn task(error: tokio::task::JoinError) -> Self {
        ApiFailure::Transport(error.to_string()).into()
    }
}

/// Offers a local file to `channel_id`, answering with the record the server
/// wrote. Nothing is uploaded: the offer is the file's name, type and size, and
/// the bytes are read back out of this client later.
///
/// `size` is the length the sender measured, which is what every range the
/// server later asks for is measured against.
pub async fn offer(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    file_name: &str,
    content_type: &str,
    size: i64,
) -> Result<StreamedFile, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/streams")?;
    url.query_pairs_mut()
        .append_pair("channel", &channel_id.to_string());

    let offer = StreamOffer {
        file_name: file_name.to_owned(),
        content_type: content_type.to_owned(),
        size,
    };

    let body = http::post_proto(endpoints, Some(access_token), url, &offer).await?;
    StreamedFile::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// Reads one streamed file onto disk at `path`, carrying on from where an
/// earlier try stopped. `progress` is told the bytes on disk of the bytes there
/// are.
///
/// The bytes land in a `.part` sibling that is renamed into place once the last
/// one has arrived, so `path` never holds half a file; the `.part` is what the
/// next call resumes from, and a failure deliberately leaves it there.
///
/// The three answers only a streamed file can give — the sender is offline, the
/// sender declined, the sender never answered — come back as their own
/// [`StreamError`] variants rather than as a status code.
pub async fn fetch_to_path(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
    path: &Path,
    progress: impl FnMut(u64, u64) + Send,
) -> Result<(), StreamError> {
    let url = http::api_url(endpoints, &format!("/api/streams/{id}"))?;

    transfer::download(
        path,
        || {
            Ok(http::download_client()?
                .get(url.clone())
                .header(http::KEY_HEADER, &endpoints.key)
                .header(AUTHORIZATION, http::bearer(access_token)))
        },
        |expected, received| StreamError::Short {
            expected,
            actual: received,
        },
        progress,
    )
    .await
}

/// Serves the range `request` asks for out of the local file at `path`.
///
/// The bytes stream off the disk a block at a time, a reader thread running one
/// [`READ_QUEUE`] ahead of the socket: a streamed file does not fit in memory,
/// which is the whole reason it is not an attachment. `Content-Length` states
/// the range exactly, so the server can promise the reader a length before the
/// first byte of it arrives.
///
/// A `request.length` of 0 is everything from `request.offset` to the end of the
/// file. A range the file cannot cover is refused before a byte goes out: the
/// file has changed since the offer, and serving what is left would hand the
/// reader another file's bytes at that offset —
/// [`crate::registry::Registry::verify`] is what should have caught it first.
///
/// `progress` is told the bytes handed to the socket of the bytes the range
/// covers. The body is what reports and reqwest owns the body, so unlike
/// [`fetch_to_path`]'s this one has to outlive the call.
pub async fn serve_chunk(
    endpoints: &Endpoints,
    access_token: &str,
    request: &StreamRequest,
    path: &Path,
    progress: impl FnMut(u64, u64) + Send + 'static,
) -> Result<(), StreamError> {
    let offset = u64::try_from(request.offset).map_err(|_| negative("offset", request.offset))?;
    let length = u64::try_from(request.length).map_err(|_| negative("length", request.length))?;

    let available = fs::metadata(path)?.len().saturating_sub(offset);
    let wanted = if length == 0 { available } else { length };
    if wanted > available {
        return Err(StreamError::Short {
            expected: wanted,
            actual: available,
        });
    }

    let mut url = http::api_url(
        endpoints,
        &format!("/api/streams/{}/chunks", request.stream_id),
    )?;
    url.query_pairs_mut()
        .append_pair("transfer", &request.transfer_id.to_string());

    let (blocks, reader) = spawn_reader(path.to_path_buf(), offset, wanted);
    // Counters and callback ride in the stream's own state: the body outlives
    // this frame, so it can borrow nothing from it.
    let body = futures::stream::unfold(
        (blocks, 0u64, Steps::new(wanted), progress),
        move |(mut blocks, mut sent, mut steps, mut progress)| async move {
            let block = blocks.recv().await?;
            sent += block.len() as u64;
            if steps.admits(sent) {
                progress(sent, wanted);
            }
            Some((
                Ok::<Bytes, std::io::Error>(block),
                (blocks, sent, steps, progress),
            ))
        },
    );

    let posted = http::upload_client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, CHUNK_TYPE)
        .header(CONTENT_LENGTH, wanted)
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await;

    // The disk error comes first: a body that stopped early is all the socket
    // ever saw of it.
    reader
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))??;

    let response = posted.map_err(|e| ApiFailure::Transport(e.to_string()))?;
    if !response.status().is_success() {
        return Err(http::failure_from(response).await.into());
    }
    Ok(())
}

/// Tells the server this client will not serve `transfer_id` after all, which
/// is what turns a reader's wait into a `410`.
///
/// `reason` is the server's record of why: a short phrase, never a path or
/// anything else out of the user's file system.
pub async fn decline(
    endpoints: &Endpoints,
    access_token: &str,
    stream_id: i64,
    transfer_id: i64,
    reason: &str,
) -> Result<(), ApiFailure> {
    let mut url = http::api_url(endpoints, &format!("/api/streams/{stream_id}/decline"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("transfer", &transfer_id.to_string());
        if !reason.is_empty() {
            query.append_pair("reason", reason);
        }
    }

    let response = http::client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }
    Ok(())
}

/// The thread that reads a served range off the disk, one block at a time, so
/// that no part of a multi-gigabyte body sits in memory and no runtime worker
/// blocks on the disk.
///
/// A file that runs out before the range does is an error rather than a short
/// body: it is no longer the file that was offered.
fn spawn_reader(
    path: PathBuf,
    offset: u64,
    length: u64,
) -> (
    mpsc::Receiver<Bytes>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (blocks, incoming) = mpsc::channel::<Bytes>(READ_QUEUE);
    let reader = tokio::task::spawn_blocking(move || {
        let mut file = fs::File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;

        let mut left = length;
        while left > 0 {
            let mut block = vec![0u8; left.min(READ_BLOCK) as usize];
            let read = file.read(&mut block)?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("{} is shorter than the range asked for", path.display()),
                ));
            }
            block.truncate(read);
            left -= read as u64;

            if blocks.blocking_send(Bytes::from(block)).is_err() {
                // The request is gone; its own error is the one worth reporting.
                break;
            }
        }

        Ok(())
    });

    (incoming, reader)
}

/// A `StreamRequest` asking for a range no file has. The frame is the server's,
/// so a value that cannot be a byte count is a malformed one.
fn negative(field: &str, value: i64) -> StreamError {
    StreamError::Api(ApiFailure::Malformed(format!(
        "the server asked for a {field} of {value}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::DownloadError as _;

    #[test]
    fn the_three_answers_only_a_stream_gives_are_read_off_the_status() {
        assert!(matches!(
            StreamError::from_status(StatusCode::CONFLICT),
            Some(StreamError::OwnerOffline)
        ));
        assert!(matches!(
            StreamError::from_status(StatusCode::GONE),
            Some(StreamError::Declined)
        ));
        assert!(matches!(
            StreamError::from_status(StatusCode::GATEWAY_TIMEOUT),
            Some(StreamError::NoAnswer)
        ));
    }

    #[test]
    fn every_other_status_falls_through_to_the_generic_mapping() {
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::FORBIDDEN,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::RANGE_NOT_SATISFIABLE,
        ] {
            assert!(StreamError::from_status(status).is_none(), "{status}");
        }
    }
}
