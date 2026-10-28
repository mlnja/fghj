#!/usr/bin/env bash
# Checks a built fghj .deb: its maintainer scripts parse, and it installs,
# runs, removes and purges cleanly in a fresh container of every supported
# Ubuntu LTS. Used by CI and the release; needs Docker.
#   packaging/check-deb.sh dist/fghj_<version>_<arch>.deb
set -euo pipefail

deb=$(realpath "$1")

echo "::group::package"
dpkg-deb --info "$deb"
dpkg-deb --contents "$deb"
echo "::endgroup::"

# cargo-deb pastes generated code into these; a script that doesn't parse
# fails only on a user's `apt remove`.
ctrl=$(mktemp -d)
trap 'rm -rf "$ctrl"' EXIT
dpkg-deb --ctrl-tarfile "$deb" | tar -x -C "$ctrl"
for script in preinst postinst prerm postrm; do
  [ -f "$ctrl/$script" ] && sh -n "$ctrl/$script" && echo "$script parses"
done

for release in 22.04 24.04 26.04; do
  echo "::group::ubuntu:$release"
  docker run --rm -v "$deb:/fghj.deb:ro" "ubuntu:$release" sh -ec '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq /fghj.deb >/dev/null
    fghj --version
    fghjd --version
    test -f /usr/lib/systemd/system/fghjd.service || test -f /lib/systemd/system/fghjd.service
    apt-get remove -y -qq fghj >/dev/null
    test ! -e /usr/bin/fghj
    apt-get purge -y -qq fghj >/dev/null
    test ! -e /var/lib/fghjd
    echo "installed, ran, removed and purged"'
  echo "::endgroup::"
done
