//! All shell run inside the guest, in one place.

/// Versions of PHP installed side by side in the base snapshot. Keep in sync
/// with the loop in `BASE_SETUP`.
pub const PHP_VERSIONS: &[&str] = &["8.3", "8.4", "8.5"];

/// The version the guest gets when detection finds nothing.
pub const DEFAULT_PHP_VERSION: &str = "8.4";

/// Node major shipped by the node:24 base image — no `n install` needed for it.
pub const BASE_NODE_MAJOR: u32 = 24;

/// Base snapshot setup: PHP 8.3-8.5 (Sury) + Composer with the soak-time
/// supply-chain plugin, npm with min-release-age, `n` for other Node majors,
/// and tcpdump for in-guest network capture.
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

/// Extract the host tarball and tag a git baseline so changes can be diffed
/// out later. Unlike microphp there is no reset-to-last-commit: the user's
/// uncommitted state is exactly what the command should operate on.
///
/// `core.hooksPath=/dev/null` disables any hooks shipped in the project (incl.
/// husky, which also drives them via core.hooksPath) so committing the baseline
/// can't run project code; `--no-verify` is the belt-and-suspenders. `add -Af`
/// forces .gitignore'd files (e.g. `.env`) into the baseline so that if package
/// code later modifies one, `git diff` against the baseline still shows it — the
/// baseline lives only in the guest and is never copied back.
pub const UNPACK: &str = r#"
set -e
mkdir -p /workspace
tar --no-same-owner -xzf /tmp/repo.tgz -C /workspace
rm -f /tmp/repo.tgz
cd /workspace
git config --global --add safe.directory /workspace
git config --global core.hooksPath /dev/null
if ! git rev-parse --git-dir >/dev/null 2>&1; then git init -q; fi
git add -Af
git -c user.email=boxme@local -c user.name=boxme commit --no-verify -qm "boxme baseline" || true
git tag -f boxme-baseline HEAD >/dev/null
echo ">> repo unpacked, baseline tagged"
"#;

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
/// `-xdev` keeps the walk on the rootfs, which already skips the mounted cache
/// volumes (/root/.composer/cache, /root/.npm, /root/.n) and the pseudo-fs. The
/// explicit prunes drop /workspace (covered by the Files tab) plus the scratch
/// and cache dirs composer/npm legitimately churn every run; trading a little
/// blind spot in those for signal in the rest of the tree. Output is
/// `size\ttype\tpath`; a missing marker prints `#NOMARKER`.
pub fn outside_scan() -> String {
    format!(
        r#"marker={BASELINE_MARKER}
if [ ! -e "$marker" ]; then echo '#NOMARKER'; exit 0; fi
find / -xdev -mindepth 1 \
  \( -path /workspace -o -path /proc -o -path /sys -o -path /dev -o -path /run \
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
