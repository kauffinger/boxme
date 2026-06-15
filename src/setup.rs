use anyhow::{bail, Context, Result};
use microsandbox::sandbox::SandboxStatus;
use microsandbox::{Sandbox, Snapshot};

use crate::scripts::BASE_SETUP;
use crate::util::stream_shell_stderr;

/// Snapshot every boxme run boots from. Built once, reused for fast boots.
pub const BASE_SNAPSHOT: &str = "boxme-base";
const BUILDER: &str = "boxme-base-builder";

pub async fn base_snapshot_exists() -> Result<bool> {
    let snapshots = Snapshot::list().await?;
    Ok(snapshots.iter().any(|s| s.name() == Some(BASE_SNAPSHOT)))
}

/// Provision the libkrun runtime (`msb` + `libkrunfw`) the SDK needs to boot
/// microVMs, into `~/.microsandbox/{bin,lib}`. The version is pinned to the
/// SDK's compile-time `PREBUILT_VERSION`, so the runtime always matches the
/// linked crate — no separate microsandbox CLI install required. Idempotent: a
/// no-op when the right version is already present, a re-download on mismatch.
async fn ensure_runtime() -> Result<()> {
    eprintln!("Ensuring microsandbox runtime (libkrun + msb) is installed...");
    microsandbox::setup::install()
        .await
        .context("installing the microsandbox runtime (libkrun + msb)")
}

/// `stop_and_wait` can return before the runtime commits the stopped state to
/// its database, and `Snapshot::create` requires a fully stopped source — so
/// poll until the status settles.
async fn wait_until_stopped(name: &str) -> Result<()> {
    for _ in 0..50 {
        let status = Sandbox::get(name).await?.status();
        if matches!(status, SandboxStatus::Stopped | SandboxStatus::Crashed) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    bail!("builder did not reach a stopped state in time")
}

pub async fn setup(force: bool) -> Result<()> {
    if !force && base_snapshot_exists().await? {
        eprintln!("base snapshot '{BASE_SNAPSHOT}' already exists — use --force to rebuild");
        return Ok(());
    }

    ensure_runtime().await?;

    eprintln!("Booting base builder from node:24...");
    let sb = Sandbox::builder(BUILDER)
        .image("node:24")
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

    Snapshot::builder(BUILDER)
        .name(BASE_SNAPSHOT)
        .force()
        .create()
        .await?;

    Sandbox::remove(BUILDER).await?;

    eprintln!("Base snapshot '{BASE_SNAPSHOT}' is ready.");
    Ok(())
}
