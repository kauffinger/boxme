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

    /// Guest PHP version (8.3, 8.4 or 8.5) instead of the detected one. Pass
    /// the version the app is deployed on, so composer resolves updates for it
    /// rather than for whatever PHP this machine runs.
    #[arg(long, global = true, value_name = "X.Y")]
    pub php: Option<String>,

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

    /// Copy the changeset staged by a `boxme --json <command>` run into the
    /// project, then remove it from `.boxme/pending`. The explicit second step
    /// of the non-interactive flow.
    Apply {
        /// Apply only these paths and whatever lies beneath them, e.g.
        /// `boxme apply composer.lock public`. The rest of the changeset is
        /// dropped. Without paths, everything is applied.
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },

    /// Drop the changeset staged by a `boxme --json <command>` run without
    /// applying it.
    Discard,

    /// Install the bundled Claude Code skills (fleet-update, fleet-fix) into
    /// `~/.claude/skills`, so any Claude session can sweep a whole folder of
    /// repos through boxme — "update all repos in ~/Code for me".
    Skills,

    /// Add host(s) to the allowlist (`.boxme/allow`) without the review TUI —
    /// the non-interactive counterpart of trusting a host in the review. A bare
    /// host matches the domain and all subdomains; prefix with `=` for that
    /// exact host only.
    Allow {
        #[arg(required = true, value_name = "HOST")]
        hosts: Vec<String>,
    },

    /// Anything else is the command to run: composer, npm or php, e.g.
    /// `boxme composer i`. Chain commands with `++` to run them sequentially in
    /// one sandbox and review one combined changeset, e.g.
    /// `boxme --json composer install ++ composer run-script post-update-cmd` —
    /// the script hook sees `vendor/` without an apply in between — or
    /// `boxme composer install ++ php artisan test`.
    #[command(external_subcommand)]
    Run(Vec<String>),
}
