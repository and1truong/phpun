# Compatibility notes — tests/lang @ 206/280 (73.6%)

Reference: PHP 8.5.11 (cli). Harness: `phpun phpt … -j 8`.

## Implemented so far (green areas)

- Tags, literals (int/float/string/heredoc/nowdoc/interpolation), constants;
  numeric literals: `_` separators, `0x`/`0b`/`0o` + octal/hex/bin overflow →
  float, leading-`0` decimal (`08`) → "Invalid numeric literal" parse error
- Variables, `$GLOBALS`, superglobals (partial), static vars (partial),
  `global $a, $b` multi-declaration
- Operators incl. PHP 8 comparison rules, `??`, `<=>`, bitwise on strings
  (`|` pads, `&`/`^` truncate), shifts (`<0` → ArithmeticError, `>=64` → 0);
  loose `==`/`<=>` compares ints as i64 (not via f64) when both sides are
  int-ish
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
  traits (basic), abstract, `new`, `$this`, static calls, magic
  `__get/__set/__call/__toString` (partial), built-in `stdClass`,
  `Throwable`/`Error`/`Exception` hierarchy (engine errors catchable)
- Param type enforcement: `Foo $a` → catchable `TypeError` with the PHP
  message incl. call/definition lines and a real `#0 file(N): f(Object(C))`
  trace frame; `P $p = null` → implicit-nullable Deprecated at declaration;
  `P $p = 42` → "Cannot use int as default value" compile fatal
- `list()`: "Cannot use int as array" / "Undefined array key N" warnings
- Diagnostics: `Warning:`/`Deprecated:`/`Notice:`/`Fatal error:` output
  format matching PHP CLI display_errors output (PHPT compares stdout);
  uncaught throwables print formatted trace frames + `thrown in` footer
- `error_reporting()` level mask (E_* bits honored)
- Builtins subset: string/array/math/type/printf-family, `print_r` nested
  container layout, eval, include (file paths), ob_*,
  set_error_handler/set_exception_handler
- PHPT harness: FILE/EXPECT/EXPECTF/EXPECTREGEX/SKIPIF/INI/ARGS/ENV/CLEAN,
  JSON report, failure diffs, crash/timeout classification

## Known failing clusters (next highest-leverage first)

| Cluster                            | ~tests | Missing semantic                                   |
|------------------------------------|--------|----------------------------------------------------|
| passByReference / returnByReference| ~17    | `&$var` references through args/returns (PR #1)    |
| numbered 008–044                   | ~13    | eval, `$this` ctor, alt syntax, exc handlers       |
| bugNNNNN singles                   | ~15    | assorted semantics                                 |
| syntax_errors                      | 1      | exact parse-error messages                         |
| unicode_escape_surrogates          | 1      | needs byte-string values (UTF-8 `Value::Str` limit)|
| bitwiseNot_variationStr            | 1      | same byte-string gap (`~"0"` → 0xCF)               |
| include_variation2/3               | 2      | include path resolution                            |
| static_basic_002/variation_001     | 2      | static-var edge cases                              |
| error_2_exception_001, zend_throw  | 2      | error→exception plumbing                           |
| catchable_error_002                | 1      | catchable fatal                                    |
| execution_order                    | 1      | operand evaluation order cases                     |
| compare_objects_basic2             | 1      | object compare handler                             |
| 045 timeout                        | 1      | register_shutdown_function timeout                 |
| short_tags.001                     | 1      | `short_open_tag` INI                               |

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
