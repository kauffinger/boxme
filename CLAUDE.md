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
boxme setup                 # install the libkrun runtime + build the boxme-base snapshot once (~10 min); required before any run
boxme setup --force         # rebuild the snapshot (needed after changing BASE_SETUP or --disk; required to bake in `boxme claude`)
boxme setup --disk 64       # build the snapshot with a 64 GiB writable overlay (default 32)

boxme claude                # run Claude Code in the sandbox interactively, copy the result into your working tree
boxme claude 'fix the bug'  # one-shot headless agent run; needs a stored token (boxme login) or an env credential
boxme login                 # save the `claude setup-token` token to the keychain so `boxme claude` can auth
boxme logout                # remove the stored token

boxme --json composer i     # non-interactive: JSON report on stdout, changeset staged to .boxme/pending (not applied)
boxme apply                 # copy the staged changeset into the project (second step of the --json flow)
boxme discard               # drop the staged changeset instead
boxme allow foo.com         # add allowlist host(s) without the TUI (=exact.host for exact-only)
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
so the `time` ceiling in `Cargo.lock` is honored. `install.sh` resolves the
latest tag from the releases redirect (no API rate limit), downloads the matching
tarball, and verifies the checksum before installing to `~/.local/bin`. The
workflow can also be run manually (`workflow_dispatch`) with a tag input.

`.github/workflows/ci.yml` gates pushes/PRs on `cargo fmt --check`, `clippy -D
warnings`, and `cargo test` (one Linux runner) so a broken tree never gets
tagged. Both workflows `apt-get install cmake pkg-config libdbus-1-dev
libcap-ng-dev` — the native build deps microsandbox pulls in (aws-lc-sys,
libdbus-sys, and libcap-ng, which msb_krun links for Linux capability handling).
msb_krun is otherwise pure Rust, so there is no libkrun system package.

Tests are pure unit tests in `#[cfg(test)] mod tests` blocks (in `netcap.rs`,
`allowlist.rs`, `manifest.rs`, `outside.rs`, `review.rs`, `report.rs`,
`copyback.rs`). There is no
integration-test harness — anything touching the sandbox is exercised by running
`boxme` against a real project. There is no rustfmt/clippy config and no
toolchain pin; default stable applies.

Note the deliberate version ceiling on `time` in `Cargo.toml`: `>=0.3.6,
<0.3.47` works around an rcgen 0.14 coherence conflict pulled in transitively by
`microsandbox-network`. Don't bump it until rcgen ships a fix.

## Architecture

Entry point `main.rs` parses the CLI and dispatches to `setup::setup`,
`dev::dev`, `dev::attach`, `claude::claude`, `auth::login`, `auth::logout`,
`report::apply`, `report::discard`, `run::allow_hosts`, or `run::run`. `cli.rs`
uses clap derive; `setup`, `dev`, `attach`, `claude`, `login`, `logout`,
`apply`, `discard` and `allow` are named subcommands, and everything else falls
into an `#[command(external_subcommand)] Run(Vec<String>)`, which is why
`boxme composer i` works — global flags (`--strict`, `--learn`, `--keep`,
`--composer-auth`, `--json`, `--memory`, `--cpus`, `-e`) must come *before* the
package-manager command (and before `claude`). `run::run` returns an exit code
(`main` propagates it) so the `--json` report can drive scripts.

`run.rs` is the orchestrator and the file to read first. It validates the tool
is `composer`/`npm`, detects versions, then chooses one of three paths (the TUI
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
manifest → scan for out-of-workspace writes → build the review rows.

`claude.rs` is a separate path (`boxme claude [PROMPT]`) that reuses the review
run's integrity core for an AI agent instead of a package manager. It boots the
same read-only overlay and runs Claude Code *inside* the box (`scripts::claude_run`),
interactive when no prompt and headless (`-p`) when given. Permission mode is
chosen per path: **interactive** uses `--permission-mode auto` (the session is
attached to the user's TTY, so auto mode's classifier is a safety net they can
answer, and it needs no root-bypass and shows no warning screen); **headless**
uses `--dangerously-skip-permissions` (unattended, where auto mode would *abort*
the session on a classifier block — `IS_SANDBOX=1` lets bypass run as the guest
root, and the sandbox is already the safety boundary). Before launching, the
script seeds `/root/.claude.json` (`scripts::CLAUDE_SEED`) to mark onboarding
complete and `/workspace` trusted — a valid token authenticates but doesn't
suppress the first-run wizard or the per-project trust dialog on a fresh config.
It then diffs the manifest exactly like `run_command` — but with **no write-set
partition**: the whole diff is the agent's changeset. There is no review TUI;
instead the changeset is staged out (`copyback::stage`) and, after teardown,
copied out by `claude::deliver`. **The default is an in-place apply** to the
working tree (`copyback::commit`) — the agent's work shows up as plain
uncommitted edits the user reviews with `git diff`, no clean repo required (it
even works in a non-git dir). The branch is a *fallback*, not the default: if any
changed path is one the user has **also edited locally** (`copyback::collisions`,
a `git status --porcelain` pathspec query — applying would clobber their work),
an interactive run asks (`ask_collision`: overwrite / branch / abort) and a
headless run, which can't prompt, lands the work on a fresh `boxme/claude-<epoch>`
branch instead. The branch path (`copyback::commit_to_branch`) builds the commit
in a throwaway git worktree at HEAD, so it never reads or writes the user's
working tree — their in-progress edits survive verbatim and the branch is a clean
"HEAD + exactly the agent's diff". Nothing reaches the host until after teardown.
The credential is resolved in precedence order
(`resolve_claude_env`): an explicit `-e` flag, then the host shell env, then the
token saved by `boxme login` (`auth::load`) — so the normal path keeps the token
out of your shell. There is no in-box browser login (the OAuth authorize endpoints
aren't reachable under enforcement), so a run with no credential bails *before*
booting. Network enforcement (only Anthropic's services — `anthropic.com` /
`claude.com` — reachable over TCP) is the exfil mitigation for putting a credential
in the box. A live run has confirmed keychain auth + boot + PHP switch; **still
unvalidated against a live VM:** that `CLAUDE_SEED` fully suppresses the first-run
wizard (the auto-mode opt-in keys `hasResetAutoModeOptInForDefaultOffer` /
`autoPermissionsNotificationCount` are best-effort — capture the real values from a
session if the opt-in still shows), and that interactive `--permission-mode auto` /
headless bypass behave as intended on the `attach_with` TTY.

The project is **bind-mounted read-only** at `/ws-lower` (`boot`) and overlaid at
`/workspace` with a guest-local writable upper, so nothing is copied in: reads
fall through to the host tree via virtiofs, the command's writes land in the
upper, and the host stays untouchable for the whole run (virtiofs rejects writes
host-side, the guest kernel returns `EROFS`). The upper can't live on the guest
root — that root is itself an overlayfs, which the kernel refuses as an upperdir
(`not supported as upperdir`) — so `unpack` puts it on a sparse loop-mounted ext4
sized to the guest root's free space (disk-backed, no RAM pressure unlike tmpfs).
Because the host project is now the *live overlay lower*, copy-back is split in
two (`copyback::stage` then `copyback::commit`): stage tars the changeset out
while the VM is alive, the VM is torn down, then commit mutates the host tree —
never while it's mounted. The whole project (including `.git`, `vendor`,
`node_modules`) is always mounted; there is no opt-out, since hiding a dir from
`/workspace` wouldn't hide it from in-guest code anyway — the full host tree is
bind-mounted read-only at `/ws-lower`, readable regardless of any whiteout.
Existing `vendor`/`node_modules` stay in place, so incremental commands do
incremental work (`npm ci` clears `node_modules` itself for a clean install).

`dev.rs` is a separate path (`boxme dev [--port …] [command]`, default command
`composer run dev`) for running a long-lived dev-server stack *inside* the guest
instead of on the host — so native binaries never have to leave Linux. It boots a
writable guest with the requested ports published, copies the project in (lighter
`DEV_UNPACK`, no git baseline), switches versions, runs `composer install` /
`npm install` *in the guest* (Linux-native, never retargeted or copied back),
then runs the command attached while a host-side `notify` watcher does **one-way
host→guest file sync**: every edit is pushed into the guest's real ext4 via the
agent fs API (`sb.fs().copy_from_host`/`mkdir`/`remove`/`rename`/`symlink`), which
fires native guest inotify so Vite/Laravel HMR works. The sync only ever flows
host→guest — nothing the guest writes is copied back — so the integrity guarantee
holds without any writable bind mount or `.boxme/write` config. The watcher and
the attached command run concurrently via `tokio::select!` (both borrow `&sb`);
when the dev server exits, the sync future is dropped and the VM torn down. The
`is_excluded` list (node_modules, vendor, .git, storage, bootstrap/cache,
public/build, public/hot, .boxme) is the sync-side inverse of a writable-path
config: the guest owns those, so the host never pushes them. Ports are bridged
guest-side (`scripts::port_bridge`, a tiny Node proxy on the guest's eth0 IP →
`127.0.0.1`) so servers that bind only loopback — artisan serve, Vite by default —
are still reachable through microsandbox's host→guest forward, which dials the
guest IP. **Unvalidated against a live VM:** per-edit sync latency under real
editing, that agentd writes fire guest inotify, and the port-bridge binding
assumption; verify these on a real `boxme dev` run.

### Module responsibilities

- `setup.rs` — provisions the libkrun runtime via the SDK
  (`microsandbox::setup::install`, pinned to the linked crate's
  `PREBUILT_VERSION`, into `~/.microsandbox/{bin,lib}`), then builds the
  `boxme-base` snapshot from `node:24` (one-time, slow). The runtime install is
  idempotent — a no-op once the matching version is present — so no separate
  microsandbox CLI install is required. The builder sets the writable overlay
  (`oci_upper_size`, `--disk` GiB, default 32) — this is the *only* place the
  per-run disk ceiling can be set, because booting `from_snapshot` inherits the
  snapshot's fixed-size `upper.ext4` and the SDK forbids resizing it at boot. The
  default microsandbox overlay is just 4 GiB, which a large repo + vendor +
  node_modules + guest-local caches overruns (ENOSPC); ext4 is sparse, so a big
  overlay costs almost nothing until used.
- `detect.rs` — resolves guest PHP `X.Y` and Node major: host binary run from the
  project dir first (so mise/asdf/herd shims resolve per-directory), then
  manifest constraints (`composer.json` require.php, `.nvmrc`, `engines.node`),
  then defaults. PHP is clamped to the 8.3–8.5 baked into the image.
- `scripts.rs` — **every shell snippet that runs inside the guest lives here** as
  a `const` or a `format!`-returning fn. The base-image setup (which now also
  bakes `@anthropic-ai/claude-code`), the unpack/baseline script, version
  switching, the `claude_run` launcher (per-path permission mode — auto for
  interactive, bypass for headless — plus the `CLAUDE_SEED` onboarding/trust
  pre-seed), the file manifest, the out-of-workspace scan, and the
  tcpdump start/parse commands are all here. Change guest behavior here, not
  inline.
- `netcap.rs` — tcpdump lifecycle plus a lenient token-scanner that parses
  `tcpdump -r` text output, joins DNS answers to SYN destinations, and classifies
  each contact as a known registry vs unexpected. Also holds the built-in domain
  sets: `STRICT_DOMAINS` (registries) and `CLAUDE_DOMAINS` (Anthropic's services —
  `anthropic.com` for the API, `claude.com` for the `platform.claude.com` startup
  check).
  Network capture runs *inside the guest* because microsandbox 0.5 exposes no
  host-side per-connection observability.
- `manifest.rs` — computes the expected write-set per command, parses the guest
  file manifest, and diffs before/after into added/modified/deleted.
- `outside.rs` — parses the scan for anything written outside `/workspace` (a
  supply-chain red flag; reported only, never copied back).
- `allowlist.rs` — the per-project allowlist files (load/merge/save, entry
  matching). A `Scope` selects the file: `Scope::Packages` → `.boxme/allow` (the
  composer/npm surface), `Scope::Claude` → `.boxme/claude-allow` (the agent
  surface). Kept separate so a package run can't inherit reachability to the
  agent's hosts and vice versa.
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
  — `commit` unpacks them into the project (via the shared `apply_staged`),
  rejecting absolute/`..` paths and applying deletions only within the project
  dir. For `boxme claude`, `collisions` reports which changed paths the user has
  *also* edited locally (a `git status --porcelain -z` pathspec query — empty if
  clean or non-git), and `commit_to_branch` is the branch *fallback*: it builds
  the agent's commit inside a throwaway git **worktree** checked out at HEAD (temp
  dir, removed after), so the user's working tree and index are never touched and
  the branch is a clean "HEAD + exactly the agent's diff". `unique_branch` bumps a
  `-N` suffix if the name is taken (no abort on a same-second re-run). Hooks are
  skipped (`--no-verify`) since the content is sandbox-produced. `Staged::tarball`
  exposes the staged path so an aborted copy-out points the user at their
  not-yet-applied changes instead of dropping them. (There is no longer a
  clean-repo preflight — the default copy-out is in place, branch is the fallback.)
  `Staged` is serde-serializable: `persist`/`load_staged` park it under
  `.boxme/pending` so a `--json` run and a later `boxme apply` can be separate
  processes.
- `dev.rs` — the `boxme dev` path: boot a writable guest with ports forwarded,
  copy in (this path still tars in via `tar_directory`, *not* the overlay — it
  installs deps in-guest and needs a writable tree), install deps in-guest, run
  the dev stack attached, and one-way
  host→guest file sync via a `notify` watcher + the agent fs API. Reuses
  `run.rs`'s `pub(crate)` helpers (`resolve_env`, `ensure_cache_volumes`,
  `cleanup`, `quote_args`, the policy builders). Never copies back. Also hosts
  `boxme attach`: the dev VM is named deterministically per folder
  (`dev_vm_name` = `boxme-dev-<slug>-<fnv-of-abs-path>`, *not* the random-nonce
  `run::vm_name`), so `attach` recomputes the name, `Sandbox::get(name).connect()`s
  to the live VM, and opens a second TTY (`attach_with`) without owning teardown.
  One dev VM per folder is enforced: `dev` refuses to boot if one is already
  `Running`. Host ports are preflighted (`resolve_free_ports`) by probing
  `127.0.0.1` — a busy port bumps to the next free one (guest side untouched), so
  parallel dev sessions across repos don't collide on 8000/5173.
- `claude.rs` — the `boxme claude [PROMPT]` path: run Claude Code inside the
  read-only overlay (interactive under `--permission-mode auto`, headless under
  bypass), then copy its net changeset back out (`deliver`). Reuses `run.rs`'s
  `pub(crate)` helpers (`resolve_env`, `ensure_cache_volumes`, `cleanup`,
  `vm_name`, `claude_policy`, `observe_policy`) and a throwaway VM (`run::vm_name`'s
  random nonce, like the review run — *not* the deterministic `dev_vm_name`). No
  review TUI: it diffs the manifest (whole diff, no write-set partition),
  `copyback::stage`s it, tears the VM down, then `deliver`s. **Default delivery is
  an in-place `copyback::commit`** (uncommitted edits the user reviews with `git
  diff`); the `boxme/claude-<epoch>` branch is the fallback `deliver` falls to only
  when `copyback::collisions` flags a changed path the user has also edited —
  interactive then prompts (`ask_collision`), headless auto-branches. Auth is
  resolved by `resolve_claude_env` — `-e` flag, then host shell env, then the
  `auth`-stored token — and the run bails before booting if none is found.
  `--learn` observes (open egress) and writes the contacted hosts to
  `.boxme/claude-allow`; otherwise it always enforces.
- `composer_auth.rs` — the `--composer-auth` path: lift the host's global
  composer `auth.json` (`COMPOSER_HOME`/XDG/`~/.composer` precedence) into the
  guest as microsandbox **placeholder secrets**, never the raw values. `build`
  walks the parsed JSON, swaps every `http-basic` password / `github-oauth` /
  `gitlab-*` / `bearer` token for a `__BOXME_COMPOSER_SECRET_N__` placeholder, and
  emits a `SecretEntry` per credential (`allowed_hosts` = `*.host`, github also
  gets `*.githubusercontent.com`; token sections enable header/basic/query
  injection, http-basic keeps the basic+header default; `require_tls_identity`
  on). `inject` is the shared entry point each path calls under its own gate:
  it pushes `COMPOSER_AUTH` (the placeholder JSON) and `NODE_EXTRA_CA_CERTS`
  (= `GUEST_TLS_CA_PATH`, so Node trusts the proxy CA — composer's PHP-curl and
  git already trust the system store the guest agent updates) onto the env, and
  returns the secrets. Registering any secret on the builder (`secret_entry`)
  auto-enables 443 TLS interception with the safe defaults (verify upstream,
  block QUIC). The guest only ever sees placeholders; the host-side TLS proxy
  splices the real value into the outgoing request only for a TLS-intercepted
  connection whose SNI matches the credential's host, and the default
  `BlockAndLog` violation action blocks any attempt to send a placeholder to a
  *different* host. So a credential can leave the box only toward the one host it
  already authenticates. `run.rs` gates on `--composer-auth && tool == composer`;
  `claude.rs` on `--composer-auth` (and bypasses the Anthropic/Claude domains
  from interception so the agent's own API traffic is untouched); `dev.rs` on
  `--composer-auth && has_composer`. The credential never enters the guest, so it
  is never copied back. Pure `build` logic is unit-tested.
- `auth.rs` — stores the Claude Code OAuth token (`claude setup-token`) that
  `boxme claude` injects as `CLAUDE_CODE_OAUTH_TOKEN`, so the credential lives in
  the macOS Keychain (`security` generic-password, service `boxme-claude-oauth`)
  or a `0600` file under `~/.config/boxme` on Linux — never in a shell rc. `login`
  prompts for the token (echo off via `stty`, piped input works for scripting),
  `logout` removes it, `load` is the read path `resolve_claude_env` falls back to.
  The token is read only at boot and injected into the *guest* env, never the host
  shell.
- `util.rs` — `tar_directory` (packs a project tarball for the **`dev`** path
  only — the review run mounts read-only instead; always skips the top-level
  `vendor/`/`node_modules/`, which `dev` reinstalls Linux-native in-guest and
  never copies back), shell quoting/slugify, `shell_capture`,
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

`boxme claude` adds a fourth policy, `claude_policy` (strict baseline + Anthropic's
services from `netcap::CLAUDE_DOMAINS` + the `.boxme/claude-allow` entries).
The agent legitimately runs composer/npm itself, so it keeps the registry
baseline. Unlike the package path there is no observe-by-default: claude always
enforces, and `boxme claude --learn` swaps in `observe_policy` to discover hosts;
`--strict` drops the `claude-allow` extras (API + registries only).

Both `.boxme/allow` and `.boxme/claude-allow` share the format: one host per line,
a bare entry matches the domain and all subdomains, a `=` prefix matches that
exact host only, `#` comments and blanks ignored. Commit them to share the
decision with a team.

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
  and `..` components and never follows symlinks out of the project dir.
- The out-of-workspace scan uses `-newercm` (ctime, which the guest can't
  backdate with `touch -t`) against a marker stamped *after* setup but *before*
  the command, so it reports only what the command itself wrote.
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
  `cleanup`/`discard`/`remove_vm` in `run.rs`, which drop the handle before
  `Sandbox::remove` (remove operates by name). `--keep` detaches instead.
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
