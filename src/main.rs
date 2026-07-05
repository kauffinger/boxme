mod allowlist;
mod auth;
mod claude;
mod cli;
mod composer_auth;
mod copyback;
mod detect;
mod dev;
mod manifest;
mod netcap;
mod outside;
mod report;
mod review;
mod run;
mod scripts;
mod setup;
mod skills;
mod util;
mod vms;

use clap::Parser;
use owo_colors::OwoColorize;

#[tokio::main]
async fn main() {
    let parsed = cli::Cli::parse();
    // `run` maps its non-interactive report onto an exit code; everything else
    // is success/failure.
    let result = match &parsed.command {
        cli::Command::Setup { force, disk } => setup::setup(*force, *disk).await.map(|()| 0),
        cli::Command::Dev { port, cmd } => dev::dev(&parsed, cmd, port).await.map(|()| 0),
        cli::Command::Attach { vm, cmd } => vms::attach(vm.as_deref(), cmd).await.map(|()| 0),
        // `exec` exits with the guest command's own code, like `docker exec`.
        cli::Command::Exec { vm, cmd } => vms::exec(vm.as_deref(), cmd).await,
        cli::Command::Ps => vms::ps(parsed.json).await.map(|()| 0),
        cli::Command::Kill { names, all } => vms::kill(names, *all).await.map(|()| 0),
        cli::Command::Claude { prompt } => claude::claude(&parsed, prompt).await.map(|()| 0),
        cli::Command::Login => auth::login().map(|()| 0),
        cli::Command::Logout => auth::logout().map(|()| 0),
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
