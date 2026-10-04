#!/usr/bin/env bash
# Fetch the PHPUnit phar and turn it into a source-checkout runner that
# phpun can execute (native `.phar` loading is still on the roadmap —
# tracked in #31). Writes ./phpunit-src/ and ./phpunit-run.php.
set -euo pipefail
dir="$(cd "$(dirname "$0")" && pwd)"
ver="${PHPUNIT_VERSION:-12.5.37}"
phar="$dir/phpunit-$ver.phar"
src="$dir/phpunit-src"
run="$dir/phpunit-run.php"

if [ ! -f "$phar" ]; then
    curl -fsSL -o "$phar" "https://phar.phpunit.de/phpunit-$ver.phar"
fi

# Extraction + stub munging needs any PHP binary — phpun can't read
# phar archives yet.
php_bin="${PHP_BIN:-$(command -v php || true)}"
if [ -z "$php_bin" ]; then
    echo "fetch-phpunit.sh: needs a php binary to extract the phar (set PHP_BIN=...)" >&2
    exit 1
fi

rm -rf "$src"
"$php_bin" -r '(new Phar($argv[1]))->extractTo($argv[2], null, true);' "$phar" "$src"

# Rewrite the phar stub into a plain PHP entrypoint:
#   - drop the shebang and __HALT_COMPILER trailer
#   - bypass the required-extensions gate (phpun reports no ext-* names
#     but implements what PHPUnit actually calls)
#   - point __PHPUNIT_PHAR_ROOT__/require paths at ./phpunit-src
#   - drop Phar::mapPhar() (no phar support needed from a source tree)
"$php_bin" -r '
    [$phar, $out] = [$argv[1], $argv[2]];
    $stub = (new Phar($phar))->getStub();
    $stub = preg_replace("/^#!.*/", "", $stub, 1);
    $stub = preg_replace("/__HALT_COMPILER\(\);.*$/s", "", $stub);
    $stub = str_replace(
        "if ([] !== \$unavailableExtensions) {",
        "if (false) {",
        $stub
    );
    $stub = preg_replace(
        "/define\('"'"'__PHPUNIT_PHAR_ROOT__'"'"', '"'"'phar:\/\/[^'"'"']+'"'"'\);/",
        "define('"'"'__PHPUNIT_PHAR_ROOT__'"'"', __DIR__ . '"'"'/phpunit-src'"'"');",
        $stub
    );
    $stub = preg_replace("/^Phar::mapPhar\([^;]*\);\n/m", "", $stub);
    $stub = preg_replace(
        "/'"'"'phar:\/\/[^'"'"']+'"'"' \./",
        "__PHPUNIT_PHAR_ROOT__ .",
        $stub
    );
    file_put_contents($out, $stub);
' "$phar" "$run"

echo "phpunit $ver extracted to $src"
echo "try: phpun $run --version"
