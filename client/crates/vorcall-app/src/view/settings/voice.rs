//! The Voice page: the devices, how audio leaves the machine, the cleanup chain in
//! front of it, and what a screen share is encoded as.

use std::fmt;

use iced::alignment::Vertical;
use iced::widget::{button, column, pick_list, progress_bar, row, slider, text};
use iced::{Element, Length, Theme};
use vorcall_core::config::{
    SHARE_DEFAULT_RESOLUTION, SHARE_FRAME_RATES, SHARE_MAX_BITRATE_KBPS, SHARE_MIN_BITRATE_KBPS,
    SHARE_RESOLUTIONS, TransmitMode, VAD_MAX_DB, VAD_MIN_DB,
};

use crate::app::message::{Message, ShareMsg, VoiceMsg};
use crate::app::state::rules::{self, SYSTEM_DEFAULT, hotkey_sentence};
use crate::app::state::voice::HotkeyStatus;
use crate::app::{App, MainState};
use crate::theme::styles;
use crate::view::settings::{choices, field, section, toggle_row};
use crate::view::{TEXT_ROW, TEXT_SECONDARY};

/// The threshold slider and the level meter share a width, so the gate and the
/// level it is measured against line up.
const METER_WIDTH: f32 = 240.0;
const METER_GIRTH: f32 = 10.0;

pub fn view<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let config = &app.config;

    let input = pick_list(
        device_options(&main.settings.inputs),
        Some(device_selection(config.input_device.as_deref())),
        |name| Message::Voice(VoiceMsg::SetInputDevice(name)),
    )
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .width(Length::Fill)
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens));

    let output = pick_list(
        device_options(&main.settings.outputs),
        Some(device_selection(config.output_device.as_deref())),
        |name| Message::Voice(VoiceMsg::SetOutputDevice(name)),
    )
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .width(Length::Fill)
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens));

    let threshold = row![
        slider(VAD_MIN_DB..=VAD_MAX_DB, config.vad_threshold_db, |value| {
            Message::Voice(VoiceMsg::SetVadThreshold(value))
        })
        .step(1.0_f32)
        .on_release(Message::Voice(VoiceMsg::VadThresholdReleased))
        .width(METER_WIDTH)
        .style(styles::slider(tokens)),
        text(format!("{:.0} dB", config.vad_threshold_db))
            .size(TEXT_ROW)
            .color(tokens.text_secondary),
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    column![
        section(
            "Devices",
            tokens,
            vec![
                field("Input device", input, None, tokens),
                field("Output device", output, None, tokens),
            ],
        ),
        section(
            "Transmit",
            tokens,
            vec![
                choices(
                    &[
                        (TransmitMode::PushToTalk, "Push to talk"),
                        (TransmitMode::VoiceActivation, "Voice activation"),
                    ],
                    config.transmit_mode,
                    |mode| Message::Voice(VoiceMsg::SetTransmitMode(mode)),
                    tokens,
                ),
                field(
                    "Voice activation threshold",
                    threshold,
                    Some("The gate opens above this level; the meter below is what it measures."),
                    tokens,
                ),
                meter(app, main),
                push_to_talk(app, main),
            ],
        ),
        section(
            "Input cleanup",
            tokens,
            vec![
                toggle_row(
                    "Echo cancellation",
                    "Removes what Vorcall plays from what your microphone sends. Other applications' audio still comes through.",
                    config.echo_cancellation,
                    |on| Message::Voice(VoiceMsg::SetEchoCancellation(on)),
                    tokens,
                ),
                toggle_row(
                    "Noise suppression",
                    "Takes fans, keyboards and hiss out of the signal.",
                    config.noise_suppression,
                    |on| Message::Voice(VoiceMsg::SetNoiseSuppression(on)),
                    tokens,
                ),
                toggle_row(
                    "Automatic gain",
                    "Evens out a microphone that is too quiet or too loud.",
                    config.auto_gain,
                    |on| Message::Voice(VoiceMsg::SetAutoGain(on)),
                    tokens,
                ),
            ],
        ),
        section(
            "Playback",
            tokens,
            vec![toggle_row(
                "Quieten others for priority speakers",
                "Drops every other voice by 12 dB while a priority speaker talks. Off by default, and set on this machine only.",
                config.priority_ducking,
                |on| Message::Voice(VoiceMsg::SetPriorityDucking(on)),
                tokens,
            )],
        ),
        share(app, main),
    ]
    .spacing(24)
    .width(Length::Fill)
    .into()
}

/// What a share this machine starts is encoded as. A change applies to the next
/// one: the encoder is built when the capture starts.
fn share<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let config = &app.config;

    let resolution = pick_list(
        ResolutionChoice::ALL.to_vec(),
        Some(ResolutionChoice(resolution_of(&config.share_resolution))),
        |choice| Message::Share(ShareMsg::SetResolution(choice.0.to_owned())),
    )
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens));

    let fps = pick_list(
        FpsChoice::ALL.to_vec(),
        Some(FpsChoice(config.share_fps)),
        |choice| Message::Share(ShareMsg::SetFps(choice.0)),
    )
    .text_size(TEXT_ROW)
    .padding([6.0, 10.0])
    .style(styles::pick_list(tokens))
    .menu_style(styles::menu(tokens));

    let rows = vec![
        field("Resolution", resolution, None, tokens),
        field("Frame rate", fps, None, tokens),
        toggle_row(
            "Automatic bitrate",
            "Lets the resolution and the frame rate decide how much a share may send.",
            config.share_bitrate_kbps.is_none(),
            |on| Message::Share(ShareMsg::SetBitrateAuto(on)),
            tokens,
        ),
        // Automatic has nothing to drag: the preset's own figure is what it would
        // ask the encoder for.
        match config.share_bitrate_kbps {
            Some(kbps) => field("Bitrate", bitrate(app, kbps), None, tokens),
            None => field(
                "Bitrate",
                text(format!("{} kbps", rules::auto_bitrate_kbps(config)))
                    .size(TEXT_ROW)
                    .color(tokens.text_secondary),
                None,
                tokens,
            ),
        },
        toggle_row(
            "Share audio",
            "Carries what this machine plays with the picture, without Vorcall's own voices.",
            config.share_audio,
            |on| Message::Share(ShareMsg::SetShareAudio(on)),
            tokens,
        ),
        text(rules::share_sentence(
            &vorcall_screen::capabilities(),
            main.voice.share.backend,
        ))
        .size(TEXT_SECONDARY)
        .color(tokens.text_muted)
        .into(),
    ];

    section("Screen share", tokens, rows)
}

/// The manual bitrate, which only the end of a drag writes to disk.
fn bitrate<'a>(app: &'a App, kbps: u32) -> Element<'a, Message> {
    let tokens = &app.tokens;
    row![
        slider(
            SHARE_MIN_BITRATE_KBPS..=SHARE_MAX_BITRATE_KBPS,
            kbps,
            |value| Message::Share(ShareMsg::SetBitrate(value))
        )
        .step(500_u32)
        .on_release(Message::Share(ShareMsg::BitrateReleased))
        .width(METER_WIDTH)
        .style(styles::slider(tokens)),
        text(format!("{kbps} kbps"))
            .size(TEXT_ROW)
            .color(tokens.text_secondary),
    ]
    .spacing(12)
    .align_y(Vertical::Center)
    .into()
}

/// The stored resolution as one of the entries the list offers. A value from a
/// hand-edited file is normalised on load, so this only has to find it again.
fn resolution_of(stored: &str) -> &'static str {
    SHARE_RESOLUTIONS
        .into_iter()
        .find(|known| *known == stored)
        .unwrap_or(SHARE_DEFAULT_RESOLUTION)
}

/// A resolution entry: the value the configuration stores, under a label that says
/// what it means.
#[derive(Clone, PartialEq, Eq)]
struct ResolutionChoice(&'static str);

impl ResolutionChoice {
    const ALL: [Self; SHARE_RESOLUTIONS.len()] = [
        Self(SHARE_RESOLUTIONS[0]),
        Self(SHARE_RESOLUTIONS[1]),
        Self(SHARE_RESOLUTIONS[2]),
        Self(SHARE_RESOLUTIONS[3]),
        Self(SHARE_RESOLUTIONS[4]),
    ];
}

impl fmt::Display for ResolutionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            "source" => f.write_str("As captured"),
            named => f.write_str(named),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FpsChoice(u32);

impl FpsChoice {
    const ALL: [Self; SHARE_FRAME_RATES.len()] = [
        Self(SHARE_FRAME_RATES[0]),
        Self(SHARE_FRAME_RATES[1]),
        Self(SHARE_FRAME_RATES[2]),
    ];
}

impl fmt::Display for FpsChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} fps", self.0)
    }
}

/// The microphone level the audio thread last reported, against the same scale as
/// the threshold above it. The bar turns green while the gate stands open.
fn meter<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let (level, open) = match main.voice.input_level {
        Some((dbfs, open)) => (dbfs.clamp(VAD_MIN_DB, 0.0), open),
        None => (VAD_MIN_DB, false),
    };
    let (bar_color, muted_color) = (
        if open {
            tokens.success
        } else {
            tokens.text_muted
        },
        tokens.track,
    );

    let mut line = row![
        text("Input level")
            .size(TEXT_ROW)
            .color(tokens.text_secondary),
        progress_bar(VAD_MIN_DB..=0.0, level)
            .length(METER_WIDTH)
            .girth(METER_GIRTH)
            .style(move |_theme: &Theme| progress_bar::Style {
                background: muted_color.into(),
                bar: bar_color.into(),
                border: iced::border::rounded(METER_GIRTH / 2.0),
            }),
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    if main.voice.input_level.is_none() {
        line = line.push(
            text("Join voice to see the input level")
                .size(TEXT_SECONDARY)
                .color(tokens.text_muted),
        );
    }
    line.into()
}

/// Where push-to-talk edges come from, and the one thing there is to press when
/// the system would not give them up.
fn push_to_talk<'a>(app: &'a App, main: &'a MainState) -> Element<'a, Message> {
    let tokens = &app.tokens;
    let window_only = matches!(main.voice.hotkey_status, HotkeyStatus::WindowOnly(_));

    let mut line = row![
        text(hotkey_sentence(
            &main.voice.hotkey_status,
            app.config.transmit_mode,
        ))
        .size(TEXT_ROW)
        .color(if window_only {
            tokens.warning
        } else {
            tokens.text_muted
        }),
    ]
    .spacing(12)
    .align_y(Vertical::Center);

    // Retrying only means anything while there is a session to listen for.
    if window_only && main.voice.session.is_some() {
        line = line.push(
            button(text("Retry").size(TEXT_ROW))
                .padding([4.0, 10.0])
                .style(styles::button::secondary(tokens))
                .on_press(Message::Voice(VoiceMsg::RetryHotkey)),
        );
    }

    field("Push to talk", line, None, tokens)
}

/// The devices the host reported, with the system default in front of them.
fn device_options(names: &[String]) -> Vec<String> {
    std::iter::once(SYSTEM_DEFAULT.to_owned())
        .chain(names.iter().cloned())
        .collect()
}

fn device_selection(chosen: Option<&str>) -> String {
    chosen.unwrap_or(SYSTEM_DEFAULT).to_owned()
}
