//! All shell run inside the guest, in one place.

use crate::util::shell_quote;

/// Versions of PHP installed side by side in the base snapshot. Keep in sync
/// with the loop in `BASE_SETUP`.
pub const PHP_VERSIONS: &[&str] = &["8.3", "8.4", "8.5"];

/// The version the guest gets when detection finds nothing.
pub const DEFAULT_PHP_VERSION: &str = "8.4";

/// Node major shipped by the node:24 base image — no `n install` needed for it.
pub const BASE_NODE_MAJOR: u32 = 24;

/// Base snapshot setup: PHP 8.3-8.5 (Sury) + Composer with the composer-fix and
/// soak-time supply-chain plugins, npm with min-release-age, `n` for other Node
/// majors, and tcpdump for in-guest network capture.
pub const BASE_SETUP: &str = r#"
set -e
export DEBIAN_FRONTEND=noninteractive
echo ">> adding Sury PHP repository (Debian ships only 8.2; we want 8.3-8.5)"
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl lsb-release git unzip procps tcpdump
curl -fsSLo /tmp/sury-keyring.deb https://packages.sury.org/debsuryorg-archive-keyring.deb
dpkg -i /tmp/sury-keyring.deb
rm -f /tmp/sury-keyring.deb
echo "deb [signed-by=/usr/share/keyrings/deb.sury.org-php.gpg] https://packages.sury.org/php/ $(lsb_release -sc) main" > /etc/apt/sources.list.d/php.list
apt-get update
echo ">> installing PHP 8.3, 8.4 and 8.5 plus tooling (this is the slow part)"
for v in 8.3 8.4 8.5; do
  apt-get install -y --no-install-recommends \
    php$v-cli php$v-mbstring php$v-xml php$v-curl php$v-sqlite3 php$v-zip php$v-bcmath php$v-intl \
    php$v-mysql php$v-pgsql
done
update-alternatives --set php /usr/bin/php8.4 || true
echo ">> installing Composer"
php -r "copy('https://getcomposer.org/installer', '/tmp/composer-setup.php');"
php /tmp/composer-setup.php --install-dir=/usr/local/bin --filename=composer --quiet
rm -f /tmp/composer-setup.php
echo 'export COMPOSER_ALLOW_SUPERUSER=1' > /etc/profile.d/composer.sh
export COMPOSER_ALLOW_SUPERUSER=1
echo ">> installing composer-fix (updates packages flagged by composer audit)"
composer global config allow-plugins.innobrain/composer-fix true
# Exempt our own plugins from the 7-day soak window: a fresh composer-fix
# release is by definition younger than the cooldown, and this global install is
# a boxme-controlled dependency, not project-supplied supply chain. Scoped to
# the two packages by name — the window still applies to everything else.
composer global config --json extra.soak-time-whitelist '["innobrain/composer-fix","innobrain/soak-time"]'
# Pinned to the 3.x line: the fleet-fix skill relies on `--no-fail` (2.0) and
# the skip-unfixable-instead-of-fail behavior (3.0), and a future major could
# change the exit contract again.
composer global require innobrain/composer-fix:^3.0 --no-interaction
echo ">> installing composer soak-time (supply-chain safety: blocks deps younger than 7 days)"
composer global config allow-plugins.innobrain/soak-time true
# The plugin can't observe its own dist download (it isn't loaded yet), so its
# integrity recorder refuses to self-pin. Install from source as it instructs.
composer global require innobrain/soak-time --prefer-source --no-interaction
echo ">> upgrading npm (need >= 11.10.0 for min-release-age cooldown)"
npm install -g npm@latest
echo ">> installing Claude Code (@anthropic-ai/claude-code) for \`boxme claude\`"
# npm >= 12 blocks install scripts by default; claude-code's postinstall is what
# downloads the native `claude` binary, so without this the install "succeeds"
# but `claude` bails with "native binary not installed".
npm install -g --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code
echo ">> enabling npm min-release-age (supply-chain safety: 7-day cooldown on new packages)"
npm config set min-release-age 7 --location=global
echo ">> installing n (Node version switcher; downloads land on the boxme-node-versions volume)"
npm install -g n
mkdir -p /root/.n
printf 'export N_PREFIX=/root/.n\nexport PATH=/root/.n/bin:$PATH\n' > /etc/profile.d/n.sh
echo ">> versions:"
for v in 8.3 8.4 8.5; do php$v -v | head -1; done
echo "default: $(php -v | head -1)"
composer --version
echo "node $(node -v) / npm $(npm -v)"
echo "npm min-release-age: $(npm config get min-release-age --location=global) day(s)"
echo "claude $(claude --version 2>/dev/null || echo '(not installed)')"
echo ">> base image ready"
"#;

/// Mount the project (read-only bind at `/ws-lower`) as the lower layer of an
/// overlay at `/workspace`, then tag a git baseline so changes can be diffed out
/// later. Nothing is copied in: reads fall through to the host tree via
/// virtiofs, writes land in the upper layer, and the host stays untouched. The
/// user's uncommitted state is exactly what the command should operate on.
///
/// The upper/work dirs can't live on the guest root — that root is itself an
/// overlayfs, which the kernel refuses as an overlay upperdir
/// (`not supported as upperdir`). A sparse loop-mounted ext4, sized to the space
/// free on the guest root, gives a disk-backed upper that overlay accepts with
/// no RAM cost (unlike tmpfs, which a clean install would overrun).
///
/// The baseline is built in an **isolated git dir** (`GIT_DIR=/boxme-git`,
/// work-tree `/workspace`), never the project's own `.git`. The host repo is
/// mounted read-only as the overlay lower, so reusing its object store would
/// make the in-guest `git add` read the host packfiles — which fails outright on
/// a partial/corrupt/otherwise-unreadable pack (`not a GIT packfile`) and aborts
/// the mount. A fresh store sidesteps that entirely: boxme only needs a snapshot
/// of the working tree to diff against, not the user's history. `.git` is
/// excluded from the add so git can't treat it as a submodule and resolve its
/// HEAD out of that same unreadable store.
///
/// `core.hooksPath=/dev/null` disables any hooks shipped in the project (incl.
/// husky, which also drives them via core.hooksPath) so committing the baseline
/// can't run project code; `--no-verify` is the belt-and-suspenders. `add -Af`
/// forces .gitignore'd files (e.g. `.env`) into the baseline so that if package
/// code later modifies one, `git diff` against the baseline still shows it.
/// `vendor`/`node_modules` are excluded from the baseline: the file review
/// prunes them anyway, and indexing them would copy the whole dep tree up into
/// the overlay for nothing. The baseline lives only in the guest, never copied
/// back.
pub const UNPACK: &str = r#"
set -e
if ! command -v mkfs.ext4 >/dev/null 2>&1; then
  echo ">> mkfs.ext4 missing from the base image — rebuild with \`boxme setup --force\`" >&2
  exit 1
fi
avail=$(df -BG --output=avail / 2>/dev/null | tail -1 | tr -dc '0-9')
[ -z "$avail" ] && avail=32
[ "$avail" -gt 4 ] && avail=$((avail - 2))
truncate -s "${avail}G" /boxme-upper.img
mkfs.ext4 -qF -O ^has_journal -E lazy_itable_init=1 -m 0 /boxme-upper.img
mkdir -p /boxme-upper /workspace
mount -o loop /boxme-upper.img /boxme-upper
mkdir -p /boxme-upper/upper /boxme-upper/work
mount -t overlay overlay -o lowerdir=/ws-lower,upperdir=/boxme-upper/upper,workdir=/boxme-upper/work /workspace
cd /workspace
export GIT_DIR=/boxme-git GIT_WORK_TREE=/workspace
git config --global --add safe.directory /workspace
git config --global core.hooksPath /dev/null
git init -q
git add -Af -- . ':(exclude).git' ':(exclude)vendor' ':(exclude)node_modules'
git -c user.email=boxme@local -c user.name=boxme commit --no-verify -qm "boxme baseline" || true
git tag -f boxme-baseline HEAD >/dev/null
echo ">> project mounted (overlay), baseline tagged"
"#;

/// Extract the host tarball into /workspace for a `dev` session. Unlike
/// `UNPACK`, no git baseline is tagged: a dev run never diffs or copies back, so
/// the (potentially slow on a big repo) baseline commit is pure overhead.
pub const DEV_UNPACK: &str = r#"
set -e
mkdir -p /workspace
tar --no-same-owner -xzf /tmp/repo.tgz -C /workspace
rm -f /tmp/repo.tgz
echo ">> project unpacked into /workspace"
"#;

/// Install composer dependencies in the guest before the dev stack runs. The
/// install happens *in the guest*, so it pulls the Linux-native artifacts the
/// guest will actually run — no host-platform retargeting (the opposite of the
/// copy-back `npm install` path). Dev dependencies are kept: `composer run dev`
/// needs pail/sail/etc.
pub const COMPOSER_INSTALL: &str = "cd /workspace && composer install --no-interaction";

/// Install npm dependencies in the guest before the dev stack runs. Linux-native
/// by design — node_modules stays in the guest and is executed there, never
/// copied back to the host.
pub const NPM_INSTALL: &str = "cd /workspace && npm install";

/// Bridge each forwarded guest port onto the guest's external interface so a dev
/// server that binds only `127.0.0.1` (artisan serve and Vite both do by
/// default) is still reachable through microsandbox's host→guest port forward,
/// which dials the guest's eth0 IP. The proxy binds `<eth0-ip>:PORT`, which does
/// not collide with the app's `127.0.0.1:PORT`; if a server already binds
/// `0.0.0.0:PORT` the proxy's bind fails harmlessly (`.on('error')`) since the
/// port is reachable directly. Ports come from a host-parsed `u16` list, never
/// guest input. Started in the background; they die with the VM.
pub fn port_bridge(ports: &[u16]) -> String {
    let mut script = String::from(
        r#"cat > /tmp/boxme-proxy.js <<'PROXY'
const net = require('net');
const port = Number(process.argv[2]);
const ip = process.argv[3];
net.createServer((s) => {
  const u = net.connect(port, '127.0.0.1');
  s.pipe(u);
  u.pipe(s);
  const done = () => { s.destroy(); u.destroy(); };
  s.on('error', done);
  u.on('error', done);
}).on('error', () => {}).listen(port, ip);
PROXY
GUEST_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
if [ -n "$GUEST_IP" ]; then
"#,
    );
    for p in ports {
        script.push_str(&format!(
            "  node /tmp/boxme-proxy.js {p} \"$GUEST_IP\" >/dev/null 2>&1 &\n"
        ));
    }
    script.push_str("fi\n");
    script
}

/// First-run config seeded into the fresh guest before Claude Code launches.
///
/// A valid token authenticates the agent, but on a brand-new `/root/.claude.json`
/// Claude Code still runs the first-run wizard (theme + login) and the per-project
/// "trust this folder" dialog regardless of the token — which is what lands the
/// user on a setup/login screen. These keys mark onboarding complete and
/// `/workspace` trusted so the session opens straight to a usable prompt.
/// `--dangerously-skip-permissions` notably does *not* suppress the trust dialog
/// (upstream bug), hence the explicit project entry. `lastOnboardingVersion` is
/// set to the guest's actual claude version so an onboarding-flow bump can still
/// re-trigger it intentionally. The auto-mode opt-in keys pre-accept the prompt
/// `--permission-mode auto` can show on first use (best-effort: harmless if the
/// real keys differ). Written fresh each boot since the guest is throwaway.
const CLAUDE_SEED: &str = r#"
mkdir -p /root/.claude
CLAUDE_VERSION="$(claude --version 2>/dev/null | grep -oE '[0-9]+(\.[0-9]+)+' | head -n1)"
cat > /root/.claude.json <<EOF
{
  "hasCompletedOnboarding": true,
  "lastOnboardingVersion": "${CLAUDE_VERSION:-2.1.185}",
  "theme": "dark",
  "hasResetAutoModeOptInForDefaultOffer": true,
  "autoPermissionsNotificationCount": 99,
  "projects": {
    "/workspace": {
      "hasTrustDialogAccepted": true,
      "hasTrustDialogHooksAccepted": true,
      "hasCompletedProjectOnboarding": true
    }
  }
}
EOF
cat > /root/.claude/settings.json <<EOF
{
  "skipDangerousModePermissionPrompt": true
}
EOF
"#;

/// Launch Claude Code inside the guest against the mounted `/workspace`.
///
/// Permission mode is chosen per path. An **interactive** session (no prompt) runs
/// under `--permission-mode auto`: it's attached to the user's TTY, so auto mode's
/// classifier is a safety net the user can answer, and it needs no root-bypass and
/// shows no bypass-mode warning screen. A **headless** run (`-p`) is unattended, so
/// it uses `--dangerously-skip-permissions` — auto mode would *abort* a headless
/// session when its classifier blocks a legitimate action (a postinstall script, a
/// git reset), and the sandbox is already the safety boundary. bypass refuses to
/// run as root unless `IS_SANDBOX=1`, which auto mode tolerates but doesn't need.
///
/// `DISABLE_AUTOUPDATER` pins the baked claude version, and
/// `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` drops telemetry/error reporting.
/// `CLAUDE_SEED` runs first so the agent skips onboarding and lands in a session.
pub fn claude_run(prompt: Option<&str>) -> String {
    let invocation = match prompt {
        Some(p) => format!(
            "claude --dangerously-skip-permissions -p {}",
            shell_quote(p)
        ),
        None => "claude --permission-mode auto".to_string(),
    };
    format!(
        "{RAISE_FDS}\n\
         {CLAUDE_SEED}\n\
         export IS_SANDBOX=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1\n\
         cd /workspace && exec {invocation}"
    )
}

/// Switch the guest's `php` alternative to the requested version. The version
/// is interpolated into shell, but it's validated against `PHP_VERSIONS` first.
pub fn php_switch(version: &str) -> String {
    format!(
        r#"
if [ ! -x /usr/bin/php{version} ]; then
  echo ">> PHP {version} is not in the base image — run \`boxme setup --force\` (it installs 8.3-8.5)"
  exit 1
fi
update-alternatives --set php /usr/bin/php{version}
update-alternatives --set phar /usr/bin/phar{version} 2>/dev/null || true
update-alternatives --set phar.phar /usr/bin/phar.phar{version} 2>/dev/null || true
echo ">> using $(php -v | head -n 1)"
"#
    )
}

/// Install + activate a Node major via `n`. First time downloads (cached on
/// the boxme-node-versions volume), afterwards instant.
pub fn node_switch(major: u32) -> String {
    format!(
        r#"
set -e
echo ">> switching to Node {major} via n"
n install {major}
echo ">> using node $(node -v)"
"#
    )
}

/// Raise the open-file-descriptor limit before running the user's command. The
/// base image inherits a low default (often 1024) and npm — `npm audit` and big
/// installs especially — opens enough sockets and files at once to hit `EMFILE:
/// too many open files`. The guest runs as root, so it may raise the hard limit
/// too (a bare `ulimit -n` sets both); the fallback covers a lower kernel cap on
/// `fs.nr_open`, and `|| true` keeps a refusal from aborting the command.
pub const RAISE_FDS: &str = "ulimit -n 1048576 2>/dev/null || ulimit -n 65536 2>/dev/null || true";

/// Raise the inotify watch ceiling before a `dev` stack starts. Vite/chokidar
/// recursively watch /workspace (vendor/ and node_modules/ included), and the
/// Linux default `fs.inotify.max_user_watches` (8192) is far too low for a real
/// Laravel + node tree — Vite aborts with `ENOSPC: System limit for number of
/// file watchers reached`. The guest is root, so it can raise the sysctls; `||
/// true` keeps a refusal (e.g. a read-only /proc/sys) from aborting the run.
pub const RAISE_INOTIFY: &str = "sysctl -w fs.inotify.max_user_watches=524288 fs.inotify.max_user_instances=1024 >/dev/null 2>&1 || true";

/// Point npm's optional-dependency resolver at the host platform instead of the
/// Linux guest it actually runs in. npm only installs the platform-gated
/// optional deps (`@esbuild/*`, `@rollup/rollup-*`, `lightningcss`, `@swc/core`,
/// `sharp`'s prebuilds, …) whose `os`/`cpu` match where npm runs — so a guest
/// install drops the macOS binaries the host will try to load. `os`/`cpu` come
/// from a fixed host-side whitelist (`darwin` + `arm64`/`x64`), never guest
/// input, so they need no quoting.
pub fn npm_platform_env(os: &str, cpu: &str) -> String {
    format!("export npm_config_os={os} npm_config_cpu={cpu}; ")
}

/// File manifest of /workspace: every entry's size/type/path, then an md5 of
/// every regular file (content hashing kills mtime noise). vendor/,
/// node_modules/ and .git are excluded — the expected dirs are summarized by
/// count separately.
///
/// Everything is NUL-delimited and the path is the last field of each record,
/// so filenames containing tabs or newlines can't inject or truncate manifest
/// lines (a guest-controlled name must not be able to forge a review entry).
/// `md5sum -z` likewise NUL-terminates and disables its backslash-escaping of
/// special characters in names.
///
/// Per-file stat/hash failures are non-fatal: stderr is dropped and the script
/// always exits 0. A volatile runtime file (e.g. Statamic's `storage` stache
/// cache, which a still-running host process can rewrite mid-run) can return
/// `ESTALE`/"Stale file handle" through the virtiofs overlay lower; letting
/// `find`/`md5sum`'s non-zero exit propagate here would abort the whole run
/// *after* the command finished, losing the review and any copy-back. A file
/// that lists but won't hash just arrives with no md5, and `manifest::diff`
/// already falls back to size comparison for those.
pub const MANIFEST: &str = r#"
cd /workspace
printf '#FILES\0'
find /workspace -mindepth 1 \( -path /workspace/.git -o -path /workspace/vendor -o -path /workspace/node_modules \) -prune -o -printf '%s\t%y\t%P\0' 2>/dev/null
printf '#MD5\0'
find /workspace -mindepth 1 \( -path /workspace/.git -o -path /workspace/vendor -o -path /workspace/node_modules \) -prune -o -type f -print0 2>/dev/null | xargs -0 -r md5sum -z 2>/dev/null
exit 0
"#;

/// Count files inside an expected dir (for the "vendor/: 4321 files" summary).
pub fn count_files(dir: &str) -> String {
    format!("find /workspace/{dir} -type f 2>/dev/null | wc -l")
}

/// Marker whose mtime is the cutoff for the out-of-workspace change scan.
/// Touched after setup (unpack, version switch) but before the command, so the
/// scan reports only what the command itself wrote outside /workspace.
pub const BASELINE_MARKER: &str = "/root/.boxme-outside-baseline";

/// Files changed anywhere outside /workspace since the baseline marker. The test
/// is `-newercm`: a file's inode-change time (ctime) is newer than the marker's
/// mtime. ctime can't be backdated with `touch -t` from inside the guest, so
/// this also catches chmod/chown/rename and binary replacement, not just writes.
///
/// `-xdev` keeps the walk on the rootfs, which already skips the separate-device
/// mounts — the cache volumes (/root/.composer/cache, /root/.npm, /root/.n), the
/// read-only project bind (/ws-lower), the overlay (/workspace) and its writable
/// upper (the /boxme-upper loop mount) — plus the pseudo-fs. The explicit prunes
/// drop /workspace (covered by the Files tab), the scratch and cache dirs
/// composer/npm legitimately churn every run, `/boxme-upper.img` — the loop
/// image backing the overlay upper, a plain rootfs file whose ctime bumps on
/// every overlay write, so it would otherwise show up as a multi-GB outside
/// write every run — `/boxme-git`, the isolated baseline git dir (boxme's own
/// machinery, written during the mount) — and the VM's own boot plumbing:
/// agentd writes /etc/hosts, /etc/hostname and /etc/resolv.conf on its own
/// schedule, which races the baseline marker, and `/.msb` is the agent's
/// runtime dir. When TLS interception is on (`--composer-auth`) the agent also
/// installs the proxy CA into the guest trust store, so those paths are pruned
/// too — but only then, so a rogue CA install by a postinstall script still
/// shows up in a normal run. Trading a little blind spot in those for signal
/// in the rest of the tree. Output is `size\ttype\tpath`; a missing marker
/// prints `#NOMARKER`.
pub fn outside_scan(tls_intercepting: bool) -> String {
    let ca_prunes = if tls_intercepting {
        " -o -path /etc/ssl/certs -o -path /usr/local/share/ca-certificates \\
     -o -path /etc/ca-certificates.conf -o -path /etc/ca-certificates"
    } else {
        ""
    };
    format!(
        r#"marker={BASELINE_MARKER}
if [ ! -e "$marker" ]; then echo '#NOMARKER'; exit 0; fi
find / -xdev -mindepth 1 \
  \( -path /workspace -o -path /boxme-upper -o -path /boxme-upper.img \
     -o -path /boxme-git -o -path /.msb \
     -o -path /etc/hosts -o -path /etc/hostname -o -path /etc/resolv.conf{ca_prunes} \
     -o -path /proc -o -path /sys -o -path /dev -o -path /run \
     -o -path /tmp -o -path /var/tmp -o -path /var/log -o -path /var/cache \
     -o -path /var/lib/apt -o -path /root/.cache -o -path /root/.npm \
     -o -path /root/.composer -o -path /root/.n \) -prune -o \
  \( -newercm "$marker" -a \( -type f -o -type l \) -printf '%s\t%y\t%p\n' \) 2>/dev/null
"#
    )
}

/// tcpdump capturing DNS + outbound TCP SYNs. `-U` flushes per packet so a
/// SIGTERM loses nothing; `exec` so killing the exec kills tcpdump itself.
pub const TCPDUMP_START: &str =
    "exec tcpdump -i any -n -U -w /tmp/cap.pcap '(udp port 53) or (tcp[tcpflags] & tcp-syn != 0)' 2>/dev/null";

/// Parse the capture in the guest — no host pcap dependency.
pub const TCPDUMP_PARSE: &str = "tcpdump -r /tmp/cap.pcap -n 2>/dev/null || true";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_uses_auto_headless_uses_bypass() {
        let interactive = claude_run(None);
        assert!(interactive.contains("claude --permission-mode auto"));
        assert!(!interactive.contains("--dangerously-skip-permissions"));

        let headless = claude_run(Some("fix it"));
        assert!(headless.contains("claude --dangerously-skip-permissions -p"));
        assert!(headless.contains("fix it"));

        // Both seed onboarding + workspace trust before launching.
        for script in [&interactive, &headless] {
            assert!(script.contains("\"hasCompletedOnboarding\": true"));
            assert!(script.contains("\"/workspace\""));
            assert!(script.contains("\"hasTrustDialogAccepted\": true"));
        }
    }

    #[test]
    fn outside_scan_prunes_boot_plumbing_and_gates_ca_store() {
        // agentd writes these on its own schedule — always pruned.
        for scan in [outside_scan(false), outside_scan(true)] {
            for path in ["/etc/hosts", "/etc/hostname", "/etc/resolv.conf", "/.msb"] {
                assert!(scan.contains(&format!("-path {path}")), "{path} not pruned");
            }
        }
        // The trust store is pruned only under TLS interception, so a rogue CA
        // install still shows up in a normal run.
        assert!(outside_scan(true).contains("-path /etc/ssl/certs"));
        assert!(!outside_scan(false).contains("-path /etc/ssl/certs"));
    }
}
