# boxme — sandboxed package-manager runner

## Context

Running `composer install` / `npm install` on the host executes arbitrary package code (postinstall scripts, composer plugins) with full access to the machine — the classic supply-chain hole. **boxme** is a new standalone Rust CLI at `~/Docker/boxme` (fresh repo, separate from microphp) that runs these commands inside a microsandbox microVM instead: `boxme composer i` boots a VM, runs the command there with live interactive output, then shows a full-screen review of (a) files changed outside the expected write-set and (b) network destinations contacted. Only on explicit approval does the result get copied back into the host repo. Supply-chain guards (composer `innobrain/soak-time` plugin, npm `min-release-age=7`) are baked into the sandbox base image. PHP/Node versions are auto-detected from the host and matched in the guest.

Decisions already made with the user:
- **Review UI**: full ratatui TUI (Files + Network tabs, inline diff pane, `a` approve / `q` abort).
- **Network**: observe-by-default (microsandbox default `public_only` policy + capture), `--strict` opt-in boots a deny-by-default registry whitelist.
- **Version detection**: host binaries run from the project dir first (`php -v` / `node -v`, so mise/asdf/nvm/herd shims resolve per-directory), manifest fallback (`composer.json require.php`, `.nvmrc`, `engines.node`).
- **v1 scope**: `composer` and `npm` only; anything else errors clearly.

Key research finding: microsandbox 0.5.5 exposes **no host-side per-connection network observability** — only aggregate byte counters. So network capture runs *inside the guest*: `tcpdump` (baked into the base image) captures DNS + outbound TCP SYNs to a pcap, which is parsed back into a domain list. Everything else maps onto patterns already proven in microphp (`/Users/kauffinger/Docker/microphp/src-tauri/src/`): snapshot build, tar-in, guest git baseline, streamed execs, named cache volumes.

## Project setup

`cargo new boxme` at `~/Docker/boxme`, single binary. Dependencies:

```toml
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
microsandbox = "0.5"            # defaults include net + prebuilt — no explicit features (matches microphp)
microsandbox-network = "0.5"    # NetworkPolicy / Rule / DomainName for --strict (microphp does the same)
ratatui = "0.29"
crossterm = "0.29"
tar = "0.4"
flate2 = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
owo-colors = "4"
```

## Modules

```
src/
  main.rs       tokio::main; parse CLI, dispatch
  cli.rs        clap derive: `Setup { --force }` + #[command(external_subcommand)] Run(Vec<String>);
                global flags: --strict, --keep, --memory (default 2048), --cpus (default 2)
  setup.rs      build "boxme-base" snapshot (streamed), existence check
  detect.rs     PHP/Node version detection (host binary → manifest → default)
  scripts.rs    all guest shell scripts as consts/fns
  run.rs        end-to-end orchestration
  netcap.rs     tcpdump lifecycle + pcap-text parsing + registry classification
  manifest.rs   guest file-manifest diff + expected-paths table
  review.rs     ratatui review TUI
  copyback.rs   guest tarball → host extraction + deletions
  util.rs       helpers copied from microphp util.rs/stream.rs: tar_directory,
                decode_utf8_stream, strip_ansi, plus stream_shell_stderr (the
                microphp stream.rs::stream_shell loop, printing to stderr
                instead of a Tauri channel)
```

`boxme composer i` works because everything that isn't `setup` falls into the `external_subcommand` variant; `run.rs` validates the first token is `composer` or `npm`.

## `boxme setup` — base snapshot

Copy microphp's build loop verbatim (`base.rs:97–192`): boot builder VM from `node:24` (`.memory(2048).cpus(2).replace().detached(true)`), stream the setup script, `stop_and_wait()` + poll-until-stopped, `Snapshot::builder(BUILDER).name("boxme-base").force().create()`, `Sandbox::remove(BUILDER)`. Existence check via `Snapshot::list()`.

Setup script = microphp's `DEFAULT_BASE_SETUP` (`base.rs:36–95`) adapted:
- apt: `ca-certificates curl lsb-release git unzip procps` **+ `tcpdump`**
- Sury keyring + PHP 8.3/8.4/8.5 side-by-side (same package list incl. mbstring/xml/curl/sqlite3/zip/bcmath/intl/mysql/pgsql), `update-alternatives --set php /usr/bin/php8.4`
- Composer installer + `COMPOSER_ALLOW_SUPERUSER=1` profile export
- `composer global config allow-plugins.innobrain/soak-time true && composer global require innobrain/soak-time --prefer-source --no-interaction` (`--prefer-source` is load-bearing)
- `npm install -g npm@latest && npm config set min-release-age 7 --location=global`
- Node multi-version: `npm install -g n`, `N_PREFIX=/root/.n` exported via `/etc/profile.d/n.sh` with PATH prepend. `/root/.n` lives on a named volume so downloaded versions persist across runs.

## Run flow (`run.rs`)

1. **Validate + detect**: first token ∈ {composer, npm}; compute expected write-set (below); `detect.rs` resolves PHP `X.Y` and Node major. Snapshot missing → `error: run \`boxme setup\` first`.
2. **Boot**: `Sandbox::builder(name).from_snapshot("boxme-base").memory(..).cpus(..).replace()` — **attached** (drop = SIGTERM = cleanup on crash). Name: `boxme-<project-slug>-<short random>`. Named volumes: `/root/.composer/cache` → `boxme-composer-cache`, `/root/.npm` → `boxme-npm-cache`, `/root/.n` → `boxme-node-versions`. `--strict`: `.network(|n| n.policy(...))` with deny-by-default + `Rule::allow_dns()` + DomainSuffix allows for the registry list on 80/443 (mirror microphp `project.rs::whitelist_policy:667–686`).
3. **Unpack**: `tar_directory` (skips top-level vendor/ + node_modules/, preserves symlinks) → `copy_from_host` → guest script: extract to `/workspace`, `git config --global --add safe.directory /workspace`, `git init` if needed, `git add -A && git commit -qm boxme-baseline || true && git tag -f boxme-baseline HEAD`. **No reset-to-last-commit** (unlike microphp): the user's uncommitted state is exactly what the command should operate on. Host git is NOT required.
4. **Switch versions**: PHP via microphp's `php_switch_script` (`project.rs:121–133`); Node via `n install <major>` when ≠ 24 (first time downloads, then cached on the volume).
5. **Manifest before**: `find /workspace -mindepth 1 (-path .git|vendor|node_modules -prune) -o -printf '%P\t%s\t%y\n' | sort` **plus md5 of every regular file** (`-type f -exec md5sum`) — content hashing kills mtime noise entirely and is cheap because vendor/node_modules are excluded. Taken *after* unpack so extraction artifacts can't pollute the diff.
6. **Start tcpdump**: long-lived streamed exec (`exec_stream_with`), keep its `ExecControl`; filter `(udp port 53) or (tcp[tcpflags] & tcp-syn != 0)` writing `/tmp/cap.pcap` on `-i any`. Must be running before step 7 starts.
7. **Run the command**: `sb.attach_with("bash", |a| a.args(["-lc", "cd /workspace && <quoted cmd>"]))` — raw host terminal, SIGWINCH, prompts and progress bars work; returns the exit code. `bash -l` sources `/etc/profile.d` (COMPOSER_ALLOW_SUPERUSER, N_PREFIX/PATH).
8. **Stop tcpdump** (`control.kill()`), parse **in the guest** with `tcpdump -r /tmp/cap.pcap -n` text output (no host pcap dep); host-side join DNS answers ↔ TCP SYN destination IPs → `Vec<NetworkContact { domain: Option<String>, ip, port }>`, classified Known/Unexpected against the registry list: packagist.org + repo./api., github.com + api./codeload./objects.githubusercontent.com/raw.githubusercontent.com, registry.npmjs.org, nodejs.org (for `n`). Parse leniently (token scan, not column positions); on parse failure degrade to "capture unreadable" banner.
9. **Manifest after** + host-side diff → added/modified/deleted, partitioned expected vs unexpected. Expected dirs summarized by file count only. For unexpected text files fetch unified diffs in the guest (`git diff boxme-baseline -- <path>` when tracked; `diff -u /dev/null <path>` for new); binary → flagged only.
10. **Review TUI** (below). Nonzero exit code from step 7 still shows the TUI, with a red `exit: N` banner.
11. **Approve** → copy-back; **abort** → nothing copied, drop handle + `Sandbox::remove(name)`. `--keep` → `sb.detach().await` and print the VM name instead of removing.

### Expected write-set

| Command | Expected dirs | Expected files |
|---|---|---|
| `composer <install\|i\|update\|require\|remove\|...>` | `vendor/` | `composer.lock`; + `composer.json` for require/remove/update |
| `npm <install\|i\|ci\|update\|...>` | `node_modules/` | `package-lock.json`; + `package.json` when the subcommand mutates it (`install <pkg>`, `uninstall`, `update` — i.e. install with package args) |

## Review TUI (`review.rs`)

State: `tab (Files|Network)`, `ListState` for file list, diff scroll offset, `Vec<FileItem>` (ExpectedSummary{count} green / Unexpected{Added|Modified|Deleted|Binary} yellow/red), `Vec<NetworkItem>` (Known green / Unexpected yellow), exit code.

Layout: header (exit code, tab titles, key hints) / main split 35% file list + 65% diff `Paragraph` with `scroll` and ±-line coloring / footer hints. Keys: `↑↓/jk` select, `Tab` switch tab, `PgUp/PgDn` scroll diff, `a` approve, `q`/`Esc`/`Ctrl-C` abort.

Loop: `ratatui::init()` → draw + `crossterm::event::poll(50ms)` → `ratatui::restore()`. Terminal interplay with `attach_with` is sequential (attach restores cooked mode before the TUI starts); defensively call `disable_raw_mode()` (idempotent) before `ratatui::init()`.

## Copy-back (`copyback.rs`)

On approval: guest builds `tar czf /tmp/result.tgz -C /workspace -- <expected dirs that exist> <changed expected files> <unexpected changed paths>`; `copy_to_host` the tarball; host-side: `remove_dir_all` each expected dir first (wholesale replace — sandbox built it fresh), then unpack with the Rust `tar` crate using `unpack_in` **plus explicit rejection of absolute paths and `..` components** (tarball content is sandbox-controlled). Deletions from the manifest diff applied explicitly, restricted to paths inside the project dir, never following symlinks. v1 approval is all-or-nothing (per the original requirement); per-file selection is v2.

## Failure modes

- Missing snapshot → clear message, exit 1. Builder/run VM name collisions → `.replace()`.
- Command nonzero exit → review still shown (red banner), abort is the natural default.
- boxme crash → attached handle drops, VM SIGTERMed; next run's `.replace()` clears stale state.
- tcpdump spawn fails (stale pre-tcpdump snapshot) → warn, skip capture, banner in Network tab suggesting `boxme setup --force`.
- Non-git host dir → fully supported (baseline git lives only in the guest).

## Verification

1. `boxme setup` builds; rerun without `--force` short-circuits.
2. Scratch Laravel app + a planted package with a `post-install-cmd` that writes `/workspace/UNEXPECTED.txt` and curls `evil.example.com`. Run `boxme composer install`:
   - file appears under Unexpected, domain under Unexpected network contacts;
   - **abort** → host has no `vendor/`, no `UNEXPECTED.txt`;
   - **approve** → `vendor/` + lock land, `UNEXPECTED.txt` lands (explicitly approved);
   - after either, no boxme VM left running (check `~/.microsandbox` state / process list).
3. Second run noticeably faster (composer cache volume hit).
4. PHP match: project on PHP 8.3 via version manager → guest `php -v` shows 8.3 in the streamed setup output.
5. `--strict`: install pulling from a non-whitelisted host fails visibly at connect time.
6. `npm install` path: same flow, node_modules/ + package-lock.json land.

## Implementation order

1. **Skeleton + setup**: cargo project, cli.rs, util.rs (port microphp helpers), setup.rs → `boxme setup` runnable.
2. **Boot + unpack**: detect.rs, scripts.rs, run.rs phases 1–4 → VM boots with the repo, right PHP/Node.
3. **Exec + plain copy-back**: tcpdump start/stop (parse stubbed), attach_with command run, manifest diff, copyback with a plain y/N prompt → already useful end-to-end.
4. **TUI + netcap parse**: review.rs, pcap text parsing, unexpected-diff fetching.
5. **Polish**: `--strict`, failure paths, mtime/hash noise filter, evil-postinstall e2e test, README.

## Reference files

- `/Users/kauffinger/Docker/microphp/src-tauri/src/base.rs:36–192` — base setup script + snapshot build loop
- `/Users/kauffinger/Docker/microphp/src-tauri/src/project.rs:31–133` (unpack/php-switch), `:667–686` (whitelist policy)
- `/Users/kauffinger/Docker/microphp/src-tauri/src/util.rs` — tar_directory, free_ports, strip_ansi; `stream.rs` — stream_shell/decode_utf8_stream
- `~/.cargo/registry/src/index.crates.io-*/microsandbox-0.5.5/lib/sandbox/mod.rs` — attach_with raw-mode handling
- `~/.cargo/registry/src/index.crates.io-*/microsandbox-network-0.5.5/lib/policy/` — NetworkPolicy/Rule construction
