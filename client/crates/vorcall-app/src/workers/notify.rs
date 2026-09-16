//! Desktop notifications for messages that land while the window is not
//! focused, delivered from one long-lived thread.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// How much of a message the notification body carries; the wire limit is 2000.
const PREVIEW_MAX_CHARS: usize = 120;

const TIMEOUT_MS: u32 = 8000;

/// Matches `packaging/linux/vorcall.desktop` and the icon `install.sh` installs.
#[cfg(all(unix, not(target_os = "macos")))]
const DESKTOP_ENTRY: &str = "vorcall";

/// GNOME Shell withdraws a notification as soon as its sender's bus name
/// vanishes, and each handle owns the connection that name belongs to.
#[cfg(all(unix, not(target_os = "macos")))]
const LIVE_HANDLES: usize = 20;

/// The macOS bundle identifier, and the AppUserModelID the Windows installer
/// registers; without it a toast is attributed to PowerShell.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const APP_ID: &str = "br.com.freedomit.vorcall";

struct Job {
    author: String,
    body: String,
}

static NOTIFIER: OnceLock<Sender<Job>> = OnceLock::new();

/// Queues one notification for the notifier thread, starting it on first use.
/// Never blocks: `show()` waits on D-Bus on Linux and runs AppleScript on macOS.
pub fn show(author: String, body: String) {
    let notifier = NOTIFIER.get_or_init(start);
    if notifier.send(Job { author, body }).is_err() {
        tracing::warn!("the notification thread is gone; dropping a notification");
    }
}

fn start() -> Sender<Job> {
    let (sender, jobs) = mpsc::channel();
    if let Err(e) = thread::Builder::new()
        .name("notify".to_owned())
        .spawn(move || run(jobs))
    {
        tracing::warn!(error = %e, "cannot start the notification thread");
    }
    sender
}

fn run(jobs: Receiver<Job>) {
    #[cfg(target_os = "macos")]
    register_bundle();

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut live = std::collections::VecDeque::with_capacity(LIVE_HANDLES);

    for job in jobs {
        let mut notification = notify_rust::Notification::new();
        notification
            .appname("Vorcall")
            .summary(&job.author)
            .body(&job.body)
            // No `sound_name`: on Windows that is what makes a toast audible,
            // and the chime is ours to play.
            .timeout(notify_rust::Timeout::Milliseconds(TIMEOUT_MS));
        #[cfg(all(unix, not(target_os = "macos")))]
        notification
            .hint(notify_rust::Hint::DesktopEntry(DESKTOP_ENTRY.to_owned()))
            .icon(DESKTOP_ENTRY);
        #[cfg(target_os = "windows")]
        notification.app_id(APP_ID);

        match notification.show() {
            #[cfg(all(unix, not(target_os = "macos")))]
            Ok(handle) => {
                if live.len() == LIVE_HANDLES {
                    live.pop_front();
                }
                live.push_back(handle);
            }
            // The notification is delivered when the handle drops, not by `show()`.
            #[cfg(target_os = "macos")]
            Ok(handle) => drop(handle),
            #[cfg(not(unix))]
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "cannot show the desktop notification"),
        }
    }
}

/// Without this mac-notification-sys posts as Finder, which macOS drops.
#[cfg(target_os = "macos")]
fn register_bundle() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::warn!(error = %e, "cannot locate the executable; notifications are off");
            return;
        }
    };
    if !inside_app_bundle(&exe) {
        tracing::info!("not running from Vorcall.app; desktop notifications need the bundle");
        return;
    }
    if let Err(e) = notify_rust::set_application(APP_ID) {
        tracing::warn!(error = %e, "cannot register the bundle for notifications");
    }
}

/// Whether `path` is an executable at `<name>.app/Contents/MacOS/<exe>`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn inside_app_bundle(path: &Path) -> bool {
    let Some(macos) = path.parent() else {
        return false;
    };
    let Some(contents) = macos.parent() else {
        return false;
    };
    let Some(bundle) = contents.parent() else {
        return false;
    };
    macos.file_name().is_some_and(|name| name == "MacOS")
        && contents.file_name().is_some_and(|name| name == "Contents")
        && bundle.extension().is_some_and(|ext| ext == "app")
}

/// Cuts on a scalar boundary, so a preview never splits a character.
pub fn preview(text: &str) -> String {
    let mut preview: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
    if text.chars().nth(PREVIEW_MAX_CHARS).is_some() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_says_when_there_was_more() {
        assert_eq!(preview("hello"), "hello");

        let long = "a".repeat(PREVIEW_MAX_CHARS + 1);
        let preview = preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    /// The cut is on a scalar, never inside one.
    #[test]
    fn a_preview_never_splits_a_character() {
        let long = "á".repeat(PREVIEW_MAX_CHARS + 4);
        let preview = preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
    }

    #[test]
    fn an_executable_inside_a_bundle_is_recognised() {
        assert!(inside_app_bundle(Path::new(
            "/Applications/Vorcall.app/Contents/MacOS/vorcall"
        )));
    }

    #[test]
    fn a_bare_executable_is_not_inside_a_bundle() {
        assert!(!inside_app_bundle(Path::new("/usr/bin/vorcall")));
    }
}
