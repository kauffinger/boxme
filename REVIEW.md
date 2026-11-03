# boxme code review — action items

Findings from a full review of the CLI (2026-07). Baseline is clean:
`cargo fmt --check`, `cargo clippy --all-targets`, and all tests pass.
Since then boxme was cut back to the package-manager run (`claude`, `dev`,
`login`/`logout`, `ps`/`attach`/`exec`/`kill` and `--keep` are gone), which
retired items 6, 9 and 10 along with the code they pointed at. The
security-boundary design is sound and mostly implemented as documented; the items
below are the real gaps, ordered so we can work them one at a time.

Work top-down. Each item has: where, what's wrong, a concrete failure scenario,
and the intended fix. Check the box when done + covered by a test.

---

## Security

### [x] 1. `--composer-auth` leaks a credential when the token is the http-basic *username*

- **Where:** `src/composer_auth.rs:145-162` (`build`, the `http-basic` branch)
- **Problem:** Only `password` is swapped for a placeholder; `username` is passed
  through verbatim into `COMPOSER_AUTH`. The comment assumes the username is never
  a secret, which is false for a common pattern.
- **Failure scenario:** A user's global `auth.json` holds
  `{"http-basic": {"repo.packagist.com": {"username": "<real-token>", "password": "x-oauth-basic"}}}`
  (GitHub PAT-over-HTTPS, Private Packagist, Satis all do this). `build()`
  placeholders only `password`; the real token stays in `username` and is
  serialized straight into the guest env, where any sandboxed code can read it —
  defeating the whole point of `--composer-auth`.
- **Fix:** Detect when the username carries the secret and placeholder it too (or
  placeholder both fields and let the proxy substitute each). Keep `allowed_hosts`
  scoped to the credential's own host. Add a unit test for the
  token-as-username shape.

### [x] 2. Copy-back plants a guest-authored symlink pointing at an arbitrary host path

- **Where:** `src/copyback.rs:382-399` (`extract`)
- **Problem:** The absolute/`..` guard only inspects `entry.path()` (where the
  link file lands). A symlink entry's *target* (`entry.link_name()`) is never
  validated. The guest's `find`/`tar` store symlinks as symlinks, so the target
  survives into `unpack_in`.
- **Failure scenario:** A malicious postinstall script — or a prompt-injected
  `boxme claude` agent — creates `innocuous.txt -> /Users/you/.ssh/id_rsa`. It
  shows as an added file in the diff and copy-back materializes it into the
  working tree. On the `boxme claude` path there is **no review TUI** at all.
  Contradicts the documented "copy-back never follows symlinks out of the project
  dir."
- **Bounded by:** tar 0.4.46's `validate_inside_dst` blocks the classic
  write-*through*-a-symlink escape, so this can't overwrite host files during
  copy-back — the residual risk is planting a dangling pointer to an arbitrary
  host path that later host tooling could dereference.
- **Fix:** In `extract`, reject any symlink/hardlink entry whose `link_name()` is
  absolute or contains `..` (mirror the existing `entry.path()` check). Add a test
  with a tarball containing an escaping symlink entry.

### [x] 3. Guest can forge a DNS answer to disguise its exfil IP as a known registry

- **Where:** `src/netcap.rs:138-152` (`parse_tcpdump_text`, the DNS-answer join)
- **Problem:** Any packet carrying answer-counts whose txn id matches a prior
  query is joined IP→domain, with no check that the packet's source is the real
  resolver (`10.0.2.3:53`).
- **Failure scenario:** A privileged guest process self-sends a spoofed DNS answer
  claiming `github.com -> <attacker IP>`, then connects there. The review labels
  the attacker IP `known` and suppresses the "unexpected host" signal — most
  damaging in observe / `--learn` runs where egress is open.
- **Scope:** Review *observability* only; the hard network-enforcement layer is
  separate and unaffected.
- **Fix:** Only accept DNS answers whose source address is the known resolver.
  Extend the parser test to reject a non-resolver-sourced answer.

---

## Correctness / robustness

### [x] 4. Interactive runs always exit 0, even when the wrapped command failed

- **Where:** `src/run.rs:165` (and `learn_run`/`enforced_run` return `Result<()>`)
- **Problem:** The guest's real exit code lives in `run.exit_code` and is only
  *displayed* in the TUI; it is never propagated to `run()`'s return value, which
  hardcodes `Ok(0)`. The `--json` path handles this correctly via
  `report::exit_code_for`.
- **Failure scenario:** `boxme composer install && php artisan migrate` proceeds
  to migrate even when composer exited non-zero inside the box.
- **Fix:** Thread `run.exit_code` back through `learn_run`/`enforced_run` so
  `run()` returns the guest's exit code (mapped sensibly against copy-back
  outcome). Decide the semantics when the command failed but the user still
  approves a partial changeset.
- **Done:** an approved run exits with the guest command's own code (a failed
  command's partial result can be applied, but the failure is not masked); an
  aborted review exits 1 (`run::EXIT_ABORTED`) so a `&&` chain stops.

### [x] 5. `--strict --learn` silently bypasses strict enforcement

- **Where:** `src/run.rs:153, 383-391, 400`
- **Problem:** `cli.learn` routes into `learn_run` with no `strict` check. The
  observe pass runs fully open; the "no re-run needed" fast path
  (`run.rs:384-391`) commits the open-network result directly, and the re-run path
  uses `enforced_policy(merged)` (registries + allowlist), not strict's
  registries-only. So `--strict` is ignored whenever `--learn` is passed.
- **Failure scenario:** `boxme --strict --learn composer install` trusts one extra
  host during review and copies back a run that executed with the network fully
  open — exactly what `--strict` promises can't happen.
- **Fix:** Make the combination an error, or let `strict` win (ignore `--learn`
  and enforce registries-only). Prefer erroring at arg-parse/validation time in
  `run()`.

### [x] 6. A `copyback::stage` failure can permanently lose an agent's work

- **Where:** `src/claude.rs` (post-run `cleanup`) + `src/copyback.rs` stage path
- **Problem:** `claude()` calls `cleanup()` after the guest run regardless of
  outcome, and `cleanup` only preserves the VM under `--keep`. If `stage` fails
  mid-copy-out (host temp disk full, transient virtiofs error), the changeset
  existed only in the now-deleted guest upper.
- **Failure scenario:** A transient copy-out error silently discards a whole
  `boxme claude` session with no fallback.
- **Fix:** On a stage/copy-out error, keep the VM (don't `cleanup`) and print how
  to recover, or retry the copy-out before tearing down.

### [x] 7. Non-atomic apply with no recovery pointer

- **Where:** `src/copyback.rs:147-179` (`apply_staged` / `commit`)
- **Problem:** `replace_dirs` (e.g. `vendor`) are removed wholesale *before*
  `extract()`. A mid-extract failure leaves a deleted/partial `vendor` and, unlike
  the interactive abort path, no message pointing at the surviving staged tarball.
- **Failure scenario:** Corrupt tarball / disk full during `commit()` leaves a
  broken `vendor` and no visible recovery instructions.
- **Fix:** At minimum, on `commit()` error print the preserved tarball path (as
  the abort path already does). Better: extract to a temp dir and swap into place,
  or only remove `replace_dirs` after a successful extract.

### [x] 8. A killed tcpdump after startup silently truncates the network review

- **Where:** `src/netcap.rs:69-101`
- **Problem:** The `exited` flag is only consulted ~700ms after start (to detect a
  missing base image). A later kill isn't surfaced; `stop()`/`contacts()` proceed
  and return only pre-kill packets.
- **Failure scenario:** A guest that exfiltrates then `pkill tcpdump` leaves an
  observe run looking artificially clean, and the user may approve a compromised
  changeset.
- **Fix:** Check `exited` (or the capture process's liveness) at `stop()` time and
  raise the capture banner if it died mid-run.

---

## Minor

### [x] 9. Linux token file is briefly world-readable (moot — `auth.rs` removed with `boxme claude`)

- **Where:** `src/auth.rs:144-145`
- **Problem:** `std::fs::write` creates the file at default umask (typically 0644)
  and only chmod's to 0600 afterward; a crash between the two leaves it broad.
- **Fix:** Create with `OpenOptions::new().mode(0o600)` from the start.

### [x] 10. `boxme dev` misses host edits made during dependency install (moot — `boxme dev` removed)

- **Where:** `src/dev.rs:192-286`
- **Problem:** The host→guest watcher starts only *after* the initial tar snapshot
  and the multi-minute `composer/npm install`; edits in that window are never
  reconciled (the watcher is event-driven, no catch-up pass).
- **Fix:** Run a reconciliation sync of the (non-excluded) tree right after the
  watcher starts. (Note: `dev.rs` is already flagged "unvalidated against a live
  VM" in CLAUDE.md.)

### [x] 11. `--json --keep` discards the VM on a failed run

- **Where:** `src/run.rs:192-195`
- **Problem:** `json_run`'s error path unconditionally `discard()`s, ignoring
  `--keep`, unlike the interactive path.
- **Fix:** Honor `--keep` on the `--json` error path (`cleanup` instead of
  `discard`).

### [x] 12. `npm_config_cpu` pinned alone via `-e` is silently clobbered

- **Where:** `src/run.rs:807-821` (`npm_platform_export`)
- **Problem:** The guard only checks whether the user pinned `npm_config_os`;
  a lone `-e npm_config_cpu=…` still gets overwritten by boxme's own export.
- **Fix:** Skip the export of whichever of `npm_config_os` / `npm_config_cpu` the
  user already pinned (check both keys independently).

### [x] 13. Retargeted symlink produces no file-review row

- **Where:** `src/manifest.rs:88`
- **Problem:** Only a *kind* change is flagged; a symlink retargeted to a new
  destination (its `size`/target-length is collected but unused for symlinks)
  shows no Added/Modified/Deleted row. Documented as "compared by existence +
  type only," but it undercuts "review every file change."
- **Fix (optional):** Compare the symlink target for symlink entries and emit a
  Modified row when it changes.

### [x] 14. Hostname matching is case-sensitive

- **Where:** `src/netcap.rs:172`, `src/allowlist.rs:75`
- **Problem:** DNS-observed domains and allowlist entries aren't lowercased before
  comparison. Fails closed (over-blocks), so usability not security.
- **Fix:** Lowercase both sides before matching.

---

## Suggested order

1. **#1** and **#2** first — both defeat stated security guarantees with realistic
   triggers.
2. **#4** next — silent success on failure is an easy footgun for `&&` chaining.
3. Then the robustness batch (#5–#8) and the minors as time allows.
