# FastRoute on phpun

End-to-end proof that phpun runs a real, unmodified Composer-era library:
[nikic/FastRoute](https://github.com/nikic/FastRoute) vendored under
`vendor/fast-route/` (MIT license), loaded by a minimal PSR-4
`spl_autoload_register` autoloader — the same shape `vendor/autoload.php`
generates.

Run the CLI demo (autoloading, `simpleDispatcher`, `(*MARK)`-based
dispatch, `preg` named captures, `?->`/result objects):

```console
$ phpun examples/fastroute/demo.php
int(1)
FastRoute\Dispatcher\Result\Matched Object
(
    [handler] => user_detail
    [variables] => Array
        (
            [id] => 42
        )
    ...
)
```

Run it as a web app (`phpun serve` → request → app code → response):

```console
$ phpun serve examples/fastroute/serve.php --port 8000
$ curl localhost:8000/user/42
{"handler":"user_detail","vars":{"id":"42"}}
$ curl localhost:8000/nope
{"error":"not found"}
```

Runtime features exercised: PSR-4 autoload + `spl_autoload_register`,
`__DIR__` from declaring file inside closures, class/interface/const
linkage across autoloaded files, PCRE2 (`(*SKIP)(*F)`, `(*MARK:x)`,
recursive patterns), `preg_match_all` with `PREG_SET_ORDER|OFFSET_CAPTURE`,
readonly-ish typed properties, `$_SERVER`/`$_GET`/JSON output.

The `Deprecated: Increment on non-numeric string` notice is emitted by
PHP 8.5 too — it's inside FastRoute's `MarkBased` (`$markName = 'a'`).
