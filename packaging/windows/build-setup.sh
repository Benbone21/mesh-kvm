#!/bin/sh
# Builds the Windows installer: packaging/windows/build-setup.sh <version> [path/to/mesh-kvm.exe]
#
# It writes <target>/windows/mesh-kvm-setup.exe, where <target> is $CARGO_TARGET_DIR if that's set
# and target otherwise. The exe defaults to the Windows release build. It needs makensis.
set -eu
cd "$(dirname "$0")/../.."

die() {
    echo "$1" >&2
    exit 1
}

[ $# -ge 1 ] && [ $# -le 2 ] || die "Usage: packaging/windows/build-setup.sh <version> [path/to/mesh-kvm.exe]"
version=$1
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "$version isn't a version such as 0.7.2."
target=${CARGO_TARGET_DIR:-target}
exe=${2:-$target/x86_64-pc-windows-gnu/release/mesh-kvm.exe}
[ -f "$exe" ] || die "$exe is missing. Build it with: cargo build --release --target x86_64-pc-windows-gnu"
command -v makensis >/dev/null || die "makensis is missing. On Debian it comes in the nsis package."

out=$target/windows
mkdir -p "$out"
# The same icon the exe has, which its build draws.
icon=$(ls -t "$target"/x86_64-pc-windows-gnu/release/build/mesh-kvm-*/out/mesh-kvm.ico 2>/dev/null | head -n 1 || true)
set -- -V2 -INPUTCHARSET UTF8 "-DVERSION=$version" "-DEXE=$(realpath "$exe")" "-DOUTFILE=$(realpath "$out")/mesh-kvm-setup.exe"
[ -z "$icon" ] || set -- "$@" "-DICON=$(realpath "$icon")"
makensis "$@" packaging/windows/mesh-kvm.nsi >&2
echo "$out/mesh-kvm-setup.exe"
