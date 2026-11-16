#!/bin/sh
# Platform checks of install-rust.sh. uname, curl and wget are stubbed on PATH, and curl and wget
# always fail, so nothing is downloaded and the script's own messages are what is compared.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
installer="$here/../install-rust.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat > "$work/bin/uname" <<'STUB'
#!/bin/sh
case "$1" in
  -s) echo "$STUB_OS" ;;
  -m) echo "$STUB_ARCH" ;;
esac
STUB
for tool in curl wget; do
  printf '#!/bin/sh\nexit 22\n' > "$work/bin/$tool"
done
chmod +x "$work/bin/uname" "$work/bin/curl" "$work/bin/wget"

failures=0
# check <label> <os> <arch> <text the output must contain> <text the output must not contain>
check() {
  set +e
  out=$(STUB_OS="$2" STUB_ARCH="$3" PATH="$work/bin:$PATH" TMPDIR="$work" CODEGRAPHV_VERSION=v0.0.13 \
    CODEGRAPHV_INSTALL_DIR="$work/install" sh "$installer" 2>&1)
  status=$?
  set -e
  if [ "$status" -eq 1 ] && printf '%s' "$out" | grep -q -- "$4" \
    && { [ -z "$5" ] || ! printf '%s' "$out" | grep -q -- "$5"; }; then
    echo "ok   $1"
  else
    echo "FAIL $1: exit $status, output: $out"
    failures=$((failures + 1))
  fi
}

check "macOS arm64 is refused, naming Apple Silicon" Darwin arm64 "Apple Silicon" ""
check "Linux arm64 keeps its refusal" Linux aarch64 "No Linux arm64 build yet" ""
check "macOS x64 gets past the refusal" Darwin x86_64 "Could not download" "Apple Silicon"

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all install-rust.sh platform checks passed"
