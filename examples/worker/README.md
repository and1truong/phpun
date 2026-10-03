# Worker mode demo

    phpun serve examples/worker/app.php --worker

Then `curl localhost:8000/x?a=1` twice — `counter` keeps counting up and
`uptime_ms` keeps growing, proving the app booted once and stayed hot.
`--workers=N` runs N warm workers (state is per-worker, PHP-shared-nothing
style); a script that returns no callable falls back to fresh-interp mode.
