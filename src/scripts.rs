//! All shell run inside the guest, in one place.

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
composer global require innobrain/composer-fix --no-interaction
echo ">> installing composer soak-time (supply-chain safety: blocks deps younger than 7 days)"
composer global config allow-plugins.innobrain/soak-time true
# The plugin can't observe its own dist download (it isn't loaded yet), so its
# integrity recorder refuses to self-pin. Install from source as it instructs.
composer global require innobrain/soak-time --prefer-source --no-interaction
echo ">> upgrading npm (need >= 11.10.0 for min-release-age cooldown)"
npm install -g npm@latest
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
pub const MANIFEST: &str = r#"
cd /workspace
printf '#FILES\0'
find /workspace -mindepth 1 \( -path /workspace/.git -o -path /workspace/vendor -o -path /workspace/node_modules \) -prune -o -printf '%s\t%y\t%P\0'
printf '#MD5\0'
find /workspace -mindepth 1 \( -path /workspace/.git -o -path /workspace/vendor -o -path /workspace/node_modules \) -prune -o -type f -print0 | xargs -0 -r md5sum -z
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
/// write every run — and `/boxme-git`, the isolated baseline git dir (boxme's
/// own machinery, written during the mount). Trading a little blind spot in
/// those for signal in the rest of the tree. Output is `size\ttype\tpath`; a
/// missing marker prints `#NOMARKER`.
pub fn outside_scan() -> String {
    format!(
        r#"marker={BASELINE_MARKER}
if [ ! -e "$marker" ]; then echo '#NOMARKER'; exit 0; fi
find / -xdev -mindepth 1 \
  \( -path /workspace -o -path /boxme-upper -o -path /boxme-upper.img \
     -o -path /boxme-git \
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
