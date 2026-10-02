#!/usr/bin/env bash
# Fetch the official php-src PHPT tests for the PHP-8.5 branch into
# vendor/php-tests/ (gitignored, ~120MB). Reproducible: pinned to a tag or
# commit passed as $1 (default: php-8.5.11).
set -euo pipefail

REF="${1:-php-8.5.11}"
DEST="$(cd "$(dirname "$0")/.." && pwd)/vendor/php-tests"

mkdir -p "$DEST"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading php-src @ $REF ..."
curl -fsSL "https://github.com/php/php-src/archive/refs/tags/${REF}.tar.gz" \
    | tar -xz -C "$tmp"

src="$tmp/php-src-${REF}"
if [ ! -d "$src/tests" ]; then
    echo "error: $src does not look like a php-src tree" >&2
    exit 1
fi

# Only PHPT test trees are vendored: tests/, Zend/tests/, ext/*/tests/.
rm -rf "$DEST"
mkdir -p "$DEST"
cp -a "$src/tests" "$DEST/tests"
mkdir -p "$DEST/Zend"
cp -a "$src/Zend/tests" "$DEST/Zend/tests"
for d in "$src"/ext/*/tests; do
    ext="$(basename "$(dirname "$d")")"
    mkdir -p "$DEST/ext/$ext"
    cp -a "$d" "$DEST/ext/$ext/tests" 2>/dev/null || true
done

echo "Vendored $(find "$DEST" -name '*.phpt' | wc -l) PHPT files into $DEST"
