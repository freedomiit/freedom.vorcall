//! A headless voice client: signs in, joins voice, sends a tone, listens, and
//! prints one JSON line about what came back.
//!
//! Two probes in two terminals are the end-to-end oracle for the voice path,
//! locally and against production. Nothing here touches an audio device: the
//! source is a synthesized sine and the sink is a level meter.

mod capture;
mod report;
mod share;
mod update_cmd;

use std::collections::HashMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use tokio::time::{MissedTickBehavior, interval, sleep_until, timeout, timeout_at};
use tracing_subscriber::EnvFilter;
use vorcall_core::connection::DisconnectReason;
use vorcall_core::{Channel, ChannelKind, Command, Event, ServerSnapshot, Session};
use vorcall_screen::preset::FrameRate;
use vorcall_voice::codec::Encoder;
use vorcall_voice::tone::{Tone, rms};
use vorcall_voice::{
    FRAME_MS, FRAME_SAMPLES, FrameSender, GateDecision, Link, MediaConfig, MediaEngine, NoiseGate,
    STEREO_FRAME_SAMPLES,
};

use report::{PeerReport, Report, Rtt, ShareReport, SpeakingEvent, WatchReport};

/// Peaks at 0.3, so a clean frame measures ≈0.21 and silence or concealment
/// stays far below the threshold below.
const DEFAULT_TONE_AMPLITUDE: f32 = 0.3;

/// Anything above this is the tone rather than silence or packet loss
/// concealment fading out.
const TONE_RMS: f32 = 0.02;

/// Opus at 48 kbit/s over 20 ms frames never comes near this.
const MAX_PACKET: usize = 512;

/// Sign-in, WebSocket, channel join and the voice handshake all fit in this.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the sharer waits for the server to announce its own share.
const SHARE_START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a watcher waits for the sharer to appear and the server to confirm
/// the watch, measured from the moment voice came up.
const WATCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Under `--expect-video`: fewer pictures than this is a broken share path
/// rather than a slow start.
const MIN_PICTURES: u64 = 5;

const USAGE: &str = "\
vorcall-probe --username U (--password P | env VORCALL_PROBE_PASSWORD) [options]

Options:
  --username <name>      account to sign in as (required)
  --password <secret>    password; defaults to $VORCALL_PROBE_PASSWORD
  --channel <id>         voice channel id to join (default: the voice channel
                         named General, else the first voice channel)
  --channel-name <name>  voice channel to join by name, case-insensitive
                         (excludes --channel)
  --send-seconds <n>     seconds of tone to send (default: 10)
  --listen-seconds <n>   seconds to keep receiving, >= --send-seconds (default: 12)
  --tone-hz <hz>         tone frequency (default: 440)
  --tone-amplitude <a>   tone peak, 0..1 (default: 0.3); 0 is digital silence
  --vad                  run every frame through the noise gate before
                         encoding; gated frames are never sent
  --vad-threshold <db>   gate open threshold in dBFS (default: -45), only
                         meaningful with --vad
  --share-seconds <n>    seconds of synthetic screen share to send; needs
                         <= --listen-seconds, and excludes --watch
  --share-audio          send a stereo tone as the share's own audio
  --share-size <WxH>     share picture size, both even (default: 1280x720)
  --share-fps <n>        share frame rate: 15, 30 or 60 (default: 30)
  --encode-threads <n>   H.264 slice threads to ask for (default: 1)
  --bitrate-kbps <n>     share bitrate (default: the preset table's value for
                         the size and frame rate)
  --watch <name>         watch that user's share and decode it
  --capture-seconds <n>  capture this machine's screen for n seconds and print
                         what the backend produced; needs no server and no
                         --username, and excludes --share-seconds and --watch
  --capture-audio        ask the capture for this machine's audio as well
  --capture-fps <n>      capture frame rate: 15, 30 or 60 (default: 30)
  --expect-peer          exit 1 unless at least 1.00 s of tone was heard
  --expect-silence       exit 1 if any audio frame was sent
  --expect-video         exit 1 unless at least 5 pictures decoded
  --expect-share-audio   exit 1 unless at least 1.00 s of share tone was heard
  --help                 print this help

Voice-activation oracle: `--vad --tone-amplitude 0 --expect-silence` must exit
0 with \"frames_sent\":0, while `--vad --tone-amplitude 0.3 --expect-peer`
alongside a second probe must hear the tone. Without --vad, --tone-amplitude 0
still sends silent frames, so --expect-silence then exits 1 — that is the
negative control.

Screen-share oracle, two terminals against the same voice channel:

  vorcall-probe --username alice --share-seconds 10 --share-audio \
                --listen-seconds 14
  vorcall-probe --username bob --watch alice --expect-video \
                --expect-share-audio --listen-seconds 14

Capture oracle, no server and no account; on Linux the portal asks which
screen to hand over, so someone has to answer the dialog:

  vorcall-probe --capture-seconds 5 --capture-audio

The server URL and key come from VORCALL_SERVER_URL and VORCALL_SERVER_KEY,
with the values baked in at build time as fallbacks.

Prints one JSON line on stdout; logs go to stderr.

Exit codes: 0 ran, 1 --expect-silence sent audio, --expect-peer heard nothing,
--expect-video saw too few pictures, --expect-share-audio heard no share tone,
the watch was never confirmed, or --capture-seconds produced fewer than 5
frames; 2 usage, sign-in, connection, media or capture failure.

Update subcommands:
  vorcall-probe check-update --username U [--password P] --platform ID
                             --out PATH [--pubkey HEX ...] [--no-download]
      Signs in, fetches and verifies the release manifest and, unless
      --no-download, downloads and hash-checks the asset for --platform into
      --out. Prints one JSON object. Without --pubkey the baked-in keys are
      used. Exit 0 checked, 1 the manifest or the download was refused (the
      JSON carries \"error\" and \"stage\"), 2 usage, sign-in or transport failure.

  vorcall-probe apply-update --file PATH
      Swaps PATH over this binary and starts it again. The relaunched process
      prints {\"relaunched\":true,...} and exits 0.";

/// Which voice channel the run joins, resolved against the server snapshot.
enum ChannelSelect {
    Id(i64),
    Name(String),
    /// The voice channel named `General`, else the first voice channel.
    Default,
}

struct Args {
    username: String,
    password: String,
    channel: ChannelSelect,
    send_seconds: u64,
    listen_seconds: u64,
    tone_hz: f32,
    tone_amplitude: f32,
    vad: bool,
    vad_threshold: f32,
    share_seconds: Option<u64>,
    share_audio: bool,
    share_size: (u32, u32),
    share_fps: FrameRate,
    encode_threads: u16,
    bitrate_kbps: Option<u32>,
    watch: Option<String>,
    expect_peer: bool,
    expect_silence: bool,
    expect_video: bool,
    expect_share_audio: bool,
}

#[tokio::main]
async fn main() {
    // The relaunched process must answer before anything else runs: it is the
    // proof the swap worked, and its argv is whatever `apply-update` was given.
    if std::env::var_os("VORCALL_RELAUNCHED").is_some() {
        let exe = std::env::current_exe().unwrap_or_default();
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        println!(
            "{}",
            serde_json::json!({
                "relaunched": true,
                "exe": exe.display().to_string(),
                "version": vorcall_core::update::Version::current().to_string(),
            })
        );
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    // reqwest is built with `rustls-no-provider`; without this it panics on the
    // first request. An Err only means someone already installed a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("check-update") => std::process::exit(update_cmd::check_update(argv).await),
        Some("apply-update") => std::process::exit(update_cmd::apply_update(argv)),
        _ => {}
    }

    // The capture oracle signs in to nothing and every backend brings its own
    // runtime, so it never touches this one.
    if argv.iter().any(|arg| arg == "--capture-seconds") {
        std::process::exit(capture::run(argv));
    }

    let args = match parse_args(argv.into_iter()) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    std::process::exit(probe(args).await);
}

async fn probe(args: Args) -> i32 {
    let endpoints = match vorcall_core::endpoints::resolve() {
        Ok(endpoints) => endpoints,
        Err(e) => {
            eprintln!("vorcall-probe: {e:#}");
            return 2;
        }
    };
    tracing::info!(?endpoints, "resolved endpoints");

    let session = match vorcall_core::auth::login(&endpoints, &args.username, &args.password).await
    {
        Ok(session) => session,
        Err(e) => {
            eprintln!("vorcall-probe: sign-in failed: {e}");
            return 2;
        }
    };

    let (mut commands, command_rx) = mpsc::channel::<Command>(32);
    let (event_tx, mut events) = mpsc::channel::<Event>(64);
    let self_id = session.user_id;
    let username = session.username.clone();
    tokio::spawn(connection_loop(
        endpoints.clone(),
        session,
        command_rx,
        event_tx,
    ));

    let mut names: HashMap<u32, (i64, String)> = HashMap::new();
    let share_flow = args.share_seconds.is_some() || args.watch.is_some();
    let ready = match wait_for_voice(
        &mut commands,
        &mut events,
        &mut names,
        &args.channel,
        share_flow,
    )
    .await
    {
        Ok(ready) => ready,
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}");
            return 2;
        }
    };

    let host = if ready.host.is_empty() {
        endpoints.host()
    } else {
        ready.host.clone()
    };
    let engine = match MediaEngine::connect(MediaConfig {
        host,
        port: ready.port,
        key: ready.key,
        ssrc: ready.ssrc,
    })
    .await
    {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("vorcall-probe: media engine failed: {e}");
            return 2;
        }
    };
    let started = Instant::now();
    tracing::info!(
        ssrc = ready.ssrc,
        channel_id = ready.channel_id,
        channel = %ready.channel_name,
        "voice connected"
    );

    let plan = SendPlan {
        hz: args.tone_hz,
        amplitude: args.tone_amplitude,
        gate: args.vad.then(|| NoiseGate::new(args.vad_threshold)),
    };
    let sending = tokio::task::spawn(send_tone(
        engine.sender(),
        plan,
        args.send_seconds * 1000 / FRAME_MS,
    ));

    let mut sharing = None;
    if let Some(seconds) = args.share_seconds {
        if let Err(reason) = start_share(
            &mut commands,
            &mut events,
            &mut names,
            ready.channel_id,
            self_id,
            args.share_audio,
        )
        .await
        {
            eprintln!("vorcall-probe: {reason}");
            return 2;
        }
        let bitrate_kbps = args
            .bitrate_kbps
            .unwrap_or_else(|| share::default_bitrate_kbps(args.share_size, args.share_fps));
        sharing = Some(share::start_share(
            engine.sender(),
            share::SharePlan {
                width: args.share_size.0,
                height: args.share_size.1,
                fps: args.share_fps,
                bitrate_kbps,
                threads: args.encode_threads,
                seconds,
                audio: args.share_audio,
            },
        ));
    }

    let mut watching = args
        .watch
        .clone()
        .map(|user| share::WatchPlan::new(user, ready.channel_id, started + WATCH_TIMEOUT));

    let heard = listen(
        &engine,
        &mut commands,
        &mut events,
        &mut names,
        watching.as_mut(),
        started + Duration::from_secs(args.listen_seconds),
    )
    .await;

    let stats = engine.stats();

    let share = sharing.map(|handles| {
        let outcome = handles.join();
        ShareReport {
            frames_encoded: outcome.frames_encoded,
            keyframes: outcome.keyframes,
            keyframe_requests: outcome.keyframe_requests,
            encode_fps: outcome.encode_fps(),
            kbps: outcome.kbps(),
            watchers_max: heard.watchers_max,
            bytes: outcome.bytes,
            threads: outcome.threads,
            skipped: outcome.skipped,
            send_failures: outcome.send_failures,
        }
    });
    if args.share_seconds.is_some() {
        let _ = commands
            .send(Command::StopShare {
                channel_id: ready.channel_id,
            })
            .await;
    }

    let mut watch_confirmed = true;
    let watch = watching.map(|plan| {
        watch_confirmed = plan.confirmed();
        let (user, user_id) = (plan.user.clone(), plan.user_id().unwrap_or_default());
        let decoded = plan.finish();
        WatchReport {
            user,
            user_id,
            pictures: decoded.pictures,
            keyframes: decoded.keyframes,
            dropped: stats.video.dropped,
            decode_errors: decoded.decode_errors,
            first_picture_ms: decoded.first_picture_ms,
            width: decoded.width,
            height: decoded.height,
            decode_fps: decoded.decode_fps(),
            keyframe_requests_sent: stats.video.keyframe_requests,
            share_tone_frames: heard.share_tone_frames,
        }
    });
    if watch.is_some() {
        let _ = commands
            .send(Command::UnwatchShare {
                channel_id: ready.channel_id,
            })
            .await;
    }
    let sent = sending.await.unwrap_or_default();
    tracing::debug!(
        frames_sent = sent.sent,
        send_failures = sent.failures,
        frames_gated = sent.gated,
        "sender finished"
    );

    let _ = commands
        .send(Command::LeaveVoice {
            channel_id: ready.channel_id,
        })
        .await;
    let _ = timeout(Duration::from_secs(1), async {
        while let Some(event) = events.next().await {
            if let Event::VoiceMemberLeft { user_id, .. } = event
                && user_id == self_id
            {
                break;
            }
        }
    })
    .await;
    engine.close().await;

    let shut_out = heard.shut_out;
    let link = if heard.link_lost {
        "lost"
    } else {
        match stats.link {
            Link::Connecting => "connecting",
            Link::Connected => "connected",
            Link::NoMedia => "no_media",
        }
    };
    let report = Report {
        user: username,
        user_id: self_id,
        channel_id: ready.channel_id,
        channel_name: ready.channel_name,
        ssrc: ready.ssrc,
        packets_sent: stats.packets_sent,
        packets_received: stats.packets_received,
        bytes_sent: stats.bytes_sent,
        bytes_received: stats.bytes_received,
        rejected: stats.rejected,
        send_failures: sent.failures,
        frames_sent: sent.sent,
        frames_gated: sent.gated,
        decoded_frames: heard.decoded_frames,
        tone_frames: heard.tone_frames,
        rtt: Rtt {
            min: stats.rtt_min_ms,
            avg: stats.rtt_avg_ms,
            max: stats.rtt_max_ms,
            last: stats.rtt_last_ms,
            samples: stats.rtt_samples,
        },
        link,
        peers: stats
            .peers
            .into_iter()
            .map(|(ssrc, peer)| {
                let (user_id, username) = names.get(&ssrc).cloned().unwrap_or_default();
                PeerReport {
                    user_id,
                    username,
                    ssrc,
                    received: peer.received,
                    lost: peer.lost,
                    late: peer.late,
                    decoded_frames: peer.decoded_frames,
                    decoder_resets: peer.decoder_resets,
                }
            })
            .collect(),
        speaking_events: heard.speaking,
        share,
        watch,
    };

    println!("{}", report.render());
    let _ = std::io::stdout().flush();

    if shut_out {
        eprintln!("vorcall-probe: a moderator kicked or banned the account mid-run");
        return 2;
    }
    if args.expect_silence && report.frames_sent > 0 {
        tracing::error!(
            frames_sent = report.frames_sent,
            "--expect-silence but audio frames went out"
        );
        return 1;
    }
    if args.expect_peer && report.tone_seconds() < 1.0 {
        tracing::error!(
            tone_seconds = report.tone_seconds(),
            "--expect-peer but no peer tone was heard"
        );
        return 1;
    }
    if !watch_confirmed {
        eprintln!(
            "vorcall-probe: no watch of {} was confirmed within {}s",
            args.watch.unwrap_or_default(),
            WATCH_TIMEOUT.as_secs()
        );
        return 1;
    }
    if args.expect_video && report.pictures() < MIN_PICTURES {
        tracing::error!(
            pictures = report.pictures(),
            "--expect-video but almost no picture was decoded"
        );
        return 1;
    }
    if args.expect_share_audio && report.share_tone_seconds() < 1.0 {
        tracing::error!(
            share_tone_seconds = report.share_tone_seconds(),
            "--expect-share-audio but no share tone was heard"
        );
        return 1;
    }
    0
}

/// Announces the local share and waits for the server to echo it back, which is
/// what makes the channel offer it to watchers.
async fn start_share(
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    names: &mut HashMap<u32, (i64, String)>,
    channel_id: i64,
    self_id: i64,
    audio: bool,
) -> Result<(), String> {
    if commands
        .send(Command::StartShare { channel_id, audio })
        .await
        .is_err()
    {
        return Err("the connection loop is gone".to_owned());
    }

    let deadline = tokio::time::Instant::now() + SHARE_START_TIMEOUT;
    loop {
        match timeout_at(deadline, events.next()).await {
            Ok(Some(Event::ShareStarted { user_id, .. })) if user_id == self_id => return Ok(()),
            Ok(Some(Event::ServerError { code, detail, .. })) if matches!(code, 15..=17) => {
                return Err(format!("the server refused the share ({code}): {detail}"));
            }
            Ok(Some(other)) => track_members(names, &other),
            Ok(None) => {
                return Err("the connection loop stopped before the share started".to_owned());
            }
            Err(_) => {
                return Err(format!(
                    "no ShareStarted within {}s",
                    SHARE_START_TIMEOUT.as_secs()
                ));
            }
        }
    }
}

/// Keeps the command sender alive for the whole run: `run` stops as soon as the
/// last sender is dropped, and the `LeaveVoice` still has to go out.
async fn connection_loop(
    endpoints: vorcall_core::Endpoints,
    session: Session,
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
) {
    vorcall_core::connection::run(endpoints, session, commands, events).await;
}

struct VoiceReady {
    channel_id: i64,
    channel_name: String,
    host: String,
    port: u16,
    key: [u8; 32],
    ssrc: u32,
}

/// Whether this channel can carry a voice session at all.
fn joinable(channel: &Channel) -> bool {
    channel.kind == ChannelKind::Voice as i32 || channel.kind == ChannelKind::Dm as i32
}

/// The sidebar order of one channel: its category's position, then its own.
/// A channel in no category sorts above every category, like the UI shows it.
fn sidebar_order(snapshot: &ServerSnapshot, channel: &Channel) -> (i32, i32) {
    let category = snapshot
        .categories
        .iter()
        .find(|category| category.id == channel.category_id)
        .map_or(i32::MIN, |category| category.position);
    (category, channel.position)
}

/// The channel the run joins, from the snapshot the server just sent. The error
/// is a usage error: nothing the probe does later can make the channel appear.
fn resolve_channel(
    snapshot: &ServerSnapshot,
    select: &ChannelSelect,
) -> Result<(i64, String), String> {
    let mut voice: Vec<&Channel> = snapshot
        .channels
        .iter()
        .filter(|channel| channel.kind == ChannelKind::Voice as i32)
        .collect();
    voice.sort_by_key(|channel| sidebar_order(snapshot, channel));

    match select {
        ChannelSelect::Id(id) => {
            let channel = snapshot
                .channels
                .iter()
                .find(|channel| channel.id == *id)
                .ok_or_else(|| format!("--channel {id} is not a channel this account can see"))?;
            if !joinable(channel) {
                return Err(format!("--channel {id} is not a voice channel or a DM"));
            }
            Ok((channel.id, channel.name.clone()))
        }
        ChannelSelect::Name(name) => voice
            .iter()
            .find(|channel| channel.name.eq_ignore_ascii_case(name))
            .map(|channel| (channel.id, channel.name.clone()))
            .ok_or_else(|| format!("--channel-name {name}: no such voice channel")),
        ChannelSelect::Default => {
            let channel = voice
                .iter()
                .find(|channel| channel.name.eq_ignore_ascii_case("General"))
                .or_else(|| voice.first())
                .ok_or("this server has no voice channel to join")?;
            Ok((channel.id, channel.name.clone()))
        }
    }
}

async fn wait_for_voice(
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    names: &mut HashMap<u32, (i64, String)>,
    select: &ChannelSelect,
    share_flow: bool,
) -> Result<VoiceReady, String> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut resolved: Option<(i64, String)> = None;

    loop {
        let event = match timeout_at(deadline, events.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => return Err("the connection loop stopped before voice was ready".to_owned()),
            Err(_) => {
                return Err(format!("no voice within {}s", READY_TIMEOUT.as_secs()));
            }
        };

        match event {
            Event::Connected { username, .. } => {
                tracing::info!(%username, "signed in on the WebSocket");
            }
            // The snapshot always follows Welcome, and it is the only place the
            // channel the run asked for can be named.
            Event::Snapshot(snapshot) => {
                let (channel_id, name) = resolve_channel(&snapshot, select)?;
                tracing::info!(channel_id, %name, "joining voice");
                resolved = Some((channel_id, name));
                if commands
                    .send(Command::JoinVoice { channel_id })
                    .await
                    .is_err()
                {
                    return Err("the connection loop is gone".to_owned());
                }
            }
            Event::Disconnected { reason, retry_in } => {
                if let DisconnectReason::Kicked
                | DisconnectReason::Banned
                | DisconnectReason::Disabled = reason
                {
                    return Err(format!("the account was shut out: {reason}"));
                }
                tracing::warn!(%reason, ?retry_in, "disconnected before voice was ready");
            }
            // 15..=17 are the share refusals, fatal only to a run that is
            // about to share or watch.
            Event::ServerError { code, detail, .. }
                if matches!(code, 5 | 7 | 20) || (share_flow && matches!(code, 15..=17)) =>
            {
                return Err(format!(
                    "the server refused the voice join ({code}): {detail}"
                ));
            }
            Event::ServerError {
                code,
                detail,
                fatal,
            } => {
                tracing::warn!(code, %detail, fatal, "server error");
            }
            Event::VoiceReady {
                channel_id,
                host,
                port,
                key,
                ssrc,
            } => {
                let channel_name = match resolved {
                    Some((id, name)) if id == channel_id => name,
                    _ => String::new(),
                };
                return Ok(VoiceReady {
                    channel_id,
                    channel_name,
                    host,
                    port,
                    key: key.0,
                    ssrc,
                });
            }
            other => track_members(names, &other),
        }
    }
}

#[derive(Default)]
struct Heard {
    decoded_frames: u64,
    tone_frames: u64,
    share_tone_frames: u64,
    watchers_max: u32,
    speaking: Vec<SpeakingEvent>,
    link_lost: bool,
    /// A moderator kicked or banned the account mid-run.
    shut_out: bool,
}

/// Pulls a frame out of the playout every 20 ms — even while nobody speaks, so
/// the jitter buffers keep draining — and follows the channel in parallel. Under
/// `--watch` it also drives the watch: asks for the sharer's stream once they
/// are known to be sharing, and starts decoding when the server confirms it.
async fn listen(
    engine: &MediaEngine,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    names: &mut HashMap<u32, (i64, String)>,
    mut watch: Option<&mut share::WatchPlan>,
    deadline: Instant,
) -> Heard {
    let playout = engine.playout();
    let mut heard = Heard::default();
    let mut ticker = interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut frame = [0.0f32; STEREO_FRAME_SAMPLES];
    let mut mono = [0.0f32; FRAME_SAMPLES];
    let deadline = tokio::time::Instant::from_std(deadline);

    loop {
        // An unconfirmed watch is given its full timeout even when it outlasts
        // --listen-seconds: without it there is nothing to measure.
        let until = match watch.as_deref() {
            Some(plan) if !plan.confirmed() => {
                deadline.max(tokio::time::Instant::from_std(plan.confirm_by))
            }
            _ => deadline,
        };

        tokio::select! {
            () = sleep_until(until) => break,
            _ = ticker.tick() => {
                let speakers = {
                    let mut playout = playout
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    playout.next_stereo_frame(&mut frame)
                };
                // A share's audio is mixed in without counting as a speaker, so
                // it is measured on its own.
                if share::is_share_tone(&frame) {
                    heard.share_tone_frames += 1;
                }
                if speakers > 0 {
                    heard.decoded_frames += 1;
                    share::downmix(&frame, &mut mono);
                    if rms(&mono) > TONE_RMS {
                        heard.tone_frames += 1;
                    }
                }
            }
            event = events.next() => {
                let Some(event) = event else { break };
                if let Some(plan) = watch.as_deref_mut() {
                    plan.note(&event);
                }
                match event {
                    Event::Speaking { user_id, speaking, .. } => {
                        heard.speaking.push(SpeakingEvent { user_id, speaking });
                    }
                    Event::ShareWatchers { count, .. } => {
                        heard.watchers_max = heard.watchers_max.max(count);
                    }
                    Event::WatchState { user_id: Some(user_id), .. } => {
                        if let Some(plan) = watch.as_deref_mut() {
                            let ssrc = names
                                .iter()
                                .find(|(_, (id, _))| *id == user_id)
                                .map(|(ssrc, _)| *ssrc);
                            if ssrc.is_none() {
                                tracing::warn!(user_id, "watching someone with no known ssrc");
                            }
                            engine.watch(ssrc);
                            plan.confirm(user_id, engine.take_access_units());
                            tracing::info!(user_id, ?ssrc, "the server confirmed the watch");
                        }
                    }
                    Event::ServerError { code, detail, .. } if matches!(code, 15..=17 | 20) => {
                        tracing::error!(code, %detail, "the server refused a command");
                    }
                    // A moderator moved or disconnected this session; either
                    // way the media key and ssrc are now stale and the probe
                    // has no re-key path, so the run ends here.
                    Event::VoiceMoved { channel_id } => {
                        println!("{{\"moved_to\":{channel_id}}}");
                        let _ = std::io::stdout().flush();
                        tracing::info!(
                            channel_id,
                            "a moderator moved this session; ending the run"
                        );
                        break;
                    }
                    Event::Disconnected { reason, retry_in } => {
                        heard.shut_out = matches!(
                            reason,
                            DisconnectReason::Kicked
                                | DisconnectReason::Banned
                                | DisconnectReason::Disabled
                        );
                        tracing::warn!(%reason, ?retry_in, "disconnected during the run");
                        heard.link_lost = true;
                        break;
                    }
                    other => track_members(names, &other),
                }

                if let Some(plan) = watch.as_deref_mut()
                    && let Some(user_id) = plan.pending_request(names)
                {
                    let channel_id = plan.channel_id;
                    tracing::info!(user_id, user = %plan.user, "asking to watch a share");
                    let _ = commands
                        .send(Command::WatchShare {
                            channel_id,
                            user_id,
                        })
                        .await;
                }
            }
        }
    }

    heard
}

fn track_members(names: &mut HashMap<u32, (i64, String)>, event: &Event) {
    match event {
        Event::VoiceState { members, .. } => {
            for member in members {
                names.insert(member.ssrc, (member.user_id, member.username.clone()));
            }
        }
        Event::VoiceMemberJoined { member, .. } => {
            names.insert(member.ssrc, (member.user_id, member.username.clone()));
        }
        Event::VoiceMemberLeft { user_id, .. } => {
            names.retain(|_, (id, _)| id != user_id);
        }
        _ => {}
    }
}

struct SendPlan {
    hz: f32,
    amplitude: f32,
    /// `Some` under `--vad`: the frames it closes on never reach the encoder.
    gate: Option<NoiseGate>,
}

#[derive(Default)]
struct SendOutcome {
    sent: u64,
    failures: u64,
    gated: u64,
}

/// Returns how many audio frames reached the socket, how many were lost to an
/// encode or send failure, and how many the gate held back.
async fn send_tone(sender: FrameSender, plan: SendPlan, frames: u64) -> SendOutcome {
    let SendPlan {
        hz,
        amplitude,
        mut gate,
    } = plan;
    let mut outcome = SendOutcome::default();
    let mut encoder = match Encoder::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "no Opus encoder; sending nothing");
            outcome.failures = frames;
            return outcome;
        }
    };
    let mut ticker = interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tone = Tone::new(hz, amplitude);
    let mut pcm = [0.0f32; FRAME_SAMPLES];
    let mut packet = [0u8; MAX_PACKET];

    for index in 0..frames {
        let now = ticker.tick().await;
        tone.fill(&mut pcm);
        let marker = match gate.as_mut() {
            Some(gate) => match gate.process(&pcm, now.into_std()) {
                GateDecision::Closed => {
                    outcome.gated += 1;
                    continue;
                }
                GateDecision::Opened => true,
                GateDecision::Open => false,
            },
            // Without a gate a talk spurt starts on the first frame and never
            // stops afterwards.
            None => index == 0,
        };
        match encoder.encode(&pcm, &mut packet) {
            Ok(written) => match sender.send_audio(&packet[..written], marker) {
                Ok(()) => outcome.sent += 1,
                Err(error) => {
                    tracing::debug!(%error, "dropping a frame the socket refused");
                    outcome.failures += 1;
                }
            },
            Err(error) => {
                tracing::debug!(%error, "dropping a frame the encoder refused");
                outcome.failures += 1;
            }
        }
    }

    outcome
}

/// `Ok(None)` means `--help` was asked for.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut username = None;
    let mut password = None;
    let mut channel_id = None;
    let mut channel_name = None;
    let mut send_seconds = 10u64;
    let mut listen_seconds = 12u64;
    let mut tone_hz = 440.0f32;
    let mut tone_amplitude = DEFAULT_TONE_AMPLITUDE;
    let mut vad = false;
    let mut vad_threshold = vorcall_voice::gate::DEFAULT_THRESHOLD_DB;
    let mut share_seconds = None;
    let mut share_audio = false;
    let mut share_size = (1280u32, 720u32);
    let mut share_fps = FrameRate::F30;
    let mut encode_threads = 1u16;
    let mut bitrate_kbps = None;
    let mut watch = None;
    let mut expect_peer = false;
    let mut expect_silence = false;
    let mut expect_video = false;
    let mut expect_share_audio = false;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(None),
            "--expect-peer" => expect_peer = true,
            "--expect-silence" => expect_silence = true,
            "--expect-video" => expect_video = true,
            "--expect-share-audio" => expect_share_audio = true,
            "--share-audio" => share_audio = true,
            "--share-seconds" => share_seconds = Some(number(&flag, &mut args)?),
            "--watch" => watch = Some(value(&flag, &mut args)?),
            "--share-size" => share_size = share::parse_size(&value(&flag, &mut args)?)?,
            "--share-fps" => {
                let raw = number(&flag, &mut args)?;
                share_fps = u32::try_from(raw)
                    .ok()
                    .and_then(FrameRate::from_hz)
                    .ok_or_else(|| format!("--share-fps wants 15, 30 or 60, got {raw}"))?;
            }
            "--encode-threads" => {
                let raw = number(&flag, &mut args)?;
                encode_threads = u16::try_from(raw)
                    .ok()
                    .filter(|threads| *threads >= 1)
                    .ok_or_else(|| format!("--encode-threads wants 1 or more, got {raw}"))?;
            }
            "--bitrate-kbps" => {
                let raw = number(&flag, &mut args)?;
                bitrate_kbps =
                    Some(u32::try_from(raw).map_err(|_| {
                        format!("--bitrate-kbps wants a smaller number, got {raw}")
                    })?);
            }
            "--vad" => vad = true,
            "--username" => username = Some(value(&flag, &mut args)?),
            "--password" => password = Some(value(&flag, &mut args)?),
            "--channel" => {
                let raw = value(&flag, &mut args)?;
                channel_id = Some(
                    raw.parse::<i64>()
                        .map_err(|_| format!("--channel wants a channel id, got {raw}"))?,
                );
            }
            "--channel-name" => channel_name = Some(value(&flag, &mut args)?),
            "--send-seconds" => send_seconds = number(&flag, &mut args)?,
            "--listen-seconds" => listen_seconds = number(&flag, &mut args)?,
            "--tone-hz" => {
                let raw = value(&flag, &mut args)?;
                tone_hz = raw
                    .parse()
                    .map_err(|_| format!("--tone-hz wants a number, got {raw}"))?;
            }
            "--tone-amplitude" => {
                let raw = value(&flag, &mut args)?;
                tone_amplitude = raw
                    .parse()
                    .map_err(|_| format!("--tone-amplitude wants a number, got {raw}"))?;
                if !(0.0..=1.0).contains(&tone_amplitude) {
                    return Err(format!("--tone-amplitude wants 0..1, got {raw}"));
                }
            }
            "--vad-threshold" => {
                let raw = value(&flag, &mut args)?;
                vad_threshold = raw
                    .parse()
                    .map_err(|_| format!("--vad-threshold wants a number, got {raw}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    let username = username.ok_or("--username is required")?;
    let password = password
        .or_else(|| std::env::var("VORCALL_PROBE_PASSWORD").ok())
        .filter(|password| !password.is_empty())
        .ok_or("--password or VORCALL_PROBE_PASSWORD is required")?;
    let channel = match (channel_id, channel_name) {
        (Some(_), Some(_)) => {
            return Err("--channel and --channel-name cannot be used together".to_owned());
        }
        (Some(id), None) => ChannelSelect::Id(id),
        (None, Some(name)) if name.is_empty() => {
            return Err("--channel-name cannot be empty".to_owned());
        }
        (None, Some(name)) => ChannelSelect::Name(name),
        (None, None) => ChannelSelect::Default,
    };
    if listen_seconds < send_seconds {
        return Err("--listen-seconds cannot be shorter than --send-seconds".to_owned());
    }
    if share_seconds.is_some() && watch.is_some() {
        return Err("--share-seconds and --watch cannot be used together".to_owned());
    }
    if share_seconds.is_some_and(|seconds| seconds > listen_seconds) {
        return Err("--listen-seconds cannot be shorter than --share-seconds".to_owned());
    }
    if watch.as_ref().is_some_and(String::is_empty) {
        return Err("--watch cannot be empty".to_owned());
    }

    Ok(Some(Args {
        username,
        password,
        channel,
        send_seconds,
        listen_seconds,
        tone_hz,
        tone_amplitude,
        vad,
        vad_threshold,
        share_seconds,
        share_audio,
        share_size,
        share_fps,
        encode_threads,
        bitrate_kbps,
        watch,
        expect_peer,
        expect_silence,
        expect_video,
        expect_share_audio,
    }))
}

fn value(flag: &str, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} wants a value"))
}

fn number(flag: &str, args: &mut impl Iterator<Item = String>) -> Result<u64, String> {
    let raw = value(flag, args)?;
    raw.parse()
        .map_err(|_| format!("{flag} wants a whole number, got {raw}"))
}
