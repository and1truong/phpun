#!/usr/bin/env bash
# bench/run-http.sh — concurrent HTTP load comparison:
#   php -S (1 process)  vs  php -S (PHP_CLI_SERVER_WORKERS)  vs
#   phpun serve (fresh interp per request)  vs  phpun serve --workers N (warm).
#
# Each config serves bench/http/app*.php (same response body shape: the
# load driver asserts a "bench-ok" marker in every 200 response).
# Metric: requests/sec = total reqs / wall time, C concurrent connections.
#
# Usage:
#   PHP=/path/to/php PHPUN=./target/release/phpun \
#   REQ=400 CONC=8 WORKERS=8 bench/run-http.sh [--save FILE]
set -u
cd "$(dirname "$0")/.."

PHP=${PHP:-php}
PHPUN=${PHPUN:-./target/release/phpun}
REQ=${REQ:-400}
CONC=${CONC:-8}
WORKERS=${WORKERS:-8}
SAVE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --save) SAVE="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

LOADER=bench/http/http-load.py
PORT0=${PORT0:-18300}
SRV_PID=""

wait_ready() { # <port> — poll until the server answers or 10s elapse
    local port="$1" i
    for ((i = 0; i < 200; i++)); do
        if curl -sf -o /dev/null "http://127.0.0.1:$port/?name=ping" 2>/dev/null; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

stop_server() {
    [ -n "$SRV_PID" ] && { kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null; SRV_PID=""; }
}
trap stop_server EXIT

# bench_cfg <label> <port> — server must be started by caller first
bench_cfg() {
    local label="$1" port="$2" out
    if ! wait_ready "$port"; then
        printf "| %-34s | %12s | %6s |\n" "$label" "no-answer" "-"
        return
    fi
    python3 "$LOADER" "$port" $((CONC * 2)) "$CONC" >/dev/null 2>&1 # warmup
    out=$(python3 "$LOADER" "$port" "$REQ" "$CONC")
    # out: <total> <wall_s> <rps> <errors> <ok|BAD>
    local rps errs
    rps=$(echo "$out" | awk '{print $3}')
    errs=$(echo "$out" | awk '{print $4}')
    printf "| %-34s | %12s | %6s |\n" "$label" "$rps" "$errs"
}

OUT=$(mktemp); trap 'rm -f "$OUT"; stop_server' EXIT

{
    echo "# bench http results"
    echo
    echo "- date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- phpun: $("$PHPUN" --version 2>/dev/null | head -1)"
    echo "- php:   $("$PHP" -r 'echo PHP_VERSION;' 2>/dev/null) ($PHP)"
    echo "- host:  $(uname -srm) $(nproc) cores"
    echo "- load:  $REQ requests, $CONC concurrent, fresh connection per request"
    echo
    printf "| %-34s | %12s | %6s |\n" "server" "req/s" "errors"
    printf "|%s|%s:|%s:|\n" "------------------------------------" "----------:" "-----:"

    # Each config gets its own port: PHP_CLI_SERVER_WORKERS children can
    # outlive the killed parent briefly and hold the listen socket.
    # 1) php built-in server, single process
    "$PHP" -S 127.0.0.1:$((PORT0 + 1)) -t bench/http bench/http/app.php >/dev/null 2>&1 &
    SRV_PID=$!
    bench_cfg "php -S (1 proc)" $((PORT0 + 1))
    stop_server

    # 2) php built-in server, N workers
    PHP_CLI_SERVER_WORKERS=$WORKERS "$PHP" -S 127.0.0.1:$((PORT0 + 2)) -t bench/http bench/http/app.php >/dev/null 2>&1 &
    SRV_PID=$!
    bench_cfg "php -S ($WORKERS workers)" $((PORT0 + 2))
    stop_server

    # 3) phpun serve, fresh interp per request
    "$PHPUN" serve bench/http/app.php --port $((PORT0 + 3)) >/dev/null 2>&1 &
    SRV_PID=$!
    bench_cfg "phpun serve (fresh interp)" $((PORT0 + 3))
    stop_server

    # 4) phpun serve, N warm workers
    "$PHPUN" serve bench/http/app-worker.php --port $((PORT0 + 4)) --workers $WORKERS >/dev/null 2>&1 &
    SRV_PID=$!
    bench_cfg "phpun serve ($WORKERS warm workers)" $((PORT0 + 4))
    stop_server
} > "$OUT"

cat "$OUT"
if [ -n "$SAVE" ]; then
    cat "$OUT" >> "$SAVE"
    echo "appended: $SAVE" >&2
fi
