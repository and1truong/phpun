//! Formatter tests (`phpun fmt`, issue #30): each case is an input →
//! expected PSR-12-shaped output pair, plus an idempotency invariant
//! over every case (formatting formatted output is a no-op).

use phpun_core::fmt::{format, unified_diff};

fn fmt(src: &str) -> String {
    format(src).expect("format failed")
}

/// Every case formats to its expectation AND is a fixpoint.
fn check(cases: &[(&str, &str)]) {
    for (src, want) in cases {
        let got = fmt(src);
        assert_eq!(*want, got, "format mismatch for:\n{src}");
        let again = fmt(&got);
        assert_eq!(got, again, "not idempotent for:\n{src}");
    }
}

#[test]
fn statements_and_indent() {
    check(&[(
        "<?php\n$a=1;$b=2;\nfunction f(int $x):int{return $x*2;}\n",
        "<?php\n$a = 1;\n$b = 2;\nfunction f(int $x): int\n{\n    return $x * 2;\n}\n",
    )]);
}

#[test]
fn class_and_brace_placement() {
    check(&[(
        "<?php\nclass Foo extends Bar implements Baz{\nprivate int $x=1;\npublic function bar():static{return $this;}\n}\n",
        "<?php\nclass Foo extends Bar implements Baz\n{\n    private int $x = 1;\n    public function bar(): static\n    {\n        return $this;\n    }\n}\n",
    )]);
    // control braces stay on the same line; decl braces on their own
    check(&[(
        "<?php\nif($a>0){one();}elseif($a<0){two();}else{three();}\n",
        "<?php\nif ($a > 0) {\n    one();\n} elseif ($a < 0) {\n    two();\n} else {\n    three();\n}\n",
    )]);
}

#[test]
fn decl_head_keywords() {
    check(&[(
        "<?php\ninterface I extends A,B{public function m():void;}\nreadonly class R{public function __construct(private int $x){}}\nenum E:string{case A='a';}\n",
        "<?php\ninterface I extends A, B\n{\n    public function m(): void;\n}\nreadonly class R\n{\n    public function __construct(private int $x)\n    {}\n}\nenum E: string\n{\n    case A = 'a';\n}\n",
    )]);
}

#[test]
fn switch_case() {
    check(&[(
        "<?php\nswitch($a){case 1:one();break;case 2:case 3:two();break;default:d();}\n",
        "<?php\nswitch ($a) {\n    case 1:\n        one();\n        break;\n    case 2:\n    case 3:\n        two();\n        break;\n    default:\n        d();\n}\n",
    )]);
}

#[test]
fn match_arms() {
    check(&[(
        "<?php\n$m=match($x){1,2=>'low',3=>'high',default=>'none'};\n",
        "<?php\n$m = match ($x) {\n    1, 2 => 'low',\n    3 => 'high',\n    default => 'none'\n};\n",
    )]);
}

#[test]
fn try_catch() {
    check(&[(
        "<?php\ntry{f();}catch(A|B $e){g();}finally{h();}\n",
        "<?php\ntry {\n    f();\n} catch (A|B $e) {\n    g();\n} finally {\n    h();\n}\n",
    )]);
}

#[test]
fn closures_and_fns() {
    check(&[(
        "<?php\n$fn=function()use($a){return $a+1;};\n$arrow=fn($x)=>$x*2;\n$anon=new class($x){function m(){}};\n",
        "<?php\n$fn = function () use ($a) {\n    return $a + 1;\n};\n$arrow = fn ($x) => $x * 2;\n$anon = new class($x) {\n    function m()\n    {}\n};\n",
    )]);
}

#[test]
fn operators() {
    check(&[(
        "<?php\n$y=(int)$a+(float)$b;$z=$a?$b??1:$c;$u=$a?:$b;$p=$q?->m();\n",
        "<?php\n$y = (int) $a + (float) $b;\n$z = $a ? $b ?? 1 : $c;\n$u = $a ?: $b;\n$p = $q?->m();\n",
    )]);
    // unary vs binary -/+/&/|
    check(&[(
        "<?php\n$a=$b&$c|$d;$e=-$f+!$g;$h=intval(&$i);\n",
        "<?php\n$a = $b & $c | $d;\n$e = -$f + !$g;\n$h = intval(&$i);\n",
    )]);
}

#[test]
fn types() {
    check(&[(
        "<?php\nfunction f(int &$a,?string $b=null):int|float{return 0;}\nstatic ?\\Closure $c=null;\n",
        "<?php\nfunction f(int &$a, ?string $b = null): int|float\n{\n    return 0;\n}\nstatic ?\\Closure $c = null;\n",
    )]);
    // ternary `?` vs nullable `?` after type-name keywords
    check(&[(
        "<?php\n$x=$a?static::$b:null;$y=$a?null:1;\n",
        "<?php\n$x = $a ? static::$b : null;\n$y = $a ? null : 1;\n",
    )]);
}

#[test]
fn call_keywords_tight() {
    check(&[(
        "<?php\nisset($a)&&empty($b);list($x,$y)=f();eval('$z=1');exit(0);\n",
        "<?php\nisset($a) && empty($b);\nlist($x, $y) = f();\neval('$z=1');\nexit(0);\n",
    )]);
}

#[test]
fn comments() {
    check(&[(
        "<?php\n// top\n$a=1;// trail\n/* block */\n/** doc\n * line\n */\n$b=2;\n",
        "<?php\n// top\n$a = 1; // trail\n/* block */\n/** doc\n * line\n */\n$b = 2;\n",
    )]);
}

#[test]
fn inline_html() {
    check(&[(
        "<?php\n$x=1;?>\n<p><?=$x?></p>\n<?php echo \"tail\"; ?>\n",
        "<?php\n$x = 1; ?>\n<p><?= $x ?></p>\n<?php\necho \"tail\"; ?>\n",
    )]);
}

#[test]
fn shebang() {
    check(&[(
        "#!/usr/bin/env php\n<?php $x=1;\n",
        "#!/usr/bin/env php\n<?php\n$x = 1;\n",
    )]);
}

#[test]
fn named_args_and_attrs() {
    check(&[(
        "<?php\nfoo(named:1,other:2);\n#[Attr(1,\"x\")]\nfunction f(#[Inject] string $dep){}\n",
        "<?php\nfoo(named: 1, other: 2);\n#[Attr(1, \"x\")]\nfunction f(#[Inject] string $dep)\n{}\n",
    )]);
}

#[test]
fn alt_syntax() {
    check(&[(
        "<?php\nif($a):one();elseif($b):two();else:three();endif;\nfor($i=0;$i<3;$i++):body();endfor;\n",
        "<?php\nif ($a):\n    one();\nelseif ($b):\n    two();\nelse:\n    three();\nendif;\nfor ($i = 0; $i < 3; $i++):\n    body();\nendfor;\n",
    )]);
}

#[test]
fn declare_and_use() {
    check(&[(
        "<?php declare(strict_types=1);\nnamespace A\\B;use C\\{D,E};use function F\\g;\n",
        "<?php\ndeclare(strict_types=1);\nnamespace A\\B;\nuse C\\{D, E};\nuse function F\\g;\n",
    )]);
}

#[test]
fn blank_lines_preserved() {
    check(&[(
        "<?php\n\n/* header */\n$a=1;\n\n\n$b=2;\n",
        "<?php\n\n/* header */\n$a = 1;\n\n$b = 2;\n",
    )]);
}

#[test]
fn goto_label() {
    check(&[(
        "<?php\nstart:\ngoto start;\n",
        "<?php\nstart:\ngoto start;\n",
    )]);
}

#[test]
fn unified_diff_basic() {
    let d = unified_diff("a\nb\nc\n", "a\nx\nc\n", 3);
    assert!(d.contains("@@ -1,3 +1,3 @@"), "hunk header: {d}");
    assert!(d.contains("-b"), "del line: {d}");
    assert!(d.contains("+x"), "add line: {d}");
    assert_eq!("", unified_diff("same\n", "same\n", 3));
}
