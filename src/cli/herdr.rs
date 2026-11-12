//! The terminal-browser pane preview inside a herdr tab: finding it, opening
//! one, pointing it at a file, and closing it.
//!
//! Which terminal-browser tab in this herdr tab holds the preview is decided
//! here, in every mode that has one to decide, so nothing outside needs to read
//! `terminal-browser ls` to use a preview.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Failure;
use super::client::Server;
use super::listing::{Listing, Preview};

/// The label put on a pane split for a preview, and the only thing that lets
/// a close take a pane down. A pane that merely runs a browser full of previews
/// may be the user's own shell running `terminal-browser open` in the
/// foreground; closing it would take the shell and anything else in it.
pub const PANE_LABEL: &str = "blueline";

/// This herdr tab and pane, and terminal-browser.
pub struct Herdr<'a> {
    pub tab: String,
    pub pane: String,
    /// An absolute path rather than the bare name, because the split runs it
    /// from a fresh shell in another pane whose PATH may differ.
    pub tb: PathBuf,
    pub server: &'a Server,
}

/// What a lookup found: every preview here, the first being the one to drive.
pub struct Found {
    pub previews: Vec<Preview>,
    listing: Listing,
}

impl Found {
    pub fn first(&self) -> Option<&Preview> {
        self.previews.first()
    }
}

fn die_ls() -> Failure {
    Failure::new("could not read the terminal-browser list - is terminal-browser working?")
}

fn die_panes() -> Failure {
    Failure::new("could not split a herdr pane - is herdr working?")
}

/// Runs a command for its stdout, None when it could not run or failed.
fn output(command: &mut Command) -> Option<String> {
    let out = command
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pane id in `herdr pane split` output.
fn new_pane_id(split: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(split).ok()?;
    let id = value.pointer("/result/pane/pane_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_owned())
}

fn close_pane(pane: &str) {
    quietly(Command::new("herdr").args(["pane", "close", pane]));
}

/// Runs a command quietly, reporting only whether it succeeded.
fn quietly(command: &mut Command) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Herdr<'_> {
    fn tb(&self) -> Command {
        Command::new(&self.tb)
    }

    /// The whole terminal-browser list, every browser in the session. `--all`
    /// rather than this tab's narrowing, because that narrowing leaves out a
    /// browser too busy to say where it is; with --all it stays in with its
    /// pane null, and the absence is something to see. The tab is matched here.
    fn read_listing(&self) -> Result<Listing, Failure> {
        let out = output(self.tb().args(["ls", "--all", "--json"])).ok_or_else(die_ls)?;
        Listing::parse(&out).map_err(|_| die_ls())
    }

    /// This tab's previews. `settle` is for a lookup whose "none" is acted on,
    /// by opening a preview or by refusing: when the list might be missing a
    /// browser it is read once more before none is believed.
    pub fn resolve(&self, settle: bool) -> Result<Found, Failure> {
        let mut listing = self.read_listing()?;
        let mut previews = listing.previews(&self.tab, &self.server.base);
        if previews.is_empty() && settle && listing.unsure(&self.tab) {
            sleep(Duration::from_millis(500));
            listing = self.read_listing()?;
            previews = listing.previews(&self.tab, &self.server.base);
        }
        Ok(Found { previews, listing })
    }

    /// Waits up to `secs` for this tab's preview to be listed with a CDP
    /// target. Listed is not enough: `action` refuses a tab without a target,
    /// and a preview that only counts as up once listed hands that refusal to
    /// whatever runs next.
    pub fn wait_preview(&self, secs: u64) -> Result<Option<Preview>, Failure> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let found = self.resolve(false)?;
            if let Some(p) = found.first().filter(|p| !p.target.is_empty()) {
                return Ok(Some(p.clone()));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(Duration::from_millis(200));
        }
    }

    /// Puts the preview of `file` on screen in this tab, which has none, and
    /// says where it went for the message.
    ///
    /// A browser already in this tab gets the preview as a new tab rather than
    /// a second pane beside it, and its own tabs are left where they were.
    ///
    /// With no browser here, herdr splits a pane off this one with --no-focus
    /// and terminal-browser is started inside it, rather than left to
    /// `terminal-browser open --split`, which splits with --focus and would
    /// drag the user over to this tab. The URL and the command reach the new
    /// shell through its environment, so the typed line is short and needs no
    /// quoting - herdr types it into a tty that refuses lines over 1024 bytes.
    /// The leading space keeps it out of shell history; `exec` makes quitting
    /// the browser close the pane; --no-merge keeps terminal-browser from
    /// deciding a neighbour would do and leaving the pane empty.
    ///
    /// A pane split here is closed again when the preview does not come up in
    /// it, rather than left as an empty shell the next run would not recognise.
    pub fn open_preview(&self, file: &Path) -> Result<&'static str, Failure> {
        let url = self.server.preview_url(file, "pane")?;
        let (opened, new_pane) = match self.browser_here()? {
            Some(browser) => {
                // Its failure shows up as the wait below timing out.
                quietly(self.tb().args(["new-tab", "--browser", &browser, &url]));
                ("in a new tab of this tab's browser", None)
            }
            None => ("in a new pane", Some(self.split_pane(&url)?)),
        };
        // A cold start - the first browser since terminal-browser's shared
        // process last exited - takes a few seconds; a warm one under a second.
        if self.wait_preview(30)?.is_some() {
            return Ok(opened);
        }
        if let Some(pane) = new_pane {
            close_pane(&pane);
        }
        Err(Failure::new(
            "the preview did not come up in terminal-browser within 30s - is it starting or stuck?",
        ))
    }

    /// A browser in this tab to put a new preview tab into, reading the list
    /// once more when it might be missing one.
    fn browser_here(&self) -> Result<Option<String>, Failure> {
        let listing = self.read_listing()?;
        let here = listing.browser_here(&self.tab);
        if here.is_some() || !listing.unsure(&self.tab) {
            return Ok(here);
        }
        sleep(Duration::from_millis(500));
        Ok(self.read_listing()?.browser_here(&self.tab))
    }

    /// Splits a labelled pane off this one and starts terminal-browser on
    /// `url` in it, returning the new pane's id.
    fn split_pane(&self, url: &str) -> Result<String, Failure> {
        let mut tb_env = OsString::from("BLUELINE_TB=");
        tb_env.push(&self.tb);
        let split = output(
            Command::new("herdr")
                .args(["pane", "split", "--pane", &self.pane])
                .args(["--direction", "right", "--no-focus"])
                .args(["--right-click", "pane", "--env"])
                .arg(tb_env)
                .arg("--env")
                .arg(format!("BLUELINE_URL={url}")),
        )
        .ok_or_else(die_panes)?;
        let pane = new_pane_id(&split).ok_or_else(die_panes)?;
        quietly(Command::new("herdr").args(["pane", "rename", &pane, PANE_LABEL]));
        let started = quietly(Command::new("herdr").args([
            "pane",
            "run",
            &pane,
            r#" exec "$BLUELINE_TB" open --no-merge "$BLUELINE_URL""#,
        ]));
        if !started {
            close_pane(&pane);
            return Err(Failure::new(
                "could not start terminal-browser in a new herdr pane - is herdr working?",
            ));
        }
        Ok(pane)
    }

    /// Points an existing preview at `file`. `action -- open` is not the way:
    /// it opens a new tab every time. The page is told to go instead, deferred
    /// to a task of its own so the eval's reply is sent before it unloads.
    ///
    /// A preview with no target yet is a page still starting and is waited
    /// for; only one that still has none, or refuses the eval, is reported as
    /// not answering. --follow brings the preview to the front of its browser,
    /// and `done` afterwards clears the "agent is acting" marker.
    pub fn navigate(&self, preview: &Preview, file: &Path) -> Result<bool, Failure> {
        let preview = if preview.target.is_empty() {
            match self.wait_preview(10)? {
                Some(p) => p,
                None => return Ok(false),
            }
        } else {
            preview.clone()
        };
        let url = serde_json::to_string(&self.server.preview_url(file, "pane")?)
            .expect("a string serializes");
        let script = format!("setTimeout(() => location.replace({url}), 0); true");
        let navigated = quietly(
            self.tb()
                .args(["action", "--follow", "--browser", &preview.key])
                .args(["--tab", &preview.tab, "--", "eval", &script]),
        );
        if navigated {
            self.done(&preview);
        }
        Ok(navigated)
    }

    /// Clears the "agent is acting" marker an action leaves on a tab.
    pub fn done(&self, preview: &Preview) {
        quietly(self.tb().args(["action", "--browser", &preview.key]).args([
            "--tab",
            &preview.tab,
            "done",
        ]));
    }

    fn pane_label(pane: &str) -> Option<String> {
        let out = output(Command::new("herdr").args(["pane", "get", pane]))?;
        serde_json::from_str::<Value>(&out)
            .ok()?
            .pointer("/result/pane/label")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Closes every preview in this tab and waits for them to leave the list,
    /// since a preview still listed would be found again by a reopen. Ok(false)
    /// when there was nothing to close.
    ///
    /// A browser holding nothing but previews, in a pane labelled as ours,
    /// goes with its pane. Any other pane stays and only the preview tabs are
    /// closed: a browser with tabs of its own keeps them, and a pane blueline
    /// did not split is not blueline's to close. A browser whose pane was
    /// closed is also waited out of the list, so that a reopen does not hand
    /// the new preview to a browser about to exit.
    pub fn close_previews(&self) -> Result<bool, Failure> {
        let found = self.resolve(true)?;
        if found.previews.is_empty() {
            return Ok(false);
        }
        let mut gone: Vec<String> = Vec::new();
        for p in &found.previews {
            if !gone.contains(&p.key) && self.close_one(p) {
                gone.push(p.key.clone());
            }
        }
        self.wait_closed(&gone)?;
        Ok(true)
    }

    /// Closes one preview: its pane when the pane is ours and holds nothing
    /// else, otherwise just the tab. True when the whole browser is going.
    fn close_one(&self, p: &Preview) -> bool {
        if p.all && Self::pane_label(&p.pane).as_deref() == Some(PANE_LABEL) {
            close_pane(&p.pane);
            return true;
        }
        quietly(
            self.tb()
                .args(["action", "--browser", &p.key, "--tab", &p.tab])
                .args(["--", "tab", "close", &p.tab]),
        );
        false
    }

    /// Waits for this tab's previews, and the browsers in `gone`, to leave
    /// the list.
    fn wait_closed(&self, gone: &[String]) -> Result<(), Failure> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = self.resolve(false)?;
            if now.previews.is_empty() && now.listing.lingering(gone) == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Failure::new("the preview in this tab did not close"));
            }
            sleep(Duration::from_millis(200));
        }
    }

    /// Runs a `terminal-browser action` command against a preview with this
    /// process's stdio, then `done`. The command's own status is what counts.
    pub fn action(&self, preview: &Preview, command: &[OsString]) -> i32 {
        let status = self
            .tb()
            .args([
                "action",
                "--browser",
                &preview.key,
                "--tab",
                &preview.tab,
                "--",
            ])
            .args(command)
            .status();
        self.done(preview);
        match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(_) => 1,
        }
    }
}
