//! Entry point for the Margin executable: mode selection and dependency
//! wiring (SPEC 13.1). The command line interface owns this path; the
//! terminal and hook integrations enter through their own commands.

use std::process::ExitCode;

fn main() -> ExitCode {
    margin::cli::main()
}
