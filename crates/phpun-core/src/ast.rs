use crate::lexer::StringPart;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Stmt {
    /// Parser-injected line marker — updates Interp::cur_line.
    Line(usize),
    /// Parser-injected compile-time deprecation (e.g. `case e;` —
    /// tests/lang/033). PHP prints these before execution begins.
    Deprecated {
        msg: String,
        line: usize,
    },
    Inline(String),
    Echo(Vec<Expr>),
    Expr(Expr),
    Block(Vec<Stmt>),
    If {
        cond: Expr,
        then: Vec<Stmt>,
        else_: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    DoWhile {
        body: Vec<Stmt>,
        cond: Expr,
    },
    For {
        init: Vec<Expr>,
        cond: Vec<Expr>,
        inc: Vec<Expr>,
        body: Vec<Stmt>,
    },
    Function(FunctionDecl),
    Return(Option<Expr>),
    Break(Option<Expr>),
    Continue(Option<Expr>),
    /// `global $a, $$b;` — each item is normally `Expr::Var`; `Expr::VarVar`
    /// resolves the global name dynamically (tests/lang/bug24396).
    Global(Vec<Expr>),
    /// `static $a = 1, $b;` — function-local persistent vars. `line` is the
    /// `static` keyword line (redeclaration detection).
    Static {
        vars: Vec<(String, Option<Expr>)>,
        line: usize,
    },
    Switch {
        cond: Expr,
        cases: Vec<(Option<Expr>, Vec<Stmt>)>,
    },
    Foreach {
        arr: Expr,
        key: Option<ForeachKey>,
        val: ForeachTarget,
        body: Vec<Stmt>,
    },
    Unset(Vec<Expr>),
    Try {
        body: Vec<Stmt>,
        catches: Vec<Catch>,
        finally: Option<Vec<Stmt>>,
    },
    /// `declare(...)` — parsed, most directives ignored.
    Declare {
        name: String,
        value: Expr,
    },
    /// `namespace Foo;` — parsed, names not yet mangled.
    Namespace(String),
    Class(Rc<ClassDecl>),
    /// `use TraitA, TraitB;` (top-level `use function`/`use const` too).
    Use(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    /// "class" | "interface" | "trait" | "enum" | "abstract class" | "final class"
    pub kind: ClassKind,
    pub is_abstract: bool,
    pub is_final: bool,
    pub parent: Option<String>,
    pub implements: Vec<String>,
    /// `use`d traits (inside the body).
    pub traits: Vec<String>,
    pub methods: Vec<Rc<MethodDecl>>,
    /// (name, default value expr, flags)
    pub props: Vec<PropDecl>,
    pub consts: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    Int,
    Float,
    String,
    Bool,
    Array,
    Object,
    Unset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Class,
    Interface,
    Trait,
    Enum,
}

#[derive(Debug, Clone)]
pub struct PropDecl {
    pub name: String,
    pub default: Option<Expr>,
    pub is_static: bool,
    pub visibility: Visibility,
    pub readonly: bool,
}

#[derive(Debug, Clone)]
pub struct MethodDecl {
    pub decl: FunctionDecl,
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

impl ClassDecl {
    /// Method lookup walking the `extends` chain (parents resolved via the
    /// interpreter's class table).
    pub fn find_method(&self, lname: &str) -> Option<Rc<MethodDecl>> {
        self.methods
            .iter()
            .find(|m| m.decl.name.to_lowercase() == lname)
            .cloned()
    }
}

#[derive(Debug, Clone)]
pub struct Catch {
    /// `catch (A|B $e)` — class names; empty matches anything.
    pub types: Vec<String>,
    pub var: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum ForeachKey {
    Var(String),
    /// `foreach ($a as &$k => $v)` is a fatal error in PHP.
    ByRef,
}

#[derive(Debug, Clone)]
pub enum ForeachTarget {
    Var(String),
    ByRef(String),
    /// Any other assignable lvalue (`$b[0]`, `$o->p`, ...).
    Lvalue(Box<Expr>),
    List(Vec<Option<ForeachTarget>>),
}

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    /// `&name(` — returns by reference.
    pub by_ref: bool,
    /// Source line of the `function` keyword (for TypeError "defined in"
    /// and compile-time deprecation diagnostics).
    pub line: usize,
    /// File the decl was registered from — PHP resolves includes relative
    /// to the file containing the call site (include_variation2).
    pub file: String,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub default: Option<Expr>,
    pub by_ref: bool,
    pub variadic: bool,
    /// Declared type members in source order; "null" included when the
    /// type is explicitly nullable (`?T` or `T|null`).
    pub ty: Option<Vec<String>>,
    /// Constructor property promotion (`public $errno` in `__construct`):
    /// declare the prop and auto-assign at call time
    /// (error_2_exception_001).
    pub promoted: bool,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Interp(Vec<StringPart>),
    Var(String),
    /// Unqualified constant read (true/false/null are separate variants).
    Const(String),
    ArrayLit(Vec<(Option<Expr>, Expr)>),
    /// `&expr` inside an array literal — element is bound by reference.
    ByRef(Box<Expr>),
    Assign {
        target: Box<Expr>,
        op: &'static str,
        value: Box<Expr>,
    },
    Binary {
        op: &'static str,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Unary {
        op: &'static str,
        e: Box<Expr>,
    },
    Ternary {
        c: Box<Expr>,
        t: Option<Box<Expr>>,
        f: Box<Expr>,
    },
    Call {
        name: Box<Expr>,
        args: Vec<Expr>,
    },
    Index {
        e: Box<Expr>,
        i: Option<Box<Expr>>,
    },
    PreInc(Box<Expr>),
    PreDec(Box<Expr>),
    PostInc(Box<Expr>),
    PostDec(Box<Expr>),
    Isset(Vec<Expr>),
    Empty(Box<Expr>),
    Print(Box<Expr>),
    Exit(Option<Box<Expr>>),
    /// `list($a, $b)` / `[$a, $b]` — only valid as an assignment target.
    List(Vec<Option<Expr>>),
    /// `include/require/eval` — argument is the filename/code expression.
    Include {
        kind: IncludeKind,
        e: Box<Expr>,
    },
    /// `throw $expr` (expression statement since PHP 8).
    Throw(Box<Expr>),
    /// `match (s) { a, b => r, default => r }`
    Match {
        subject: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// `function (params) use ($a, &$b) { body }` / `fn() => expr`.
    Closure(ClosureExpr),
    /// `new ClassName(args)` — optionally with property/method access on result.
    New {
        class: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `$obj->prop` / `$obj->method()` / `?->`.
    Prop {
        obj: Box<Expr>,
        name: PropName,
        nullsafe: bool,
    },
    MethodCall {
        obj: Box<Expr>,
        name: PropName,
        args: Vec<Expr>,
        nullsafe: bool,
    },
    /// `ClassName::CONST` / `::method()` / `::$prop` / `className::class`.
    StaticProp {
        class: Box<Expr>,
        name: PropName,
    },
    StaticCall {
        class: Box<Expr>,
        name: String,
        args: Vec<Expr>,
    },
    ClassConst {
        class: Box<Expr>,
        name: String,
    },
    /// `clone $obj`
    Clone(Box<Expr>),
    /// `(int)`/`(string)`/... cast.
    Cast {
        kind: CastKind,
        e: Box<Expr>,
    },
    /// `$obj instanceof ClassName`
    Instanceof {
        obj: Box<Expr>,
        class: Box<Expr>,
    },
    /// Magic constant resolved at eval time (__LINE__ handled in parser).
    MagicConst(MagicConst),
    /// `$$x` / `${expr}` — variable variable.
    VarVar(Box<Expr>),
    /// Anonymous class declaration (`new class { ... }`).
    AnonClass(Rc<ClassDecl>),
}

#[derive(Debug, Clone)]
pub enum PropName {
    Name(String),
    /// `$obj->{$expr}` / `Class::{$expr}`.
    Expr(Box<Expr>),
    /// `$obj->$var` — name is a variable whose value is the name.
    Var(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeKind {
    Include,
    IncludeOnce,
    Require,
    RequireOnce,
    Eval,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    /// Empty conds = `default` arm.
    pub conds: Vec<Expr>,
    pub result: Expr,
}

#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub decl: FunctionDecl,
    /// `use ($a, &$b)` captures; bool = by-ref.
    pub uses: Vec<(String, bool)>,
    /// Arrow fn `fn(...) => expr`: captures whole parent scope by value.
    pub arrow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagicConst {
    Line,
    File,
    Dir,
    Function,
    Method,
    Class,
    Namespace,
}
