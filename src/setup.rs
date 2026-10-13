use anyhow::{bail, Context, Result};
use microsandbox::sandbox::{SandboxBuilder, SandboxStatus};
use microsandbox::setup::{
    install_runtime, resolve_runtime, resolve_runtime_version, InstallOptions,
};
use microsandbox::{Sandbox, Snapshot};

use crate::scripts::BASE_SETUP;
use crate::util::stream_shell_stderr;

/// Snapshot every boxme run boots from. Built once, reused for fast boots.
pub const BASE_SNAPSHOT: &str = "boxme-base";
pub(crate) const BUILDER: &str = "boxme-base-builder";

/// A sandbox builder that cold-boots `name` from the base snapshot.
///
/// microsandbox 0.7 moved snapshot boots to `Sandbox::restore`, but that
/// builder deliberately has no `env`, `secret_entry`, TLS settings, `replace`
/// or attached mode — all of which every boxme boot needs (credentials and
/// composer-auth placeholders, crash-SIGTERM). The full creation builder still
/// resolves a snapshot source through `override_snapshot` (hidden from the docs;
/// it's what the `msb` CLI uses for config-file snapshot sources), so boot
/// through that. `BASE_SNAPSHOT` is a disk snapshot, so this is a fresh boot
/// of the captured disk, not a resumed VM.
pub(crate) fn base_builder(name: &str) -> SandboxBuilder {
    Sandbox::builder(name).override_snapshot(BASE_SNAPSHOT)
}

pub async fn base_snapshot_exists() -> Result<bool> {
    let snapshots = Snapshot::list().await?;
    Ok(snapshots.iter().any(|s| s.name() == Some(BASE_SNAPSHOT)))
}

/// Provision the libkrun runtime (`msb` + `libkrunfw`) the SDK needs to boot
/// microVMs, into `~/.microsandbox/{bin,lib}`. The version is pinned to the
/// SDK's compile-time `PREBUILT_VERSION` (`InstallOptions::default`), so the
/// runtime always matches the linked crate — no separate microsandbox CLI
/// install required. Idempotent: a no-op when the right version is already
/// present, a forced re-download on mismatch. (The SDK's own `ensure_runtime`
/// accepts *any* complete pair, which would leave a pre-upgrade runtime in place
/// and silently drop to its older launch contract.)
async fn ensure_runtime() -> Result<()> {
    eprintln!("Ensuring microsandbox runtime (libkrun + msb) is installed...");
    let config = microsandbox::config::config().context("loading the microsandbox config")?;
    let options = InstallOptions::default();
    // Any lookup failure (absent, partial, unreadable) falls through to a
    // forced reinstall, which is also what repairs a partial install.
    let installed = resolve_runtime(&config)
        .ok()
        .and_then(|runtime| resolve_runtime_version(runtime.msb_path).ok().flatten());
    if installed.is_some_and(|v| v.to_string() == options.version) {
        return Ok(());
    }
    install_runtime(
        &config,
        InstallOptions {
            force: true,
            ..options
        },
    )
    .await
    .context("installing the microsandbox runtime (libkrun + msb)")?;
    Ok(())
}

/// `stop_and_wait` can return before the runtime commits the stopped state to
/// its database, and `Snapshot::create` requires a fully stopped source — so
/// poll until the status settles.
async fn wait_until_stopped(name: &str) -> Result<()> {
    for _ in 0..50 {
        let status = Sandbox::get(name).await?.status_snapshot();
        if matches!(status, SandboxStatus::Stopped | SandboxStatus::Crashed) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    bail!("builder did not reach a stopped state in time")
}

pub async fn setup(force: bool, disk_gib: u32) -> Result<()> {
    // Before the snapshot check, so a plain `boxme setup` after upgrading boxme
    // brings the runtime up to the new SDK without rebuilding the snapshot.
    ensure_runtime().await?;

    if !force && base_snapshot_exists().await? {
        eprintln!("base snapshot '{BASE_SNAPSHOT}' already exists — use --force to rebuild");
        return Ok(());
    }

    // The writable overlay (root disk) is a fixed-size ext4 file baked into the
    // snapshot; booting from the snapshot can't resize it, so the run-time disk
    // ceiling is whatever we set here. ext4 is sparse, so a generous size is
    // nearly free until used.
    let upper_mib = disk_gib * 1024;
    eprintln!("Booting base builder from node:24 ({disk_gib} GiB overlay)...");
    let sb = Sandbox::builder(BUILDER)
        .image("node:24")
        .root_disk(upper_mib)
        .memory(2048)
        .cpus(2)
        .replace()
        .detached(true)
        .create()
        .await?;

    let code = stream_shell_stderr(&sb, BASE_SETUP).await?;
    if code != 0 {
        let _ = sb.stop().await;
        bail!("base setup exited with code {code}");
    }

    eprintln!("\nStopping builder and capturing snapshot...");
    sb.stop().await?;
    wait_until_stopped(BUILDER).await?;

    Snapshot::builder(BASE_SNAPSHOT)
        .from_sandbox(BUILDER)
        .force()
        .create()
        .await?;

    Sandbox::remove(BUILDER).await?;

    eprintln!("Base snapshot '{BASE_SNAPSHOT}' is ready.");
    Ok(())
}
