//! Assets embedded in the binary and served locally, so that nothing goes out
//! to the network: mermaid.js, highlight.js and its styles, and
//! github-markdown-css. The files live in `assets/vendor`; see the README there for
//! where each came from.

/// One embedded file and the content type it is served as.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

/// Every asset, by the URL path the page asks for it under.
const ASSETS: &[(&str, Asset)] = &[
    (
        "/assets/github-markdown.css",
        Asset {
            content_type: CSS,
            bytes: include_bytes!("../assets/vendor/github-markdown.css"),
        },
    ),
    (
        "/assets/hljs-light.css",
        Asset {
            content_type: CSS,
            bytes: include_bytes!("../assets/vendor/hljs-github.min.css"),
        },
    ),
    (
        "/assets/hljs-dark.css",
        Asset {
            content_type: CSS,
            bytes: include_bytes!("../assets/vendor/hljs-github-dark.min.css"),
        },
    ),
    (
        "/assets/highlight.js",
        Asset {
            content_type: JS,
            bytes: include_bytes!("../assets/vendor/highlight.all.min.js"),
        },
    ),
    (
        "/assets/mermaid.min.js",
        Asset {
            content_type: JS,
            bytes: include_bytes!("../assets/vendor/mermaid.min.js"),
        },
    ),
];

/// The asset served at `path`, if there is one.
pub fn get(path: &str) -> Option<Asset> {
    ASSETS
        .iter()
        .find(|(known, _)| *known == path)
        .map(|(_, asset)| *asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_is_present_and_typed() {
        for (path, asset) in ASSETS {
            assert!(!asset.bytes.is_empty(), "{path} is empty");
            let expected = if path.ends_with(".css") { CSS } else { JS };
            assert_eq!(asset.content_type, expected, "{path}");
        }
        assert!(get("/assets/nope.js").is_none());
    }

    #[test]
    fn highlighter_carries_every_language() {
        let script = String::from_utf8_lossy(get("/assets/highlight.js").unwrap().bytes);
        let grammars = script.matches("grammar compiled for Highlight.js").count();
        // Far more than the 36 common languages the package's bundle alone has;
        // not an exact count, so a release that adds a grammar does not fail it.
        assert!(grammars > 150, "{grammars} grammars");
        for language in ["dockerfile", "powershell", "elixir", "nix"] {
            let marker = format!("`{language}` grammar compiled");
            assert!(script.contains(&marker), "{language}");
        }
    }
}
