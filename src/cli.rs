use clap::{Parser, Subcommand};

/// Run composer/npm inside a sandboxed microVM and review what it did before
/// anything touches your repo.
///
/// Global flags go before the package-manager command:
/// `boxme --strict composer install`.
#[derive(Parser, Debug)]
#[command(name = "boxme", version, about)]
pub struct Cli {
    /// Deny-by-default network: only package registries are reachable.
    #[arg(long, global = true)]
    pub strict: bool,

    /// Observe the command once, let you pick which contacted domains to trust,
    /// save them to `.boxme/allow`, then re-run the command under deny-by-default
    /// enforcement before the file review.
    #[arg(long, global = true)]
    pub learn: bool,

    /// Keep the VM running after the run instead of removing it.
    #[arg(long, global = true)]
    pub keep: bool,

    /// Guest memory in MiB.
    #[arg(long, global = true, default_value_t = 2048)]
    pub memory: u32,

    /// Guest CPU count.
    #[arg(long, global = true, default_value_t = 2)]
    pub cpus: u8,

    /// Pass an environment variable into the guest: `-e KEY` copies the host
    /// value, `-e KEY=VALUE` sets it explicitly. Repeatable.
    #[arg(short = 'e', long = "env", global = true, value_name = "KEY[=VALUE]")]
    pub env: Vec<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build the boxme-base snapshot (PHP 8.3-8.5, Node, composer/npm guards, tcpdump).
    Setup {
        /// Rebuild even if the snapshot already exists.
        #[arg(long)]
        force: bool,
    },
    /// Anything else is the package-manager command to run, e.g. `boxme composer i`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
