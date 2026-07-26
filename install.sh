#!/bin/sh
set -eu

repository="${BENDER_REPOSITORY:-lnbits/bender}"
version="${BENDER_VERSION:-latest}"
prefix="${BENDER_PREFIX:-${HOME}/.local}"
non_interactive=false

usage() {
  cat <<'EOF'
Usage: install.sh [--version TAG] [--prefix DIR] [--non-interactive]

Installs only the Bender binary. Dependencies such as Codex are not installed.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || {
        echo "Error: --version requires a tag such as v0.2.0." >&2
        exit 2
      }
      version=$2
      shift 2
      ;;
    --prefix)
      [ "$#" -ge 2 ] || {
        echo "Error: --prefix requires a directory." >&2
        exit 2
      }
      prefix=$2
      shift 2
      ;;
    --non-interactive)
      non_interactive=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# Test-only uname overrides make platform mapping deterministic without changing
# the production download path.
os_name="${BENDER_TEST_UNAME_S:-$(uname -s)}"
machine="${BENDER_TEST_UNAME_M:-$(uname -m)}"

case "$os_name" in
  Linux) platform="linux" ;;
  Darwin) platform="macos" ;;
  MINGW*|MSYS*|CYGWIN*) platform="windows" ;;
  *)
    echo "Error: unsupported operating system: $os_name" >&2
    exit 1
    ;;
esac

case "$machine" in
  x86_64|amd64) architecture="x86_64" ;;
  arm64|aarch64) architecture="aarch64" ;;
  *)
    echo "Error: unsupported architecture: $machine" >&2
    exit 1
    ;;
esac

case "${platform}-${architecture}" in
  linux-x86_64) asset="bender-linux-x86_64" ;;
  linux-aarch64) asset="bender-linux-aarch64" ;;
  macos-x86_64) asset="bender-macos-x86_64" ;;
  macos-aarch64) asset="bender-macos-aarch64" ;;
  windows-x86_64) asset="bender-windows-x86_64.exe" ;;
  *)
    echo "Error: no Bender release asset supports ${platform}-${architecture}." >&2
    exit 1
    ;;
esac

case "$prefix" in
  /*) ;;
  *)
    echo "Error: --prefix must be an absolute path." >&2
    exit 1
    ;;
esac

install_dir="${BENDER_INSTALL_DIR:-${prefix}/bin}"
destination="${install_dir}/bender"
if [ "$platform" = "windows" ]; then
  destination="${destination}.exe"
fi

if [ "$version" = "latest" ]; then
  release_base="https://github.com/${repository}/releases/latest/download"
else
  case "$version" in
    v[0-9]*) ;;
    *)
      echo "Error: --version must be a release tag such as v0.2.0." >&2
      exit 2
      ;;
  esac
  release_base="https://github.com/${repository}/releases/download/${version}"
fi
release_base="${BENDER_DOWNLOAD_BASE_URL:-$release_base}"

mkdir -p "$install_dir"
binary_tmp=$(mktemp "${install_dir}/.bender-download.XXXXXX")
sums_tmp=$(mktemp "${install_dir}/.bender-checksums.XXXXXX")
cleanup() {
  rm -f "$binary_tmp" "$sums_tmp"
}
trap cleanup EXIT HUP INT TERM

download() {
  source_url=$1
  output_path=$2
  if command -v curl >/dev/null 2>&1; then
    if ! curl -fsSL "$source_url" -o "$output_path"; then
      echo "Error: download failed: $source_url" >&2
      return 1
    fi
  elif command -v wget >/dev/null 2>&1; then
    if ! wget -q "$source_url" -O "$output_path"; then
      echo "Error: download failed: $source_url" >&2
      return 1
    fi
  else
    echo "Error: Bender installation requires curl or wget." >&2
    return 1
  fi
}

download "${release_base}/${asset}" "$binary_tmp"
download "${release_base}/SHA256SUMS" "$sums_tmp"

expected_checksum=$(awk -v asset="$asset" '
  $2 == asset || $2 == "*" asset { print $1; found = 1; exit }
  END { if (!found) exit 1 }
' "$sums_tmp") || {
  echo "Error: SHA256SUMS has no checksum for ${asset}; refusing to install." >&2
  exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
  actual_checksum=$(sha256sum "$binary_tmp" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual_checksum=$(shasum -a 256 "$binary_tmp" | awk '{print $1}')
else
  echo "Error: checksum verification requires sha256sum or shasum." >&2
  exit 1
fi

if [ "$actual_checksum" != "$expected_checksum" ]; then
  echo "Error: checksum verification failed for ${asset}; refusing to install." >&2
  exit 1
fi

chmod 0755 "$binary_tmp"
# The temporary binary is created in the destination directory, so rename is
# atomic and an existing installation survives every earlier failure.
mv -f "$binary_tmp" "$destination"
binary_tmp="${install_dir}/.bender-download.removed"

installed_version=$("$destination" --version 2>/dev/null || true)
if [ -z "$installed_version" ]; then
  installed_version="${version} (${asset})"
fi

echo "Bender installed successfully."
echo "Installed: ${destination}"
echo "Version: ${installed_version}"

case ":${PATH:-}:" in
  *":${install_dir}:"*) ;;
  *)
    echo
    echo "Add ${install_dir} to PATH, for example:"
    echo "  export PATH=\"${install_dir}:\$PATH\""
    ;;
esac

if [ "$non_interactive" = true ]; then
  :
fi

echo
echo "Next:"
echo "  codex login"
echo "  cd /path/to/project"
echo "  bender doctor"
echo "  bender"
