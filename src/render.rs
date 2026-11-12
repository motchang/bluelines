//! Markdown rendering: markdown to HTML with GitHub alerts and heading IDs.
//! Mermaid blocks are passed through for the browser to draw, since mermaid
//! needs a DOM, and so is syntax highlighting: code blocks carry their
//! language as a class for highlight.js to pick up in the page.
//!
//! The renderer touches no file system. Where a link or an image points is the
//! server's business (it decides what a relative reference resolves to and
//! what it may serve), so every URL goes through the `rewrite` callback, and
//! the table of contents is left to the server too, built from the headings
//! returned alongside the body.

use std::path::Path;

use comrak::nodes::{AlertType, AstNode, NodeHtmlBlock, NodeValue};
use comrak::{Anchorizer, Arena, Options, format_html, parse_document};

/// The suffixes that count as markdown, compared case-insensitively. `.mdown`
/// is here because the CLI has always accepted it.
const MARKDOWN_EXTENSIONS: [&str; 3] = ["md", "markdown", "mdown"];

/// Whether a path names a markdown file, judged by its suffix alone.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

/// What kind of reference a URL handed to `rewrite` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reference {
    /// The destination of a link, autolinks included.
    Link,
    /// The source of an image.
    Image,
}

/// A heading in the rendered document, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// 1 to 6.
    pub level: u8,
    /// The `id` attribute the heading was given; unique within the document.
    pub id: String,
    /// The heading's content rendered as inline HTML, without the `<hN>` tag.
    pub html: String,
}

/// A rendered document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The document body as HTML.
    pub html: String,
    /// Every heading in the body, for the table of contents.
    pub headings: Vec<Heading>,
}

/// Renders a markdown document to HTML.
///
/// `rewrite` is called once for every link destination and image source, in
/// document order. Returning `Some(url)` replaces the URL; `None` leaves it as
/// the document wrote it.
///
/// Raw HTML in the document is passed through, with the tags in
/// [`FILTERED_TAGS`] neutralised: the page is served under a nonce-only script
/// policy, which is what keeps the rest inert.
pub fn render(
    source: &str,
    rewrite: &mut dyn FnMut(Reference, &str) -> Option<String>,
) -> Rendered {
    let options = options();
    let arena = Arena::new();
    let root = parse_document(&arena, source, &options);

    // Collected up front because the passes below detach and replace nodes,
    // which an iterator over the live tree would trip over.
    let nodes: Vec<&AstNode> = root.descendants().collect();

    // Links first, so that a link inside a heading is already rewritten when
    // the heading is rendered on its own below.
    // Raw HTML is filtered before then too, for the same reason.
    for node in &nodes {
        filter_raw_html(node);
        let mut ast = node.data_mut();
        let (kind, link) = match ast.value {
            NodeValue::Link(ref mut link) => (Reference::Link, link),
            NodeValue::Image(ref mut link) => (Reference::Image, link),
            _ => continue,
        };
        if let Some(url) = rewrite(kind, &link.url) {
            link.url = url;
        }
    }

    let mut anchorizer = Anchorizer::new();
    let mut headings = Vec::new();
    for node in &nodes {
        let value = node.data().value.clone();
        match value {
            NodeValue::Heading(heading) => {
                let id = anchorizer.anchorize(&node.collect_text());
                let html = heading_inner_html(node, &options, heading.level);
                replace_with_html(
                    node,
                    format!(
                        "<h{level} id=\"{id}\">{html}</h{level}>",
                        level = heading.level,
                        id = escape_html(&id),
                    ),
                );
                headings.push(Heading {
                    level: heading.level,
                    id,
                    html,
                });
            }
            NodeValue::CodeBlock(block) => {
                replace_with_html(node, code_block(&block.info, &block.literal));
            }
            NodeValue::Alert(alert) => {
                let title = alert
                    .title
                    .as_deref()
                    .map_or_else(|| alert_title(alert.alert_type).to_owned(), escape_html);
                unwrap_alert(
                    &arena,
                    node,
                    format!(
                        "<div class=\"markdown-alert {class}\">\n\
                         <p class=\"markdown-alert-title\">{icon}{title}</p>",
                        class = alert_class(alert.alert_type),
                        icon = alert_icon(alert.alert_type),
                    ),
                );
            }
            _ => {}
        }
    }

    let mut html = String::new();
    format_html(root, &options, &mut html).expect("writing to a String cannot fail");
    Rendered { html, headings }
}

/// Escapes text for an HTML text node or a quoted attribute value.
///
/// `'` is escaped although nothing here writes a single-quoted attribute, so
/// that a future one cannot be broken out of. `&` goes first or it would
/// re-escape the entities after it.
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Tags that raw HTML in a document may not open. GFM's disallowed raw HTML
/// list, plus the document-level tags that would let a document act on the
/// page around it: `<meta http-equiv="refresh">` navigates the preview away,
/// `<base>` redirects its relative URLs, and `<link>` pulls in remote
/// stylesheets or prefetches.
const FILTERED_TAGS: [&str; 12] = [
    "title",
    "textarea",
    "style",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "script",
    "plaintext",
    "meta",
    "base",
    "link",
];

/// Applies [`filter_tags`] to a raw HTML node; other nodes are left alone.
fn filter_raw_html<'a>(node: &'a AstNode<'a>) {
    match node.data_mut().value {
        NodeValue::HtmlBlock(ref mut block) => block.literal = filter_tags(&block.literal),
        NodeValue::HtmlInline(ref mut literal) => *literal = filter_tags(literal),
        _ => {}
    }
}

/// Neutralises every opening or closing tag in [`FILTERED_TAGS`] by escaping
/// its `<`, the way GFM's tagfilter extension does, so the tag shows as text.
fn filter_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name = after.strip_prefix('/').unwrap_or(after);
        if starts_filtered_tag(name) {
            out.push_str("&lt;");
        } else {
            out.push('<');
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether `name` begins with a filtered tag name, ended the way HTML ends a
/// tag name: by whitespace, `/`, `>` or the end of the input.
fn starts_filtered_tag(name: &str) -> bool {
    FILTERED_TAGS.iter().any(|tag| {
        name.get(..tag.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(tag))
            && name[tag.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_ascii_whitespace() || c == '/' || c == '>')
    })
}

/// GitHub Flavored Markdown, with raw HTML passed through.
fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.alerts = true;
    options.render.r#unsafe = true;
    options
}

/// The markup for a fenced or indented code block.
///
/// Mermaid is handed to the browser as text for mermaid.js to draw. Anything
/// else keeps its language, the first word of the info string, as a class for
/// highlight.js; the `hljs` class gives every block the theme's look even when
/// the language is one highlight.js does not know. A block with no language is
/// marked `nohighlight` so that highlight.js does not guess one.
fn code_block(info: &str, literal: &str) -> String {
    let language = info.split_whitespace().next().unwrap_or("");
    let text = escape_html(literal.strip_suffix('\n').unwrap_or(literal));
    match language {
        "mermaid" => format!("<pre class=\"mermaid\">{text}</pre>"),
        "" => format!("<pre><code class=\"hljs nohighlight\">{text}</code></pre>"),
        _ => format!(
            "<pre><code class=\"hljs language-{}\">{text}</code></pre>",
            escape_html(language)
        ),
    }
}

/// A heading's content as inline HTML: the heading rendered on its own, with
/// its `<hN>` tags taken off.
fn heading_inner_html<'a>(node: &'a AstNode<'a>, options: &Options, level: u8) -> String {
    let mut html = String::new();
    format_html(node, options, &mut html).expect("writing to a String cannot fail");
    let open = format!("<h{level}>");
    let close = format!("</h{level}>");
    let inner = html.trim_end();
    let inner = inner.strip_prefix(&open).unwrap_or(inner);
    let inner = inner.strip_suffix(&close).unwrap_or(inner);
    inner.to_owned()
}

/// Turns a block node into a raw HTML block holding `html`, children dropped.
fn replace_with_html<'a>(node: &'a AstNode<'a>, html: String) {
    while let Some(child) = node.first_child() {
        child.detach();
    }
    node.data_mut().value = NodeValue::HtmlBlock(NodeHtmlBlock {
        block_type: 0,
        literal: html,
    });
}

/// Replaces an alert with its own children, between raw HTML blocks that open
/// and close the alert's `<div>`. This is how the alert gets its icon, which
/// the stock alert renderer does not draw.
fn unwrap_alert<'a>(arena: &'a Arena<'a>, node: &'a AstNode<'a>, open: String) {
    let html_block = |literal: String| -> &'a AstNode<'a> {
        arena.alloc(AstNode::from(NodeValue::HtmlBlock(NodeHtmlBlock {
            block_type: 0,
            literal,
        })))
    };
    node.insert_before(html_block(open));
    while let Some(child) = node.first_child() {
        child.detach();
        node.insert_before(child);
    }
    node.insert_before(html_block("</div>".to_owned()));
    node.detach();
}

fn alert_class(kind: AlertType) -> &'static str {
    match kind {
        AlertType::Note => "markdown-alert-note",
        AlertType::Tip => "markdown-alert-tip",
        AlertType::Important => "markdown-alert-important",
        AlertType::Warning => "markdown-alert-warning",
        AlertType::Caution => "markdown-alert-caution",
    }
}

fn alert_title(kind: AlertType) -> &'static str {
    match kind {
        AlertType::Note => "Note",
        AlertType::Tip => "Tip",
        AlertType::Important => "Important",
        AlertType::Warning => "Warning",
        AlertType::Caution => "Caution",
    }
}

/// The icon GitHub draws in an alert's title: Primer Octicons (MIT licensed),
/// the same ones github.com uses for these alerts.
fn alert_icon(kind: AlertType) -> String {
    let (name, path) = match kind {
        AlertType::Note => (
            "info",
            "M0 8a8 8 0 1 1 16 0A8 8 0 0 1 0 8Zm8-6.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13ZM6.5 7.75A.75.75 0 0 1 7.25 7h1a.75.75 0 0 1 .75.75v2.75h.25a.75.75 0 0 1 0 1.5h-2a.75.75 0 0 1 0-1.5h.25v-2h-.25a.75.75 0 0 1-.75-.75ZM8 6a1 1 0 1 1 0-2 1 1 0 0 1 0 2Z",
        ),
        AlertType::Tip => (
            "light-bulb",
            "M8 1.5c-2.363 0-4 1.69-4 3.75 0 .984.424 1.625.984 2.304l.214.253c.223.264.47.556.673.848.284.411.537.896.621 1.49a.75.75 0 0 1-1.484.211c-.04-.282-.163-.547-.37-.847a8.456 8.456 0 0 0-.542-.68c-.084-.1-.173-.205-.268-.32C3.201 7.75 2.5 6.766 2.5 5.25 2.5 2.31 4.863 0 8 0s5.5 2.31 5.5 5.25c0 1.516-.701 2.5-1.328 3.259-.095.115-.184.22-.268.319-.207.245-.383.453-.541.681-.208.3-.33.565-.37.847a.751.751 0 0 1-1.485-.212c.084-.593.337-1.078.621-1.489.203-.292.45-.584.673-.848.075-.088.147-.173.213-.253.561-.679.985-1.32.985-2.304 0-2.06-1.637-3.75-4-3.75ZM5.75 12h4.5a.75.75 0 0 1 0 1.5h-4.5a.75.75 0 0 1 0-1.5ZM6 15.25a.75.75 0 0 1 .75-.75h2.5a.75.75 0 0 1 0 1.5h-2.5a.75.75 0 0 1-.75-.75Z",
        ),
        AlertType::Important => (
            "report",
            "M0 1.75C0 .784.784 0 1.75 0h12.5C15.216 0 16 .784 16 1.75v9.5A1.75 1.75 0 0 1 14.25 13H8.06l-2.573 2.573A1.458 1.458 0 0 1 3 14.543V13H1.75A1.75 1.75 0 0 1 0 11.25Zm1.75-.25a.25.25 0 0 0-.25.25v9.5c0 .138.112.25.25.25h2a.75.75 0 0 1 .75.75v2.19l2.72-2.72a.749.749 0 0 1 .53-.22h6.5a.25.25 0 0 0 .25-.25v-9.5a.25.25 0 0 0-.25-.25Zm7 2.25v2.5a.75.75 0 0 1-1.5 0v-2.5a.75.75 0 0 1 1.5 0ZM9 9a1 1 0 1 1-2 0 1 1 0 0 1 2 0Z",
        ),
        AlertType::Warning => (
            "alert",
            "M6.457 1.047c.659-1.234 2.427-1.234 3.086 0l6.082 11.378A1.75 1.75 0 0 1 14.082 15H1.918a1.75 1.75 0 0 1-1.543-2.575Zm1.763.707a.25.25 0 0 0-.44 0L1.698 13.132a.25.25 0 0 0 .22.368h12.164a.25.25 0 0 0 .22-.368Zm.53 3.996v2.5a.75.75 0 0 1-1.5 0v-2.5a.75.75 0 0 1 1.5 0ZM9 11a1 1 0 1 1-2 0 1 1 0 0 1 2 0Z",
        ),
        AlertType::Caution => (
            "stop",
            "M4.47.22A.749.749 0 0 1 5 0h6c.199 0 .389.079.53.22l4.25 4.25c.141.14.22.331.22.53v6a.749.749 0 0 1-.22.53l-4.25 4.25A.749.749 0 0 1 11 16H5a.749.749 0 0 1-.53-.22L.22 11.53A.749.749 0 0 1 0 11V5c0-.199.079-.389.22-.53Zm.84 1.28L1.5 5.31v5.38l3.81 3.81h5.38l3.81-3.81V5.31L10.69 1.5ZM8 4a.75.75 0 0 1 .75.75v3.5a.75.75 0 0 1-1.5 0v-3.5A.75.75 0 0 1 8 4Zm0 8a1 1 0 1 1 0-2 1 1 0 0 1 0 2Z",
        ),
    };
    format!(
        "<svg class=\"octicon octicon-{name} mr-2\" viewBox=\"0 0 16 16\" width=\"16\" \
         height=\"16\" aria-hidden=\"true\"><path d=\"{path}\"></path></svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_plain(source: &str) -> Rendered {
        render(source, &mut |_, _| None)
    }

    /// The fixture exercises every feature; its rendering is checked against a
    /// stored snapshot. Run with `UPDATE_SNAPSHOTS=1` to rewrite the snapshot
    /// after an intended change, and review the diff.
    #[test]
    fn fixture_matches_snapshot() {
        let source = include_str!("../tests/fixtures/features.md");
        let rendered = render(source, &mut |kind, url| Some(format!("[{kind:?}:{url}]")));
        let mut actual = rendered.html;
        actual.push_str("\n<!-- headings -->\n");
        for heading in &rendered.headings {
            actual.push_str(&format!(
                "{} #{} {}\n",
                heading.level, heading.id, heading.html
            ));
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/features.html");
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .expect("snapshot missing: run with UPDATE_SNAPSHOTS=1 to create it");
        assert_eq!(
            actual, expected,
            "rendering changed; rerun with UPDATE_SNAPSHOTS=1 if intended"
        );
    }

    #[test]
    fn filtered_tags_are_escaped() {
        let html = render_plain(
            "<meta http-equiv=\"refresh\" content=\"0;url=https://evil.example/\">\n\n\
             <BASE href=\"https://evil.example/\"/>\n\n\
             a <link rel=stylesheet href=https://evil.example/x.css> b\n\n\
             <script>x</script>\n\n\
             <style>p{}</style>\n",
        )
        .html;
        for tag in [
            "<meta", "<BASE", "<link", "<script", "</script", "<style", "</style",
        ] {
            assert!(!html.contains(tag), "{tag} survived in {html}");
        }
        assert!(html.contains("&lt;meta http-equiv"));
        assert!(html.contains("&lt;BASE href"));
        assert!(html.contains("a &lt;link rel"));
        assert!(html.contains("&lt;/script>"));
    }

    #[test]
    fn other_raw_html_passes_through() {
        let html = render_plain("<details><summary>s</summary>\n\n<metadata>x</metadata> <linked> <br/>\n\n</details>\n").html;
        for tag in ["<details>", "<summary>", "<metadata>", "<linked>", "<br/>"] {
            assert!(html.contains(tag), "{tag} missing in {html}");
        }
    }

    #[test]
    fn markdown_extensions() {
        for path in ["a.md", "a.MD", "a.markdown", "dir/a.mdown"] {
            assert!(is_markdown(Path::new(path)), "{path}");
        }
        for path in ["a.txt", "a", "md", "a.md.bak", "a.mdx"] {
            assert!(!is_markdown(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn escape_matches_server() {
        assert_eq!(
            escape_html(r#"<a href="x">&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn rewrite_sees_links_and_images_in_order() {
        let mut seen = Vec::new();
        render(
            "[a](one.md) ![b](two.png) <https://three.example> www.four.example\n\n# [c](five.md)",
            &mut |kind, url| {
                seen.push((kind, url.to_owned()));
                None
            },
        );
        assert_eq!(
            seen,
            [
                (Reference::Link, "one.md".to_owned()),
                (Reference::Image, "two.png".to_owned()),
                (Reference::Link, "https://three.example".to_owned()),
                (Reference::Link, "http://www.four.example".to_owned()),
                (Reference::Link, "five.md".to_owned()),
            ]
        );
    }

    #[test]
    fn rewritten_url_is_used_and_escaped() {
        let rendered = render("[a](x.md) ![b](y.png)", &mut |kind, _| {
            Some(match kind {
                Reference::Link => "/?file=/x.md&surface=tab".to_owned(),
                Reference::Image => "/image?file=/y.png".to_owned(),
            })
        });
        assert_eq!(
            rendered.html,
            "<p><a href=\"/?file=/x.md&amp;surface=tab\">a</a> \
             <img src=\"/image?file=/y.png\" alt=\"b\" /></p>\n"
        );
    }

    #[test]
    fn mermaid_is_escaped_text() {
        let rendered = render_plain("```mermaid\nA-->B & <C>\n```\n");
        assert_eq!(
            rendered.html,
            "<pre class=\"mermaid\">A--&gt;B &amp; &lt;C&gt;</pre>\n"
        );
    }

    #[test]
    fn code_language_is_escaped() {
        let rendered = render_plain("```a\"b\nx\n```\n");
        assert_eq!(
            rendered.html,
            "<pre><code class=\"hljs language-a&quot;b\">x</code></pre>\n"
        );
    }

    #[test]
    fn heading_ids_are_unique_and_github_style() {
        let rendered = render_plain("# Hello, World!\n# Hello, World!\n## Ünïcode & `code`\n");
        let ids: Vec<_> = rendered.headings.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["hello-world", "hello-world-1", "ünïcode--code"]);
        assert_eq!(rendered.headings[2].html, "Ünïcode &amp; <code>code</code>");
    }

    #[test]
    fn alert_with_custom_title_and_nested_content() {
        let rendered = render_plain("> [!WARNING] Mind <this>\n> - one\n> - two\n");
        assert!(
            rendered
                .html
                .contains("octicon-alert mr-2\" viewBox=\"0 0 16 16\""),
            "{}",
            rendered.html
        );
        assert!(
            rendered.html.contains("</svg>Mind &lt;this&gt;</p>"),
            "{}",
            rendered.html
        );
        assert!(
            rendered
                .html
                .ends_with("<ul>\n<li>one</li>\n<li>two</li>\n</ul>\n</div>\n"),
            "{}",
            rendered.html
        );
    }
}
