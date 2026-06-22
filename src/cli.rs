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

    /// Run Claude Code inside the sandbox in full autonomy
    /// (`--dangerously-skip-permissions`), then commit exactly what it changed
    /// onto a fresh `boxme/claude-<n>` branch you can diff and merge. Give a prompt
    /// for a one-shot headless run, or omit it for an interactive session:
    /// `boxme claude` or `boxme claude 'fix the failing test'`.
    Claude {
        /// One-shot prompt (headless). Omit for an interactive session. Put it
        /// after the global flags: `boxme --learn claude 'upgrade to PHP 8.4'`.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "PROMPT"
        )]
        prompt: Vec<String>,
    },

    /// Save the Claude Code OAuth token (from `claude setup-token`) to your system
    /// keychain so `boxme claude` can authenticate without a token in your shell
    /// environment. Prompts for the token (echo off); pipe it in to script setup.
    Login,

    /// Remove the stored Claude token from your keychain.
    Logout,

    /// Anything else is the package-manager command to run, e.g. `boxme composer i`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
