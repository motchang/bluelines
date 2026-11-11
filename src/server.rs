//! The render server: plain HTTP on the loopback port. It serves rendered
//! pages for registered files, the embedded assets, and a reload stream per
//! page that fires when the file is saved.

use std::process::ExitCode;

use crate::cli::ServeArgs;

/// Runs the server in the foreground. Not ported yet: see
/// `blueline-server/server.ts` for the behaviour this will take over.
pub fn run(args: &ServeArgs) -> ExitCode {
    eprintln!(
        "blueline: the Rust render server is not implemented yet (port {})",
        args.port
    );
    ExitCode::FAILURE
}
