//! The secret the CLI and its render server share, which is what keeps other
//! local processes - other users on the machine, or a sandboxed program that
//! can reach the loopback port but not the user's files - away from both.
//!
//! The server makes a fresh token when it starts and writes it to a file only
//! this user can read. The CLI reads it from there and
//!
//! - sends it as a bearer token on the tooling routes (`/open`, `/shutdown`,
//!   `/viewers`, `/ticket`), which refuse anything without it, and
//! - checks that whatever answers `/health` on the port knows it, by sending a
//!   random challenge and verifying the HMAC that comes back. A process that
//!   took the port first cannot answer that, so it never gets the token, a
//!   file path, or a signal.
//!
//! The token never leaves the CLI for a server that has not proved it knows it
//! already.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

/// The header a `/health` challenge travels in.
pub const CHALLENGE_HEADER: &str = "x-blueline-challenge";

/// Random bytes in a token, before hex.
const TOKEN_BYTES: usize = 32;

/// Separates the `/health` proof from any other use of the token as a key.
const PROOF_CONTEXT: &[u8] = b"blueline health v1\n";

/// `n` random bytes, as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    // Without the system RNG there is no secret to make, and an all-zero one
    // would be a secret everybody knows.
    getrandom::fill(&mut bytes).expect("the system random number generator is available");
    hex(&bytes)
}

/// A fresh token for a server to hand out.
pub fn new_token() -> String {
    random_hex(TOKEN_BYTES)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn mac(token: &str, challenge: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes a key of any length");
    mac.update(PROOF_CONTEXT);
    mac.update(challenge.as_bytes());
    mac
}

/// What a server that knows `token` answers to `challenge`.
pub fn proof(token: &str, challenge: &str) -> String {
    hex(&mac(token, challenge).finalize().into_bytes())
}

/// Whether `proof` is the answer to `challenge` from a server knowing `token`.
pub fn proof_holds(token: &str, challenge: &str, proof: &str) -> bool {
    unhex(proof).is_some_and(|bytes| mac(token, challenge).verify_slice(&bytes).is_ok())
}

/// Compares two secrets in time that does not depend on where they differ.
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a token read back from disk looks like one a server wrote.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_BYTES * 2 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// The token file

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The directory the token files live in: `blueline-<uid>` under
/// `$XDG_RUNTIME_DIR` when there is one, else under `$TMPDIR` or `/tmp`. The
/// server inherits the environment of the CLI that starts it, so both look in
/// the same place.
pub fn state_dir() -> PathBuf {
    let base = ["XDG_RUNTIME_DIR", "TMPDIR"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("blueline-{}", euid()))
}

/// The token file of the server on `port`.
pub fn token_path(port: u16) -> PathBuf {
    state_dir().join(format!("server-{port}.token"))
}

/// Whether `meta` is something only this user can get at. A shared `/tmp` is
/// where someone else could have made the directory, or a link in its place,
/// before blueline ever ran.
fn private(meta: &fs::Metadata) -> bool {
    meta.uid() == euid() && meta.mode() & 0o077 == 0
}

fn not_private(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} is not private to this user - remove it, and blueline makes a new one",
            path.display()
        ),
    )
}

/// The state directory, made if it is missing, once it has been checked to be
/// a real directory that belongs to this user and nobody else can enter.
fn private_dir(create: bool) -> io::Result<PathBuf> {
    let dir = state_dir();
    if create {
        match DirBuilder::new().mode(0o700).create(&dir) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || !private(&meta) {
        return Err(not_private(&dir));
    }
    Ok(dir)
}

/// Writes the token for the server on `port`, readable by this user alone. It
/// is written beside the final name and renamed over it, so a reader never
/// sees half of one.
pub fn write_token(port: u16, token: &str) -> io::Result<PathBuf> {
    let dir = private_dir(true)?;
    let path = dir.join(format!("server-{port}.token"));
    let staging = dir.join(format!(".server-{port}.token.{}", std::process::id()));
    let _ = fs::remove_file(&staging);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staging)?;
    file.write_all(token.as_bytes())?;
    drop(file);
    fs::rename(&staging, &path)?;
    Ok(path)
}

/// The token of the server on `port`, or None when there is none to read or
/// the file is not one only this user could have written.
pub fn read_token(port: u16) -> Option<String> {
    let path = private_dir(false)
        .ok()?
        .join(format!("server-{port}.token"));
    let mut file = open_private(&path).ok()?;
    let mut token = String::new();
    file.read_to_string(&mut token).ok()?;
    let token = token.trim().to_owned();
    well_formed(&token).then_some(token)
}

fn open_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || !private(&meta) {
        return Err(not_private(path));
    }
    Ok(file)
}

/// Removes the token file of the server on `port`, if it is still `token`: a
/// server that replaced this one has written its own there.
pub fn remove_token(port: u16, token: &str) {
    if read_token(port).is_some_and(|on_disk| same_secret(&on_disk, token)) {
        let _ = fs::remove_file(token_path(port));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proofs_hold_only_for_the_same_token_and_challenge() {
        let token = new_token();
        assert!(well_formed(&token));
        let challenge = random_hex(16);
        let answer = proof(&token, &challenge);
        assert!(proof_holds(&token, &challenge, &answer));
        assert!(!proof_holds(&new_token(), &challenge, &answer));
        assert!(!proof_holds(&token, &random_hex(16), &answer));
        assert!(!proof_holds(&token, &challenge, "zz"));
        assert!(!proof_holds(&token, &challenge, ""));
        assert!(!proof_holds(&token, &challenge, &answer[1..]));
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
        assert!(!same_secret("", "a"));
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("g0"), None);
        assert_eq!(unhex("é0"), None);
    }

    #[test]
    fn tokens_must_look_like_tokens() {
        assert!(!well_formed(""));
        assert!(!well_formed(&"g".repeat(64)));
        assert!(!well_formed(&"a".repeat(63)));
    }

    #[test]
    fn private_means_owned_and_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "x").unwrap();
        let set = |mode| {
            fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
            fs::metadata(&path).unwrap()
        };
        assert!(private(&set(0o600)));
        assert!(!private(&set(0o640)));
        assert!(!private(&set(0o604)));
        assert!(open_private(&path).is_err());
        set(0o600);
        assert!(open_private(&path).is_ok());

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_private(&link).is_err(), "a link is not followed");
    }
}
