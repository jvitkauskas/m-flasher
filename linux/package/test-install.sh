#!/bin/sh
# Installs a package in a clean container of a distribution with its own
# package manager (so its dependencies resolve), then checks that M-Flasher
# runs and its files are in place. For CI:
#   docker run --rm -v "$PWD:/w" -w /w debian:12 linux/package/test-install.sh m-flasher_*.deb
set -eu
pkg=$1
if command -v apt-get >/dev/null; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq "./$pkg" >/dev/null
elif command -v dnf >/dev/null; then
    dnf install -y -q "./$pkg"
elif command -v zypper >/dev/null; then
    zypper --non-interactive --quiet install --allow-unsigned-rpm "./$pkg"
else
    echo "no known package manager" >&2
    exit 1
fi
m-flasher --help
for f in /usr/lib/udev/rules.d/70-m-flasher.rules /usr/lib/modules-load.d/m-flasher.conf \
    /usr/share/applications/io.github.jvitkauskas.m-flasher.desktop \
    /usr/share/metainfo/io.github.jvitkauskas.m-flasher.metainfo.xml \
    /usr/share/icons/hicolor/256x256/apps/io.github.jvitkauskas.m-flasher.png; do
    test -f "$f" || { echo "missing: $f" >&2; exit 1; }
done
# the libraries the window loads when it starts
for lib in libEGL.so.1 libxkbcommon.so.0; do
    ldconfig -p | grep -q "$lib" || { echo "not installed: $lib" >&2; exit 1; }
done
echo "OK: $(. /etc/os-release; echo "$PRETTY_NAME")"
