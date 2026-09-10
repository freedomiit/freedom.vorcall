#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The Vorcall desktop client: an account signs in over REST, one iced
//! subscription owns the WebSocket, and nothing but preferences and tokens ever
//! touches the disk.

mod app;
mod audio;
mod brand;
mod notify;
mod view;
mod voice;

use iced::Theme;
use tracing_subscriber::EnvFilter;
use vorcall_core::Config;

use app::App;

fn main() -> iced::Result {
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
        move || App::boot(endpoints.clone(), config.clone(), session.clone()),
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .style(|_: &App, theme: &Theme| brand::palette::style(theme))
    .subscription(App::subscription)
    .run()
}
