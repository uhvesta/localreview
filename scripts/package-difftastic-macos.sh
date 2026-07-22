#!/bin/sh
set -eu

version=0.69.0
arm_sha=c958b87885a5825a356c5899ac7ecdd752a7942084199f2be4bc0bf8c9de8e33
x86_sha=5f5487e7a6e817194a1cef297d2ffb300454371635a4cde865087dbc064730a2
universal_sha=2359e078a1899d9f194a2710b4c1221088545a289eec78c2d8434d1cc1ca767c
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
resource="$script_dir/../src-tauri/resources/localreview-sidecars/difft"
unsigned_cache="$script_dir/../target/localreview-sidecars/difft-unsigned"
signing_identity=${APPLE_SIGNING_IDENTITY-}

usage() {
  cat >&2 <<'EOF'
usage: package-difftastic-macos.sh [--ensure|--check]

With no option, rebuild the unsigned universal Difftastic cache from verified
upstream archives and prepare the bundled resource. --ensure reuses verified
artifacts when possible. --check only verifies the existing cache and resource.

When APPLE_SIGNING_IDENTITY is non-empty, the bundled resource must be signed
with that identity. Use '-' for an ad-hoc local signature. The pinned SHA-256
always applies to the unsigned cache, never to signed Mach-O bytes.
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

verify_architectures_and_version() {
  candidate=$1
  if [ ! -x "$candidate" ]; then
    echo "Difftastic sidecar is missing or is not executable: $candidate" >&2
    return 1
  fi

  if ! lipo "$candidate" -verify_arch x86_64 arm64; then
    echo "Difftastic sidecar is not a universal x86_64/arm64 binary: $candidate" >&2
    return 1
  fi

  actual_version=$(
    "$candidate" --version | sed -n '1{s/^Difftastic //;p;}'
  )
  if [ "$actual_version" != "$version" ]; then
    echo "Difftastic version mismatch: expected $version, got $actual_version" >&2
    return 1
  fi
}

verify_unsigned() {
  candidate=$1
  if [ ! -x "$candidate" ]; then
    echo "Unsigned Difftastic cache is missing or is not executable: $candidate" >&2
    return 1
  fi

  actual=$(shasum -a 256 "$candidate" | awk '{print $1}')
  if [ "$actual" != "$universal_sha" ]; then
    echo "Unsigned Difftastic universal binary digest mismatch: $candidate" >&2
    return 1
  fi

  # Execute the candidate only after its exact pinned digest is authenticated.
  verify_architectures_and_version "$candidate"
}

resolve_signing_identity_hash() {
  identity=$1
  if printf '%s\n' "$identity" | grep -Eq '^[[:xdigit:]]{40}$'; then
    printf '%s\n' "$identity" | tr '[:lower:]' '[:upper:]'
    return 0
  fi

  identity_hash=$(
    security find-certificate -c "$identity" -Z 2>/dev/null |
      sed -n 's/^SHA-1 hash: //p' |
      sed -n '1p'
  )
  if ! printf '%s\n' "$identity_hash" | grep -Eq '^[[:xdigit:]]{40}$'; then
    echo "Could not resolve code-signing identity certificate: $identity" >&2
    return 1
  fi
  printf '%s\n' "$identity_hash"
}

verify_signed_resource() {
  identity=$1
  if [ ! -x "$resource" ]; then
    echo "Signed Difftastic resource is missing or is not executable: $resource" >&2
    return 1
  fi

  if ! codesign --verify --strict --all-architectures "$resource"; then
    echo "Difftastic resource has an invalid code signature: $resource" >&2
    return 1
  fi

  signature_details=$(codesign --display --verbose=4 "$resource" 2>&1)
  if ! printf '%s\n' "$signature_details" | grep -Eq 'flags=.*\(.*runtime.*\)'; then
    echo "Difftastic resource signature does not enable hardened runtime" >&2
    return 1
  fi

  if [ "$identity" = - ]; then
    if ! printf '%s\n' "$signature_details" | grep -Eq '^Signature=adhoc$'; then
      echo "Difftastic resource is not signed with the requested ad-hoc identity" >&2
      return 1
    fi
  else
    if ! printf '%s\n' "$signature_details" | grep -Eq '^Timestamp='; then
      echo "Difftastic resource signature does not include a trusted timestamp" >&2
      return 1
    fi
    identity_hash=$(resolve_signing_identity_hash "$identity") || return 1
    requirement="certificate leaf = H\"$identity_hash\""
    if ! codesign --verify --strict --all-architectures \
      -R="$requirement" "$resource"; then
      echo "Difftastic resource is not signed with the requested identity: $identity" >&2
      return 1
    fi
  fi

  # Execute the candidate only after its requested signature is authenticated.
  verify_architectures_and_version "$resource"
}

verify_expected_resource() {
  if [ -n "$signing_identity" ]; then
    verify_signed_resource "$signing_identity"
  else
    verify_unsigned "$resource"
  fi
}

if [ "$mode" = check ]; then
  verify_unsigned "$unsigned_cache"
  verify_expected_resource
  if [ -n "$signing_identity" ]; then
    echo "Verified unsigned Difftastic $version cache and signed resource: $resource"
  else
    echo "Verified unsigned Difftastic $version cache and resource: $resource"
  fi
  exit 0
fi

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

rebuild_unsigned_cache() {
  stage=$(mktemp -d "${TMPDIR:-/tmp}/localreview-difftastic.XXXXXX")
  trap 'rm -rf "$stage"' EXIT HUP INT TERM

  download_and_verify aarch64-apple-darwin "$arm_sha"
  download_and_verify x86_64-apple-darwin "$x86_sha"
  lipo -create \
    -output "$stage/difft-universal" \
    "$stage/aarch64-apple-darwin/difft" \
    "$stage/x86_64-apple-darwin/difft"
  verify_unsigned "$stage/difft-universal"
  mkdir -p "$(dirname -- "$unsigned_cache")"
  install -m 755 "$stage/difft-universal" "$unsigned_cache"

  rm -rf "$stage"
  trap - EXIT HUP INT TERM
}

ensure_unsigned_cache() {
  if verify_unsigned "$unsigned_cache" 2>/dev/null; then
    return 0
  fi

  # Upgrade an existing pre-signing checkout without another download.
  if verify_unsigned "$resource" 2>/dev/null; then
    mkdir -p "$(dirname -- "$unsigned_cache")"
    install -m 755 "$resource" "$unsigned_cache"
    verify_unsigned "$unsigned_cache"
    return 0
  fi

  rebuild_unsigned_cache
}

prepare_resource() {
  if verify_expected_resource 2>/dev/null; then
    return 0
  fi

  mkdir -p "$(dirname -- "$resource")"
  install -m 755 "$unsigned_cache" "$resource"

  if [ -n "$signing_identity" ]; then
    if [ "$signing_identity" = - ]; then
      codesign --force --options runtime --timestamp=none \
        --sign - "$resource"
    else
      codesign --force --options runtime --timestamp \
        --sign "$signing_identity" "$resource"
    fi
  fi

  verify_expected_resource
}

if [ "$mode" = rebuild ]; then
  rebuild_unsigned_cache
else
  ensure_unsigned_cache
fi
prepare_resource

if [ -n "$signing_identity" ]; then
  echo "Packaged and verified signed Difftastic $version universal sidecar: $resource"
else
  echo "Packaged and verified unsigned Difftastic $version universal sidecar: $resource"
fi
