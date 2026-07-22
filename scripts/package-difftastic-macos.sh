#!/bin/sh
set -eu

version=0.69.0
arm_sha=c958b87885a5825a356c5899ac7ecdd752a7942084199f2be4bc0bf8c9de8e33
x86_sha=5f5487e7a6e817194a1cef297d2ffb300454371635a4cde865087dbc064730a2
universal_sha=2359e078a1899d9f194a2710b4c1221088545a289eec78c2d8434d1cc1ca767c
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
resource="$script_dir/../src-tauri/resources/localreview-sidecars/difft"

usage() {
  cat >&2 <<'EOF'
usage: package-difftastic-macos.sh [--ensure|--check]

With no option, rebuild the universal Difftastic sidecar from verified
upstream archives. --ensure accepts an already verified artifact or rebuilds
it. --check only verifies the existing artifact.
EOF
  exit 2
}

mode=rebuild
if [ "$#" -gt 1 ]; then
  usage
fi
if [ "$#" -eq 1 ]; then
  case "$1" in
    --ensure) mode=ensure ;;
    --check) mode=check ;;
    *) usage ;;
  esac
fi

if [ "$(uname -s)" != Darwin ]; then
  echo "The universal desktop sidecar can only be packaged on macOS." >&2
  exit 1
fi

verify_resource() {
  if [ ! -x "$resource" ]; then
    echo "Difftastic sidecar is missing or is not executable: $resource" >&2
    return 1
  fi

  actual=$(shasum -a 256 "$resource" | awk '{print $1}')
  if [ "$actual" != "$universal_sha" ]; then
    echo "Difftastic universal binary digest mismatch" >&2
    return 1
  fi

  if ! lipo "$resource" -verify_arch x86_64 arm64; then
    echo "Difftastic sidecar is not a universal x86_64/arm64 binary" >&2
    return 1
  fi

  actual_version=$(
    "$resource" --version | sed -n '1{s/^Difftastic //;p;}'
  )
  if [ "$actual_version" != "$version" ]; then
    echo "Difftastic version mismatch: expected $version, got $actual_version" >&2
    return 1
  fi
}

if [ "$mode" = check ]; then
  verify_resource
  echo "Verified Difftastic $version universal sidecar: $resource"
  exit 0
fi

if [ "$mode" = ensure ] && verify_resource 2>/dev/null; then
  echo "Using verified Difftastic $version universal sidecar: $resource"
  exit 0
fi

stage=$(mktemp -d "${TMPDIR:-/tmp}/localreview-difftastic.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM

download_and_verify() {
  target=$1
  expected=$2
  archive="$stage/difft-$target.tar.gz"
  curl --fail --location --proto '=https' --tlsv1.2 \
    --output "$archive" \
    "https://github.com/Wilfred/difftastic/releases/download/$version/difft-$target.tar.gz"
  actual=$(shasum -a 256 "$archive" | awk '{print $1}')
  if [ "$actual" != "$expected" ]; then
    echo "Difftastic archive digest mismatch for $target" >&2
    exit 1
  fi
  mkdir -p "$stage/$target"
  tar -xzf "$archive" -C "$stage/$target" difft
}

download_and_verify aarch64-apple-darwin "$arm_sha"
download_and_verify x86_64-apple-darwin "$x86_sha"
lipo -create \
  -output "$stage/difft-universal" \
  "$stage/aarch64-apple-darwin/difft" \
  "$stage/x86_64-apple-darwin/difft"
mkdir -p "$(dirname -- "$resource")"
install -m 755 "$stage/difft-universal" "$resource"

verify_resource
echo "Packaged and verified Difftastic $version universal sidecar: $resource"
