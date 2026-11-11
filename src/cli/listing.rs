//! Reading `terminal-browser ls --all --json`: which tabs in this herdr tab are
//! previews, and whether the list can be trusted to say there are none.
//!
//! Kept free of process handling so that the rules can be tested on their own.

use serde_json::Value;

/// One preview tab in this herdr tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// The terminal-browser browser holding it.
    pub key: String,
    /// The tab's id inside that browser.
    pub tab: String,
    /// The herdr pane the browser runs in.
    pub pane: String,
    /// Whether every tab in that browser is a preview, which is what decides
    /// whether closing the preview may take the pane with it.
    pub all: bool,
    /// The tab's CDP target, empty while the page is still starting: `ls` lists
    /// a tab before it has one, and every `action` against it fails until then.
    pub target: String,
}

/// A parsed listing. A shape this does not recognise fails rather than reading
/// as "no preview": a quiet mismatch would split off a fresh pane on every run.
#[derive(Debug)]
pub struct Listing {
    browsers: Vec<Value>,
}

/// The listing did not have the shape expected of it.
#[derive(Debug, PartialEq, Eq)]
pub struct BadListing;

/// A scalar as text: strings as they are, numbers written out, anything else
/// (null included) as nothing.
fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn pane_field<'a>(browser: &'a Value, field: &str) -> Option<&'a Value> {
    browser.get("pane").and_then(|p| p.get(field))
}

fn in_tab(browser: &Value, tab: &str) -> bool {
    pane_field(browser, "tab").and_then(Value::as_str) == Some(tab)
}

fn tabs(browser: &Value) -> &[Value] {
    browser
        .get("tabs")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Whether a tab is on the render server at `base`, as a preview.
fn is_preview(tab: &Value, base: &str) -> bool {
    let url = tab.get("url").and_then(Value::as_str).unwrap_or_default();
    url.strip_prefix(base)
        .is_some_and(|path| path == "/" || path.starts_with("/?"))
}

/// One browser's preview tabs, each with whether it is the active one. A
/// browser without a key or a pane cannot be driven and has none.
fn browser_previews(browser: &Value, base: &str) -> Vec<(bool, Preview)> {
    let (Some(key), Some(pane)) = (
        scalar(browser.get("key")),
        scalar(pane_field(browser, "pane")),
    ) else {
        return Vec::new();
    };
    let all_tabs = tabs(browser);
    let here: Vec<&Value> = all_tabs.iter().filter(|t| is_preview(t, base)).collect();
    let all = here.len() == all_tabs.len();
    here.into_iter()
        .filter_map(|t| {
            let preview = Preview {
                key: key.clone(),
                tab: scalar(t.get("id"))?,
                pane: pane.clone(),
                all,
                target: t
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            };
            Some((t.get("active") == Some(&Value::Bool(true)), preview))
        })
        .collect()
}

impl Listing {
    /// Parses the first JSON value in `ls` output. No output at all is an
    /// error too: a terminal-browser that dies without printing must not read
    /// as a tab with no preview in it.
    pub fn parse(output: &str) -> Result<Listing, BadListing> {
        let first = serde_json::Deserializer::from_str(output)
            .into_iter::<Value>()
            .next()
            .ok_or(BadListing)?
            .map_err(|_| BadListing)?;
        match first.get("browsers") {
            Some(Value::Array(browsers)) => Ok(Listing {
                browsers: browsers.clone(),
            }),
            _ => Err(BadListing),
        }
    }

    /// This herdr tab's preview tabs, the active one first.
    ///
    /// A preview is a tab on this render server and nothing else: a browser
    /// that the user opened for something of their own is not one, and
    /// navigating it would throw away whatever they had open. Matching on the
    /// URL also keeps a server on another port out of it. `/?` and a bare `/`
    /// are the two shapes a preview URL takes; `/health` and the other
    /// endpoints are not previews even though they are on the same server.
    pub fn previews(&self, tab: &str, base: &str) -> Vec<Preview> {
        let mut found: Vec<(bool, Preview)> = self
            .browsers
            .iter()
            .filter(|b| in_tab(b, tab))
            .flat_map(|b| browser_previews(b, base))
            .collect();
        // Stable, so the listing's own order holds among equals.
        found.sort_by_key(|(active, _)| !active);
        found.into_iter().map(|(_, p)| p).collect()
    }

    /// Whether the listing might be missing a browser in this tab: one that
    /// did not say where it is in time, or one here whose tabs could not be
    /// read - `ls` reports those as no tabs rather than failing. Either can be
    /// a preview that is there and busy, and taking its absence at face value
    /// splits a second pane beside it.
    pub fn unsure(&self, tab: &str) -> bool {
        self.browsers.iter().any(|b| {
            matches!(pane_field(b, "pane"), None | Some(Value::Null))
                || (in_tab(b, tab) && tabs(b).is_empty())
        })
    }

    /// Any browser in this tab, preview or not, for a new tab to go into
    /// rather than a second pane beside it. A browser with no tabs is passed
    /// over: that is what one whose pane has just closed looks like on its way
    /// out, and a tab handed to it goes down with it.
    pub fn browser_here(&self, tab: &str) -> Option<String> {
        self.browsers
            .iter()
            .filter(|b| in_tab(b, tab) && !tabs(b).is_empty())
            .find_map(|b| scalar(b.get("key")))
    }

    /// How many of the browsers named in `gone` are still listed.
    pub fn lingering(&self, gone: &[String]) -> usize {
        self.browsers
            .iter()
            .filter(|b| scalar(b.get("key")).is_some_and(|k| gone.contains(&k)))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "http://127.0.0.1:43128";

    fn listing(json: &str) -> Listing {
        Listing::parse(json).expect("valid listing")
    }

    #[test]
    fn unexpected_shapes_fail() {
        assert_eq!(Listing::parse("").unwrap_err(), BadListing);
        assert_eq!(Listing::parse("not json").unwrap_err(), BadListing);
        assert_eq!(Listing::parse("{}").unwrap_err(), BadListing);
        assert_eq!(
            Listing::parse(r#"{"browsers":null}"#).unwrap_err(),
            BadListing
        );
        assert!(Listing::parse(r#"{"browsers":[]}"#).is_ok());
    }

    #[test]
    fn picks_previews_in_this_tab_active_first() {
        let l = listing(
            r#"{"browsers":[
              {"key":"b1","pane":{"tab":"t1","pane":"p1"},"tabs":[
                {"id":1,"url":"http://127.0.0.1:43128/?file=%2Fa.md&surface=pane","targetId":"T1"},
                {"id":2,"url":"http://127.0.0.1:43128/","active":true}
              ]},
              {"key":"b2","pane":{"tab":"t2","pane":"p2"},"tabs":[
                {"id":3,"url":"http://127.0.0.1:43128/?file=x","targetId":"T3"}
              ]}
            ]}"#,
        );
        let found = l.previews("t1", BASE);
        assert_eq!(
            found,
            vec![
                Preview {
                    key: "b1".into(),
                    tab: "2".into(),
                    pane: "p1".into(),
                    all: true,
                    target: String::new(),
                },
                Preview {
                    key: "b1".into(),
                    tab: "1".into(),
                    pane: "p1".into(),
                    all: true,
                    target: "T1".into(),
                },
            ]
        );
        assert_eq!(l.previews("t2", BASE).len(), 1);
        assert!(l.previews("t3", BASE).is_empty());
    }

    #[test]
    fn other_pages_and_ports_are_not_previews() {
        let l = listing(
            r#"{"browsers":[
              {"key":"b1","pane":{"tab":"t1","pane":"p1"},"tabs":[
                {"id":1,"url":"http://127.0.0.1:43128/?file=a","targetId":"T1"},
                {"id":2,"url":"https://example.com/"},
                {"id":3,"url":"http://127.0.0.1:43128/health"},
                {"id":4,"url":"http://127.0.0.1:43129/?file=a"},
                {"id":5}
              ]}
            ]}"#,
        );
        let found = l.previews("t1", BASE);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tab, "1");
        assert!(!found[0].all, "the browser holds tabs of its own");
    }

    #[test]
    fn browsers_without_key_or_pane_are_skipped() {
        let l = listing(
            r#"{"browsers":[
              {"key":null,"pane":{"tab":"t1","pane":"p1"},"tabs":[{"id":1,"url":"http://127.0.0.1:43128/"}]},
              {"key":"b2","pane":{"tab":"t1","pane":null},"tabs":[{"id":2,"url":"http://127.0.0.1:43128/"}]}
            ]}"#,
        );
        assert!(l.previews("t1", BASE).is_empty());
    }

    #[test]
    fn unsure_when_a_browser_did_not_say_where_it_is() {
        let placed = r#"{"key":"b1","pane":{"tab":"t1","pane":"p1"},"tabs":[{"id":1,"url":"x"}]}"#;
        assert!(!listing(&format!(r#"{{"browsers":[{placed}]}}"#)).unsure("t1"));
        assert!(
            listing(&format!(
                r#"{{"browsers":[{placed},{{"key":"b2","pane":null,"tabs":null}}]}}"#
            ))
            .unsure("t1")
        );
        let empty_here = r#"{"browsers":[{"key":"b1","pane":{"tab":"t1","pane":"p1"},"tabs":[]}]}"#;
        assert!(listing(empty_here).unsure("t1"));
        assert!(!listing(empty_here).unsure("t2"));
    }

    #[test]
    fn browser_here_skips_empty_browsers() {
        let l = listing(
            r#"{"browsers":[
              {"key":"dying","pane":{"tab":"t1","pane":"p1"},"tabs":[]},
              {"key":"b2","pane":{"tab":"t1","pane":"p2"},"tabs":[{"id":1,"url":"https://example.com"}]}
            ]}"#,
        );
        assert_eq!(l.browser_here("t1").as_deref(), Some("b2"));
        assert_eq!(l.browser_here("t2"), None);
    }

    #[test]
    fn lingering_counts_named_browsers() {
        let l = listing(r#"{"browsers":[{"key":"b1","pane":null},{"key":"b2","pane":null}]}"#);
        assert_eq!(l.lingering(&["b1".into()]), 1);
        assert_eq!(l.lingering(&["b3".into()]), 0);
        assert_eq!(l.lingering(&[]), 0);
    }
}
