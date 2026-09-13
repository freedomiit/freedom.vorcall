//! Attachments: what a file is sent as, how large one may be, and the REST
//! calls that put one on the server and get it back.
//!
//! Any type at all, and up to [`MAX_BYTES`] — far more than belongs in memory,
//! so a file streams up from disk and back down to it, and only a preview ever
//! holds one whole. Pictures are still told apart by their magic number
//! ([`sniff_image`]): that is what the app may draw, and what [`crate::images`]
//! accepts.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use bytes::Bytes;
use prost::Message as _;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use reqwest::{Body, StatusCode};
use tokio::sync::mpsc;
use vorcall_proto::v1::Attachment;

use crate::connection::Blob;
use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};
use crate::transfer::{self, Steps, TRANSFER_QUEUE, disk_failure, task_failure};

/// `PROTOCOL.md` § Limits: the largest body the upload endpoint accepts.
pub const MAX_BYTES: u64 = 2 << 30;

/// `PROTOCOL.md` § Limits: attachment ids a single `SendMessage` may carry.
pub const MAX_PER_MESSAGE: usize = 4;

/// The optional header carrying the original file name on an upload.
pub const FILENAME_HEADER: &str = "X-Vorcall-Filename";

/// What a file nothing can name a type for is sent as.
pub const DEFAULT_TYPE: &str = "application/octet-stream";

/// The most of a file name the header carries. The server keeps less still, and
/// a name is metadata — never worth a refused request.
const MAX_FILENAME_BYTES: usize = 255;

/// How much of a file is read at a time on the way up.
const READ_CHUNK: usize = 256 * 1024;

/// The content type of these bytes read from their magic number. `None` when
/// they are not one of the four picture types the app can draw.
///
/// Attachments are no longer sniffed — the server stores whatever type it is
/// told — but [`crate::images`] still is, on both sides.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
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

/// The type to send a file as: its extension when that names one, the magic
/// number of `bytes` when the caller has already read some, and
/// [`DEFAULT_TYPE`] otherwise.
pub fn guess_type(path: &Path, bytes: Option<&[u8]>) -> String {
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str())
        && let Some(known) = type_for_extension(&extension.to_ascii_lowercase())
    {
        return known.to_owned();
    }

    if let Some(bytes) = bytes
        && let Some(sniffed) = sniff_image(bytes)
    {
        return sniffed.to_owned();
    }

    DEFAULT_TYPE.to_owned()
}

/// The everyday types, spelled as the wire spells them. A file outside this
/// table is not refused, it just travels as [`DEFAULT_TYPE`].
fn type_for_extension(extension: &str) -> Option<&'static str> {
    let content_type = match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => return None,
    };
    Some(content_type)
}

/// Uploads bytes already in memory — a paste, a screenshot — to `channel_id`.
pub async fn upload_bytes(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    file_name: &str,
    content_type: &str,
    bytes: Blob,
) -> Result<Attachment, ApiFailure> {
    let size = bytes.len() as u64;
    if size > MAX_BYTES {
        return Err(too_large());
    }

    // The body reads the shared buffer rather than a copy of it: `Bytes` keeps
    // the blob alive for as long as the request needs it.
    send_upload(
        endpoints,
        access_token,
        channel_id,
        file_name,
        content_type,
        Body::from(Bytes::from_owner(bytes)),
        size,
    )
    .await
}

/// Uploads the file at `path` to `channel_id`, answering with the stored
/// attachment. `progress` is told the bytes handed to the socket of the bytes
/// there are.
///
/// The file is never held whole: a reader thread hands chunks to the request
/// body as fast as the socket takes them, which is what lets an attachment be
/// gigabytes. `progress` outlives this call because the body does — the request
/// owns it until the last chunk is sent.
pub async fn upload_from_path(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    path: &Path,
    content_type: &str,
    progress: impl FnMut(u64, u64) + Send + 'static,
) -> Result<Attachment, ApiFailure> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();

    let source = path.to_path_buf();
    let (file, size) = tokio::task::spawn_blocking(move || open_capped(&source))
        .await
        .map_err(task_failure)??;

    send_upload(
        endpoints,
        access_token,
        channel_id,
        &file_name,
        content_type,
        Body::wrap_stream(read_stream(file, size, progress)),
        size,
    )
    .await
}

/// The one request both upload paths make: a raw body, not protobuf, so it is
/// built here rather than going through [`http::post_proto`].
async fn send_upload(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    file_name: &str,
    content_type: &str,
    body: Body,
    size: u64,
) -> Result<Attachment, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/attachments")?;
    url.query_pairs_mut()
        .append_pair("channel", &channel_id.to_string());

    // A type the caller made up must not cost the upload: an unreadable one is
    // what the server treats as [`DEFAULT_TYPE`] anyway.
    let content_type = HeaderValue::from_str(content_type)
        .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_TYPE));

    let mut request = http::upload_client()?
        .post(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .header(CONTENT_TYPE, content_type)
        // The server charges the quota against the declared length and answers
        // 411 without one, so it is stated rather than left to the body.
        .header(CONTENT_LENGTH, size)
        .body(body);
    if let Some(name) = filename_header(file_name) {
        request = request.header(FILENAME_HEADER, name);
    }

    let response = request.send().await.map_err(|e| {
        // A body error on the way up is the reader thread's: the disk gave out,
        // not the network.
        if e.is_body() {
            ApiFailure::Io(e.to_string())
        } else {
            ApiFailure::Transport(e.to_string())
        }
    })?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    Attachment::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))
}

/// The file as a body: a thread reads it a chunk at a time into a channel the
/// request drains, so the socket sets the pace and nothing but the chunks in
/// flight is ever in memory.
fn read_stream(
    mut file: File,
    size: u64,
    progress: impl FnMut(u64, u64) + Send + 'static,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let (chunks, incoming) = mpsc::channel::<std::io::Result<Bytes>>(TRANSFER_QUEUE);
    tokio::task::spawn_blocking(move || {
        loop {
            let mut buffer = vec![0u8; READ_CHUNK];
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.truncate(read);
                    if chunks.blocking_send(Ok(Bytes::from(buffer))).is_err() {
                        // The request is gone; so is any reason to keep reading.
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = chunks.blocking_send(Err(e));
                    break;
                }
            }
        }
    });

    futures::stream::unfold(
        (incoming, 0u64, Steps::new(size), progress),
        move |(mut incoming, mut sent, mut steps, mut progress)| async move {
            let Some(chunk) = incoming.recv().await else {
                // The end of the file, and the report the steps held back.
                progress(sent, size);
                return None;
            };

            if let Ok(bytes) = &chunk {
                sent += bytes.len() as u64;
                if steps.admits(sent) {
                    progress(sent, size);
                }
            }

            Some((chunk, (incoming, sent, steps, progress)))
        },
    )
}

/// Fetches one attachment into memory, for a preview or a thumbnail.
///
/// `max_bytes` is what the caller is prepared to hold: an attachment may now be
/// gigabytes, and nothing that draws one wants it all. The file itself goes
/// through [`download_to_path`].
pub async fn download(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
    max_bytes: u64,
) -> Result<Vec<u8>, ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/attachments/{id}"))?;

    // A large body does not fit the shared client's total timeout.
    let mut response = http::download_client()?
        .get(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    let declared = response.content_length();
    if let Some(length) = declared
        && length > max_bytes
    {
        return Err(over_budget(id, max_bytes));
    }

    let mut bytes = Vec::with_capacity(declared.unwrap_or(0).min(max_bytes) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(over_budget(id, max_bytes));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

/// Fetches one attachment onto disk at `path`, carrying on from where an
/// earlier try stopped. `progress` is told the bytes on disk of the bytes there
/// are.
///
/// The bytes land in a `.part` sibling that is renamed into place once the last
/// one has arrived, so `path` never holds half a file; the `.part` is what the
/// next call resumes from.
pub async fn download_to_path(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
    path: &Path,
    progress: impl FnMut(u64, u64) + Send,
) -> Result<(), ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/attachments/{id}"))?;

    transfer::download(
        path,
        || {
            Ok(http::download_client()?
                .get(url.clone())
                .header(http::KEY_HEADER, &endpoints.key)
                .header(AUTHORIZATION, http::bearer(access_token)))
        },
        |expected, received| {
            ApiFailure::Transport(format!(
                "attachment {id} ended after {received} of {expected} bytes"
            ))
        },
        progress,
    )
    .await
}

/// The open file and the length the request declares, refused before a byte of
/// it is read when it is over the ceiling the server would refuse it at anyway.
fn open_capped(path: &Path) -> Result<(File, u64), ApiFailure> {
    let file = File::open(path).map_err(|e| disk_failure("read", path, &e))?;
    let size = file
        .metadata()
        .map_err(|e| disk_failure("read", path, &e))?
        .len();
    if size > MAX_BYTES {
        return Err(too_large());
    }

    Ok((file, size))
}

/// The `X-Vorcall-Filename` value for `file_name`, or `None` when nothing worth
/// sending is left of it.
///
/// A header value carries any byte from 0x20 up but no control character, so a
/// name's UTF-8 travels as it is — Kestrel reads request headers as UTF-8 — and
/// only what a header cannot hold is dropped. The name is decoration: it must
/// never be what fails an upload.
fn filename_header(file_name: &str) -> Option<HeaderValue> {
    let mut cleaned = String::with_capacity(file_name.len().min(MAX_FILENAME_BYTES));
    for character in file_name.trim().chars() {
        if character.is_control() {
            continue;
        }
        if cleaned.len() + character.len_utf8() > MAX_FILENAME_BYTES {
            break;
        }
        cleaned.push(character);
    }

    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }

    HeaderValue::from_bytes(cleaned.as_bytes()).ok()
}

/// What the server answers an upload it will not take, said here so a file that
/// cannot go anywhere is not read into memory first.
fn too_large() -> ApiFailure {
    ApiFailure::Status(
        StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
        "attachments must be 2 GiB or smaller".to_owned(),
    )
}

/// The body is larger than the caller said it could hold.
fn over_budget(id: i64, max_bytes: u64) -> ApiFailure {
    ApiFailure::Malformed(format!(
        "attachment {id} is larger than the {max_bytes} bytes asked for"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_a_png_header() {
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
        assert_eq!(sniff_image(&bytes), Some("image/png"));
    }

    #[test]
    fn sniffs_a_jpeg_header() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(sniff_image(&bytes), Some("image/jpeg"));
    }

    #[test]
    fn sniffs_both_gif_headers() {
        assert_eq!(sniff_image(b"GIF87a...."), Some("image/gif"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("image/gif"));
    }

    #[test]
    fn sniffs_a_webp_header() {
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff_image(&bytes), Some("image/webp"));
    }

    #[test]
    fn webp_needs_both_tags() {
        let mut riff_only = Vec::from(*b"RIFF");
        riff_only.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        riff_only.extend_from_slice(b"AVI LIST");
        assert_eq!(sniff_image(&riff_only), None);

        // The WEBP tag alone, without the RIFF container, is not a webp file.
        assert_eq!(sniff_image(b"XXXX\0\0\0\0WEBP"), None);
    }

    #[test]
    fn rejects_a_bmp_header_and_empty_input() {
        assert_eq!(sniff_image(b"BM\x36\x00\x00\x00"), None);
        assert_eq!(sniff_image(&[]), None);
    }

    #[test]
    fn reads_the_type_off_the_extension() {
        for (name, expected) in [
            ("holiday.PNG", "image/png"),
            ("holiday.jpeg", "image/jpeg"),
            ("holiday.jpg", "image/jpeg"),
            ("loop.gif", "image/gif"),
            ("sticker.webp", "image/webp"),
            ("contract.pdf", "application/pdf"),
            ("logs.zip", "application/zip"),
            ("dump.gz", "application/gzip"),
            ("bundle.tar", "application/x-tar"),
            ("notes.txt", "text/plain"),
            ("README.md", "text/markdown"),
            ("rows.csv", "text/csv"),
            ("config.json", "application/json"),
            ("feed.xml", "application/xml"),
            ("clip.mp4", "video/mp4"),
            ("clip.webm", "video/webm"),
            ("clip.mkv", "video/x-matroska"),
            ("song.mp3", "audio/mpeg"),
            ("song.flac", "audio/flac"),
            ("song.wav", "audio/wav"),
            ("song.ogg", "audio/ogg"),
            ("old.doc", "application/msword"),
            ("sheet.xls", "application/vnd.ms-excel"),
            ("deck.ppt", "application/vnd.ms-powerpoint"),
        ] {
            assert_eq!(guess_type(Path::new(name), None), expected, "{name}");
        }

        assert!(
            guess_type(Path::new("report.docx"), None).ends_with("wordprocessingml.document"),
            "a .docx is a word document"
        );
    }

    #[test]
    fn falls_back_to_the_magic_number_then_to_octet_stream() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

        // An extension nothing knows, but bytes that speak for themselves.
        assert_eq!(guess_type(Path::new("clipboard"), Some(&png)), "image/png");
        assert_eq!(guess_type(Path::new("archive.tar.zst"), None), DEFAULT_TYPE);
        assert_eq!(guess_type(Path::new("no-extension"), None), DEFAULT_TYPE);
        assert_eq!(
            guess_type(Path::new("junk.bin"), Some(b"not a picture")),
            DEFAULT_TYPE
        );

        // The extension is read first: a mislabelled file still travels as what
        // it is named, which is what the server stores and the app shows.
        assert_eq!(guess_type(Path::new("notes.txt"), Some(&png)), "text/plain");
    }

    #[test]
    fn a_non_ascii_name_travels_as_its_own_utf8() {
        let value = filename_header("café.txt").expect("a header value");
        assert_eq!(value.as_bytes(), "café.txt".as_bytes());
    }

    #[test]
    fn control_characters_are_dropped_rather_than_the_name() {
        let value = filename_header("re\nport\u{7f}\t.txt").expect("a header value");
        assert_eq!(value.as_bytes(), b"report.txt");
    }

    #[test]
    fn a_name_with_nothing_in_it_sends_no_header() {
        assert!(filename_header("").is_none());
        assert!(filename_header("   ").is_none());
        assert!(filename_header("\u{0}\u{1}").is_none());
    }

    #[test]
    fn a_long_name_is_cut_on_a_character_boundary() {
        let name = format!("{}é.txt", "a".repeat(MAX_FILENAME_BYTES - 1));
        let value = filename_header(&name).expect("a header value");

        assert!(value.as_bytes().len() <= MAX_FILENAME_BYTES);
        assert_eq!(
            std::str::from_utf8(value.as_bytes()).expect("still text"),
            "a".repeat(MAX_FILENAME_BYTES - 1)
        );
    }
}
