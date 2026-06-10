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

### Review keys

`↑↓`/`jk` select · `Tab` switch Files/Network · `PgUp/PgDn` scroll diff ·
`a` approve · `q`/`Esc` abort

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
- Network capture is observe-by-default; `--strict` boots the VM with a
  deny-by-default policy allowing only DNS and the package registries over
  HTTP(S).
- Approval is all-or-nothing in v1.
- A nonzero exit from the command still shows the review (red banner) — abort
  is the natural choice there.
- `BOXME_DEBUG_NET=/path/file.txt` dumps the raw in-guest `tcpdump -r` text
  used for the Network tab, for debugging capture/classification.
