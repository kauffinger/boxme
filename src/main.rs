mod allowlist;
mod cli;
mod composer_auth;
mod copyback;
mod detect;
mod dns;
mod manifest;
mod netcap;
mod outside;
mod packages;
mod report;
mod review;
mod run;
mod scripts;
mod setup;
mod skills;
mod util;

use clap::Parser;
use owo_colors::OwoColorize;

#[tokio::main]
async fn main() {
    // Before anything can spawn the VMM: it inherits our RLIMIT_NOFILE, and
    // every open virtiofs lower file pins a host fd in it (see raise_fd_limit).
    util::raise_fd_limit();
    let parsed = cli::Cli::parse();
    // `run` exits with the guest command's code (or the --json report's);
    // everything else is success/failure.
    let result = match &parsed.command {
        cli::Command::Setup { force, disk } => setup::setup(*force, *disk).await.map(|()| 0),
        cli::Command::Apply => report::apply(parsed.json).map(|()| 0),
        cli::Command::Discard => report::discard(parsed.json).map(|()| 0),
        cli::Command::Skills => skills::install().map(|()| 0),
        cli::Command::Allow { hosts } => run::allow_hosts(hosts).map(|()| 0),
        cli::Command::Run(args) => run::run(&parsed, args).await,
    };
    match result {
        Ok(code) => {
            if code != 0 {
                std::process::exit(code);
            }
        }
        Err(e) => {
            eprintln!("{} {e:#}", "error:".red().bold());
            std::process::exit(1);
        }
    }
}
