#!/usr/bin/env bash
# Build the release bundles into target/release/bundle/. Which bundles is
# decided by `bundle.targets` in src-tauri/tauri.conf.json (a Debian
# package and an AppImage today).
# Usage: scripts/build-tauri.sh [tauri build args]
#
# linuxdeploy's GTK plugin copies directory trees with
# `cp --archive --parents`. The Rust coreutils that newer Ubuntu releases
# install as `cp` fail on that call ("are the same file"), so where GNU
# cp is installed next to them as `gnucp` it is put first on the PATH for
# the build.
set -euo pipefail
cd "$(dirname "$0")/.."

if cp --version 2>/dev/null | grep -qi uutils; then
  if gnucp="$(command -v gnucp)"; then
    shim="$(mktemp -d)"
    trap 'rm -rf "$shim"' EXIT
    ln -s "$gnucp" "$shim/cp"
    export PATH="$shim:$PATH"
    echo "build-tauri: using GNU cp ($gnucp) for linuxdeploy"
  else
    echo "build-tauri: cp is uutils and gnucp was not found; install gnu-coreutils if the bundling step fails" >&2
  fi
fi

npx tauri build "$@"
