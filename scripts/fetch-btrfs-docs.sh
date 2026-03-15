#!/usr/bin/env bash
# Fetch btrfs-progs Documentation/ (RST files) into docs/btrfs-docs/
set -euo pipefail

REPO="https://github.com/kdave/btrfs-progs.git"
DEST="$(cd "$(dirname "$0")/.." && pwd)/docs/btrfs-docs"

# Find the latest stable release branch (e.g. v6.19.x)
BRANCH=$(git ls-remote --heads "$REPO" 'v*.x' | sed 's|.*refs/heads/||' | sort -V | tail -1)

rm -rf "$DEST"
mkdir -p "$DEST"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

git clone --depth 1 --branch "$BRANCH" --filter=blob:none --sparse "$REPO" "$tmp/btrfs-progs"
cd "$tmp/btrfs-progs"
git sparse-checkout set Documentation

cp -R Documentation/*.rst "$DEST/"
# include dev/ subdirectory if present
if [ -d Documentation/dev ]; then
  cp -R Documentation/dev "$DEST/dev"
fi

# Inline ch-*.rst fragments into their parent files, then remove the fragments
python3 -c "
import re, sys
from pathlib import Path

docs = Path(sys.argv[1])

for f in docs.glob('*.rst'):
    if not f.is_file():
        continue
    text = f.read_text()
    def inline(m):
        inc = docs / m.group(1)
        return inc.read_text() if inc.exists() else m.group(0)
    text = re.sub(r'^\.\. include:: (ch-[^\s]+)$', inline, text, flags=re.MULTILINE)
    f.write_text(text)

for f in docs.glob('ch-*.rst'):
    f.unlink()
" "$DEST"

echo "Fetched $(find "$DEST" -name '*.rst' | wc -l | tr -d ' ') RST files to $DEST (ch-* fragments inlined)"
