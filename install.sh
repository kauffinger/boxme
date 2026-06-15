#!/bin/sh
# boxme installer — downloads a prebuilt binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/kauffinger/boxme/main/install.sh | sh
#
# Install a specific version:
#   curl -fsSL .../install.sh | sh -s -- v0.1.0
#
# Knobs (environment variables):
#   BOXME_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   BOXME_VERSION       tag to install (default: latest; arg overrides this)
set -eu

REPO="kauffinger/boxme"
BIN="boxme"

info() { printf '%s\n' "$*" >&2; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "required tool not found: $1"; }
need uname
need tar
need mkdir

# Prefer curl, fall back to wget.
if command -v curl >/dev/null 2>&1; then
  http_to_stdout() { curl -fsSL "$1"; }
  http_to_file() { curl -fsSL -o "$2" "$1"; }
  # Follow redirects with a HEAD request and print the final URL.
  final_url() { curl -fsSLI -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  http_to_stdout() { wget -qO- "$1"; }
  http_to_file() { wget -qO "$2" "$1"; }
  final_url() { wget -q --max-redirect=10 -S -O /dev/null "$1" 2>&1 | awk '/^  Location: /{u=$2} END{print u}'; }
else
  err "need either curl or wget installed"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      *) err "boxme requires an Apple Silicon Mac; got macOS/$arch (Intel is not supported by microsandbox)" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
      *) err "unsupported Linux architecture: $arch" ;;
    esac
    ;;
  *)
    err "unsupported OS: $os (boxme supports macOS and Linux)"
    ;;
esac

# Resolve the version. Argument wins, then BOXME_VERSION, then "latest".
version="${1:-${BOXME_VERSION:-latest}}"
if [ "$version" = "latest" ]; then
  info "Resolving latest release..."
  resolved="$(final_url "https://github.com/$REPO/releases/latest")"
  tag="${resolved##*/}"
  [ -n "$tag" ] && [ "$tag" != "latest" ] || err "could not determine the latest release tag (is there a published release yet?)"
else
  tag="$version"
fi

tarball="$BIN-$target.tar.gz"
base_url="https://github.com/$REPO/releases/download/$tag"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "Downloading $tarball ($tag)..."
http_to_file "$base_url/$tarball" "$tmp/$tarball" || err "download failed: $base_url/$tarball"

# Verify the checksum when the .sha256 sidecar is available.
if http_to_file "$base_url/$tarball.sha256" "$tmp/$tarball.sha256" 2>/dev/null; then
  expected="$(awk '{print $1}' "$tmp/$tarball.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$tarball" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$tarball" | awk '{print $1}')"
  else
    actual=""
    info "warning: no sha256 tool found; skipping checksum verification"
  fi
  if [ -n "$actual" ]; then
    [ "$expected" = "$actual" ] || err "checksum mismatch for $tarball (expected $expected, got $actual)"
    info "Checksum verified."
  fi
else
  info "warning: no checksum file published for $tag; skipping verification"
fi

tar -xzf "$tmp/$tarball" -C "$tmp"
[ -f "$tmp/$BIN" ] || err "archive did not contain a '$BIN' binary"
chmod +x "$tmp/$BIN"

install_dir="${BOXME_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
mv -f "$tmp/$BIN" "$install_dir/$BIN"

info ""
info "Installed boxme $tag to $install_dir/$BIN"

case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    info ""
    info "$install_dir is not on your PATH. Add it, e.g.:"
    info "  echo 'export PATH=\"$install_dir:\$PATH\"' >> ~/.zshrc && exec \$SHELL"
    ;;
esac

info ""
info "Next: build the base snapshot once (~10 min):"
info "  boxme setup"
info ""
info "boxme needs hardware virtualization — Apple Silicon, or Linux with KVM."
