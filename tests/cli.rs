//! Integration tests for the CLI: each test runs the `blueline` binary against
//! its own render server on a free port, with stand-ins for the programs it
//! drives - `open` / `xdg-open`, `herdr` and `terminal-browser` - written as
//! small sh scripts that keep their state in files.

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The browser opener: records the URL it was asked to open.
const OPENER: &str = r#"#!/bin/sh
printf '%s\n' "$1" >> "$FAKE_DIR/opened"
"#;

/// terminal-browser. Each browser is a file `browsers/<key>` whose first line
/// is `<herdr tab> <pane>` and whose other lines are `<tab id> <url>`.
const TERMINAL_BROWSER: &str = r#"#!/bin/sh
d="$FAKE_DIR/browsers"
mkdir -p "$d"
cmd=$1; shift
case $cmd in
  ls)
    printf '{"browsers":['
    sep=''
    for f in "$d"/*; do
      [ -f "$f" ] || continue
      key=$(basename "$f")
      read -r tab pane < "$f"
      printf '%s{"key":"%s","pane":{"tab":"%s","pane":"%s"},"tabs":[' "$sep" "$key" "$tab" "$pane"
      tsep=''
      active=true
      tail -n +2 "$f" | while read -r id url; do
        printf '%s{"id":%s,"url":"%s","targetId":"T%s","active":%s}' "$tsep" "$id" "$url" "$id" "$active"
        tsep=','
        active=false
      done
      printf ']}'
      sep=','
    done
    printf ']}\n'
    ;;
  new-tab)
    key=$2; url=$3
    n=$(wc -l < "$d/$key")
    printf '%s %s\n' "$((n + 100))" "$url" >> "$d/$key"
    ;;
  action)
    [ "$1" = --follow ] && shift
    key=$2; tab=$4; shift 4
    [ "$1" = -- ] && shift
    f="$d/$key"
    case $1 in
      eval)
        url=$(printf '%s' "$2" | sed 's/.*replace("\([^"]*\)").*/\1/')
        awk -v t="$tab" -v u="$url" 'NR > 1 && $1 == t { $2 = u } { print }' "$f" > "$f.new"
        mv "$f.new" "$f"
        ;;
      tab)
        awk -v t="$tab" 'NR == 1 || $1 != t' "$f" > "$f.new"
        mv "$f.new" "$f"
        [ "$(wc -l < "$f")" -gt 1 ] || rm -f "$f"
        ;;
      done) ;;
      *) printf 'ACTION %s\n' "$*" ;;
    esac
    ;;
esac
"#;

/// herdr. Panes are files under `panes/`: `<pane>.url` holds the URL a split
/// was given, `<pane>.label` its label.
const HERDR: &str = r#"#!/bin/sh
d="$FAKE_DIR/panes"
b="$FAKE_DIR/browsers"
mkdir -p "$d" "$b"
shift
cmd=$1; shift
case $cmd in
  split)
    n=$(ls "$d" | grep -c '\.url$')
    pane="p$((n + 10))"
    for arg in "$@"; do
      case $arg in BLUELINE_URL=*) printf '%s\n' "${arg#BLUELINE_URL=}" > "$d/$pane.url" ;; esac
    done
    printf '{"result":{"pane":{"pane_id":"%s"}}}\n' "$pane"
    ;;
  rename) printf '%s\n' "$2" > "$d/$1.label" ;;
  run)
    printf '%s %s\n1 %s\n' "$HERDR_TAB_ID" "$1" "$(cat "$d/$1.url")" > "$b/k$1"
    ;;
  close) rm -f "$b/k$1" ;;
  get)
    label=$(cat "$d/$1.label" 2>/dev/null)
    printf '{"result":{"pane":{"label":"%s"}}}\n' "$label"
    ;;
esac
"#;

/// A scratch directory with the stand-ins on PATH, and a free port for the
/// server. The server is stopped when this is dropped.
struct Env {
    dir: tempfile::TempDir,
    port: u16,
    herdr: bool,
    /// PATH beyond the stand-ins, which the opener test empties: the machine
    /// may have a real `open` or `xdg-open`.
    system_path: &'static str,
}

impl Env {
    fn new(herdr: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let mut scripts = vec![("open", OPENER), ("xdg-open", OPENER)];
        if herdr {
            scripts.push(("terminal-browser", TERMINAL_BROWSER));
            scripts.push(("herdr", HERDR));
        }
        for (name, body) in scripts {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Env {
            dir,
            port,
            herdr,
            system_path: ":/usr/bin:/bin",
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes a markdown file in the scratch directory.
    fn markdown(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, format!("# {name}\n")).unwrap();
        path.canonicalize().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_blueline"));
        // Only the stand-ins and the basic tools they use, so that a real
        // herdr or terminal-browser on the machine is never reached.
        command
            .args(args)
            .env(
                "PATH",
                format!("{}{}", self.path("bin").display(), self.system_path),
            )
            .env("BLUELINE_PORT", self.port.to_string())
            .env("TMPDIR", self.dir.path())
            .env("FAKE_DIR", self.dir.path())
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID");
        if self.herdr {
            command.env("HERDR_TAB_ID", "t1").env("HERDR_PANE_ID", "p1");
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs and expects success, returning stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "blueline {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Runs and expects failure, returning stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "blueline {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn opened(&self) -> Vec<String> {
        fs::read_to_string(self.path("opened"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The fake browsers, as `key: first line, tab lines...`.
    fn browsers(&self) -> Vec<(String, String)> {
        let Ok(entries) = fs::read_dir(self.path("browsers")) else {
            return Vec::new();
        };
        let mut all: Vec<(String, String)> = entries
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        all.sort();
        all
    }

    fn url(&self, file: &Path, surface: &str) -> String {
        let encoded: String = file
            .to_string_lossy()
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        format!(
            "http://127.0.0.1:{}/?file={encoded}&surface={surface}",
            self.port
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self
            .command(&["--stop"])
            .env_remove("HERDR_TAB_ID")
            .output();
    }
}

#[test]
fn previews_in_a_browser_tab_and_stops() {
    let env = Env::new(false);
    let file = env.markdown("a b.md");
    let out = env.ok(&[file.to_str().unwrap()]);
    assert_eq!(
        out,
        format!("blueline: previewing {} in a browser tab\n", file.display())
    );
    assert_eq!(env.opened(), vec![env.url(&file, "tab")]);

    // With no file, --browser comes back to the latest one.
    env.ok(&["--browser"]);
    assert_eq!(env.opened().len(), 2);

    let out = env.ok(&["--stop"]);
    assert_eq!(
        out,
        format!("blueline: stopped the server on {}\n", env.port)
    );
    let out = env.ok(&["--stop"]);
    assert_eq!(out, format!("blueline: no server on {}\n", env.port));
}

#[test]
fn reports_what_cannot_be_previewed() {
    let env = Env::new(false);
    let err = env.fails(&["--browser"]);
    assert_eq!(
        err,
        "blueline: nothing is being previewed - run 'blueline <file.md>'\n"
    );
    let missing = env.path("missing.md");
    let err = env.fails(&[missing.to_str().unwrap()]);
    assert_eq!(
        err,
        format!("blueline: no such file: {}\n", missing.display())
    );

    let text = env.path("notes.txt");
    fs::write(&text, "plain").unwrap();
    let err = env.fails(&["--browser", text.to_str().unwrap()]);
    assert!(err.contains("would not take"), "{err}");

    let err = env.fails(&["--term", "a.md"]);
    assert!(
        err.contains("outside a herdr session there is none"),
        "{err}"
    );
    let err = env.fails(&["--", "snapshot"]);
    assert!(err.contains("'snapshot' would be run against"), "{err}");
    let err = env.fails(&["--pane"]);
    assert!(err.contains("--pane needs a herdr pane"), "{err}");
}

#[test]
fn refuses_without_an_opener() {
    let mut env = Env::new(false);
    env.system_path = "";
    fs::remove_file(env.path("bin/open")).unwrap();
    fs::remove_file(env.path("bin/xdg-open")).unwrap();
    let file = env.markdown("a.md");
    let err = env.fails(&[file.to_str().unwrap()]);
    assert!(
        err.contains("neither open nor xdg-open is on PATH"),
        "{err}"
    );
}

#[test]
fn term_opens_navigates_resets_and_stops_a_pane_preview() {
    let env = Env::new(true);
    let a = env.markdown("a.md");
    let b = env.markdown("b.md");

    let out = env.ok(&["--term", a.to_str().unwrap()]);
    assert_eq!(
        out,
        format!("blueline: previewing {} in a new pane\n", a.display())
    );
    assert_eq!(
        env.browsers(),
        vec![(
            "kp10".into(),
            format!("t1 p10\n1 {}\n", env.url(&a, "pane"))
        )]
    );

    // A second file navigates the same preview.
    let out = env.ok(&["--term", b.to_str().unwrap()]);
    assert_eq!(out, format!("blueline: previewing {}\n", b.display()));
    assert_eq!(
        env.browsers(),
        vec![(
            "kp10".into(),
            format!("t1 p10\n1 {}\n", env.url(&b, "pane"))
        )]
    );

    assert_eq!(env.ok(&["--pane"]), "p10\n");
    assert_eq!(env.ok(&["--", "snapshot", "-i"]), "ACTION snapshot -i\n");

    // --reset replaces the pane, on the file it was showing.
    let out = env.ok(&["--reset"]);
    assert_eq!(
        out,
        format!(
            "blueline: reset ok - previewing {} in a new pane\n",
            b.display()
        )
    );
    assert_eq!(env.browsers().len(), 1);
    assert_eq!(env.browsers()[0].0, "kp11");

    let out = env.ok(&["--stop"]);
    assert_eq!(
        out,
        format!(
            "blueline: stopped the server on {} and closed this tab's preview\n",
            env.port
        )
    );
    assert!(env.browsers().is_empty());
}

#[test]
fn a_browser_with_tabs_of_its_own_keeps_them() {
    let env = Env::new(true);
    fs::create_dir_all(env.path("browsers")).unwrap();
    fs::write(env.path("browsers/mine"), "t1 p5\n1 https://example.com/\n").unwrap();
    let a = env.markdown("a.md");

    let out = env.ok(&["--term", a.to_str().unwrap()]);
    assert_eq!(
        out,
        format!(
            "blueline: previewing {} in a new tab of this tab's browser\n",
            a.display()
        )
    );
    assert_eq!(env.ok(&["--pane"]), "p5\n");

    env.ok(&["--stop"]);
    assert_eq!(
        env.browsers(),
        vec![("mine".into(), "t1 p5\n1 https://example.com/\n".into())]
    );
}

#[test]
fn pane_modes_need_a_preview() {
    let env = Env::new(true);
    let err = env.fails(&["--pane"]);
    assert!(err.contains("no preview in this tab"), "{err}");
    let err = env.fails(&["--", "snapshot"]);
    assert!(err.contains("no preview in this tab"), "{err}");
    let err = env.fails(&["--reset"]);
    assert!(
        err.contains("no pane preview in this tab to reset"),
        "{err}"
    );
}
