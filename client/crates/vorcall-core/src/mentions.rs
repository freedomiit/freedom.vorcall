//! The `<@id>` mention tokens of a message text: writing them when the user
//! types `@name`, reading them back for rendering, and the fixed reaction
//! palette the composer offers.

/// The wire form of a mention, which is what the server stores and parses.
pub fn token(user_id: i64) -> String {
    format!("<@{user_id}>")
}

/// Rewrites every `@username` naming a known user into its `<@id>` token.
///
/// The longest known username wins at each position, so `@Ana Maria` beats
/// `@Ana` when both exist. Matching ignores case; a name must start at the
/// beginning of the text or after whitespace and end at the end, at whitespace
/// or at `.,!?;:`, which also leaves an already-written `<@7>` alone.
pub fn encode(text: &str, users: &[(i64, String)]) -> String {
    let mut candidates: Vec<(i64, String, usize)> = users
        .iter()
        .filter(|(_, name)| !name.is_empty())
        .map(|(id, name)| (*id, name.to_lowercase(), name.chars().count()))
        .collect();
    if candidates.is_empty() {
        return text.to_owned();
    }
    candidates.sort_by_key(|(_, _, scalars)| std::cmp::Reverse(*scalars));

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous: Option<char> = None;

    while let Some(ch) = rest.chars().next() {
        if ch == '@'
            && previous.is_none_or(char::is_whitespace)
            && let Some((id, name)) = match_name(&rest[1..], &candidates)
        {
            out.push_str(&token(id));
            previous = name.chars().next_back();
            rest = &rest[1 + name.len()..];
            continue;
        }
        out.push(ch);
        previous = Some(ch);
        rest = &rest[ch.len_utf8()..];
    }

    out
}

/// The longest candidate `text` starts with, followed by a word boundary.
fn match_name<'a>(text: &'a str, candidates: &[(i64, String, usize)]) -> Option<(i64, &'a str)> {
    for (id, lowered, scalars) in candidates {
        let end = match text.char_indices().nth(*scalars) {
            Some((end, _)) => end,
            None if text.chars().count() == *scalars => text.len(),
            None => continue,
        };
        let head = &text[..end];
        if head.to_lowercase() != *lowered {
            continue;
        }
        let boundary = text[end..].chars().next().is_none_or(|next| {
            next.is_whitespace() || matches!(next, '.' | ',' | '!' | '?' | ';' | ':')
        });
        if boundary {
            return Some((*id, head));
        }
    }
    None
}

/// One run of a message text: plain characters, or a mention to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Mention { user_id: i64, username: String },
}

/// Splits a stored text into the runs the message view draws.
///
/// A token naming a user the client does not know renders as `unknown` rather
/// than leaking the raw id.
pub fn segments(text: &str, users: &[(i64, String)]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut pending = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("<@") {
        if let Some((user_id, len)) = parse_token(&rest[start..]) {
            pending.push_str(&rest[..start]);
            if !pending.is_empty() {
                out.push(Segment::Text(std::mem::take(&mut pending)));
            }
            let username = users
                .iter()
                .find(|(id, _)| *id == user_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "unknown".to_owned());
            out.push(Segment::Mention { user_id, username });
            rest = &rest[start + len..];
        } else {
            pending.push_str(&rest[..start + 2]);
            rest = &rest[start + 2..];
        }
    }

    pending.push_str(rest);
    if !pending.is_empty() {
        out.push(Segment::Text(pending));
    }

    out
}

/// The id and byte length of the `<@id>` token `text` starts with.
fn parse_token(text: &str) -> Option<(i64, usize)> {
    let digits: String = text[2..].chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !text[2 + digits.len()..].starts_with('>') {
        return None;
    }
    let id = digits.parse().ok()?;
    Some((id, digits.len() + 3))
}

/// Whether the message names `me`, which is what turns a row into a highlight.
pub fn mentions_me(mention_ids: &[i64], me: i64) -> bool {
    mention_ids.contains(&me)
}

/// The reactions the composer offers; the server accepts these and nothing else.
pub const PALETTE: [&str; 8] = ["👍", "❤️", "😂", "😮", "😢", "🔥", "🎉", "👀"];

/// A short ASCII label for a palette emoji, for tooltips and fallback rendering.
pub fn reaction_label(emoji: &str) -> &str {
    match emoji {
        "👍" => "+1",
        "❤️" => "<3",
        "😂" => "lol",
        "😮" => "wow",
        "😢" => "sad",
        "🔥" => "fire",
        "🎉" => "party",
        "👀" => "eyes",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<(i64, String)> {
        vec![
            (7, "ana".to_owned()),
            (9, "Ana Maria".to_owned()),
            (11, "joão".to_owned()),
        ]
    }

    #[test]
    fn encodes_a_known_name() {
        assert_eq!(encode("hi @ana", &users()), "hi <@7>");
    }

    #[test]
    fn prefers_the_longest_name() {
        assert_eq!(encode("@Ana Maria hi", &users()), "<@9> hi");
    }

    #[test]
    fn matches_case_insensitively_including_unicode() {
        assert_eq!(encode("@ANA!", &users()), "<@7>!");
        assert_eq!(encode("@JOÃO", &users()), "<@11>");
    }

    #[test]
    fn leaves_an_unknown_name_alone() {
        assert_eq!(encode("@bob hi", &users()), "@bob hi");
    }

    #[test]
    fn needs_a_boundary_before_the_at_sign() {
        assert_eq!(encode("a@ana", &users()), "a@ana");
    }

    #[test]
    fn needs_a_boundary_after_the_name() {
        assert_eq!(encode("@anaX", &users()), "@anaX");
        assert_eq!(encode("@ana, hi", &users()), "<@7>, hi");
    }

    #[test]
    fn leaves_an_existing_token_alone() {
        assert_eq!(encode("hi <@7> there", &users()), "hi <@7> there");
    }

    #[test]
    fn splits_text_mention_text() {
        assert_eq!(
            segments("hi <@7> there", &users()),
            vec![
                Segment::Text("hi ".to_owned()),
                Segment::Mention {
                    user_id: 7,
                    username: "ana".to_owned()
                },
                Segment::Text(" there".to_owned()),
            ]
        );
    }

    #[test]
    fn merges_adjacent_text_around_a_broken_token() {
        assert_eq!(
            segments("a <@x> b", &users()),
            vec![Segment::Text("a <@x> b".to_owned())]
        );
    }

    #[test]
    fn renders_an_unknown_id_as_unknown() {
        assert_eq!(
            segments("<@404>", &users()),
            vec![Segment::Mention {
                user_id: 404,
                username: "unknown".to_owned()
            }]
        );
    }

    #[test]
    fn plain_text_is_one_segment() {
        assert_eq!(
            segments("nothing here", &users()),
            vec![Segment::Text("nothing here".to_owned())]
        );
    }

    #[test]
    fn a_bare_token_is_one_segment() {
        assert_eq!(
            segments("<@9>", &users()),
            vec![Segment::Mention {
                user_id: 9,
                username: "Ana Maria".to_owned()
            }]
        );
    }

    #[test]
    fn a_written_token_round_trips_through_segments() {
        assert_eq!(
            segments(&token(11), &users()),
            vec![Segment::Mention {
                user_id: 11,
                username: "joão".to_owned()
            }]
        );
    }

    #[test]
    fn mentions_me_only_when_listed() {
        assert!(mentions_me(&[3, 7], 7));
        assert!(!mentions_me(&[3, 7], 9));
        assert!(!mentions_me(&[], 7));
    }

    #[test]
    fn labels_every_palette_entry() {
        let labels: Vec<&str> = PALETTE.iter().map(|emoji| reaction_label(emoji)).collect();
        assert_eq!(
            labels,
            vec!["+1", "<3", "lol", "wow", "sad", "fire", "party", "eyes"]
        );
    }

    #[test]
    fn labels_a_non_palette_emoji_as_itself() {
        assert_eq!(reaction_label("🐧"), "🐧");
    }
}
