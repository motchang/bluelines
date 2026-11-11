//! The preview grammar: which mode a command line asks for.
//!
//! Parsed by hand rather than with clap, because the grammar is not one clap
//! models well: `--` hands the rest of the line to terminal-browser untouched,
//! a file really called `--term.md` has to stay nameable, and a bare word that
//! is not a markdown path is refused with a pointer to `--` instead of being
//! taken as a file.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

use super::Failure;

pub const USAGE: &str = "usage: blueline <file.md> | --browser [file.md] | --term <file.md> | -- <terminal-browser action command> | --pane | --reset [file.md] | --stop";

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Help,
    Version,
    /// `blueline <file>` (a browser tab) or `blueline --term <file>` (a pane).
    Preview {
        file: OsString,
        term: bool,
    },
    /// `--browser [file]`: a browser tab, on the latest file when none is named.
    Browser {
        file: Option<OsString>,
    },
    /// `--reset [file]`: replace this tab's pane preview.
    Reset {
        file: Option<OsString>,
    },
    /// `--pane`: print the herdr panes holding this tab's previews.
    Pane,
    /// `--stop`: close this tab's preview and stop the server.
    Stop,
    /// `-- <command>`: a `terminal-browser action` command against the preview.
    Action {
        command: Vec<OsString>,
    },
}

/// Whether an argument is spelled like a markdown path. The decision is made on
/// the spelling and not on whether the path exists, so that `blueline snapshot`,
/// the old way of driving the preview, cannot turn into a preview of a file
/// called `snapshot` that someone left in the cwd. `./snapshot` names the file.
pub fn is_markdown_spelling(arg: &OsStr) -> bool {
    let bytes = arg.as_bytes();
    [b".md".as_slice(), b".markdown", b".mdown"]
        .iter()
        .any(|suffix| bytes.ends_with(suffix))
        || bytes.contains(&b'/')
}

fn starts_with_dash(arg: &OsStr) -> bool {
    arg.as_bytes().first() == Some(&b'-')
}

fn show(arg: &OsStr) -> String {
    arg.to_string_lossy().into_owned()
}

/// An option where a file was expected. Markdown spellings are let through
/// before anything that starts with `-` is refused, so that a file really
/// called `--term.md` or `./--term` can still be named.
fn not_an_option(arg: &OsStr) -> Result<(), Failure> {
    if !is_markdown_spelling(arg) && starts_with_dash(arg) {
        return Err(Failure::usage(format!("unknown option: {}", show(arg))));
    }
    Ok(())
}

/// Anything after the file. Left unchecked it would be dropped without a word,
/// so `blueline README.md --term` would open a browser tab and look like
/// --term had failed to do anything.
fn no_more_args(stray: &OsStr, form: &str) -> Failure {
    Failure::new(format!(
        "unexpected argument after the file: {} - flags go first ('{form}')",
        show(stray)
    ))
}

/// An optional file argument, where an empty one counts as none.
fn optional_file(arg: Option<&OsString>) -> Option<OsString> {
    arg.filter(|a| !a.is_empty()).cloned()
}

/// Parses the arguments after the program name.
pub fn parse(args: &[OsString]) -> Result<Mode, Failure> {
    let Some(first) = args.first() else {
        return Err(Failure::usage_only());
    };
    let rest = &args[1..];
    match first.as_bytes() {
        b"-h" | b"--help" => Ok(Mode::Help),
        b"-V" | b"--version" => Ok(Mode::Version),
        b"--stop" | b"--pane" => {
            if let Some(stray) = rest.first() {
                return Err(Failure::new(format!(
                    "unexpected argument after {}: {} - it takes none",
                    show(first),
                    show(stray)
                )));
            }
            Ok(if first == "--stop" {
                Mode::Stop
            } else {
                Mode::Pane
            })
        }
        b"--reset" | b"--browser" => {
            if let Some(file) = rest.first() {
                not_an_option(file)?;
            }
            if let Some(stray) = rest.get(1) {
                // The form a stray argument is pointed at is the one the sh
                // implementation named for each flag.
                let form = if first == "--reset" {
                    "blueline --reset <file.md>"
                } else {
                    "blueline --term <file.md>"
                };
                return Err(no_more_args(stray, form));
            }
            let file = optional_file(rest.first());
            Ok(if first == "--reset" {
                Mode::Reset { file }
            } else {
                Mode::Browser { file }
            })
        }
        b"--" => {
            if rest.is_empty() {
                return Err(Failure::usage_only());
            }
            Ok(Mode::Action {
                command: rest.to_vec(),
            })
        }
        b"--term" => {
            // The two ways to get --term wrong that the generic checks would
            // only answer with the usage line: leaving the file off, and
            // stacking another flag on it.
            let file = match rest.first() {
                None => return Err(term_needs_file()),
                Some(f) if f.is_empty() => return Err(term_needs_file()),
                Some(f) if !is_markdown_spelling(f) && starts_with_dash(f) => {
                    return Err(Failure::new(format!(
                        "--term takes a file, not {}",
                        show(f)
                    )));
                }
                Some(f) => f,
            };
            if let Some(stray) = rest.get(1) {
                return Err(no_more_args(stray, "blueline --term <file.md>"));
            }
            check_file_spelling(file)?;
            Ok(Mode::Preview {
                file: file.clone(),
                term: true,
            })
        }
        _ => {
            if first.is_empty() {
                return Err(Failure::usage_only());
            }
            if let Some(stray) = rest.first() {
                return Err(no_more_args(stray, "blueline --term <file.md>"));
            }
            check_file_spelling(first)?;
            Ok(Mode::Preview {
                file: first.clone(),
                term: false,
            })
        }
    }
}

fn term_needs_file() -> Failure {
    Failure::new("--term needs a file: 'blueline --term <file.md>'")
}

/// A markdown path is the only thing taken as a file. Write `./snapshot` to
/// mean the file, and `blueline -- snapshot` to mean the command.
fn check_file_spelling(arg: &OsStr) -> Result<(), Failure> {
    if is_markdown_spelling(arg) {
        return Ok(());
    }
    if starts_with_dash(arg) {
        return Err(Failure::usage(format!("unknown option: {}", show(arg))));
    }
    Err(Failure::new(format!(
        "'{0}' is not a markdown path - name a file with a .md suffix or a ./ prefix, or put a command for the preview after --: 'blueline -- {0}'",
        show(arg)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Mode, Failure> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args)
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn message(result: Result<Mode, Failure>) -> String {
        result.expect_err("expected a failure").to_string()
    }

    #[test]
    fn no_arguments_prints_usage() {
        let failure = parse_strs(&[]).unwrap_err();
        assert!(failure.show_usage);
        assert!(failure.message.is_none());
    }

    #[test]
    fn a_markdown_file_is_a_browser_preview() {
        assert_eq!(
            parse_strs(&["README.md"]).unwrap(),
            Mode::Preview {
                file: os("README.md"),
                term: false
            }
        );
        for name in ["notes.markdown", "a.mdown", "./snapshot", "dir/file"] {
            assert!(matches!(
                parse_strs(&[name]).unwrap(),
                Mode::Preview { term: false, .. }
            ));
        }
    }

    #[test]
    fn a_dashed_markdown_name_is_still_a_file() {
        assert_eq!(
            parse_strs(&["--term.md"]).unwrap(),
            Mode::Preview {
                file: os("--term.md"),
                term: false
            }
        );
        assert_eq!(
            parse_strs(&["--term", "./--x"]).unwrap(),
            Mode::Preview {
                file: os("./--x"),
                term: true
            }
        );
    }

    #[test]
    fn a_bare_word_points_at_double_dash() {
        let msg = message(parse_strs(&["snapshot"]));
        assert!(msg.contains("'snapshot' is not a markdown path"), "{msg}");
        assert!(msg.contains("'blueline -- snapshot'"), "{msg}");
    }

    #[test]
    fn unknown_options_print_usage() {
        let failure = parse_strs(&["--nope"]).unwrap_err();
        assert!(failure.show_usage);
        assert_eq!(failure.to_string(), "unknown option: --nope");
        assert!(parse_strs(&["--browser", "--nope"]).unwrap_err().show_usage);
        assert!(parse_strs(&["--reset", "-x"]).unwrap_err().show_usage);
    }

    #[test]
    fn arguments_after_the_file_are_refused() {
        let msg = message(parse_strs(&["README.md", "--term"]));
        assert_eq!(
            msg,
            "unexpected argument after the file: --term - flags go first ('blueline --term <file.md>')"
        );
        let msg = message(parse_strs(&["--reset", "a.md", "b.md"]));
        assert!(msg.contains("('blueline --reset <file.md>')"), "{msg}");
        let msg = message(parse_strs(&["--term", "a.md", "b.md"]));
        assert!(msg.contains("after the file: b.md"), "{msg}");
        assert!(parse_strs(&["--stop", "x"]).is_err());
        assert!(parse_strs(&["--pane", "x"]).is_err());
    }

    #[test]
    fn term_needs_a_file_and_not_a_flag() {
        assert_eq!(
            message(parse_strs(&["--term"])),
            "--term needs a file: 'blueline --term <file.md>'"
        );
        assert_eq!(
            message(parse_strs(&["--term", "--browser"])),
            "--term takes a file, not --browser"
        );
        assert!(message(parse_strs(&["--term", "snapshot"])).contains("not a markdown path"));
    }

    #[test]
    fn optional_file_modes() {
        assert_eq!(
            parse_strs(&["--browser"]).unwrap(),
            Mode::Browser { file: None }
        );
        assert_eq!(
            parse_strs(&["--browser", ""]).unwrap(),
            Mode::Browser { file: None }
        );
        assert_eq!(
            parse_strs(&["--reset", "a.md"]).unwrap(),
            Mode::Reset {
                file: Some(os("a.md"))
            }
        );
        // --browser and --reset take any existing path, as before: the server
        // is what refuses a suffix it will not render.
        assert_eq!(
            parse_strs(&["--browser", "notes.txt"]).unwrap(),
            Mode::Browser {
                file: Some(os("notes.txt"))
            }
        );
    }

    #[test]
    fn everything_after_double_dash_is_passed_through() {
        assert_eq!(
            parse_strs(&["--", "screenshot", "--full", "/tmp/a.png"]).unwrap(),
            Mode::Action {
                command: vec![os("screenshot"), os("--full"), os("/tmp/a.png")]
            }
        );
        assert!(parse_strs(&["--"]).unwrap_err().show_usage);
    }

    #[test]
    fn simple_flags() {
        assert_eq!(parse_strs(&["--stop"]).unwrap(), Mode::Stop);
        assert_eq!(parse_strs(&["--pane"]).unwrap(), Mode::Pane);
        assert_eq!(parse_strs(&["--help"]).unwrap(), Mode::Help);
        assert_eq!(parse_strs(&["-V"]).unwrap(), Mode::Version);
    }
}
