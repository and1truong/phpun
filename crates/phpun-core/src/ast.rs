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
    /// `static` keyword line; each var carries its own declarator line
    /// (redeclaration diagnostics name the redeclared var's line).
    Static {
        vars: Vec<(String, Option<Expr>, usize)>,
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
    /// Declaration line — feeds 'Cannot redeclare' diagnostics.
    pub line: usize,
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
    /// Declaration line — lazy const-init errors attribute to the
    /// declaring file at this line (zend reports the const's own line,
    /// not the resolution site).
    pub line: usize,
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
    /// Source line of the declaration.
    pub line: usize,
    /// Line of the default-value expr's first token (lazy-init Errors
    /// attribute there); 0 = same as `line`.
    pub dline: usize,
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
    /// Source line of the `get`/`set` keyword (hook-signature
    /// incompatibilities report on it, not the prop's line).
    pub line: usize,
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
    /// `&$v` / `&$o->p` / `&$a[i]` — a `new_variable` chain bound by ref.
    ByRef(Box<Expr>),
    /// Any other assignable lvalue (`$b[0]`, `$o->p`, ...).
    Lvalue(Box<Expr>),
    /// `as [$a, 'k' => $b]` destructuring — `(key expr, target)` per
    /// element; zend forbids mixing keyed and unkeyed entries.
    List(Vec<Option<(Option<Expr>, ForeachTarget)>>),
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
    pub name: Rc<str>,
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
    pub file: Rc<str>,
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
        /// The assignment node's own line — the target's first-token
        /// line, where zend emits the ASSIGN op (post-eval lineno for
        /// `${expr}`/`->{expr}` name diagnostics).
        line: usize,
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
        /// Line the call's pushed frames are sited at: the name token's
        /// line for `name(...)`, the `(` line for `callable_expr(...)`
        /// (zend_compile_call_common's `lineno`).
        site: usize,
        /// The callee expression's first-token line — zend's
        /// INIT_DYNAMIC_CALL lineno, where resolution errors
        /// (undefined function, not-callable) site before args run.
        callee: usize,
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
    /// Elements are `(key, target)`; `key` is the evaluated key expr of
    /// a keyed element (`'k' => $v`) — zend forbids mixing keyed and
    /// unkeyed entries in one list.
    List(Vec<Option<(Option<Expr>, Expr)>>),
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
        /// Line of the last arm-result's final token — zend stamps an
        /// empty `[]` result's lone INIT_ARRAY at its `]`.
        end: usize,
    },
    /// `function (params) use ($a, &$b) { body }` / `fn() => expr`.
    Closure(ClosureExpr),
    /// `new ClassName(args)` — optionally with property/method access on result.
    New {
        class: Box<Expr>,
        args: Vec<Expr>,
        /// Trace site: the class expression's first-token line
        /// (ZEND_AST_NEW inherits child0's lineno).
        site: usize,
    },
    /// `$obj->prop` / `$obj->method()` / `?->`.
    Prop {
        obj: Box<Expr>,
        name: PropName,
        nullsafe: bool,
        /// Trace site: the member-name token's line
        /// (zend_ast_get_lineno(prop_ast)).
        site: usize,
    },
    MethodCall {
        obj: Box<Expr>,
        name: PropName,
        args: Vec<Expr>,
        nullsafe: bool,
        /// Trace site: the member-name token's line
        /// (zend_ast_get_lineno(method_ast)).
        site: usize,
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
        /// Trace site: the member-name token's line.
        site: usize,
    },
    /// `C::$var(...)` — static call whose method name is an expression.
    StaticCallDyn {
        class: Box<Expr>,
        name: Box<Expr>,
        args: Vec<Expr>,
        /// Trace site: the member-name expression's first-token line.
        site: usize,
    },
    ClassConst {
        class: Box<Expr>,
        name: String,
    },
    /// `Cls::{expr}` — class-constant fetch with a dynamic name
    /// (FETCH_CLASS_CONSTANT, not a static prop).
    ClassConstDyn {
        class: Box<Expr>,
        name: Box<Expr>,
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
    /// `$$x` / `${expr}` — variable variable. Second field is the
    /// construct's own end line (the `}`'s line for `${expr}`; the
    /// last token's line for `$$x`): a folded (`${expr}` with a
    /// compile-const inner) read sites there, or at the enclosing
    /// `=`'s own line when it is the assign's direct value.
    VarVar(Box<Expr>, usize),
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
    /// Arrow fn `fn(...) => expr`: `uses` contains lexical imports by value.
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

/// The line an expression's evaluation ends at — zend's post-eval
/// (and post-compile) lineno. A call leaves it at the deepest
/// last-arg marker (dispatch re-sites at the call's own site), a prop
/// read at the member name's line, a ternary/binary at the last
/// source operand's end. `None` when `e` has no recorded end.
pub fn end_line(e: &Expr) -> Option<usize> {
    match e {
        // zend's post-eval lineno for a call is its last arg's
        // line (the DO_FCALL lineno override only touches the op,
        // not CG) — a zero-arg call leaves it at the call site.
        Expr::Call { args, site, .. }
        | Expr::MethodCall { args, site, .. }
        | Expr::StaticCall { args, site, .. }
        | Expr::StaticCallDyn { args, site, .. }
        | Expr::New { args, site, .. } => args.last().and_then(end_line).or(Some(*site)),
        Expr::ArrayLit(items) => items.iter().rev().find_map(|(_, v)| end_line(v)),
        Expr::Binary {
            op: "argline",
            l,
            r,
        } => match r.as_ref() {
            // A parse-time-folded concat's zval stamps at the reduce
            // lookahead — the mark records that token's line.
            Expr::Binary {
                op: ".",
                l: cl,
                r: cr,
            } if zval_lit(cl) && zval_lit(cr) => match l.as_ref() {
                Expr::Int(n) => Some(*n as usize),
                _ => end_line(r),
            },
            _ => end_line(r).or_else(|| match l.as_ref() {
                Expr::Int(n) => Some(*n as usize),
                _ => None,
            }),
        },
        Expr::Binary { r, .. } => end_line(r),
        Expr::Ternary { f, .. } => end_line(f),
        Expr::Prop { name, site, .. } => match name {
            PropName::Expr(inner) => end_line(inner).or(Some(*site)),
            _ => Some(*site),
        },
        Expr::Index { e, i } => i.as_deref().and_then(end_line).or_else(|| end_line(e)),
        Expr::Paren(e)
        | Expr::PreInc(e)
        | Expr::PreDec(e)
        | Expr::PostInc(e)
        | Expr::PostDec(e)
        | Expr::Print(e)
        | Expr::Clone(e)
        | Expr::Unpack(e)
        | Expr::Fcc(e)
        | Expr::Throw(e)
        | Expr::YieldFrom(e)
        | Expr::Empty(e) => end_line(e),
        Expr::Unary { e, .. } | Expr::Cast { e, .. } => end_line(e),
        // A varvar's effective position is its inner's — for a
        // folded varvar that's the inner's first-token line (CV
        // semantics), for a dynamic one the inner's last
        // evaluated line.
        Expr::VarVar(inner, _) => end_line(inner),
        // Zend emits the ASSIGN op at the assignment node's own
        // line (the target's first token) — not the value's end.
        // A `list()`/`[]` destructure ends at its last element's
        // own store line instead (zend's post-eval lineno).
        Expr::Assign { target, line, .. } => match target.as_ref() {
            Expr::List(_) => end_line(target).or(Some(*line)),
            _ => Some(*line),
        },
        Expr::List(items) => items
            .iter()
            .rev()
            .find_map(|i| i.as_ref())
            .and_then(|(_, e)| end_line(e)),
        // A match expr's compiled end is its last arm's result. A
        // parse-time-folded concat already carries its following-token
        // stamp on its argline mark; `[]`'s lone INIT_ARRAY stamps at
        // the `]` token instead of an element line.
        Expr::Match { arms, end, .. } => match arms.last() {
            Some(arm) if matches!(unmarked(&arm.result), Expr::ArrayLit(items) if items.is_empty()) => {
                Some(*end)
            }
            _ => arms.last().and_then(|a| end_line(&a.result)),
        },
        _ => None,
    }
}

/// Marks transparent to a node's shape — `argline`, parens and by-ref
/// wrappers — so a folded literal under them is still seen.
fn unmarked(mut e: &Expr) -> &Expr {
    loop {
        e = match e {
            Expr::Paren(inner) | Expr::ByRef(inner) => inner,
            Expr::Binary {
                op: "argline", r, ..
            } => r,
            _ => return e,
        };
    }
}

/// A literal-string-shaped concat operand — zend's parse-time
/// `zend_ast_create_concat_op` folds these into one zval.
pub(crate) fn zval_lit(e: &Expr) -> bool {
    match unmarked(e) {
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => true,
        Expr::Interp(parts) => parts.iter().all(|p| matches!(p, StringPart::Lit(_))),
        Expr::Binary { op: ".", l, r } => zval_lit(l) && zval_lit(r),
        _ => false,
    }
}

/// The expression's first-token line — zend_ast_get_lineno. An
/// `argline` mark records exactly that for the expression it wraps;
/// every other node resolves to its first child's first token
/// (create_N takes child1's lineno). `None` for bare leaves, which
/// carry no line (their `argline` wrapper does).
pub fn start_line(e: &Expr) -> Option<usize> {
    match e {
        Expr::Binary {
            op: "argline",
            l,
            r,
        } => match l.as_ref() {
            Expr::Int(n) => Some(*n as usize),
            _ => start_line(r),
        },
        // `k => v` in a destructure list: zend's ARRAY_ELEM lineno is
        // the VALUE's first token, not the key's.
        Expr::Binary {
            op: "listkey", r, ..
        } => start_line(r),
        Expr::Binary { l, .. } => start_line(l),
        Expr::Paren(e)
        | Expr::ByRef(e)
        | Expr::PreInc(e)
        | Expr::PreDec(e)
        | Expr::PostInc(e)
        | Expr::PostDec(e)
        | Expr::Print(e)
        | Expr::Clone(e)
        | Expr::Unpack(e)
        | Expr::Fcc(e)
        | Expr::Throw(e)
        | Expr::YieldFrom(e)
        | Expr::Empty(e) => start_line(e),
        Expr::Unary { e, .. } | Expr::Cast { e, .. } => start_line(e),
        Expr::Ternary { c, .. } => start_line(c),
        Expr::Index { e, .. } => start_line(e),
        Expr::Prop { obj, .. } | Expr::MethodCall { obj, .. } => start_line(obj),
        Expr::Call { callee, .. } => Some(*callee),
        Expr::StaticCall { class, .. }
        | Expr::StaticCallDyn { class, .. }
        | Expr::StaticProp { class, .. }
        | Expr::ClassConst { class, .. } => start_line(class),
        Expr::Instanceof { obj, .. } => start_line(obj),
        Expr::New { site, .. } => Some(*site),
        Expr::Assign { line, .. } => Some(*line),
        // zend's ARRAY list-node lineno is the first element's —
        // an ARRAY_ELEM's lineno is its VALUE's first token.
        Expr::ArrayLit(items) => items.first().and_then(|(_, v)| start_line(v)),
        Expr::List(items) => items
            .iter()
            .flatten()
            .next()
            .and_then(|(_, e)| start_line(e)),
        Expr::Isset(args) => args.first().and_then(start_line),
        Expr::VarVar(inner, _) => start_line(inner),
        _ => None,
    }
}
