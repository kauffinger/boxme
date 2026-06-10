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

/// Tar the approved paths in the guest, pull the tarball, replace the expected
/// dirs and unpack — then apply deletions. All target paths are confined to
/// the project dir (tarball content is sandbox-controlled).
pub async fn apply(sb: &Sandbox, project_dir: &Path, plan: &CopyPlan) -> Result<()> {
    let mut tar_paths: Vec<String> = Vec::new();
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
        }
    }
    tar_paths.extend(plan.files.iter().cloned());

    if !tar_paths.is_empty() {
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

        // Wholesale replace each expected dir the sandbox built fresh.
        for dir in &plan.dirs {
            if !tar_paths.iter().any(|p| p == dir) {
                continue;
            }
            let target = safe_join(project_dir, dir)?;
            if target.exists() {
                std::fs::remove_dir_all(&target)
                    .with_context(|| format!("removing {} failed", target.display()))?;
            }
        }

        extract(&host_tgz, project_dir)?;
        let _ = std::fs::remove_file(&host_tgz);
    }

    for path in &plan.deletions {
        let target = safe_join(project_dir, path)?;
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&target)
                .with_context(|| format!("deleting {} failed", target.display()))?,
            Ok(_) => std::fs::remove_file(&target)
                .with_context(|| format!("deleting {} failed", target.display()))?,
            Err(_) => {}
        }
    }

    Ok(())
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
