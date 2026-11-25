//! oxisort binary: parse arguments, run the pipeline, map the result to an
//! exit code (0 ok, 1 some files failed, 2 usage error, 3 fatal, 130 SIGINT).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args = match oxisort::cli::parse_from(std::env::args_os()) {
        Ok(a) => a,
        // clap prints help/version/usage errors itself and picks 0 or 2.
        Err(e) => e.exit(),
    };
    match args.into_config() {
        Ok(_cfg) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oxisort: {e}");
            ExitCode::from(2)
        }
    }
}
