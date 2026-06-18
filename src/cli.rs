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

    /// Don't copy the project's `.git` directory into the sandbox. The guest
    /// rebuilds its own baseline from the working tree, so the diff is
    /// unaffected — this just drops the (often large) history.
    #[arg(long, short = 'G', global = true)]
    pub without_git: bool,

    /// Don't copy image/video/audio/archive assets into the sandbox.
    /// composer/npm install scripts don't read them, and they often dominate a
    /// large repo's size.
    #[arg(long, short = 'M', global = true)]
    pub without_media: bool,

    /// Don't copy the existing `vendor/`/`node_modules/` into the sandbox; let
    /// the guest install from scratch. Smaller transfer for a full install; the
    /// default copies them in so incremental commands (`composer require`,
    /// `npm install <pkg>`) only do incremental work.
    #[arg(long, short = 'D', global = true)]
    pub without_deps: bool,

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

        /// Writable overlay size for the base snapshot, in GiB. Sets the disk
        /// ceiling every run inherits. ext4 is sparse, so a large value costs
        /// almost nothing until used. Changing it requires `--force`.
        #[arg(long, default_value_t = 32)]
        disk: u32,
    },
    /// Run a dev-server stack (default `composer run dev`) inside the sandbox,
    /// syncing host edits in one-way so HMR works without the guest ever writing
    /// to your machine. Forwards guest ports back to the host.
    Dev {
        /// Forward a guest port to the host: `HOST:GUEST`, or a bare `PORT` for
        /// both sides. Repeatable. Defaults to 8000 and 5173.
        #[arg(long, short = 'p', value_name = "[HOST:]GUEST")]
        port: Vec<String>,

        /// The command to run inside the guest. Defaults to `composer run dev`.
        /// Put it after the flags: `boxme dev -p 3000 npm run dev`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        cmd: Vec<String>,
    },

    /// Open another shell — or run a one-off command — inside the running
    /// `boxme dev` session for the current folder, e.g. `boxme attach` or
    /// `boxme attach php artisan migrate`.
    Attach {
        /// Command to run in the dev session. Defaults to an interactive shell.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        cmd: Vec<String>,
    },

    /// Anything else is the package-manager command to run, e.g. `boxme composer i`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
