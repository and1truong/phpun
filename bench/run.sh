#!/usr/bin/env bash
# bench/run.sh — side-by-side timing of phpun vs reference PHP on bench/*.php.
#
# Each script must print a single deterministic "RESULT <checksum>" line; the
# runner verifies both runtimes produce byte-identical stdout before comparing
# times (a bench that diverges is reported, not trusted).
#
# Usage:
#   PHP=/path/to/php PHPUN=./target/release/phpun bench/run.sh [--save FILE]
#
# Timing = min wall-clock ms over a few reps (reps adapt to script duration).
set -u
cd "$(dirname "$0")/.."

PHP=${PHP:-php}
PHPUN=${PHPUN:-./target/release/phpun}
TIMEOUT=${TIMEOUT:-300}
SAVE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --save) SAVE="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

now_ms() { date +%s%N; }

# run_min <runtime> <script> <capfile>
# writes rep-1 stdout to <capfile>, echoes min wall ms (-1 on failure/timeout).
run_min() {
    local rt="$1" script="$2" cap="$3" ms min i reps t0 t1
    t0=$(( $(now_ms) ))
    timeout "$TIMEOUT" "$rt" "$script" >"$cap" 2>/dev/null
    local rc=$?
    t1=$(( $(now_ms) ))
    [ $rc -ne 0 ] && { echo -1; return; }
    ms=$(( (t1 - t0) / 1000000 ))
    min=$ms
    if   [ "$ms" -lt 150 ];  then reps=5
    elif [ "$ms" -lt 1000 ]; then reps=3
    else reps=2; fi
    for ((i = 2; i <= reps; i++)); do
        t0=$(( $(now_ms) ))
        timeout "$TIMEOUT" "$rt" "$script" >/dev/null 2>&1
        t1=$(( $(now_ms) ))
        ms=$(( (t1 - t0) / 1000000 ))
        [ "$ms" -lt "$min" ] && min=$ms
    done
    echo "$min"
}

OUT=$(mktemp)
CAP_PHP=$(mktemp); CAP_PHPUN=$(mktemp)
trap 'rm -f "$OUT" "$CAP_PHP" "$CAP_PHPUN"' EXIT

{
    echo "# bench results"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- phpun: $("$PHPUN" --version 2>/dev/null | head -1) ($PHPUN)"
    echo "- php:   $("$PHP" -r 'echo PHP_VERSION;' 2>/dev/null) ($PHP)"
    echo "- host:  $(uname -srm) $(nproc) cores"
    echo "- metric: min wall-clock ms (adaptive reps); ratio = phpun / php"
    echo
    printf "| %-28s | %10s | %10s | %9s | %s |\n" "bench" "php ms" "phpun ms" "x slower" "stdout"
    printf "|%s|%s:|%s:|%s:|%s|\n" "------------------------------" "----------:" "----------:" "---------:" ":------"

    geo_sum=0; geo_n=0
    for f in bench/[0-9]*.php; do
        php_ms=$(run_min "$PHP" "$f" "$CAP_PHP")
        phpun_ms=$(run_min "$PHPUN" "$f" "$CAP_PHPUN")
        name=$(basename "$f" .php)
        if [ "$php_ms" -ge 0 ] && [ "$phpun_ms" -ge 0 ] && cmp -s "$CAP_PHP" "$CAP_PHPUN"; then
            match="ok"
        else
            match="MISMATCH"
        fi
        if [ "$php_ms" -gt 0 ] && [ "$phpun_ms" -gt 0 ]; then
            ratio=$(awk "BEGIN{printf \"%.1f\", $phpun_ms / $php_ms}")
        else
            ratio="n/a"
        fi
        printf "| %-28s | %10s | %10s | %9s | %s |\n" "$name" "$php_ms" "$phpun_ms" "$ratio" "$match"
        if [ "$match" = "ok" ] && [ "$ratio" != "n/a" ]; then
            geo_sum=$(awk "BEGIN{print $geo_sum + log($phpun_ms / $php_ms)}")
            geo_n=$((geo_n + 1))
        fi
    done
    [ "$geo_n" -gt 0 ] && printf "\ngeometric mean slowdown (matched benches): %.1fx\n" \
        "$(awk "BEGIN{print exp($geo_sum / $geo_n)}")"
} > "$OUT"

cat "$OUT"
if [ -n "$SAVE" ]; then
    cp "$OUT" "$SAVE"
    echo "saved: $SAVE" >&2
fi
