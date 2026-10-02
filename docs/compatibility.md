# Compatibility notes — tests/lang @ 140/280 (50.0%)

Reference: PHP 8.5.11 (cli). Harness: `phpun phpt … -j 8`.

## Implemented so far (green areas)

- Tags, literals (int/float/string/heredoc/nowdoc/interpolation), constants
- Variables, `$GLOBALS`, superglobals (partial), static vars (partial)
- Operators incl. PHP 8 comparison rules, `??`, `<=>`, bitwise on strings
  (`|` pads, `&`/`^` truncate), shifts (`<0` → ArithmeticError, `>=64` → 0)
- Int/float/string coercions: int overflow → float promotion,
  leading-numeric strings → "A non-numeric value encountered" + partial parse,
  out-of-range float → "not representable as an int" warning + mod-2^64 wrap
- Control flow: if/else, while, do, for, foreach (arrays, Iterator,
  IteratorAggregate), switch, match, try/catch/finally, throw
- Functions: default/variadic/named args, closures, arrow fns, first-class
  callable syntax, static methods, func_get_args (partial)
- Classes: properties, methods, visibility, inheritance, interfaces,
  traits (basic), abstract, `new`, `$this`, static calls, magic
  `__get/__set/__call/__toString` (partial), built-in `stdClass`,
  `Throwable`/`Error`/`Exception` hierarchy (engine errors catchable)
- Diagnostics: `Warning:`/`Deprecated:`/`Notice:`/`Fatal error:` output
  format matching PHP CLI display_errors output (PHPT compares stdout)
- `error_reporting()` level mask (E_* bits honored)
- Builtins subset: string/array/math/type/printf-family, eval, include
  (file paths), ob_* , set_error_handler/set_exception_handler
- PHPT harness: FILE/EXPECT/EXPECTF/EXPECTREGEX/SKIPIF/INI/ARGS/ENV/CLEAN,
  JSON report, failure diffs, crash/timeout classification

## Known failing clusters (next highest-leverage first)

| Cluster                            | ~tests | Missing semantic                                   |
|------------------------------------|--------|----------------------------------------------------|
| passByReference / returnByReference| ~18    | `&$var` references through args/returns            |
| unicode_escape_*                   | 7      | `\u{...}` validation errors + surrogate handling   |
| func_get_arg(s)/func_num_args misc | ~6     | arg reflection outside fns, by-ref capture         |
| foreachLoopObjects + references    | ~6     | foreach by-ref, prop mutation during iteration     |
| 007/008/023/024/028/030/033/035+   | ~15    | globals/statics in fns, eval, `$this` rules        |
| hexadecimal/octal/binary_64bit     | 4      | numeric-string → int for hex/oct/bin formats       |
| type_hints_00x, syntax_errors      | 4      | strict-ish type checks, parse-error messages       |
| ~60 misc singles                   | 60     | assorted semantics                                 |
| bitwiseNot_variationStr            | 1      | needs byte-string values (UTF-8 `Value::Str` limit)|

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
