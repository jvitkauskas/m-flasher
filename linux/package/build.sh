#!/bin/sh
# Makes the .deb and .rpm from target/release/m-flasher, with nFPM:
#   linux/package/build.sh VERSION ARCH [OUTDIR]     (ARCH: amd64 or arm64)
set -eu
cd "$(dirname "$0")/../.."
version=$1
arch=$2
out=${3:-target/package}
gen=target/package-files
rm -rf "$gen"
mkdir -p "$gen" "$out"
# dated by the last commit, so that a rebuild makes the same files
epoch=$(git log -1 --format=%ct 2>/dev/null || date +%s)
export SOURCE_DATE_EPOCH="$epoch"

# the packages' changelogs (nFPM writes the deb's and the rpm's from it)
cat > "$gen/changelog.yml" <<CHANGELOG
- semver: $version-1
  date: $(date -u -d "@$epoch" +%Y-%m-%dT%H:%M:%SZ)
  packager: Julius Vitkauskas <zadintuvas@gmail.com>
  deb:
    urgency: medium
    distributions: [unstable]
  changes:
    - note: "M-Flasher $version, see its release notes:"
    - note: "https://github.com/jvitkauskas/m-flasher/releases/tag/v$version"
CHANGELOG
gzip -9n -c linux/package/m-flasher.1 > "$gen/m-flasher.1.gz"
# the metainfo, with this release
sed "s|</component>|  <releases>\n    <release version=\"$version\" date=\"$(date -u -d "@$epoch" +%F)\"/>\n  </releases>\n</component>|" \
    linux/package/io.github.jvitkauskas.m-flasher.metainfo.xml > "$gen/io.github.jvitkauskas.m-flasher.metainfo.xml"

for format in deb rpm; do
    VERSION=$version ARCH=$arch "${NFPM:-nfpm}" package -f linux/package/nfpm.yaml -p "$format" -t "$out/"
done
