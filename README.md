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
aborts and nothing lands.

Updates read as package changes, not lockfile JSON: the `composer.lock` /
`package-lock.json` row opens with every version the run moved — counted by
kind, majors and downgrades in red — above the raw diff. Below 1.0 a minor bump
counts as major, the way `^` treats it (`^0.1` stops before 0.2). An excerpt
from a real `composer update` of a Laravel 12 app:

```
104 composer packages changed: 12 major, 66 minor, 23 patch, 3 added

composer/pcre                + 3.4.0     added
laravel/framework   v12.25.0 → v12.70.2  minor
laravel/mcp           v0.1.1 → v0.5.9    major
symfony/console       v7.3.2 → v7.4.21   minor
symfony/string        v7.3.2 → v8.1.9    major
```

After you approve, the same one-line summary stays in your terminal. The base image also blocks dependencies younger than
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
install needed. To upgrade, re-run the one-liner, then `boxme setup` once: it
brings the runtime up to the new boxme's version without rebuilding the snapshot.

## Usage

Global flags go **before** the command; everything after `composer`/`npm`/`php`
is passed through verbatim. boxme warns when one of its own flags lands after the
command — `boxme npm i --strict` hands `--strict` to npm and runs unstrict.

```sh
boxme composer require foo/bar
boxme npm ci
boxme --strict composer install     # registries only, allowlist ignored
boxme --learn composer install      # re-open the host picker
boxme --memory 4096 --cpus 4 composer update
boxme --php 8.3 composer update     # resolve for the PHP the app is deployed on
boxme -e NPM_TOKEN=xyz npm install  # env into the guest — visible to package code
boxme -a composer install           # --composer-auth: credentials the guest can't read
boxme composer install ++ composer run-script post-update-cmd
                                    # chain with ++: one sandbox, one combined changeset
boxme composer install ++ php artisan test
                                    # install, then run the tests on what was installed
```

A `++` chain runs the commands in order in the same guest and stops at the
first failure — so a script hook runs with the install's `vendor/` already in
place, and everything lands (or is staged, under `--json`) as one changeset.
`php` commands are there to test what an update installed without running it on
your machine: nothing they write is expected, so a test run's logs and caches
show up in the review like any other change.

Existing `vendor/` and `node_modules/` stay visible, so incremental commands do
incremental work. For a clean install, remove them on the host first or use
`npm ci`.

boxme exits with the wrapped command's own exit code, so
`boxme composer install && php artisan migrate` stops when composer failed
inside the box. Aborting the review exits `1`.

### Private composer repos (`--composer-auth`, `-a`)

`-e COMPOSER_AUTH` hands raw tokens to every postinstall script. `-a` reads
your global composer `auth.json` and injects each credential as a microsandbox
**secret**: the guest sees only a placeholder, and a host-side TLS proxy
splices the real token in — only on a verified connection to that credential's
own host. A placeholder aimed at any other host is blocked, so a leaked
credential can travel exactly one place: the host it already authenticates.

The host still has to be reachable under the network policy (`.boxme/allow`);
`github.com` is a built-in registry, so a `github-oauth` token works right
away. npm runs ignore the flag.

## Non-interactive mode (agents, scripts, CI)

Without a terminal there is no TUI, so `--json` replaces it with a two-step
flow — same guarantee, nothing lands without explicit approval:

```sh
boxme --json composer install   # run + JSON report on stdout; changeset staged, NOT applied
boxme apply                     # copy the staged changeset into the project
boxme apply composer.lock public  # or only these paths; the rest is dropped
boxme discard                   # or drop it
boxme allow some-host.com       # trust a blocked host without the TUI, then re-run
```

Step 1 always enforces (registries + `.boxme/allow`), streams command output to
stderr, and prints a report to stdout: changed files with diffs (expected vs
unexpected), the package versions that changed (`packages`: name, from, to,
and `major`/`minor`/`patch`/`downgrade`/`changed`/`added`/`removed`), network
contacts (`registry`/`allowed`/`blocked`), outside writes, guest exit code.
What an update legitimately regenerates — the lockfiles,
`composer-integrity.lock` (soak-time), Laravel's
`bootstrap/cache/{packages,services}.php` — counts as expected, so a clean
`composer update` exits `0`. The changeset is a plain gzipped tar under `.boxme/pending/`
(self-gitignored, report kept next to it); the report's `pending` object
carries the apply/discard commands so an agent needs no docs.

Exit codes: `0` clean (safe to `boxme apply`) · `1` boxme failed · `2` the
command failed (nothing staged) · `3` findings — blocked hosts, unexpected
files, outside writes — listed in `findings` for a script to branch on.

## Fleet skills: sweeping many repos

`boxme skills` installs two bundled Claude Code skills into `~/.claude/skills`:

- **fleet-update** — "update all repos in ~/Code": runs
  `composer update`/`npm update` via `boxme --json` in every repo, applies the
  clean results, reports the rest.
- **fleet-fix** — security-only, minimal churn: `composer fix`
  ([innobrain/composer-fix]) and `npm audit fix`, non-breaking fixes only.

Every install runs sandboxed; only clean changesets are applied automatically.

[innobrain/composer-fix]: https://packagist.org/packages/innobrain/composer-fix

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

The sandbox's DNS goes through a small relay in boxme that forwards to your
resolvers (`/etc/resolv.conf`) and raises short TTLs to 10 minutes. An allowed
host is reachable only at an IP its DNS answer named, and only while that
answer is valid — composer and npm cache addresses longer than many resolvers
keep them, so without the relay a long install could lose its connection
mid-download.

### Review keys

`↑↓`/`jk` select · `g`/`G` first/last · `h`/`l`/`Tab` or `1`/`2`/`3` switch
tabs · `Ctrl-d/u` half-page · `Ctrl-f/b`/`PgUp/PgDn` full page · `J`/`K` line
scroll · `c` expand the truncated command · `Space` trust/mark host · `r`
allow marked + re-run · `a` approve · `q`/`Ctrl-C` abort. `Esc` is unbound so
a reflexive press can't abort a run.

## Version matching

- **PHP**: `--php X.Y` if given, else `php -v` from the project dir
  (mise/asdf/Herd shims resolve per-directory), then `composer.json`
  `require.php`, then 8.4. The image ships 8.3, 8.4 and 8.5. Composer resolves
  updates for the guest's PHP, so pass `--php` with the deploy target when this
  machine runs a different one.
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
- Composer refuses versions with known advisories, and soak-time holds back
  releases younger than 7 days (security fixes: 72 h). While a dependency's fix
  is that fresh, `composer update` can't resolve at all;
  `boxme composer update --no-security-blocking` updates everything else in
  the meantime.
- A failed command still opens the review (red banner) when it left something
  to look at: changed files, a partial `vendor/`, writes outside the workspace,
  a blocked host. If it changed nothing, boxme skips the review, leaves the
  error on screen and exits with the command's code.
- `BOXME_DEBUG_NET=/path/file.txt` dumps the raw in-guest `tcpdump -r` text
  behind the Network tab.
