#!/bin/sh
set -eu

repository="${BENDER_REPOSITORY:-lnbits/bender}"
version="${BENDER_VERSION:-latest}"
install_dir="${BENDER_INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) platform="linux" ;;
  Darwin) platform="macos" ;;
  *) echo "Unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) architecture="x86_64" ;;
  arm64|aarch64) architecture="aarch64" ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$platform" = "linux" ] && [ "$architecture" != "x86_64" ]; then
  echo "No prebuilt Bender release is currently published for linux-$architecture." >&2
  exit 1
fi

asset="bender-${platform}-${architecture}"
if [ "$version" = "latest" ]; then
  url="https://github.com/${repository}/releases/latest/download/${asset}"
else
  url="https://github.com/${repository}/releases/download/${version}/${asset}"
fi

mkdir -p "$install_dir"
temporary="${TMPDIR:-/tmp}/bender-install-$$"
trap 'rm -f "$temporary"' EXIT HUP INT TERM

if command -v curl >/dev/null 2>&1; then
  curl -fL "$url" -o "$temporary"
elif command -v wget >/dev/null 2>&1; then
  wget -O "$temporary" "$url"
else
  echo "Bender installation requires curl or wget." >&2
  exit 1
fi

chmod 0755 "$temporary"
mv "$temporary" "$install_dir/bender"
trap - EXIT HUP INT TERM

echo "Bender installed."
echo
echo "Next:"
echo "  1. Install and authenticate Codex CLI."
echo "  2. Enter a project directory."
echo "  3. Run bender doctor."
echo "  4. Run bender."
