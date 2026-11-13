#!/bin/sh
# Install codegraphV (the Rust rewrite of codegraph) from a GitHub Release (Linux x64, macOS arm64 / x64).
#
#   curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
#
# Options:
#   CODEGRAPHV_VERSION      release tag to install, for example v0.0.2 (default: latest)
#   CODEGRAPHV_INSTALL_DIR  where codegraphV goes (default: ~/.local/bin)
# The download is checked against the release's SHA256SUMS; temporary files are deleted.
set -eu
repo=UntaDotMy/codegraphV

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "Unsupported OS $(uname -s); on Windows use install-rust.ps1." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) echo "Unsupported CPU $(uname -m)." >&2; exit 1 ;;
esac
if [ "$os-$arch" = linux-arm64 ]; then
  echo "No Linux arm64 build yet; build from source with: cargo build --release -p codegraph-cli" >&2
  exit 1
fi

# Try curl, then wget: a host can have curl installed but blocked from the release host.
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2" && return 0
  fi
  if command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1" && return 0
  fi
  echo "Could not download $1" >&2
  return 1
}

# "latest" comes from the releases/latest web redirect first: the unauthenticated API is
# limited to 60 requests an hour per IP.
# `codegraph upgrade` of codegraph v0.0.1 (before the rename) runs this script with the old
# variable names. It gets codegraphV, then a migrate of its setup.
legacy=
if [ -n "${CODEGRAPH_INSTALL_DIR:-}" ] && [ -z "${CODEGRAPHV_INSTALL_DIR:-}" ]; then legacy=1; fi
version=${CODEGRAPHV_VERSION:-}
if [ -z "$version" ] && [ -n "$legacy" ]; then version=${CODEGRAPH_VERSION:-}; fi
if [ -z "$version" ] && command -v curl >/dev/null 2>&1; then
  # A failed request must fall through to the API; a plain assignment would end the script
  # under `set -e` when the host is offline or redirects are blocked.
  version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest" 2>/dev/null | sed -n 's#.*/releases/tag/##p') || version=
fi
if [ -z "$version" ]; then
  tmpjson=$(mktemp)
  fetch "https://api.github.com/repos/$repo/releases/latest" "$tmpjson"
  version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmpjson" | head -n 1)
  rm -f "$tmpjson"
fi
[ -n "$version" ] || { echo "Could not find the latest release; set CODEGRAPHV_VERSION (for example v0.0.2)." >&2; exit 1; }
case "$version" in v*) ;; *) version="v$version" ;; esac
dir=${CODEGRAPHV_INSTALL_DIR:-$HOME/.local/bin}
name="codegraphV-$version-$os-$arch"
base="https://github.com/$repo/releases/download/$version"

tmp=$(mktemp -d)
mkdir -p "$dir"
# One installer at a time. A crashed run leaves the lock; take it over after ten minutes.
lock="$dir/.codegraphV.install.lock"
if ! mkdir "$lock" 2>/dev/null; then
  if [ -n "$(find "$lock" -mmin +10 2>/dev/null)" ]; then
    rmdir "$lock" 2>/dev/null || true
  fi
  if ! mkdir "$lock" 2>/dev/null; then
    echo "Another codegraphV install is in progress ($lock). Retry when it finishes." >&2
    rm -rf "$tmp"
    exit 1
  fi
fi
trap 'rm -rf "$tmp"; rmdir "$lock" 2>/dev/null || true' EXIT
echo "Downloading codegraphV $version..."
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
expected=$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)
fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "Checksum mismatch for $name.tar.gz; nothing was installed." >&2
  exit 1
fi
tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
# Replace by rename so a running codegraphV (an MCP server) keeps its old file. The stage name
# is unique per process, so concurrent installers never share or overwrite a partial download.
stage=$(mktemp "$dir/.codegraphV.new.XXXXXX")
cp "$tmp/$name/codegraphV" "$stage"
chmod +x "$stage"
mv -f "$stage" "$dir/codegraphV"

echo "Installed $("$dir/codegraphV" --version) to $dir/codegraphV"
case ":$PATH:" in
  *":$dir:"*)
    # Another codegraphV earlier on PATH would run instead.
    first=$(command -v codegraphV || true)
    if [ -n "$first" ] && [ "$first" != "$dir/codegraphV" ]; then
      echo "Warning: $first comes first on PATH and runs instead of this install."
      echo "Remove it or put $dir first on PATH."
    fi
    ;;
  *) echo "Add $dir to your PATH, for example: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
esac
if [ -n "$legacy" ]; then
  echo "codegraph is now called codegraphV. Moving your agents and indexes over (codegraphV migrate)..."
  if ! "$dir/codegraphV" migrate --yes; then
    echo "Warning: migration did not complete. Run 'codegraphV migrate' to retry." >&2
  fi
  echo "From now on run 'codegraphV'. The old 'codegraph upgrade' may now report that codegraph does not report $version; that is expected, because codegraphV replaced it."
# The original codegraph (npm @colbymchenry/codegraph, or codegraph v0.0.1 of this fork).
elif command -v codegraph >/dev/null 2>&1; then
  echo "Found the original codegraph. Next: run 'codegraphV migrate' to move your agents and indexes over."
else
  echo "Next: run 'codegraphV install' to connect Claude Code, Cursor, Codex or opencode."
fi
