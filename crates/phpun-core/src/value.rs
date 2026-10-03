use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

/// PHP arrays are insertion-ordered maps. Keys normalize per PHP rules:
/// `"8"` → 8, `"08"` stays string, `8.5` → 8, `true` → 1, `null` → "".
///
/// Stored as a Vec of entries (PHP tests exercise small arrays); existing
/// keys update in place so iteration order is insertion order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArrKey {
    Int(i64),
    Str(Rc<str>),
    /// Zend-style tombstone: `unset`/`array_shift` mark a bucket dead but
    /// keep its position so a live `foreach (&$v)` iteration over bucket
    /// positions still sees later elements (foreachLoop.013/.015).
    Tomb,
}

pub type Cell = Rc<RefCell<Value>>;

#[derive(Debug)]
pub struct PhpArray {
    /// Elements are cells so `$a[0] =& $x` and `foreach (&$v)` can alias them.
    pub entries: Vec<(ArrKey, Cell)>,
    /// Next free integer key for `$arr[] = ...` (max int key seen + 1).
    pub next: i64,
    /// Zend's is_ref: once elements are aliased (`foreach &$v`, `=&`),
    /// writes through a shared (copied) zval must NOT copy-on-write split.
    pub is_ref: bool,
}

impl Default for PhpArray {
    fn default() -> Self {
        Self::new()
    }
}

impl PhpArray {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 0,
            is_ref: false,
        }
    }

    pub fn get(&self, k: &ArrKey) -> Option<Value> {
        self.entries
            .iter()
            .find(|(ek, _)| ek == k)
            .map(|(_, v)| v.borrow().clone())
    }

    /// The cell holding an element (for by-ref binding).
    pub fn get_cell(&self, k: &ArrKey) -> Option<Cell> {
        self.entries
            .iter()
            .find(|(ek, _)| ek == k)
            .map(|(_, v)| v.clone())
    }

    pub fn push(&mut self, v: Value) {
        self.entries
            .push((ArrKey::Int(self.next), Rc::new(RefCell::new(v))));
        self.next += 1;
    }

    /// Append an existing cell (by-ref variadics alias their args).
    pub fn push_cell(&mut self, c: Cell) {
        self.entries.push((ArrKey::Int(self.next), c));
        self.next += 1;
    }

    pub fn set(&mut self, k: ArrKey, v: Value) {
        self.set_cell(k, Rc::new(RefCell::new(v)));
    }

    /// Insert or update. An existing key's cell is replaced with the new
    /// value (so aliases bound to the cell see it); a missing key appends.
    pub fn set_cell(&mut self, k: ArrKey, c: Cell) {
        if let Some(slot) = self.entries.iter_mut().find(|(ek, _)| *ek == k) {
            // Same cell on both sides (a $GLOBALS sync can alias the slot to
            // its own global) — writing it would borrow_mut+borrow itself.
            if Rc::ptr_eq(&slot.1, &c) {
                return;
            }
            *slot.1.borrow_mut() = c.borrow().clone();
            return;
        }
        if let ArrKey::Int(i) = k {
            if i >= self.next {
                self.next = i + 1;
            }
            self.entries.push((ArrKey::Int(i), c));
        } else {
            self.entries.push((k, c));
        }
    }

    /// Bind an element slot to a specific cell (`$a[k] =& $x`).
    pub fn bind_cell(&mut self, k: ArrKey, c: Cell) {
        if let ArrKey::Int(i) = k {
            if i >= self.next {
                self.next = i + 1;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = c;
        } else {
            self.entries.push((k, c));
        }
    }

    /// Remove a key (unset). The bucket is tombstoned — position kept,
    /// value gone (see ArrKey::Tomb). Returns whether it existed.
    pub fn unset(&mut self, k: &ArrKey) -> bool {
        if let Some(slot) = self.entries.iter_mut().find(|(ek, _)| ek == k) {
            slot.0 = ArrKey::Tomb;
            true
        } else {
            false
        }
    }

    /// Live entries only (tombstones skipped).
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &(ArrKey, Cell)> {
        self.entries
            .iter()
            .filter(|(k, _)| !matches!(k, ArrKey::Tomb))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of live elements.
    pub fn len(&self) -> usize {
        self.iter().count()
    }
}

impl Clone for PhpArray {
    fn clone(&self) -> Self {
        // Deep-clone cell contents (PHP copy-on-write: the copy is independent).
        Self {
            entries: self
                .entries
                .iter()
                .map(|(k, c)| (k.clone(), Rc::new(RefCell::new(c.borrow().clone()))))
                .collect(),
            next: self.next,
            is_ref: false,
        }
    }
}

/// PHP array keys as produced by `$arr[k]` indexing and array literals.
pub fn to_key(v: &Value) -> ArrKey {
    match v {
        Value::Int(i) => ArrKey::Int(*i),
        Value::Float(f) => ArrKey::Int(*f as i64),
        Value::Bool(b) => ArrKey::Int(*b as i64),
        Value::Null => ArrKey::Str("".into()),
        Value::Str(s) => {
            // Canonical integer strings become int keys ("8"→8, " 8"/"08"/"+8" don't).
            if let Some(i) = canonical_int(s) {
                ArrKey::Int(i)
            } else {
                // PHP array keys are byte strings; ArrKey keeps UTF-8 for
                // now — non-UTF8 keys collapse through the lossy path.
                ArrKey::Str(String::from_utf8_lossy(s).into_owned().into())
            }
        }
        Value::Array(_) | Value::Object(_) | Value::Callable(_) | Value::Resource(_) => {
            ArrKey::Str("".into()) // illegal key — caller warns
        }
    }
}

/// PHP's stack-trace argument printer: `'str'`, `Object(C)`, `Array`,
/// scalars as their plain value (tests/lang/type_hints_001.phpt).
/// Render Zend-style stack frames innermost-first, `#N {main}` last:
/// `#0 file(7): fn('a', 2)` / `#0 [internal function]: cb('x')`.
/// Internal callees hide their args (PHP: no arg info for builtins).
/// call_user_func*/forward_static_call trampolines are
/// ZEND_ACC_CALL_VIA_TRAMPOLINE — Zend omits them from backtraces
/// (named_params/call_user_func_array_variadic shows only the
/// forwarded `array_multisort(: 1)` frame).
pub fn trace_frame_hidden(fr: &TraceFrame) -> bool {
    fr.internal
        && matches!(
            fr.function.as_str(),
            "call_user_func"
                | "call_user_func_array"
                | "forward_static_call"
                | "forward_static_call_array"
        )
}

pub fn format_trace(frames: &[TraceFrame]) -> String {
    let rev: Vec<TraceFrame> = frames.iter().rev().cloned().collect();
    let mut t = format_backtrace_frames(&rev);
    t.push_str(&format!(
        "#{} {{main}}",
        frames.iter().filter(|f| !trace_frame_hidden(f)).count()
    ));
    t
}

/// debug_print_backtrace() output: innermost-first frames already ordered
/// by the caller, no `{main}` trailer.
pub fn format_backtrace_frames(frames: &[TraceFrame]) -> String {
    let mut t = String::new();
    let mut i = 0;
    for fr in frames.iter() {
        if trace_frame_hidden(fr) {
            continue;
        }
        let site = if fr.file == "[internal function]" {
            fr.file.clone()
        } else {
            format!("{}({})", fr.file, fr.line)
        };
        let callee = match &fr.class {
            Some(c) => format!("{}{}{}", c, fr.ty, fr.function),
            None => fr.function.clone(),
        };
        // Internal callees render their args too (PHP 8 shows
        // `strlen('a', 'b')`); named args render `name: value`.
        let mut arg_strs: Vec<String> = fr.args.iter().map(|c| trace_arg(&c.borrow())).collect();
        for (n, c) in &fr.named_args {
            arg_strs.push(format!("{}: {}", n, trace_arg(&c.borrow())));
        }
        let args = arg_strs.join(", ");
        t.push_str(&format!("#{} {}: {}({})\n", i, site, callee, args));
        i += 1;
    }
    t
}

pub fn trace_arg(v: &Value) -> String {
    match v {
        Value::Object(o) => format!("Object({})", o.borrow().class.name()),
        Value::Str(s) => {
            let ls = String::from_utf8_lossy(s);
            if ls.chars().count() > 15 {
                format!("'{}...'", ls.chars().take(15).collect::<String>())
            } else {
                format!("'{}'", ls)
            }
        }
        Value::Array(_) => "Array".into(),
        Value::Null => "NULL".into(),
        Value::Callable(_) => "Object(Closure)".into(),
        Value::Resource(_) => "Resource id #1".into(),
        other => other.to_php_string(),
    }
}

/// Lossy UTF-8 view of a byte string — for APIs/names that are
/// effectively always ASCII (function names, class names, identifiers).
pub fn lossy<'a>(s: &'a (impl AsRef<[u8]> + ?Sized)) -> std::borrow::Cow<'a, str> {
    String::from_utf8_lossy(s.as_ref())
}

/// Integer strings that PHP treats as int array keys: optional `-`, digits,
/// no leading `+`, no whitespace, no leading zeros (except "0").
pub fn canonical_int(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let t = || std::str::from_utf8(s).ok()?.parse::<i64>().ok();
    if s == b"0" || (s[0] == b'-' && s[1..].iter().all(|c| c.is_ascii_digit()) && s.len() > 1) {
        return t();
    }
    if s.iter().all(|c| c.is_ascii_digit()) && s[0] != b'0' {
        return t();
    }
    if s[0] == b'-' && s.len() > 1 {
        return t();
    }
    None
}

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// PHP strings are byte arrays — UTF-8 only at display boundaries.
    Str(Rc<[u8]>),
    /// Copy-on-write via Rc: clones share until mutated (see interp::set_index).
    Array(Rc<RefCell<PhpArray>>),
    /// Instances of user-defined and builtin classes.
    Object(Rc<RefCell<PhpObject>>),
    /// Closures (`function(){}`, `fn()=>`, first-class `f(...)`).
    Callable(Rc<PhpCallable>),
    /// `resource` — opaque handle for fopen() and friends.
    Resource(Rc<RefCell<PhpResource>>),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::Str(s.into().into_bytes().into())
    }

    /// Build a string Value from raw bytes (binary literals, byte ops).
    pub fn bytes(b: impl Into<Vec<u8>>) -> Self {
        Value::Str(b.into().into())
    }

    /// Byte-faithful string coercion — the workhorse for concat, offsets,
    /// preg, binary output. `to_php_string` is the lossy display variant.
    pub fn to_php_bytes(&self) -> Vec<u8> {
        match self {
            Value::Null => Vec::new(),
            Value::Bool(b) => {
                if *b {
                    b"1".to_vec()
                } else {
                    Vec::new()
                }
            }
            Value::Int(i) => i.to_string().into_bytes(),
            Value::Float(f) => format_float(*f).into_bytes(),
            Value::Str(s) => s.to_vec(),
            Value::Array(_) => b"Array".to_vec(),
            Value::Object(o) => format!("Object id #{}", o.borrow().id).into_bytes(),
            Value::Callable(_) => b"Closure".to_vec(),
            Value::Resource(r) => format!("Resource id #{}", r.borrow().id()).into_bytes(),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Callable(_) => "object",
            Value::Resource(_) => "resource",
        }
    }

    /// PHP gettype() names.
    pub fn gettype(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "double",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) | Value::Callable(_) => "object",
            Value::Resource(_) => "resource",
        }
    }

    /// PHP 8's `get_debug_type` — used in engine diagnostics ("int given",
    /// "true given", class name for objects).
    pub fn debug_type(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(_) => "int".to_string(),
            Value::Float(_) => "float".to_string(),
            Value::Str(_) => "string".to_string(),
            Value::Array(_) => "array".to_string(),
            Value::Object(o) => o.borrow().class.name().to_string(),
            Value::Callable(_) => "Closure".to_string(),
            Value::Resource(_) => "resource".to_string(),
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty() && s.as_ref() != b"0".as_slice(),
            Value::Array(a) => !a.borrow().is_empty(),
            Value::Object(_) | Value::Callable(_) => true,
            Value::Resource(_) => true,
        }
    }

    /// String cast *without* invoking __toString (interp handles that).
    pub fn to_php_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => if *b { "1" } else { "" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => String::from_utf8_lossy(s).into_owned(),
            // PHP raises "Array to string conversion" warning — caller emits it.
            Value::Array(_) => "Array".to_string(),
            Value::Object(o) => format!("Object id #{}", o.borrow().id),
            Value::Callable(_) => "Closure".to_string(),
            Value::Resource(r) => format!("Resource id #{}", r.borrow().id()),
        }
    }

    pub fn to_int(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Bool(b) => *b as i64,
            Value::Int(i) => *i,
            Value::Float(f) => *f as i64,
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => i,
                Numeric::Float(f) => f as i64,
                Numeric::Leading(f, _) => f as i64,
                Numeric::NonNumeric => 0,
            },
            Value::Array(a) => {
                if a.borrow().is_empty() {
                    0
                } else {
                    1
                }
            }
            Value::Object(_) | Value::Callable(_) => 1,
            Value::Resource(r) => r.borrow().id() as i64,
        }
    }

    pub fn to_float(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => *b as i64 as f64,
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => i as f64,
                Numeric::Float(f) => f,
                Numeric::Leading(f, _) => f,
                Numeric::NonNumeric => 0.0,
            },
            Value::Array(a) => {
                if a.borrow().is_empty() {
                    0.0
                } else {
                    1.0
                }
            }
            Value::Object(_) | Value::Callable(_) => 1.0,
            Value::Resource(r) => r.borrow().id() as f64,
        }
    }
}

/// Result of PHP's is_numeric-style string analysis.
pub enum Numeric {
    /// Fully numeric integer string (leading whitespace allowed).
    Int(i64),
    Float(f64),
    /// Leading numeric portion of a non-well-formed string
    /// (float value, true when the parsed part is an integer literal).
    Leading(f64, bool),
    NonNumeric,
}

impl Numeric {
    /// The parsed numeric portion as a float (0 for non-numeric).
    pub fn to_float(&self) -> f64 {
        match self {
            Numeric::Int(i) => *i as f64,
            Numeric::Float(f) | Numeric::Leading(f, _) => *f,
            Numeric::NonNumeric => 0.0,
        }
    }
}

/// Parse a string the way PHP coerces it to a number.
/// Accepts leading whitespace; trailing whitespace for fully-numeric forms.
pub fn numeric(s: &[u8]) -> Numeric {
    // PHP numeric-string whitespace: space, \t, \n, \r, \v, \f.
    let t = {
        let mut i = 0;
        while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
            i += 1;
        }
        &s[i..]
    };
    if t.is_empty() {
        return Numeric::NonNumeric;
    }
    let bytes = t;
    let mut i = 0;
    if bytes[i] == b'+' || bytes[i] == b'-' {
        i += 1;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                seen_digit = true;
                i += 1;
            }
            b'.' if !seen_dot && !seen_exp => {
                seen_dot = true;
                i += 1;
            }
            b'e' | b'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                i += 1;
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    if !seen_digit {
        return Numeric::NonNumeric;
    }
    let text = std::str::from_utf8(&t[..i]).unwrap_or("");
    let rest = &t[i..];
    if rest
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
    {
        if !seen_dot && !seen_exp {
            if let Ok(v) = text.parse::<i64>() {
                return Numeric::Int(v);
            }
        }
        if let Ok(v) = text.parse::<f64>() {
            return Numeric::Float(v);
        }
        Numeric::NonNumeric
    } else {
        let is_int = !seen_dot && !seen_exp && text.parse::<i64>().is_ok();
        text.parse::<f64>()
            .map(|f| Numeric::Leading(f, is_int))
            .unwrap_or(Numeric::NonNumeric)
    }
}

/// Format an f64 the way PHP's echo/string conversion does (precision=14).
/// Float → string for echo/print/casts (PHP `precision=14` rules).
pub fn format_float(f: f64) -> String {
    php_gcvt(f, 14)
}

/// Float → string for var_dump/print_r (PHP `serialize_precision=-1`:
/// shortest round-trip repr, G-style scientific cutoff at 17 digits).
pub fn format_float_repr(f: f64) -> String {
    php_gcvt(f, 17)
}

/// Float → string honoring an explicit INI precision (`precision=N` for
/// echo/casts, `serialize_precision=N` for var_dump/var_export/print_r):
/// forced %G formatting — always `n` significant digits (bug24640). A
/// negative `prec` selects the shortest round-trip form (`-1`).
pub fn format_float_prec(f: f64, prec: i64) -> String {
    if prec < 0 {
        format_float_repr(f)
    } else {
        php_gcvt_fixed(f, prec as usize)
    }
}

/// PHP zend_gcvt-style float formatting with a forced significant-digit
/// count (%.Ng): digits come from `{:.*e}` rounding, trailing zeros trimmed.
fn php_gcvt_fixed(f: f64, precision: usize) -> String {
    php_gcvt_impl(f, precision, true)
}

/// PHP zend_gcvt-style float formatting: significant digits come from the
/// shortest round-trip representation (rounded to `precision` digits only
/// when the shortest form is longer); scientific notation when the decimal
/// exponent is < -4 or >= precision.
fn php_gcvt(f: f64, precision: usize) -> String {
    php_gcvt_impl(f, precision, false)
}

fn php_gcvt_impl(f: f64, precision: usize, force: bool) -> String {
    if f.is_nan() {
        return "NAN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    let neg = f < 0.0;
    let (digits, exp) = gcvt_digits(f.abs(), precision, force);
    let nd = digits.len() as i64;
    let sign = if neg { "-" } else { "" };
    if exp < -4 || exp >= precision as i64 {
        // Scientific: X.YE±E — mantissa always carries a decimal point.
        let mant = if nd > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            format!("{}.0", &digits[..1])
        };
        format!(
            "{}{}E{}{}",
            sign,
            mant,
            if exp < 0 { "-" } else { "+" },
            exp.abs()
        )
    } else if exp >= 0 {
        let e = exp as usize;
        let s = if digits.len() <= e + 1 {
            format!("{}{}", digits, "0".repeat(e + 1 - digits.len()))
        } else {
            format!("{}.{}", &digits[..e + 1], &digits[e + 1..])
        };
        format!("{}{}", sign, s)
    } else {
        format!("{}0.{}{}", sign, "0".repeat((-exp - 1) as usize), digits)
    }
}

/// Significant digits (no decimal point) + decimal exponent of |v|.
/// Uses the shortest round-trip repr; if that exceeds `precision` digits
/// the value is re-rounded to `precision` digits.
fn gcvt_digits(v: f64, precision: usize, force: bool) -> (String, i64) {
    let split = |s: String| -> (String, i64) {
        let (mant, e) = s.split_once('e').unwrap();
        let exp: i64 = e.parse().unwrap_or(0);
        let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
        let digits = digits.trim_end_matches('0').to_string();
        let digits = if digits.is_empty() {
            "0".to_string()
        } else {
            digits
        };
        (digits, exp)
    };
    if force {
        return split(format!("{:.*e}", precision - 1, v));
    }
    let (d, e) = split(format!("{:e}", v));
    if d.len() > precision {
        split(format!("{:.*e}", precision - 1, v))
    } else {
        (d, e)
    }
}

/// C `%.*G` formatting for sprintf's %g/%h (fixed `precision` digits).
pub fn gcvt(value: f64, precision: usize) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let exp = value.abs().log10().floor() as i64;
    if exp < -4 || exp >= precision as i64 {
        let s = format!("{:.*e}", precision.saturating_sub(1), value);
        let (mant, exp_s) = s.split_once('e').unwrap();
        let mant = mant.trim_end_matches('0').trim_end_matches('.');
        let exp_v: i64 = exp_s.parse().unwrap_or(0);
        format!(
            "{}E{}{:02}",
            mant,
            if exp_v < 0 { "-" } else { "+" },
            exp_v.abs()
        )
    } else {
        let decimals = (precision as i64 - 1 - exp).max(0) as usize;
        let mut s = format!("{:.*}", decimals, value);
        if s.contains('.') {
            s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        s
    }
}

/// PHP loose comparison (`<=>` semantics) implementing the PHP 8 rules.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    match (a, b) {
        (Bool(_), _) | (_, Bool(_)) | (Null, _) | (_, Null) => a.is_truthy().cmp(&b.is_truthy()),
        (Int(_) | Float(_), Str(s)) => {
            match numeric(s) {
                // Int strings compare exactly — f64 would lose low bits on
                // 64-bit ints (operators/operator_equals_variation_64bit).
                Numeric::Int(si) => match a {
                    Int(ai) => ai.cmp(&si),
                    _ => num_cmp(a.to_float(), si as f64),
                },
                Numeric::Float(_) => num_cmp(a.to_float(), b.to_float()),
                // PHP 8: non-numeric (incl. leading-numeric) string →
                // the number is cast to string and compared as strings.
                Numeric::Leading(_, _) | Numeric::NonNumeric => {
                    a.to_php_bytes().as_slice().cmp(s.as_ref())
                }
            }
        }
        (Str(_), Int(_) | Float(_)) => compare(b, a).reverse(),
        (Int(x), Int(y)) => x.cmp(y),
        (Int(_) | Float(_), Int(_) | Float(_)) => num_cmp(a.to_float(), b.to_float()),
        (Str(x), Str(y)) => {
            // Both numeric strings → numeric compare, else string compare.
            match (numeric(x), numeric(y)) {
                (Numeric::Int(xi), Numeric::Int(yi)) => xi.cmp(&yi),
                (Numeric::Int(_) | Numeric::Float(_), Numeric::Int(_) | Numeric::Float(_)) => {
                    num_cmp(numeric(x).to_float(), numeric(y).to_float())
                }
                _ => x.as_ref().cmp(y.as_ref()),
            }
        }
        (Array(x), Array(y)) => {
            // Loose array comparison: equal if same key/values loosely.
            let x = x.borrow();
            let y = y.borrow();
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for (k, c) in &x.entries {
                match y.get(k) {
                    Some(yv) if compare(&c.borrow(), &yv) == Ordering::Equal => {}
                    _ => return Ordering::Less, // PHP's real rule is more subtle; approximate.
                }
            }
            Ordering::Equal
        }
        (Array(_), _) => Ordering::Greater,
        (_, Array(_)) => Ordering::Less,
        (Object(x), Object(y)) => {
            // Loose object ==: same class and loosely-equal props.
            let x = x.borrow();
            let y = y.borrow();
            if x.class.name() != y.class.name() {
                return Ordering::Less;
            }
            if x.props.len() != y.props.len() {
                return x.props.len().cmp(&y.props.len());
            }
            for (k, c) in x.props.iter() {
                match y.props.get(k) {
                    Some(yc) if compare(&c.borrow(), &yc.borrow()) == Ordering::Equal => {}
                    _ => return Ordering::Less,
                }
            }
            Ordering::Equal
        }
        (Object(_), _) | (Callable(_), _) => Ordering::Greater,
        (_, Object(_)) | (_, Callable(_)) => Ordering::Less,
        (Resource(x), Resource(y)) => x.borrow().id().cmp(&y.borrow().id()),
        (Resource(_), _) => Ordering::Greater,
        (_, Resource(_)) => Ordering::Less,
    }
}

fn num_cmp(a: f64, b: f64) -> Ordering {
    // NaN is never equal, not even to NaN (nan-comparison-false.phpt).
    a.partial_cmp(&b).unwrap_or(Ordering::Less)
}

/// Strict comparison `===`.
pub fn identical(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        (Array(x), Array(y)) => {
            let x = x.borrow();
            let y = y.borrow();
            x.len() == y.len()
                && x.iter().enumerate().all(|(i, (k, c))| {
                    // === also requires same order.
                    match y.entries.get(i) {
                        Some((yk, yc)) => k == yk && identical(&c.borrow(), &yc.borrow()),
                        None => false,
                    }
                })
        }
        (Object(x), Object(y)) => Rc::ptr_eq(x, y),
        (Callable(x), Callable(y)) => Rc::ptr_eq(x, y),
        (Resource(x), Resource(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_php_string())
    }
}

// ----- objects, closures, resources -----

/// A resolved class: declaration plus runtime state (static props).
#[derive(Debug)]
pub struct PhpClass {
    pub decl: Rc<crate::ast::ClassDecl>,
    /// `self::$prop` storage, initialized lazily from declared defaults.
    pub statics: RefCell<HashMap<String, Cell>>,
    pub statics_init: RefCell<bool>,
}

impl PhpClass {
    pub fn name(&self) -> &str {
        &self.decl.name
    }

    /// Method lookup walking the parent chain.
    pub fn find_method(&self, name: &str) -> Option<Rc<crate::ast::MethodDecl>> {
        let lname = name.to_lowercase();
        self.decl.find_method(&lname)
    }
}

#[derive(Debug)]
pub struct PhpObject {
    pub class: Rc<PhpClass>,
    /// Instance properties (declared + dynamic).
    pub props: HashMap<String, Cell>,
    /// Declared-property order for var_dump/foreach output.
    pub prop_order: Vec<String>,
    /// PHP's per-process object handle counter.
    pub id: u64,
    /// Internal payload for builtin classes (e.g. Exception fields).
    pub internal: Option<ObjectInternal>,
}

/// One recorded call for exception backtraces (getTrace()).
#[derive(Debug, Clone)]
pub struct TraceFrame {
    /// Callee name (`fopen`, `Error2Exception`, `Cls::m`/`{closure}`-ish).
    pub function: String,
    /// Class name for method calls (None for plain/builtin functions).
    pub class: Option<String>,
    /// `->` for object methods, `::` for static — empty for functions.
    pub ty: String,
    /// Call-site file and line; `"[internal function]"`/0 when the caller
    /// is a builtin (e.g. a userland callback invoked from ob_end_clean).
    pub file: String,
    pub line: u32,
    /// Call args (rendered with trace_arg).
    pub args: Vec<Cell>,
    /// Named args, rendered `name: value` after the positionals.
    pub named_args: Vec<(String, Cell)>,
    /// Callee is an internal/builtin function — marks builtin frames so
    /// callers can attribute userland callbacks (`[internal function]`).
    pub internal: bool,
}

pub enum ObjectInternal {
    /// Throwable fields (message/code/file/line/trace string).
    Exception {
        file: String,
        line: u32,
        /// Fully formatted trace body (`#0 f(1): g()\n#1 {main}`); empty →
        /// callers fall back to `#0 {main}`.
        trace: String,
        /// `thrown in` footer line — usually `line`; param TypeErrors
        /// attribute to the callee's declaration line.
        thrown: u32,
        /// Uncaught-display message when it differs from `message`
        /// (param TypeErrors show "... and defined in FILE:M").
        full_msg: String,
        /// ParseError raised inside eval()'d code: the inner source line.
        /// Uncaught display uses the plain `Parse error:` form
        /// (`in FILE(N) : eval()'d code on line M` — tests/lang/019).
        eval_ctx: u32,
        /// Call stack snapshot at construction → getTrace() (tests/lang/038).
        frames: Rc<Vec<TraceFrame>>,
    },
    /// SPL ArrayIterator state: backing array + iteration cursor.
    ArrayIter {
        arr: Rc<RefCell<PhpArray>>,
        pos: usize,
        flags: i64,
    },
    /// ReflectionAttribute payload: the attribute's name, unevaluated arg
    /// Exprs, and the TARGET_* bit of the declaration it was read from.
    ReflectionAttribute {
        name: String,
        args: Rc<Vec<crate::ast::Expr>>,
        target: i64,
    },
    /// PDO connection (spike #15): sqlite via rusqlite.
    Sqlite {
        conn: Rc<RefCell<rusqlite::Connection>>,
    },
    /// PDOStatement state: compiled query + materialized rows + cursor.
    SqliteStmt {
        conn: Rc<RefCell<rusqlite::Connection>>,
        sql: String,
        /// executed result rows: [(col_name, value)] per row
        rows: Vec<Vec<(String, Value)>>,
        affected: i64,
        /// fetch cursor
        pos: usize,
        /// positional binds from bindValue/bindParam
        bound: Vec<Value>,
        /// named binds (':' stripped)
        named: HashMap<String, Value>,
    },
    /// DateTime, closures-as-objects, etc. — opaque marker.
    None,
}

impl std::fmt::Debug for ObjectInternal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectInternal::Exception { .. } => f.write_str("Exception"),
            ObjectInternal::ArrayIter { .. } => f.write_str("ArrayIter"),
            ObjectInternal::ReflectionAttribute { .. } => f.write_str("ReflectionAttribute"),
            ObjectInternal::Sqlite { .. } => f.write_str("Sqlite"),
            ObjectInternal::SqliteStmt { .. } => f.write_str("SqliteStmt"),
            ObjectInternal::None => f.write_str("None"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PhpCallable {
    /// Zend object-store handle id (var_dump `object(Closure)#N`).
    pub id: std::cell::Cell<u64>,
    /// None for plain closures built from a decl.
    pub kind: CallableKind,
    /// Captured `use`/`fn` scope: name → cell.
    pub captures: Vec<(String, Cell)>,
    /// `$this` binding for methods-as-closures.
    pub this_obj: Option<Rc<RefCell<PhpObject>>>,
    /// Declared class context for `self::`/`static::` inside the body.
    pub scope_class: Option<Rc<PhpClass>>,
}

#[derive(Debug, Clone)]
pub enum CallableKind {
    /// Closure / arrow fn built from a decl.
    Closure(Rc<crate::ast::FunctionDecl>),
    /// `create_function`-style or first-class callable of a named function.
    Named(String),
    /// `[$objOrClass, 'method']` callable.
    Method {
        obj: Option<Rc<RefCell<PhpObject>>>,
        class: Option<Rc<PhpClass>>,
        name: String,
    },
}

#[derive(Debug)]
pub enum PhpResource {
    /// fopen(): a file handle with PHP mode flags.
    File {
        id: u64,
        file: std::fs::File,
        read: bool,
        write: bool,
        /// Byte position used for reads (we do our own buffering for fgets).
        pos: u64,
        eof: bool,
    },
    /// STDIN/STDOUT/STDERR — php:// and the CLI-SAPI constants.
    Stdio { id: u64, which: u8 },
    /// php://input — the request body, readable like a file.
    Input {
        id: u64,
        body: std::rc::Rc<Vec<u8>>,
        pos: u64,
    },
    /// curl/db handles etc. — opaque placeholder.
    Other { id: u64, kind: &'static str },
}

impl PhpResource {
    pub fn id(&self) -> u64 {
        match self {
            PhpResource::File { id, .. } => *id,
            PhpResource::Stdio { id, .. } => *id,
            PhpResource::Input { id, .. } => *id,
            PhpResource::Other { id, .. } => *id,
        }
    }
}

use std::collections::HashMap;
