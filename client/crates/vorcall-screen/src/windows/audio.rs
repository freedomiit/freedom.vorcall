//! The machine's own playout, so a shared video is shared with its sound.
//!
//! WASAPI process loopback takes everything this process is not playing, which
//! is what keeps the call's own voices out of the share; it needs Windows build
//! 20348 or newer. Anything older falls back to plain loopback through cpal,
//! which cannot leave this process out — the app is told which of the two it
//! got and can say so.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::channel::mpsc::UnboundedSender;
use wasapi::{
    AudioCaptureClient, AudioClient, Direction, Handle, SampleType, StreamMode, WaveFormat,
    initialize_mta,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

use super::send;
use crate::{AudioChunk, AudioMode, CaptureEvent};

/// What the process-loopback client is asked for; WASAPI converts from whatever
/// the endpoint really speaks.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 20 ms, the slice the rest of the client already works in.
const CHUNK_FRAMES: usize = RATE as usize / 50;
/// Device buffer, in the 100 ns units WASAPI counts in: 20 ms.
const BUFFER_HNS: i64 = 200_000;
/// How long the loop waits on WASAPI before looking at the stop flag again.
const WAIT_MS: u32 = 100;
/// The same, for the cpal fallback, whose stream runs on its own thread.
const POLL: Duration = Duration::from_millis(100);
/// How long [`spawn`] waits for the thread to say whether it has audio at all.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// The audio thread's leash. Its stream is open and its [`AudioMode`] is known
/// — which is what [`CaptureEvent::Started`] needs — but it holds every chunk
/// back until this is released, so nothing reaches the app ahead of `Started`.
/// Dropping it instead tells the thread to stand down without ever capturing.
pub(super) struct Release(mpsc::Sender<()>);

impl Release {
    pub(super) fn go(self) {
        let _ = self.0.send(());
    }
}

/// Opens this machine's playout. `None` means there is none to be had; the
/// capture goes ahead without it.
pub(super) fn spawn(
    events: UnboundedSender<CaptureEvent>,
    stop: Arc<AtomicBool>,
) -> Option<(JoinHandle<()>, AudioMode, Release)> {
    let (ready_tx, ready_rx) = mpsc::channel::<AudioMode>();
    let (go_tx, go_rx) = mpsc::channel::<()>();

    let thread = std::thread::Builder::new()
        .name("vorcall-share-audio".to_string())
        .spawn(move || run(&events, &stop, &ready_tx, &go_rx))
        .inspect_err(|error| tracing::warn!(%error, "cannot start the share audio thread"))
        .ok()?;

    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(mode) => Some((thread, mode, Release(go_tx))),
        // Dropping `go_tx` is what tells a thread whose answer came too late to
        // stand down, so nothing ever sends audio the app was not promised.
        Err(_) => {
            tracing::warn!("this machine's audio cannot be shared");
            None
        }
    }
}

fn run(
    events: &UnboundedSender<CaptureEvent>,
    stop: &AtomicBool,
    ready: &mpsc::Sender<AudioMode>,
    go: &mpsc::Receiver<()>,
) {
    if let Err(error) = initialize_mta().ok() {
        tracing::debug!(%error, "COM refused the multi-threaded apartment");
    }

    match Excluded::open() {
        Ok(mut stream) => {
            if ready.send(AudioMode::Excluded).is_ok() && released(go, stop) {
                stream.run(events, stop);
            }
        }
        Err(error) => {
            tracing::debug!(%error, "WASAPI process loopback is unavailable");
            match Playout::open(events.clone()) {
                Ok(stream) => {
                    if ready.send(AudioMode::IncludesOwnPlayout).is_ok() && released(go, stop) {
                        stream.run(stop);
                    }
                }
                Err(error) => tracing::warn!(%error, "cannot capture this machine's audio"),
            }
        }
    }
}

/// Waits for the capture thread to release the stream, which it does only once
/// `Started` has gone out. `false` means no chunk may ever be sent: the capture
/// dropped the [`Release`] because it failed to open, or it is already stopping
/// — and the stop flag is polled so a teardown never waits on this forever.
fn released(go: &mpsc::Receiver<()>, stop: &AtomicBool) -> bool {
    loop {
        match go.recv_timeout(POLL) {
            Ok(()) => return true,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
            }
        }
    }
}

/// Loopback that leaves this process out, which is what keeps the call's own
/// voices from being shared back into it.
struct Excluded {
    client: AudioClient,
    event: Handle,
    capture: AudioCaptureClient,
}

impl Excluded {
    fn open() -> Result<Excluded, String> {
        // SAFETY: reads nothing but the caller's own process id.
        let process = unsafe { GetCurrentProcessId() };
        let mut client = AudioClient::new_application_loopback_client(process, false)
            .map_err(|error| error.to_string())?;
        let format = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            RATE as usize,
            usize::from(CHANNELS),
            None,
        );
        client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: BUFFER_HNS,
                },
            )
            .map_err(|error| error.to_string())?;
        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
        client.start_stream().map_err(|e| e.to_string())?;

        Ok(Excluded {
            client,
            event,
            capture,
        })
    }

    fn run(&mut self, events: &UnboundedSender<CaptureEvent>, stop: &AtomicBool) {
        let bytes = CHUNK_FRAMES * usize::from(CHANNELS) * size_of::<f32>();
        let mut queue: VecDeque<u8> = VecDeque::with_capacity(bytes * 4);

        while !stop.load(Ordering::Relaxed) {
            // A timeout is not a failure: it is how often the stop flag is
            // read on a silent machine.
            if self.event.wait_for_event(WAIT_MS).is_err() {
                continue;
            }
            if let Err(error) = self.capture.read_from_device_to_deque(&mut queue) {
                tracing::warn!(%error, "the shared audio stream stopped");
                return;
            }

            while queue.len() >= bytes {
                let raw: Vec<u8> = queue.drain(..bytes).collect();
                let (samples, _) = raw.as_chunks::<{ size_of::<f32>() }>();
                let interleaved: Vec<f32> =
                    samples.iter().copied().map(f32::from_le_bytes).collect();
                let chunk = AudioChunk {
                    sample_rate: RATE,
                    channels: CHANNELS,
                    interleaved,
                };
                if !send(events, CaptureEvent::Audio(chunk)) {
                    return;
                }
            }
        }
    }
}

impl Drop for Excluded {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

/// Plain loopback on the default output: everything the machine plays, this
/// call included.
struct Playout {
    /// Dropping it stops the stream; nothing else reads it.
    _stream: cpal::Stream,
}

impl Playout {
    fn open(events: UnboundedSender<CaptureEvent>) -> Result<Playout, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let config = device
            .default_output_config()
            .map_err(|error| error.to_string())?
            .config();
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        // An input stream on an output device is how cpal spells WASAPI
        // loopback; it converts to `f32` for us.
        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let chunk = AudioChunk {
                        sample_rate,
                        channels,
                        interleaved: data.to_vec(),
                    };
                    let _ = events.unbounded_send(CaptureEvent::Audio(chunk));
                },
                |error| tracing::warn!(%error, "shared audio stream error"),
                None,
            )
            .map_err(|error| error.to_string())?;
        stream.play().map_err(|error| error.to_string())?;

        Ok(Playout { _stream: stream })
    }

    fn run(&self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(POLL);
        }
    }
}
