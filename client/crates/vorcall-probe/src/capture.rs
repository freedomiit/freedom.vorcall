//! `--capture-seconds`: the headless oracle for the screen-capture backends.
//!
//! Nothing here talks to a server, so it needs no account and no connection.
//! It also runs entirely on its own std thread: every backend brings its own
//! runtime, and the teardown path — dropping the [`Capturer`] on the thread
//! that holds it while the event receiver is still alive, then dropping the
//! receiver — is exactly what this oracle exists to exercise.

use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::channel::mpsc::TryRecvError;
use vorcall_screen::preset::FrameRate;
use vorcall_screen::{
    AudioMode, CaptureEvent, CaptureRequest, Capturer, SourceId, SourceKind, Unavailable,
};

use crate::report::quote;

/// How long the loop waits before looking for another capture event.
const POLL: Duration = Duration::from_millis(2);

/// How long the backend is given to finish stopping after the handle is
/// dropped, with the receiver still alive.
const DRAIN: Duration = Duration::from_secs(1);

/// Fewer frames than this means the backend never really started.
const MIN_FRAMES: u64 = 5;

pub struct Args {
    seconds: u64,
    audio: bool,
    fps: FrameRate,
}

/// `Ok(None)` means `--help` was asked for.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut seconds = None;
    let mut audio = false;
    let mut fps = FrameRate::F30;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(None),
            "--capture-audio" => audio = true,
            "--capture-seconds" => seconds = Some(crate::number(&flag, &mut args)?),
            "--capture-fps" => {
                let raw = crate::number(&flag, &mut args)?;
                fps = u32::try_from(raw)
                    .ok()
                    .and_then(FrameRate::from_hz)
                    .ok_or_else(|| format!("--capture-fps wants 15, 30 or 60, got {raw}"))?;
            }
            "--share-seconds" | "--watch" => {
                return Err(format!("{flag} cannot be used with --capture-seconds"));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    let seconds = seconds.ok_or("--capture-seconds is required")?;
    Ok(Some(Args {
        seconds,
        audio,
        fps,
    }))
}

/// Runs the capture on a thread of its own and returns the process exit code.
pub fn run(argv: Vec<String>) -> i32 {
    let args = match parse(argv.into_iter()) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{}", crate::USAGE);
            return 0;
        }
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}\n\n{}", crate::USAGE);
            return 2;
        }
    };

    std::thread::Builder::new()
        .name("probe-capture".to_owned())
        .spawn(move || capture(args))
        .expect("spawning the capture thread")
        .join()
        .unwrap_or(2)
}

#[derive(Default)]
struct Tally {
    width: u32,
    height: u32,
    frames: u64,
    bytes: u64,
    audio_mode: Option<AudioMode>,
    audio_chunks: u64,
    audio_samples: u64,
    sample_rate: u32,
    channels: u16,
    ended: Option<String>,
}

fn capture(args: Args) -> i32 {
    let capabilities = vorcall_screen::capabilities();
    eprintln!(
        "vorcall-probe: capture backend {} (portal picker {}, windows {}, audio {})",
        capabilities.backend, capabilities.portal_picker, capabilities.windows, capabilities.audio
    );

    // A portal backend has its own picker; everywhere else the probe has to
    // name a display itself.
    let source = if capabilities.portal_picker {
        None
    } else {
        match first_display() {
            Ok(source) => Some(source),
            Err(error) => {
                eprintln!("vorcall-probe: {error}");
                return 2;
            }
        }
    };

    let (events, mut incoming) = mpsc::unbounded();
    let capturer = match Capturer::start(
        CaptureRequest {
            source,
            fps: args.fps,
            cursor: true,
            audio: args.audio,
            max_size: None,
        },
        events,
    ) {
        Ok(capturer) => capturer,
        Err(error) => {
            eprintln!("vorcall-probe: {error}");
            return 2;
        }
    };
    let backend = capturer.backend();

    let mut tally = Tally::default();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    while Instant::now() < deadline {
        match incoming.try_recv() {
            Ok(event) => {
                if note(&mut tally, event) {
                    break;
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(POLL),
            // The backend dropped its sender without an Ended: it is gone.
            Err(TryRecvError::Closed) => break,
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    // The crash this oracle watches for lives here: the handle is dropped on
    // the thread that owns it, the receiver outlives it, and only then goes.
    let stopping = Instant::now();
    drop(capturer);
    let stop_ms = stopping.elapsed().as_secs_f64() * 1000.0;

    let drain = Instant::now() + DRAIN;
    while Instant::now() < drain {
        match incoming.try_recv() {
            Ok(event) => {
                note(&mut tally, event);
            }
            Err(TryRecvError::Empty) => std::thread::sleep(POLL),
            Err(TryRecvError::Closed) => break,
        }
    }
    drop(incoming);

    tracing::info!(
        bytes_per_frame = tally.bytes.checked_div(tally.frames).unwrap_or_default(),
        sample_rate = tally.sample_rate,
        channels = tally.channels,
        "capture finished"
    );

    let fps = if elapsed > 0.0 {
        tally.frames as f64 / elapsed
    } else {
        0.0
    };
    println!(
        "{{\"capture\":{{\"backend\":{},\"width\":{},\"height\":{},\"frames\":{},\"fps\":{:.2},\
\"audio_mode\":{},\"audio_chunks\":{},\"audio_samples\":{},\"ended\":{},\"stop_ms\":{:.2}}}}}",
        quote(backend),
        tally.width,
        tally.height,
        tally.frames,
        fps,
        tally.audio_mode.map_or("null".to_owned(), audio_mode),
        tally.audio_chunks,
        tally.audio_samples,
        tally.ended.as_deref().map_or("null".to_owned(), quote),
        stop_ms,
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if tally.frames < MIN_FRAMES {
        tracing::error!(frames = tally.frames, "the capture produced almost nothing");
        return 1;
    }
    0
}

/// `true` when the capture ended and there is nothing more to wait for.
fn note(tally: &mut Tally, event: CaptureEvent) -> bool {
    match event {
        CaptureEvent::Started {
            width,
            height,
            audio,
        } => {
            tally.width = width;
            tally.height = height;
            tally.audio_mode = audio;
            tracing::info!(width, height, ?audio, "the capture started");
        }
        CaptureEvent::Video(frame) => {
            tally.frames += 1;
            tally.width = frame.width;
            tally.height = frame.height;
            tally.bytes += frame.bgra.len() as u64;
        }
        CaptureEvent::Audio(chunk) => {
            tally.audio_chunks += 1;
            tally.audio_samples += chunk.interleaved.len() as u64;
            tally.sample_rate = chunk.sample_rate;
            tally.channels = chunk.channels;
        }
        CaptureEvent::Ended(reason) => {
            tracing::warn!(%reason, "the backend ended the capture");
            tally.ended = Some(reason);
            return true;
        }
    }
    false
}

fn first_display() -> Result<SourceId, Unavailable> {
    let sources = vorcall_screen::enumerate()?;
    sources
        .into_iter()
        .find(|source| source.kind == SourceKind::Display)
        .map(|source| source.id)
        .ok_or_else(|| Unavailable::Unsupported("this machine lists no display".to_owned()))
}

fn audio_mode(mode: AudioMode) -> String {
    match mode {
        AudioMode::Excluded => quote("excluded"),
        AudioMode::IncludesOwnPlayout => quote("own_playout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_argv(argv: &[&str]) -> Result<Option<Args>, String> {
        parse(argv.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn capture_flags_parse_and_reject_the_share_modes() {
        let args = parse_argv(&[
            "--capture-seconds",
            "5",
            "--capture-audio",
            "--capture-fps",
            "60",
        ])
        .expect("valid")
        .expect("not help");
        assert_eq!(args.seconds, 5);
        assert!(args.audio);
        assert_eq!(args.fps, FrameRate::F60);

        assert!(parse_argv(&["--capture-seconds", "5", "--watch", "alice"]).is_err());
        assert!(parse_argv(&["--capture-seconds", "5", "--share-seconds", "5"]).is_err());
        assert!(parse_argv(&["--capture-seconds", "5", "--capture-fps", "24"]).is_err());
        assert!(parse_argv(&["--capture-audio"]).is_err());
        assert!(parse_argv(&["--capture-seconds"]).is_err());
        assert!(parse_argv(&["--help"]).expect("valid").is_none());
    }
}
