# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`boxme` is a single-binary Rust CLI that runs `composer install` / `npm install`
(and friends) inside a [microsandbox](https://microsandbox.dev) microVM instead
of on the host, so postinstall scripts and composer plugins can't touch your
machine. After the command runs it shows a full-screen TUI review of every file
change and network destination, and only copies the result back into the repo on
explicit approval. Read `README.md` for the user-facing behavior. That is the
whole product: there is deliberately no dev-server mode, no agent mode and no
VM-management surface — keep it that way.

## Commands

```sh
cargo build                 # debug build
cargo build --release       # release build
cargo test                  # run all unit tests (they live inline in modules)
cargo test parse_tcpdump    # run a single test by name substring
cargo clippy --all-targets  # lint
cargo fmt                    # format

cargo install --path .      # install the `boxme` binary
boxme setup                 # install the libkrun runtime + build the boxme-base snapshot once (~10 min); required before any run
boxme setup --force         # rebuild the snapshot (needed after changing BASE_SETUP or --disk)
boxme setup --disk 64       # build the snapshot with a 64 GiB writable overlay (default 32)

boxme composer install      # interactive: run in the box, review in the TUI, copy back on approval
boxme --json composer i     # non-interactive: JSON report on stdout, changeset staged to .boxme/pending (not applied)
boxme apply                 # copy the staged changeset into the project (second step of the --json flow)
boxme discard               # drop the staged changeset instead
boxme allow foo.com         # add allowlist host(s) without the TUI (=exact.host for exact-only)
boxme skills                # install the bundled fleet-update/fleet-fix Claude Code skills to ~/.claude/skills
```

## Releasing

Teammates install with the `curl … | sh` one-liner in `README.md`, which pulls a
prebuilt binary from GitHub Releases. To cut a release:

```sh
# bump version in Cargo.toml, commit, then:
git tag v0.2.0
git push origin v0.2.0
```

`.github/workflows/release.yml` fires on any `v*` tag: it builds
`aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
natively (one hosted runner each — cross-compiling the microsandbox/msb_krun tree
is not worth the pain), packages each as `boxme-<target>.tar.gz` + a `.sha256`,
and publishes them to a Release via the runner's `gh` CLI. Builds use `--locked`
so the pinned dependency set in `Cargo.lock` is honored. `install.sh` resolves the
latest tag from the releases redirect (no API rate limit), downloads the matching
tarball, and verifies the checksum before installing to `~/.local/bin`. The
workflow can also be run manually (`workflow_dispatch`) with a tag input.

`.github/workflows/ci.yml` gates pushes/PRs on `cargo fmt --check`, `clippy -D
warnings`, and `cargo test` (one Linux runner) so a broken tree never gets
tagged. Both workflows `apt-get install cmake pkg-config libdbus-1-dev
libcap-ng-dev` — the native build deps microsandbox pulls in (aws-lc-sys,
libdbus-sys via the SDK's `keyring` feature, and libcap-ng, which msb_krun links
for Linux capability handling). msb_krun is otherwise pure Rust, so there is no
libkrun system package.

Tests are pure unit tests in `#[cfg(test)] mod tests` blocks (in `netcap.rs`,
`allowlist.rs`, `manifest.rs`, `outside.rs`, `review.rs`, `report.rs`,
`copyback.rs`, `skills.rs`, `run.rs`, `composer_auth.rs`, `detect.rs`,
`scripts.rs`, `packages.rs`, `dns.rs`, `util.rs`). There is no integration-test harness — anything
touching the sandbox is exercised by running `boxme` against a real project.
There is no rustfmt/clippy config and no toolchain pin; default stable applies.

## Architecture

Entry point `main.rs` parses the CLI and dispatches to `setup::setup`,
`report::apply`, `report::discard`, `skills::install`, `run::allow_hosts`, or
`run::run`. `cli.rs` uses clap derive; `setup`, `apply`, `discard`, `skills` and
`allow` are named subcommands, and everything else falls into an
`#[command(external_subcommand)] Run(Vec<String>)`, which is why `boxme composer
i` works — global flags (`--strict`, `--learn`, `--composer-auth`, `--json`,
`--memory`, `--cpus`, `-e`) must come *before* the package-manager command.
`run::run` returns an exit code that `main` propagates: the `--json` report's
documented code, or — interactively — the guest command's own exit code on
approval and `1` (`run::EXIT_ABORTED`) when the user aborts the review, so
`boxme composer install && php artisan migrate` stops when composer failed in
the box.

`run.rs` is the orchestrator and the file to read first. It splits the args on
`++` into a command chain (`split_commands` — each command must start with
`composer`/`npm`; the chain runs sequentially in one guest via `&&` and
produces one combined changeset, with `manifest::expected_write_set_all` as the
union write-set), detects versions, then chooses one of three paths (the TUI
paths fail fast with a pointer to `--json` when stdin/stdout isn't a terminal):

- **`enforced_run`** — deny-by-default pass: boot under the allowlist, run,
  review, copy back on approval. If the user marks blocked hosts and confirms,
  it appends them to `.boxme/allow` and re-runs itself clean under the updated
  policy (recursively, discarding the throwaway VM) — so a newly-needed host can
  be allowed straight from the review without `--learn` or hand-editing. Under
  `--strict` this affordance is off (the allowlist is ignored anyway).
- **`learn_run`** — runs when `--learn` is passed or a project has no
  `.boxme/allow` yet. Observes the command with the network open, lets the user
  trust contacted hosts in the review, saves them to `.boxme/allow`, then either
  copies back directly (if nothing it touched would be blocked under
  enforcement) or discards the observe VM and re-runs via `enforced_run` for a
  clean result.
- **`json_run`** — the non-interactive path (`--json`, for agents/scripts/CI):
  always enforces (never learns — no one is present to vouch for a host; with no
  allowlist it's registries-only and the report lists what was blocked for
  `boxme allow <host>` + re-run), streams the command's output to stderr, stages
  the changeset under `.boxme/pending/` (self-gitignored; `copyback::persist`),
  tears the VM down, and prints a JSON `report::Report` to stdout — file diffs
  partitioned expected/unexpected, network contacts classified, outside writes,
  guest exit code. **Nothing is applied**: the explicit second step is
  `boxme apply` (`copyback::load_staged` + `commit`) or `boxme discard`. Exit
  codes: 0 clean, 1 boxme error, 2 command failed (nothing staged), 3 findings
  (`blocked_hosts`, `unexpected_files`, `outside_writes`, plus
  `command_failed` / `*_unavailable`) — derived in `Report::finalize`.

`run_command` is the shared core all paths call: mount the project read-only as
an overlay lower + tag a guest git baseline (`scripts::UNPACK`, no tar copy-in) →
switch PHP/Node versions → snapshot the file manifest → start tcpdump → run the
command interactively via `attach_with` (streamed via `stream_shell_stderr`
under `--json`, keeping stdout clean for the report) → stop capture → diff the
manifest → scan for out-of-workspace writes → build the review rows. Every VM
is throwaway and removed by `remove_vm` as soon as its result has been staged
out (or discarded); there is no keep/attach affordance.

The project is **bind-mounted read-only** at `/ws-lower` (`boot`) and overlaid at
`/workspace` with a guest-local writable upper, so nothing is copied in: reads
fall through to the host tree via virtiofs, the command's writes land in the
upper, and the host stays untouchable for the whole run (virtiofs rejects writes
host-side, the guest kernel returns `EROFS`). The upper can't live on the guest
root — that root is itself an overlayfs, which the kernel refuses as an upperdir
(`not supported as upperdir`) — so `unpack` puts it on a sparse loop-mounted ext4
sized to the guest root's free space (disk-backed, no RAM pressure unlike tmpfs).
Because the host project is the *live overlay lower*, copy-back is split in
two (`copyback::stage` then `copyback::commit`): stage tars the changeset out
while the VM is alive, the VM is torn down, then commit mutates the host tree —
never while it's mounted. The whole project (including `.git`, `vendor`,
`node_modules`) is always mounted; there is no opt-out, since hiding a dir from
`/workspace` wouldn't hide it from in-guest code anyway — the full host tree is
bind-mounted read-only at `/ws-lower`, readable regardless of any whiteout.
Existing `vendor`/`node_modules` stay in place, so incremental commands do
incremental work (`npm ci` clears `node_modules` itself for a clean install).

### Module responsibilities

- `setup.rs` — provisions the libkrun runtime via the SDK
  (`microsandbox::setup::install_runtime`, pinned to the linked crate's
  `PREBUILT_VERSION` via `InstallOptions::default`, into
  `~/.microsandbox/{bin,lib}`), then builds the `boxme-base` snapshot from
  `node:24` (one-time, slow). The runtime install is idempotent — a no-op once
  the matching version is present (checked with `resolve_runtime_version`; the
  SDK's own `ensure_runtime` would accept any older pair), a forced re-download
  on mismatch — so no separate microsandbox CLI install is required. It runs
  before the snapshot-exists early return, so a plain `boxme setup` after a boxme
  update brings the runtime up to the new SDK without `--force`. The crate
  is built with `default-features = false` + `local,net,keyring`: no cloud
  backend, and no `download-binaries` (it installs the runtime on the *build*
  machine, useless for a released binary). The builder sets the writable overlay
  (`root_disk`, `--disk` GiB, default 32) — this is the *only* place the
  per-run disk ceiling can be set, because booting from the snapshot inherits
  the snapshot's fixed-size `upper.ext4` and the SDK forbids resizing it at boot.
  The default microsandbox overlay is just 4 GiB, which a large repo + vendor +
  node_modules + guest-local caches overruns (ENOSPC); ext4 is sparse, so a big
  overlay costs almost nothing until used. Every boot goes through
  `setup::base_builder` (`Sandbox::builder(name).override_snapshot(<path>)`):
  microsandbox 0.7's public `Sandbox::restore` has no `env` / `secret_entry` /
  TLS / `replace` / attached mode, which every boxme boot needs, while the full
  builder still resolves a snapshot source via the doc-hidden `override_snapshot`
  (what the `msb` CLI uses for config-file snapshot sources). The snapshot is
  passed by its *artifact path* (newest `Snapshot::list()` entry named
  `boxme-base`), never by name: since 0.7.8 a bare name selects a snapshot
  group's head, which a pre-0.7.8 flat snapshot doesn't have and a rebuilt root
  doesn't advance. 0.7.8 also made grouped snapshots immutable (`force` is
  rejected), so `setup --force` removes every old `boxme-base` once the new
  builder is ready, then captures into the `boxme` group. Re-check both on
  each microsandbox bump.
- `detect.rs` — resolves guest PHP `X.Y` and Node major: host binary run from the
  project dir first (so mise/asdf/herd shims resolve per-directory), then
  manifest constraints (`composer.json` require.php, `.nvmrc`, `engines.node`),
  then defaults. PHP is clamped to the 8.3–8.5 baked into the image. The host
  PHP wins only when it doesn't contradict the composer.json constraint (a
  small `satisfies` checker handles `^`/`~`/ranges/`||` at X.Y granularity) —
  a project requiring ^8.5 boots 8.5 even when the host runs 8.3.
- `scripts.rs` — **every shell snippet that runs inside the guest lives here** as
  a `const` or a `format!`-returning fn: the base-image setup, the
  unpack/baseline script, version switching, the file manifest (which records
  each symlink's target so a retargeted link diffs as modified), the
  out-of-workspace scan, the npm host-platform export, and the tcpdump
  start/parse commands. Change guest behavior here, not inline.
- `netcap.rs` — tcpdump lifecycle plus a lenient token-scanner that parses
  `tcpdump -r` text output, joins DNS answers to SYN destinations (only answers
  that arrived inbound on the NIC from port 53, so a guest-forged answer can't
  alias an exfil IP to a registry name), lowercases DNS names, and classifies
  each contact as a known registry vs unexpected. Also holds `STRICT_DOMAINS`
  (the registries). Network capture runs *inside the guest* because
  microsandbox exposes no host-side per-connection observability.
- `manifest.rs` — computes the expected write-set per command, parses the guest
  file manifest, and diffs before/after into added/modified/deleted (files by
  md5, symlinks by target, dirs by existence). Every composer command also
  expects `composer-integrity.lock` (the base image's soak-time plugin writes
  it) and Laravel's `bootstrap/cache/{packages,services}.php` (regenerated by
  `package:discover` on every autoload dump) — without them every clean Laravel
  update reported `unexpected_files`. composer.json is expected only for the
  commands (and aliases) that rewrite it: `require`/`r`, `remove`/`rm`/
  `uninstall`, `update`/`u`/`up`/`upgrade`, `bump`.
- `packages.rs` — the package-level summary of an update: parses composer.lock
  (`packages` + `packages-dev`, dev branches keyed by commit) and
  package-lock.json (v2/v3 `packages` keyed by name over the set of installed
  versions so tree moves don't count, v1 nested `dependencies`) before (host
  copy — the read-only lower) and after (guest `/workspace` copy), and diffs
  them into `PackageChange`s classified major/minor/patch/downgrade/changed/
  added/removed. Below 1.0 the first non-zero part is the breaking one (`^`
  semantics), so 0.1 → 0.5 is major. Shown above the lockfile's diff in the
  review, printed after approval, and emitted as `packages` in the JSON report.
- `outside.rs` — parses the scan for anything written outside `/workspace` (a
  supply-chain red flag; reported only, never copied back).
- `allowlist.rs` — the per-project allowlist `.boxme/allow` (load/merge/save,
  entry matching). Entries and hosts compare lowercased.
- `review.rs` — the ratatui TUI (Files / Network / Outside tabs, inline diffs,
  host selection in learn and enforce runs, the allow-and-re-run confirmation).
  Returns a `Decision` (approve / abort / re-run) plus the chosen hosts.
- `report.rs` — the non-interactive (`--json`) surface: the serializable
  `Report` (with `finalize` deriving `findings`/`clean` and `exit_code_for`
  mapping them to the documented exit codes), the `.boxme/pending` lifecycle
  (`save_pending` writes a `*` .gitignore so the staged changeset can't be
  committed, keeps a `report.json` copy next to it), and the `boxme apply` /
  `boxme discard` subcommands. Findings/exit-code logic is unit-tested here.
- `copyback.rs` — applies the changeset in two phases so the host tree is never
  mutated while it's a live overlay lower: `stage` tars the approved paths out of
  the guest into a host-side tarball (VM alive, read-only), then — after teardown
  — `commit` extracts into a scratch dir inside the project and swaps it into
  place, so a corrupt tarball or full disk fails before anything is removed. It
  rejects absolute/`..` paths, validates symlink/hardlink *targets* too, and
  applies deletions only within the project dir. A failed apply names the
  surviving tarball. `Staged` is serde-serializable: `persist`/`load_staged`
  park it under `.boxme/pending` so a `--json` run and a later `boxme apply` can
  be separate processes.
- `composer_auth.rs` — the `--composer-auth` path: lift the host's global
  composer `auth.json` (`COMPOSER_HOME`/XDG/`~/.composer` precedence) into the
  guest as microsandbox **placeholder secrets**, never the raw values. `build`
  walks the parsed JSON, swaps every `http-basic` username *and* password /
  `github-oauth` / `gitlab-*` / `bearer` token for a `__BOXME_COMPOSER_SECRET_N__`
  placeholder, and emits a `SecretEntry` per credential (`allowed_hosts` =
  `*.host`, github also gets `*.githubusercontent.com`; token sections enable
  header + query `SecretSubstitution`, http-basic keeps the header-only default
  (header substitution also covers decoded `Authorization: Basic`);
  `require_tls_identity` on). `inject` pushes `COMPOSER_AUTH` (the placeholder
  JSON) and `NODE_EXTRA_CA_CERTS` (= `GUEST_TLS_CA_PATH`, so Node trusts the
  proxy CA — composer's PHP-curl and git already trust the system store the guest
  agent updates) onto the env, and returns the secrets. Registering any secret on
  the builder (`secret_entry`) auto-enables 443 TLS interception with the safe
  defaults (verify upstream, block QUIC). The guest only ever sees placeholders;
  the host-side TLS proxy splices the real value into the outgoing request only
  for a TLS-intercepted connection whose SNI matches the credential's host, and
  the default `BlockAndLog` violation action blocks any attempt to send a
  placeholder to a *different* host. So a credential can leave the box only
  toward the one host it already authenticates. `run.rs` gates on
  `--composer-auth && tool == composer`. Pure `build` logic is unit-tested.
- `skills.rs` — the `boxme skills` path: installs the bundled Claude Code
  skills (fleet-update, fleet-fix — bulk dependency updates / vulnerability
  fixes across a folder of repos via the `--json` flow) into
  `$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`. The SKILL.md files under
  `.claude/skills/` are the single source of truth, compiled in via
  `include_str!` — edit them there and the binary picks them up on the next
  build (a test asserts each frontmatter `name` matches its install dir). A
  skill dir that is already a symlink (a dev checkout wired up by hand) is left
  untouched.
- `dns.rs` — the host-side DNS relay every boot points microsandbox at
  (`dns::relay`, started once per process on a loopback port, UDP + TCP). It
  forwards each query unchanged to the nameservers in the host's
  `/etc/resolv.conf` and raises every answer TTL to at least 600 s. Why: a
  microsandbox domain rule only lets a connection through while the dialled IP
  is bound to the name by a DNS answer, for exactly that answer's TTL, but curl
  (composer) caches addresses 60 s and npm 5 min, and resolvers hand out
  counted-down TTLs (13–54 s observed) — so a long install dialled an IP the
  sandbox had forgotten and died with `curl error 35`/`7` mid-download. No
  usable resolver → no relay, the sandbox asks the host's resolvers itself.
  The TTL rewrite is unit-tested; a message that doesn't parse passes through
  untouched.
- `util.rs` — shell quoting/slugify, `raise_fd_limit`, `shell_capture`,
  `stream_shell_stderr` (streams guest exec output to the host terminal).

### Network policy & the observe/enforce model

A run is either **observe** (every outbound TCP succeeds and is recorded) or
**enforce** (deny-by-default; only DNS + registries + the allowlist reach the
network). UDP is always blocked apart from DNS — that closes the QUIC/raw-UDP
exfil path tcpdump's SYN capture can't see. The three policies are built in
`run.rs`: `observe_policy`, `strict_policy` (registries only, from
`netcap::STRICT_DOMAINS`), and `enforced_policy` (strict + allowlist entries).
`boot` sets microsandbox's own `strict` network flag to **false**: since 0.7.8
it defaults on and denies any TLS connection allowed by a domain rule unless
the TLS is intercepted, which killed every HTTPS request to a registry. Non-strict
keeps the check boxme relies on — the SNI must match the rule *and* the dialled
IP must be bound to that name by a DNS answer, so an SNI claimed on an
unresolved IP is still refused. That binding lasts only the answer's TTL, which
is why DNS goes through the `dns.rs` relay (TTL floor, see above).
The mere *existence* of `.boxme/allow` flips a normal run from observe to
enforce; `--strict` ignores the allowlist and permits only registries.
`--strict --learn` and `--json --learn` are rejected up front
(`conflicting_flags`).

`.boxme/allow` format: one host per line, a bare entry matches the domain and
all subdomains, a `=` prefix matches that exact host only, `#` comments and
blanks ignored, case-insensitive. Commit it to share the decision with a team.

### Security boundaries to preserve when editing

- The project is mounted **read-only** as the overlay lower (`boot` →
  `m.bind(project_dir).readonly()`), so the command physically cannot write to
  the host tree during the run — writes go to the guest-local upper, and the host
  is only ever touched by `copyback::commit` *after* teardown, on approval.
- The guest git baseline (`scripts::UNPACK`) disables hooks via
  `core.hooksPath=/dev/null` and `--no-verify` so committing the baseline can't
  run project code, and force-adds gitignored files so later modifications still
  diff (excluding `vendor`/`node_modules`, which the review prunes — indexing
  them would copy the whole dep tree up into the overlay for nothing).
- The file manifest is NUL-delimited with the path last, so a guest-controlled
  filename containing tabs/newlines can't forge a review entry.
- Copy-back treats the tarball as sandbox-controlled: it rejects absolute paths
  and `..` components (for link targets too) and never follows symlinks out of
  the project dir.
- The out-of-workspace scan uses `-newercm` (ctime, which the guest can't
  backdate with `touch -t`) against a marker stamped *after* setup but *before*
  the command, so it reports only what the command itself wrote.
- The DNS join in `netcap` only trusts answers captured inbound on the NIC
  from port 53, and a tcpdump that died mid-run raises a "treat as suspect"
  banner instead of a clean-looking contact list.
- The `dns.rs` relay may only *raise TTLs* of upstream answers. It must never
  answer a query itself or touch an address: microsandbox's IP↔name binding is
  what the domain rules rest on, and a longer TTL is something an allowed
  domain's own DNS could already send.
- `--composer-auth` must never put a raw credential in the guest. The real token
  only ever lives in the host-side TLS proxy (`SecretEntry.value`); the guest
  receives a placeholder. Keep `require_tls_identity` on and `allowed_hosts`
  scoped to the credential's own host so substitution happens only on a verified,
  host-matched TLS connection — and leave the violation action at its
  `BlockAndLog` default so a placeholder aimed at any other host is blocked.
  Don't widen `allowed_hosts` to `Any` or add a placeholder to a plaintext
  channel.

## Conventions

- Guest-side logic belongs in `scripts.rs`, never inlined into `run.rs`.
- Anything interpolated into a guest shell is validated host-side first (e.g. PHP
  versions against `PHP_VERSIONS`) or quoted via `util::shell_quote`.
- VMs are booted *attached* so a crash SIGTERMs them; teardown goes through
  `remove_vm` in `run.rs`, which drops the handle before `Sandbox::remove`
  (remove operates by name).
- The base snapshot is named `boxme-base` (`setup::BASE_SNAPSHOT`); the
  `boxme-node-versions` volume (Node majors installed by `n`) is ensured before
  boot in `ensure_cache_volumes` and mounted at `/root/.n`. The composer/npm
  *download* caches are intentionally **not** mounted — they stay guest-local.
  A mounted volume is virtiofs-backed, so each cache file the guest holds open is
  also held open by the host `msb` VMM process, which macOS caps at
  `kern.maxfilesperproc`; `npm` keeps tens of thousands of `_cacache` files open
  during a large reify and overruns that cap, surfacing as `EMFILE` inside the
  guest regardless of the guest's own `ulimit -n` (which `scripts::RAISE_FDS`
  already raises to ~1M). Guest-local caches move that ceiling back to the guest
  fd limit; the tradeoff is no cross-run download-cache reuse.
