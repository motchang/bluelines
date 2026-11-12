//! The CLI's side of the render server: talking to it over HTTP on the
//! loopback port, telling ours apart from anything else holding that port, and
//! starting, stopping or replacing it.
//!
//! Which server holds the port is answered by the server itself: `/health`
//! names the program, its version and the executable it runs from, and proves
//! it knows the token in this user's token file (see `auth`). Only a server
//! that has proved that is sent the token, a file path, or anything else of
//! the user's. Nothing is ever signalled: a pid is only a claim, and the
//! process making it may not be the one it names.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Failure;
use crate::auth;

/// What `/health` says about the server that answered it.
#[derive(Debug, Default)]
pub struct Health {
    /// The file a bare `/` renders: the one the pane modes last opened.
    pub file: Option<String>,
    /// The most recent `/open` of any kind.
    pub recent: Option<String>,
    identity: Option<Identity>,
}

/// The fields a blueline server adds to `/health` to say which binary it is.
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    version: String,
    exe: Option<String>,
    proof: Option<String>,
}

/// Who holds the port, as far as `/health` can tell.
#[derive(Debug, PartialEq, Eq)]
pub enum Holder {
    /// Nothing answered.
    Nobody,
    /// This very binary, at this version, knowing this user's token.
    Ours,
    /// Another blueline that knows this user's token: another version, or
    /// another copy of the binary. It renders with code this one does not
    /// have, so it is retired.
    OtherBlueline,
    /// Something that says it is blueline but cannot prove it knows the
    /// token: a blueline that keeps its token somewhere else (another
    /// `XDG_RUNTIME_DIR` or `TMPDIR`), one from before tokens, or a process
    /// that took the port to pass itself off as one. It is asked to stop, and
    /// told nothing else.
    Unverified,
    /// Something answered that does not say it is blueline.
    Unknown,
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl Health {
    /// Reads a `/health` body. Anything that is not a JSON object - a foreign
    /// server answering HTML, say - reads as a server that said nothing.
    pub fn parse(body: &str) -> Health {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) else {
            return Health::default();
        };
        let identity =
            (map.get("server").and_then(Value::as_str) == Some("blueline")).then(|| Identity {
                version: map
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                exe: non_empty_string(map.get("exe")),
                proof: non_empty_string(map.get("proof")),
            });
        Health {
            file: non_empty_string(map.get("file")),
            recent: non_empty_string(map.get("recent")),
            identity,
        }
    }

    /// The file to come back to for `--browser` with no file: the most recent
    /// one, or the current one from a server that does not report `recent`.
    pub fn recent_or_current(&self) -> Option<&str> {
        self.recent.as_deref().or(self.file.as_deref())
    }

    /// Who answered, given the challenge it was sent and the token it should
    /// know.
    fn holder(&self, own_exe: Option<&Path>, token: Option<&str>, challenge: &str) -> Holder {
        let Some(identity) = &self.identity else {
            return Holder::Unknown;
        };
        let proved = match (token, &identity.proof) {
            (Some(token), Some(proof)) => auth::proof_holds(token, challenge, proof),
            _ => false,
        };
        if !proved {
            return Holder::Unverified;
        }
        let same_exe = match (&identity.exe, own_exe) {
            (Some(theirs), Some(ours)) => same_file(Path::new(theirs), ours),
            _ => false,
        };
        if same_exe && identity.version == env!("CARGO_PKG_VERSION") {
            Holder::Ours
        } else {
            Holder::OtherBlueline
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A reply: the status and the body as text.
struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The render server on one loopback port.
pub struct Server {
    pub port: u16,
    pub base: String,
    /// The token, once the server on the port has proved it knows it. Until
    /// then nothing that needs it is sent.
    trusted: RefCell<Option<String>>,
}

impl Server {
    pub fn new(port: u16) -> Server {
        Server {
            port,
            base: format!("http://127.0.0.1:{port}"),
            trusted: RefCell::new(None),
        }
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Every status is a reply worth reading: /open explains a 400 in
            // its body, and that body is what the user needs to see.
            .http_status_as_error(false)
            // Loopback only. A proxy from the environment would carry these
            // requests somewhere that is not this machine's port.
            .proxy(None)
            .build()
            .into()
    }

    /// The Authorization header for a server that has proved itself.
    fn bearer(&self) -> Option<String> {
        self.trusted
            .borrow()
            .as_ref()
            .map(|token| format!("Bearer {token}"))
    }

    fn get(&self, path: &str, timeout: Duration) -> Option<Reply> {
        self.get_with(path, &[], timeout)
    }

    fn get_with(&self, path: &str, headers: &[(&str, &str)], timeout: Duration) -> Option<Reply> {
        let mut request = Self::agent(timeout).get(format!("{}{path}", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(bearer) = self.bearer() {
            request = request.header("authorization", bearer);
        }
        let mut response = request.call().ok()?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().ok()?;
        Some(Reply { status, body })
    }

    fn post(&self, path: &str, body: &str, timeout: Duration) -> Option<Reply> {
        let mut request = Self::agent(timeout)
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json");
        if let Some(bearer) = self.bearer() {
            request = request.header("authorization", bearer);
        }
        let mut response = request.send(body).ok()?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().ok()?;
        Some(Reply { status, body })
    }

    /// Whether anything answers `/health` with a success.
    fn answers(&self, timeout: Duration) -> bool {
        self.get("/health", timeout).is_some_and(|r| r.success())
    }

    /// Who holds the port, sending it a fresh challenge. A server that answers
    /// it is trusted with the token from here on; any other is not, whatever
    /// was trusted before.
    pub fn holder(&self) -> Holder {
        self.trusted.replace(None);
        let challenge = auth::random_hex(16);
        let Some(reply) = self.get_with(
            "/health",
            &[(auth::CHALLENGE_HEADER, &challenge)],
            Duration::from_secs(1),
        ) else {
            return Holder::Nobody;
        };
        if !reply.success() {
            return Holder::Nobody;
        }
        let token = auth::read_token(self.port);
        let own_exe = std::env::current_exe().ok();
        let holder =
            Health::parse(&reply.body).holder(own_exe.as_deref(), token.as_deref(), &challenge);
        if matches!(holder, Holder::Ours | Holder::OtherBlueline) {
            self.trusted.replace(token);
        }
        holder
    }

    /// The files `/health` reports, from a server that has proved itself, or
    /// none: the paths are the user's, and an unproved server would be making
    /// them up.
    pub fn files(&self) -> Health {
        if self.trusted.borrow().is_none() {
            return Health::default();
        }
        self.get("/health", Duration::from_secs(2))
            .filter(Reply::success)
            .map(|reply| Health::parse(&reply.body))
            .unwrap_or_default()
    }

    /// The log a server started from here writes to.
    pub fn log_path() -> PathBuf {
        let dir = std::env::var_os("TMPDIR")
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/tmp".into());
        PathBuf::from(dir).join("blueline-server.log")
    }

    /// Makes sure the server on the port is this binary, starting it when the
    /// port is free and replacing another blueline that holds it. `/health`
    /// alone cannot tell one blueline from another, so without this it would
    /// hand back a server from another install for as long as that process
    /// lived - rendering with code this copy does not have, silently.
    pub fn ensure(&self) -> Result<(), Failure> {
        match self.holder() {
            Holder::Ours => return Ok(()),
            Holder::Nobody => {}
            Holder::OtherBlueline | Holder::Unverified => self.retire()?,
            Holder::Unknown => {
                // Not ours to stop, and not to be handed the user's file paths
                // either: it may be there to collect them.
                return Err(Failure::new(format!(
                    "something on port {} answers but does not say it is blueline - not sending it anything; stop that process, or set BLUELINE_PORT to another port",
                    self.port
                )));
            }
        }
        self.start()
    }

    fn start(&self) -> Result<(), Failure> {
        let log = Self::log_path();
        let not_started = || {
            Failure::new(format!(
                "server did not start on {} - see {}",
                self.base,
                log.display()
            ))
        };
        let exe = std::env::current_exe().map_err(|_| not_started())?;
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .map_err(|e| Failure::new(format!("cannot open {}: {e}", log.display())))?;
        let errors = output.try_clone().map_err(|_| not_started())?;
        // Its own process group, so that a Ctrl-C in the terminal that started
        // it does not take the shared server down with this command.
        let mut child = Command::new(exe)
            .arg("serve")
            .arg("--port")
            .arg(self.port.to_string())
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            .process_group(0)
            .spawn()
            .map_err(|_| not_started())?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.answers(Duration::from_secs(1)) {
                break;
            }
            // A server that has already exited - the port taken by something
            // that does not speak HTTP, say - will not come up by waiting.
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            sleep(Duration::from_millis(100));
        }
        // Whatever answers now has to prove it is the server just started -
        // or one another run started a moment earlier, which is as good - and
        // not something that took the port while this one was starting.
        match self.holder() {
            Holder::Ours => Ok(()),
            Holder::Nobody => Err(not_started()),
            _ => Err(Failure::new(format!(
                "the server that answers on {} is not the one blueline just started and cannot prove it is yours - stop it, or set BLUELINE_PORT to another port",
                self.base
            ))),
        }
    }

    /// Asks the server on the port to stop and waits for the port to come
    /// free, which it has to before a replacement can bind it. The token goes
    /// only to a server that has proved itself; one that has not is asked
    /// without it, which a blueline from before tokens still honours.
    fn retire(&self) -> Result<(), Failure> {
        let asked = self
            .post("/shutdown", "{}", Duration::from_secs(2))
            .is_some_and(|r| r.success());
        self.trusted.replace(None);
        if !asked {
            return Err(Failure::new(format!(
                "the server on port {} would not stop - it may be a blueline that keeps its token somewhere else (another XDG_RUNTIME_DIR or TMPDIR), or a process passing itself off as one; stop it by hand, or set BLUELINE_PORT to another port",
                self.port
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !self.answers(Duration::from_millis(500)) {
                return Ok(());
            }
            sleep(Duration::from_millis(200));
        }
        Err(Failure::new(format!(
            "the server on {} did not stop",
            self.port
        )))
    }

    /// Stops whatever blueline holds the port. Ok(false) when there was none.
    pub fn stop(&self) -> Result<bool, Failure> {
        match self.holder() {
            Holder::Nobody => Ok(false),
            Holder::Ours | Holder::OtherBlueline | Holder::Unverified => {
                self.retire().map(|()| true)
            }
            Holder::Unknown => Err(Failure::new(format!(
                "something on {} answers but does not say it is blueline - not stopping it; stop that process by hand",
                self.port
            ))),
        }
    }

    /// The refusal for a call that needs a server that has proved itself.
    fn unproved(&self) -> Failure {
        Failure::new(format!(
            "the render server on {} has not proved it is yours - not sending it anything",
            self.port
        ))
    }

    /// Registers a file with the server, which is also what reloads a page
    /// already showing it. `current: false` registers it without making it the
    /// file a bare `/` follows - see the browser path for why.
    pub fn open(&self, file: &Path, current: bool) -> Result<(), Failure> {
        if self.trusted.borrow().is_none() {
            return Err(self.unproved());
        }
        let body = serde_json::json!({
            "path": file.to_string_lossy(),
            "current": current,
        })
        .to_string();
        let Some(reply) = self.post("/open", &body, Duration::from_secs(10)) else {
            // No reply is not the same as nothing having happened: the server
            // sets the file and broadcasts the reload before it answers.
            return Err(Failure::new(format!(
                "no reply from the render server on {} - {} may already have reached it",
                self.base,
                file.display()
            )));
        };
        open_outcome(&reply.body).map_err(|reason| {
            Failure::new(format!(
                "the render server on {} would not take {} - {reason}",
                self.port,
                file.display()
            ))
        })
    }

    /// How many browser tabs have this file open, or 0 when that cannot be
    /// told - a silence has to read as "no tabs" so that the preview happens.
    pub fn tab_viewers(&self, file: &Path) -> u64 {
        if self.trusted.borrow().is_none() {
            return 0;
        }
        let path = format!("/viewers?file={}", uri_encode(&file.to_string_lossy()));
        self.get(&path, Duration::from_secs(2))
            .filter(Reply::success)
            .and_then(|r| serde_json::from_str::<Value>(&r.body).ok())
            .and_then(|v| v.get("count").and_then(Value::as_u64))
            .unwrap_or(0)
    }

    /// A single-use ticket that lets one browser read pages.
    fn ticket(&self) -> Result<String, Failure> {
        if self.trusted.borrow().is_none() {
            return Err(self.unproved());
        }
        self.post("/ticket", "{}", Duration::from_secs(2))
            .filter(Reply::success)
            .and_then(|r| serde_json::from_str::<Value>(&r.body).ok())
            .and_then(|v| non_empty_string(v.get("ticket")))
            .ok_or_else(|| {
                Failure::new(format!(
                    "the render server on {} gave no ticket for a preview",
                    self.port
                ))
            })
    }

    /// The URL of one preview surface, `pane` or `tab`, with the file in the
    /// query rather than left to whatever the server thinks is current. That is
    /// what lets several previews show different files at once. Each URL
    /// carries a fresh ticket, which the server swaps for a cookie and drops
    /// from the address on the first visit.
    pub fn preview_url(&self, file: &Path, surface: &str) -> Result<String, Failure> {
        Ok(format!(
            "{}/?file={}&surface={surface}&ticket={}",
            self.base,
            uri_encode(&file.to_string_lossy()),
            self.ticket()?
        ))
    }
}

/// Reads an `/open` reply. Only an object with `ok: true` is consent; anything
/// else is a refusal, with the server's `error` as the reason when it gave one
/// and the start of the body when it did not - a foreign server owes us no JSON.
fn open_outcome(body: &str) -> Result<(), String> {
    let parsed = serde_json::from_str::<Value>(body).ok();
    if let Some(Value::Object(map)) = &parsed {
        if map.get("ok") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        match map.get("error") {
            Some(Value::String(s)) if !s.is_empty() => return Err(s.clone()),
            Some(Value::Null | Value::Bool(false)) | None => {}
            Some(Value::String(_)) => {}
            Some(other) => return Err(other.to_string()),
        }
        // A JSON object with no reason in it falls through to the body itself.
    }
    let first_line: String = body
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    if first_line.is_empty() {
        Err("it refused and said nothing about why".to_owned())
    } else {
        Err(first_line)
    }
}

/// Percent-encodes everything but the unreserved characters, as jq's `@uri`
/// does. A path may hold spaces, and a `&` or `#` in one would otherwise end
/// the query or the URL early.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encoding_matches_jq() {
        assert_eq!(uri_encode("/a b/c&d#e.md"), "%2Fa%20b%2Fc%26d%23e.md");
        assert_eq!(uri_encode("日本.md"), "%E6%97%A5%E6%9C%AC.md");
        assert_eq!(uri_encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn nothing_is_sent_to_an_unproved_server() {
        let server = Server::new(9);
        assert!(server.preview_url(Path::new("/a.md"), "pane").is_err());
        assert!(server.open(Path::new("/a.md"), true).is_err());
        assert_eq!(server.tab_viewers(Path::new("/a.md")), 0);
        assert!(server.files().file.is_none());
    }

    #[test]
    fn health_reads_files_and_identity() {
        let h = Health::parse(r#"{"ok":true,"file":"/a.md","recent":null}"#);
        assert_eq!(h.file.as_deref(), Some("/a.md"));
        assert_eq!(h.recent_or_current(), Some("/a.md"));
        assert_eq!(h.holder(None, None, "c"), Holder::Unknown);

        let h = Health::parse("<html>");
        assert!(h.file.is_none());
        assert_eq!(h.holder(None, None, "c"), Holder::Unknown);
    }

    #[test]
    fn holder_needs_proof_then_compares_exe_and_version() {
        let exe = std::env::current_exe().unwrap();
        let token = auth::new_token();
        let challenge = auth::random_hex(16);
        let body = |version: &str, exe: &str, proof: &str| {
            serde_json::json!({
                "ok": true, "server": "blueline", "version": version,
                "exe": exe, "proof": proof,
            })
            .to_string()
        };
        let proof = auth::proof(&token, &challenge);
        let own = exe.to_string_lossy();
        let holder = |body: &str, token: Option<&str>| {
            Health::parse(body).holder(Some(&exe), token, &challenge)
        };

        let ours = body(env!("CARGO_PKG_VERSION"), &own, &proof);
        assert_eq!(holder(&ours, Some(&token)), Holder::Ours);
        // The same answer means nothing without the token to check it by, or
        // checked by another token.
        assert_eq!(holder(&ours, None), Holder::Unverified);
        assert_eq!(holder(&ours, Some(&auth::new_token())), Holder::Unverified);
        // A proof for another challenge is a replay.
        let replayed = body(env!("CARGO_PKG_VERSION"), &own, &auth::proof(&token, "old"));
        assert_eq!(holder(&replayed, Some(&token)), Holder::Unverified);
        let unproved = body(env!("CARGO_PKG_VERSION"), &own, "");
        assert_eq!(holder(&unproved, Some(&token)), Holder::Unverified);

        let older = body("0.0.0-old", &own, &proof);
        assert_eq!(holder(&older, Some(&token)), Holder::OtherBlueline);
        let elsewhere = body(env!("CARGO_PKG_VERSION"), "/elsewhere/blueline", &proof);
        assert_eq!(holder(&elsewhere, Some(&token)), Holder::OtherBlueline);
    }

    #[test]
    fn open_reply_is_consent_only_when_ok_is_true() {
        assert_eq!(open_outcome(r#"{"ok":true,"file":"/a.md"}"#), Ok(()));
        assert_eq!(
            open_outcome(r#"{"ok":false,"error":"not markdown"}"#),
            Err("not markdown".to_owned())
        );
        // A string "true" is not consent.
        assert!(open_outcome(r#"{"ok":"true"}"#).is_err());
        assert_eq!(open_outcome("forbidden"), Err("forbidden".to_owned()));
        assert_eq!(
            open_outcome(""),
            Err("it refused and said nothing about why".to_owned())
        );
        let long = "x".repeat(500);
        assert_eq!(open_outcome(&long).unwrap_err().len(), 200);
    }
}
