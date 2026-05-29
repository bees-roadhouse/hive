//! Markdown → HTML shared helper. Trusted-content path — every entry is
//! written by us (CLI, UI compose, or an AI we run), so raw HTML passes
//! through. Add a sanitizer here if untrusted writers ever land.
//!
//! In addition to plain markdown, this module recognizes three mention
//! syntaxes inside body text (NOT inside code spans / fenced blocks):
//!
//!  - `@slug` ... a person mention; renders as a link to `/people/<slug>`.
//!  - `[[type:slug]]` ... a typed entity reference; renders as a link
//!    to `/<type-route>/<slug>`.
//!  - `[[freeform]]` ... a wikilink-style reference resolved via the
//!    entry's `links` table rows; falls back to a
//!    `<span class="mention-broken">` when unresolved.
//!
//! `#tag` is intentionally left alone in the body ... tag chips on the
//! entry meta line already cover tags. The mention pass runs as a
//! source-level rewrite BEFORE pulldown-cmark, with a small code-context
//! state machine so mentions inside `` `code spans` ``, fenced blocks,
//! and indented code blocks pass through verbatim.

use std::collections::HashMap;

use pulldown_cmark::{Parser, html};

/// A resolved mention, e.g. `@pia` → person "Pia Apiara" at `/people/pia`.
/// The detail page can supply these via `MentionContext`; the feed renders
/// without context and accepts that broken slugs 404 ... that's honest.
#[derive(Debug, Clone)]
pub struct ResolvedMention {
    /// The route to link to, e.g. `/people/pia` or `/tasks/fix-traefik`.
    pub href: String,
    /// The text to display inside the `<a>`. Often the entity's display name
    /// (richer than the raw `@slug`).
    pub display: String,
    /// The CSS modifier class, e.g. `mention-person`, `mention-task`.
    pub kind_class: String,
}

/// Side-channel resolution map for the markdown renderer. Keyed by the raw
/// mention token as it appears in the source (`@pia`, `[[task:fix-traefik]]`,
/// `[[freeform reference]]`). When a token isn't in the map, the renderer
/// falls back to a slug-only `<a>` (for `@slug` / `[[type:slug]]`) or a
/// `<span class="mention-broken">` (for unresolved `[[freeform]]`).
#[derive(Debug, Default, Clone)]
pub struct MentionContext {
    pub resolved: HashMap<String, ResolvedMention>,
}

impl MentionContext {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Render markdown source to HTML, with no mention resolution context.
/// Mentions still render as links/spans; the hrefs are slug-only.
pub fn render_markdown(src: &str) -> String {
    render_markdown_with(src, &MentionContext::empty())
}

/// Render markdown source to HTML, using the supplied resolution map to
/// enrich mention link text (e.g. show the person's display name instead
/// of `@slug`) and to resolve `[[freeform]]` wikilinks.
pub fn render_markdown_with(src: &str, ctx: &MentionContext) -> String {
    let rewritten = transform_mentions(src, ctx);
    let parser = Parser::new(&rewritten);
    let mut out = String::with_capacity(rewritten.len());
    html::push_html(&mut out, parser);
    out
}

/// Walk `src` line by line, tracking whether we're inside a fenced/indented
/// code block. Inside code, mentions are left alone. Outside, each line is
/// scanned with a small state machine that skips inline code spans
/// (`` `...` ``, ``` ``...`` ``` ... any run of backticks closed by a matching
/// run) and replaces mention tokens with inline HTML.
fn transform_mentions(src: &str, ctx: &MentionContext) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    let mut in_fence: Option<String> = None; // Some(fence-marker) when inside

    for line in src.split_inclusive('\n') {
        // Strip the trailing newline for matching, but remember it for output.
        let (content, eol) = split_eol(line);

        // Fence open/close detection. Fences are ``` or ~~~ runs of 3+ on
        // a line by themselves (we accept leading whitespace; markdown
        // allows up to 3 spaces). Compare run-by-run so a longer closing
        // fence is still recognized.
        if let Some(open) = &in_fence {
            if is_fence_close(content, open) {
                in_fence = None;
            }
            out.push_str(line);
            continue;
        } else if let Some(fence) = fence_open(content) {
            in_fence = Some(fence);
            out.push_str(line);
            continue;
        }

        // Indented code blocks: 4+ leading spaces OR a hard tab. The
        // surrounding markdown context (e.g. inside a list item) can
        // change this, but for the trusted-content path it's good enough.
        if is_indented_code(content) {
            out.push_str(line);
            continue;
        }

        // Scan the line, skipping inline code spans.
        scan_line(content, ctx, &mut out);
        out.push_str(eol);
    }

    out
}

/// Split a line (which may or may not end in `\n`) into (content, eol).
fn split_eol(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}

/// True if `line` opens a fenced code block. Returns the fence marker on
/// success (e.g. "```" or "~~~~" ... a sequence of 3+ matching backticks
/// or tildes). Accepts up to 3 leading spaces and an optional info string.
fn fence_open(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len() - trimmed.len();
    if leading > 3 {
        return None;
    }
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run_len = trimmed.chars().take_while(|&c| c == first).count();
    if run_len < 3 {
        return None;
    }
    Some(first.to_string().repeat(run_len))
}

/// True if `line` closes the currently open fence. A closing fence is a
/// run of the same character at least as long as the opener, with no
/// other content (after the run), optionally with up to 3 leading spaces.
fn is_fence_close(line: &str, open: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len() - trimmed.len();
    if leading > 3 {
        return false;
    }
    let ch = open.chars().next().unwrap();
    let run_len = trimmed.chars().take_while(|&c| c == ch).count();
    if run_len < open.len() {
        return false;
    }
    trimmed[run_len..].trim().is_empty()
}

/// Indented code block test: 4+ leading spaces or a tab. We DON'T try to
/// handle the "must be preceded by blank line" rule ... false positives
/// here just mean a mention is left as-is, which is the safe direction.
fn is_indented_code(line: &str) -> bool {
    line.starts_with("    ") || line.starts_with('\t')
}

/// Scan one line's content, copying into `out`. Inline code spans are
/// passed through; everything outside them is searched for mention
/// tokens.
fn scan_line(line: &str, ctx: &MentionContext, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];

        // Inline code span: a run of N backticks closed by a matching run
        // of exactly N backticks on the same line. If unclosed, pass the
        // opening backticks through and resume scanning ... markdown will
        // not interpret them as a code span anyway.
        if c == b'`' {
            let run_len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            // Look for a closing run of exactly `run_len` backticks.
            let search_start = i + run_len;
            if let Some(close_off) = find_backtick_close(&bytes[search_start..], run_len) {
                let span_end = search_start + close_off + run_len;
                out.push_str(&line[i..span_end]);
                i = span_end;
                continue;
            }
            // Unclosed run: copy the backticks and move on.
            out.push_str(&line[i..i + run_len]);
            i += run_len;
            continue;
        }

        // `[[...]]` wikilink. Must close on the same line; otherwise it
        // passes through literally.
        if c == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let after_open = i + 2;
            if let Some(rel_end) = find_double_close(&bytes[after_open..]) {
                let inner_start = after_open;
                let inner_end = after_open + rel_end;
                let token_end = inner_end + 2;
                let inner = &line[inner_start..inner_end];
                let raw = &line[i..token_end];
                if let Some(html) = render_wikilink(raw, inner, ctx) {
                    out.push_str(&html);
                    i = token_end;
                    continue;
                }
            }
            // Unclosed or empty `[[...]]` ... emit one `[` and resume.
            out.push('[');
            i += 1;
            continue;
        }

        // `@slug` mention. Must be at a word boundary AND followed by a
        // valid slug char. Skips email-like strings (`nate@host`).
        if c == b'@' && at_word_boundary(bytes, i) {
            let slug_start = i + 1;
            let slug_len = scan_slug(&bytes[slug_start..]);
            if slug_len > 0 {
                let slug_end = slug_start + slug_len;
                let slug = &line[slug_start..slug_end];
                let raw = &line[i..slug_end];
                out.push_str(&render_at_mention(raw, slug, ctx));
                i = slug_end;
                continue;
            }
        }

        // Pass-through. Push one UTF-8 char (not one byte) at a time so we
        // don't split a multibyte codepoint.
        let ch_len = utf8_char_len(c);
        out.push_str(&line[i..i + ch_len]);
        i += ch_len;
    }
}

/// Find the offset in `bytes` of the first run of exactly `target` backticks.
/// (A longer run is NOT a match ... that's the CommonMark rule for inline
/// code spans.)
fn find_backtick_close(bytes: &[u8], target: usize) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        if run == target {
            return Some(i);
        }
        i += run;
    }
    None
}

/// True if a `@` at byte index `i` sits at a word boundary ... start of the
/// slice, or preceded by something that isn't an ascii letter, digit, or
/// underscore. Prevents `nate@host.com` from matching.
fn at_word_boundary(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = bytes[i - 1];
    !(prev.is_ascii_alphanumeric() || prev == b'_')
}

/// Slug grammar for mentions: ascii letters, digits, `-`, `_`. (Mirrors
/// what the mention parser is expected to recognize; matches the common
/// "kebab-case" entity slug.)
fn scan_slug(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n < bytes.len() {
        let b = bytes[n];
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Find the relative byte offset of the FIRST `]]` in `bytes`. Returns
/// `None` if not found.
fn find_double_close(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// UTF-8 byte length of the codepoint that starts at `b`.
fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        // Continuation byte at the start ... treat as 1 to make progress.
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// Render an `@slug` mention. If the resolution map has it, use the rich
/// href + display name; otherwise default to `/people/<slug>` and show
/// `@<slug>` as the link text.
fn render_at_mention(raw: &str, slug: &str, ctx: &MentionContext) -> String {
    if let Some(r) = ctx.resolved.get(raw) {
        format!(
            r#"<a class="mention {kind}" href="{href}">{display}</a>"#,
            kind = escape_attr(&r.kind_class),
            href = escape_attr(&r.href),
            display = escape_html(&r.display),
        )
    } else {
        format!(
            r#"<a class="mention mention-person" href="/people/{slug}">@{slug}</a>"#,
            slug = escape_attr(slug),
        )
    }
}

/// Render a `[[...]]` wikilink. Three shapes:
///   - `[[type:slug]]` ... typed reference; link to `/<type-route>/<slug>`.
///   - `[[freeform]]` ... resolved via the context; fallback to broken.
///
/// Returns `None` if the inner is empty or malformed.
fn render_wikilink(raw: &str, inner: &str, ctx: &MentionContext) -> Option<String> {
    let inner_trimmed = inner.trim();
    if inner_trimmed.is_empty() {
        return None;
    }
    // Context lookup wins regardless of shape ... lets the resolver
    // override the default routing for typed slugs too.
    if let Some(r) = ctx.resolved.get(raw) {
        return Some(format!(
            r#"<a class="mention {kind}" href="{href}">{display}</a>"#,
            kind = escape_attr(&r.kind_class),
            href = escape_attr(&r.href),
            display = escape_html(&r.display),
        ));
    }
    if let Some((kind, slug)) = inner_trimmed.split_once(':') {
        let kind = kind.trim();
        let slug = slug.trim();
        if !kind.is_empty() && !slug.is_empty() && is_valid_slug(slug) {
            let route = type_route(kind);
            return Some(format!(
                r#"<a class="mention mention-{kind_class}" href="/{route}/{slug}">{label}</a>"#,
                kind_class = escape_attr(kind),
                route = escape_attr(route),
                slug = escape_attr(slug),
                label = escape_html(inner_trimmed),
            ));
        }
    }
    // Unresolved freeform.
    Some(format!(
        r#"<span class="mention-broken">[[{inner}]]</span>"#,
        inner = escape_html(inner_trimmed),
    ))
}

/// Map a typed-reference prefix (`task`, `note`, `event`, `journal`,
/// `person`) to its URL segment. Falls back to the prefix itself when
/// unknown ... we don't enumerate every possible kind.
fn type_route(kind: &str) -> &str {
    match kind {
        "task" => "tasks",
        "note" => "notes",
        "event" => "events",
        "journal" => "journal",
        "person" | "people" => "people",
        other => other,
    }
}

/// True if a slug is the kebab-/snake-case shape we accept.
fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Minimal HTML escape for body text appearing inside `<a>`...`</a>`.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Attribute-safe escape (href / class). Same coverage as `escape_html`
/// ... we keep them separate so an attribute can't be expanded later
/// with attribute-specific escaping without touching body callers.
fn escape_attr(s: &str) -> String {
    escape_html(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_markdown_unchanged() {
        let html = render_markdown("# Hello\n\nworld");
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<p>world</p>"));
    }

    #[test]
    fn at_mention_renders_as_link() {
        let html = render_markdown("hi @pia, see you");
        assert!(
            html.contains(r#"<a class="mention mention-person" href="/people/pia">@pia</a>"#),
            "got: {html}"
        );
    }

    #[test]
    fn at_mention_skips_email_like() {
        let html = render_markdown("contact nate@example.com please");
        // The `@example` after `nate` should NOT match (word boundary).
        assert!(!html.contains(r#"class="mention"#), "got: {html}");
    }

    #[test]
    fn typed_wikilink_renders_with_route() {
        let html = render_markdown("see [[task:fix-traefik]] for details");
        assert!(
            html.contains(
                r#"<a class="mention mention-task" href="/tasks/fix-traefik">task:fix-traefik</a>"#
            ),
            "got: {html}"
        );
    }

    #[test]
    fn unresolved_freeform_renders_broken() {
        let html = render_markdown("see [[some random ref]] for more");
        assert!(
            html.contains(r#"<span class="mention-broken">[[some random ref]]</span>"#),
            "got: {html}"
        );
    }

    #[test]
    fn resolved_freeform_uses_context() {
        let mut ctx = MentionContext::empty();
        ctx.resolved.insert(
            "[[traefik outage]]".to_string(),
            ResolvedMention {
                href: "/events/traefik-outage".to_string(),
                display: "Traefik outage".to_string(),
                kind_class: "mention-event".to_string(),
            },
        );
        let html = render_markdown_with("recall [[traefik outage]]", &ctx);
        assert!(
            html.contains(r#"href="/events/traefik-outage""#),
            "got: {html}"
        );
        assert!(html.contains(">Traefik outage<"), "got: {html}");
    }

    #[test]
    fn mention_inside_code_span_is_inviolate() {
        let html = render_markdown("use `@pia` as the slug");
        assert!(html.contains("<code>@pia</code>"), "got: {html}");
        assert!(!html.contains(r#"class="mention"#), "got: {html}");
    }

    #[test]
    fn mention_inside_fenced_block_is_inviolate() {
        let html = render_markdown("```\n@pia and [[task:x]]\n```");
        assert!(html.contains("@pia"), "got: {html}");
        assert!(!html.contains(r#"class="mention"#), "got: {html}");
    }

    #[test]
    fn hashtag_in_body_is_left_alone() {
        let html = render_markdown("see #infra tag");
        // No <a> wrapping the hashtag.
        assert!(!html.contains(r#"<a "#), "got: {html}");
        assert!(html.contains("#infra"), "got: {html}");
    }

    #[test]
    fn unclosed_wikilink_passes_through() {
        let html = render_markdown("a stray [[ token here");
        assert!(html.contains("[[ token here"), "got: {html}");
    }

    #[test]
    fn tilde_fence_protects_mentions() {
        let html = render_markdown("~~~\n@pia\n~~~");
        assert!(!html.contains(r#"class="mention"#), "got: {html}");
    }
}
