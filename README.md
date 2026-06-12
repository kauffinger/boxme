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
   your host, project tarred in, composer/npm caches on persistent volumes).
2. The command runs fully interactively — prompts and progress bars work.
3. Inside the guest, tcpdump records every DNS lookup and outbound TCP SYN.
4. A full-screen review shows:
   - **Files**: the expected write-set (`vendor/`, lockfiles) summarized,
     anything *outside* it itemized with inline diffs;
   - **Network**: every destination contacted, classified known registry vs
     unexpected.
5. Only on `a` (approve) is the result copied back into your repo. `q` aborts
   and nothing lands.

Supply-chain guards baked into the base image: the composer
`innobrain/soak-time` plugin and npm `min-release-age=7` block dependencies
younger than 7 days.

## Setup

```sh
cargo install --path .
boxme setup        # builds the boxme-base snapshot once (~10 min)
```

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

# Pass environment variables into the guest (private registries, auth):
boxme -e COMPOSER_AUTH composer install      # copy host value
boxme -e NPM_TOKEN=xyz npm install           # set explicitly
```

Anything you pass with `-e` is visible to the package code running in the
sandbox — a malicious postinstall could read it and try to send it somewhere.
The Network tab shows every destination contacted; `--strict` limits where
anything can go.

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

**Every later run** in that project enforces `.boxme/allow` automatically. Add
`--learn` to re-open the picker when a new dependency needs a new host, or just
edit the file. `--strict` ignores the allowlist and permits only the registries
(the tightest setting).

`.boxme/allow` is one entry per line — commit it to share the decision with your
team:

```
example.com         # the domain and every subdomain
=api.example.com    # this exact host only
# comments and blank lines are ignored
```

Real-time "allow this connection? [y/n]" prompting mid-run isn't offered: the
sandbox's network policy is fixed when the VM boots, so trust is decided in the
review between runs, not during one. Path-level rules (e.g. `github.com/org/*`)
aren't possible either — the URL path lives inside TLS, so the policy only ever
sees the hostname.

### Review keys

`↑↓`/`jk` select · `g`/`G` first/last · `h`/`l`/`Tab` switch Files/Network
(`1`/`2` jump directly) · `Ctrl-d`/`Ctrl-u` half-page scroll ·
`Ctrl-f`/`Ctrl-b`/`PgUp`/`PgDn` full-page scroll · `J`/`K` line scroll ·
`Space` trust host (observe run) · `a` approve · `q`/`Ctrl-C` abort

Scroll keys act on the diff on the Files tab and on the list on the Network
tab. `Esc` is deliberately unbound so a reflexive press can't abort a run.

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
