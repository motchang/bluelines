//! Command-line interface: the client side of blueline, which finds or starts
//! the render server and points a surface at a file - a desktop browser tab,
//! or a terminal-browser pane in this herdr tab.
//!
//! The render server knows nothing about herdr: it serves plain HTTP on a
//! loopback port, so the same preview shows in an ordinary browser tab, and
//! that is where a file goes unless `--term` asks for a pane.

mod args;
mod client;
mod herdr;
mod listing;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};

use clap::{Args, Parser, Subcommand};

use args::{Mode, USAGE};
use client::Server;
use herdr::Herdr;
use listing::Preview;

/// The render server's default loopback port; `BLUELINE_PORT` overrides it.
pub const DEFAULT_PORT: u16 = 43128;

/// The clap definition, which covers only the hidden `serve` subcommand: the
/// preview grammar is parsed by hand, in `args`.
#[derive(Debug, Parser)]
#[command(name = "blueline", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the render server in the foreground. The CLI starts it by
    /// re-executing itself with this subcommand; it is not meant to be typed.
    #[command(hide = true)]
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The loopback port to listen on.
    #[arg(long, env = "BLUELINE_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,
}

/// Whether a command line is for the server rather than the preview CLI.
pub fn is_serve(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|a| a == "serve")
}

/// A run that ends in failure: a message for stderr, the usage line, or both.
#[derive(Debug)]
pub struct Failure {
    message: Option<String>,
    show_usage: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Failure {
        Failure {
            message: Some(message.into()),
            show_usage: false,
        }
    }

    fn usage(message: impl Into<String>) -> Failure {
        Failure {
            message: Some(message.into()),
            show_usage: true,
        }
    }

    fn usage_only() -> Failure {
        Failure {
            message: None,
            show_usage: true,
        }
    }

    fn report(&self) {
        if let Some(message) = &self.message {
            eprintln!("blueline: {message}");
        }
        if self.show_usage {
            eprintln!("{USAGE}");
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message.as_deref().unwrap_or(USAGE))
    }
}

/// Runs the preview CLI on the arguments after the program name.
pub fn run(args: &[OsString]) -> ExitCode {
    match args::parse(args).and_then(|mode| Session::new()?.run(mode)) {
        Ok(code) => code,
        Err(failure) => {
            failure.report();
            ExitCode::FAILURE
        }
    }
}

/// Searches PATH for an executable, returning its absolute path.
fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(|p| std::path::absolute(&p).unwrap_or(p))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Everything a run needs to know about where it is.
struct Session {
    server: Server,
    /// This herdr tab and pane. Both are required together: scoping reuse to
    /// a tab while splitting panes off wherever the focus happens to be would
    /// find nothing next run and open another, every run.
    scope: Option<(String, String)>,
    tb: Option<PathBuf>,
}

impl Session {
    fn new() -> Result<Session, Failure> {
        let port = match std::env::var("BLUELINE_PORT") {
            Ok(v) if !v.is_empty() => v
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| Failure::new(format!("BLUELINE_PORT is not a port: {v}")))?,
            _ => DEFAULT_PORT,
        };
        let scope = non_empty_env("HERDR_TAB_ID").zip(non_empty_env("HERDR_PANE_ID"));
        Ok(Session {
            server: Server::new(port),
            scope,
            tb: find_on_path("terminal-browser"),
        })
    }

    fn run(&self, mode: Mode) -> Result<ExitCode, Failure> {
        match mode {
            Mode::Help => print_and_succeed(USAGE),
            Mode::Version => print_and_succeed(concat!("blueline ", env!("CARGO_PKG_VERSION"))),
            Mode::Stop => self.stop(),
            Mode::Pane => self.pane(),
            Mode::Reset { file } => self.reset(file.as_deref()),
            Mode::Browser { file } => self.browser(file.as_deref()),
            Mode::Action { command } => self.action(&command),
            Mode::Preview { file, term } => self.preview(&file, term),
        }
    }

    fn preview(&self, file: &OsStr, term: bool) -> Result<ExitCode, Failure> {
        if term {
            self.term(file)
        } else {
            self.browser(Some(file))
        }
    }

    /// A browser tab on `file`, or with no file on the most recent file any
    /// surface was pointed at - what "the preview" means to someone who did
    /// not name one.
    fn browser(&self, file: Option<&OsStr>) -> Result<ExitCode, Failure> {
        let file = match file {
            Some(f) => self.named_file(f)?,
            None => self.resume(Resume::Recent)?,
        };
        self.preview_in_browser(&file)
    }

    /// A file named on the command line, with our server up to show it.
    fn named_file(&self, arg: &OsStr) -> Result<PathBuf, Failure> {
        let file = resolve_file(arg)?;
        self.server.ensure()?;
        Ok(file)
    }

    /// The herdr side, for the modes that act on a pane preview. `mode` names
    /// the mode asking, for the messages.
    fn herdr(&self, mode: &str) -> Result<Herdr<'_>, Failure> {
        let Some((tab, pane)) = &self.scope else {
            return Err(Failure::new(format!(
                "{mode} needs a herdr pane to scope to - there is no tab to look in from a plain terminal"
            )));
        };
        let tb = self.require_tb(mode)?;
        Ok(Herdr {
            tab: tab.clone(),
            pane: pane.clone(),
            tb,
            server: &self.server,
        })
    }

    /// terminal-browser, or a refusal. Only --term has a way round its
    /// absence - the same file in a browser tab - so only --term says so.
    fn require_tb(&self, mode: &str) -> Result<PathBuf, Failure> {
        if let Some(tb) = &self.tb {
            return Ok(tb.clone());
        }
        Err(Failure::new(if mode == "--term" {
            "terminal-browser is not on PATH - install it (https://github.com/zenbu-labs/terminal-browser); 'blueline <file.md>' opens a browser tab without it".to_owned()
        } else {
            format!(
                "{mode} needs terminal-browser, which is not on PATH - install it (https://github.com/zenbu-labs/terminal-browser)"
            )
        }))
    }

    /// The file the server is showing, for the modes that take no file, with
    /// our own server left up and ready to show it again. A server has to be
    /// up already: a fresh one has no current file.
    ///
    /// The file is read before ownership of the port is settled, on purpose:
    /// the server answering may be another blueline, and its current file is
    /// still the file on screen, which is the thing being restored.
    fn resume(&self, which: Resume) -> Result<PathBuf, Failure> {
        let health = self
            .server
            .health(std::time::Duration::from_secs(2))
            .unwrap_or_default();
        let recent = health.recent_or_current().map(str::to_owned);
        let file = match which {
            Resume::Recent => recent.clone(),
            Resume::Current => health.file.clone(),
        };
        let Some(file) = file else {
            return Err(Failure::new(match (which, recent) {
                // A pane mode finding no pane-side file while a browser tab
                // holds one: something is being previewed, just not in a pane.
                (Resume::Current, Some(recent)) => format!(
                    "nothing is being previewed in a pane - {recent} is open in a browser tab, which --reset does not rebuild; name a file: 'blueline --reset <file.md>'"
                ),
                (Resume::Current, None) => {
                    "nothing is being previewed in a pane - run 'blueline --term <file.md>'"
                        .to_owned()
                }
                (Resume::Recent, _) => {
                    "nothing is being previewed - run 'blueline <file.md>'".to_owned()
                }
            }));
        };
        let file = PathBuf::from(file);
        if !file.is_file() {
            return Err(Failure::new(format!(
                "the previewed file is gone: {}",
                file.display()
            )));
        }
        self.server.ensure()?;
        Ok(file)
    }

    /// The command that hands a URL to the desktop browser, chosen by platform
    /// rather than by which name turns up on PATH first: on Linux a program
    /// called `open` is as likely to be something else. Each platform's second
    /// choice is kept rather than dropped.
    fn find_opener() -> Option<&'static str> {
        let order = if cfg!(target_os = "macos") {
            ["open", "xdg-open"]
        } else {
            ["xdg-open", "open"]
        };
        order.into_iter().find(|name| find_on_path(name).is_some())
    }

    /// The whole of a browser preview once the file and server are settled.
    ///
    /// The opener is looked up before the file is registered and run after:
    /// registering navigates every page on this preview, a pane included, so
    /// the failure knowable in advance - no opener at all - is reported while
    /// nothing has changed yet. The file reaches the server before the tab
    /// opens, so a new tab never renders whatever the server was holding.
    ///
    /// A tab already showing this file is left alone rather than joined by a
    /// second one: registering the file has just reloaded it. The count is
    /// taken before registering, because a page in the middle of that reload
    /// has dropped its event stream and would not be counted.
    fn preview_in_browser(&self, file: &Path) -> Result<ExitCode, Failure> {
        let url = self.server.preview_url(file, "tab");
        let Some(opener) = Self::find_opener() else {
            return Err(Failure::new(format!(
                "no way to open a browser - neither open nor xdg-open is on PATH; the preview is at {url}"
            )));
        };
        let showing = self.server.tab_viewers(file);
        self.server.open(file, false)?;
        if showing > 0 {
            println!(
                "blueline: {} is already open in a browser tab - reloaded that instead of opening another",
                file.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        let status = Process::new(opener).arg(&url).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return Err(Failure::new(format!(
                    "{opener} could not open {url} (exit status {})",
                    s.code().map_or("unknown".to_owned(), |c| c.to_string())
                )));
            }
            Err(e) => return Err(Failure::new(format!("could not run {opener}: {e}"))),
        }
        println!("blueline: previewing {} in a browser tab", file.display());
        Ok(ExitCode::SUCCESS)
    }

    /// `--stop`: this tab's preview goes too, since with the server gone it is
    /// a page that will never reload. Only this tab's, and only previews.
    /// Failing to close it is not a reason to leave the server running - the
    /// part that was asked for - so it is reported and the stop carries on.
    fn stop(&self) -> Result<ExitCode, Failure> {
        let (closed, mut ok) = self.close_for_stop()?;
        let port = self.server.port;
        match self.server.stop() {
            Ok(true) => println!("blueline: stopped the server on {port}{closed}"),
            Ok(false) => println!("blueline: no server on {port}{closed}"),
            Err(failure) => {
                failure.report();
                if let Some(done) = closed.strip_prefix(" and ") {
                    println!("blueline: {done}");
                }
                ok = false;
            }
        }
        Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    /// Closes this tab's preview for `--stop`, returning what to add to the
    /// message and whether it went well. Outside herdr, or without
    /// terminal-browser, there is no preview here and nothing to say.
    fn close_for_stop(&self) -> Result<(&'static str, bool), Failure> {
        if self.scope.is_none() || self.tb.is_none() {
            return Ok(("", true));
        }
        Ok(match self.herdr("--stop")?.close_previews() {
            Ok(true) => (" and closed this tab's preview", true),
            Ok(false) => ("", true),
            Err(failure) => {
                failure.report();
                (" but could not close this tab's preview", false)
            }
        })
    }

    /// `--pane`: the herdr panes holding this tab's previews, one per line.
    fn pane(&self) -> Result<ExitCode, Failure> {
        let herdr = self.herdr("--pane")?;
        let found = herdr.resolve(true)?;
        if found.previews.is_empty() {
            return Err(Failure::new(
                "no preview in this tab - run 'blueline --term <file.md>' first",
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for p in &found.previews {
            if !seen.contains(&p.pane.as_str()) {
                seen.push(&p.pane);
                println!("{}", p.pane);
            }
        }
        Ok(ExitCode::SUCCESS)
    }

    /// `--reset`: replaces this tab's preview rather than trying to revive it.
    /// A preview that has stopped answering is cheaper to throw away than to
    /// diagnose, and the page holds no state worth keeping.
    fn reset(&self, file: Option<&OsStr>) -> Result<ExitCode, Failure> {
        let herdr = self.herdr("--reset")?;
        let file = match file {
            Some(f) => self.named_file(f)?,
            None => self.reset_onto_current(&herdr)?,
        };
        // Handed over before anything is closed, because closing is
        // destructive and this is the last step that can still refuse.
        self.server.open(&file, true)?;
        herdr.close_previews()?;
        let opened = herdr.open_preview(&file)?;
        println!(
            "blueline: reset ok - previewing {} {opened}",
            file.display()
        );
        Ok(ExitCode::SUCCESS)
    }

    /// The file `--reset` with no file comes back to. With no pane preview
    /// here it is refused rather than rebuilt onto whatever a pane in some
    /// other tab last opened.
    fn reset_onto_current(&self, herdr: &Herdr<'_>) -> Result<PathBuf, Failure> {
        if herdr.resolve(true)?.previews.is_empty() {
            return Err(Failure::new(
                "no pane preview in this tab to reset - reload a browser tab yourself; 'blueline --term <file.md>' opens a pane",
            ));
        }
        self.resume(Resume::Current)
    }

    /// `-- <command>`: a `terminal-browser action` command against this tab's
    /// preview. Naming the browser and tab belongs here: left to itself,
    /// action drives the active tab of this tab's browser, which is not the
    /// preview whenever the preview shares a browser with tabs of someone's
    /// own. terminal-browser rejects commands it does not have.
    fn action(&self, command: &[OsString]) -> Result<ExitCode, Failure> {
        if self.scope.is_none() {
            return Err(Failure::new(format!(
                "'{}' would be run against a terminal-browser preview, and outside a herdr session there is none - the preview is a plain browser tab.",
                command[0].to_string_lossy()
            )));
        }
        let herdr = self.herdr("'blueline -- <command>'")?;
        let found = herdr.resolve(true)?;
        let Some(preview) = found.first() else {
            return Err(Failure::new(
                "no preview in this tab - run 'blueline --term <file.md>' to open one; a preview opened without --term is a desktop tab and cannot be driven from here",
            ));
        };
        let preview = with_target(&herdr, preview)?;
        let code = herdr.action(&preview, command);
        Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
    }

    /// `--term <file>`: a terminal-browser pane in this herdr tab. Refused up
    /// front outside one rather than quietly turned into a browser tab:
    /// whoever wrote it wants a page `blueline --` can drive.
    fn term(&self, file: &OsStr) -> Result<ExitCode, Failure> {
        if self.scope.is_none() {
            return Err(Failure::new(
                "--term opens a terminal-browser pane in this herdr tab, and outside a herdr session there is none - drop --term to open a browser tab",
            ));
        }
        let herdr = self.herdr("--term")?;
        let file = self.named_file(file)?;
        let found = herdr.resolve(true)?;
        self.server.open(&file, true)?;
        let opened = match found.first() {
            None => format!(" {}", herdr.open_preview(&file)?),
            Some(preview) => {
                Self::navigate_or_replace(&herdr, preview, &file).map(|()| String::new())?
            }
        };
        println!("blueline: previewing {}{opened}", file.display());
        Ok(ExitCode::SUCCESS)
    }

    /// Points the preview at `file`. One that will not take the navigation is
    /// replaced rather than reported: the caller asked to see a file, and a
    /// new preview shows it.
    fn navigate_or_replace(
        herdr: &Herdr<'_>,
        preview: &Preview,
        file: &Path,
    ) -> Result<(), Failure> {
        if herdr.navigate(preview, file)? {
            return Ok(());
        }
        eprintln!("blueline: the preview in this tab did not answer - replacing it");
        herdr.close_previews()?;
        herdr.open_preview(file)?;
        Ok(())
    }
}

fn print_and_succeed(text: &str) -> Result<ExitCode, Failure> {
    println!("{text}");
    Ok(ExitCode::SUCCESS)
}

/// A preview with a CDP target, waiting for one when it is still starting:
/// `action` refuses a tab without one.
fn with_target(herdr: &Herdr<'_>, preview: &Preview) -> Result<Preview, Failure> {
    if !preview.target.is_empty() {
        return Ok(preview.clone());
    }
    herdr.wait_preview(10)?.ok_or_else(|| {
        Failure::new(
            "the preview in this tab has no CDP target after 10s - is it stuck? 'blueline --reset' replaces it",
        )
    })
}

#[derive(Clone, Copy)]
enum Resume {
    /// The file a bare `/` renders, which the pane modes last opened.
    Current,
    /// The most recent `/open` of any kind.
    Recent,
}

/// An absolute path to an existing file. The directory is resolved on its own
/// so that a path with no directory to enter is reported as the missing file
/// it is.
fn resolve_file(arg: &OsStr) -> Result<PathBuf, Failure> {
    let path = Path::new(arg);
    let missing = || Failure::new(format!("no such file: {}", path.display()));
    let name = path.file_name().ok_or_else(missing)?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file = dir.canonicalize().map_err(|_| missing())?.join(name);
    if !file.is_file() {
        return Err(missing());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_is_hidden_but_parses() {
        let cli = Cli::try_parse_from(["blueline", "serve", "--port", "5000"]).unwrap();
        let Command::Serve(args) = cli.command;
        assert_eq!(args.port, 5000);
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("serve"), "serve should not appear in --help");
    }

    #[test]
    fn serve_is_routed_to_clap_and_the_rest_is_not() {
        let line = |args: &[&str]| -> Vec<OsString> { args.iter().map(OsString::from).collect() };
        assert!(is_serve(&line(&["blueline", "serve"])));
        assert!(!is_serve(&line(&["blueline", "README.md"])));
        assert!(!is_serve(&line(&["blueline", "--", "serve"])));
    }

    #[test]
    fn resolve_file_makes_paths_absolute() {
        let dir = std::env::temp_dir().join(format!("blueline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.md");
        std::fs::write(&file, "# hi").unwrap();
        let resolved = resolve_file(file.as_os_str()).unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("a b.md"));

        let missing = dir.join("nope").join("x.md");
        let err = resolve_file(missing.as_os_str()).unwrap_err().to_string();
        assert_eq!(err, format!("no such file: {}", missing.display()));
        assert!(
            resolve_file(dir.as_os_str()).is_err(),
            "a directory is not a file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
