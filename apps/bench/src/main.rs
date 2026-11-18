//! `bench`: measurement and diagnostics CLI for ziz.

mod com;
mod os_info;
mod probe;
mod process_stats;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "bench",
    version,
    about = "ziz measurement and diagnostics tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List adapters, outputs, capture support and hardware codecs (read-only; starts no capture).
    Probe(ProbeArgs),
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// Where to write the JSON report [default: <lab>/probe-<unix ms>.json].
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,

    /// Run the probe this many times and report handle and memory growth.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    repeat: u32,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Probe(args) => probe::run(args.json, args.repeat),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
