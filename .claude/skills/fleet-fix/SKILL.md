---
name: fleet-fix
description: Fix vulnerable composer/npm dependencies across every repo in a folder with minimal churn, using boxme's sandboxed non-interactive flow — composer fix and npm audit fix, non-breaking only. Use when the user asks to fix vulnerabilities, patch security advisories, or run audit fixes across repos without a general dependency update (e.g. "fix the vulnerable deps in all repos in ~/Code"). For general dependency updates, use fleet-update instead. Args: the folder to sweep (defaults to the current directory) and any extra constraints.
---

# Fleet vulnerability fix via boxme

Fix known-vulnerable dependencies across every repo in a folder, touching only
the affected packages — no general update churn. Every fix runs inside a boxme
microVM — **never run composer/npm mutations directly on the host**. Only
non-breaking (in-constraint) fixes are applied; a vulnerability whose safe
version is out of range is reported as skipped, never forced.

The tools:

- **composer:** `composer fix --no-fail` — the `innobrain/composer-fix` plugin
  baked into boxme's base image. It audits installed packages and runs a
  targeted in-range update of the vulnerable ones only. Out-of-range fixes are
  reported and left alone. `--no-fail` is required: without it the command
  exits 1 whenever any advisory survives the update, which boxme reports as a
  failed command (exit 2) and stages nothing — throwing away the in-range fixes
  it did make. Residual vulnerabilities are this skill's expected outcome, not
  a failure.
- **npm:** `npm audit fix` — same semantics, non-breaking without `--force`.

This skill only fixes vulnerabilities. If the user wants dependencies updated
generally, use the `fleet-update` skill instead.

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
run replaces it.

When one step depends on another's output (e.g. a script hook that needs
`vendor/` present), don't apply in between — chain the commands with `++` in a
single run: `boxme --json composer fix --no-fail ++ composer run-script
post-update-cmd`. The chain runs in one VM, stops at the first failure, and
stages one combined changeset.

### Composer repos

1. `boxme --json composer fix --no-fail > report.json 2> boxme.log`
   (add `--composer-auth` if `composer.json` has a `repositories` section that
   plausibly needs credentials and the host has a global composer `auth.json`).
   **Never pass `--force`** — that rewrites root constraints (breaking), which
   the policy forbids.
   If the log shows `Command "fix" is not defined`, the user's base snapshot
   predates the plugin — tell them to run `boxme setup --force`, and skip the
   composer side of the sweep (do not fall back to `composer update`; that is
   the fleet-update skill's job and causes churn this skill promises to avoid).
   If it shows `--no-fail` is not a defined option, the snapshot predates
   composer-fix 2.0.0 — same remedy, `boxme setup --force`.
   `composer fix` needs either an installed `vendor/` or a `composer.lock`; a
   repo with neither exits 1 and is reported as unfixable, not retried.
2. Handle the exit code (see "Exit codes and findings" below); on success,
   `boxme apply`.
3. After a successful apply, list what is still vulnerable for the final
   report — **host-side is OK here** because this exact invocation executes no
   project code: `composer audit --locked --no-plugins --format=json`.
   Anything still reported is out of constraint range (breaking to fix) or held
   back by the soak-time plugin's recency window — either way, per policy
   **do not force it**; list it as "vulnerable, skipped" with the reason if the
   `[composer-fix]` log output states one.

### npm repos

1. `boxme --json npm audit fix > report.json 2> boxme.log` → handle exit code →
   `boxme apply` on success. **Never pass `--force`** — that is exactly the
   breaking-major escalation the policy forbids.
2. Remaining vulnerabilities: host-side `npm audit --json` (reads the lockfile
   and queries the registry advisory API; runs no project code). Whatever
   remains after a non-forced `audit fix` needs a breaking upgrade — report it
   as skipped.

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
  - `unexpected_files`: inspect before deciding. Read each `files[]` entry
    with `expected: false` — its `diff` in the report when present, otherwise
    the staged content via
    `tar xzf .boxme/pending/changeset.tgz -O -- <path>`. Inspection is
    **read-only**: never execute, source, or import anything from a staged
    changeset. Apply only if every unexpected change is plainly benign for the
    command that ran (e.g. a metadata/manifest file a package legitimately
    regenerates, a patch applied by a patches plugin). Anything you can't
    positively explain — new executable or script files, edits to app source,
    CI/config, dotfiles — leave the changeset staged, include it in the final
    report, and move on. When unsure, don't apply.
  - `outside_writes`: **never apply.** Writes outside `/workspace` are a
    supply-chain red flag. Leave the changeset staged, include
    `outside.files[]` in the final report, and move on.
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

| repo | vulns fixed | vulns skipped (breaking / held back) | needs review |
|------|-------------|--------------------------------------|--------------|

- "needs review" rows must include the finding kind, the offending
  hosts/files, and the exact follow-up commands:
  `cd <repo> && tar tzf .boxme/pending/changeset.tgz` to inspect,
  then `boxme apply` or `boxme discard`.
- List every skipped advisory with package, installed version, and the
  advisory title, so the user can schedule the breaking upgrades deliberately
  (e.g. with `boxme claude 'upgrade <pkg> to <major> and fix the breakage'`).
- Report failures truthfully: a repo whose command failed is "failed", not
  "skipped".

## Hard rules

- Never run composer/npm mutations on the host; only `composer audit
  --locked --no-plugins` and `npm audit` (read-only, no project code) are
  allowed host-side.
- Never apply a changeset whose findings include `outside_writes`. Unexpected
  files may be applied only after reading every one and finding it benign —
  and never execute, run, or source any file from a staged changeset.
- Never use `npm audit fix --force` or `composer fix --force`, never edit
  version constraints in `composer.json`/`package.json` to force a
  vulnerability fix — breaking fixes are reported, not applied.
- Never fall back to a general `composer update`/`npm update` — minimal churn
  is this skill's contract.
- Only `boxme allow` hosts you can point to in the repo's own configuration;
  everything else is the user's call.
