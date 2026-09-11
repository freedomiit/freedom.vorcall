#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The Vorcall desktop client: an account signs in over REST, one iced
//! subscription owns the WebSocket, and nothing but preferences and tokens ever
//! touches the disk.

mod app;
mod audio;
mod brand;
mod images;
mod notify;
mod update_ui;
mod view;
mod voice;

use std::ffi::OsStr;

use iced::Theme;
use tracing_subscriber::EnvFilter;
use vorcall_core::Config;
use vorcall_core::update::{self, PublicKey, Version};

use app::App;

fn main() -> iced::Result {
    // Answered before anything else: the release tooling asks a freshly built
    // binary what it is, and that has to work with no display, no server key
    // and no configuration.
    if matches!(
        std::env::args_os()
            .nth(1)
            .as_deref()
            .and_then(OsStr::to_str),
        Some("--version" | "-V")
    ) {
        println!("vorcall {} {}", Version::current(), update::platform());
        std::process::exit(0);
    }

    // Whatever the last Windows swap moved aside; a no-op everywhere else.
    update::swap::cleanup_old();

    // reqwest is built with `rustls-no-provider`; without this it panics on the
    // first request. An Err only means someone already installed a provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let endpoints = match vorcall_core::endpoints::resolve() {
        Ok(endpoints) => endpoints,
        Err(e) => {
            eprintln!("vorcall: {e:#}");
            std::process::exit(2);
        }
    };
    if endpoints.is_dev_key() {
        tracing::warn!("running with the development server key");
    }
    tracing::info!(?endpoints, "resolved endpoints");

    // An unusable key list is not a reason to refuse to start: it disables the
    // updater, which `disabled_reason` then reports.
    let keys = update::keys::baked().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "ignoring the baked-in update keys");
        Vec::new()
    });
    let disabled = update::disabled_reason(&endpoints, &keys, cfg!(debug_assertions));
    if disabled.is_none() {
        apply_pending_update(&keys);
    }

    // A broken preferences or session file is not worth refusing to start over:
    // the worst case is a first run that asks for a sign-in again.
    let config = match vorcall_core::config::load() {
        Ok(config) => config.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "ignoring the stored configuration");
            Config::default()
        }
    };
    let session = match vorcall_core::session::load() {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(error = %e, "ignoring the stored session");
            None
        }
    };

    iced::daemon(
        move || {
            App::boot(
                endpoints.clone(),
                config.clone(),
                session.clone(),
                keys.clone(),
                disabled.clone(),
            )
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .style(|_: &App, theme: &Theme| brand::palette::style(theme))
    .subscription(App::subscription)
    .run()
}

/// A download an earlier run finished takes effect here, before the first
/// window is up: this is the one moment the binary can be replaced with nothing
/// on screen to lose. Everything is verified again from disk, so a download
/// that no longer holds up is dropped rather than applied.
fn apply_pending_update(keys: &[PublicKey]) {
    let Ok(dir) = update::swap::install_dir() else {
        return;
    };
    let Some(pending) =
        update::pending::take_verified(&dir, keys, &Version::current(), &update::platform())
    else {
        return;
    };

    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match update::swap::apply_and_relaunch(&pending, &args) {
        // On unix the process image is already gone by now; this is Windows,
        // where the relaunched process is coming up and this one is in its way.
        Ok(update::swap::Relaunched::Spawned) => std::process::exit(0),
        Err(e) => tracing::warn!(error = %e, "pending update not applied"),
    }
}
