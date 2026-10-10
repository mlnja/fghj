# Bump the patch version, commit, tag, and push.
release:
    #!/usr/bin/env bash
    set -euo pipefail

    current=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

    IFS='.' read -r major minor patch <<< "$current"
    next="$major.$minor.$((patch + 1))"

    echo "Bumping $current → $next"

    sed -i.bak "s/^version = \"$current\"/version = \"$next\"/" Cargo.toml
    rm Cargo.toml.bak

    cargo update --workspace --quiet

    git add Cargo.toml Cargo.lock
    git commit -m "chore: bump version to $next"

    git tag -a "v$next" -m "v$next"

    git push origin main
    git push origin "v$next"

    echo "Released v$next"

# Manual fallback for updating the Homebrew tap formula.
#
# The release workflow's `tap` job already does this automatically on every
# tagged release — reach for this only to fix a formula without cutting a
# release, or if that job failed. Both drive the same `# darwin-<arch>`
# marker comments in the formula, so they can't disagree.
#
# Waits for the release assets to be published rather than failing if they
# aren't up yet, so it can be run immediately after `just release`. Override
# the poll with TAP_WAIT_TIMEOUT / TAP_WAIT_INTERVAL (seconds), or set
# TAP_WAIT_TIMEOUT=0 to fail immediately if the assets are missing.
#
# Usage: just update-tap [version]   (defaults to current Cargo.toml version)
update-tap version="":
    #!/usr/bin/env bash
    set -euo pipefail

    if [[ -z "{{ version }}" ]]; then
        VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
    else
        VERSION="{{ version }}"
    fi

    FORMULA="../homebrew-tap/Formula/fghj.rb"
    TMPDIR=$(mktemp -d)
    trap "rm -rf $TMPDIR" EXIT

    # Checked before the wait, not after: otherwise a missing tap checkout
    # only surfaces half an hour later, having spent the whole poll.
    if [[ ! -f "$FORMULA" ]]; then
        echo "error: $FORMULA not found — clone mlnja/homebrew-tap next to this repo" >&2
        exit 1
    fi

    echo "Updating Homebrew tap for v$VERSION..."

    # fghj is macOS on Apple Silicon only — the one platform with release assets.
    PLATFORMS=(darwin-arm64)
    BASE="https://github.com/mlnja/fghj/releases/download/v$VERSION"

    # Wait for the assets to exist before touching the formula.
    #
    # `just release` tags and pushes, and the workflow that starts has to
    # finish a full release compile of the crate (the macOS binary) plus
    # two more inside Docker (the sidecar, which `create-release` is gated on)
    # before it publishes anything. Running this recipe straight afterwards
    # used to 404 on the first curl, which reads as a broken formula rather
    # than as "not yet" — and left no way to do the obvious thing of starting
    # it and walking away.
    #
    # Polls the `.sha256` assets themselves rather than the release object or
    # the workflow run: they're the exact bytes this recipe reads, so their
    # presence is the real precondition, and checking them needs neither `gh`
    # nor a token.
    WAIT_TIMEOUT="${TAP_WAIT_TIMEOUT:-2700}"
    WAIT_INTERVAL="${TAP_WAIT_INTERVAL:-15}"
    deadline=$(( $(date +%s) + WAIT_TIMEOUT ))
    announced=0

    while :; do
        missing=()
        for platform in "${PLATFORMS[@]}"; do
            # No `-S` here, unlike the fetch below: a 404 is the expected
            # answer while waiting, and curl would otherwise print two
            # "error: 404" lines on every single poll.
            if ! curl -fsL -o /dev/null "$BASE/fghj-$platform.tar.gz.sha256"; then
                missing+=("$platform")
            fi
        done
        if [[ ${#missing[@]} -eq 0 ]]; then
            break
        fi

        if (( $(date +%s) >= deadline )); then
            if [[ $announced -eq 1 ]]; then echo; fi
            echo "error: v$VERSION assets are still missing after ${WAIT_TIMEOUT}s: ${missing[*]}" >&2
            echo "       check the release run:  gh run list --workflow=release.yml --limit 3" >&2
            exit 1
        fi

        if [[ $announced -eq 0 ]]; then
            echo "  v$VERSION isn't published yet (${missing[*]}) — polling every ${WAIT_INTERVAL}s, up to ${WAIT_TIMEOUT}s"
            announced=1
        fi
        printf '.'
        sleep "$WAIT_INTERVAL"
    done
    if [[ $announced -eq 1 ]]; then echo " published"; fi

    for platform in "${PLATFORMS[@]}"; do
        sha=$(curl -fsSL "$BASE/fghj-$platform.tar.gz.sha256" | cut -d' ' -f1)
        echo "  $platform  $sha"
        awk -v sha="$sha" -v marker="# $platform" \
            '$0 ~ marker { sub(/"[0-9a-f]+"/, "\"" sha "\"") } { print }' \
            "$FORMULA" > "$TMPDIR/formula.tmp" && mv "$TMPDIR/formula.tmp" "$FORMULA"
    done

    sed -i.bak "s/^  version \"[^\"]*\"/  version \"$VERSION\"/" "$FORMULA"
    rm "$FORMULA.bak"

    # A formula still holding placeholder zeros would install nothing and
    # fail only at `brew install` time, on someone else's machine.
    if grep -q '"0\{64\}"' "$FORMULA"; then
        echo "error: formula still contains placeholder checksums" >&2
        exit 1
    fi

    cd ../homebrew-tap
    git add Formula/fghj.rb
    git commit -m "chore: update fghj to v$VERSION"
    git push origin main

    echo "Homebrew tap updated for v$VERSION"
