//! The bundled colour emoji face, and why Linux needs one.
//!
//! Fedora — and most other distributions by now — ships Noto Color Emoji as a
//! COLRv1 build, whose base glyphs are empty outlines with every colour held in
//! a `COLR` version 1 table. swash, the rasteriser under iced's text stack,
//! reads `COLR` version 0 records only and never checks the version, so that
//! face scales to a blank glyph. cosmic-text reaches emoji through the fallback
//! family name "Noto Color Emoji" and, among the faces carrying that name,
//! takes the lowest face id — a system one, loaded first — so a bundled face
//! wins only once every system face of the family is out of the database.
//!
//! The bundled file is Google's CBDT build of Noto Color Emoji v2.047
//! (`assets/fonts/NotoColorEmoji.ttf`, 10 643 852 bytes, sha256
//! `39ee3c587e10e89669b9ff32703261d10d5f9c4dd5ad147b6b5a1c5200591817`) under
//! the SIL Open Font License 1.1, kept beside it as
//! `assets/fonts/LICENSE-NotoColorEmoji.txt`.

/// The family cosmic-text's Unix fallback list ends on, and the one the bundled
/// face announces itself under.
#[cfg(target_os = "linux")]
const FAMILY: &str = "Noto Color Emoji";

/// `include_bytes!` is relative to this file: `src/` -> `vorcall-app/` ->
/// `crates/` -> `client/` -> the repository root.
#[cfg(target_os = "linux")]
const NOTO_COLOR_EMOJI: &[u8] = include_bytes!("../../../../assets/fonts/NotoColorEmoji.ttf");

/// Retires the system emoji faces and puts the bundled one in their place.
///
/// Called once at startup, before any text is shaped. Idempotent: a second call
/// finds no system face left to remove, and `load_font` deduplicates a borrowed
/// slice by its address. A no-op off Linux, whose binaries carry none of the
/// bytes.
pub fn install() {
    #[cfg(target_os = "linux")]
    install_linux();
}

#[cfg(target_os = "linux")]
fn install_linux() {
    use std::borrow::Cow;

    use iced::advanced::graphics::text::cosmic_text::fontdb::Source;
    use iced::advanced::graphics::text::font_system;

    let mut fonts = font_system().write().expect("write the font system");

    // Anything already loaded from bytes is ours; a face read off the disk is
    // the platform's, whatever the file behind it turned into.
    let system: Vec<_> = fonts
        .raw()
        .db()
        .faces()
        .filter(|face| {
            !matches!(face.source, Source::Binary(_))
                && face.families.iter().any(|(name, _)| name == FAMILY)
        })
        .map(|face| face.id)
        .collect();
    let retired = system.len();

    let db = fonts.raw().db_mut();
    for id in system {
        db.remove_face(id);
    }

    fonts.load_font(Cow::Borrowed(NOTO_COLOR_EMOJI));
    tracing::info!(retired, "installed the bundled colour emoji face");
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use iced::advanced::graphics::text::cosmic_text::{
        Attrs, Buffer, Metrics, Shaping, SwashCache, SwashContent, fontdb,
    };
    use iced::advanced::graphics::text::font_system;

    use super::{FAMILY, install};

    /// The whole point of the module: an emoji has to shape onto the bundled
    /// face and scale to a colour bitmap with pixels in it. The COLRv1 face this
    /// replaces fails the last two assertions — it yields an empty outline, not
    /// a colour image.
    #[test]
    fn a_colour_emoji_rasterises_from_the_bundled_face() {
        install();

        let mut fonts = font_system().write().expect("write the font system");
        let raw = fonts.raw();

        let mut buffer = Buffer::new(raw, Metrics::new(16.0, 20.0));
        buffer.set_text(raw, "🔥", &Attrs::new(), Shaping::Advanced, None);
        buffer.shape_until_scroll(raw, false);

        let (font_id, cache_key) = {
            let run = buffer.layout_runs().next().expect("a shaped line");
            let glyph = run.glyphs.first().expect("a shaped glyph");
            (glyph.font_id, glyph.physical((0.0, 0.0), 1.0).cache_key)
        };

        let survivors = raw
            .db()
            .faces()
            .filter(|face| {
                !matches!(face.source, fontdb::Source::Binary(_))
                    && face.families.iter().any(|(name, _)| name == FAMILY)
            })
            .count();
        assert_eq!(survivors, 0, "a system face of {FAMILY} outlived install()");

        let source = &raw.db().face(font_id).expect("the shaped face").source;
        assert!(
            matches!(source, fontdb::Source::Binary(_)),
            "the emoji shaped onto a face that is not the bundled one"
        );

        let mut cache = SwashCache::new();
        let image = cache
            .get_image_uncached(raw, cache_key)
            .expect("a scaled glyph image");
        assert_eq!(image.content, SwashContent::Color);
        assert!(image.placement.width > 0 && image.placement.height > 0);
    }
}
