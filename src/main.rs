//! `blueline`: preview markdown in a browser tab or a terminal-browser pane.
//!
//! One binary plays both roles. Run normally it is the CLI; run as
//! `blueline serve` (a hidden subcommand) it is the render server, which the CLI
//! starts by re-executing itself.

mod assets;
mod cli;
mod render;
mod server;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cli.command {
        Some(cli::Command::Serve(args)) => server::run(&args),
        None => cli::run(&cli.preview),
    }
}
