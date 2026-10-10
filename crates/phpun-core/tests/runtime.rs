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
    let (out, err, code) = eval(
        "strict-builtin.php",
        r#"<?php
    declare(strict_types=1);
    foreach ([0,1,2,3] as $case) {
        try {
            if ($case===0) { parse_str([]); }
            if ($case===1) { call_user_func('parse_str', []); }
            if ($case===2) { parse_str([], $r, 3); }
            if ($case===3) { getopt([]); }
        } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    }
"#,
        &[],
    );
    assert_eq!(out, "ArgumentCountError: parse_str() expects exactly 2 arguments, 1 given\nArgumentCountError: parse_str() expects exactly 2 arguments, 1 given\nArgumentCountError: parse_str() expects exactly 2 arguments, 3 given\nTypeError: getopt(): Argument #1 ($short_options) must be of type string, array given\n");
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

#[test]
fn class_names_and_float_gates_preserve_coercion_and_errors() {
    let (out, err, code) = eval(
        "class-float.php",
        r#"<?php
function floating(float $n): float { return $n; }
function nullable(?float $n): ?float { return $n; }
function widened(): float { return 7; }
function rejected(): float { return 'bad'; }
var_dump(floating(1.5), floating(7), floating('2.5'), nullable(null), nullable(2), widened());
try { rejected(); } catch (Throwable $e) { echo get_class($e),"\n"; }
class OrdinaryName { public function __construct(public int $n) {} }
$o=new OrdinaryName(3); echo get_class($o),' ',$o->n,"\n";
$a=new class { public function read() { return 4; } }; echo strpos(get_class($a),'@anonymous') !== false ? 'anonymous' : 'bad', ' ', $a->read(),"\n";
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"float(1.5)
float(7)
float(2.5)
NULL
float(2)
float(7)
TypeError
OrdinaryName 3
anonymous 4
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn plain_property_cache_guards_class_scope_missing_slots_and_hooks() {
    let (out, err, code) = eval(
        "property-cache.php",
        r#"<?php
class BaseSlot { public int $x=5; public function read(): int { return $this->x; } }
class ChildSlot extends BaseSlot { public int $x=8; }
class HookSlot extends BaseSlot { public int $x=9 { get { echo "hook\n"; return $this->x; } } }
$a=new BaseSlot();$b=new ChildSlot();$h=new HookSlot();
foreach([$a,$a,$b,$a,$h,$a] as $o) { echo $o->read(),"\n"; }
class MagicSlot { public int $x=10; public function read(): int {return $this->x;} public function clear() {unset($this->x);} public function put($v) {$this->x=$v;} public function __get($n) {return 42;} }
$m=new MagicSlot();echo $m->read(),' ',$m->read(),"\n";$m->clear();echo $m->read(),"\n";$m->put(12);echo $m->read(),"\n";
class UninitSlot { public int $x; public function read(): int {return $this->x;} public function put($v) {$this->x=$v;} }
$u=new UninitSlot();try{$u->read();}catch(Throwable $e){echo $e->getMessage(),"\n";} $u->put(3); echo $u->read(),' ',$u->read(),"\n";
class PrivateParent { private int $x=4; public function reader(){return fn()=>$this->x;} }
class PrivateChild extends PrivateParent {private int $x=14;}
$p=new PrivateChild();$f=$p->reader();echo $f(),' ',$f(),"\n";$g=$f->bindTo($p,PrivateChild::class);echo $g(),' ',$f(),' ',$g(),"\n";
$t=new BaseSlot();$w=WeakReference::create($t);echo $t->read(),"\n";unset($t);var_dump($w->get());
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"5
5
8
5
hook
9
5
10 10
42
12
Typed property UninitSlot::$x must not be accessed before initialization
3 3
4 4
14 4 14
5
NULL
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn scalar_coercion_order_preserves_union_preference_and_constructor_args() {
    let (out, err, code) = eval(
        "coercion.php",
        r#"<?php
function ni(int|float $x) { var_dump($x); }
function fs(float|string $x) { var_dump($x); }
function bi(false|int $x) { var_dump($x); }
function fl(FloAt $x) { var_dump($x); }
function nb(bool|array $x) { var_dump($x); }
function ref_float(float &$x) { var_dump($x); }
foreach (["42", "42.0", "2e2", true] as $v) ni($v);
foreach ([42, "42", false] as $v) fs($v);
foreach ([false, true, "2"] as $v) bi($v);
fl(7); fl("2.5"); nb([]); nb(2);
$x = 3; ref_float($x); var_dump($x);
class Converted { function __construct(public float $x, public float|string $y) { var_dump(func_get_args()); } }
$c = new Converted(4, 5); var_dump($c->x, $c->y);
class Text { function __toString() { echo "cast\n"; return "ok"; } }
function text(string $x) { var_dump($x); }
text(new Text);
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"int(42)
float(42)
float(200)
int(1)
float(42)
string(2) "42"
float(0)
bool(false)
int(1)
int(2)
float(7)
float(2.5)
array(0) {
}
bool(true)
float(3)
float(3)
array(2) {
  [0]=>
  float(4)
  [1]=>
  float(5)
}
float(4)
float(5)
cast
string(2) "ok"
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn promoted_slot_binding_preserves_args_hooks_readonly_and_fallbacks() {
    let (out, err, code) = eval(
        "promoted.php",
        r#"<?php
class Promoted {
    function __construct(public int $x = 1, private float $y = 2.0) {
        var_dump(func_get_args(), $this->x, $this->y);
        $x = 99; echo "local $x property $this->x\n";
    }
}
new Promoted(3, 4);
new Promoted;
new Promoted(y: 5);
class ChildPromoted extends Promoted {}
new ChildPromoted(...[7, 8]);
try { new Promoted([]); } catch (TypeError $e) { echo "bad type\n"; }
class Locked {
    function __construct(public readonly int $x) { echo "locked $x\n"; }
}
$r = new Locked(6);
try { $r->__construct(8); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class HookedCtor {
    function __construct(public int $x {
        set {
            $bt = debug_backtrace();
            echo "hook $value ", $bt[1]['function'], ' ', $bt[1]['args'][0], "\n";
            if ($value < 0) throw new Exception('negative');
            $this->x = $value * 2;
        }
    }) { echo "body $x $this->x\n"; }
}
new HookedCtor(4);
try { new HookedCtor(-1); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
class DefaultCtor {
    const N = 12;
    function __construct(public int $x = self::N) { echo "default $x\n"; }
}
new DefaultCtor;
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"array(2) {
  [0]=>
  int(3)
  [1]=>
  float(4)
}
int(3)
float(4)
local 99 property 3
array(0) {
}
int(1)
float(2)
local 99 property 1
array(2) {
  [0]=>
  int(1)
  [1]=>
  float(5)
}
int(1)
float(5)
local 99 property 1
array(2) {
  [0]=>
  int(7)
  [1]=>
  float(8)
}
int(7)
float(8)
local 99 property 7
bad type
locked 6
Cannot modify readonly property Locked::$x
hook 4 __construct 4
body 4 8
hook -1 __construct -1
negative
default 12
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn borrowed_concat_preserves_bytes_aliases_and_conversion_order() {
    let (out, err, code) = eval(
        "concat.php",
        r#"<?php
function joined($a, $b) { return $a . $b; }
$s = "\xff\0a"; $alias = $s; $s .= "\0b";
echo bin2hex($s), ' ', bin2hex($alias), ' ', bin2hex(joined($alias, "\xfe")), "\n";
$a = 'A'; $a .= ($a = 'B'); echo "$a\n";
$b = 'A'; echo $b . ($b = 'B'), "\n";
class Piece {
    function __construct(private string $name, private bool $throw = false) {}
    function __toString() { echo "cast $this->name\n"; if ($this->throw) throw new Exception($this->name); return $this->name; }
}
echo joined(new Piece('L'), new Piece('R')), "\n";
try { joined(new Piece('stop', true), new Piece('never')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
set_error_handler(function ($n, $m) { echo "$m\n"; return true; });
echo joined([], 7), "\n";
restore_error_handler();
var_dump(joined(null, false), joined(true, 2.5));
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"ff00610062 ff0061 ff0061fe
BB
BB
cast L
cast R
LR
cast stop
stop
Array to string conversion
Array7
string(0) ""
string(4) "12.5"
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn pooled_frames_snapshot_method_context_args_and_release_receivers() {
    let (out, err, code) = eval(
        "frame.php",
        r#"<?php
function trace_leaf(int $n) { return debug_backtrace(0, 4); }
function trace_recur(int $n) { return $n ? trace_recur($n - 1) : trace_leaf($n); }
class TraceOwner {
    function run(int $n) { return trace_recur($n); }
    static function fail(int $n): int { throw new Exception('kept'); }
}
$o = new TraceOwner;
$weak = WeakReference::create($o);
$trace = $o->run(1);
foreach ($trace as $f) echo ($f['class'] ?? ''), ($f['type'] ?? ''), $f['function'], ':', json_encode($f['args']), "\n";
unset($trace, $f, $o);
var_dump($weak->get());
try { TraceOwner::fail(7); } catch (Exception $e) { $kept = $e; }
trace_recur(2);
foreach ($kept->getTrace() as $f) echo ($f['class'] ?? ''), ($f['type'] ?? ''), $f['function'], ':', json_encode($f['args']), "\n";
set_error_handler(function ($n, $m) { $f = debug_backtrace(0, 2); echo $f[1]['function'], ':', json_encode($f[1]['args']), "\n"; return true; });
function trace_warn(int $n) { $x = []; return $x[$n]; }
trace_warn(9);
restore_error_handler();
"#,
        &[],
    );
    assert_eq!(out, "trace_leaf:[0]\ntrace_recur:[0]\ntrace_recur:[1]\nTraceOwner->run:[1]\nNULL\nTraceOwner::fail:[7]\ntrace_warn:[9]\n");
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn promoted_reference_properties_preserve_aliases_and_type_owners() {
    let (out, err, code) = eval(
        "promoted-ref.php",
        r#"<?php
class RefBox {
    function __construct(public int &$x) { $x += 1; }
}
$x = 1; $a = new RefBox($x); $a->x = 9; echo "$x $a->x\n";
$x = 10; echo "$a->x\n";
try { $x = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
unset($a); $x = []; echo gettype($x), "\n";
class PrivateRef {
    function __construct(private int &$x) {}
    function put(int $n) { $this->x = $n; }
    function get() { return $this->x; }
}
$v = 2; $p = new PrivateRef(x: $v); $p->put(12); echo "$v ", $p->get(), "\n";
class LockedRef { function __construct(public readonly int &$x) {} }
try { new LockedRef($v); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class LockedObjectRef { public readonly object $x; function __construct() { $this->x = new stdClass; } }
$lockedObject = new LockedObjectRef; $replacement = new stdClass;
try { $lockedObject->x =& $replacement; } catch (Error $e) { echo $e->getMessage(), "\n"; }
class PairRef { function __construct(public int &$a, public int &$b) {} }
$n = 4; $q = new PairRef($n, $n); $q->a++; echo "$n $q->a $q->b\n";
class HookRef { function __construct(public int &$x { set { $this->x = $value; } }) {} }
try { new HookRef($v); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#,
        &[],
    );
    assert_eq!(out, "9 9\n10\nCannot assign array to reference held by property RefBox::$x of type int\narray\n12 12\nCannot indirectly modify readonly property LockedRef::$x\nCannot assign by reference to overloaded object\n5 5 5\nTyped property HookRef::$x must not be accessed before initialization\n");
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn streaming_json_preserves_flags_nested_keys_hooks_and_utf8() {
    let (out, err, code) = eval(
        "json-sink.php",
        r#"<?php
class StreamJson implements JsonSerializable { function jsonSerialize(): mixed { echo "serialize\n"; return ['ok' => 3]; } }
class HookJson {
    public string $name { get { echo "get\n"; return 'hook'; } }
    private int $hidden = 9;
    public int $x = 2;
}
$payload = ['text' => "<>&'\"/\n\r\t\0\\é😀", 'nested' => [[null, true, false, 12, 2.5], [2 => 'two', 'x' => 'key']], 'empty' => []];
foreach ([0, 15, 64, 256, 320] as $flags) echo json_encode($payload, $flags), "\n";
echo json_encode([new StreamJson, new HookJson]), "\n";
$s = '{"ascii":"user-123@example.com","utf8":"é😀","escaped":"a\\u0000b\\n","a":[1,true,null]}';
$a = json_decode($s, true);
echo $a['ascii'], ' ', bin2hex($a['utf8']), ' ', bin2hex($a['escaped']), ' ', json_encode($a['a']), "\n";
echo json_encode(json_decode('{"x":1,"nested":{"k":"v"}}')), "\n";
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"{"text":"<>&'\"\/\n\r\t\u0000\\\u00e9\ud83d\ude00","nested":[[null,true,false,12,2.5],{"2":"two","x":"key"}],"empty":[]}
{"text":"\u003C\u003E\u0026\u0027\u0022\/\n\r\t\u0000\\\u00e9\ud83d\ude00","nested":[[null,true,false,12,2.5],{"2":"two","x":"key"}],"empty":[]}
{"text":"<>&'\"/\n\r\t\u0000\\\u00e9\ud83d\ude00","nested":[[null,true,false,12,2.5],{"2":"two","x":"key"}],"empty":[]}
{"text":"<>&'\"\/\n\r\t\u0000\\é😀","nested":[[null,true,false,12,2.5],{"2":"two","x":"key"}],"empty":[]}
{"text":"<>&'\"/\n\r\t\u0000\\é😀","nested":[[null,true,false,12,2.5],{"2":"two","x":"key"}],"empty":[]}
serialize
get
[{"ok":3},{"name":"hook","x":2}]
user-123@example.com c3a9f09f9880 6100620a [1,true,null]
{"x":1,"nested":{"k":"v"}}
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}

#[test]
fn slot_immediates_preserve_numeric_fallbacks_errors_and_evaluation_order() {
    let (out, err, code) = eval(
        "slot.php",
        r#"<?php
function immediate($n) { return [$n + 1, $n < 2, $n * -3, $n === null, $n == false, $n . 'x']; }
foreach ([0, 1, 3, 2.5, '4', null] as $n) echo json_encode(immediate($n)), "\n";
function loops($n) { $s = 0; for ($i = 0; $i < $n; $i++) { if ($i % 2) continue; $s += 3; if ($s > 10) break; } return $s; }
echo loops(20), "\n";
function overflow($n) { return $n + 1; }
var_dump(gettype(overflow(PHP_INT_MAX)));
function zero($n) { return $n / 0; }
try { zero(3); } catch (DivisionByZeroError $e) { echo $e->getMessage(), ' ', $e->getTrace()[0]['function'], "\n"; }
function changed($n) { $n += 3; return $n; }
echo changed(4), "\n";
function order($n) { return $n + ($n = 5); }
echo order(1), "\n";
$x = 'a'; $x .= 'b'; $alias = $x; $x .= ($x = 'c'); echo "$x $alias\n";
"#,
        &[],
    );
    assert_eq!(
        out,
        r#"[1,true,0,false,true,"0x"]
[2,true,-3,false,false,"1x"]
[4,false,-9,false,false,"3x"]
[3.5,false,-7.5,false,false,"2.5x"]
[5,false,-12,false,false,"4x"]
[1,true,0,true,true,"x"]
12
string(6) "double"
Division by zero zero
7
10
cc ab
"#
    );
    assert_eq!(err, "");
    assert_eq!(code, 0);
}
