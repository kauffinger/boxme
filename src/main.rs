mod allowlist;
mod auth;
mod claude;
mod cli;
mod copyback;
mod detect;
mod dev;
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
        cli::Command::Dev { port, cmd } => dev::dev(&parsed, cmd, port).await,
        cli::Command::Attach { cmd } => dev::attach(cmd).await,
        cli::Command::Claude { prompt } => claude::claude(&parsed, prompt).await,
        cli::Command::Login => auth::login(),
        cli::Command::Logout => auth::logout(),
        cli::Command::Run(args) => run::run(&parsed, args).await,
    };
    if let Err(e) = result {
        eprintln!("{} {e:#}", "error:".red().bold());
        std::process::exit(1);
    }
}
