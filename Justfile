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

# Update the Homebrew tap formula with real SHAs for the given release.
# Run this after GitHub Actions has finished publishing the release.
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

    echo "Updating Homebrew tap for v$VERSION..."

    # fghj is macOS-only today — only these two platforms have release assets.
    for platform in darwin-arm64 darwin-amd64; do
        url="https://github.com/mlnja/fghj/releases/download/v$VERSION/fghj-$platform.tar.gz.sha256"
        sha=$(curl -fsSL "$url" | cut -d' ' -f1)
        echo "  $platform  $sha"
        awk -v sha="$sha" -v marker="# $platform" \
            '$0 ~ marker { sub(/"[0-9a-f]+"/, "\"" sha "\"") } { print }' \
            "$FORMULA" > "$TMPDIR/formula.tmp" && mv "$TMPDIR/formula.tmp" "$FORMULA"
    done

    sed -i.bak "s/version \"[^\"]*\"/version \"$VERSION\"/" "$FORMULA"
    rm "$FORMULA.bak"

    cd ../homebrew-tap
    git add Formula/fghj.rb
    git commit -m "chore: update fghj to v$VERSION"
    git push origin main

    echo "Homebrew tap updated for v$VERSION"
