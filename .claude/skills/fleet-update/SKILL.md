---
name: fleet-update
description: Update composer/npm dependencies across every repo in a folder, using boxme's sandboxed non-interactive flow. Use when the user asks to update all repos in a directory or bulk-update dependencies across projects (e.g. "update all repos in ~/Code for me"). For a security-only pass that fixes vulnerable packages without general churn, use fleet-fix instead. Args: the folder to sweep (defaults to the current directory) and any extra constraints.
---

# Fleet dependency update via boxme

Update dependencies across every repo in a folder. Every update runs inside a
boxme microVM — **never run composer/npm install/update directly on the host**.
Updates stay within each project's existing version constraints
(semver-compatible), so nothing breaking is applied.

This skill does general updates. If the user only wants vulnerabilities fixed
(minimal churn), use the `fleet-fix` skill instead.

## Preflight

1. `command -v boxme` — if missing, stop and tell the user to install it
   (`cargo install --path .` in the boxme repo, or the `curl … | sh` one-liner
   from its README).
2. Resolve the target folder from the user's request. Enumerate candidate repos:
   immediate subdirectories containing `composer.json` or `package.json` (if the
   folder itself is such a repo, treat it as the single candidate). Skip
   `node_modules`, `vendor`, hidden dirs.
3. Classify each repo:
   - `composer.json` → composer work.
   - `package.json` **and no** `yarn.lock`/`pnpm-lock.yaml` → npm work.
     If a yarn/pnpm lockfile exists, skip the JS side and report
     "unsupported package manager" (boxme only wraps composer and npm, and
     running npm there would corrupt the lockfile setup).
   - A repo can be both.
4. Dirty-file guard: in each git repo, run
   `git status --porcelain -- composer.json composer.lock package.json package-lock.json`.
   If any of those are locally modified, skip the repo and report it — the
   in-place `boxme apply` would clobber the user's uncommitted edits.
5. If the very first boxme run fails with "base snapshot missing", tell the user
   to run `boxme setup` (~10 min, one-time) and stop; don't run setup unasked.

## Per-repo procedure

Boxme operates on the current directory: **every command below must run with the
repo as cwd** (use a subshell: `(cd "$repo" && boxme …)`). Capture stderr to a
per-repo log file — `--json` streams the guest command's output there.

Decide on each staged changeset (apply/discard) **before** running the next
boxme command in the same repo: `.boxme/pending` is a single slot and the next
run replaces it, and the next VM mounts the host tree as-is, so an unapplied
update is invisible to the following step.

### Composer repos

1. `boxme --json composer update > report.json 2> boxme.log`
   (add `--composer-auth` if `composer.json` has a `repositories` section that
   plausibly needs credentials and the host has a global composer `auth.json`).
2. Handle the exit code (see "Exit codes and findings" below); on success,
   `boxme apply`.
3. After a successful apply, note what is still vulnerable for the final
   report — **host-side is OK here** because this exact invocation executes no
   project code: `composer audit --locked --no-plugins --format=json`.
   After a full in-range update, anything still reported needs a
   constraint/major bump — a breaking change, so **do not attempt it**; list it
   as "vulnerable, fix requires breaking upgrade (skipped)".

### npm repos

1. `boxme --json npm update > report.json 2> boxme.log` → handle exit code →
   `boxme apply` on success.
2. Remaining vulnerabilities for the final report: host-side `npm audit --json`
   (reads the lockfile and queries the registry advisory API; runs no project
   code). Report what remains as "still vulnerable" — fixing it is the
   `fleet-fix` skill's job (non-breaking) or a deliberate breaking upgrade.

## Exit codes and findings

`boxme --json` prints a `Report` (schema 1) to stdout and exits:

- **0 — clean.** Nothing needs a second look: run `boxme apply`.
- **2 — the guest command itself failed.** Nothing was staged. Record the tail
  of `boxme.log` for the final report and move on to the next repo.
- **3 — findings.** The changeset is staged under `.boxme/pending/` (with a
  `report.json` copy next to it) but must not be blindly applied. Read
  `findings` from the report:
  - `blocked_hosts` **only**: look at `network.contacts[]` entries with
    `status: "blocked"`. If a blocked host is plainly the repo's own
    infrastructure — it appears in `composer.json` `repositories` or the repo's
    `.npmrc` — allow it and retry once:
    `boxme discard && boxme allow <host> && boxme --json <same command>`.
    Any other blocked host: leave the changeset staged, report it, move on.
  - `unexpected_files` or `outside_writes`: **never apply.** These are
    supply-chain red flags (a postinstall script wrote outside the expected
    vendor/lockfile surface). Leave the changeset staged, include the file list
    (`files[]` entries with `expected: false`, and `outside.files[]`) in the
    final report, and move on.
  - `network_capture_unavailable` / `outside_scan_unavailable`: the integrity
    evidence is missing, so don't auto-apply; leave staged and flag for the
    user.
- **1 — boxme error** (bad invocation, missing snapshot, VM failure). Record
  and move on; if it's the missing-snapshot error, stop the whole sweep (every
  repo will fail the same way).

`boxme apply` and `boxme discard` also run with the repo as cwd.

## Concurrency

Sequential is the safe default. Each VM takes ~2 GiB RAM / 2 CPUs, so on a
well-provisioned host you may run up to 3 repos concurrently (background Bash
per repo), but never two boxme commands in the same repo at once (one pending
slot, and same-name VM collisions). When in doubt, go sequential — the runs are
minutes each, not hours.

## Final report

End with a per-repo summary the user can act on:

| repo | composer | npm | still vulnerable (breaking to fix) | needs review |
|------|----------|-----|------------------------------------|--------------|

- "needs review" rows must include the finding kind, the offending
  hosts/files, and the exact follow-up commands:
  `cd <repo> && tar tzf .boxme/pending/changeset.tgz` to inspect,
  then `boxme apply` or `boxme discard`.
- List every still-vulnerable advisory with package, installed version, and
  the advisory title, so the user can schedule the breaking upgrades
  deliberately (e.g. with `boxme claude 'upgrade <pkg> to <major> and fix the
  breakage'`).
- Report failures truthfully: a repo whose command failed is "failed", not
  "skipped".

## Hard rules

- Never run composer/npm mutations on the host; only `composer audit
  --locked --no-plugins` and `npm audit` (read-only, no project code) are
  allowed host-side.
- Never apply a changeset whose findings include `unexpected_files` or
  `outside_writes`.
- Never edit version constraints in `composer.json`/`package.json` — updates
  stay within what the project already allows.
- Only `boxme allow` hosts you can point to in the repo's own configuration;
  everything else is the user's call.
