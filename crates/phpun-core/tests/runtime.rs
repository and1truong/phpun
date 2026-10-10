//! Runtime self-tests: drive the interpreter on PHP snippets and fixture
//! directories, asserting exact stdout. These are the regression net for
//! semantics bugs fixed along the way — each case names the PHPT test or
//! issue that motivated it where one exists.
//!
//! Fixtures live under `tests/fixtures/`:
//!   - `name.php` + `name.expect`     — single file, exact stdout match
//!   - `name.php` + `name.expectf`    — EXPECTF pattern match
//!   - `dir/main.php` + `dir/expect` — multi-file fixture dir (sibling
//!     .php files are includeable)

use phpun_core::Interp;
use std::path::{Path, PathBuf};

fn eval(file: &str, src: &str, ini: &[(&str, &str)]) -> (String, String, i32) {
    let mut it = Interp::new(file);
    for (k, v) in ini {
        it.ini.insert(k.to_string(), v.to_string());
    }
    let r = it.run_source(src);
    (
        String::from_utf8_lossy(&it.out).into_owned(),
        it.err_buf,
        r.exit_code,
    )
}

fn expectf_matches(expected: &str, actual: &str) -> bool {
    let body = phpun_phpt::expectf::expectf_to_regex(expected.trim());
    phpun_phpt::expectf::anchored(&body)
        .map(|re| re.is_match(actual.trim()))
        .unwrap_or(false)
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn run_fixture(entry: &Path) -> Result<String, String> {
    let abs = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let abs_s = abs.display().to_string();
    let src =
        std::fs::read_to_string(&abs).map_err(|e| format!("read {}: {}", abs.display(), e))?;
    // Fixtures run under the run-tests.ini defaults (log_errors=0,
    // fatal_error_backtraces=Off) — the same env `phpun phpt` uses.
    let (out, _err, _code) = eval(
        &abs_s,
        &src,
        &[("log_errors", "0"), ("fatal_error_backtraces", "Off")],
    );
    let stem = entry.with_extension("");
    let expectf = stem.with_extension("expectf");
    let expect = stem.with_extension("expect");
    if expectf.exists() {
        let exp = std::fs::read_to_string(&expectf).unwrap();
        if !expectf_matches(&exp, &out) {
            return Err(format!(
                "{}: EXPECTF mismatch\n--- expected\n{}\n--- actual\n{}",
                entry.display(),
                exp.trim(),
                out.trim()
            ));
        }
    } else if expect.exists() {
        let exp = std::fs::read_to_string(&expect).unwrap();
        if exp.trim_end() != out.trim_end() {
            return Err(format!(
                "{}: EXPECT mismatch\n--- expected\n{}\n--- actual\n{}",
                entry.display(),
                exp.trim_end(),
                out.trim_end()
            ));
        }
    } else {
        return Err(format!("{}: no .expect/.expectf file", entry.display()));
    }
    Ok(String::new())
}

/// Multi-file fixture directory: `dir/main.php` is the entry, the dir's
/// `expect`/`expectf` file holds the expected output.
fn run_fixture_dir(dir: &Path) -> Result<String, String> {
    let entry = dir.join("main.php");
    let abs = entry.canonicalize().unwrap_or_else(|_| entry.to_path_buf());
    let abs_s = abs.display().to_string();
    let src =
        std::fs::read_to_string(&abs).map_err(|e| format!("read {}: {}", abs.display(), e))?;
    let (out, _err, _code) = eval(
        &abs_s,
        &src,
        &[("log_errors", "0"), ("fatal_error_backtraces", "Off")],
    );
    let expectf = dir.join("expectf");
    let expect = dir.join("expect");
    if expectf.exists() {
        let exp = std::fs::read_to_string(&expectf).unwrap();
        if !expectf_matches(&exp, &out) {
            return Err(format!(
                "{}: EXPECTF mismatch\n--- expected\n{}\n--- actual\n{}",
                dir.display(),
                exp.trim(),
                out.trim()
            ));
        }
    } else {
        let exp =
            std::fs::read_to_string(&expect).map_err(|e| format!("{}: {}", dir.display(), e))?;
        if exp.trim_end() != out.trim_end() {
            return Err(format!(
                "{}: EXPECT mismatch\n--- expected\n{}\n--- actual\n{}",
                dir.display(),
                exp.trim_end(),
                out.trim_end()
            ));
        }
    }
    Ok(String::new())
}

#[test]
fn fixtures() {
    let root = fixture_dir();
    let mut failures = Vec::new();
    // Single-file fixtures: name.php paired with name.expect(f).
    for e in std::fs::read_dir(&root).unwrap().flatten() {
        let p = e.path();
        if p.is_file() && p.extension().is_some_and(|x| x == "php") {
            if let Err(m) = run_fixture(&p) {
                failures.push(m);
            }
        } else if p.is_dir() && p.join("main.php").exists() {
            if let Err(m) = run_fixture_dir(&p) {
                failures.push(m);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ---- inline regression cases ----

#[test]
fn named_args_func_get_args() {
    // Zend binds named args into the CV table positionally — a named call
    // fills to the highest bound param (named_params/func_get_args).
    let (out, _, _) = eval(
        "/t.php",
        r#"<?php
function test($a = 'a', $b = 'b', $c = 'c') {
    var_dump(func_num_args(), func_get_args());
}
test(c: 'C', a: 'A');
"#,
        &[],
    );
    assert!(out.contains("int(3)"), "{}", out);
}

#[test]
fn named_args_positional_after_named_is_compile_fatal() {
    let (out, _, code) = eval("/t.php", "<?php\ntest(\"A\", a: \"B\", \"C\");\n", &[]);
    assert_eq!(code, 255);
    assert!(
        out.contains("Fatal error: Cannot use positional argument after named argument"),
        "{}",
        out
    );
}

#[test]
fn attribute_reflection_new_instance() {
    let (out, _, code) = eval(
        "/t.php",
        r#"<?php
#[Attribute(Attribute::TARGET_CLASS)]
class A { public function __construct(public int $flags = 1) {} }
#[A(flags: 4)]
class C {}
$attrs = (new ReflectionClass('C'))->getAttributes();
echo $attrs[0]->getName(), " ";
$a = $attrs[0]->newInstance();
echo $a->flags, "\n";
"#,
        &[],
    );
    assert_eq!(code, 0, "{}", out);
    assert_eq!(out, "A 4\n");
}

#[test]
fn closure_bind_scope_isolation() {
    // Composer's composerRequire uses Closure::bind to scope-isolate
    // include (issue #5).
    let (out, _, code) = eval(
        "/t.php",
        r#"<?php
$f = \Closure::bind(static function () { return "bound"; }, null, null);
echo $f(), "\n";
var_dump(\Closure::fromCallable('strlen'));
"#,
        &[],
    );
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("bound"));
}

#[test]
fn static_prop_default_dir_is_declaring_file() {
    // __DIR__ in a class const-expr binds lexically — /dir/sub, not the
    // requiring file's dir (composer autoload_static.php).
    let dir = std::env::temp_dir().join(format!("phpun_t{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("sub").join("b.php"),
        "<?php\nclass C { public static $d = __DIR__; }\n",
    )
    .unwrap();
    let a = dir.join("a.php");
    std::fs::write(&a, "<?php\nrequire __DIR__ . '/sub/b.php';\necho C::$d;\n").unwrap();
    let (out, _, code) = eval(
        &a.display().to_string(),
        &std::fs::read_to_string(&a).unwrap(),
        &[],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, 0, "{}", out);
    assert_eq!(out.trim_end(), dir.join("sub").display().to_string());
}

#[test]
fn globals_view_aliases() {
    // $GLOBALS['List'] =& $this — writing through the global view aliases
    // the object (tests/lang/030 shape).
    let (out, _, _) = eval(
        "/t.php",
        r#"<?php
$GLOBALS['g'] = 1;
$g = 5;
echo $GLOBALS['g'], "\n";
"#,
        &[],
    );
    assert_eq!(out, "5\n");
}

#[test]
fn trampoline_frames_hidden() {
    // call_user_func internal frames don't appear in backtraces
    // (named_params/backtrace).
    let (out, _, _) = eval(
        "/t.php",
        r#"<?php
function t() { debug_print_backtrace(); }
call_user_func('t');
"#,
        &[],
    );
    assert!(!out.contains("call_user_func"), "{}", out);
    assert!(out.contains("t()"), "{}", out);
}

#[test]
fn dynamic_builtin_reference_sends_and_zpp_types() {
    let (out, err, code) = eval(
        "dynamic-builtin.php",
        r#"<?php
set_error_handler(function($n,$m){ echo "WARN $m\n"; });
$r = 'keep';
call_user_func('parse_str', 'a=1', $r); var_dump($r);
call_user_func_array('parse_str', ['a=1', $r]); var_dump($r);
call_user_func_array('parse_str', ['a=1', &$r]); var_dump($r);
$r = 'keep';
call_user_func_array('parse_str', ['result'=>&$r, 'string'=>'b=2']); var_dump($r);
$r = 'keep';
call_user_func_array('parse_str', ['result'=>$r, 'string'=>'b=2']); var_dump($r);
foreach (['x', null] as $bad) {
    try { getopt('a:', $bad); } catch (Throwable $e) { echo $e->getMessage(),"\n"; }
    try { getopt(long_options:$bad, short_options:'a:'); } catch (Throwable $e) { echo $e->getMessage(),"\n"; }
}
$r = 'keep';
try { parse_str([], $r); } catch (Throwable $e) { echo $e->getMessage(),"\n"; } var_dump($r);
try { parse_str(result:$r, string:[]); } catch (Throwable $e) { echo $e->getMessage(),"\n"; } var_dump($r);
try { call_user_func('parse_str', [], $r); } catch (Throwable $e) { echo $e->getMessage(),"\n"; } var_dump($r);
set_error_handler(function($n,$m){ throw new Exception('warning intercepted'); });
try { call_user_func('parse_str', 'a=1', $r); } catch (Throwable $e) { echo $e->getMessage(),"\n"; } var_dump($r);
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"WARN parse_str(): Argument #2 ($result) must be passed by reference, value given
string(4) "keep"
WARN parse_str(): Argument #2 ($result) must be passed by reference, value given
string(4) "keep"
array(1) {
  ["a"]=>
  string(1) "1"
}
array(1) {
  ["b"]=>
  string(1) "2"
}
WARN parse_str(): Argument #2 ($result) must be passed by reference, value given
string(4) "keep"
getopt(): Argument #2 ($long_options) must be of type array, string given
getopt(): Argument #2 ($long_options) must be of type array, string given
getopt(): Argument #2 ($long_options) must be of type array, null given
getopt(): Argument #2 ($long_options) must be of type array, null given
parse_str(): Argument #1 ($string) must be of type string, array given
string(4) "keep"
parse_str(): Argument #1 ($string) must be of type string, array given
string(4) "keep"
WARN parse_str(): Argument #2 ($result) must be passed by reference, value given
parse_str(): Argument #1 ($string) must be of type string, array given
string(4) "keep"
warning intercepted
string(4) "keep"
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn scalar_value_calls_preserve_observed_arguments_and_cell_fallbacks() {
    let (out, err, code) = eval(
        "scalar-value.php",
        r#"<?php
function recur(int $n): int { return $n < 2 ? $n : recur($n-1) + recur($n-2); }
function untyped($n) { return $n < 2 ? $n : untyped($n-1) + untyped($n-2); }
function introspect(int $n) { return func_get_arg(0) + func_num_args() + count(func_get_args()); }
function capture() { return debug_backtrace(); }
function parent_arg(int $n) { return capture(); }
function bump(&$n) { $n++; }
function promote(int $n) { bump($n); return $n + func_get_arg(0); }
function overwrite(int $n) { $n=8; return func_get_arg(0); }
function bad_return(int $n): int { return 'bad'; }
function failure(int $n) { return bad_return($n); }
function divide(int $n) { return 10 / $n; }
function observe_error(int $n) { return trigger_error('probe'); }
function extra(int $n) { return func_num_args(); }
function optional(int $n=3) { return $n; }
function decimal(float $n): float { return $n; }
echo recur(12), ' ', untyped(12), ' ', introspect(7), ' ', promote(7), ' ', overwrite(7), "\n";
$t = parent_arg(9); var_dump($t[1]['args']);
try { failure(11); } catch (Throwable $e) { $t=$e->getTrace(); var_dump($t[0]['args'], $t[1]['args']); }
try { divide(0); } catch (Throwable $e) { $t=$e->getTrace(); var_dump($t[0]['args']); }
set_error_handler(function($n,$m) { $t=debug_backtrace(); var_dump($t[2]['args']); return true; });
observe_error(13);
echo extra(3,4), ' ', optional(), ' ', optional(n:5), ' ', decimal(7), "\n";
try { recur('not numeric'); } catch (Throwable $e) { echo "type-error\n"; }
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"144 144 9 16 8
array(1) {
  [0]=>
  int(9)
}
array(1) {
  [0]=>
  int(11)
}
array(1) {
  [0]=>
  int(11)
}
array(1) {
  [0]=>
  int(0)
}
array(1) {
  [0]=>
  int(13)
}
2 3 5 7
type-error
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn shared_string_replacement_preserves_bytes_counts_and_conversion_order() {
    let (out, err, code) = eval(
        "string-replace.php",
        r#"<?php
$s="fox\0DOG\xfffox"; $alias=$s;
$r=str_replace('fox','wolf',$s,$count); echo bin2hex($r),' ',$count,' ',bin2hex($s),"\n";
$r=str_ireplace(['FOX','dog'],['X','Y'],$s,$count); echo bin2hex($r),' ',$count,"\n";
$r=str_replace(['ab','x'],['x',''],'abab',$count); var_dump($r,$count);
$r=str_replace(['a','b'],['X'],['k'=>'ab',4=>'ba'],$count); var_dump($r,$count);
$r=str_replace(['','absent'],['x','y'],$s,$count); echo bin2hex($r),' ',$count,' ',bin2hex($alias),"\n";
$r=str_replace('aa','a','aaaaa',$count); var_dump($r,$count);
$r=str_replace('o','O',$s,$s); echo bin2hex($r),' ',$s,"\n";
class Text { public function __toString(): string { echo "cast\n"; return 'fox'; } }
$r=str_replace(new Text(), new Text(), new Text(),$count); var_dump($r,$count);
class BadText { public function __toString(): string { throw new Exception('cast-error'); } }
$count=99;
try { str_replace('x','y',new BadText(),$count); } catch(Throwable $e) { echo $e->getMessage()," ",$count,"\n"; }
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"776f6c6600444f47ff776f6c66 2 666f7800444f47ff666f78
580059ff58 3
string(0) ""
int(4)
array(2) {
  ["k"]=>
  string(1) "X"
  [4]=>
  string(1) "X"
}
int(4)
666f7800444f47ff666f78 0 666f7800444f47ff666f78
string(3) "aaa"
int(2)
664f7800444f47ff664f78 2
cast
cast
cast
string(3) "fox"
int(1)
cast-error 99
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}
