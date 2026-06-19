use std::path::{Component, Path, PathBuf};

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

/// Apply a staged changeset to the project: replace the expected dirs wholesale,
/// unpack the tarball, then apply deletions. Run only after the VM is gone, so
/// the host tree is no longer a live overlay lower. All target paths are
/// confined to the project dir (tarball content is sandbox-controlled).
pub fn commit(project_dir: &Path, staged: Staged) -> Result<()> {
    // Resolve the project dir once so containment checks compare canonical
    // paths. Every destructive target below is confined to this.
    let base = project_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {} failed", project_dir.display()))?;

    if let Some(host_tgz) = &staged.tgz {
        for dir in &staged.replace_dirs {
            if let Some(target) = contained_target(&base, dir)? {
                remove_contained(&target)?;
            }
        }
        extract(host_tgz, &base)?;
        let _ = std::fs::remove_file(host_tgz);
    }

    for path in &staged.deletions {
        if let Some(target) = contained_target(&base, path)? {
            remove_contained(&target)?;
        }
    }

    Ok(())
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
