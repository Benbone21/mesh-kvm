#!/bin/sh
# Builds <target>/deb/mesh-kvm_<version>_amd64.deb from <target>/release, where <target> is
# $CARGO_TARGET_DIR if that's set and target otherwise.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
target=${CARGO_TARGET_DIR:-target}
exe=$target/release/mesh-kvm
out=$target/deb
root=$out/root
rm -rf "$out"
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/lib/mesh-kvm" "$root/usr/lib/systemd/user" "$root/usr/lib/udev/rules.d" \
    "$root/usr/share/applications" "$root/etc/xdg/autostart" \
    "$root/usr/share/polkit-1/actions" "$root/usr/share/polkit-1/rules.d" \
    "$root/usr/share/doc/mesh-kvm"

install -m 755 "$exe" "$root/usr/bin/mesh-kvm"
install -m 755 "$target/release/apply-update" "$root/usr/lib/mesh-kvm/apply-update"
install -m 644 packaging/deb/io.github.benbone21.mesh-kvm.apply-update.policy "$root/usr/share/polkit-1/actions/"
install -m 644 packaging/deb/50-mesh-kvm-update.rules "$root/usr/share/polkit-1/rules.d/"
install -m 644 packaging/deb/mesh-kvm.service "$root/usr/lib/systemd/user/"
install -m 644 99-mesh-kvm-uinput.rules "$root/usr/lib/udev/rules.d/"
install -m 755 packaging/deb/postinst packaging/deb/prerm packaging/deb/postrm "$root/DEBIAN/"
install -m 644 README.md "$root/usr/share/doc/mesh-kvm/"
install -m 644 third_party/opus/COPYING "$root/usr/share/doc/mesh-kvm/opus-COPYING"
install -m 644 third_party/fonts/Inter-OFL.txt "$root/usr/share/doc/mesh-kvm/Inter-OFL.txt"
install -m 644 third_party/fonts/JetBrainsMono-OFL.txt "$root/usr/share/doc/mesh-kvm/JetBrainsMono-OFL.txt"

# The program writes its own launchers and icon; only their paths differ here.
home=$out/home
mkdir -p "$home"
home=$(realpath "$home")
HOME=$home "$exe" --install-launchers >/dev/null
sed "s|^Exec=.*mesh-kvm |Exec=/usr/bin/mesh-kvm |" "$home/.local/share/applications/mesh-kvm.desktop" \
    >"$root/usr/share/applications/mesh-kvm.desktop"
sed "s|^Exec=.*mesh-kvm |Exec=/usr/bin/mesh-kvm |" "$home/.config/autostart/mesh-kvm-tray.desktop" \
    >"$root/etc/xdg/autostart/mesh-kvm-tray.desktop"
for icon in "$home"/.local/share/icons/hicolor/*/apps/mesh-kvm.png; do
    icon_size=$(basename "$(dirname "$(dirname "$icon")")")
    install -D -m 644 "$icon" "$root/usr/share/icons/hicolor/$icon_size/apps/mesh-kvm.png"
done
chmod 644 "$root/usr/share/applications/mesh-kvm.desktop" "$root/etc/xdg/autostart/mesh-kvm-tray.desktop"

find "$root" -type d -exec chmod 755 {} +
size=$(du -sk "$root" | cut -f1)
cat >"$root/DEBIAN/control" <<CONTROL
Package: mesh-kvm
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: bread <jj1nfz2nb@mozmail.com>
Installed-Size: $size
Depends: libc6 (>= 2.39), libgcc-s1, pipewire-bin, wireplumber, x11-xserver-utils, libkscreen-bin, qdbus-qt6, dbus-bin, libegl1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-cursor0, libwayland-egl1, libx11-6, libxcursor1, libxi6, pkexec, polkitd
Recommends: libgl1
Description: Share one mouse, keyboard, clipboard and audio between devices
 mesh-kvm moves the pointer between a Debian desktop and a Windows laptop
 at the edge of the screen, and shares the clipboard, files and audio.
CONTROL

dpkg-deb --build --root-owner-group "$root" "$out/mesh-kvm_${version}_amd64.deb"
