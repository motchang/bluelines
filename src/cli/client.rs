//! The CLI's side of the render server: talking to it over HTTP on the
//! loopback port, telling ours apart from anything else holding that port, and
//! starting, stopping or replacing it.
//!
//! Which server holds the port is answered by the server itself: `/health`
//! names the program, its version, its pid and the executable it runs from.
//! That replaces the `lsof` / `ps` / `curl` / `jq` pipeline of the sh version,
//! and with it the dependency on all four being installed.

use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Failure;

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
    pid: Option<u32>,
    exe: Option<String>,
}

/// Who holds the port, as far as `/health` can tell.
#[derive(Debug, PartialEq, Eq)]
pub enum Holder {
    /// Nothing answered.
    Nobody,
    /// This very binary, at this version.
    Ours,
    /// Another blueline: another version, or another copy of the binary. It
    /// renders with code this one does not have, so it is retired.
    OtherBlueline { pid: Option<u32> },
    /// Something answered that does not say it is blueline - an older
    /// blueline-server, or another program. Neither is ours to kill.
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
                pid: map
                    .get("pid")
                    .and_then(Value::as_u64)
                    .and_then(|p| u32::try_from(p).ok()),
                exe: non_empty_string(map.get("exe")),
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

    fn holder(&self, own_exe: Option<&Path>) -> Holder {
        let Some(identity) = &self.identity else {
            return Holder::Unknown;
        };
        let same_exe = match (&identity.exe, own_exe) {
            (Some(theirs), Some(ours)) => same_file(Path::new(theirs), ours),
            _ => false,
        };
        if same_exe && identity.version == env!("CARGO_PKG_VERSION") {
            Holder::Ours
        } else {
            Holder::OtherBlueline { pid: identity.pid }
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

/// The render server on one loopback port.
pub struct Server {
    pub port: u16,
    pub base: String,
}

impl Server {
    pub fn new(port: u16) -> Server {
        Server {
            port,
            base: format!("http://127.0.0.1:{port}"),
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

    fn get(&self, path: &str, timeout: Duration) -> Option<Reply> {
        let mut response = Self::agent(timeout)
            .get(format!("{}{path}", self.base))
            .call()
            .ok()?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().ok()?;
        Some(Reply { status, body })
    }

    fn post(&self, path: &str, body: &str, timeout: Duration) -> Option<Reply> {
        let mut response = Self::agent(timeout)
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .send(body)
            .ok()?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().ok()?;
        Some(Reply { status, body })
    }

    /// `/health`, or None when nothing answered with a success.
    pub fn health(&self, timeout: Duration) -> Option<Health> {
        let reply = self.get("/health", timeout)?;
        (200..300)
            .contains(&reply.status)
            .then(|| Health::parse(&reply.body))
    }

    pub fn holder(&self) -> Holder {
        match self.health(Duration::from_secs(1)) {
            None => Holder::Nobody,
            Some(health) => health.holder(std::env::current_exe().ok().as_deref()),
        }
    }

    /// The log a server started from here writes to: `server.log` in a
    /// directory only this user can use. That is `$XDG_STATE_HOME/blueline`,
    /// else `~/.local/state/blueline`, and only without a home directory
    /// `blueline-<uid>` in `$TMPDIR` or `/tmp`.
    pub fn log_path() -> PathBuf {
        log_dir(
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("HOME"),
            std::env::var_os("TMPDIR"),
            current_uid(),
        )
        .join("server.log")
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
            Holder::OtherBlueline { pid } => {
                self.retire(pid)?;
            }
            Holder::Unknown => {
                // Not ours to kill, and refusing would trade a check that only
                // matters when something else is already here for a preview
                // that never works at all. What must not be skipped is saying
                // so, because the failure it leaves open - rendering through
                // other code - shows up as nothing more than a stale page.
                eprintln!(
                    "blueline: the server already on {} does not say it is blueline (an older blueline-server, or another program) - if the preview renders stale, stop that process by hand",
                    self.port
                );
                return Ok(());
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
        let output = open_log(&log)
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
            if self.health(Duration::from_secs(1)).is_some() {
                return Ok(());
            }
            // A server that has already exited - the port taken by something
            // that does not speak HTTP, say - will not come up by waiting.
            if !matches!(child.try_wait(), Ok(None)) {
                break;
            }
            sleep(Duration::from_millis(100));
        }
        // Another run may have started the server while this one's copy lost
        // the race for the port; a server that answers is the one wanted.
        if self.health(Duration::from_secs(1)).is_some() {
            return Ok(());
        }
        Err(not_started())
    }

    /// Stops another blueline on the port and waits for the port to come free,
    /// which it has to before a replacement can bind it.
    fn retire(&self, pid: Option<u32>) -> Result<(), Failure> {
        let asked = self
            .post("/shutdown", "{}", Duration::from_secs(2))
            .is_some_and(|r| (200..300).contains(&r.status));
        if let (false, Some(pid)) = (asked, pid) {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.health(Duration::from_millis(500)).is_none() {
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
            Holder::Ours => self.retire(None).map(|()| true),
            Holder::OtherBlueline { pid } => self.retire(pid).map(|()| true),
            Holder::Unknown => Err(Failure::new(format!(
                "something on {} answers but does not say it is blueline - not stopping it; stop that process by hand",
                self.port
            ))),
        }
    }

    /// Registers a file with the server, which is also what reloads a page
    /// already showing it. `current: false` registers it without making it the
    /// file a bare `/` follows - see the browser path for why.
    pub fn open(&self, file: &Path, current: bool) -> Result<(), Failure> {
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
        let path = format!("/viewers?file={}", uri_encode(&file.to_string_lossy()));
        self.get(&path, Duration::from_secs(2))
            .filter(|r| (200..300).contains(&r.status))
            .and_then(|r| serde_json::from_str::<Value>(&r.body).ok())
            .and_then(|v| v.get("count").and_then(Value::as_u64))
            .unwrap_or(0)
    }

    /// The URL of one preview surface, `pane` or `tab`, with the file in the
    /// query rather than left to whatever the server thinks is current. That is
    /// what lets several previews show different files at once.
    pub fn preview_url(&self, file: &Path, surface: &str) -> String {
        format!(
            "{}/?file={}&surface={surface}",
            self.base,
            uri_encode(&file.to_string_lossy())
        )
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

/// The directory [`Server::log_path`] puts the log in. Relative values are
/// ignored, as the XDG base directory spec asks.
fn log_dir(
    state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    tmpdir: Option<std::ffi::OsString>,
    uid: u32,
) -> PathBuf {
    let absolute = |value: Option<std::ffi::OsString>| {
        value.map(PathBuf::from).filter(|path| path.is_absolute())
    };
    if let Some(state) = absolute(state_home) {
        return state.join("blueline");
    }
    if let Some(home) = absolute(home) {
        return home.join(".local/state/blueline");
    }
    absolute(tmpdir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("blueline-{uid}"))
}

fn current_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Opens the server log for appending, refusing anything another user could
/// have planted. Its directory is created private (0700) or must already be a
/// real directory of ours, which is then made private; the log itself is
/// created 0600, never opened through a symlink, and must be a regular file of
/// ours. A shared `/tmp` is the case this is for: there, someone else could
/// otherwise pre-create the log to read it, or point it somewhere with a link.
fn open_log(path: &Path) -> io::Result<File> {
    let uid = current_uid();
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the log path has no directory"))?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.file_type().is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if meta.uid() != uid {
        return Err(io::Error::other(format!(
            "{} belongs to another user",
            dir.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() || meta.uid() != uid {
        return Err(io::Error::other(
            "not a regular file of this user's".to_owned(),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_prefers_per_user_locations() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            log_dir(os("/state"), os("/home/me"), os("/tmp/x"), 7),
            PathBuf::from("/state/blueline")
        );
        assert_eq!(
            log_dir(os("rel"), os("/home/me"), None, 7),
            PathBuf::from("/home/me/.local/state/blueline")
        );
        assert_eq!(
            log_dir(None, None, os("/var/tmp"), 7),
            PathBuf::from("/var/tmp/blueline-7")
        );
        assert_eq!(
            log_dir(None, os(""), None, 7),
            PathBuf::from("/tmp/blueline-7")
        );
    }

    #[test]
    fn open_log_creates_private_dir_and_file() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("a/blueline/server.log");
        drop(open_log(&log).unwrap());
        let dir_mode = std::fs::metadata(log.parent().unwrap()).unwrap().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&log).unwrap().mode() & 0o777, 0o600);
        // Reopening appends to the same file.
        drop(open_log(&log).unwrap());
    }

    #[test]
    fn open_log_tightens_an_existing_dir_of_ours() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("blueline");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o777)).unwrap();
        drop(open_log(&dir.join("server.log")).unwrap());
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn open_log_refuses_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        let dir = root.path().join("blueline");
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("server.log")).unwrap();
        assert!(open_log(&dir.join("server.log")).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");

        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&dir, &linked).unwrap();
        assert!(open_log(&linked.join("server.log")).is_err());
    }

    #[test]
    fn uri_encoding_matches_jq() {
        assert_eq!(uri_encode("/a b/c&d#e.md"), "%2Fa%20b%2Fc%26d%23e.md");
        assert_eq!(uri_encode("日本.md"), "%E6%97%A5%E6%9C%AC.md");
        assert_eq!(uri_encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn preview_url_carries_file_and_surface() {
        let server = Server::new(43128);
        assert_eq!(
            server.preview_url(Path::new("/tmp/a b.md"), "pane"),
            "http://127.0.0.1:43128/?file=%2Ftmp%2Fa%20b.md&surface=pane"
        );
    }

    #[test]
    fn health_reads_files_and_identity() {
        let h = Health::parse(r#"{"ok":true,"file":"/a.md","recent":null}"#);
        assert_eq!(h.file.as_deref(), Some("/a.md"));
        assert_eq!(h.recent_or_current(), Some("/a.md"));
        assert_eq!(h.holder(None), Holder::Unknown);

        let h = Health::parse("<html>");
        assert!(h.file.is_none());
        assert_eq!(h.holder(None), Holder::Unknown);
    }

    #[test]
    fn holder_compares_exe_and_version() {
        let exe = std::env::current_exe().unwrap();
        let body = |version: &str, exe: &str| {
            serde_json::json!({
                "ok": true, "server": "blueline", "version": version,
                "pid": 42, "exe": exe,
            })
            .to_string()
        };
        let ours = Health::parse(&body(env!("CARGO_PKG_VERSION"), &exe.to_string_lossy()));
        assert_eq!(ours.holder(Some(&exe)), Holder::Ours);

        let older = Health::parse(&body("0.0.0-old", &exe.to_string_lossy()));
        assert_eq!(
            older.holder(Some(&exe)),
            Holder::OtherBlueline { pid: Some(42) }
        );

        let elsewhere = Health::parse(&body(env!("CARGO_PKG_VERSION"), "/elsewhere/blueline"));
        assert_eq!(
            elsewhere.holder(Some(&exe)),
            Holder::OtherBlueline { pid: Some(42) }
        );
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
