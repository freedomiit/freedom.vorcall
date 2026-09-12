//! Desktop notifications for messages that land while the window is not
//! focused.

/// How much of a message the notification body carries; the wire limit is 2000.
const PREVIEW_MAX_CHARS: usize = 120;

const TIMEOUT_MS: u32 = 8000;

/// Shows one notification, or logs why it could not.
///
/// `show()` opens a D-Bus connection and waits for the reply on Linux, so it runs
/// on a blocking thread instead of on the executor. On macOS the notification is
/// delivered when the returned handle drops, which is what happens here; the
/// handle type differs per platform, hence the discard.
pub async fn show(author: String, body: String) {
    let sent = tokio::task::spawn_blocking(move || {
        notify_rust::Notification::new()
            .appname("Vorcall")
            .summary(&author)
            .body(&body)
            // No `sound_name`: on Windows that is what makes a toast audible,
            // and the chime is ours to play.
            .timeout(notify_rust::Timeout::Milliseconds(TIMEOUT_MS))
            .show()
            .map(|_| ())
    })
    .await;

    match sent {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "cannot show the desktop notification"),
        Err(e) => tracing::warn!(error = %e, "the notification thread did not finish"),
    }
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
}
