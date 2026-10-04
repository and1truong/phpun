//! Free helpers shared by the `interp` submodules: cell/key
//! plumbing, numeric coercions, builtin signature tables, type-name
//! rendering and the SPL iterator prelude evaluated by `Interp::new`.

use super::*;

/// `__METHOD__` scope name for a hook on `dcls`: a trait-origin hook
/// keeps its trait name via `decl_in`, else the declaring class.
pub(in crate::interp) fn decl_owner(dcls: &Rc<PhpClass>, pname: &str) -> String {
    dcls.decl
        .props
        .iter()
        .find(|p| p.name == pname)
        .and_then(|p| p.decl_in.clone())
        .unwrap_or_else(|| dcls.name().to_string())
}

/// Hooks merged along a chain (hook, declaring class), nearest first.
pub type MergedHooks = Vec<(PropHook, Rc<PhpClass>)>;
/// `(emitted key, slot key, decl+decl class)` — `None` decl means a
/// dynamic property (property-hooks serialization views).
pub type SerialEntry = (String, String, Option<(PropDecl, Rc<PhpClass>)>);
/// `(name, cell)` entries a `...$v` unpack yields — `None` name is
/// positional.
pub type SpreadItems = Vec<(Option<Rc<str>>, Cell)>;

/// Zend-style render for the `assert(<args>)` AssertionError message.
pub(in crate::interp) fn assert_arg_repr(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => crate::value::trace_arg(&Value::Float(*f)),
        Value::Str(s) => format!("'{}'", crate::value::lossy(&s)),
        Value::Array(_) => "Array".into(),
        Value::Object(o) => format!("Object({})", o.borrow().class.name()),
        Value::Callable(_) => "Object(Closure)".into(),
        Value::Resource(_) => "Resource id #1".into(),
    }
}

pub(in crate::interp) fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

pub(in crate::interp) fn key_value(k: &ArrKey) -> Value {
    match k {
        ArrKey::Int(i) => Value::Int(*i),
        ArrKey::Str(s) => Value::str(s.to_string()),
        ArrKey::Tomb => Value::Null,
    }
}

pub(in crate::interp) enum Num {
    I(i64),
    F(f64),
}

impl Num {
    pub(in crate::interp) fn to_float(&self) -> f64 {
        match self {
            Num::I(i) => *i as f64,
            Num::F(f) => *f,
        }
    }
}

pub(in crate::interp) fn num_bin(
    a: Num,
    b: Num,
    fi: fn(i64, i64) -> Option<i64>,
    ff: fn(f64, f64) -> f64,
) -> Value {
    match (a, b) {
        (Num::I(x), Num::I(y)) => match fi(x, y) {
            // Integer overflow promotes to float (multiply_basiclong_64bit.phpt).
            Some(r) => Value::Int(r),
            None => Value::Float(ff(x as f64, y as f64)),
        },
        (x, y) => Value::Float(ff(x.to_float(), y.to_float())),
    }
}

/// Perl-style string increment ("a"→"b", "z"→"aa", "A9"→"B0").
pub(in crate::interp) fn perl_inc(s: &[u8]) -> Vec<u8> {
    let mut bytes = s.to_vec();
    let mut i = bytes.len();
    let mut carry = true;
    while carry && i > 0 {
        i -= 1;
        let c = bytes[i];
        let next = match c {
            b'a'..=b'y' | b'A'..=b'Y' => c + 1,
            b'z' => {
                bytes[i] = b'a';
                continue;
            }
            b'Z' => {
                bytes[i] = b'A';
                continue;
            }
            b'0'..=b'8' => c + 1,
            b'9' => {
                bytes[i] = b'0';
                continue;
            }
            _ => {
                carry = false;
                continue;
            }
        };
        bytes[i] = next;
        carry = false;
    }
    if carry {
        // Determine the carried character class from the first char.
        let first = bytes.first().copied().unwrap_or(b'a');
        let c = if first.is_ascii_uppercase() {
            b'A'
        } else if first.is_ascii_lowercase() {
            b'a'
        } else {
            b'1'
        };
        bytes.insert(0, c);
    }
    bytes
}

/// PHP float→int conversion (zend_dtoi64): warns on out-of-range,
/// wraps modulo 2^64; NaN/INF → 0.
pub(in crate::interp) fn coerce_float(f: f64, mut warn: impl FnMut(&str)) -> i64 {
    const MOD: f64 = 18446744073709551616.0; // 2^64
    if !f.is_finite() {
        warn(&format!(
            "The float {} is not representable as an int, cast occurred",
            format_float_repr(f)
        ));
        return 0;
    }
    if f >= i64::MAX as f64 || f < i64::MIN as f64 {
        warn(&format!(
            "The float {} is not representable as an int, cast occurred",
            format_float_repr(f)
        ));
        let m = f % MOD;
        let u = if m < 0.0 { m + MOD } else { m };
        return u as u64 as i64;
    }
    f as i64
}

pub(in crate::interp) fn bitwise_str(op: &str, a: &[u8], b: &[u8]) -> Vec<u8> {
    // `|` pads the shorter operand with NUL; `&`/`^` truncate to min length.
    let (x, y) = (a, b);
    let n = if op == "|" {
        x.len().max(y.len())
    } else {
        x.len().min(y.len())
    };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let xi = x.get(i).copied().unwrap_or(0);
        let yi = y.get(i).copied().unwrap_or(0);
        out.push(match op {
            "&" => xi & yi,
            "|" => xi | yi,
            _ => xi ^ yi,
        });
    }
    out
}

/// By-ref flags for builtin parameters (only slots that accept references are
/// `true`). Used to warn on non-variable args in by-ref positions and to alias
/// real cells for mutating builtins like array_pop/sort/preg_match.
pub(in crate::interp) fn builtin_byref(name: &str) -> Option<&'static [bool]> {
    Some(match name {
        "array_pop" | "array_shift" | "array_walk" | "sort" | "rsort" | "asort" | "arsort"
        | "ksort" | "krsort" | "usort" | "uasort" | "uksort" | "natsort" | "natcasesort"
        | "shuffle" | "reset" | "end" | "next" | "prev" | "current" | "pos" | "each"
        | "array_push" | "array_unshift" | "array_splice" | "array_multisort" => &[true],
        "preg_match" | "preg_match_all" => &[false, false, true],
        "preg_replace"
        | "preg_replace_callback"
        | "preg_filter"
        | "str_replace"
        | "str_ireplace" => &[false, false, false, false, true],
        "preg_replace_callback_array" => &[false, false, false, true],
        "parse_str" => &[false, true],
        "is_callable" => &[false, false, true],
        "sscanf" | "fscanf" => &[false, false],
        "exec" => &[false, true, true],
        "passthru" | "system" => &[false, true],
        "preg_grep" => &[false],
        _ => return None,
    })
}

/// Weak-mode scalar coercion used by typed-property writes and hook
/// type checks ("C::$p: Return value must be of type int" family).
pub(in crate::interp) fn weak_ty_coerce(tys: &[String], v: &Value) -> Option<Value> {
    for t in tys {
        let coerced = match (t.as_str(), v) {
            ("int", Value::Str(s)) => {
                let tr = crate::value::lossy(s);
                let tr = tr.trim();
                let base = if let Some(h) = tr.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).ok()
                } else if let Some(o) = tr.strip_prefix("0o") {
                    i64::from_str_radix(o, 8).ok()
                } else if let Some(b) = tr.strip_prefix("0b") {
                    i64::from_str_radix(b, 2).ok()
                } else {
                    tr.parse::<i64>().ok()
                };
                base.map(Value::Int)
            }
            ("int", Value::Float(f)) => {
                // Zend refuses out-of-range float->int coercions
                // (NaN/Inf/|f| >= 2^63 → TypeError, no saturation).
                if f.is_finite() && *f < 9.223372036854776e18 && *f >= -9.223372036854776e18 {
                    Some(Value::Int(*f as i64))
                } else {
                    None
                }
            }
            ("int", Value::Bool(b)) => Some(Value::Int(*b as i64)),
            ("string", Value::Int(i)) => Some(Value::str(i.to_string())),
            ("string", Value::Float(f)) => Some(Value::str(format_float_repr(*f))),
            ("string", Value::Bool(b)) => Some(Value::str(if *b { "1" } else { "" })),
            ("float", Value::Int(i)) => Some(Value::Float(*i as f64)),
            ("float", Value::Str(s)) => crate::value::lossy(s)
                .trim()
                .parse::<f64>()
                .ok()
                .map(Value::Float),
            ("float", Value::Bool(b)) => Some(Value::Float(if *b { 1.0 } else { 0.0 })),
            ("bool", _) => Some(Value::Bool(v.is_truthy())),
            _ => None,
        };
        if let Some(c) = coerced {
            return Some(c);
        }
    }
    None
}

/// Render a parsed type member list the way Zend prints it — a union
/// containing `null` displays as `?T`.
pub(in crate::interp) fn ty_disp(ty: &[String]) -> String {
    let mut nullable = false;
    let mut rest: Vec<String> = Vec::new();
    for m in ty {
        if m.eq_ignore_ascii_case("null") {
            nullable = true;
        } else {
            rest.push(
                m.split('&')
                    .map(|p| {
                        let p = p.trim_start_matches('\\');
                        if let Some(pos) = p.find("@anonymous$") {
                            format!("{}@anonymous", &p[..pos])
                        } else {
                            p.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
    }
    let joined = if rest.len() > 1 || nullable {
        rest.iter()
            .map(|m| {
                if m.contains('&') {
                    format!("({m})")
                } else {
                    m.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    } else {
        rest.join("|")
    };
    if nullable && rest.is_empty() {
        "null".to_string()
    } else if nullable && rest.len() == 1 && !rest[0].contains('&') {
        format!("?{}", joined)
    } else if nullable {
        // `(X&Y)|null` — intersections can't take the ? shortcut.
        format!("{}|null", joined)
    } else {
        joined
    }
}

/// Typed-const compat in trait composition: same member list
/// (case-insensitive; both `None` is compatible).
pub(in crate::interp) fn ty_list_eq(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(m, n)| m.eq_ignore_ascii_case(n))
        }
        _ => false,
    }
}

/// Whether a const initializer is a pure compile-time expression
/// (literals and operators over them — no fetches, calls, `new`).
/// Only these get Zend's eager "Cannot use ... as value" fatal at
/// class registration; everything else type-checks lazily at access.
pub(in crate::interp) fn is_compile_const(e: &Expr) -> bool {
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => true,
        Expr::Interp(parts) => parts
            .iter()
            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))),
        Expr::ArrayLit(items) => items.iter().all(|(_, v)| is_compile_const(v)),
        Expr::Unary { e, .. } => is_compile_const(e),
        Expr::Binary { l, r, .. } => is_compile_const(l) && is_compile_const(r),
        Expr::Cast { e, .. } => is_compile_const(e),
        Expr::Ternary { c, t, f } => {
            is_compile_const(c)
                && t.as_ref().map(|x| is_compile_const(x)).unwrap_or(true)
                && is_compile_const(f)
        }
        _ => false,
    }
}

/// Zend's normalized union display for redundancy errors: iterable
/// expands to its members, class names first (written order), then
/// `object`, then `array`, then remaining builtins, `null` last.
pub(in crate::interp) fn ty_norm_disp(ty: &[String]) -> String {
    let mut classes: Vec<String> = Vec::new();
    let mut scalars: Vec<String> = Vec::new();
    let mut obj = false;
    let mut arr = false;
    let mut nul = false;
    for m in ty {
        let mut members: Vec<String> = if m.eq_ignore_ascii_case("iterable") {
            vec!["Traversable".into(), "array".into()]
        } else {
            vec![m.clone()]
        };
        for e in members.drain(..) {
            let el = e.to_lowercase();
            match el.as_str() {
                "null" => nul = true,
                "object" => obj = true,
                "array" => arr = true,
                "self" | "static" | "parent" => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        classes.push(e);
                    }
                }
                "int" | "float" | "string" | "bool" | "callable" | "iterable" | "mixed"
                | "void" | "never" | "false" | "true" => {
                    if !scalars.iter().any(|c| c == &el) {
                        scalars.push(el);
                    }
                }
                _ => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        if e.contains('&') {
                            classes.push(format!("({})", e));
                        } else {
                            classes.push(e);
                        }
                    }
                }
            }
        }
    }
    let mut out = classes;
    if obj {
        out.push("object".into());
    }
    if arr {
        out.push("array".into());
    }
    out.extend(scalars);
    if nul {
        out.push("null".into());
    }
    out.join("|")
}

/// SPL iterator-wrapper classes expressed in plain PHP and eval'd once
/// per Interp (Interp::new). Written in PHP because they are pure
/// delegation over Iterator methods; the engine supplies the leaves
/// (DirectoryIterator/FilesystemIterator/RecursiveDirectoryIterator).
pub(in crate::interp) const SPL_ITERATOR_PRELUDE: &str = r#"
interface OuterIterator extends Iterator {
    public function getInnerIterator();
}
interface RecursiveIterator extends Iterator {
    public function hasChildren();
    public function getChildren();
}
class IteratorIterator implements OuterIterator {
    protected $inner;
    public function __construct($iterator) {
        $it = $iterator;
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->inner = $it;
    }
    public function getInnerIterator() { return $this->inner; }
    public function __call($func, $params) { return $this->inner->$func(...$params); }
    public function rewind() { $this->inner->rewind(); }
    public function valid() { return $this->inner->valid(); }
    public function current() { return $this->inner->current(); }
    public function key() { return $this->inner->key(); }
    public function next() { $this->inner->next(); }
}
abstract class FilterIterator extends IteratorIterator {
    abstract public function accept();
    public function rewind() { $this->inner->rewind(); $this->fetch(); }
    public function next() { $this->inner->next(); $this->fetch(); }
    private function fetch() {
        while ($this->inner->valid() && !$this->accept()) {
            $this->inner->next();
        }
    }
}
abstract class RecursiveFilterIterator extends FilterIterator implements RecursiveIterator {
    public function hasChildren() { return $this->inner->hasChildren(); }
    // SPL: children come back wrapped in the same filter class.
    public function getChildren() {
        $cls = static::class;
        return new $cls($this->inner->getChildren());
    }
}
class CallbackFilterIterator extends FilterIterator {
    private $callback;
    public function __construct($iterator, $callback) {
        parent::__construct($iterator);
        $this->callback = $callback;
    }
    public function accept() {
        return ($this->callback)($this->current(), $this->key(), $this->inner);
    }
}
class RecursiveIteratorIterator implements OuterIterator {
    const LEAVES_ONLY = 0;
    const SELF_FIRST = 1;
    const CHILD_FIRST = 2;
    const CALL_TOSTRING = 4;
    const CATCH_GET_CHILD = 8;
    private $stack = [];
    private $emitted = [];
    private $mode;
    private $flags;
    private $yieldParent = false;
    public function __construct($iterator, $mode = 0, $flags = 0) {
        $it = $iterator;
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->mode = $mode;
        $this->flags = $flags;
        $this->stack = [$it];
        $this->emitted = [false];
        $it->rewind();
        $this->descend();
    }
    private function top() { return $this->stack[count($this->stack) - 1]; }
    public function getDepth() { return count($this->stack) - 1; }
    public function getSubIterator($level = null) {
        $i = $level === null ? count($this->stack) - 1 : $level;
        return $this->stack[$i] ?? null;
    }
    public function getInnerIterator() { return $this->top(); }
    private function descend() {
        while (count($this->stack) > 0) {
            $top = $this->top();
            if (!$top->valid()) {
                array_pop($this->stack);
                array_pop($this->emitted);
                $this->yieldParent = false;
                if (count($this->stack) === 0) {
                    return;
                }
                $i = count($this->stack) - 1;
                if ($this->mode === self::CHILD_FIRST && !$this->emitted[$i]) {
                    $this->emitted[$i] = true;
                    $this->yieldParent = true;
                    return;
                }
                $this->top()->next();
                continue;
            }
            if ($top instanceof RecursiveIterator && $top->hasChildren()) {
                $i = count($this->stack) - 1;
                if ($this->mode === self::SELF_FIRST && !$this->emitted[$i]) {
                    $this->emitted[$i] = true;
                    $this->yieldParent = true;
                    return;
                }
                try {
                    $child = $top->getChildren();
                } catch (Throwable $e) {
                    if (!($this->flags & self::CATCH_GET_CHILD)) {
                        throw $e;
                    }
                    $top->next();
                    continue;
                }
                $child->rewind();
                $this->stack[] = $child;
                $this->emitted[] = false;
                continue;
            }
            $this->yieldParent = false;
            return;
        }
        $this->yieldParent = false;
    }
    public function valid() {
        return count($this->stack) > 0 && $this->top()->valid();
    }
    public function current() {
        return count($this->stack) > 0 ? $this->top()->current() : null;
    }
    public function key() {
        return count($this->stack) > 0 ? $this->top()->key() : null;
    }
    public function next() {
        if (count($this->stack) === 0) {
            return;
        }
        if ($this->yieldParent && $this->mode === self::SELF_FIRST) {
            $top = $this->top();
            try {
                $child = $top->getChildren();
            } catch (Throwable $e) {
                if (!($this->flags & self::CATCH_GET_CHILD)) {
                    throw $e;
                }
                $top->next();
                $this->yieldParent = false;
                $this->descend();
                return;
            }
            $child->rewind();
            $i = count($this->stack) - 1;
            $this->emitted[$i] = false;
            $this->stack[] = $child;
            $this->emitted[] = false;
            $this->yieldParent = false;
            $this->descend();
            return;
        }
        if ($this->yieldParent) {
            $i = count($this->stack) - 1;
            $this->emitted[$i] = false;
            $this->yieldParent = false;
            $this->top()->next();
            $this->descend();
            return;
        }
        $this->top()->next();
        $this->descend();
    }
    public function rewind() {
        $this->stack = [$this->stack[0]];
        $this->emitted = [false];
        $this->stack[0]->rewind();
        $this->yieldParent = false;
        $this->descend();
    }
}
class AppendIterator extends IteratorIterator {
    private $its = [];
    private $idx = 0;
    public function __construct() {}
    public function append($it) {
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->its[] = $it;
        if ($this->idx === 0 && count($this->its) === 1) {
            $this->inner = $it;
        }
    }
    private function sync() {
        while ($this->idx < count($this->its) && !$this->its[$this->idx]->valid()) {
            $this->idx++;
        }
        $this->inner = $this->idx < count($this->its) ? $this->its[$this->idx] : null;
    }
    public function rewind() {
        foreach ($this->its as $it) {
            $it->rewind();
        }
        $this->idx = 0;
        $this->sync();
    }
    public function valid() {
        return $this->idx < count($this->its) && $this->its[$this->idx]->valid();
    }
    public function next() {
        if ($this->idx < count($this->its)) {
            $this->its[$this->idx]->next();
        }
        $this->sync();
    }
    public function getInnerIterator() {
        return $this->idx < count($this->its) ? $this->its[$this->idx] : null;
    }
}
class EmptyIterator implements Iterator {
    public function current() { return null; }
    public function key() { return null; }
    public function next() {}
    public function rewind() {}
    public function valid() { return false; }
}
class SplObjectStorage implements Countable, Iterator, ArrayAccess {
    private array $objs = [];
    private array $data = [];
    private int $pos = 0;
    private int $idx = 0;
    private $info;
    private function hashOf($obj) {
        if (!is_object($obj)) {
            throw new TypeError('SplObjectStorage::offsetSet(): Argument #1 ($object) must be of type object');
        }
        return spl_object_id($obj);
    }
    public function attach($object, $data = null) { $this->offsetSet($object, $data); }
    public function detach($object) { $this->offsetUnset($object); }
    public function contains($object) { return $this->offsetExists($object); }
    public function offsetExists($obj): bool { return isset($this->objs[$this->hashOf($obj)]); }
    public function offsetSet($obj, $data = null): void {
        $h = $this->hashOf($obj);
        if (!isset($this->objs[$h])) {
            $this->objs[$h] = $obj;
        }
        $this->data[$h] = $data;
    }
    public function offsetGet($obj) {
        $h = $this->hashOf($obj);
        if (!isset($this->objs[$h])) {
            throw new UnexpectedValueException('Object not found');
        }
        return $this->data[$h];
    }
    public function offsetUnset($obj): void {
        $h = $this->hashOf($obj);
        unset($this->objs[$h], $this->data[$h]);
    }
    public function getHash($obj) { return (string) $this->hashOf($obj); }
    public function count(): int { return count($this->objs); }
    public function setInfo($data) { $this->info = $data; }
    public function getInfo() { return $this->info; }
    // Iteration: key() is a 0-based index, current() the stored object.
    public function rewind(): void { $this->pos = 0; $this->idx = 0; }
    public function valid(): bool { return $this->idx < count($this->objs); }
    public function current() { return array_values($this->objs)[$this->idx]; }
    public function key(): int { return $this->idx; }
    public function next(): void { $this->idx++; }
    public function addAll($storage) {
        foreach ($storage as $obj) { $this->attach($obj, $storage->getInfo()); }
    }
    public function removeAll($storage) {
        foreach ($storage as $obj) { $this->detach($obj); }
    }
    public function removeAllExcept($storage) {
        foreach ($this->objs as $h => $obj) {
            if (!$storage->contains($obj)) { unset($this->objs[$h], $this->data[$h]); }
        }
    }
}
class SplFixedArray implements ArrayAccess, Iterator, Countable {
    private array $data;
    private int $pos = 0;
    public function __construct(int $size = 0) {
        $this->data = array_fill(0, max(0, $size), null);
    }
    public static function fromArray(array $array, bool $preserveKeys = true) {
        $a = new self($preserveKeys ? count($array) : 0);
        if ($preserveKeys) {
            $max = 0;
            foreach ($array as $k => $v) {
                if (!is_int($k) || $k < 0) {
                    throw new InvalidArgumentException('array must contain only positive integer keys');
                }
                $max = max($max, $k + 1);
            }
            $a = new self($max);
            foreach ($array as $k => $v) { $a->data[$k] = $v; }
        } else {
            $a = new self(count($array));
            $i = 0;
            foreach ($array as $v) { $a->data[$i++] = $v; }
        }
        return $a;
    }
    public function toArray(): array { return $this->data; }
    public function getSize(): int { return count($this->data); }
    public function setSize(int $size): bool {
        $size = max(0, $size);
        $cur = count($this->data);
        if ($size > $cur) {
            $this->data = array_merge($this->data, array_fill(0, $size - $cur, null));
        } else {
            $this->data = array_slice($this->data, 0, $size);
        }
        return true;
    }
    private function normKey($key): int {
        if (is_object($key)) {
            throw new TypeError('Illegal SplFixedArray index type');
        }
        return (int) $key;
    }
    public function offsetExists($key): bool {
        $k = $this->normKey($key);
        return $k >= 0 && $k < count($this->data) && $this->data[$k] !== null;
    }
    public function offsetGet($key) {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        return $this->data[$k];
    }
    public function offsetSet($key, $value): void {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        $this->data[$k] = $value;
    }
    public function offsetUnset($key): void {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        $this->data[$k] = null;
    }
    public function count(): int { return count($this->data); }
    public function rewind(): void { $this->pos = 0; }
    public function valid(): bool { return $this->pos < count($this->data); }
    public function current() { return $this->data[$this->pos]; }
    public function key(): int { return $this->pos; }
    public function next(): void { $this->pos++; }
}
"#;
