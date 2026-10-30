#!/usr/bin/env bash
# Puts the machine back to "fghj installed, nothing wired": unwires ~/files
# and deletes it. Docker's image and build caches stay, so the next take
# downloads and builds quickly. The tapes run this before every take.
set -euo pipefail
WS="$HOME/files"

# Only ever delete a folder that holds nothing but the example repos.
if [ -d "$WS" ]; then
  for f in "$WS"/* "$WS"/.[!.]*; do
    [ -e "$f" ] || continue
    case "$(basename "$f")" in
      files-api|files-web|files-worker|files-platform|.fghj) ;;
      *) echo "refusing to delete $WS: it holds $(basename "$f")" >&2; exit 1 ;;
    esac
  done
fi

id=$(fghj workspaces | awk -v ws="$WS" '$2 == ws { print $1 }')
if [ -n "$id" ]; then
  fghj workspaces rm "$id"
fi
rm -rf "$WS"
