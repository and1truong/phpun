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
