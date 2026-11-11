# Vendored browser assets

These files are embedded in the `blueline` binary (see `src/assets.rs`) and
served from the render server, so a preview never fetches anything from the
network. They are copied unmodified from the npm packages below; the tarballs
were checked against the registry's sha512 integrity before copying.

| File | Package | Path in package | License |
| --- | --- | --- | --- |
| `github-markdown.css` | `github-markdown-css@5.9.0` | `github-markdown.css` | MIT, `LICENSE-github-markdown-css` |
| `hljs-github.min.css` | `highlight.js@11.11.1` | `styles/github.min.css` | BSD-3-Clause, `LICENSE-highlight.js` |
| `hljs-github-dark.min.css` | `highlight.js@11.11.1` | `styles/github-dark.min.css` | BSD-3-Clause, `LICENSE-highlight.js` |
| `highlight.min.js` | `@highlightjs/cdn-assets@11.11.1` | `highlight.min.js` | BSD-3-Clause, `LICENSE-highlight.js` |
| `mermaid.min.js` | `mermaid@11.16.0` | `dist/mermaid.min.js` | MIT, `LICENSE-mermaid` |

`mermaid.min.js` is a bundle and includes mermaid's own dependencies under
their respective licenses.

The page's chrome copies a few color values from `github-markdown.css` (see
`STYLE` in `src/server.rs`), so updating that stylesheet means copying them
again.
