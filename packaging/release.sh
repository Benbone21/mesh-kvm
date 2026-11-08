#!/bin/sh
# Publishes a release: packaging/release.sh [--dry-run] [--latest <version>] <version> <notes-file>
#
# It checks the key, the tree and the tests, builds both targets, the .deb and the Windows installer,
# checks that what was built is this version with this key, writes release.json and signs it. Then it uploads a draft,
# pushes the tag and publishes a full release. The version must already be the one in Cargo.toml.
#
# If it fails partway through publishing, run it again: nothing can see a draft, and a tag on this
# commit or a draft already there is carried on from.
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
tagged=$(git rev-parse -q --verify "refs/tags/v$version^{commit}" || true)
if [ -n "$tagged" ] && [ "$tagged" != "$(git rev-parse HEAD)" ]; then
    die "The tag v$version already exists on another commit."
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
setup=$(packaging/windows/build-setup.sh "$version")

windows_exe=$target/$windows/release/mesh-kvm.exe
deb=$target/deb/mesh-kvm_${version}_amd64.deb

# What ships is looked at as it ships: the Linux programs come out of the .deb.
unpacked=$target/release-check
rm -rf "$unpacked"
mkdir -p "$unpacked"
dpkg-deb --fsys-tarfile "$deb" | tar -x -C "$unpacked" ./usr/bin/mesh-kvm ./usr/lib/mesh-kvm/apply-update
shipped_exe=$unpacked/usr/bin/mesh-kvm

# It can't be pointed elsewhere, and has this key in it.
for program in "$shipped_exe" "$unpacked/usr/lib/mesh-kvm/apply-update" "$windows_exe"; do
    if grep -aq 'MESH_KVM_TEST_' "$program"; then
        die "$program was built with the test hooks."
    fi
    grep -aq "$derived" "$program" || die "$program doesn't hold the release key."
done

# It's the version it's about to be published as.
said=$("$shipped_exe" --version)
[ "$said" = "mesh-kvm $version" ] || die "The program says \"$said\", not version $version."
grep -aq "mesh-kvm/$version" "$windows_exe" || die "$windows_exe isn't version $version."
[ "$(dpkg-deb -f "$deb" Package)" = mesh-kvm ] || die "$deb isn't the mesh-kvm package."
[ "$(dpkg-deb -f "$deb" Version)" = "$version" ] || die "$deb isn't version $version."

dist=$target/release-$version
rm -rf "$dist"
mkdir -p "$dist"
cp "$windows_exe" "$dist/mesh-kvm-windows-x86_64.exe"
cp "$deb" "$dist/"
cp "$setup" "$dist/mesh-kvm-setup.exe"

entry() {
    printf '"%s":{"size":%s,"sha256":"%s"}' "$1" "$(stat -c %s "$dist/$1")" "$(sha256sum "$dist/$1" | cut -d ' ' -f 1)"
}
# The installer is signed for too. A device looks up only its own file, so an older one isn't
# bothered by a name it doesn't know.
printf '{"version":"%s","files":{%s,%s,%s}}\n' "$version" \
    "$(entry mesh-kvm-windows-x86_64.exe)" "$(entry "mesh-kvm_${version}_amd64.deb")" "$(entry mesh-kvm-setup.exe)" >"$dist/release.json"

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

assets="$dist/release.json $dist/release.json.sig $dist/mesh-kvm-windows-x86_64.exe $dist/mesh-kvm_${version}_amd64.deb $dist/mesh-kvm-setup.exe"

# A draft first: the updater reads only the latest full release, so nothing is offered until every
# file is up, and a failure here can simply be run again.
[ -n "$tagged" ] || run git tag -a "v$version" -m "Release $version"
if [ -z "$dry_run" ] && gh release view "v$version" --repo "$repository" >/dev/null 2>&1; then
    # shellcheck disable=SC2086
    run gh release upload "v$version" --repo "$repository" --clobber $assets
else
    # shellcheck disable=SC2086
    run gh release create "v$version" --repo "$repository" --draft --title "mesh-kvm $version" --notes-file "$notes" $assets
fi
run git push origin "v$version"
run gh release edit "v$version" --repo "$repository" --draft=false --latest --verify-tag --notes-file "$notes"

if [ -n "$dry_run" ]; then
    echo "Dry run finished. Nothing left this machine. The files are in $dist."
else
    echo "Released $version."
fi
