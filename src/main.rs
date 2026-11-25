//! oxisort binary: parse arguments, run the pipeline, map the result to an
//! exit code (0 ok, 1 some files failed, 2 usage error, 3 fatal, 130 SIGINT).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;

use oxisort::exec::Stop;
use oxisort::logging::{HEADER, Logger, Progress};

fn main() -> ExitCode {
    let args = match oxisort::cli::parse_from(std::env::args_os()) {
        Ok(a) => a,
        // clap prints help/version/usage errors itself and picks 0 or 2.
        Err(e) => e.exit(),
    };
    let cfg = match args.into_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("oxisort: {e}");
            return ExitCode::from(2);
        }
    };
    let progress = if cfg.progress {
        Progress::enabled()
    } else {
        Progress::disabled()
    };
    let logger = match Logger::new(cfg.verbosity, progress.multi(), cfg.log_file.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            let path = cfg
                .log_file
                .as_deref()
                .unwrap_or(std::path::Path::new(""))
                .display();
            eprintln!("oxisort: cannot open log file {path}: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = logger.install() {
        eprintln!("oxisort: cannot install logger: {e}");
        return ExitCode::from(3);
    }
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    log::info!(target: HEADER, "oxisort {} run: {}", env!("CARGO_PKG_VERSION"), argv.join(" "));
    if let Err(e) = oxisort::signal::install() {
        log::warn!("Ctrl-C handling unavailable: {e}");
    }

    let stop = Stop::new();
    let result = catch_unwind(AssertUnwindSafe(|| oxisort::run(&cfg, &stop, &progress)));
    let code = match result {
        Ok(Ok(summary)) => summary.exit_code(),
        Ok(Err(fatal)) => {
            log::error!("{fatal}");
            3
        }
        Err(_) => {
            log::error!("internal error (panic); see the message above");
            3
        }
    };
    log::logger().flush();
    ExitCode::from(code)
}
