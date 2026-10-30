//! Mattermost messages are Markdown. This turns them into Pango markup.
//!
//! Not a renderer — a translator into what a `GtkLabel` can already draw.
//! Pango has no block layout, so anything structural (a code block, a table)
//! is left to the caller as a separate [`Block`], and only the inline span
//! markup is produced here.
//!
//! Everything that reaches Pango is escaped first. A message is arbitrary text
//! from another person, and `<span>` in someone's message must render as those
//! six characters and never as a tag.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// The URL scheme mentions are rendered with. Not a real scheme — it exists so
/// a link handler can tell "open this person" from "open this web page".
pub const MENTION_SCHEME: &str = "mm-mention:";

/// A message split into things a widget can be built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Pango markup for a run of prose.
    Text(String),
    /// A fenced or indented code block, and its language when it named one.
    Code { language: String, text: String },
}

/// Splits a message into blocks, translating inline Markdown into Pango markup.
pub fn parse(message: &str) -> Vec<Block> {
    // Everything resolves to itself when nobody says otherwise — used by the
    // tests and by callers with no roster to check against.
    parse_with(message, &|handle| Some(handle.to_string()))
}

/// Parses a message, tinting only the mentions `known` recognises.
/// `known` answers with the name to show for a handle, or `None` when it
/// resolves to nobody.
pub fn parse_with(message: &str, known: &dyn Fn(&str) -> Option<String>) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut code: Option<(String, String)> = None;
    // Nesting depth of list items, so a nested bullet is indented rather than
    // flattened onto the parent.
    let mut list_depth: usize = 0;
    let mut quote_depth: usize = 0;

    let flush = |text: &mut String, blocks: &mut Vec<Block>| {
        let trimmed = text.trim_matches('\n');
        if !trimmed.is_empty() {
            blocks.push(Block::Text(trimmed.to_string()));
        }
        text.clear();
    };

    let mut in_link = false;
    // Consecutive text, gathered: the parser splits a run at every `_` or
    // `*` that might have opened emphasis, which cut a URL in two.
    let mut prose = String::new();
    for event in Parser::new_ext(message, options) {
        if let (Event::Text(t), None, false) = (&event, code.as_ref(), in_link) {
            prose.push_str(t);
            continue;
        }
        if !prose.is_empty() {
            text.push_str(&inline(&prose, known));
            prose.clear();
        }
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut text, &mut blocks);
                let language = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((language, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((language, body)) = code.take() {
                    blocks.push(Block::Code {
                        language,
                        text: body.trim_end_matches('\n').to_string(),
                    });
                }
            }
            Event::Text(t) => match code.as_mut() {
                Some((_, body)) => body.push_str(&t),
                // Inside [text](url) the text is already a link, and a link
                // cannot hold another; prose outside one is gathered above.
                None => text.push_str(&inline_plain(&t, known)),
            },
            // Inline code is a span, not a block: it belongs in the sentence.
            Event::Code(t) => {
                text.push_str("<tt><span background=\"#00000018\"> ");
                text.push_str(&escape(&t));
                text.push_str(" </span></tt>");
            }
            Event::Start(Tag::Strong) => text.push_str("<b>"),
            Event::End(TagEnd::Strong) => text.push_str("</b>"),
            Event::Start(Tag::Emphasis) => text.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => text.push_str("</i>"),
            Event::Start(Tag::Strikethrough) => text.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => text.push_str("</s>"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                in_link = true;
                // Pango's own link markup, so the label handles activation.
                text.push_str(&format!("<a href=\"{}\">", escape(&dest_url)));
            }
            Event::End(TagEnd::Link) => {
                in_link = false;
                text.push_str("</a>");
            }
            Event::Start(Tag::Heading { .. }) => text.push_str("<b><big>"),
            Event::End(TagEnd::Heading(_)) => {
                text.push_str("</big></b>\n");
            }
            Event::Start(Tag::List(_)) => {
                // A nested list opens inside its parent's item, whose text has
                // just been written and has no line break after it yet.
                if list_depth > 0 {
                    text.push('\n');
                }
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 {
                    text.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                text.push_str(&"    ".repeat(list_depth.saturating_sub(1)));
                text.push_str("• ");
            }
            Event::End(TagEnd::Item) => text.push('\n'),
            Event::Start(Tag::BlockQuote(_)) => quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::Paragraph) => {
                if quote_depth > 0 {
                    text.push_str("<i>");
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if quote_depth > 0 {
                    text.push_str("</i>");
                }
                text.push('\n');
            }
            // Tables have no Pango equivalent, so they are laid out as text:
            // cells separated, rows on their own lines. Losing the grid is
            // better than losing the content, which is what dropping the tags
            // did — every cell ran together into one line.
            Event::Start(Tag::TableCell) => {}
            Event::End(TagEnd::TableCell) => text.push_str("  │  "),
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                // Trim the separator the last cell just added.
                while text.ends_with([' ', '│']) {
                    text.pop();
                }
                text.push('\n');
            }
            Event::SoftBreak | Event::HardBreak => text.push('\n'),
            Event::Rule => text.push_str("\n──────\n"),
            // Tables, footnotes, HTML: rendered as their own text rather than
            // dropped, since dropping loses what someone wrote.
            Event::Html(t) | Event::InlineHtml(t) => text.push_str(&escape(&t)),
            _ => {}
        }
    }
    text.push_str(&inline(&prose, known));
    flush(&mut text, &mut blocks);
    blocks
}

/// Escapes for Pango, then substitutes the things Mattermost writes as text
/// but means as something else: `:shortcode:` emoji and `@name` mentions.
///
/// Done after escaping, because both replacements *emit* markup and would
/// otherwise be escaped along with everything else.
/// A bare `https://…` in prose is a link, as every Mattermost client shows
/// it; CommonMark only links one written as `<https://…>`.
fn inline(text: &str, known: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = find_url(rest) {
        out.push_str(&inline_plain(&rest[..start], known));
        let url = url_at(&rest[start..]);
        out.push_str(&format!("<a href=\"{0}\">{0}</a>", escape(url)));
        rest = &rest[start + url.len()..];
    }
    out.push_str(&inline_plain(rest, known));
    out
}

/// Where the next `http://` or `https://` starts, at the start of a word.
fn find_url(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find("http").map(|i| i + from) {
        let starts_word = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let tail = &text[at..];
        if starts_word && (tail.starts_with("https://") || tail.starts_with("http://")) {
            return Some(at);
        }
        from = at + 4;
    }
    None
}

/// The URL at the start of `text`: up to whitespace, less the punctuation a
/// sentence puts after a link. A closing bracket stays when the URL opened
/// one, as Wikipedia's do.
fn url_at(text: &str) -> &str {
    let end = text
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
        .unwrap_or(text.len());
    let mut url = &text[..end];
    loop {
        let trimmed = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']);
        let trimmed = match trimmed.strip_suffix(')') {
            Some(inner) if inner.matches('(').count() < inner.matches(')').count() + 1 => inner,
            _ => trimmed,
        };
        if trimmed.len() == url.len() {
            return url;
        }
        url = trimmed;
    }
}

fn inline_plain(text: &str, known: &dyn Fn(&str) -> Option<String>) -> String {
    let escaped = escape(text);
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped.as_str();

    while let Some(start) = rest.find([':', '@']) {
        out.push_str(&rest[..start]);
        let sigil = rest.as_bytes()[start];
        let after = &rest[start + 1..];

        let end = match sigil {
            b':' => after.find(':').filter(|end| {
                // A shortcode has no spaces in it; "10:30 and" is not one.
                let name = &after[..*end];
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-')
            }),
            _ => Some(
                after
                    .find(|c: char| !(c.is_alphanumeric() || matches!(c, '.' | '-' | '_')))
                    .unwrap_or(after.len()),
            ),
        };

        match (sigil, end) {
            (b':', Some(end)) => {
                let name = &after[..end];
                match crate::emoji::resolve(name) {
                    crate::emoji::Rendered::Unicode(glyph) => out.push_str(glyph),
                    // ponytail: a custom emoji stays as its shortcode in
                    // message bodies. Substituting the image needs a TextView
                    // with child anchors instead of a Label, which is a
                    // rewrite of the whole message row for a rare case; the
                    // shortcode is at least readable, and reaction chips do
                    // draw the picture. Upgrade when custom emoji in prose
                    // actually get in the way.
                    crate::emoji::Rendered::Custom => {
                        out.push(':');
                        out.push_str(name);
                        out.push(':');
                    }
                }
                rest = &after[end + 1..];
            }
            (b'@', Some(end)) if end > 0 && known(&after[..end]).is_some() => {
                // Shown as the name, not the handle: "@Anna Petrova" is who
                // was addressed, and "@apetrova" is an implementation detail
                // of the account. Tinted rather than linked — making it
                // clickable would promise a profile card this does not open.
                let handle = &after[..end];
                let shown = known(handle).unwrap_or_else(|| handle.to_string());
                // A link rather than a coloured span, so clicking it opens the
                // person. The scheme is ours; the label's activate-link
                // handler recognises it and never lets it reach a browser.
                out.push_str(&format!("<a href=\"{MENTION_SCHEME}{}\">@", escape(handle)));
                out.push_str(&escape(&shown));
                out.push_str("</a>");
                rest = &after[end..];
            }
            _ => {
                out.push(sigil as char);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escapes the five characters Pango treats as markup. Public because message
/// rendering builds a little markup of its own.
pub fn escape_for_pango(text: &str) -> String {
    escape(text)
}

/// Escapes the five characters Pango treats as markup.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(message: &str) -> String {
        match parse(message).as_slice() {
            [Block::Text(t)] => t.clone(),
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    #[test]
    fn bare_urls_become_links() {
        assert_eq!(
            text_of("see https://altlinux.space/a/b?x=1&y=2."),
            "see <a href=\"https://altlinux.space/a/b?x=1&amp;y=2\">https://altlinux.space/a/b?x=1&amp;y=2</a>."
        );
        // Brackets: kept when the URL has its own, dropped when they wrap it.
        assert_eq!(
            text_of("(https://en.wikipedia.org/wiki/Rust_(language))"),
            "(<a href=\"https://en.wikipedia.org/wiki/Rust_(language)\">https://en.wikipedia.org/wiki/Rust_(language)</a>)"
        );
        // Not inside a word, and not twice inside a written link.
        assert_eq!(text_of("xhttps://a.b"), "xhttps://a.b");
        assert_eq!(
            text_of("[docs](https://example.com)"),
            "<a href=\"https://example.com\">docs</a>"
        );
    }

    #[test]
    fn inline_styles_become_pango_spans() {
        assert_eq!(
            text_of("**bold** and *soft*"),
            "<b>bold</b> and <i>soft</i>"
        );
        assert_eq!(text_of("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn markup_in_a_message_is_never_markup() {
        // The whole reason to escape: this is someone's text, not a tag.
        assert_eq!(
            text_of("<span foreground='red'>hi</span> & <b>"),
            "&lt;span foreground=&apos;red&apos;&gt;hi&lt;/span&gt; &amp; &lt;b&gt;"
        );
    }

    #[test]
    fn code_fences_come_out_as_their_own_block() {
        let blocks = parse("before\n\n```rust\nlet x = 1;\n```\n\nafter");
        assert_eq!(
            blocks,
            vec![
                Block::Text("before".into()),
                Block::Code {
                    language: "rust".into(),
                    text: "let x = 1;".into()
                },
                Block::Text("after".into()),
            ]
        );
    }

    #[test]
    fn code_content_is_not_escaped_twice() {
        // It goes into a non-markup label, so it must stay literal.
        let blocks = parse("```\n<b>&\n```");
        assert_eq!(
            blocks,
            vec![Block::Code {
                language: String::new(),
                text: "<b>&".into()
            }]
        );
    }

    #[test]
    fn links_keep_their_target() {
        assert_eq!(
            text_of("[docs](https://example.com/a?b=1&c=2)"),
            "<a href=\"https://example.com/a?b=1&amp;c=2\">docs</a>"
        );
    }

    #[test]
    fn lists_are_indented_by_depth() {
        assert_eq!(text_of("- one\n- two"), "• one\n• two");
        assert_eq!(text_of("- one\n    - deep"), "• one\n    • deep");
    }

    #[test]
    fn an_unknown_mention_is_left_alone() {
        let blocks = parse_with("hi @nobody and @anna", &|name| {
            (name == "anna").then(|| "Anna Petrova".to_string())
        });
        let Block::Text(rendered) = &blocks[0] else {
            panic!("expected text");
        };
        assert!(rendered.contains("@nobody"), "got {rendered:?}");
        assert!(!rendered.contains("mm-mention:nobody"), "got {rendered:?}");
        // Shown by name, linked by handle: the label shows who was
        // addressed, the target says which account to open.
        assert!(
            rendered.contains("<a href=\"mm-mention:anna\">@Anna Petrova</a>"),
            "got {rendered:?}"
        );
    }

    #[test]
    fn shortcodes_become_emoji_and_mentions_are_linked() {
        assert_eq!(text_of("nice :tada:"), "nice 🎉");
        // Linked, not merely coloured: clicking a mention opens the person.
        assert!(text_of("hi @anna").contains("<a href=\"mm-mention:anna\">@anna</a>"));
    }

    #[test]
    fn colons_that_are_not_shortcodes_are_left_alone() {
        // A time, a ratio and an unknown name must all survive as written.
        assert_eq!(text_of("at 10:30 sharp"), "at 10:30 sharp");
        assert_eq!(text_of("ratio 4:3"), "ratio 4:3");
        assert_eq!(text_of(":not_an_emoji:"), ":not_an_emoji:");
    }

    #[test]
    fn a_table_keeps_its_cells_on_their_rows() {
        let rendered = text_of("| a | b |\n| - | - |\n| 1 | 2 |");
        assert!(rendered.contains("a  │  b"), "got {rendered:?}");
        assert!(rendered.contains("1  │  2"), "got {rendered:?}");
    }

    #[test]
    fn plain_text_survives_unchanged() {
        assert_eq!(text_of("just a sentence"), "just a sentence");
    }
}
