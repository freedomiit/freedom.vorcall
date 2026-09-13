//! What a message text carries beyond its characters: the `<@id>` form that
//! names one user, the literal `@everyone` / `@here` words that name a whole
//! channel, the links the view makes clickable, and the fixed reaction palette
//! the composer offers.

/// The two words that name everyone in a channel. They stay literal text on the
/// wire; the server reads them the same way and only honours them for a sender
/// holding `MENTION_EVERYONE`.
pub const EVERYONE: &str = "@everyone";
pub const HERE: &str = "@here";

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
/// `@everyone` and `@here` are left as they are, whoever is registered.
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
        if ch == '@' {
            // The channel-wide words belong to the server, not to whoever
            // registered that username.
            if let Some(literal) = [EVERYONE, HERE]
                .into_iter()
                .find(|literal| literal_at(rest, previous, literal))
            {
                out.push_str(literal);
                previous = literal.chars().next_back();
                rest = &rest[literal.len()..];
                continue;
            }
            if previous.is_none_or(char::is_whitespace)
                && let Some((id, name)) = match_name(&rest[1..], &candidates)
            {
                out.push_str(&token(id));
                previous = name.chars().next_back();
                rest = &rest[1 + name.len()..];
                continue;
            }
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
        if text[end..].chars().next().is_none_or(boundary) {
            return Some((*id, head));
        }
    }
    None
}

/// Whether `rest` opens with `literal` as a whole word: at the start of the text
/// or after whitespace, and followed by the end, whitespace or `.,!?;:`.
/// Case-sensitive, like the server.
fn literal_at(rest: &str, previous: Option<char>, literal: &str) -> bool {
    if !previous.is_none_or(char::is_whitespace) {
        return false;
    }
    let Some(after) = rest.strip_prefix(literal) else {
        return false;
    };
    after.chars().next().is_none_or(boundary)
}

fn boundary(next: char) -> bool {
    next.is_whitespace() || matches!(next, '.' | ',' | '!' | '?' | ';' | ':')
}

fn contains_literal(text: &str, literal: &str) -> bool {
    let mut rest = text;
    let mut previous: Option<char> = None;

    while let Some(ch) = rest.chars().next() {
        if literal_at(rest, previous, literal) {
            return true;
        }
        previous = Some(ch);
        rest = &rest[ch.len_utf8()..];
    }

    false
}

/// Whether the text carries a live `@everyone`. The composer warns on it, and
/// the server only sets the flag for a sender holding `MENTION_EVERYONE`.
pub fn mentions_everyone(text: &str) -> bool {
    contains_literal(text, EVERYONE)
}

/// Whether the text carries a live `@here`, which reaches only the members
/// online at the time and is never counted as an unread mention.
pub fn mentions_here(text: &str) -> bool {
    contains_literal(text, HERE)
}

/// One run of a message text: plain characters, a mention to render, one of the
/// two channel-wide words, or a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Mention {
        user_id: i64,
        username: String,
    },
    Everyone,
    Here,
    /// An `http`/`https` URL exactly as it was written. Nothing else is ever a
    /// link: message text is whatever a sender typed, and a `file:` or
    /// `javascript:` URL the reader can click is a way to hand them something
    /// they never asked for.
    Link(String),
}

/// Splits a stored text into the runs the message view draws.
///
/// A token naming a user the client does not know renders as `unknown` rather
/// than leaking the raw id.
pub fn segments(text: &str, users: &[(i64, String)]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut pending = String::new();
    let mut rest = text;
    let mut previous: Option<char> = None;

    while let Some(ch) = rest.chars().next() {
        if rest.starts_with("<@")
            && let Some((user_id, len)) = parse_token(rest)
        {
            flush(&mut pending, &mut out);
            let username = users
                .iter()
                .find(|(id, _)| *id == user_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "unknown".to_owned());
            out.push(Segment::Mention { user_id, username });
            previous = Some('>');
            rest = &rest[len..];
            continue;
        }

        if let Some((literal, segment)) = [(EVERYONE, Segment::Everyone), (HERE, Segment::Here)]
            .into_iter()
            .find(|(literal, _)| literal_at(rest, previous, literal))
        {
            flush(&mut pending, &mut out);
            out.push(segment);
            previous = literal.chars().next_back();
            rest = &rest[literal.len()..];
            continue;
        }

        if let Some(url) = link_at(rest, previous) {
            flush(&mut pending, &mut out);
            previous = url.chars().next_back();
            let len = url.len();
            out.push(Segment::Link(url.to_owned()));
            rest = &rest[len..];
            continue;
        }

        pending.push(ch);
        previous = Some(ch);
        rest = &rest[ch.len_utf8()..];
    }

    flush(&mut pending, &mut out);

    out
}

fn flush(pending: &mut String, out: &mut Vec<Segment>) {
    if !pending.is_empty() {
        out.push(Segment::Text(std::mem::take(pending)));
    }
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

/// The two schemes a link may carry, and the only two: a reader clicks these,
/// so nothing that reaches the file system or runs anything is ever one.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// The URL `rest` opens with, if it opens with one.
///
/// A link starts at the beginning of the text, after whitespace, or after an
/// opening bracket or quote, and it always carries its scheme: a bare
/// `www.example.com` is text, which keeps the rule small enough to be sure of.
/// It runs to the first whitespace, angle bracket or quote — `<` ends it, so a
/// mention written straight after one stays a mention — and then gives back the
/// punctuation a sentence put after it.
fn link_at(rest: &str, previous: Option<char>) -> Option<&str> {
    if !previous.is_none_or(opens_a_link) {
        return None;
    }
    let scheme = SCHEMES.iter().find(|scheme| {
        rest.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })?;

    let end = rest
        .char_indices()
        .find(|(_, character)| !url_char(*character))
        .map_or(rest.len(), |(at, _)| at);
    let url = trim_trailing(&rest[..end]);

    // A scheme and nothing after it is not a link.
    (url.len() > scheme.len()).then_some(url)
}

/// What may sit right before a link: a sentence's space, or the bracket or
/// quote the sentence put it inside.
fn opens_a_link(previous: char) -> bool {
    previous.is_whitespace() || matches!(previous, '(' | '[' | '{' | '<' | '"' | '\'')
}

/// Whether a character can still be part of a URL. The angle brackets and the
/// quotes are what a text wraps a URL in, never part of one.
fn url_char(character: char) -> bool {
    !character.is_whitespace()
        && !character.is_control()
        && !matches!(character, '<' | '>' | '"' | '`')
}

/// Gives back the punctuation that ended the sentence rather than the URL.
///
/// A closing bracket only goes when the URL did not open one itself, so
/// `(https://example.com/a)` loses its bracket while a Wikipedia
/// `..._(disambiguation)` keeps its own.
fn trim_trailing(url: &str) -> &str {
    let mut url = url;
    while let Some(last) = url.chars().next_back() {
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => count(url, ')') > count(url, '('),
            ']' => count(url, ']') > count(url, '['),
            '}' => count(url, '}') > count(url, '{'),
            _ => false,
        };
        if !drop {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    url
}

fn count(text: &str, character: char) -> usize {
    text.chars().filter(|found| *found == character).count()
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
    fn leaves_the_channel_words_alone_even_as_usernames() {
        let users = vec![(7, "everyone".to_owned()), (8, "here".to_owned())];
        assert_eq!(encode("@everyone and @here", &users), "@everyone and @here");
        assert_eq!(encode("@everyone, hi", &users), "@everyone, hi");
        // Not a whole word, so the username match applies as usual.
        assert_eq!(encode("@everyones", &users), "@everyones");
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
    fn the_channel_words_are_their_own_segments() {
        assert_eq!(
            segments("hi @everyone!", &users()),
            vec![
                Segment::Text("hi ".to_owned()),
                Segment::Everyone,
                Segment::Text("!".to_owned()),
            ]
        );
        assert_eq!(segments("@here", &users()), vec![Segment::Here]);
        assert_eq!(
            segments("@everyone, listen", &users()),
            vec![Segment::Everyone, Segment::Text(", listen".to_owned())]
        );
        assert_eq!(
            segments("<@7> @here", &users()),
            vec![
                Segment::Mention {
                    user_id: 7,
                    username: "ana".to_owned()
                },
                Segment::Text(" ".to_owned()),
                Segment::Here,
            ]
        );
    }

    #[test]
    fn a_channel_word_without_boundaries_is_text() {
        for text in ["email@everyone.com", "x@here", "@everyones", "@HERE"] {
            assert_eq!(
                segments(text, &users()),
                vec![Segment::Text(text.to_owned())],
                "{text} is plain text"
            );
        }
    }

    #[test]
    fn a_bare_url_is_one_link() {
        assert_eq!(
            segments("https://example.com", &users()),
            vec![Segment::Link("https://example.com".to_owned())]
        );
        assert_eq!(
            segments("http://example.com/a/b", &users()),
            vec![Segment::Link("http://example.com/a/b".to_owned())]
        );
    }

    #[test]
    fn a_url_keeps_the_sentence_punctuation_out() {
        assert_eq!(
            segments("see https://example.com/a.", &users()),
            vec![
                Segment::Text("see ".to_owned()),
                Segment::Link("https://example.com/a".to_owned()),
                Segment::Text(".".to_owned()),
            ]
        );
        assert_eq!(
            segments("https://example.com/a, then", &users()),
            vec![
                Segment::Link("https://example.com/a".to_owned()),
                Segment::Text(", then".to_owned()),
            ]
        );
    }

    #[test]
    fn a_url_in_parentheses_keeps_the_bracket_out() {
        assert_eq!(
            segments("(https://example.com/b)", &users()),
            vec![
                Segment::Text("(".to_owned()),
                Segment::Link("https://example.com/b".to_owned()),
                Segment::Text(")".to_owned()),
            ]
        );
    }

    #[test]
    fn a_url_keeps_the_brackets_it_opened_itself() {
        let url = "https://en.wikipedia.org/wiki/Vorcall_(disambiguation)";
        assert_eq!(segments(url, &users()), vec![Segment::Link(url.to_owned())]);
        // The one the sentence added still goes.
        assert_eq!(
            segments(&format!("({url})"), &users()),
            vec![
                Segment::Text("(".to_owned()),
                Segment::Link(url.to_owned()),
                Segment::Text(")".to_owned()),
            ]
        );
    }

    #[test]
    fn a_query_string_stays_whole() {
        let url = "https://example.com/search?q=vorcall&lang=pt-BR&page=2";
        assert_eq!(segments(url, &users()), vec![Segment::Link(url.to_owned())]);
    }

    #[test]
    fn only_http_and_https_are_links() {
        for text in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vorcall://join/7",
            "www.example.com",
            "ftp://example.com/pub",
            "https://",
        ] {
            assert_eq!(
                segments(text, &users()),
                vec![Segment::Text(text.to_owned())],
                "{text} is plain text"
            );
        }
        // A scheme in the middle of a word is not a link either.
        assert_eq!(
            segments("xhttps://example.com", &users()),
            vec![Segment::Text("xhttps://example.com".to_owned())]
        );
    }

    #[test]
    fn a_link_and_a_mention_stay_apart() {
        assert_eq!(
            segments("<@7> https://example.com/x", &users()),
            vec![
                Segment::Mention {
                    user_id: 7,
                    username: "ana".to_owned()
                },
                Segment::Text(" ".to_owned()),
                Segment::Link("https://example.com/x".to_owned()),
            ]
        );
        assert_eq!(
            segments("https://example.com/x <@7>", &users()),
            vec![
                Segment::Link("https://example.com/x".to_owned()),
                Segment::Text(" ".to_owned()),
                Segment::Mention {
                    user_id: 7,
                    username: "ana".to_owned()
                },
            ]
        );
        // A mention written straight after a URL is still a mention: `<` ends
        // the URL.
        assert_eq!(
            segments("https://example.com/x<@7>", &users()),
            vec![
                Segment::Link("https://example.com/x".to_owned()),
                Segment::Mention {
                    user_id: 7,
                    username: "ana".to_owned()
                },
            ]
        );
        // And what looks like a channel word inside a URL is part of the URL.
        assert_eq!(
            segments("https://example.com/@everyone", &users()),
            vec![Segment::Link("https://example.com/@everyone".to_owned())]
        );
    }

    #[test]
    fn text_without_a_url_is_untouched() {
        for text in [
            "nothing here",
            "a http b",
            "ratio 3:1",
            "email@everyone.com",
        ] {
            assert_eq!(
                segments(text, &users()),
                vec![Segment::Text(text.to_owned())],
                "{text} is plain text"
            );
        }
    }

    #[test]
    fn a_url_may_carry_a_non_ascii_path() {
        assert_eq!(
            segments("veja https://example.com/café.", &users()),
            vec![
                Segment::Text("veja ".to_owned()),
                Segment::Link("https://example.com/café".to_owned()),
                Segment::Text(".".to_owned()),
            ]
        );
        let cyrillic = "https://пример.рф/путь";
        assert_eq!(
            segments(cyrillic, &users()),
            vec![Segment::Link(cyrillic.to_owned())]
        );
    }

    #[test]
    fn mentions_everyone_and_here_read_the_same_rule() {
        assert!(mentions_everyone("hey @everyone"));
        assert!(mentions_everyone("@everyone,"));
        assert!(mentions_everyone("a\n@everyone b"));
        assert!(!mentions_everyone("email@everyone.com"));
        assert!(!mentions_everyone("@everyones"));
        assert!(!mentions_everyone("@Everyone"));
        assert!(!mentions_everyone("@here"));

        assert!(mentions_here("@here!"));
        assert!(mentions_here("ping @here please"));
        assert!(!mentions_here("x@here"));
        assert!(!mentions_here("@here2"));
        assert!(!mentions_here("hey @everyone"));
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
