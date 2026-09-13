//! X11 backend: the `CLIPBOARD` selection, through `x11-clipboard`.
//!
//! Reading a selection is a round trip through the client that owns it, and
//! anything past the server's maximum request size arrives in `INCR` chunks —
//! which a pasted screenshot always is. `x11-clipboard` runs both halves on top
//! of the same x11rb the rest of the client already links, so nothing here
//! touches the protocol directly.
//!
//! There is no `TARGETS` query: a conversion the owner cannot make comes back
//! as an empty answer, so asking for each target in turn says both whether it
//! is offered and what it holds, in one round trip instead of two.

use std::time::Duration;

use x11_clipboard::{Atom, Clipboard as Selection};

use crate::{Flavour, Pasted, Unavailable, uri};

/// How long one transfer may take before it is given up on. The owner is
/// another application and may be busy, so this is a ceiling, not a target.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

const URI_LIST: &str = "text/uri-list";
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";
const PNG: &str = "image/png";

pub(crate) struct X11 {
    selection: Selection,
}

impl X11 {
    pub(crate) fn new() -> Result<X11, Unavailable> {
        let selection = Selection::new()
            .map_err(|err| Unavailable::Unsupported(format!("cannot reach the X server: {err}")))?;
        Ok(X11 { selection })
    }

    pub(crate) fn read(&self) -> Result<Pasted, Unavailable> {
        let clipboard = self.selection.getter.atoms.clipboard;
        let property = self.selection.getter.atoms.property;

        let atom = |name: &str| -> Result<Atom, String> {
            self.selection
                .getter
                .get_atom(name)
                .map_err(|err| format!("cannot intern {name}: {err}"))
        };
        let load = |target: Atom| -> Result<Vec<u8>, String> {
            self.selection
                .load(clipboard, target, property, TRANSFER_TIMEOUT)
                .map_err(|err| err.to_string())
        };

        Ok(crate::pick(|flavour| match flavour {
            Flavour::Files => {
                for target in [URI_LIST, GNOME_COPIED_FILES] {
                    let bytes = load(atom(target)?)?;
                    if bytes.is_empty() {
                        continue;
                    }
                    let paths = if target == URI_LIST {
                        uri::from_uri_list(&bytes)
                    } else {
                        uri::from_gnome_copied_files(&bytes)
                    };
                    if !paths.is_empty() {
                        return Ok(Some(Pasted::Files(paths)));
                    }
                }
                Ok(None)
            }
            Flavour::Png => {
                let bytes = load(atom(PNG)?)?;
                if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Pasted::Png(bytes)))
                }
            }
            // X11 has no raw-bitmap target of its own; an image is offered as
            // one of the `image/*` targets, encoded.
            Flavour::Rgba => Ok(None),
            Flavour::Text => {
                let bytes = load(self.selection.getter.atoms.utf8_string)?;
                if !bytes.is_empty() {
                    return Ok(Some(Pasted::Text(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    )));
                }
                let bytes = load(self.selection.getter.atoms.string)?;
                if bytes.is_empty() {
                    return Ok(None);
                }
                // STRING is Latin-1 by the ICCCM, so every byte is its own code
                // point — not UTF-8 to be decoded.
                Ok(Some(Pasted::Text(
                    bytes.iter().map(|byte| char::from(*byte)).collect(),
                )))
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the X server that is actually running, over its own selection
    /// protocol: a second connection takes `CLIPBOARD` and offers a URI list,
    /// and the backend reads it back. Run by hand — it needs `DISPLAY`.
    #[test]
    #[ignore = "needs a live X server"]
    fn reads_a_uri_list_a_file_manager_could_have_offered() {
        let owner = Selection::new().expect("an X server");
        let target = owner.getter.get_atom(URI_LIST).expect("the uri-list atom");
        owner
            .store(
                owner.getter.atoms.clipboard,
                target,
                &b"# a comment\r\nfile:///tmp/a%20file.txt\r\nfile:///tmp/caf%C3%A9.txt\r\n"[..],
            )
            .expect("the selection is taken");

        let reader = X11::new().expect("a second connection");
        assert_eq!(
            reader.read().expect("the read completes"),
            Pasted::Files(vec!["/tmp/a file.txt".into(), "/tmp/café.txt".into()])
        );
    }
}
