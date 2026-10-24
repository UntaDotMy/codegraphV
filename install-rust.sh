#!/bin/sh
# Install the Rust codegraph from a GitHub Release (Linux x64, macOS arm64 / x64).
#
#   curl -fsSL https://raw.githubusercontent.com/UntaDotMy/codegraphV/rust-rewrite/install-rust.sh | sh
#
# Options:
#   CODEGRAPH_VERSION      release tag to install, for example v0.0.1 (default: latest)
#   CODEGRAPH_INSTALL_DIR  where codegraph goes (default: ~/.local/bin)
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

fetch() { if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"; else wget -qO "$2" "$1"; fi; }

# "latest" comes from the releases/latest web redirect first: the unauthenticated API is
# limited to 60 requests an hour per IP.
version=${CODEGRAPH_VERSION:-}
if [ -z "$version" ] && command -v curl >/dev/null 2>&1; then
  version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest" | sed -n 's#.*/releases/tag/##p')
fi
if [ -z "$version" ]; then
  tmpjson=$(mktemp)
  fetch "https://api.github.com/repos/$repo/releases/latest" "$tmpjson"
  version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmpjson" | head -n 1)
  rm -f "$tmpjson"
fi
[ -n "$version" ] || { echo "Could not find the latest release; set CODEGRAPH_VERSION (for example v0.0.1)." >&2; exit 1; }
case "$version" in v*) ;; *) version="v$version" ;; esac
dir=${CODEGRAPH_INSTALL_DIR:-$HOME/.local/bin}
name="codegraph-$version-$os-$arch"
base="https://github.com/$repo/releases/download/$version"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading codegraph $version..."
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
mkdir -p "$dir"
# Replace by rename so a running codegraph (an MCP server) keeps its old file.
cp "$tmp/$name/codegraph" "$dir/.codegraph.new"
chmod +x "$dir/.codegraph.new"
mv -f "$dir/.codegraph.new" "$dir/codegraph"

echo "Installed $("$dir/codegraph" --version) to $dir/codegraph"
case ":$PATH:" in
  *":$dir:"*)
    # Another codegraph earlier on PATH (often the npm package) would run instead.
    first=$(command -v codegraph || true)
    if [ -n "$first" ] && [ "$first" != "$dir/codegraph" ]; then
      echo "Warning: $first comes first on PATH and runs instead of this install."
      echo "Remove it (for the npm package: npm rm -g @colbymchenry/codegraph) or put $dir first on PATH."
    fi
    ;;
  *) echo "Add $dir to your PATH, for example: echo 'export PATH=\"$dir:\$PATH\"' >> ~/.profile" ;;
esac
echo "Next: run 'codegraph install' to connect Claude Code, Cursor, Codex or opencode."
