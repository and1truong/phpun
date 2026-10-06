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
    /// "Deliberately shared" table flag (zend's IS_REFERENCE on the
    /// array zval itself): the $GLOBALS table, `&...$refs` variadic
    /// tables and arrays under a live `foreach(&$v)` iteration never
    /// CoW-split on write. NOT the element-aliasing mark — per-element
    /// references live in `ref_cells`.
    pub is_ref: bool,
    /// Internal pointer for current/key/next/prev/reset/end/each — an index
    /// into `entries` (may sit on a tombstone; live_* helpers skip it).
    pub iter_pos: usize,
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
            iter_pos: 0,
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

    /// First live (non-tombstone) index at or after `i`.
    fn live_at(&self, i: usize) -> Option<usize> {
        self.entries[i..]
            .iter()
            .position(|(k, _)| !matches!(k, ArrKey::Tomb))
            .map(|off| i + off)
    }

    /// Element under the internal pointer (skips tombstones).
    pub fn ptr_entry(&self) -> Option<&(ArrKey, Cell)> {
        self.live_at(self.iter_pos).map(|i| &self.entries[i])
    }

    /// Advance the internal pointer to the next live element.
    pub fn ptr_advance(&mut self) {
        if let Some(i) = self.live_at(self.iter_pos) {
            self.iter_pos = i + 1;
        } else {
            self.iter_pos = self.entries.len();
        }
    }

    /// Move the internal pointer to the previous live element.
    pub fn ptr_retreat(&mut self) {
        let mut i = self.iter_pos;
        while i > 0 {
            i -= 1;
            if !matches!(self.entries[i].0, ArrKey::Tomb) {
                self.iter_pos = i;
                return;
            }
        }
        self.iter_pos = self.entries.len();
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
            iter_pos: self.iter_pos,
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

/// Prop-table slot name for zend's int-keyed object bucket: an SPL
/// `[]=` append on object-backed storage lands in the prop hash under
/// an INT key — a name no userland prop write can produce. Surfaces
/// that enumerate props decode it back (`int_prop_index`).
pub fn int_prop_key(n: i64) -> String {
    format!("\0int\0{}", n)
}

/// Decode an int-keyed prop slot back to its int index.
pub fn int_prop_index(k: &str) -> Option<i64> {
    k.strip_prefix("\0int\0")?.parse().ok()
}

/// PHP's stack-trace argument printer: `'str'`, `Object(C)`, `Array`,
/// scalars as their plain value (tests/lang/type_hints_001.phpt).
/// Render Zend-style stack frames innermost-first, `#N {main}` last:
/// `#0 file(7): fn('a', 2)` / `#0 [internal function]: cb('x')`.
/// Internal callees hide their args (PHP: no arg info for builtins).
/// call_user_func* are ZEND_ACC_CALL_VIA_TRAMPOLINE — Zend omits them
/// from backtraces (named_params/call_user_func_array_variadic shows
/// only the forwarded `array_multisort(: 1)` frame).
/// forward_static_call* are ORDINARY internal functions — their frames
/// always render, and callees they dispatch sit at
/// `[internal function]`.
/// `!visible` frames — literal calls Zend compile-specializes into
/// dedicated opcodes (rope sprintf) — emit no call at all, so every
/// render path (backtraces, exception traces, fatal frames) skips
/// them here rather than at each call site.
pub fn trace_frame_hidden(fr: &TraceFrame) -> bool {
    !fr.visible
        || (fr.internal
            && !fr.named_dispatch
            && matches!(
                fr.function.as_str(),
                "call_user_func" | "call_user_func_array"
            ))
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
        t.push_str(&format!("#{} {}\n", i, trace_frame_str_at(fr, i)));
        i += 1;
    }
    t
}

pub fn trace_arg(v: &Value) -> String {
    match v {
        Value::Object(o) => format!("Object({})", o.borrow().class.name()),
        Value::Str(s) => {
            // Zend escapes args in stack traces: named escapes plus
            // `\xNN` (uppercase) for other non-printables.
            let esc: String = s
                .iter()
                .flat_map(|&b| {
                    let mut out = String::new();
                    match b {
                        b'\n' => out.push_str("\\n"),
                        b'\r' => out.push_str("\\r"),
                        b'\t' => out.push_str("\\t"),
                        0x0B => out.push_str("\\v"),
                        0x0C => out.push_str("\\f"),
                        0x1B => out.push_str("\\e"),
                        b'\\' => out.push_str("\\\\"),
                        0x20..=0x7E => out.push(b as char),
                        _ => out.push_str(&format!("\\x{:02X}", b)),
                    }
                    out.chars().collect::<Vec<_>>()
                })
                .collect();
            if esc.chars().count() > 15 {
                format!("'{}...'", esc.chars().take(15).collect::<String>())
            } else {
                format!("'{}'", esc)
            }
        }
        Value::Array(_) => "Array".into(),
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Callable(_) => "Object(Closure)".into(),
        Value::Resource(_) => "Resource id #1".into(),
        Value::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{f:.1}")
            } else {
                format_float_repr(*f)
            }
        }
        other => other.to_php_string(),
    }
}

/// `#N`-less frame body `file(line): Fn(args)` used by both
/// `format_backtrace_frames` and synthetic exception traces (arg-type
/// TypeErrors carry real callee frames below the call site).
pub fn trace_frame_str(fr: &TraceFrame) -> String {
    let site = if fr.file == "[internal function]" {
        fr.file.clone()
    } else {
        format!("{}({})", fr.file, fr.line)
    };
    let callee = match &fr.class {
        Some(c) => format!("{}{}{}", c, fr.ty, fr.function),
        None => fr.function.clone(),
    };
    let mut arg_strs: Vec<String> = fr.args.iter().map(|c| trace_arg(&c.borrow())).collect();
    for (n, c) in &fr.named_args {
        arg_strs.push(format!("{}: {}", n, trace_arg(&c.borrow())));
    }
    format!("{}: {}({})", site, callee, arg_strs.join(", "))
}

/// The `include`/`require` pseudo-frame the interpreter pushes around an
/// included file's execution — Zend's `require`/`include` backtrace
/// entries (the *_once kinds share these names).
pub fn include_frame(fr: &TraceFrame) -> bool {
    fr.internal
        && matches!(
            fr.function.as_str(),
            "include" | "include_once" | "require" | "require_once"
        )
}

/// Frame body with call args suppressed (`fn()` — no arg list).
fn trace_frame_str_noargs(fr: &TraceFrame) -> String {
    let mut f = fr.clone();
    f.args.clear();
    f.named_args.clear();
    trace_frame_str(&f)
}

/// Frame body for the `idx`-th frame of an innermost-first live
/// backtrace. The innermost include/require pseudo-frame renders bare
/// (`require()` — the include op_array's own executing context carries
/// no call args in Zend); deeper include frames keep their path
/// argument (`require('/tmp/x/inc....')`).
pub fn trace_frame_str_at(fr: &TraceFrame, idx: usize) -> String {
    if idx == 0 && include_frame(fr) {
        trace_frame_str_noargs(fr)
    } else {
        trace_frame_str(fr)
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
            Value::Resource(r) => {
                if matches!(&*r.borrow(), PhpResource::Closed { .. }) {
                    "resource (closed)"
                } else {
                    "resource"
                }
            }
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
            Value::Resource(r) => {
                if matches!(&*r.borrow(), PhpResource::Closed { .. }) {
                    "resource (closed)".to_string()
                } else {
                    "resource".to_string()
                }
            }
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

thread_local! {
    /// Left operands currently open on the compare stack — zend marks
    /// only the LEFT container while recursing inside it
    /// (GC_PROTECT_RECURSION(ht1) / Z_PROTECT_RECURSION_P(o1): "It's
    /// enough to protect only one of the arrays. The second one may
    /// be referenced from the first"); re-entering an already-marked
    /// LEFT operand through a cyclic reference aborts the whole
    /// comparison.
    static CMP_MARKS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// Set when a marked container is re-entered — zend fatals with
    /// "Nesting level too deep - recursive dependency?" rather than
    /// comparing equal. Read+cleared by the interpreter eval site.
    static CMP_DEPTH_ERR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Sticky "an exception is pending" for the compare layer — zend's
    /// `if (EG(exception)) return 1` inside zend_compare's conversion
    /// arm: once a depth Error is pending, later scalar-to-array /
    /// scalar-to-resource compares in the SAME sort report 1 too.
    /// Cleared alongside CMP_DEPTH_ERR at eval/builtin boundaries.
    static CMP_EXC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// E_NOTICEs raised inside a comparison (object→number casts);
    /// the interp layer drains and emits them at the call site so
    /// they flow through the user error-handler machinery.
    static CMP_NOTICES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Clear the cyclic-compare flag before a fresh top-level comparison.
pub fn clear_cmp_depth_err() {
    CMP_DEPTH_ERR.with(|f| f.set(false));
    CMP_EXC.with(|f| f.set(false));
}

/// zend's `EG(exception)` as compare sees it — the caller threw
/// mid-sort and every later conversion-arm compare answers 1.
pub(crate) fn cmp_exc() -> bool {
    CMP_EXC.with(|f| f.get())
}

/// True when a container pair was re-entered during the comparison
/// just run — the interpreter turns it into zend's catchable
/// `Error: Nesting level too deep - recursive dependency?`.
pub fn cmp_depth_err() -> bool {
    CMP_DEPTH_ERR.with(|f| f.get())
}

/// Take the notices a comparison just queued (object→number casts).
/// Called right after `compare`/`identical` at interp + builtin
/// boundaries; the messages go out as E_NOTICE in order.
pub fn take_cmp_notices() -> Vec<String> {
    CMP_NOTICES.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

/// PHP loose comparison (`<=>` semantics) implementing the PHP 8 rules.
pub fn compare(a: &Value, b: &Value) -> Ordering {
    let mark = match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            // Same zval short-circuits — zend's quick_equal never
            // descends into props (also covers cyclic self-compares).
            if Rc::ptr_eq(x, y) {
                return Ordering::Equal;
            }
            Some(Rc::as_ptr(x) as usize)
        }
        (Value::Array(x), Value::Array(y)) => {
            if Rc::ptr_eq(x, y) {
                return Ordering::Equal;
            }
            Some(Rc::as_ptr(x) as usize)
        }
        _ => None,
    };
    if let Some(ap) = mark {
        let reentered = CMP_MARKS.with(|v| {
            let mut v = v.borrow_mut();
            // zend's depth check fires on re-entry into a marked LEFT
            // operand — zend_hash_compare checks GC_IS_RECURSIVE(ht1)
            // before protecting ht1 alone. The right operand is
            // compared structurally with no mark check of its own, so
            // a (fresh, marked) pair still descends fine, e.g.
            // `$a=[[$n]]; $b=[&$a]; $a==$b` → false, no Error.
            if v.contains(&ap) {
                true
            } else {
                // The outermost call resets the flag so a stale one
                // left by non-interp callers (sort callbacks) can't
                // leak into the next eval.
                if v.is_empty() {
                    CMP_DEPTH_ERR.with(|f| f.set(false));
                }
                v.push(ap);
                false
            }
        });
        if reentered {
            // zend_hash_compare returns ZEND_UNCOMPARABLE (2) with the
            // depth Error pending — for every caller that survives the
            // error (a sort keeps comparing) that reads "greater".
            CMP_DEPTH_ERR.with(|f| f.set(true));
            CMP_EXC.with(|f| f.set(true));
            return Ordering::Greater;
        }
        let r = compare_r(a, b);
        CMP_MARKS.with(|v| {
            v.borrow_mut().pop();
        });
        return r;
    }
    compare_r(a, b)
}

fn compare_r(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    match (a, b) {
        // zend_compare's explicit type pairs ahead of the truthy
        // default block: null vs string compares by string length
        // alone — `null <=> "0"` is -1 (nonempty), not truthy-equal.
        (Null, Str(s)) => {
            if s.is_empty() {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        (Str(s), Null) => {
            if s.is_empty() {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        // DOUBLE×STRING / STRING×DOUBLE short-circuit on NaN — zend
        // returns 1 in BOTH directions.
        (Float(f), Str(_)) if f.is_nan() => Ordering::Greater,
        (Str(_), Float(f)) if f.is_nan() => Ordering::Greater,
        // zend's default-block bool/null arms (op<IS_TRUE / op==IS_TRUE
        // against zval_is_true) — truthiness on either side decides.
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
        // STRING×STRING → zendi_smart_strcmp.
        (Str(x), Str(y)) => smart_strcmp(x, y),
        (Array(x), Array(y)) => {
            // Loose array comparison (zend_hash_compare ordered=0):
            // equal len, then each ht1 key must exist in ht2 with an
            // equal element — the first differing pair's ordering is
            // the result; a missing key means ht1 > ht2.
            let x = x.borrow();
            let y = y.borrow();
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for (k, c) in &x.entries {
                match y.get(k) {
                    Some(yv) => {
                        let ord = compare(&c.borrow(), &yv);
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    None => return Ordering::Greater,
                }
            }
            Ordering::Equal
        }
        (Object(x), Object(y)) => {
            // Loose object ==: same class and loosely-equal props.
            // Like the array walk, the first differing prop's ordering
            // is the result; a prop missing in ht2 means ht1 > ht2.
            let x = x.borrow();
            let y = y.borrow();
            if x.class.name() != y.class.name() {
                // zend_std_compare_objects: different ce → ret 1.
                return Ordering::Greater;
            }
            if x.props.len() != y.props.len() {
                return x.props.len().cmp(&y.props.len());
            }
            // zend walks the properties hash in insertion order —
            // prop_order mirrors it; any leftover slots not tracked
            // there trail behind.
            let mut keys: Vec<&String> = x
                .prop_order
                .iter()
                .filter(|k| x.props.contains_key(*k))
                .collect();
            keys.extend(x.props.keys().filter(|k| !x.prop_order.contains(k)));
            for k in keys {
                match y.props.get(k) {
                    Some(yc) => {
                        let ord = compare(&x.props[k].borrow(), &yc.borrow());
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    None => return Ordering::Greater,
                }
            }
            Ordering::Equal
        }
        // Loose closure == : zend compares the wrapped function —
        // same function name/method target is equal (closure_compare).
        (Callable(x), Callable(y)) => {
            let eq = match (&x.kind, &y.kind) {
                (CallableKind::Named(a), CallableKind::Named(b)) => a.eq_ignore_ascii_case(b),
                (
                    CallableKind::Method {
                        obj: o1,
                        class: c1,
                        name: n1,
                    },
                    CallableKind::Method {
                        obj: o2,
                        class: c2,
                        name: n2,
                    },
                ) => {
                    n1.eq_ignore_ascii_case(n2)
                        && match (o1, o2) {
                            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                            (None, None) => true,
                            _ => false,
                        }
                        && match (c1, c2) {
                            (Some(a), Some(b)) => a.name() == b.name(),
                            (None, None) => true,
                            _ => false,
                        }
                }
                (CallableKind::Closure(d1), CallableKind::Closure(d2)) => {
                    Rc::ptr_eq(d1, d2)
                        && match (&x.this_obj, &y.this_obj) {
                            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                            (None, None) => true,
                            _ => false,
                        }
                }
                _ => false,
            };
            if eq {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        // Mixed object kinds (Closure object vs stdClass): zend's
        // zend_std_compare_objects returns 1 on class mismatch for
        // BOTH directions — asymmetric.
        (Object(_) | Callable(_), Object(_) | Callable(_)) => Ordering::Greater,
        // Object vs number: zend casts the object to the operand's
        // number type (an E_NOTICE "could not be converted") and it
        // counts as 1 — `new stdClass == 1` is true.
        (Object(_) | Callable(_), Int(_) | Float(_))
        | (Int(_) | Float(_), Object(_) | Callable(_)) => {
            let cls = match (a, b) {
                (Object(o), _) | (_, Object(o)) => o.borrow().class.name().to_string(),
                _ => "Closure".to_string(),
            };
            let ty = if matches!(a, Float(_)) || matches!(b, Float(_)) {
                "float"
            } else {
                "int"
            };
            CMP_NOTICES.with(|v| {
                v.borrow_mut().push(format!(
                    "Object of class {} could not be converted to {}",
                    cls, ty
                ))
            });
            if matches!(a, Object(_) | Callable(_)) {
                num_cmp(1.0, b.to_float())
            } else {
                num_cmp(a.to_float(), 1.0)
            }
        }
        // Objects beat everything else — including arrays.
        (Object(_) | Callable(_), _) => Ordering::Greater,
        (_, Object(_) | Callable(_)) => Ordering::Less,
        // zend's conversion arm: arrays and resources convert the pair
        // to numbers, and with an exception already pending the arm
        // returns 1 in BOTH directions ("to stop comparison of
        // arrays").
        (Array(_), _) => Ordering::Greater,
        (_, Array(_)) => {
            if cmp_exc() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Resource(x), Resource(y)) => {
            if cmp_exc() {
                Ordering::Greater
            } else {
                x.borrow().id().cmp(&y.borrow().id())
            }
        }
        (Resource(_), _) => Ordering::Greater,
        (_, Resource(_)) => {
            // zend converts the resource to its numeric handle and
            // does THREEWAY — a NaN left operand still wins — and a
            // pending exception returns 1 in both directions.
            if cmp_exc() || matches!(a, Float(f) if f.is_nan()) {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

/// ZEND_THREEWAY_COMPARE: `a==b ? 0 : (a<b ? -1 : 1)` — NaN fails both
/// legs so it reports 1: NaN sorts GREATER than everything
/// (`sort([1,NAN])` → `[NAN,1]`; `NAN <=> NAN` → 1).
pub(crate) fn num_cmp(a: f64, b: f64) -> Ordering {
    if a.is_nan() || b.is_nan() {
        Ordering::Greater
    } else {
        a.partial_cmp(&b).unwrap_or(Ordering::Equal)
    }
}

/// zendi_smart_strcmp (Zend/zend_operators.c): two fully-numeric
/// strings compare numerically — integer literals that overflowed
/// i64 in the same direction fall back to the byte compare (double
/// precision would tie), as do two same-sign infinities. Anything
/// else is a binary strcmp.
pub(crate) fn smart_strcmp(a: &[u8], b: &[u8]) -> Ordering {
    let na = numeric(a);
    let nb = numeric(b);
    let a_num = matches!(na, Numeric::Int(_) | Numeric::Float(_));
    let b_num = matches!(nb, Numeric::Int(_) | Numeric::Float(_));
    if !a_num || !b_num {
        return a.cmp(b);
    }
    // oflow in zend's is_numeric_string: the string is a pure integer
    // literal that overflowed i64 (numeric() reports it Float).
    fn int_oflow(s: &[u8], n: &Numeric) -> Option<i32> {
        if !matches!(n, Numeric::Float(_)) {
            return None;
        }
        let ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
        let mut i = 0;
        while i < s.len() && ws(s[i]) {
            i += 1;
        }
        let neg = s.get(i) == Some(&b'-');
        if matches!(s.get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        if i == start || s[i..].iter().any(|&c| !ws(c)) {
            return None;
        }
        Some(if neg { -1 } else { 1 })
    }
    let oa = int_oflow(a, &na);
    let ob = int_oflow(b, &nb);
    if let (Some(x), Some(y)) = (oa, ob) {
        if x == y && na.to_float() == nb.to_float() {
            // Same-direction integer overflows whose doubles tie —
            // precision lost, string-compare instead.
            return a.cmp(b);
        }
    }
    let a_dbl = matches!(na, Numeric::Float(_));
    let b_dbl = matches!(nb, Numeric::Float(_));
    if a_dbl || b_dbl {
        if !a_dbl {
            // a is a long, b a double: an overflowed-int b sits beyond
            // every representable long on its side.
            if let Some(y) = ob {
                return if y > 0 {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
        } else if !b_dbl {
            if let Some(x) = oa {
                return if x > 0 {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
        } else {
            let (da, db) = (na.to_float(), nb.to_float());
            if da == db && !da.is_finite() {
                return a.cmp(b);
            }
        }
        let d = na.to_float() - nb.to_float();
        return if d > 0.0 {
            Ordering::Greater
        } else if d < 0.0 {
            Ordering::Less
        } else {
            Ordering::Equal
        };
    }
    match (na, nb) {
        (Numeric::Int(x), Numeric::Int(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
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
            // Same zval → identical without descending (covers cyclic
            // self-compares, which zend resolves via zval_ptr_eq).
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let ap = Rc::as_ptr(x) as usize;
            // zend_hash_compare marks only the LEFT operand while
            // inside it (ordered=1 goes through the same impl) —
            // re-entering a marked left raises the same catchable
            // depth Error as == (the eval site reads CMP_DEPTH_ERR).
            // A marked right operand alone gets no check: zend
            // compares it structurally.
            let am = CMP_MARKS.with(|v| v.borrow().contains(&ap));
            if am {
                CMP_DEPTH_ERR.with(|f| f.set(true));
                CMP_EXC.with(|f| f.set(true));
                return false;
            }
            CMP_MARKS.with(|v| {
                v.borrow_mut().push(ap);
            });
            let x = x.borrow();
            let y = y.borrow();
            let r = x.len() == y.len()
                && x.iter().enumerate().all(|(i, (k, c))| {
                    // === also requires same order.
                    match y.entries.get(i) {
                        Some((yk, yc)) => k == yk && identical(&c.borrow(), &yc.borrow()),
                        None => false,
                    }
                });
            CMP_MARKS.with(|v| {
                v.borrow_mut().pop();
            });
            r
        }
        (Object(x), Object(y)) => Rc::ptr_eq(x, y),
        (Callable(x), Callable(y)) => Rc::ptr_eq(x, y),
        (Resource(x), Resource(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

// ----- zend_sort (libc++ introsort) -----

/// Element carried through a zend sort: the original insertion index
/// (zend stamps it into Z_EXTRA before sorting so the comparator's
/// RETURN_STABLE_SORT fallback can tiebreak Equal pairs on position),
/// plus the bucket's key/value cell.
pub(crate) type SortElem = (u32, ArrKey, Cell);

/// zend_sort_2/3/4/5: fixed sorting networks for the smallest slices —
/// element order, compare pairing and arg order byte-match the C.
fn zsort_2<T>(v: &mut [T], a: usize, b: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    if cmp(&v[a], &v[b]) == Ordering::Greater {
        v.swap(a, b);
    }
}

fn zsort_3<T>(v: &mut [T], a: usize, b: usize, c: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    if cmp(&v[a], &v[b]) != Ordering::Greater {
        if cmp(&v[b], &v[c]) != Ordering::Greater {
            return;
        }
        v.swap(b, c);
        if cmp(&v[a], &v[b]) == Ordering::Greater {
            v.swap(a, b);
        }
        return;
    }
    if cmp(&v[c], &v[b]) != Ordering::Greater {
        v.swap(a, c);
        return;
    }
    v.swap(a, b);
    if cmp(&v[b], &v[c]) == Ordering::Greater {
        v.swap(b, c);
    }
}

fn zsort_4<T>(
    v: &mut [T],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    zsort_3(v, a, b, c, cmp);
    if cmp(&v[c], &v[d]) == Ordering::Greater {
        v.swap(c, d);
        if cmp(&v[b], &v[c]) == Ordering::Greater {
            v.swap(b, c);
            if cmp(&v[a], &v[b]) == Ordering::Greater {
                v.swap(a, b);
            }
        }
    }
}

fn zsort_5<T>(
    v: &mut [T],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    e: usize,
    cmp: &mut impl FnMut(&T, &T) -> Ordering,
) {
    zsort_4(v, a, b, c, d, cmp);
    if cmp(&v[d], &v[e]) == Ordering::Greater {
        v.swap(d, e);
        if cmp(&v[c], &v[d]) == Ordering::Greater {
            v.swap(c, d);
            if cmp(&v[b], &v[c]) == Ordering::Greater {
                v.swap(b, c);
                if cmp(&v[a], &v[b]) == Ordering::Greater {
                    v.swap(a, b);
                }
            }
        }
    }
}

/// zend_insert_sort: networks for n<=5, sentinel insertion above.
/// The first pass sorts elements 0..5; the second strides down by
/// two, guarded by that sorted prefix — ported line-for-line so the
/// compare sequence under a non-total relation is zend's.
fn zend_insert_sort<T>(v: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    match v.len() {
        0 | 1 => {}
        2 => zsort_2(v, 0, 1, cmp),
        3 => zsort_3(v, 0, 1, 2, cmp),
        4 => zsort_4(v, 0, 1, 2, 3, cmp),
        5 => zsort_5(v, 0, 1, 2, 3, 4, cmp),
        _ => {
            let n = v.len();
            let sentry = 6;
            for i in 1..sentry {
                let mut j = i - 1;
                if cmp(&v[j], &v[i]) != Ordering::Greater {
                    continue;
                }
                while j != 0 {
                    j -= 1;
                    if cmp(&v[j], &v[i]) != Ordering::Greater {
                        j += 1;
                        break;
                    }
                }
                let mut k = i;
                while k > j {
                    v.swap(k, k - 1);
                    k -= 1;
                }
            }
            for i in sentry..n {
                let mut j = i - 1;
                if cmp(&v[j], &v[i]) != Ordering::Greater {
                    continue;
                }
                loop {
                    j -= 2;
                    if cmp(&v[j], &v[i]) != Ordering::Greater {
                        j += 1;
                        if cmp(&v[j], &v[i]) != Ordering::Greater {
                            j += 1;
                        }
                        break;
                    }
                    if j == 0 {
                        break;
                    }
                    if j == 1 {
                        j -= 1;
                        if cmp(&v[i], &v[j]) == Ordering::Greater {
                            j += 1;
                        }
                        break;
                    }
                }
                let mut k = i;
                while k > j {
                    v.swap(k, k - 1);
                    k -= 1;
                }
            }
        }
    }
}

/// zend_sort (Zend/zend_sort.c, php-8.5.11): the libc++-derived
/// introsort — insertion sort at n<=16, quicksort with a median pivot
/// above, recursing on the smaller partition and looping on the
/// larger. Element pairing and `cmp(arg1, arg2)` operand order match
/// the C exactly, which is observable whenever the comparator is not
/// a total order (loose compare's bool arm) — and the left operand
/// stays the cyclic-protected one.
pub(crate) fn zend_sort<T>(v: &mut [T], cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    let mut base = 0usize;
    let mut n = v.len();
    loop {
        if n <= 16 {
            zend_insert_sort(&mut v[base..base + n], cmp);
            return;
        }
        let start = base;
        let end = base + n;
        let offset = n >> 1;
        let mut pivot = start + offset;
        if n >> 10 != 0 {
            let delta = offset >> 1;
            zsort_5(v, start, start + delta, pivot, pivot + delta, end - 1, cmp);
        } else {
            zsort_3(v, start, pivot, end - 1, cmp);
        }
        v.swap(start + 1, pivot);
        pivot = start + 1;
        let mut i = pivot + 1;
        let mut j = end - 1;
        'part: loop {
            while cmp(&v[pivot], &v[i]) == Ordering::Greater {
                i += 1;
                if i == j {
                    break 'part;
                }
            }
            j -= 1;
            if j == i {
                break 'part;
            }
            while cmp(&v[j], &v[pivot]) == Ordering::Greater {
                j -= 1;
                if j == i {
                    break 'part;
                }
            }
            v.swap(i, j);
            i += 1;
            if i == j {
                break 'part;
            }
        }
        v.swap(pivot, i - 1);
        let left = (i - 1) - start;
        let right = end - i;
        if left < right {
            zend_sort(&mut v[start..i - 1], cmp);
            base = i;
            n = right;
        } else {
            zend_sort(&mut v[i..end], cmp);
            n = left;
        }
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
        // Anonymous classes carry a `$LINE` uniquifier internally;
        // Zend's public name is `{Base}@anonymous`.
        if let Some(pos) = self.decl.name.find("@anonymous$") {
            &self.decl.name[..pos + "@anonymous".len()]
        } else {
            &self.decl.name
        }
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
    /// Typed props that were `unset()` — reads route to `__get` like
    /// undefined props instead of the uninitialized-typed Error.
    pub unset_props: std::collections::HashSet<String>,
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
    /// Zend emits this frame in exception/backtraces — every real call
    /// produces one, literal or dynamic. False only for a literal call
    /// Zend compile-specializes into dedicated opcodes (a const-format
    /// `sprintf` becomes rope-concat — no call exists, conversion
    /// errors trace `{main}` only).
    pub visible: bool,
    /// A call_user_func* call carrying named args isn't trampoline-
    /// inlined in Zend — it's a real internal frame: it shows in
    /// traces (overriding the cufa transparency filter) and the
    /// callee's call site attributes to `[internal function]`.
    pub named_dispatch: bool,
}

/// Shared storage slot for spl array-objects — zend's `intern->array`
/// zval. Objects linked by getIterator()/exchangeArray()/spl-source
/// construction hold clones of this cell, so a storage swap reaches
/// every sibling; `pos`/`flags`/`iterator_class` stay per-object.
pub struct AoStore {
    /// The backing table (prop-mirror for object storage).
    pub arr: Rc<RefCell<PhpArray>>,
    /// The backing OBJECT when storage came from an object input —
    /// zend serializes it as `__serialize()` slot 1 instead of the
    /// storage hash. For a self-backed object (ctor arg `$this`) this
    /// is the object itself and flag bit 0x1000000 is set on `flags`.
    pub src: Option<Rc<RefCell<PhpObject>>>,
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
    /// SPL ArrayIterator state: shared storage slot + iteration cursor.
    ArrayIter {
        /// The `intern->array` slot: getIterator()/exchangeArray()
        /// siblings see the same backing table because they hold clones
        /// of this cell, not copies of the table Rc.
        store: Rc<RefCell<AoStore>>,
        pos: usize,
        flags: i64,
        /// ArrayObject's `iteratorClass` ctor arg / setIteratorClass —
        /// a validated ArrayIterator-derived class name getIterator()
        /// instantiates; None = "ArrayIterator".
        iterator_class: Option<String>,
        /// zend's nApplyCount > 0: true while a sort method runs —
        /// storage mutations (dim writes, exchangeArray, unserialize)
        /// raise "Modification of X during sorting is prohibited".
        sorting: bool,
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
    /// `yield`-function deferred execution: the call returns a Generator
    /// object; the body runs on the first Iterator method and every
    /// yielded (key, value) lands in `items`.
    Generator(Rc<RefCell<GenState>>),
    /// DirectoryIterator state: the dir's entry paths + cursor.
    DirIter {
        entries: Vec<String>,
        pos: usize,
        flags: i64,
        /// Path of the iterated dir relative to the root iterator's dir
        /// (RecursiveDirectoryIterator::getSubPath).
        sub_path: String,
    },
    /// DateTime, closures-as-objects, etc. — opaque marker.
    None,
}

/// One yielded pair — the value cell so `&function` generators can
/// yield by reference (typed_properties_033/034).
pub type GenItem = (Value, Cell);

/// Generator internal state (object internal behind the `Generator`
/// class, which implements `Iterator`).
pub struct GenState {
    /// Everything needed to re-enter the function frame later.
    pub setup: GenSetup,
    /// Materialized (key, value) pairs after the body ran.
    pub items: Vec<GenItem>,
    /// Iteration cursor.
    pub pos: usize,
    /// Body has been started (ran eagerly on first use).
    pub started: bool,
    /// Body completed (items final).
    pub finished: bool,
    /// `return` value — read by getReturn().
    pub return_val: Value,
    /// `function &gen()` — yields expose their cells to `foreach ..&`.
    pub by_ref: bool,
    /// Auto keys for keyless `yield $v` (0, 1, 2…).
    pub auto_key: i64,
    /// Every send() value ever passed, in call order — the k-th send
    /// feeds the k-th yield expression when the body (re)runs.
    pub sends: Vec<Value>,
    /// Output produced after a yield suspends mid-expression — Zend
    /// defers it to resume; buffered per yield index and emitted when
    /// the consumer advances `pos` past it (closure_call_leak).
    pub pending_out: Vec<(usize, Vec<u8>)>,
}

pub enum GenSetup {
    /// invoke_fn capture: decl + evaluated args + call context.
    Invoke {
        decl: Rc<crate::ast::FunctionDecl>,
        args: crate::interp::CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
        decl_class: Option<Rc<PhpClass>>,
        called_class: Option<Rc<PhpClass>>,
        /// `use ($a, &$b)` cells for closure-generators.
        captures: Vec<(String, Cell, bool)>,
        /// The generator-creating closure — its id keys the
        /// per-instance statics table (`fn_statics_key`).
        closure_rc: Option<Rc<PhpCallable>>,
    },
}

impl std::fmt::Debug for ObjectInternal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObjectInternal::Exception { .. } => f.write_str("Exception"),
            ObjectInternal::ArrayIter { .. } => f.write_str("ArrayIter"),
            ObjectInternal::ReflectionAttribute { .. } => f.write_str("ReflectionAttribute"),
            ObjectInternal::Generator { .. } => f.write_str("Generator"),
            ObjectInternal::DirIter { .. } => f.write_str("DirIter"),
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
    pub captures: Vec<(String, Cell, bool)>,
    /// `$this` binding for methods-as-closures.
    pub this_obj: Option<Rc<RefCell<PhpObject>>>,
    /// Declared class context for `self::`/`static::` inside the body.
    pub scope_class: Option<Rc<PhpClass>>,
    /// Late-static-binding class captured at creation — `static::`
    /// inside the body resolves to it (closure_049-052, bug66622).
    pub called_class: Option<Rc<PhpClass>>,
    /// `static function`/static-method callables can never bind $this
    /// (closure_041/043, disallows_*).
    pub is_static: bool,
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
    /// php://memory / php://temp — an in-memory byte buffer that is
    /// always read/write, seekable (Composer's BufferIO).
    Mem {
        id: u64,
        buf: Vec<u8>,
        pos: u64,
        eof: bool,
        /// fwrite honors the fopen mode ('r' → false); fprintf does not
        /// (zend php_stream_printf bypasses the check).
        write: bool,
    },
    /// A resource closed via fclose()/fclose-aliased wrappers — Zend
    /// keeps the zval `resource (closed)` (gettype "resource (closed)",
    /// var_dump "of type (Unknown)", is_resource() false) and every
    /// stream function on it throws "must be an open stream resource".
    Closed { id: u64 },
    /// curl/db handles etc. — opaque placeholder.
    Other { id: u64, kind: &'static str },
}

impl PhpResource {
    pub fn id(&self) -> u64 {
        match self {
            PhpResource::File { id, .. } => *id,
            PhpResource::Stdio { id, .. } => *id,
            PhpResource::Input { id, .. } => *id,
            PhpResource::Mem { id, .. } => *id,
            PhpResource::Closed { id, .. } => *id,
            PhpResource::Other { id, .. } => *id,
        }
    }

    /// Zend's `zend_rsrc_list_get_rsrc_type` name for var_dump's
    /// `of type (..)` and `get_resource_type()`.
    pub fn type_name(&self) -> &'static str {
        match self {
            PhpResource::Closed { .. } => "Unknown",
            PhpResource::Other { kind, .. } => kind,
            _ => "stream",
        }
    }
}

use std::collections::HashMap;
