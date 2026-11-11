//! Command-line interface: argument parsing and the client side of blueline,
//! which finds or starts the render server and points a surface at a file.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

/// The render server's default loopback port; `BLUELINE_PORT` overrides it.
pub const DEFAULT_PORT: u16 = 43128;

#[derive(Debug, Parser)]
#[command(name = "blueline", version, about)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub preview: PreviewArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the render server in the foreground. The CLI starts it by
    /// re-executing itself with this subcommand; it is not meant to be typed.
    #[command(hide = true)]
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
pub struct PreviewArgs {
    /// The markdown file to preview.
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The loopback port to listen on.
    #[arg(long, env = "BLUELINE_PORT", default_value_t = DEFAULT_PORT)]
    pub port: u16,
}

/// Runs the client side. Not ported yet: see the sh implementation in
/// `bin/blueline` for the behaviour this will take over.
pub fn run(args: &PreviewArgs) -> ExitCode {
    let _ = &args.file;
    eprintln!("blueline: the Rust port does not preview files yet - use bin/blueline");
    ExitCode::FAILURE
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
    fn file_argument_parses() {
        let cli = Cli::try_parse_from(["blueline", "README.md"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.preview.file, Some(PathBuf::from("README.md")));
    }

    #[test]
    fn serve_is_hidden_but_parses() {
        let cli = Cli::try_parse_from(["blueline", "serve", "--port", "5000"]).unwrap();
        match cli.command {
            Some(Command::Serve(args)) => assert_eq!(args.port, 5000),
            other => panic!("expected serve, got {other:?}"),
        }
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("serve"), "serve should not appear in --help");
    }
}
