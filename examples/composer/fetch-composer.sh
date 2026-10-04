#!/usr/bin/env bash
# Fetch Composer and extract its phar into ./composer-src so phpun can
# run it from a source checkout (native `.phar` loading is still on the
# roadmap — tracked in #31).
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
ver="${COMPOSER_VERSION:-2.10.3}"
phar="$dir/composer-$ver.phar"
src="$dir/composer-src"

if [ ! -f "$phar" ]; then
    curl -fsSL -o "$phar" "https://getcomposer.org/download/$ver/composer.phar"
fi

# Extraction needs any PHP binary — phpun can't read phar archives yet.
php_bin="${PHP_BIN:-$(command -v php || true)}"
if [ -z "$php_bin" ]; then
    echo "fetch-composer.sh: needs a php binary to extract the phar (set PHP_BIN=...)" >&2
    exit 1
fi

rm -rf "$src"
"$php_bin" -r '(new Phar($argv[1]))->extractTo($argv[2], null, true);' "$phar" "$src"

echo "composer $ver extracted to $src"
echo "try: phpun $src/bin/composer --version"
