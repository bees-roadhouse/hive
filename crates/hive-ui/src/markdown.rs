//! Markdown → HTML shared helper. Trusted-content path — every entry is
//! written by us (CLI, UI compose, or an AI we run), so raw HTML passes
//! through. Add a sanitizer here if untrusted writers ever land.

use pulldown_cmark::{Parser, html};

/// Render markdown source to HTML.
pub fn render_markdown(src: &str) -> String {
    let parser = Parser::new(src);
    let mut out = String::with_capacity(src.len());
    html::push_html(&mut out, parser);
    out
}
