//! `blueline`: preview markdown in a browser tab or a terminal-browser pane.
//!
//! One binary plays both roles. Run normally it is the CLI; run as
//! `blueline serve` (a hidden subcommand) it is the render server, which the CLI
//! starts by re-executing itself.

mod assets;
mod auth;
mod cli;
mod render;
mod server;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // Only the server goes through clap. The preview grammar - `--` handing the
    // rest of the line to terminal-browser, markdown spellings deciding what is
    // a file - is parsed by hand in `cli`.
    if cli::is_serve(&args) {
        let cli::Command::Serve(serve) = cli::Cli::parse().command;
        return server::run(&serve);
    }
    cli::run(args.get(1..).unwrap_or_default())
}
