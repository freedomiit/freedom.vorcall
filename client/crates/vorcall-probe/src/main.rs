//! A headless voice client: signs in, joins voice, sends a tone, listens, and
//! prints one JSON line about what came back.
//!
//! Two probes in two terminals are the end-to-end oracle for the voice path,
//! locally and against production. Nothing here touches an audio device: the
//! source is a synthesized sine and the sink is a level meter.

mod report;
mod update_cmd;

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::{SinkExt as _, StreamExt as _};
use tokio::time::{MissedTickBehavior, interval, sleep_until, timeout, timeout_at};
use tracing_subscriber::EnvFilter;
use vorcall_core::connection::GENERAL_ROOM;
use vorcall_core::{Command, Event, Session};
use vorcall_voice::codec::Encoder;
use vorcall_voice::tone::{Tone, rms};
use vorcall_voice::{
    FRAME_MS, FRAME_SAMPLES, FrameSender, Link, MediaConfig, MediaEngine, Playout,
};

use report::{PeerReport, Report, Rtt, SpeakingEvent};

/// Peaks at 0.3, so a clean frame measures ≈0.21 and silence or concealment
/// stays far below the threshold below.
const TONE_AMPLITUDE: f32 = 0.3;

/// Anything above this is the tone rather than silence or packet loss
/// concealment fading out.
const TONE_RMS: f32 = 0.02;

/// Opus at 48 kbit/s over 20 ms frames never comes near this.
const MAX_PACKET: usize = 512;

/// Sign-in, WebSocket, room join and the voice handshake all fit in this.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

const USAGE: &str = "\
vorcall-probe --username U (--password P | env VORCALL_PROBE_PASSWORD) [options]

Options:
  --username <name>      account to sign in as (required)
  --password <secret>    password; defaults to $VORCALL_PROBE_PASSWORD
  --room <id>            room to join (default: general)
  --send-seconds <n>     seconds of tone to send (default: 10)
  --listen-seconds <n>   seconds to keep receiving, >= --send-seconds (default: 12)
  --tone-hz <hz>         tone frequency (default: 440)
  --expect-peer          exit 1 unless at least 1.00 s of tone was heard
  --help                 print this help

The server URL and key come from VORCALL_SERVER_URL and VORCALL_SERVER_KEY,
with the values baked in at build time as fallbacks.

Prints one JSON line on stdout; logs go to stderr.

Exit codes: 0 ran, 1 --expect-peer heard nothing, 2 usage, sign-in,
connection or media failure.

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

struct Args {
    username: String,
    password: String,
    room: String,
    send_seconds: u64,
    listen_seconds: u64,
    tone_hz: f32,
    expect_peer: bool,
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
    let ready = match wait_for_voice(&mut commands, &mut events, &mut names, &args.room).await {
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
    tracing::info!(ssrc = ready.ssrc, room = %ready.room_id, "voice connected");

    let sending = tokio::task::spawn(send_tone(
        engine.sender(),
        args.tone_hz,
        args.send_seconds * 1000 / FRAME_MS,
    ));

    let heard = listen(
        engine.playout(),
        &mut events,
        &mut names,
        started + Duration::from_secs(args.listen_seconds),
    )
    .await;

    let stats = engine.stats();
    let (packets_sent_ok, send_failures) = sending.await.unwrap_or_default();
    tracing::debug!(packets_sent_ok, send_failures, "sender finished");

    let _ = commands
        .send(Command::LeaveVoice {
            room_id: ready.room_id.clone(),
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
        room: ready.room_id,
        ssrc: ready.ssrc,
        packets_sent: stats.packets_sent,
        packets_received: stats.packets_received,
        bytes_sent: stats.bytes_sent,
        bytes_received: stats.bytes_received,
        rejected: stats.rejected,
        send_failures,
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
    };

    println!("{}", report.render());
    let _ = std::io::stdout().flush();

    if args.expect_peer && report.tone_seconds() < 1.0 {
        return 1;
    }
    0
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
    room_id: String,
    host: String,
    port: u16,
    key: [u8; 32],
    ssrc: u32,
}

async fn wait_for_voice(
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    names: &mut HashMap<u32, (i64, String)>,
    room: &str,
) -> Result<VoiceReady, String> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;

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
                if commands
                    .send(Command::JoinVoice {
                        room_id: room.to_owned(),
                    })
                    .await
                    .is_err()
                {
                    return Err("the connection loop is gone".to_owned());
                }
            }
            Event::Disconnected { reason, retry_in } => {
                tracing::warn!(%reason, ?retry_in, "disconnected before voice was ready");
            }
            Event::ServerError { code, detail, .. } if matches!(code, 5..=7) => {
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
                room_id,
                host,
                port,
                key,
                ssrc,
            } => {
                return Ok(VoiceReady {
                    room_id,
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
    speaking: Vec<SpeakingEvent>,
    link_lost: bool,
}

/// Pulls a frame out of the playout every 20 ms — even while nobody speaks, so
/// the jitter buffers keep draining — and follows the room in parallel.
async fn listen(
    playout: Arc<Mutex<Playout>>,
    events: &mut mpsc::Receiver<Event>,
    names: &mut HashMap<u32, (i64, String)>,
    deadline: Instant,
) -> Heard {
    let mut heard = Heard::default();
    let mut ticker = interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut frame = [0.0f32; FRAME_SAMPLES];
    let deadline = tokio::time::Instant::from_std(deadline);

    loop {
        tokio::select! {
            () = sleep_until(deadline) => break,
            _ = ticker.tick() => {
                let speakers = {
                    let mut playout = playout
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    playout.next_frame(&mut frame)
                };
                if speakers > 0 {
                    heard.decoded_frames += 1;
                    if rms(&frame) > TONE_RMS {
                        heard.tone_frames += 1;
                    }
                }
            }
            event = events.next() => {
                let Some(event) = event else { break };
                match event {
                    Event::Speaking { user_id, speaking, .. } => {
                        heard.speaking.push(SpeakingEvent { user_id, speaking });
                    }
                    Event::Disconnected { reason, retry_in } => {
                        tracing::warn!(%reason, ?retry_in, "disconnected during the run");
                        heard.link_lost = true;
                        break;
                    }
                    other => track_members(names, &other),
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

/// Returns how many frames reached the socket and how many were lost to an
/// encode or send failure.
async fn send_tone(sender: FrameSender, hz: f32, frames: u64) -> (u64, u64) {
    let mut encoder = match Encoder::new() {
        Ok(encoder) => encoder,
        Err(error) => {
            tracing::error!(%error, "no Opus encoder; sending nothing");
            return (0, frames);
        }
    };
    let mut ticker = interval(Duration::from_millis(FRAME_MS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut tone = Tone::new(hz, TONE_AMPLITUDE);
    let mut pcm = [0.0f32; FRAME_SAMPLES];
    let mut packet = [0u8; MAX_PACKET];
    let (mut sent, mut failures) = (0, 0);

    for index in 0..frames {
        ticker.tick().await;
        tone.fill(&mut pcm);
        match encoder.encode(&pcm, &mut packet) {
            // A talk spurt starts on the first frame and never stops afterwards.
            Ok(written) => match sender.send_audio(&packet[..written], index == 0) {
                Ok(()) => sent += 1,
                Err(error) => {
                    tracing::debug!(%error, "dropping a frame the socket refused");
                    failures += 1;
                }
            },
            Err(error) => {
                tracing::debug!(%error, "dropping a frame the encoder refused");
                failures += 1;
            }
        }
    }

    (sent, failures)
}

/// `Ok(None)` means `--help` was asked for.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut username = None;
    let mut password = None;
    let mut room = GENERAL_ROOM.to_owned();
    let mut send_seconds = 10u64;
    let mut listen_seconds = 12u64;
    let mut tone_hz = 440.0f32;
    let mut expect_peer = false;

    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(None),
            "--expect-peer" => expect_peer = true,
            "--username" => username = Some(value(&flag, &mut args)?),
            "--password" => password = Some(value(&flag, &mut args)?),
            "--room" => room = value(&flag, &mut args)?,
            "--send-seconds" => send_seconds = number(&flag, &mut args)?,
            "--listen-seconds" => listen_seconds = number(&flag, &mut args)?,
            "--tone-hz" => {
                let raw = value(&flag, &mut args)?;
                tone_hz = raw
                    .parse()
                    .map_err(|_| format!("--tone-hz wants a number, got {raw}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    let username = username.ok_or("--username is required")?;
    let password = password
        .or_else(|| std::env::var("VORCALL_PROBE_PASSWORD").ok())
        .filter(|password| !password.is_empty())
        .ok_or("--password or VORCALL_PROBE_PASSWORD is required")?;
    if room.is_empty() {
        return Err("--room cannot be empty".to_owned());
    }
    if listen_seconds < send_seconds {
        return Err("--listen-seconds cannot be shorter than --send-seconds".to_owned());
    }

    Ok(Some(Args {
        username,
        password,
        room,
        send_seconds,
        listen_seconds,
        tone_hz,
        expect_peer,
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
