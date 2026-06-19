# boxme

Sandboxed package-manager runner. `composer install` and `npm install` execute
arbitrary package code (postinstall scripts, composer plugins) with full access
to your machine — boxme runs them inside a [microsandbox](https://microsandbox.dev)
microVM instead, then shows you exactly what they did before anything touches
your repo.

```sh
boxme composer install
boxme npm i some-package
```

What happens:

1. A microVM boots from the `boxme-base` snapshot (PHP/Node versions matched to
   your host, your project mounted **read-only** with a throwaway writable layer
   on top, composer/npm caches on persistent volumes).
2. The command runs fully interactively — prompts and progress bars work.
3. Inside the guest, tcpdump records every DNS lookup and outbound TCP SYN.
4. A full-screen review shows:
   - **Files**: the expected write-set (`vendor/`, lockfiles) summarized,
     anything *outside* it itemized with inline diffs;
   - **Network**: every destination contacted, classified known registry vs
     unexpected;
   - **Outside**: anything the command wrote *outside* `/workspace` (a binary in
     `/usr/local/bin`, a key in `/root/.ssh`, ...) — a supply-chain red flag.
     These are reported only, never copied back.
5. Only on `a` (approve) is the result copied back into your repo. `q` aborts
   and nothing lands.

Supply-chain guards baked into the base image: the composer
`innobrain/soak-time` plugin and npm `min-release-age=7` block dependencies
younger than 7 days.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/kauffinger/boxme/main/install.sh | sh
```

This downloads the prebuilt binary for your platform (Apple Silicon macOS, Linux
x86_64, Linux arm64) into `~/.local/bin` and verifies its checksum. Override the
location with `BOXME_INSTALL_DIR`, or pin a version by passing the tag:

```sh
curl -fsSL https://raw.githubusercontent.com/kauffinger/boxme/main/install.sh | sh -s -- v0.1.0
```

Or build from source (needs a Rust toolchain):

```sh
cargo install --path . --locked
```

Then build the base snapshot once (~10 min):

```sh
boxme setup            # 32 GiB writable guest disk by default
boxme setup --disk 64  # bump it for very large repos (sparse, ~free until used)
```

`boxme setup` also downloads the microsandbox runtime (the `msb` binary and
`libkrunfw`) into `~/.microsandbox` on first run, so you don't need a separate
microsandbox CLI install — the runtime version is pinned to the SDK boxme links.

boxme needs hardware virtualization to boot the microVM — an Apple Silicon Mac,
or Linux with KVM.

## Usage

```sh
boxme composer install
boxme composer require foo/bar
boxme npm ci

# Global flags go BEFORE the command (everything after `composer`/`npm`
# is passed through verbatim):
boxme --strict composer install   # deny-by-default network: registries only
boxme --learn composer install    # re-open the host picker to re-curate
boxme --keep npm install          # keep the VM around afterwards
boxme --memory 4096 --cpus 4 composer update

# The whole project is mounted read-only and the existing vendor/ and
# node_modules/ stay visible, so an incremental command only does incremental
# work (for a clean install, remove them on the host first, or use `npm ci`):
boxme composer require vendor/package    # reuses the existing vendor/
boxme npm install some-package           # reuses the existing node_modules/

# Pass environment variables into the guest (private registries, auth):
boxme -e COMPOSER_AUTH composer install      # copy host value
boxme -e NPM_TOKEN=xyz npm install           # set explicitly
```

Anything you pass with `-e` is visible to the package code running in the
sandbox — a malicious postinstall could read it and try to send it somewhere.
The Network tab shows every destination contacted; `--strict` limits where
anything can go.

## Dev server

`boxme dev` runs your whole dev stack *inside* the sandbox and forwards its ports
back to your machine. The default command is `composer run dev` (Laravel's
concurrently-driven `artisan serve` + queue + Vite), but anything works:

```sh
boxme dev                         # composer run dev, ports 8000 + 5173
boxme dev npm run dev             # just Vite
boxme dev -p 3000 -p 5173 npm run dev   # custom ports (HOST or HOST:GUEST)
```

### Attaching a second shell

While a `boxme dev` session is running, open another terminal in the same folder
and drop into the live VM — for migrations, tinker, a one-off build, or just
poking around:

```sh
boxme attach                       # interactive shell in /workspace
boxme attach php artisan migrate   # run one command and exit
boxme attach php artisan tinker
```

`attach` finds the session by the folder (one dev VM per folder) and connects as
a second shell alongside the running stack — it never tears the VM down. The
database lives in the guest, so anything stateful (creating the sqlite file,
running migrations) happens here, in the attached shell. Equivalently, chain it
into the dev command itself:

```sh
boxme dev bash -lc 'php artisan migrate --force && composer run dev'
```

What it does: boots a writable guest, copies your project in, runs `composer
install` / `npm install` **in the guest** (so dependencies get their Linux-native
binaries — no platform mismatch), then runs your command and watches your project
for edits. Every save is synced **one-way, host → guest**, so the dev server and
Vite HMR see your changes live — but nothing the guest writes is ever copied back.
Your machine stays read-only to the sandbox; the integrity guarantee holds.

Because the install happens in the guest, this is also the cleanest way to develop
a project whose native modules (esbuild, sharp, …) differ between your host and
Linux — you never have to reconcile the two `node_modules`. Stop the dev server
(Ctrl-C) and the sandbox is torn down.

Files the guest owns aren't synced from the host: `node_modules`, `vendor`,
`.git`, `storage`, `bootstrap/cache`, `public/build`, `public/hot`. A dev server
often needs to reach a database or external API, so the network policy still
applies — run `boxme <pm> install --learn` first if you want deny-by-default
enforcement during the session (otherwise egress is open but recorded).

> Servers that bind only `127.0.0.1` inside the guest (artisan serve, Vite by
> default) are bridged onto the guest's interface automatically, so the forwarded
> host ports reach them without any config.

Running `boxme dev` in several repos at once works — each gets its own VM, named
per folder. If a default host port (8000/5173) is already taken by another
session, boxme bumps it to the next free one and prints what it chose
(`host port 8000 busy → using 8001`); the app still serves its normal port inside
its own VM.

## Deciding what the network can reach

A run is one of two things: **observe** or **enforce**. UDP is always blocked
apart from DNS; the difference is what TCP can reach.

**First run in a project** (no `.boxme/allow` yet) observes: every outbound TCP
connection succeeds and is recorded, and the review's Network tab lets you trust
hosts. Known registries are always allowed and shown for reference; each
*unexpected* named host gets a checkbox (`Space` to trust); a bare-IP contact
with no resolved name can't be allowlisted — and is itself worth leaving
blocked. On approve, your picks are saved to `.boxme/allow` and:

- if the command only ever contacted hosts that are now allowed, the observe run
  *is* the clean result and it's copied back as-is — no second run;
- if it touched anything that enforcement would block, the command **re-runs**
  under deny-by-default (DNS + registries + your allowlist) and *that* clean
  result is what you review and copy back.

**Every later run** in that project enforces `.boxme/allow` automatically. When a
new dependency contacts a host the allowlist doesn't cover, that host shows up
**blocked** in the review's Network tab — mark it with `Space`, press `r`, and
confirm: boxme appends it to `.boxme/allow` and re-runs the command clean under
the updated policy, so the result you review actually had the host available. You
can still pass `--learn` to re-open the full host picker, or edit the file by
hand. `--strict` ignores the allowlist and permits only the registries (the
tightest setting) — there blocked hosts are shown for reference but can't be
allowed inline.

`.boxme/allow` is one entry per line — commit it to share the decision with your
team:

```
example.com         # the domain and every subdomain
=api.example.com    # this exact host only
# comments and blank lines are ignored
```

Real-time "allow this connection? [y/n]" prompting mid-run isn't offered: the
sandbox's network policy is fixed when the VM boots, so trust is decided in the
review (and applied on a clean re-run), not during a run. Path-level rules (e.g. `github.com/org/*`)
aren't possible either — the URL path lives inside TLS, so the policy only ever
sees the hostname.

### Review keys

`↑↓`/`jk` select · `g`/`G` first/last · `h`/`l`/`Tab` switch Files/Network/Outside
(`1`/`2`/`3` jump directly) · `Ctrl-d`/`Ctrl-u` half-page scroll ·
`Ctrl-f`/`Ctrl-b`/`PgUp`/`PgDn` full-page scroll · `J`/`K` line scroll ·
`c` expand a truncated command · `Space` trust host (observe run) /
mark a blocked host (enforce run) · `r` allow marked hosts + re-run (enforce run) ·
`a` approve · `q`/`Ctrl-C` abort

A long command line is truncated with `…` in the header so the tabs stay visible;
`c` toggles the full command, wrapped below the tabs. Scroll keys act on the diff
on the Files tab and on the list on the Network and Outside tabs. `Esc` is
deliberately unbound so a reflexive press can't abort a run.

## How versions are matched

- **PHP**: `php -v` run from the project dir (so mise/asdf/Herd shims resolve
  per-directory), falling back to `composer.json` `require.php`, then 8.4.
  The base image ships 8.3, 8.4 and 8.5 side by side.
- **Node**: `node -v` from the project dir, then `.nvmrc`, then
  `package.json` `engines.node`. Majors other than 24 are installed via `n`
  on first use and cached on a named volume.

## Notes

- The guest gets a git baseline commit of your tree (including uncommitted
  changes — that's exactly the state the command should operate on). Your host
  repo is never required to be a git repo and is never touched by guest git.
- Observe vs enforce is covered above; in both, UDP is blocked apart from DNS
  (composer and npm need nothing else over UDP, and blocking it closes the
  QUIC/raw-UDP exfiltration path that the SYN-based capture can't observe).
- The composer/npm/Node caches live on named volumes shared across every
  project boxme runs. A malicious package can write to those caches, so poisoned
  cache content could be picked up by a later run in a *different* project.
  Running the install as a non-root user does **not** close this: the cache has
  to be writable by whoever runs the install, which is the same identity the
  package code runs as. The lockfile integrity checks (npm) and `--strict` bound
  the blast radius; per-project isolation is on the roadmap.
- Approval is all-or-nothing in v1.
- A nonzero exit from the command still shows the review (red banner) — abort
  is the natural choice there.
- `BOXME_DEBUG_NET=/path/file.txt` dumps the raw in-guest `tcpdump -r` text
  used for the Network tab, for debugging capture/classification.
