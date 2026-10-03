# Compatibility notes — tests/lang @ 272/280 (97.1%)

Reference: PHP 8.5.11 (cli). Harness: `phpun phpt … -j 8`.

## Implemented so far (green areas)

- Tags, literals (int/float/string/heredoc/nowdoc/interpolation), constants;
  numeric literals: `_` separators, `0x`/`0b`/`0o` + octal/hex/bin overflow →
  float, leading-`0` decimal (`08`) → "Invalid numeric literal" parse error
- Variables, `$GLOBALS`, superglobals (partial), static vars
  (per-function-decl storage, eval inherits caller's table),
  `global $a, $b` multi-declaration, chained `$$$a` variable variables
- Operators incl. PHP 8 comparison rules, `??`, `<=>`, bitwise on strings
  (`|` pads, `&`/`^` truncate), shifts (`<0` → ArithmeticError, `>=64` → 0);
  loose `==`/`<=>` compares ints as i64 (not via f64) when both sides are
  int-ish; binary ops bind plain CVs at op-execution (right operand runs
  before the left `$var` reads its value — `$a . ($a=$b)` → "goodgood")
- Int/float/string coercions: int overflow → float promotion,
  leading-numeric strings → "A non-numeric value encountered" + partial parse,
  out-of-range float → "not representable as an int" warning + mod-2^64 wrap
- Control flow: if/else, while, do, for, foreach (arrays, Iterator,
  IteratorAggregate, plain objects — live `prop_order` iteration),
  switch, match, try/catch/finally, throw
- Zend assignment model: dim/prop name exprs evaluate early (innermost
  first), container traversal at write time against the var's *current*
  value; the RHS-register quirk clobbers only `{expr}` prop names and
  call-result index dims — `$o->$k`, `$a[$k+1]`, literals keep their value
- Functions: default/variadic/named args, closures, arrow fns, first-class
  callable syntax, static methods; `func_num_args`/`func_get_args`/
  `func_get_arg` with PHP's param-current-value semantics and the exact
  `Error` messages for global-scope/out-of-range use
- Classes: properties, methods, visibility, inheritance, interfaces,
  traits (basic), abstract, `new`, `$this`, static calls, dynamic
  `C::$var()` calls, non-static forwarding (`$this` passes when caller's
  is-an-instance-of callee), magic `__get/__set/__call/__toString`
  (partial), built-in `stdClass`/`DateTime` (stub),
  `Throwable`/`Error`/`Exception` hierarchy (engine errors catchable)
- Param type enforcement: `Foo $a` → catchable `TypeError` with the PHP
  message incl. call/definition lines and a real `#0 file(N): f(Object(C))`
  trace frame; `P $p = null` → implicit-nullable Deprecated at declaration;
  `P $p = 42` → "Cannot use int as default value" compile fatal
- `list()`: "Cannot use int as array" / "Undefined array key N" warnings
- String offsets: `Illegal string offset "k"` warning for leading-int keys
  followed by junk (offset = leading digits); `TypeError` for keys with no
  leading int or float-shaped keys ("1.5")
- Diagnostics: `Warning:`/`Deprecated:`/`Notice:`/`Fatal error:` output
  format matching PHP CLI display_errors output (PHPT compares stdout);
  uncaught throwables print formatted trace frames + `thrown in` footer;
  `html_errors` rendering with docref links; include/require appear as
  internal backtrace frames; `debug_backtrace`/`debug_print_backtrace`
- Parse errors: non-printable bytes → `unexpected character 0xNN`;
  `"$arr['k']"` quoted keys in simple interpolation → E_PARSE;
  bracket balance → `Unclosed 'X'` / `Unmatched 'Y'` /
  `Unclosed 'X' [on line N] does not match 'Y'`
- `error_reporting()` level mask (E_* bits honored); `ini_set`/`ini_get`
  stored incl. K/M/G shorthand; `memory_limit` approximates allocation via
  emitted bytes and drops ob buffers on its fatal; `set_time_limit` /
  `hard_timeout` deadline → `Maximum execution time` fatal (also inside
  shutdown functions)
- Includes: `include`/`require`/`_once` resolve against include_path →
  calling file's dir → cwd; failed opens warn (`Failed to open stream` +
  `Failed opening` pair for include*, `Uncaught Error` for require*);
  top-level function decls hoist per compilation unit
- Builtins subset: string/array/math/type/printf-family, `print_r` nested
  container layout, eval, include, ob_* (handler mode bits,
  `[internal function]` callback sites), `set_error_handler`,
  `set_exception_handler`, `get_declared_classes`/`_interfaces`/`_traits`
- PHPT harness: FILE/EXPECT/EXPECTF/EXPECTREGEX/SKIPIF/INI/ARGS/ENV/CLEAN,
  JSON report, failure diffs, crash/timeout classification

## Known failing clusters (next highest-leverage first)

| Cluster                            | ~tests | Missing semantic                                   |
|------------------------------------|--------|----------------------------------------------------|
| 030 (`$GLOBALS['x'] =& $this`)     | 1      | `$this` inside a by-ref global slot                |
| bug20175/22510/24658 + returnByRef | 4      | residual reference edge cases                      |
| bug55754                           | 1      | `ZEND_SEND_PREFER_REF` (prefer-ref builtin args)   |
| unicode_escape_surrogates          | 1      | needs byte-string values (UTF-8 `Value::Str` limit)|
| bitwiseNot_variationStr            | 1      | same byte-string gap (`~"0"` → 0xCF)               |

## Surprising semantics reproduced (documented in code)

- `~9.2233720368548E+18` → warn + wraps to `i64::MIN` (x86 `cvttsd2si`)
- `PHP_INT_MIN % -1` → `0`; `intdiv(PHP_INT_MIN, -1)` → `ArithmeticError`;
  `PHP_INT_MIN / -1` → float
- string `|`/`^`/`&` ops: `|` pads to max len, `&`/`^` truncate to min
- `NAN === NAN` true; NaN unordered (all `<`/`>`/`==` false, `<=>` −1)
- `++`/`--` on non-well-formed numeric strings: Deprecated + Perl-style
- `error_reporting(E_ERROR)` suppresses E_WARNING diagnostics in output
- PHP CLI emits diagnostics to stdout only for PHPT comparison purposes
  (`PHP X:` stderr line is suppressed via `error_log` in the harness)
- `\u{HEX}` parse errors are strict ("Invalid UTF-8 codepoint escape
  sequence", "Codepoint too large"); surrogate halves would need CESU-8
  bytes — deferred with byte strings
- Binary-op left operands that are plain variables read post-right-operand
  values (Zend CV binding); dim/prop fetches still evaluate at their own
  position (execution_order)
- `include*` emits a warning pair on failed opens; `require*` emits the
  stream warning + `Uncaught Error` — verified against php 8.5 CLI
- `include`/`require` sit as internal-function frames in backtraces even
  for failed opens; builtin-machinery callbacks (ob handlers) show
  `[internal function]` while eval-position callbacks show their real site
- Non-static method called statically receives `$this` when the caller's
  `$this` is-an-instance-of the callee class (bug21961)
