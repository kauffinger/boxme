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

    /// Inject your global composer `auth.json` (`~/.config/composer/auth.json`)
    /// into the sandbox as TLS-proxy secrets: the guest only ever sees opaque
    /// placeholders, while microsandbox substitutes the real tokens onto the
    /// wire — and only for each credential's own host. A private repo still has
    /// to be reachable (`.boxme/allow`); the credential rides along once it is.
    #[arg(short = 'a', long, global = true)]
    pub composer_auth: bool,

    /// Non-interactive mode for scripts and agents: skip the review TUI, print
    /// a JSON report of everything the command did to stdout (guest output goes
    /// to stderr), and stage the changeset under `.boxme/pending` instead of
    /// applying it. Review the report, then `boxme apply` or `boxme discard`.
    /// Always enforces the network policy (use `boxme allow` to extend it).
    /// Exit codes: 0 clean, 1 boxme error, 2 command failed, 3 findings.
    #[arg(long, global = true)]
    pub json: bool,

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

    /// Copy the changeset staged by a `boxme --json <command>` run into the
    /// project, then remove it from `.boxme/pending`. The explicit second step
    /// of the non-interactive flow.
    Apply,

    /// Drop the changeset staged by a `boxme --json <command>` run without
    /// applying it.
    Discard,

    /// Add host(s) to the package-run allowlist (`.boxme/allow`) without the
    /// review TUI — the non-interactive counterpart of trusting a host in the
    /// review. A bare host matches the domain and all subdomains; prefix with
    /// `=` for that exact host only.
    Allow {
        #[arg(required = true, value_name = "HOST")]
        hosts: Vec<String>,
    },

    /// Anything else is the package-manager command to run, e.g. `boxme composer i`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
