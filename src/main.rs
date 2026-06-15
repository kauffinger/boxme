mod allowlist;
mod cli;
mod copyback;
mod detect;
mod manifest;
mod netcap;
mod outside;
mod review;
mod run;
mod scripts;
mod setup;
mod util;

use clap::Parser;
use owo_colors::OwoColorize;

#[tokio::main]
async fn main() {
    let parsed = cli::Cli::parse();
    let result = match &parsed.command {
        cli::Command::Setup { force, disk } => setup::setup(*force, *disk).await,
        cli::Command::Run(args) => run::run(&parsed, args).await,
    };
    if let Err(e) = result {
        eprintln!("{} {e:#}", "error:".red().bold());
        std::process::exit(1);
    }
}
