# boxme

`composer install` and `npm install` run arbitrary package code with full
access to your machine. boxme runs them in a
[microsandbox](https://microsandbox.dev) microVM, shows you every file change
and network contact, and copies the result into your repo only when you
approve.

```sh
boxme composer install
boxme npm i some-package
```

Your project is mounted **read-only** with a throwaway writable layer on top,
and tcpdump in the guest records every DNS lookup and outbound TCP SYN. The
command runs fully interactively, then a review TUI shows three tabs: **Files**
(expected writes like `vendor/` summarized, everything else itemized with
diffs), **Network** (every destination, registry vs unexpected), **Outside**
(writes outside `/workspace` — reported, never copied back). `a` applies, `q`
aborts and nothing lands. The base image also blocks dependencies younger than
7 days (composer `innobrain/soak-time`, npm `min-release-age=7`).

## Install

Needs hardware virtualization: Apple Silicon macOS, or Linux with KVM.

```sh
curl -fsSL https://raw.githubusercontent.com/kauffinger/boxme/main/install.sh | sh
boxme setup    # one-time base snapshot, ~10 min; --disk 64 for very large repos
```

The installer verifies a checksum and installs to `~/.local/bin`
(`BOXME_INSTALL_DIR` overrides; append `-s -- v0.1.0` to pin a version). Or
build from source with `cargo install --path . --locked`. `boxme setup` also
downloads the pinned microsandbox runtime into `~/.microsandbox` — no separate
install needed.

## Usage

Global flags go **before** the command; everything after `composer`/`npm` is
passed through verbatim.

```sh
boxme composer require foo/bar
boxme npm ci
boxme --strict composer install     # registries only, allowlist ignored
boxme --learn composer install      # re-open the host picker
boxme --keep npm install            # keep the VM after the run
boxme --memory 4096 --cpus 4 composer update
boxme -e NPM_TOKEN=xyz npm install  # env into the guest — visible to package code
boxme -a composer install           # --composer-auth: credentials the guest can't read
```

Existing `vendor/` and `node_modules/` stay visible, so incremental commands do
incremental work. For a clean install, remove them on the host first or use
`npm ci`.

### Private composer repos (`--composer-auth`, `-a`)

`-e COMPOSER_AUTH` hands raw tokens to every postinstall script. `-a` reads
your global composer `auth.json` and injects each credential as a microsandbox
**secret**: the guest sees only a placeholder, and a host-side TLS proxy
splices the real token in — only on a verified connection to that credential's
own host. A placeholder aimed at any other host is blocked, so a leaked
credential can travel exactly one place: the host it already authenticates.

The host still has to be reachable under the network policy (`.boxme/allow`);
`github.com` is a built-in registry, so a `github-oauth` token works right
away. Also applies to `boxme claude` and `boxme dev`; npm runs ignore it.

## Non-interactive mode (agents, scripts, CI)

Without a terminal there is no TUI, so `--json` replaces it with a two-step
flow — same guarantee, nothing lands without explicit approval:

```sh
boxme --json composer install   # run + JSON report on stdout; changeset staged, NOT applied
boxme apply                     # copy the staged changeset into the project
boxme discard                   # or drop it
boxme allow some-host.com       # trust a blocked host without the TUI, then re-run
```

Step 1 always enforces (registries + `.boxme/allow`), streams command output to
stderr, and prints a report to stdout: changed files with diffs (expected vs
unexpected), network contacts (`registry`/`allowed`/`blocked`), outside writes,
guest exit code. The changeset is a plain gzipped tar under `.boxme/pending/`
(self-gitignored, report kept next to it); the report's `pending` object
carries the apply/discard commands so an agent needs no docs.

Exit codes: `0` clean (safe to `boxme apply`) · `1` boxme failed · `2` the
command failed (nothing staged) · `3` findings — blocked hosts, unexpected
files, outside writes — listed in `findings` for a script to branch on.

## Dev server

`boxme dev` runs your dev stack **inside** the sandbox and forwards its ports.
Dependencies install in the guest (Linux-native binaries — no host/Linux
`node_modules` mismatch), your edits sync one-way host→guest so Vite/HMR sees
them live, and nothing the guest writes ever comes back. Ctrl-C tears it down.

```sh
boxme dev                              # composer run dev, ports 8000 + 5173
boxme dev npm run dev
boxme dev -p 3000 -p 5173 npm run dev  # custom ports (HOST or HOST:GUEST)
```

Guest-owned paths aren't synced: `node_modules`, `vendor`, `.git`, `storage`,
`bootstrap/cache`, `public/build`, `public/hot`. Servers that bind only
loopback (artisan serve, Vite) are bridged automatically. One VM per folder;
several repos at once work, and a busy host port bumps to the next free one.
The network policy still applies — without an allowlist, egress is open but
recorded.

The database lives in the guest, so run migrations there — attach a second
shell from another terminal in the same folder:

```sh
boxme attach                       # interactive shell in /workspace
boxme attach php artisan migrate   # one command and exit
```

## Claude Code agent

`boxme claude` runs Claude Code inside the sandbox, then copies exactly what it
changed into your working tree as plain uncommitted edits — review with
`git diff`, commit or `git checkout .`. Works on a dirty tree, or a directory
that isn't a git repo at all.

```sh
boxme claude                        # interactive session (permission mode: auto)
boxme claude 'fix the failing test' # headless one-shot (checks bypassed — the sandbox is the boundary)
```

If the agent changed a file you'd **also** edited locally, boxme stops and
asks: overwrite, put the work on a `boxme/claude-<n>` branch, or abort. A
headless run branches automatically. The branch is built in a throwaway
worktree at `HEAD` — "HEAD + exactly what the agent did", your working tree
untouched.

### Authentication

There is no browser login inside the box (only Anthropic's API hosts are
reachable), so authenticate with a token:

```sh
claude setup-token   # OAuth on the host, prints a 1-year token
boxme login          # stored in your keychain (Linux: 0600 file), never a dotfile
```

Resolution order: `-e` flag → shell `CLAUDE_CODE_OAUTH_TOKEN` /
`ANTHROPIC_API_KEY` → the saved token; the token is injected into the guest
env only. With none, `boxme claude` fails before booting. Network enforcement
is the exfil mitigation: only `anthropic.com` / `claude.com` plus the package
registries are reachable, so a leaked token can't be sent anywhere else. (A
copied subscription login is not reused — its access token expires within
hours and the refresh flow doesn't work headless.)

`boxme --learn claude '…'` observes with open egress and saves contacted hosts
to `.boxme/claude-allow` — kept separate from `.boxme/allow` so the two
surfaces can't inherit each other's reachability. `--strict` drops the extras.

## Fleet skills: sweeping many repos

`boxme skills` installs two bundled Claude Code skills into `~/.claude/skills`:

- **fleet-update** — "update all repos in ~/Code": runs
  `composer update`/`npm update` via `boxme --json` in every repo, applies the
  clean results, reports the rest.
- **fleet-fix** — security-only, minimal churn: `composer fix`
  ([innobrain/composer-fix]) and `npm audit fix`, non-breaking fixes only.

Every install runs sandboxed; only clean changesets are applied automatically.

[innobrain/composer-fix]: https://packagist.org/packages/innobrain/composer-fix

## Kept VMs and housekeeping

`--keep` leaves the VM running after a run — useful for autopsy when something
failed. `boxme claude` also keeps the VM automatically if copying the result
out fails.

```sh
boxme --json --keep composer update   # failed run stays up
boxme ps                              # list boxme's VMs (--json for scripts)
boxme attach [--vm NAME]              # shell into this folder's running VM
boxme exec composer why-not php 8.4   # one command, split streams, exit code propagated
boxme kill boxme-app-3f2a             # stop + remove
boxme kill --all                      # sweep every boxme VM
```

`kill` only accepts boxme's own VM names, so it can't remove another tool's
sandbox on a typo.

## Network policy

A run either **observes** or **enforces**. UDP is always blocked except DNS —
that closes the QUIC/raw-UDP exfiltration path the SYN capture can't see.

- **No `.boxme/allow` yet → observe**: every TCP connection succeeds and is
  recorded; the review lets you trust hosts with `Space` (bare IPs with no
  resolved name can't be trusted — leave them blocked). If the run only
  contacted now-allowed hosts, it's copied back as-is; otherwise it re-runs
  clean under deny-by-default first.
- **`.boxme/allow` exists → enforce**: DNS + registries + the allowlist. A
  newly blocked host shows in the Network tab — mark it with `Space`, press
  `r`, and boxme appends it and re-runs clean under the updated policy.
  `--learn` re-opens the full picker; `--strict` permits registries only.

`.boxme/allow` is one entry per line — commit it to share with your team:

```
example.com         # the domain and every subdomain
=api.example.com    # this exact host only
# comments and blanks ignored
```

There is no mid-run "allow? [y/n]" (the policy is fixed at boot) and no
path-level rules (the URL path lives inside TLS — only the hostname is
visible).

### Review keys

`↑↓`/`jk` select · `g`/`G` first/last · `h`/`l`/`Tab` or `1`/`2`/`3` switch
tabs · `Ctrl-d/u` half-page · `Ctrl-f/b`/`PgUp/PgDn` full page · `J`/`K` line
scroll · `c` expand the truncated command · `Space` trust/mark host · `r`
allow marked + re-run · `a` approve · `q`/`Ctrl-C` abort. `Esc` is unbound so
a reflexive press can't abort a run.

## Version matching

- **PHP**: `php -v` from the project dir (mise/asdf/Herd shims resolve
  per-directory), then `composer.json` `require.php`, then 8.4. The image
  ships 8.3, 8.4 and 8.5.
- **Node**: `node -v` from the project dir, then `.nvmrc`, then
  `engines.node`. Majors other than 24 install via `n` on first use, cached on
  a named volume.

## Notes

- The guest gets a git baseline of your tree, including uncommitted changes.
  Your host repo needn't be a git repo and is never touched by guest git.
- composer/npm download caches are guest-local — no cross-run reuse, but also
  no shared cache a malicious package could poison. Only the Node-versions
  volume is shared across projects.
- Approval is all-or-nothing.
- A nonzero exit from the command still shows the review (red banner).
- `BOXME_DEBUG_NET=/path/file.txt` dumps the raw in-guest `tcpdump -r` text
  behind the Network tab.
