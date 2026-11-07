#!/bin/sh
# Publishes a release: packaging/release.sh [--dry-run] [--latest <version>] <version> <notes-file>
#
# It checks the key, the tree and the tests, builds both targets and the .deb, writes release.json,
# signs it, tags, pushes the tag and creates a full release. The version must already be the one in
# Cargo.toml.
#
# --dry-run does everything up to and including signing, and prints what it would then have done.
# It never talks to GitHub, so --latest stands in for the latest release's version there.
set -eu
cd "$(dirname "$0")/.."

repository=Benbone21/mesh-kvm
key=$HOME/.config/mesh-kvm-release/signing.key
public=packaging/release-key.pub
target=${CARGO_TARGET_DIR:-target}
windows=x86_64-pc-windows-gnu

die() {
    echo "$1" >&2
    exit 1
}

dry_run=
latest=
while [ $# -gt 0 ]; do
    case $1 in
        --dry-run) dry_run=1; shift ;;
        --latest) [ $# -ge 2 ] || die "--latest needs a version."; latest=$2; shift 2 ;;
        --*) die "Unknown option: $1" ;;
        *) break ;;
    esac
done
[ $# -eq 2 ] || die "Usage: packaging/release.sh [--dry-run] [--latest <version>] <version> <notes-file>"
version=$1
notes=$2

# The same shape the program accepts, so a release can't be made that it would refuse.
is_version() {
    printf '%s\n' "$1" | grep -Eq '^(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})$'
}

is_newer() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]
}

is_version "$version" || die "$version isn't a version such as 0.7.0."
[ -s "$notes" ] || die "The release notes file $notes is missing or empty."
[ -z "$latest" ] || [ -n "$dry_run" ] || die "--latest only works with --dry-run."

# The key: refused while it's the placeholder, and unless the private half matches it.
if grep -q '^PLACEHOLDER' "$public"; then
    die "$public is still the placeholder. Run packaging/make-release-key.sh, and commit the result."
fi
[ -f "$key" ] || die "The signing key $key is missing."
case $(stat -c '%a' "$key") in
    400 | 600) ;;
    *) die "The signing key $key can be read by others. It must be readable only by its owner." ;;
esac
derived=$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')
[ "$derived" = "$(tr -d ' \n' <"$public")" ] || die "The signing key doesn't match $public."

# The tree. layout.json is written by running the program, and isn't part of a release.
[ -z "$(git status --porcelain -- . ':!layout.json')" ] || die "The tree has uncommitted changes."
in_cargo=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ "$in_cargo" = "$version" ] || die "Cargo.toml says $in_cargo, not $version."
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    die "The tag v$version already exists."
fi

if [ -z "$dry_run" ]; then
    latest=$(gh api "repos/$repository/releases/latest" --jq .tag_name) || die "Couldn't read the latest release."
    latest=${latest#v}
fi
if [ -n "$latest" ]; then
    is_newer "$version" "$latest" || die "$version isn't newer than the latest release, $latest."
fi

echo "Running the tests."
cargo test --locked --quiet

echo "Building."
cargo build --release --locked
cargo build --release --locked --target "$windows"
packaging/build-deb.sh >/dev/null

exe=$target/release/mesh-kvm
helper=$target/release/apply-update
windows_exe=$target/$windows/release/mesh-kvm.exe
deb=$target/deb/mesh-kvm_${version}_amd64.deb

# What ships can't be pointed elsewhere, and has this key in it.
for program in "$exe" "$helper" "$windows_exe"; do
    if grep -aq 'MESH_KVM_TEST_' "$program"; then
        die "$program was built with the test hooks."
    fi
    grep -aq "$derived" "$program" || die "$program doesn't hold the release key."
done

dist=$target/release-$version
rm -rf "$dist"
mkdir -p "$dist"
cp "$windows_exe" "$dist/mesh-kvm-windows-x86_64.exe"
cp "$deb" "$dist/"

entry() {
    printf '"%s":{"size":%s,"sha256":"%s"}' "$1" "$(stat -c %s "$dist/$1")" "$(sha256sum "$dist/$1" | cut -d ' ' -f 1)"
}
printf '{"version":"%s","files":{%s,%s}}\n' "$version" \
    "$(entry mesh-kvm-windows-x86_64.exe)" "$(entry "mesh-kvm_${version}_amd64.deb")" >"$dist/release.json"

openssl pkeyutl -sign -inkey "$key" -rawin -in "$dist/release.json" -out "$dist/release.json.sig"
[ "$(stat -c %s "$dist/release.json.sig")" = 64 ] || die "The signature came out the wrong size."
openssl pkey -in "$key" -pubout -out "$dist/release.pem"
openssl pkeyutl -verify -pubin -inkey "$dist/release.pem" -rawin -in "$dist/release.json" \
    -sigfile "$dist/release.json.sig" >/dev/null || die "The signature doesn't verify."
rm -f "$dist/release.pem"

run() {
    if [ -n "$dry_run" ]; then
        echo "Would run: $*"
    else
        "$@"
    fi
}

run git tag -a "v$version" -m "Release $version"
run git push origin "v$version"
run gh release create "v$version" --repo "$repository" --verify-tag --latest --title "mesh-kvm $version" \
    --notes-file "$notes" "$dist/release.json" "$dist/release.json.sig" \
    "$dist/mesh-kvm-windows-x86_64.exe" "$dist/mesh-kvm_${version}_amd64.deb"

if [ -n "$dry_run" ]; then
    echo "Dry run finished. Nothing left this machine. The files are in $dist."
else
    echo "Released $version."
fi
