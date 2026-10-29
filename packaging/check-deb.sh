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

    # What a running fghjd leaves: both CAs and their keys, the anchor in the
    # system store, and one wired workspace with its own database. A
    # container has no systemd to start fghjd, so they are put there by hand.
    mkdir -p /var/lib/fghjd/ca /var/lib/fghjd/certs /var/lib/fghjd/sidecar-ca /ws/.fghj /ws/api
    echo key > /var/lib/fghjd/ca/ca-key.pem
    echo key > /var/lib/fghjd/sidecar-ca/signing-key.pem
    cp "$(ls /usr/share/ca-certificates/mozilla/*.crt | head -1)" /var/lib/fghjd/certs/cert.pem
    cp /var/lib/fghjd/certs/cert.pem /usr/local/share/ca-certificates/fghj-local-ca.crt
    update-ca-certificates >/dev/null 2>&1
    printf "{\n  \"ws-1\": \"/ws\"\n}\n" > /var/lib/fghjd/workspaces.json
    echo "{}" > /var/lib/fghjd/daemon-state.json
    touch /ws/.fghj/fghj.db

    apt-get remove -y -qq fghj >/dev/null
    test ! -e /usr/bin/fghj
    # No CA anywhere: not trusted, not on disk.
    test ! -e /usr/local/share/ca-certificates/fghj-local-ca.crt
    test ! -e /var/lib/fghjd/ca && test ! -e /var/lib/fghjd/certs && test ! -e /var/lib/fghjd/sidecar-ca
    test -f /var/lib/fghjd/workspaces.json

    apt-get purge -y -qq fghj >/dev/null
    test ! -e /var/lib/fghjd
    test ! -e /ws/.fghj && test -d /ws/api
    echo "installed, ran, removed (no CA left) and purged"'
  echo "::endgroup::"
done
