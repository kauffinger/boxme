use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use microsandbox::Sandbox;

use crate::util::shell_capture;

/// What approval copies back: expected dirs wholesale, plus individual files.
pub struct CopyPlan {
    /// Expected dirs (e.g. "vendor") — replaced wholesale on the host.
    pub dirs: Vec<String>,
    /// Individual changed files (expected like composer.lock + approved
    /// unexpected adds/modifications).
    pub files: Vec<String>,
    /// Paths to delete on the host (from the manifest diff).
    pub deletions: Vec<String>,
}

/// The approved changeset, pulled out of the guest and waiting on the host to
/// be applied to the project. Produced by [`stage`] while the VM is alive;
/// consumed by [`commit`] after the VM (and its read-only bind of the project)
/// is gone — the project is the live overlay lower during the run, so it must
/// not be mutated until the mount is torn down.
pub struct Staged {
    /// Host path to the result tarball, or `None` if nothing was packed.
    tgz: Option<PathBuf>,
    /// Expected dirs to replace wholesale (only those that existed in the guest).
    replace_dirs: Vec<String>,
    /// Paths to delete on the host.
    deletions: Vec<String>,
}

impl Staged {
    /// The host-side result tarball, if anything was packed. Surfaced so an
    /// aborted copy-out can point the user at their not-yet-applied changes
    /// instead of silently dropping them.
    pub fn tarball(&self) -> Option<&Path> {
        self.tgz.as_deref()
    }
}

/// Tar the approved paths out of the guest into a host-side tarball. Only reads
/// the guest `/workspace`; performs no host mutation, so it is safe to run while
/// the VM still has the project bind-mounted read-only.
pub async fn stage(sb: &Sandbox, project_dir: &Path, plan: &CopyPlan) -> Result<Staged> {
    // Validate every host target up front so a bad path fails before we pull
    // anything, not half-way through applying.
    for dir in &plan.dirs {
        safe_join(project_dir, dir)?;
    }
    for path in &plan.deletions {
        safe_join(project_dir, path)?;
    }

    let mut tar_paths: Vec<String> = Vec::new();
    let mut replace_dirs: Vec<String> = Vec::new();
    for dir in &plan.dirs {
        let exists = shell_capture(
            sb,
            &format!("test -d /workspace/{dir} && echo yes || echo no"),
        )
        .await?
        .trim()
            == "yes";
        if exists {
            tar_paths.push(dir.clone());
            replace_dirs.push(dir.clone());
        }
    }
    tar_paths.extend(plan.files.iter().cloned());

    let tgz = if tar_paths.is_empty() {
        None
    } else {
        // Null-delimited path list via a file: immune to arg-length limits and
        // to any quoting in path names.
        let list = tar_paths.join("\0");
        sb.fs()
            .write("/tmp/boxme-paths.txt", list.as_bytes())
            .await?;
        shell_capture(
            sb,
            "cd /workspace && tar czf /tmp/result.tgz --null --verbatim-files-from -T /tmp/boxme-paths.txt",
        )
        .await
        .context("packing results in the guest failed")?;

        let host_tgz =
            std::env::temp_dir().join(format!("boxme-result-{}.tgz", std::process::id()));
        sb.fs()
            .copy_to_host("/tmp/result.tgz", &host_tgz)
            .await
            .context("copying result tarball to host failed")?;
        Some(host_tgz)
    };

    Ok(Staged {
        tgz,
        replace_dirs,
        deletions: plan.deletions.clone(),
    })
}

/// Write a staged changeset into `base` (an already-canonical dir): replace the
/// expected dirs wholesale, unpack the tarball, then apply deletions. Does *not*
/// remove the source tarball — the caller drops it only once the whole operation
/// has succeeded. Every target is confined to `base` (tarball content is
/// sandbox-controlled).
fn apply_staged(base: &Path, staged: &Staged) -> Result<()> {
    if let Some(host_tgz) = &staged.tgz {
        for dir in &staged.replace_dirs {
            if let Some(target) = contained_target(base, dir)? {
                remove_contained(&target)?;
            }
        }
        extract(host_tgz, base)?;
    }
    for path in &staged.deletions {
        if let Some(target) = contained_target(base, path)? {
            remove_contained(&target)?;
        }
    }
    Ok(())
}

/// Apply a staged changeset to the project in place: replace the expected dirs
/// wholesale, unpack the tarball, then apply deletions. Run only after the VM is
/// gone, so the host tree is no longer a live overlay lower. All target paths are
/// confined to the project dir (tarball content is sandbox-controlled).
pub fn commit(project_dir: &Path, staged: Staged) -> Result<()> {
    // Resolve the project dir once so containment checks compare canonical
    // paths. Every destructive target is confined to this.
    let base = project_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {} failed", project_dir.display()))?;
    apply_staged(&base, &staged)?;
    if let Some(host_tgz) = &staged.tgz {
        let _ = std::fs::remove_file(host_tgz);
    }
    Ok(())
}

/// Which of `paths` (project-relative) currently have uncommitted changes on the
/// host — the files an in-place copy-out would write over on top of the user's
/// own edits. Empty when the tree is clean on those paths, or when `project_dir`
/// isn't a git repo (nothing to protect). Asks git directly with the paths as a
/// pathspec, so the answer is correct whether `project_dir` is the repo root or a
/// subdirectory of one.
pub fn collisions(project_dir: &Path, paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() || !is_git_repo(project_dir) {
        return Ok(Vec::new());
    }
    let mut args: Vec<&str> = vec!["status", "--porcelain", "-z", "--"];
    args.extend(paths.iter().map(String::as_str));
    let out = git(project_dir, &args)?;
    Ok(porcelain_paths(&out))
}

/// Land a staged changeset as a single commit on a fresh branch *without* ever
/// reading or writing the user's working tree — the `boxme claude` branch
/// fallback, taken when an in-place copy-out would overwrite the user's own
/// uncommitted edits. A throwaway git worktree is checked out at HEAD in a temp
/// dir, the agent's files are written and committed there (`--no-verify`: the
/// content is sandbox-produced, and a failing hook shouldn't drop the agent's
/// work), and the worktree is removed — leaving only the new branch. The working
/// tree and index the user is sitting on are untouched, so their in-progress
/// edits survive verbatim. Returns the (possibly uniquified) branch name, or
/// `None` if there was nothing to commit. Run only after the VM is gone.
pub fn commit_to_branch(
    project_dir: &Path,
    staged: Staged,
    message: &str,
    branch: &str,
) -> Result<Option<String>> {
    if staged.tgz.is_none() && staged.replace_dirs.is_empty() && staged.deletions.is_empty() {
        return Ok(None);
    }
    let base = project_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {} failed", project_dir.display()))?;
    if !git_ok(&base, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
        bail!(
            "can't create a branch — this repo has no commits yet.\n\
             commit something first, then re-run; your changes are staged at {}",
            staged
                .tgz
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(nothing packed)".to_string())
        );
    }

    let branch = unique_branch(&base, branch);
    let worktree = std::env::temp_dir().join(format!("boxme-wt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&worktree);
    let wt = worktree.to_string_lossy().into_owned();
    git(&base, &["worktree", "add", "-b", &branch, &wt, "HEAD"])
        .with_context(|| format!("creating worktree for branch {branch} failed"))?;

    let result = (|| -> Result<()> {
        let wt_base = worktree
            .canonicalize()
            .with_context(|| format!("canonicalizing {} failed", worktree.display()))?;
        apply_staged(&wt_base, &staged)?;
        git(&wt_base, &["add", "-A"])?;
        git(&wt_base, &["commit", "--no-verify", "-m", message])?;
        Ok(())
    })();

    // Always tear the worktree down (the branch it created survives); only drop
    // the tarball once we know the commit landed, so a failure leaves it for
    // recovery.
    let _ = git(&base, &["worktree", "remove", "--force", &wt]);
    let _ = std::fs::remove_dir_all(&worktree);
    result?;
    if let Some(host_tgz) = &staged.tgz {
        let _ = std::fs::remove_file(host_tgz);
    }
    Ok(Some(branch))
}

/// Run `git -C <dir> <args>`, returning stdout. Errors carry git's stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like [`git`] but reports only whether the command *succeeded*, swallowing any
/// error — for existence probes where a non-zero exit is an expected answer, not
/// a failure.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_git_repo(dir: &Path) -> bool {
    git_ok(dir, &["rev-parse", "--is-inside-work-tree"])
}

fn branch_exists(base: &Path, name: &str) -> bool {
    git_ok(
        base,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
}

/// First free branch name at or after `desired` (`name`, `name-2`, `name-3`, …),
/// so a second run in the same second — or a leftover branch — bumps the suffix
/// instead of aborting the commit.
fn unique_branch(base: &Path, desired: &str) -> String {
    if !branch_exists(base, desired) {
        return desired.to_string();
    }
    for n in 2.. {
        let candidate = format!("{desired}-{n}");
        if !branch_exists(base, &candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Pull the file paths out of `git status --porcelain -z` output. Each record is
/// NUL-terminated and prefixed with a two-char status code + space (`" M file"`,
/// `"?? file"`); a rename emits the destination as a prefixed record and the
/// source as a bare follow-on. We only need the touched paths, so strip the
/// prefix when present and keep bare records as-is. `-z` means paths are literal
/// (never quoted), so this is a clean split.
fn porcelain_paths(out: &str) -> Vec<String> {
    out.split('\0')
        .filter(|r| !r.is_empty())
        .map(|record| {
            if record.len() > 3 && record.as_bytes()[2] == b' ' {
                record[3..].to_string()
            } else {
                record.to_string()
            }
        })
        .collect()
}

/// Remove a target without following a final-component symlink: a symlink is
/// unlinked itself (not its target), a real dir is removed recursively, a file
/// is removed. A vanished target is a no-op.
fn remove_contained(target: &Path) -> Result<()> {
    match std::fs::symlink_metadata(target) {
        // symlink_metadata doesn't follow, so a symlink reports is_dir() == false
        // and falls through to remove_file, unlinking the link, never its target.
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(target)
            .with_context(|| format!("removing {} failed", target.display())),
        Ok(_) => std::fs::remove_file(target)
            .with_context(|| format!("removing {} failed", target.display())),
        Err(_) => Ok(()),
    }
}

/// Resolve a sandbox-supplied relative path to an absolute target guaranteed to
/// live inside `base` (which must already be canonical). On top of [`safe_join`]'s
/// absolute/`..` rejection, this canonicalizes the target's *parent* — resolving
/// any symlinked components — and refuses anything that lands outside `base`, so
/// a deletion or wholesale-replace can't be redirected out of the project via a
/// symlinked directory (the same guard `tar`'s `validate_inside_dst` applies on
/// extract). `Ok(None)` means the target (or its parent) doesn't exist, so there
/// is nothing to remove.
fn contained_target(base: &Path, rel: &str) -> Result<Option<PathBuf>> {
    let joined = safe_join(base, rel)?;
    let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) else {
        return Ok(None);
    };
    let canon_parent = match parent.canonicalize() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !canon_parent.starts_with(base) {
        bail!("path escapes the project dir via a symlinked parent: {rel}");
    }
    Ok(Some(canon_parent.join(name)))
}

/// Unpack with explicit rejection of absolute paths and `..` components on top
/// of `unpack_in`'s own containment.
fn extract(tgz: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(tgz)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            bail!("tarball entry escapes the project dir: {}", path.display());
        }
        entry.unpack_in(dest)?;
    }
    Ok(())
}

/// Join a sandbox-supplied relative path onto the project dir, refusing
/// anything that could land outside it.
fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("path escapes the project dir: {rel}");
    }
    Ok(base.join(rel_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_paths_strips_status_and_keeps_bare_records() {
        // Modified, untracked, staged-add, and a rename (dest prefixed, source bare).
        let out = " M composer.lock\0?? app/New.php\0A  src/added.rs\0R  to/new.rs\0from/old.rs\0";
        let paths = porcelain_paths(out);
        assert_eq!(
            paths,
            vec![
                "composer.lock",
                "app/New.php",
                "src/added.rs",
                "to/new.rs",
                "from/old.rs",
            ]
        );
    }

    #[test]
    fn porcelain_paths_empty_when_clean() {
        assert!(porcelain_paths("").is_empty());
    }

    #[test]
    fn porcelain_paths_preserves_spaces_in_names() {
        let out = " M dir/a file.txt\0";
        assert_eq!(porcelain_paths(out), vec!["dir/a file.txt"]);
    }
}
