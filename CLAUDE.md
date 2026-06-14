# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`boxme` is a single-binary Rust CLI that runs `composer install` / `npm install`
(and friends) inside a [microsandbox](https://microsandbox.dev) microVM instead
of on the host, so postinstall scripts and composer plugins can't touch your
machine. After the command runs it shows a full-screen TUI review of every file
change and network destination, and only copies the result back into the repo on
explicit approval. Read `README.md` for the user-facing behavior and `PLAN.md`
for the original design rationale and decisions.

## Commands

```sh
cargo build                 # debug build
cargo build --release       # release build
cargo test                  # run all unit tests (they live inline in modules)
cargo test parse_tcpdump    # run a single test by name substring
cargo clippy --all-targets  # lint
cargo fmt                    # format

cargo install --path .      # install the `boxme` binary
boxme setup                 # build the boxme-base snapshot once (~10 min); required before any run
boxme setup --force         # rebuild the snapshot (needed after changing BASE_SETUP)
```

Tests are pure unit tests in `#[cfg(test)] mod tests` blocks (in `netcap.rs`,
`allowlist.rs`, `manifest.rs`, `outside.rs`, `review.rs`). There is no
integration-test harness — anything touching the sandbox is exercised by running
`boxme` against a real project. There is no rustfmt/clippy config and no
toolchain pin; default stable applies.

Note the deliberate version ceiling on `time` in `Cargo.toml`: `>=0.3.6,
<0.3.47` works around an rcgen 0.14 coherence conflict pulled in transitively by
`microsandbox-network`. Don't bump it until rcgen ships a fix.

## Architecture

Entry point `main.rs` parses the CLI and dispatches to `setup::setup` or
`run::run`. `cli.rs` uses clap derive; everything that isn't the `setup`
subcommand falls into an `#[command(external_subcommand)] Run(Vec<String>)`,
which is why `boxme composer i` works — global flags (`--strict`, `--learn`,
`--keep`, `--memory`, `--cpus`, `-e`) must come *before* the package-manager
command.

`run.rs` is the orchestrator and the file to read first. It validates the tool
is `composer`/`npm`, detects versions, then chooses one of two paths:

- **`enforced_run`** — single pass under a fixed `NetworkPolicy`: boot, run,
  read-only review, copy back on approval.
- **`learn_run`** — runs when `--learn` is passed or a project has no
  `.boxme/allow` yet. Observes the command with the network open, lets the user
  trust contacted hosts in the review, saves them to `.boxme/allow`, then either
  copies back directly (if nothing it touched would be blocked under
  enforcement) or discards the observe VM and re-runs via `enforced_run` for a
  clean result.

`run_command` is the shared core both paths call: tar the project in → unpack and
tag a guest git baseline → switch PHP/Node versions → snapshot the file manifest
→ start tcpdump → run the command interactively via `attach_with` → stop capture
→ diff the manifest → scan for out-of-workspace writes → build the review rows.

### Module responsibilities

- `setup.rs` — builds the `boxme-base` snapshot from `node:24` (one-time, slow).
- `detect.rs` — resolves guest PHP `X.Y` and Node major: host binary run from the
  project dir first (so mise/asdf/herd shims resolve per-directory), then
  manifest constraints (`composer.json` require.php, `.nvmrc`, `engines.node`),
  then defaults. PHP is clamped to the 8.3–8.5 baked into the image.
- `scripts.rs` — **every shell snippet that runs inside the guest lives here** as
  a `const` or a `format!`-returning fn. The base-image setup, the unpack/baseline
  script, version switching, the file manifest, the out-of-workspace scan, and
  the tcpdump start/parse commands are all here. Change guest behavior here, not
  inline.
- `netcap.rs` — tcpdump lifecycle plus a lenient token-scanner that parses
  `tcpdump -r` text output, joins DNS answers to SYN destinations, and classifies
  each contact as a known registry vs unexpected. Network capture runs *inside
  the guest* because microsandbox 0.5 exposes no host-side per-connection
  observability.
- `manifest.rs` — computes the expected write-set per command, parses the guest
  file manifest, and diffs before/after into added/modified/deleted.
- `outside.rs` — parses the scan for anything written outside `/workspace` (a
  supply-chain red flag; reported only, never copied back).
- `allowlist.rs` — the `.boxme/allow` per-project file (load/merge/save, entry
  matching).
- `review.rs` — the ratatui TUI (Files / Network / Outside tabs, inline diffs,
  host selection in learn mode). Returns a `Decision` plus the chosen hosts.
- `copyback.rs` — on approval, tars the approved paths out of the guest and
  unpacks them into the project, rejecting absolute/`..` paths and applying
  deletions only within the project dir.
- `util.rs` — `tar_directory`, shell quoting/slugify, `shell_capture`,
  `stream_shell_stderr` (streams guest exec output to the host terminal).

### Network policy & the observe/enforce model

A run is either **observe** (every outbound TCP succeeds and is recorded) or
**enforce** (deny-by-default; only DNS + registries + the allowlist reach the
network). UDP is always blocked apart from DNS — that closes the QUIC/raw-UDP
exfil path tcpdump's SYN capture can't see. The three policies are built in
`run.rs`: `observe_policy`, `strict_policy` (registries only, from
`netcap::STRICT_DOMAINS`), and `enforced_policy` (strict + allowlist entries).
The mere *existence* of `.boxme/allow` flips a normal run from observe to
enforce; `--strict` ignores the allowlist and permits only registries.

`.boxme/allow` format: one host per line, a bare entry matches the domain and all
subdomains, a `=` prefix matches that exact host only, `#` comments and blanks
ignored. Commit it to share the decision with a team.

### Security boundaries to preserve when editing

- The guest git baseline (`UNPACK` in `scripts.rs`) disables hooks via
  `core.hooksPath=/dev/null` and `--no-verify` so committing the baseline can't
  run project code, and force-adds gitignored files so later modifications still
  diff.
- The file manifest is NUL-delimited with the path last, so a guest-controlled
  filename containing tabs/newlines can't forge a review entry.
- Copy-back treats the tarball as sandbox-controlled: it rejects absolute paths
  and `..` components and never follows symlinks out of the project dir.
- The out-of-workspace scan uses `-newercm` (ctime, which the guest can't
  backdate with `touch -t`) against a marker stamped *after* setup but *before*
  the command, so it reports only what the command itself wrote.

## Conventions

- Guest-side logic belongs in `scripts.rs`, never inlined into `run.rs`.
- Anything interpolated into a guest shell is validated host-side first (e.g. PHP
  versions against `PHP_VERSIONS`) or quoted via `util::shell_quote`.
- VMs are booted *attached* so a crash SIGTERMs them; teardown goes through
  `cleanup`/`discard`/`remove_vm` in `run.rs`, which drop the handle before
  `Sandbox::remove` (remove operates by name). `--keep` detaches instead.
- The base snapshot is named `boxme-base` (`setup::BASE_SNAPSHOT`); the three
  shared cache volumes (`boxme-composer-cache`, `boxme-npm-cache`,
  `boxme-node-versions`) are ensured before boot in `ensure_cache_volumes`.
