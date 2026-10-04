# PHPUnit on phpun

phpun runs PHPUnit 12.5 end-to-end from a source checkout: it discovers
`*Test.php` files, builds the test suite via Reflection, runs each
`test*` method in a sandbox, and prints the standard progress /
report output — pass, failure (with unified diff), error, skipped,
incomplete — byte-for-byte like reference PHP 8.5.

## Setup

Extraction needs any `php` binary (phpun can't read phar archives yet —
tracked in #31):

```console
$ cd examples/phpunit
$ ./fetch-phpunit.sh        # writes ./phpunit-src/ + ./phpunit-run.php
```

`fetch-phpunit.sh` downloads `phpunit-12.5.37.phar`, extracts it, and
rewrites the phar stub into `phpunit-run.php`: `phar://` paths point at
`./phpunit-src`, `Phar::mapPhar()` is dropped, and the required-extension
gate is bypassed. The generated file is plain PHP — reference `php` runs
it identically, which is how the outputs below were compared.

## Run

```console
$ phpun phpunit-run.php --version
PHPUnit 12.5.37 by Sebastian Bergmann and contributors.

$ phpun phpunit-run.php tests
PHPUnit 12.5.37 by Sebastian Bergmann and contributors.

Runtime:       PHP 8.5.11-phpun

...                                                                 3 / 3 (100%)

Time: 00:00.025, Memory: 2.00 MB

OK (3 tests, 6 assertions)
```

`--version` is byte-identical to reference PHP. Suite runs are
byte-identical except the `Runtime:` line (engine name) and
`Time/Memory`. Verified identical: a passing suite (`...OK`), a failing
assertion (`.F` + unified diff + `FAILURES!`), a thrown exception
(`.E` + `ERRORS!`), `markTestSkipped`/`markTestIncomplete` (`.IS`), and
exit codes (0 pass / 1 fail).

## What this exercises

The engine work behind it: array internal pointer
(`current`/`next`/`key`/`reset`, `iter_pos`), `array_splice` key
renumbering, error/exception handler stacks (`set_`/`restore_`), the
`Reflection*` family hierarchy + `getMethods`/`getDeclaringClass`/
attribute reads, `glob()` flag semantics, `getopt`, `hrtime`,
`gc_status`, `flock`/`file` flags, `stream_get_contents` offset+length,
`int ** int` int-preserving pow, `is_callable` on `__invoke`, SPL
iterators (RecursiveFilterIterator, SplObjectStorage, SplFixedArray,
RII `CATCH_GET_CHILD`), and `parent::__construct` dispatch through the
native Throwable ctor chain (what makes `getMessage()` survive
PHPUnit's own exception hierarchy).
