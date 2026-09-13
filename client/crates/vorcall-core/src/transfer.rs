//! The one resumable ranged download, and the pieces every transfer in this
//! crate reports and writes with.
//!
//! An attachment and a streamed file are fetched exactly alike — a `.part`
//! sibling, a `Range` past what it already holds, a writer thread the socket
//! cannot outrun — and differ only in the request they send and in what they
//! call a failure. Those two are what [`download`] takes from its caller;
//! everything else, the resume matrix above all, lives here.

use std::ffi::OsString;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::RANGE;
use tokio::sync::mpsc;

use crate::http::{self, ApiFailure};

/// The coarsest step a transfer reports, so ten gigabytes are not a report per
/// chunk; below it, one percent of the transfer is the step. Every report costs
/// the app a frame.
pub(crate) const PROGRESS_STEP: u64 = 1 << 20;

/// How far either end of a transfer may run ahead of the other: four chunks
/// between the socket and the disk, in whichever direction.
pub(crate) const TRANSFER_QUEUE: usize = 4;

/// What a caller of [`download`] calls a failure.
///
/// Everything a mapping needs is the error type itself, so it is a trait rather
/// than a closure the call site has to carry: a streamed file answers three
/// statuses of its own and keeps its `io::Error`, an attachment neither.
pub(crate) trait DownloadError: From<ApiFailure> + Sized {
    /// The caller's own reading of a status, taken before the generic mapping.
    /// `None` leaves the status to [`http::failure_from`].
    fn from_status(_status: StatusCode) -> Option<Self> {
        None
    }

    /// A transfer the disk, not the server, put an end to.
    fn disk(action: &str, path: &Path, error: std::io::Error) -> Self;

    /// A thread that wrote or renamed a file panicked: local, never the network.
    fn task(error: tokio::task::JoinError) -> Self;
}

impl DownloadError for ApiFailure {
    fn disk(action: &str, path: &Path, error: std::io::Error) -> Self {
        disk_failure(action, path, &error)
    }

    fn task(error: tokio::task::JoinError) -> Self {
        task_failure(error)
    }
}

/// Fetches one file onto disk at `path`, carrying on from where an earlier try
/// stopped. `progress` is told the bytes on disk of the bytes there are.
///
/// `request` builds the unranged `GET` — the URL and its headers are all a
/// caller decides; the `Range` header, and the second request a `416` needs,
/// are this function's. `short` is the caller's word for a body that ended
/// early, which is the one failure that carries call-site context.
///
/// The bytes land in a `.part` sibling that is renamed into place once the last
/// one has arrived, so `path` never holds half a file; the `.part` is what the
/// next call resumes from, and a failure deliberately leaves it there.
pub(crate) async fn download<E, R, S>(
    path: &Path,
    request: R,
    short: S,
    mut progress: impl FnMut(u64, u64) + Send,
) -> Result<(), E>
where
    E: DownloadError,
    R: Fn() -> Result<reqwest::RequestBuilder, ApiFailure>,
    S: FnOnce(u64, u64) -> E,
{
    let part = part_path(path);
    let held = fs::metadata(&part).map(|file| file.len()).unwrap_or(0);

    let mut response = ranged_get(&request, held).await?;
    if response.status() == StatusCode::RANGE_NOT_SATISFIABLE && held > 0 {
        // What is on disk reaches past the end of what the other side has, so
        // it is no prefix of it either: the only safe move is to fetch again.
        response = ranged_get(&request, 0).await?;
    }

    let status = response.status();
    if let Some(peculiar) = E::from_status(status) {
        return Err(peculiar);
    }
    if !status.is_success() {
        return Err(http::failure_from(response).await.into());
    }

    // Only a 206 answers the range that was asked for. A 200 is the whole file,
    // which makes whatever is on disk stale.
    let resumed = status == StatusCode::PARTIAL_CONTENT && held > 0;
    let start = if resumed { held } else { 0 };
    let expected = response.content_length().map(|length| start + length);

    let (chunks, writer) = spawn_writer(part.clone(), resumed);
    let mut received = start;
    let pumped = pump::<E>(
        &mut response,
        chunks,
        &mut received,
        expected,
        &mut progress,
    )
    .await;

    // The disk error comes first: it is also what closed the channel the pump
    // then failed to send on.
    writer
        .await
        .map_err(E::task)?
        .map_err(|e| E::disk("write", &part, e))?;
    pumped?;

    if let Some(expected) = expected
        && received != expected
    {
        // The `.part` stays: the next call carries on from where this stopped.
        return Err(short(expected, received));
    }

    let destination = path.to_path_buf();
    let renamed = part.clone();
    tokio::task::spawn_blocking(move || fs::rename(&renamed, &destination))
        .await
        .map_err(E::task)?
        .map_err(|e| E::disk("rename", &part, e))?;

    progress(received, received);
    Ok(())
}

/// The `GET`, asking for everything past `held` when there is anything on disk.
/// A non-success answer comes back whole: the caller classifies it.
async fn ranged_get<R>(request: &R, held: u64) -> Result<reqwest::Response, ApiFailure>
where
    R: Fn() -> Result<reqwest::RequestBuilder, ApiFailure>,
{
    let mut builder = request()?;
    if held > 0 {
        builder = builder.header(RANGE, format!("bytes={held}-"));
    }

    builder
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))
}

/// Hands every chunk of the body to the writer, reporting as it goes.
async fn pump<E: DownloadError>(
    response: &mut reqwest::Response,
    chunks: mpsc::Sender<Bytes>,
    received: &mut u64,
    expected: Option<u64>,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), E> {
    let mut steps = Steps::new(expected.unwrap_or(0));

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?
    {
        *received += chunk.len() as u64;
        if chunks.send(chunk).await.is_err() {
            // The writer is gone; its own error is the one worth reporting.
            break;
        }
        if steps.admits(*received) {
            progress(*received, expected.unwrap_or(*received));
        }
    }

    Ok(())
}

/// The thread that owns the partial file. Appending continues a resumed
/// download; anything else starts the file over.
///
/// The channel is bounded so that a disk slower than the link backpressures the
/// socket instead of piling a multi-gigabyte body up in memory, and blocking is
/// what the writer does, so it belongs on a blocking thread and nowhere near a
/// runtime worker.
fn spawn_writer(
    part: PathBuf,
    append: bool,
) -> (
    mpsc::Sender<Bytes>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (chunks, mut incoming) = mpsc::channel::<Bytes>(TRANSFER_QUEUE);
    let writer = tokio::task::spawn_blocking(move || {
        if let Some(parent) = part.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true);
        if append {
            options.append(true);
        } else {
            options.write(true).truncate(true);
        }

        let mut file = BufWriter::new(options.open(&part)?);
        while let Some(chunk) = incoming.blocking_recv() {
            file.write_all(&chunk)?;
        }
        file.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()
    });

    (chunks, writer)
}

/// The sibling a download is written into until it is whole.
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(".part");
    path.with_file_name(name)
}

/// A transfer the disk, not the server, put an end to.
pub(crate) fn disk_failure(action: &str, path: &Path, error: &std::io::Error) -> ApiFailure {
    ApiFailure::Io(format!("cannot {action} {}: {error}", path.display()))
}

/// A thread that read or wrote a file panicked: local, never the network.
pub(crate) fn task_failure(error: tokio::task::JoinError) -> ApiFailure {
    ApiFailure::Io(error.to_string())
}

/// How often a transfer reports. Every report redraws the window, so a step has
/// to be worth looking at: one percent of the transfer, at most
/// [`PROGRESS_STEP`], and the end of the transfer whatever the step.
pub(crate) struct Steps {
    step: u64,
    total: u64,
    last: u64,
}

impl Steps {
    pub(crate) fn new(total: u64) -> Self {
        let step = if total == 0 {
            // Nothing to take a percentage of: a server that declared no length.
            PROGRESS_STEP
        } else {
            (total / 100).clamp(1, PROGRESS_STEP)
        };
        Self {
            step,
            total,
            last: 0,
        }
    }

    pub(crate) fn admits(&mut self, received: u64) -> bool {
        let reached = self.total > 0 && received >= self.total;
        if !reached && received.saturating_sub(self.last) < self.step {
            return false;
        }
        self.last = received;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_report_a_percent_up_to_a_megabyte() {
        // Ten million bytes: a percent of them is a hundred thousand, which is
        // under the ceiling and so is the step.
        let mut steps = Steps::new(10_000_000);
        assert!(!steps.admits(99_999));
        assert!(steps.admits(100_000));
        assert!(!steps.admits(199_999));
        assert!(steps.admits(200_000));

        // A gigabyte: one percent is far past the ceiling, so the ceiling wins.
        let mut steps = Steps::new(1 << 30);
        assert!(!steps.admits(PROGRESS_STEP - 1));
        assert!(steps.admits(PROGRESS_STEP));

        // No declared length: the ceiling is all there is to go by.
        let mut steps = Steps::new(0);
        assert!(!steps.admits(PROGRESS_STEP - 1));
        assert!(steps.admits(PROGRESS_STEP));
    }

    #[test]
    fn the_last_byte_is_always_reported() {
        let mut steps = Steps::new(10_000_000);
        assert!(steps.admits(100_000));
        // Nowhere near a step past the last report, but it is the end.
        assert!(steps.admits(10_000_000));

        // Without a total there is no end to recognise, only the ceiling.
        let mut steps = Steps::new(0);
        assert!(!steps.admits(1));
    }

    #[test]
    fn the_partial_file_is_a_sibling() {
        assert_eq!(
            part_path(Path::new("/tmp/holiday.png")),
            PathBuf::from("/tmp/holiday.png.part")
        );
    }

    #[test]
    fn an_attachment_reads_no_status_of_its_own() {
        // Every status an attachment can get goes through `http::failure_from`.
        for status in [
            StatusCode::CONFLICT,
            StatusCode::GONE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::NOT_FOUND,
        ] {
            assert!(<ApiFailure as DownloadError>::from_status(status).is_none());
        }
    }

    #[test]
    fn a_disk_failure_names_the_action_and_the_path() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let failure = <ApiFailure as DownloadError>::disk("write", Path::new("/tmp/a.part"), error);
        let ApiFailure::Io(message) = failure else {
            panic!("a disk failure is an Io failure");
        };
        assert!(
            message.starts_with("cannot write /tmp/a.part: "),
            "{message}"
        );
    }
}
