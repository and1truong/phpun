use crate::lexer::StringPart;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Stmt {
    /// Parser-injected line marker — updates Interp::cur_line.
    Line(usize),
    /// Parser-injected compile-time deprecation (e.g. `case e;` —
    /// tests/lang/033). PHP prints these before execution begins.
    /// Compile-time diagnostic drained from `Token::Diag`: printed
    /// before execution, like Zend's compile warnings (octal overflow).
    Diag {
        level: &'static str,
        msg: String,
        line: usize,
    },
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
    /// `goto name;` — jumps to `name:` at function/file statement scope.
    Goto(String),
    /// `name:` — goto target marker; executes as a no-op.
    Label(String),
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
    /// Names listed are only the ones needing the non-compound-name
    /// warning (e.g. `use A;`) — the parser already applied aliases.
    Use(Vec<String>),
    /// Top-level `const NAME = expr, ...;` — declares global constants;
    /// names arrive already namespace-qualified (namespaces/ns_042).
    ConstDecl(Vec<(String, Expr)>),
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    /// "class" | "interface" | "trait" | "enum" | "abstract class" | "final class"
    pub kind: ClassKind,
    pub is_abstract: bool,
    pub is_final: bool,
    /// `readonly class` modifier — forbids hooked props (gh15419).
    pub readonly: bool,
    pub parent: Option<String>,
    pub implements: Vec<String>,
    /// `#[Attr]` groups preceding the declaration (AllowDynamicProperties
    /// detection, ReflectionAttribute::getAttributes).
    pub attrs: Vec<AttrDecl>,
    /// `use`d traits (inside the body).
    pub traits: Vec<String>,
    /// Adaptations inside `use T { ... }` blocks.
    pub adaptations: Vec<TraitAdaptation>,
    pub methods: Vec<Rc<MethodDecl>>,
    /// (name, default value expr, flags)
    pub props: Vec<PropDecl>,
    pub consts: Vec<ConstDecl>,
    /// Declaring file — filled at registration; const-exprs inside
    /// (prop/const defaults) bind __FILE__/__DIR__ to it.
    pub file: String,
}

/// One rule inside a `use T { ... }` trait-use block.
#[derive(Debug, Clone)]
pub enum TraitAdaptation {
    /// `T::m insteadof T2, T3` — T's m wins; the listed traits' m is
    /// suppressed during composition.
    Insteadof {
        trait_name: String,
        method: String,
        excludes: Vec<String>,
    },
    /// `[T::]m as [visibility] [alias]` — clone m under a new name
    /// and/or change its visibility; the original stays.
    Alias {
        trait_name: Option<String>,
        method: String,
        alias: Option<String>,
        vis: Option<Visibility>,
        is_final: bool,
    },
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
pub struct ConstDecl {
    pub name: String,
    pub value: Expr,
    pub visibility: Visibility,
    pub is_final: bool,
    /// Declared type members (`const string C1`, PHP 8.3 typed consts);
    /// None = untyped.
    pub ty: Option<Vec<String>>,
    /// `#[Attr]` groups on the const (ReflectionClassConstant::
    /// getAttributes — constant_020).
    pub attrs: Vec<AttrDecl>,
    /// Trait the const was merged from (`use T`); None = declared here.
    pub decl_in: Option<String>,
    /// `case` member of an enum — materializes a singleton case object.
    pub enum_case: bool,
}

#[derive(Debug, Clone)]
pub struct PropDecl {
    pub name: String,
    pub default: Option<Expr>,
    pub is_static: bool,
    pub visibility: Visibility,
    pub readonly: bool,
    /// Declared type members in source order (`?T`/`T|null` include
    /// "null"); None for an untyped property. Only used for hook
    /// set-parameter compat checks — the runtime doesn't enforce it.
    pub ty: Option<Vec<String>>,
    /// `abstract` modifier (only meaningful with bodiless hooks).
    pub is_abstract: bool,
    /// `final` modifier — the prop (and its hooks) cannot be overridden.
    pub is_final: bool,
    /// Asymmetric write visibility (`public private(set) $p`); None
    /// means writes use `visibility`.
    pub set_vis: Option<Visibility>,
    /// Trait the prop was merged from (`use T`), for `__METHOD__`
    /// inside hooks (`T::$prop::get`).
    pub decl_in: Option<String>,
    /// PHP 8.4 property hooks (`public $p { get => ..; set => .. }`);
    /// None for a plain property.
    pub hooks: Option<Vec<PropHook>>,
    /// `#[Attr]` groups preceding the declaration (compile-checked
    /// builtins like ReturnTypeWillChange).
    pub attrs: Vec<AttrDecl>,
    /// Source line of the declaration (Zend reports hook/prop
    /// incompatibilities on the prop's own line).
    pub line: usize,
}

/// One `get`/`set` hook on a hooked property (Zend/tests/property_hooks).
#[derive(Debug, Clone)]
pub struct PropHook {
    /// Raw hook identifier (`get`/`set`; anything else is a decl error).
    pub name: String,
    /// `get` vs `set`.
    pub is_get: bool,
    /// `set`'s declared params (`set(T $v)`); empty = implicit `$value`.
    pub params: Vec<Param>,
    /// A `(...)` parameter list was written at all (`get()` is a decl
    /// error even when empty).
    pub has_plist: bool,
    /// `None` = abstract declaration (interfaces/abstract classes only).
    pub body: Option<Vec<Stmt>>,
    /// `&get` returns by reference.
    pub by_ref: bool,
    /// `final` hook.
    pub is_final: bool,
    /// Hook's own visibility when written explicitly (`private get`).
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone)]
pub struct MethodDecl {
    pub decl: FunctionDecl,
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub visibility: Visibility,
    /// Created by a `T::m as vis alias` trait adaptation — holds the
    /// ORIGINAL method name (ReflectionClass::getTraitAliases reports
    /// `alias => "T::orig"`). The private-final warning skips alias
    /// copies — gh17214 vs gh12854.
    pub trait_alias_of: Option<String>,
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

/// A parsed `#[Name(args)]` attribute group entry — args stay as Exprs
/// and are evaluated lazily by ReflectionAttribute::getArguments() and
/// ::newInstance().
#[derive(Debug, Clone)]
pub struct AttrDecl {
    pub name: String,
    pub args: Vec<Expr>,
    /// Line of the `#[` token (compile-fatals attribute to the
    /// attributed declaration, which Zend reports a line later).
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub name: String,
    pub params: Vec<Param>,
    /// Return type members (source order); None = no declaration.
    pub ret: Option<Vec<String>>,
    pub body: Vec<Stmt>,
    /// `#[Attr]` groups preceding the declaration.
    pub attrs: Vec<AttrDecl>,
    /// `&name(` — returns by reference.
    pub by_ref: bool,
    /// Source line of the `function` keyword (for TypeError "defined in"
    /// and compile-time deprecation diagnostics).
    pub line: usize,
    /// Line of the closing `}` — Zend attributes "none returned"
    /// TypeErrors to the function's last line.
    pub end_line: usize,
    /// File the decl was registered from — PHP resolves includes relative
    /// to the file containing the call site (include_variation2).
    pub file: String,
    /// Declaring namespace (`test\ns1` or "" for global) — unqualified
    /// calls/consts inside this function try the namespaced name first,
    /// then fall back to global (Zend/tests/namespaces).
    pub ns: String,
    /// Trait this method was merged from (`use T`) — `__METHOD__` and
    /// `__TRAIT__` name the trait, `__CLASS__`/`self`/`static` the
    /// consuming class.
    pub decl_in: Option<String>,
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
    /// Promotion modifiers retained for synthesized props.
    pub vis: Option<Visibility>,
    pub readonly: bool,
    pub is_final: bool,
    /// Asymmetric write visibility (`private(set)`).
    pub set_vis: Option<Visibility>,
    /// Hooks on a promoted property (`public $p { get {} }`).
    pub hooks: Option<Vec<PropHook>>,
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
    /// `yield [k =>] v` — inside a function the call becomes a lazy
    /// Generator; outside one it's a fatal at eval.
    Yield {
        key: Option<Box<Expr>>,
        val: Option<Box<Expr>>,
    },
    /// `yield from iterable` — splices another iterable's items.
    YieldFrom(Box<Expr>),
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
    /// `(expr)` — keeps `(parent::$p)::get()` distinct from the
    /// property-hook call syntax `parent::$p::get()`.
    Paren(Box<Expr>),
    StaticCall {
        class: Box<Expr>,
        name: String,
        args: Vec<Expr>,
    },
    /// `C::$var(...)` — static call whose method name is an expression.
    StaticCallDyn {
        class: Box<Expr>,
        name: Box<Expr>,
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
    /// `expr(...)` — first-class callable syntax (PHP 8.1): wraps the
    /// call node whose arg list was the bare `...` (Call/MethodCall/
    /// StaticCall/StaticCallDyn, args emptied at parse time).
    Fcc(Box<Expr>),
    /// `...$expr` inside a call's argument list — argument unpacking.
    Unpack(Box<Expr>),
    /// Internal parse marker: a call arg list that was exactly `...`
    /// (rewritten to `Fcc` at the call-construction sites).
    FccMark,
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
    /// `static function` — never binds $this.
    pub is_static: bool,
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
    /// `__PROPERTY__` — the hooked prop's name inside a hook, "" outside.
    Property,
    /// `__TRAIT__` — the trait a method was merged from, "" outside.
    Trait,
}
