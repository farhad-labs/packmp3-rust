// main.rs - packmp3 command line binary.
// The whole UI lives in the `packmp3::cli` module; this is just a thin wrapper
// around `Cli::run_main` (the port of the C++ main()).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut cli = packmp3::cli::Cli::new();
    let rc = cli.run_main(&args);
    if rc < 0 {
        // C++ main() returns -1 to signal "wrong input"
        ExitCode::from(255)
    } else {
        ExitCode::from(rc as u8)
    }
}
