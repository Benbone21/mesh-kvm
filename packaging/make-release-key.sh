#!/bin/sh
# Makes the release signing key, once. The private half goes to
# ~/.config/mesh-kvm-release/signing.key, readable only by its owner, and the public half into
# packaging/release-key.pub, where the build picks it up. It refuses to replace either: a new key
# can only reach devices through one more hand install.
set -eu
cd "$(dirname "$0")/.."
umask 077

dir=$HOME/.config/mesh-kvm-release
key=$dir/signing.key
public=packaging/release-key.pub

if [ -e "$key" ] || [ -L "$key" ]; then
    echo "$key already exists, so nothing was changed." >&2
    exit 1
fi
if ! grep -q '^PLACEHOLDER' "$public"; then
    echo "$public already holds a key, so nothing was changed." >&2
    exit 1
fi

mkdir -p "$dir"
chmod 700 "$dir"
fresh=$(mktemp "$dir/.signing.XXXXXX")
trap 'rm -f "$fresh"' EXIT
openssl genpkey -algorithm ed25519 -out "$fresh"
# `ln` fails if the name has been taken since the look above, where a move would replace it.
ln "$fresh" "$key"
rm -f "$fresh"

openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n' >"$public"
echo >>"$public"

echo "The private key is at $key. Back it up now: if it's lost, a new key can only reach devices"
echo "through one more hand install."
echo "Commit $public before the next release."
