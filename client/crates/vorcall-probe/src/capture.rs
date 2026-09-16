//! `--capture-seconds` and `--capture-camera-seconds`: the headless oracles for
//! the screen-capture and camera backends.
//!
//! Nothing here talks to a server, so neither needs an account or a connection.
//! Both also run entirely on their own std thread: every backend brings its own
//! runtime, and the teardown path — dropping the [`Capturer`] on the thread
//! that holds it while the event receiver is still alive, then dropping the
//! receiver — is exactly what these oracles exist to exercise.

use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::channel::mpsc::TryRecvError;
use vorcall_screen::preset::FrameRate;
use vorcall_screen::{
    AudioMode, CameraRequest, CameraSource, CaptureEvent, CaptureRequest, Capturer, SourceId,
    SourceKind, Unavailable,
};

use crate::report::quote;

/// How long the loop waits before looking for another capture event.
const POLL: Duration = Duration::from_millis(2);

/// How long the backend is given to finish stopping after the handle is
/// dropped, with the receiver still alive.
const DRAIN: Duration = Duration::from_secs(1);

/// Fewer frames than this means the backend never really started.
const MIN_FRAMES: u64 = 5;

/// A camera that has produced nothing by now is not going to: a run asking for
/// a long capture must not hang on a device that never delivers a frame.
const FIRST_FRAME_BUDGET: Duration = Duration::from_secs(10);

pub struct Args {
    mode: Mode,
}

enum Mode {
    Screen {
        seconds: u64,
        audio: bool,
        fps: FrameRate,
    },
    Camera {
        seconds: u64,
        device: Option<String>,
        size: (u32, u32),
        fps: FrameRate,
    },
    ListCameras,
}

/// `Ok(None)` means `--help` was asked for.
pub fn parse(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut seconds = None;
    let mut audio = false;
    let mut fps = FrameRate::F30;
    let mut camera_seconds = None;
    let mut device = None;
    let mut camera_size = (640u32, 360u32);
    let mut camera_fps = FrameRate::F30;
    let mut list_cameras = false;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(None),
            "--capture-audio" => audio = true,
            "--capture-seconds" => seconds = Some(crate::number(&flag, &mut args)?),
            "--capture-fps" => fps = frame_rate(&flag, &mut args)?,
            "--capture-camera-seconds" => camera_seconds = Some(crate::number(&flag, &mut args)?),
            "--camera-device" => device = Some(crate::value(&flag, &mut args)?),
            "--camera-size" => {
                camera_size = crate::share::parse_size(&flag, &crate::value(&flag, &mut args)?)?;
            }
            "--camera-fps" => camera_fps = frame_rate(&flag, &mut args)?,
            "--list-cameras" => list_cameras = true,
            "--share-seconds" | "--watch" | "--camera-seconds" | "--watch-camera" => {
                return Err(format!("{flag} needs a server, so it cannot be used here"));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    if list_cameras {
        if seconds.is_some() || camera_seconds.is_some() {
            return Err("--list-cameras takes no capture of its own".to_owned());
        }
        return Ok(Some(Args {
            mode: Mode::ListCameras,
        }));
    }
    if let Some(seconds) = camera_seconds {
        if device.as_deref().is_some_and(str::is_empty) {
            return Err("--camera-device cannot be empty".to_owned());
        }
        if audio {
            return Err("--capture-audio is not something a camera can answer".to_owned());
        }
        return Ok(Some(Args {
            mode: Mode::Camera {
                seconds,
                device,
                size: camera_size,
                fps: camera_fps,
            },
        }));
    }

    let seconds = seconds.ok_or("--capture-seconds is required")?;
    Ok(Some(Args {
        mode: Mode::Screen {
            seconds,
            audio,
            fps,
        },
    }))
}

fn frame_rate(flag: &str, args: &mut impl Iterator<Item = String>) -> Result<FrameRate, String> {
    let raw = crate::number(flag, args)?;
    u32::try_from(raw)
        .ok()
        .and_then(FrameRate::from_hz)
        .ok_or_else(|| format!("{flag} wants 15, 30 or 60, got {raw}"))
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
        .spawn(move || match args.mode {
            Mode::Screen {
                seconds,
                audio,
                fps,
            } => capture(seconds, audio, fps),
            Mode::Camera {
                seconds,
                device,
                size,
                fps,
            } => camera(seconds, device, size, fps),
            Mode::ListCameras => list_cameras(),
        })
        .expect("spawning the capture thread")
        .join()
        .unwrap_or(2)
}

#[derive(Default)]
struct Tally {
    started: bool,
    width: u32,
    height: u32,
    frames: u64,
    bytes: u64,
    first_frame_ms: Option<f64>,
    audio_mode: Option<AudioMode>,
    audio_chunks: u64,
    audio_samples: u64,
    sample_rate: u32,
    channels: u16,
    ended: Option<String>,
}

/// What driving one capture to its end produced, whichever backend it was.
struct Run {
    tally: Tally,
    elapsed: f64,
    stop_ms: f64,
    /// The backend acknowledged the stop — it sent an `Ended` or dropped its
    /// sender — within [`DRAIN`] of the handle going away.
    stopped: bool,
}

/// Drains `incoming` for `seconds`, then drops the handle and drains again.
///
/// `give_up` bounds the wait for the very first frame: past it, a capture that
/// has produced nothing is abandoned rather than run to its full length.
fn drive(
    capturer: Capturer,
    incoming: &mut mpsc::UnboundedReceiver<CaptureEvent>,
    seconds: u64,
    give_up: Option<Duration>,
) -> Run {
    let mut tally = Tally::default();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if let Some(budget) = give_up
            && tally.frames == 0
            && now.duration_since(started) >= budget
        {
            tracing::error!(
                budget_s = budget.as_secs(),
                "no frame arrived within the budget"
            );
            break;
        }
        match incoming.try_recv() {
            Ok(event) => {
                if note(&mut tally, started, event) {
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
    let stop_ms = millis(stopping.elapsed());

    let mut stopped = tally.ended.is_some();
    let drain = Instant::now() + DRAIN;
    while Instant::now() < drain {
        match incoming.try_recv() {
            Ok(event) => {
                if note(&mut tally, started, event) {
                    stopped = true;
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(POLL),
            Err(TryRecvError::Closed) => {
                stopped = true;
                break;
            }
        }
    }

    Run {
        tally,
        elapsed,
        stop_ms,
        stopped,
    }
}

fn capture(seconds: u64, audio: bool, fps: FrameRate) -> i32 {
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
            fps,
            cursor: true,
            audio,
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

    let run = drive(capturer, &mut incoming, seconds, None);
    drop(incoming);
    let tally = run.tally;

    tracing::info!(
        bytes_per_frame = tally.bytes.checked_div(tally.frames).unwrap_or_default(),
        sample_rate = tally.sample_rate,
        channels = tally.channels,
        "capture finished"
    );

    println!(
        "{{\"capture\":{{\"backend\":{},\"width\":{},\"height\":{},\"frames\":{},\"fps\":{:.2},\
\"audio_mode\":{},\"audio_chunks\":{},\"audio_samples\":{},\"ended\":{},\"stop_ms\":{:.2}}}}}",
        quote(backend),
        tally.width,
        tally.height,
        tally.frames,
        per_second(tally.frames, run.elapsed),
        tally.audio_mode.map_or("null".to_owned(), audio_mode),
        tally.audio_chunks,
        tally.audio_samples,
        tally.ended.as_deref().map_or("null".to_owned(), quote),
        run.stop_ms,
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if tally.frames < MIN_FRAMES {
        tracing::error!(frames = tally.frames, "the capture produced almost nothing");
        return 1;
    }
    0
}

fn camera(seconds: u64, device: Option<String>, size: (u32, u32), fps: FrameRate) -> i32 {
    let capabilities = vorcall_screen::camera_capabilities();
    eprintln!(
        "vorcall-probe: camera backend {} (available {}, enumerates {})",
        capabilities.backend, capabilities.available, capabilities.enumerates
    );
    if !capabilities.available {
        eprintln!("vorcall-probe: this platform has no camera backend");
        return 2;
    }

    let source = match device.as_deref().map(pick_camera).transpose() {
        Ok(source) => source,
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}");
            return 2;
        }
    };
    let named = source.as_ref().map(|source| source.name.clone());

    let (events, mut incoming) = mpsc::unbounded();
    let capturer = match vorcall_screen::start_camera(CameraRequest { source, size, fps }, events) {
        Ok(capturer) => capturer,
        Err(error) => {
            eprintln!("vorcall-probe: {error}");
            return 2;
        }
    };
    let backend = capturer.backend();

    let run = drive(capturer, &mut incoming, seconds, Some(FIRST_FRAME_BUDGET));
    drop(incoming);
    let tally = run.tally;

    tracing::info!(
        bytes_per_frame = tally.bytes.checked_div(tally.frames).unwrap_or_default(),
        ended = tally.ended.as_deref().unwrap_or_default(),
        "camera capture finished"
    );

    println!(
        "{{\"camera_capture\":{{\"backend\":{},\"device\":{},\"width\":{},\"height\":{},\
\"frames\":{},\"fps_observed\":{:.2},\"first_frame_ms\":{},\"stopped\":{}}}}}",
        quote(backend),
        named.as_deref().map_or("null".to_owned(), quote),
        tally.width,
        tally.height,
        tally.frames,
        per_second(tally.frames, run.elapsed),
        tally
            .first_frame_ms
            .map_or("null".to_owned(), |ms| format!("{ms:.2}")),
        run.stopped,
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if !tally.started {
        eprintln!("vorcall-probe: the camera never reported that it had started");
        return 1;
    }
    if tally.frames == 0 {
        eprintln!("vorcall-probe: the camera delivered no frame");
        return 1;
    }
    0
}

fn list_cameras() -> i32 {
    let capabilities = vorcall_screen::camera_capabilities();
    let cameras = vorcall_screen::cameras();
    let list: Vec<String> = cameras
        .iter()
        .map(|source| {
            format!(
                "{{\"id\":{},\"name\":{}}}",
                quote(&source.id),
                quote(&source.name)
            )
        })
        .collect();
    println!(
        "{{\"cameras\":[{}],\"camera_capabilities\":{{\"backend\":{},\"available\":{},\
\"enumerates\":{}}}}}",
        list.join(","),
        quote(capabilities.backend),
        capabilities.available,
        capabilities.enumerates,
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());
    0
}

/// The camera `--camera-device` names, by the id [`vorcall_screen::cameras`]
/// reports.
fn pick_camera(id: &str) -> Result<CameraSource, String> {
    vorcall_screen::cameras()
        .into_iter()
        .find(|source| source.id == id)
        .ok_or_else(|| format!("--camera-device {id}: this machine lists no such camera"))
}

/// `true` when the capture ended and there is nothing more to wait for.
fn note(tally: &mut Tally, since: Instant, event: CaptureEvent) -> bool {
    match event {
        CaptureEvent::Started {
            width,
            height,
            audio,
        } => {
            tally.started = true;
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
            tally
                .first_frame_ms
                .get_or_insert_with(|| millis(since.elapsed()));
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

fn per_second(frames: u64, elapsed: f64) -> f64 {
    if elapsed > 0.0 {
        frames as f64 / elapsed
    } else {
        0.0
    }
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_argv(argv: &[&str]) -> Result<Option<Args>, String> {
        parse(argv.iter().map(|arg| (*arg).to_owned()))
    }

    fn screen(argv: &[&str]) -> (u64, bool, FrameRate) {
        match parse_argv(argv).expect("valid").expect("not help").mode {
            Mode::Screen {
                seconds,
                audio,
                fps,
            } => (seconds, audio, fps),
            _ => panic!("not a screen capture"),
        }
    }

    fn camera_args(argv: &[&str]) -> (u64, Option<String>, (u32, u32), FrameRate) {
        match parse_argv(argv).expect("valid").expect("not help").mode {
            Mode::Camera {
                seconds,
                device,
                size,
                fps,
            } => (seconds, device, size, fps),
            _ => panic!("not a camera capture"),
        }
    }

    #[test]
    fn capture_flags_parse_and_reject_the_share_modes() {
        let (seconds, audio, fps) = screen(&[
            "--capture-seconds",
            "5",
            "--capture-audio",
            "--capture-fps",
            "60",
        ]);
        assert_eq!(seconds, 5);
        assert!(audio);
        assert_eq!(fps, FrameRate::F60);

        assert!(parse_argv(&["--capture-seconds", "5", "--watch", "alice"]).is_err());
        assert!(parse_argv(&["--capture-seconds", "5", "--share-seconds", "5"]).is_err());
        assert!(parse_argv(&["--capture-seconds", "5", "--capture-fps", "24"]).is_err());
        assert!(parse_argv(&["--capture-audio"]).is_err());
        assert!(parse_argv(&["--capture-seconds"]).is_err());
        assert!(parse_argv(&["--help"]).expect("valid").is_none());
    }

    #[test]
    fn camera_capture_flags_parse_and_default_to_the_system_camera() {
        let (seconds, device, size, fps) = camera_args(&[
            "--capture-camera-seconds",
            "6",
            "--camera-device",
            "cam-1",
            "--camera-size",
            "1280x720",
            "--camera-fps",
            "15",
        ]);
        assert_eq!(seconds, 6);
        assert_eq!(device.as_deref(), Some("cam-1"));
        assert_eq!(size, (1280, 720));
        assert_eq!(fps, FrameRate::F15);

        let (seconds, device, size, fps) = camera_args(&["--capture-camera-seconds", "4"]);
        assert_eq!(seconds, 4);
        assert_eq!(device, None);
        assert_eq!(size, (640, 360));
        assert_eq!(fps, FrameRate::F30);
    }

    #[test]
    fn camera_capture_rejects_the_server_flags_and_bad_values() {
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--watch", "alice"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--watch-camera", "alice"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--camera-seconds", "5"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--username", "alice"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--camera-device"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--camera-device", ""]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--camera-size", "640x"]).is_err());
        assert!(
            parse_argv(&["--capture-camera-seconds", "5", "--camera-size", "641x360"]).is_err()
        );
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--camera-fps", "24"]).is_err());
        assert!(parse_argv(&["--capture-camera-seconds", "5", "--capture-audio"]).is_err());
    }

    #[test]
    fn list_cameras_stands_alone() {
        assert!(matches!(
            parse_argv(&["--list-cameras"])
                .expect("valid")
                .expect("not help")
                .mode,
            Mode::ListCameras
        ));
        assert!(parse_argv(&["--list-cameras", "--capture-seconds", "5"]).is_err());
        assert!(parse_argv(&["--list-cameras", "--capture-camera-seconds", "5"]).is_err());
    }

    #[test]
    fn observed_rates_and_delays_are_plain_arithmetic() {
        assert!((per_second(150, 5.0) - 30.0).abs() < 1e-9);
        assert!((per_second(0, 5.0)).abs() < 1e-9);
        // A capture that never ran has no elapsed time to divide by.
        assert!((per_second(7, 0.0)).abs() < 1e-9);

        assert!((millis(Duration::from_millis(250)) - 250.0).abs() < 1e-9);
        assert!((millis(Duration::from_micros(1500)) - 1.5).abs() < 1e-9);
        assert!((millis(Duration::ZERO)).abs() < 1e-9);
    }
}
