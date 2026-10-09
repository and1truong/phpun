//! Expression evaluation: `eval` and the assign/store/index/cell
//! machinery, typed slots, inc/dec, unary/binary/arith ops and casts.

use super::util::*;
use super::*;

/// Newlines before the first token of a `${...}` interp snippet —
/// whitespace and `//`, `#`, `/* */` comments don't produce tokens,
/// so a folded name's CV-equivalent line is `base` plus this count.
fn leading_src_lines(src: &str) -> usize {
    let b = src.as_bytes();
    let mut i = 0;
    let mut lines = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => {
                lines += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' | 0x0b | 0x0c => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    if b[i] == b'\n' {
                        lines += 1;
                    }
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => break,
        }
    }
    lines
}

/// One match/switch cond's shape after zend's compile-time const
/// scan (`zend_eval_const_expr` inside the jumptable probes) — see
/// `Interp::cond_fold`.
pub(in crate::interp) enum CondFold<'a> {
    /// Already a zval — the compare keeps this node's own token line
    /// (the scanned expr carries its `argline` marker).
    Literal(&'a Expr, Value),
    /// Folded to a zval stamped at the subject's compiled-end line.
    Folded(Value),
    /// Didn't fold — ends the const scan. The node is what remains in
    /// the cond slot: zend_eval_const_expr splices the surviving `??`
    /// /`?:` branch INTO it, so the compare's operand is that child
    /// (a CV there binds inside the compare like any other).
    Unfolded(&'a Expr),
}

impl CondFold<'_> {
    fn value(&self) -> &Value {
        match self {
            CondFold::Literal(_, v) | CondFold::Folded(v) => v,
            CondFold::Unfolded(_) => &Value::Null,
        }
    }
}

/// A dim-write operand for `assign_index_path`: `[]` appends; `Key`
/// carries an eagerly-bound key cell (literals and expression keys —
/// zend evaluates them with the lvalue chain before the RHS); `Cv`
/// defers a plain-`$var` key to its own dim op, so an intermediate
/// FETCH_DIM_W failure preempts the var's read (`$b['x'][$u] = v` on
/// a scalar `$b['x']` fatals without warning `$u`).
#[derive(Clone)]
enum DimArg {
    Append,
    Key(Cell),
    /// The CV's marked (argline) line sites its own read diagnostic.
    Cv(String, Option<usize>),
}

impl From<Option<Cell>> for DimArg {
    fn from(k: Option<Cell>) -> Self {
        match k {
            Some(c) => DimArg::Key(c),
            None => DimArg::Append,
        }
    }
}

/// An unset dim key: a plain-`$var` key is read inside its own
/// FETCH_DIM_UNSET level (zend reads the container operand first, the
/// CV second — a missing-root unset still runs every later CV read's
/// warn); every other key expr is a delayed dim, evaluated ahead of
/// the fetch chain (`unset($a[lit()][$u])` prints LIT then warns $u).
enum UnsetKey {
    /// The CV's marked (argline) line sites its own read diagnostic.
    Cv(String, Option<usize>),
    Val(Value),
}

/// The container binding a var-rooted dim write detached from:
/// `name` is the root variable and `pre` its value when the dim op
/// began (zend binds the operand slot at op entry; a handler that
/// rebinds the var mid-key-eval leaves the write landing on the stale
/// slot — invisible and silent, assign_dim_014).
/// The op-start container for a var-rooted dim op — captured as a
/// Weak so the pending write doesn't hold a strong Rc (which would
/// make every shared-looking array cow-split per write, an O(N) hit).
/// Value-types keep their value (zend's refcount sentinel can't fire
/// on non-refcounted scalars — no detach check exists there).
#[derive(Clone)]
enum DimPre {
    Arr(std::rc::Weak<RefCell<PhpArray>>),
    Obj(std::rc::Weak<RefCell<PhpObject>>),
    Str(std::rc::Weak<[u8]>),
    Callable(std::rc::Weak<PhpCallable>),
    Res(std::rc::Weak<RefCell<PhpResource>>),
    Scalar(Value),
}

impl DimPre {
    fn of(v: &Value) -> Self {
        match v {
            Value::Array(rc) => Self::Arr(Rc::downgrade(rc)),
            Value::Object(rc) => Self::Obj(Rc::downgrade(rc)),
            Value::Str(rc) => Self::Str(Rc::downgrade(&rc.rc)),
            Value::Callable(rc) => Self::Callable(Rc::downgrade(rc)),
            Value::Resource(rc) => Self::Res(Rc::downgrade(rc)),
            _ => Self::Scalar(v.clone()),
        }
    }

    /// The materialized op-start value — `None` when a refcounted
    /// container was fully destroyed since capture.
    fn value(&self) -> Option<Value> {
        match self {
            Self::Arr(w) => w.upgrade().map(Value::Array),
            Self::Obj(w) => w.upgrade().map(Value::Object),
            Self::Str(w) => w
                .upgrade()
                .map(|rc| Value::Str(crate::value::PhpStr::adopt(rc))),
            Self::Callable(w) => w.upgrade().map(Value::Callable),
            Self::Res(w) => w.upgrade().map(Value::Resource),
            Self::Scalar(v) => Some(v.clone()),
        }
    }
}

#[derive(Clone)]
struct DimDetach {
    name: String,
    pre: DimPre,
    /// The var CELL's Rc ptr at op entry — non-refcounted `pre`
    /// (scalars/Null) can't detach by zval identity, so only a cell
    /// replacement counts as a rebind (auto-viv Null → Array mutates
    /// the same cell and stays bound). 0 = unknown (det built on a
    /// scratch, e.g. `??=`'s Null `cur`).
    pre_cell: usize,
    /// `??=`'s assign is a separate op dispatching on the CURRENT
    /// container — its write runs on that value, not `pre`.
    coalesce: bool,
}

impl DimDetach {
    /// The stale op-start slot a detached write lands on — a dead
    /// string weak still gets an (empty) Str scratch so zend's
    /// string-offset conversion diagnostics dispatch before the
    /// write drops (assign_to_string_offset addref'd the string
    /// across check_string_offset, then aborts on the delref).
    fn scratch(&self) -> Cell {
        cell(match &self.pre {
            DimPre::Str(w) => w
                .upgrade()
                .map(|rc| Value::Str(crate::value::PhpStr::adopt(rc)))
                .unwrap_or_else(|| Value::bytes(Vec::new())),
            _ => self.pre.value().unwrap_or(Value::Null),
        })
    }
}

impl<'a> Interp<'a> {
    // ----- expressions -----

    /// The line an expression's evaluation ends at — zend's
    /// post-eval lineno. A call leaves it at the deepest last-arg
    /// marker (dispatch re-sites at the call's own site), a prop
    /// read at the member name's line, a ternary/binary at the last
    /// source operand's end. `None` when `e` has no recorded end
    /// (cur_line already holds the right line).
    pub(in crate::interp) fn inner_end_line(e: &Expr) -> Option<usize> {
        crate::ast::end_line(e)
    }

    /// Runs a call-producing expression with send_line scoped to that
    /// call's own dispatch: the site covers frames pushed while binding
    /// and invoking, then the previous send_line resumes — a callee's
    /// own `new`/call sites must not leak into the caller's error
    /// lines (foreach's traversable check after getIterator()).
    fn scoped_send<T>(
        &mut self,
        run: impl FnOnce(&mut Self) -> Result<T, PhpError>,
    ) -> Result<T, PhpError> {
        let prev = self.send_line;
        let r = run(self);
        self.send_line = prev;
        r
    }

    pub fn eval(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Interp(parts) => {
                let mut s: Vec<u8> = Vec::new();
                for p in parts {
                    match p {
                        StringPart::Lit(t) => s.extend_from_slice(t),
                        // Each interpolated part sites at its own
                        // absolute line (the marker's/the `$`'s line):
                        // Zend attributes the part's read ops there, so
                        // diagnostics inside unmarked positions and in
                        // `$var` parts report the part's line, not the
                        // enclosing statement's.
                        StringPart::Var(name, base) => {
                            self.cur_line = *base;
                            self.send_line = Some(*base);
                            let v = self.var_get(name)?;
                            let cs = self.conv_bytes(&v)?;
                            s.extend_from_slice(&cs);
                        }
                        StringPart::Expr(src, base) => {
                            self.cur_line = *base;
                            self.send_line = Some(*base);
                            let (expr, _) = parser::parse_expr_src(src, *base)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let v = self.eval(&expr)?;
                            // The part's trailing conversion sites at
                            // the inner expr's last evaluated line —
                            // a call-shaped inner leaves cur_line at
                            // the call site, so re-site; the stale
                            // part-base send_line goes with it.
                            if let Some(l) = Self::inner_end_line(&expr) {
                                self.cur_line = l;
                            }
                            self.send_line = Some(self.cur_line);
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                        StringPart::DollarBraceExpr(src, base) => {
                            self.cur_line = *base;
                            self.send_line = Some(*base);
                            // `${expr}` — deprecated variable-variable
                            // interpolation; its deprecation + inner
                            // diagnostics already emitted at lex time
                            // (heredoc_nowdoc/flexible-heredoc-complex-*).
                            let (expr, _) = parser::parse_expr_src(src, *base)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let nv = self.eval(&expr)?;
                            // A folded name reads like a CV: it sites at
                            // the inner's first-token line (base plus the
                            // snippet's leading newlines). Otherwise the
                            // name-conversion + variable read site at
                            // the inner expr's last evaluated line (same
                            // re-site as the StringPart::Expr above).
                            if is_compile_const(&expr) {
                                self.cur_line = *base + leading_src_lines(src);
                            } else if let Some(l) = Self::inner_end_line(&expr) {
                                self.cur_line = l;
                            }
                            self.send_line = Some(self.cur_line);
                            let name = self.conv_str(&nv)?;
                            let v = self.var_get(&name)?;
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                    }
                }
                Ok(Value::bytes(s))
            }
            Expr::Var(name) => self.var_get(name),
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                if is_compile_const(inner) {
                    // A folded varvar reads exactly like a CV: the
                    // name read sites at the inner's first-token line
                    // (the inner's `argline` already sited cur_line
                    // there during eval — keep it), except where an
                    // enclosing op binds the CV's read: vv_rhs_site
                    // carries that op's line (`=` RHS, delayed dim,
                    // `->` member, binary op, match/switch compare).
                    if let Some(l) = self.vv_rhs_site.take() {
                        self.cur_line = l;
                    }
                } else if let Some(l) = Self::inner_end_line(inner) {
                    // The name-conversion + variable read site at the
                    // inner expr's last evaluated line (zend's
                    // post-eval lineno) — same re-site as the
                    // `{$...}` re-parse path above.
                    self.cur_line = l;
                }
                self.send_line = Some(self.cur_line);
                let name = self.conv_str(&n)?;
                self.var_get(&name)
            }
            Expr::Const(name) => self.const_read(name),
            Expr::MagicConst(m) => Ok(self.magic(*m)),
            Expr::ArrayLit(items) => {
                let mut arr = PhpArray::new();
                for (k, v) in items {
                    // Elements carry an `argline` marker for their own
                    // first-token line — look past it for the
                    // by-ref/spread shapes, but eval the marked expr so
                    // the marker still sets the line.
                    let shape = Self::unmark_rhs(v);
                    // `&$x` elements bind the source cell, not a copy.
                    if let Expr::ByRef(e) = shape {
                        // `&$this` can never be IS_REFERENCE — the
                        // element holds a plain copy of the object
                        // handle (bug72598).
                        if matches!(Self::unmark_rhs(e), Expr::Var(n) if n == "this") {
                            let c = cell(self.eval(e)?);
                            match k {
                                Some(ke) => {
                                    let kv = self.eval(ke)?;
                                    arr.set_cell(self.arr_key(&kv)?, c);
                                }
                                None => arr.push_cell(c),
                            }
                            continue;
                        }
                        let c = self.eval_cell(e)?;
                        self.mark_ref(&c);
                        self.reg_arr_ref(&c);
                        match k {
                            Some(ke) => {
                                let kv = self.eval(ke)?;
                                arr.bind_cell(self.arr_key(&kv)?, c);
                            }
                            None => {
                                let key = ArrKey::Int(arr.next);
                                arr.bind_cell(key, c);
                            }
                        }
                        continue;
                    }
                    // Zend evaluates the key expression before the value
                    // (namespaces/ns_077_3).
                    match k {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            let val = self.eval(v)?;
                            let key = self.arr_key(&kv)?;
                            // A keyed overwrite of a bound-ref bucket
                            // installs a fresh cell — zend's update
                            // separates, never writes into a reference
                            // an earlier element (spread/`&`) still
                            // shares.
                            match arr.get_cell(&key) {
                                Some(s) if self.is_ref_cell(&s) => {
                                    arr.bind_cell(key, cell(val));
                                }
                                _ => arr.set(key, val),
                            }
                        }
                        None => {
                            // `...$it` spread: int keys renumber
                            // positionally, string keys set (PHP 8.1+).
                            if let Expr::Unpack(e) = shape {
                                let sv = self.eval(e)?;
                                for (sk, c, shared) in self.unpack_items(&sv, false)? {
                                    // zend zval-copies elements — a live
                                    // IS_REFERENCE one keeps its cell.
                                    match sk {
                                        Some(s) => crate::builtins::array::copy_elem_shared(
                                            self,
                                            &mut arr,
                                            &ArrKey::Str(s),
                                            &c,
                                            shared,
                                        ),
                                        None => crate::builtins::array::push_elem_shared(
                                            &mut arr, &c, shared,
                                        ),
                                    }
                                }
                                continue;
                            }
                            let val = self.eval(v)?;
                            arr.push(val);
                        }
                    }
                }
                // zend: HashTable alloc + one bucket per element —
                // tracked so the charge releases when the literal dies.
                let rc = Rc::new(RefCell::new(arr));
                // zend allocs the packed/mixed arData up front.
                let req = {
                    let b = rc.borrow();
                    Self::ht_req(b.entries.len(), b.packed)
                };
                self.mem_track(&rc, req);
                Ok(Value::Array(rc))
            }
            Expr::ByRef(e) => {
                // `&expr` outside array literals binds the target cell.
                let c = self.eval_cell(e)?;
                let v = c.borrow().clone();
                Ok(v)
            }
            Expr::List(_) => self.fail(PhpError::fatal("Cannot use list() as value", 0)),
            Expr::Assign {
                target,
                op,
                value,
                line,
            } => self.assign(target, op, value, *line),
            Expr::Binary { op, l, r } => self.binary(op, l, r),
            Expr::Unary { op, e } => self.unary(op, e),
            Expr::Ternary { c, t, f } => {
                let c = self.eval(c)?;
                if c.is_truthy() {
                    match t {
                        Some(t) => self.eval(t),
                        None => Ok(c),
                    }
                } else {
                    self.eval(f)
                }
            }
            Expr::Call {
                name,
                args,
                site,
                callee,
            } => self.scoped_send(|s| s.call(name, args, Some(*site), Some(*callee))),
            Expr::Fcc(inner) => self.fcc(inner),
            Expr::Unpack(_) | Expr::FccMark => {
                self.fail(PhpError::fatal("argument unpacking/FCC outside of call", 0))
            }
            Expr::Index { e, i } => self.index_read(e, i.as_deref()),
            Expr::PreInc(t) => self.incdec(t, 1, false),
            Expr::PreDec(t) => self.incdec(t, -1, false),
            Expr::PostInc(t) => self.incdec(t, 1, true),
            Expr::PostDec(t) => self.incdec(t, -1, true),
            Expr::Isset(args) => {
                self.isset_quiet += 1;
                let mut ok = true;
                for a in args {
                    match self.isset_val_mode(a, 0) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            ok = false;
                            break;
                        }
                        Err(e) => {
                            self.isset_quiet -= 1;
                            return Err(e);
                        }
                    }
                }
                self.isset_quiet -= 1;
                Ok(Value::Bool(ok))
            }
            Expr::Empty(e) => {
                self.isset_quiet += 1;
                let v = self.isset_val_mode(e, 1);
                self.isset_quiet -= 1;
                match v {
                    Ok(Some(v)) => Ok(Value::Bool(!v.is_truthy())),
                    Ok(None) => Ok(Value::Bool(true)),
                    Err(e) => Err(e),
                }
            }
            Expr::Print(e) => {
                let v = self.eval(e)?;
                let s = self.conv_str(&v)?;
                self.emit(&s);
                Ok(Value::Int(1))
            }
            Expr::Yield { key, val } => {
                // `function &gen()` yields the value's own cell so
                // `foreach ($gen as &$v)` writes back into it
                // (typed_properties_033/034).
                let by_ref = self.stack.last().map(|f| f.ret_by_ref).unwrap_or(false);
                let vc = match val {
                    Some(e) if by_ref => self.eval_cell(e)?,
                    Some(e) => cell(self.eval(e)?),
                    None => cell(Value::Null),
                };
                let k = match key {
                    Some(e) => self.eval(e)?,
                    None => {
                        if self.gen_sink.is_none() {
                            return self.fail(PhpError::fatal(
                                "The \"yield\" expression can only be used inside a function",
                                self.cur_line,
                            ));
                        }
                        let i = self.gen_auto;
                        self.gen_auto += 1;
                        Value::Int(i)
                    }
                };
                match &self.gen_sink {
                    Some(sink) => {
                        sink.borrow_mut().push((k, vc));
                        let idx = sink.borrow().len() - 1;
                        // A yield inside a `finally` region marks the
                        // force-close fatal — destruction replay
                        // raises 'Cannot yield from finally in a
                        // force-closed generator' here.
                        if self.gen_fin_depth > 0 {
                            if let Some(q) = &self.gen_fin_q {
                                q.borrow_mut().yields.push((idx, self.cur_line));
                            }
                        }
                        // A `Generator->throw()` queued for this yield
                        // raises the throwable as the expression's
                        // result — the body's own try/catch/finally
                        // performs the unwind.
                        if self.gen_throws.front().is_some_and(|(i, _)| *i == idx) {
                            let (_, v) = self.gen_throws.pop_front().unwrap();
                            self.gen_throws_fired.push(idx);
                            return Err(self.throw(v));
                        }
                        // Queued send()s are keyed by yield index —
                        // replayed yields before the suspended one
                        // take NULL, not the incoming send.
                        Ok(if self.gen_sends.front().is_some_and(|(i, _)| *i == idx) {
                            self.gen_sends.pop_front().unwrap().1
                        } else {
                            Value::Null
                        })
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::YieldFrom(e) => {
                let v = self.eval(e)?;
                let sink = self.gen_sink.clone();
                match sink {
                    Some(sink) => {
                        // `yield from` splices the inner keys verbatim —
                        // duplicates and all — and doesn't touch the
                        // keyless auto counter. The materialization
                        // drives inner iterators under the foreach
                        // marking so an inner-gen death keeps its own
                        // trace; items gathered before it still reach
                        // the sink, and the death becomes this body's
                        // own (deferred-raising) death.
                        self.iter_calls += 1;
                        let base = sink.borrow().len();
                        // The inner drain's flushed stream bytes retag
                        // into THIS gen's deferred queue at `base` —
                        // a live emit would echo inner output before
                        // the consumer reached it (yield-from order).
                        let saved_cbase = self.gen_collect_base.replace(base);
                        let saved_seen = std::mem::take(&mut self.gen_collect_seen);
                        let saved_crun = self
                            .gen_collect_run
                            .replace(self.gen_run_state.clone().unwrap());
                        // The drain's drives aren't foreach iter_calls
                        // — the delegate's suspended frame cites the
                        // yield-from's own line, not the loop header.
                        let saved_isite = self.gen_iter_site.take();
                        let (items, death) = self.yield_from_collect(&v);
                        self.gen_iter_site = saved_isite;
                        self.gen_collect_base = saved_cbase;
                        self.gen_collect_seen = saved_seen;
                        self.gen_collect_run = saved_crun;
                        self.iter_calls -= 1;
                        let inner_len = items.len();
                        // Record the delegation window: consumer
                        // send()/throw() landing inside it routes into
                        // the delegate's own queues (Zend's chain is
                        // live), and the delegate's return value is
                        // the yield-from expression's own value.
                        let inner_ret = if let Value::Object(o) = &v {
                            match &o.borrow().internal {
                                Some(crate::value::ObjectInternal::Generator(ist)) => {
                                    if let Some(run) = &self.gen_run_state {
                                        let mut r = run.borrow_mut();
                                        r.delegate_gens.push((base, inner_len));
                                        // Live delegate journal —
                                        // killing this incarnation
                                        // displaces its delegates.
                                        r.fin_q
                                            .borrow_mut()
                                            .delegate_fins
                                            .push((base, ist.borrow().fin_q.clone()));
                                        // The eager drain above drove
                                        // the delegate's own cursor to
                                        // its production count — the
                                        // consumer-facing position is
                                        // zero until the outer's
                                        // cursor reaches `base`.
                                        ist.borrow().fin_q.borrow_mut().set_vis_tree(0);
                                    }
                                    ist.borrow().return_val.clone()
                                }
                                _ => Value::Null,
                            }
                        } else {
                            Value::Null
                        };
                        sink.borrow_mut().extend(items);
                        // A gen suspended inside `yield from` shares
                        // the OUTER gen's destruction: its destruction
                        // journal (snapshotted at its start, before
                        // the drain pruned it) merges into the
                        // parent's — tags retagged into the parent's
                        // item space and the splice range recorded, so
                        // the replay fires only while the consumer's
                        // cursor is inside the delegate's stream (Zend
                        // force-closes just the actually-suspended
                        // delegation chain — a delegate never reached
                        // replays nothing).
                        if let Some(mut inner_fin) = self.gen_yield_from_fin.take() {
                            if let Some(q) = &self.gen_fin_q {
                                inner_fin.retag(base);
                                q.borrow_mut().delegates.push(crate::value::FinDelegate {
                                    entry: base,
                                    span: inner_len,
                                    fin: inner_fin,
                                });
                            }
                        }
                        match death {
                            Some(e) => Err(e),
                            None => Ok(inner_ret),
                        }
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield from\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::Exit(arg) => {
                let code = if let Some(a) = arg {
                    match self.eval(a)? {
                        Value::Int(i) => i as i32,
                        Value::Str(s) => {
                            self.emit_bytes(&s);
                            0
                        }
                        _ => 0,
                    }
                } else {
                    0
                };
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Fatal,
                    message: format!("\u{1}exit:{}", code),
                    line: 0,
                })
            }
            Expr::Include { kind, e } => self.include(*kind, e),
            Expr::Throw(e) => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(_) => Err(self.throw(v)),
                    _ => {
                        // PHP 8: "Can only throw objects"
                        let v = self.exception("Error", "Can only throw objects");
                        self.pending_exception = Some(v);
                        Err(PhpError {
                            trace: None,
                            thrown_line: None,
                            display_msg: None,
                            kind: ErrorKind::Throw,
                            message: "throw".into(),
                            line: 0,
                        })
                    }
                }
            }
            Expr::Match { subject, arms, .. } => {
                // zend compiles the subject once, then const-scans the
                // conds L→R (`can_match_use_jumptable` — the same scan
                // that picks the MATCH jumptable): each foldable cond
                // rewrites to a zval stamped with CG(zend_lineno) —
                // the subject's compiled end line — and the scan stops
                // at the first cond that can't fold to an int/string
                // zval. Everything qualifies and ≥2 conds → a single
                // MATCH op binds the subject at its own line. Otherwise
                // a CV subject binds inside each CASE op — warning once
                // per evaluated compare at the cond's scan line.
                let num_conds: usize = arms.iter().map(|a| a.conds.len()).sum();
                let subj_u = Self::unmark_rhs(subject);
                let cv_subject = Self::is_cv(subj_u);
                let cv_site = Self::cv_site_of(subject, subj_u).unwrap_or(self.cur_line);
                // CG(zend_lineno) after the subject's compile — where
                // every folded cond's stamped zval sites.
                let subj_site = if cv_subject {
                    cv_site
                } else {
                    match subj_u {
                        // TYPE_CHECK sites on a non-CV subject: binary
                        // ops (folded or not) site at their end line —
                        // the const-fold stamp never shifts one past.
                        Expr::Binary { .. } => {
                            Self::inner_end_line(subject).unwrap_or(self.cur_line)
                        }
                        _ => Self::marked_line(subject).unwrap_or(self.cur_line),
                    }
                };
                // The const scan's per-cond compare line and operand
                // — only entries for the scanned prefix exist; later
                // conds fall back to their own end line. The operand
                // differs from the cond only when zend's scan spliced
                // a `??`/`?:` survivor into the slot.
                let mut sites: Vec<(usize, &Expr)> = Vec::with_capacity(num_conds);
                let mut jumptable = num_conds >= 2;
                'scan: for arm in arms {
                    for c in &arm.conds {
                        match self.cond_fold(c) {
                            CondFold::Literal(e, v) => {
                                sites.push((
                                    Self::marked_line(e)
                                        .or_else(|| Self::inner_end_line(e))
                                        .unwrap_or(subj_site),
                                    c,
                                ));
                                if !matches!(v, Value::Int(_) | Value::Str(_)) {
                                    jumptable = false;
                                    break 'scan;
                                }
                            }
                            CondFold::Folded(v) => {
                                sites.push((subj_site, c));
                                if !matches!(v, Value::Int(_) | Value::Str(_)) {
                                    jumptable = false;
                                    break 'scan;
                                }
                            }
                            CondFold::Unfolded(u) => {
                                sites.push((
                                    self.unfold_tail_site(u, subj_site)
                                        .or_else(|| Self::inner_end_line(u))
                                        .or_else(|| Self::marked_line(u))
                                        .or_else(|| Self::inner_end_line(c))
                                        .or_else(|| Self::marked_line(c))
                                        .unwrap_or(self.cur_line),
                                    u,
                                ));
                                jumptable = false;
                                break 'scan;
                            }
                        }
                    }
                }
                let sv: Option<Value> = if jumptable {
                    Some(if cv_subject {
                        self.eval_cv_at(subj_u, cv_site)?
                    } else {
                        self.eval(subject)?
                    })
                } else if cv_subject {
                    None
                } else {
                    Some(self.eval(subject)?)
                };
                let mut default: Option<&Expr> = None;
                let mut last_cmp = self.cur_line;
                let mut ci = 0;
                for arm in arms {
                    if arm.conds.is_empty() {
                        default = Some(&arm.result);
                        continue;
                    }
                    for c in &arm.conds {
                        // Folded conds' compares site at the subject's
                        // compiled end (their stamped zval); unfolded
                        // conds at their own end line. MATCH_ERROR
                        // sites at the last compare op's lineno.
                        let scanned = sites.get(ci).is_some();
                        let (cl, operand) = sites.get(ci).copied().unwrap_or_else(|| {
                            (
                                Self::inner_end_line(c)
                                    .or_else(|| Self::marked_line(c))
                                    .unwrap_or(self.cur_line),
                                c,
                            )
                        });
                        ci += 1;
                        last_cmp = cl;
                        self.cur_line = cl;
                        // IS_IDENTICAL binds op1 (subject) then op2
                        // (cond) inside the compare op: a CV cond's
                        // read happens there too, so its warn follows
                        // the subject's. Any other cond emits its own
                        // ops first — its warnings precede the bind.
                        let cu = Self::unmark_rhs(operand);
                        // The const scan ran on this cond: its folded
                        // leaves carry the subject-end stamp, so ops
                        // inside the operand site by the tail rule.
                        let prev_stamp = self.scan_stamp.take();
                        if scanned {
                            self.scan_stamp = Some(subj_site);
                        }
                        let pair = if sv.is_none() && Self::is_cv(cu) {
                            self.eval_cv_at(subj_u, cl)
                                .and_then(|svv| self.eval(operand).map(|cv| (cv, svv)))
                        } else {
                            self.eval(operand).and_then(|cv| {
                                // A deferred CV subject is read inside
                                // THIS compare op — a fresh read each
                                // time (zend's CV operand binds per op:
                                // warns per cond line).
                                let svv = match &sv {
                                    Some(sv) => Ok(sv.clone()),
                                    None => self.eval_cv_at(subj_u, cl),
                                };
                                svv.map(|svv| (cv, svv))
                            })
                        };
                        self.scan_stamp = prev_stamp;
                        let (cv, svv) = pair?;
                        crate::value::clear_cmp_depth_err();
                        // ZEND_CASE_STRICT (TMP|VAR subjects) is
                        // noncommutative — subject stays left; CONST|CV
                        // subjects emit IS_IDENTICAL which pass_two
                        // commutative-swaps when the arm ranks higher.
                        let r = compare_operand_rank(subject);
                        let (x, y) = if (r & 6) == 0 && r < compare_operand_rank(c) {
                            (&cv, &svv)
                        } else {
                            (&svv, &cv)
                        };
                        let hit = identical(x, y);
                        self.emit_cmp_notices()?;
                        if crate::value::cmp_depth_err() {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Nesting level too deep - recursive dependency?",
                                self.cur_line,
                            ));
                        }
                        if hit {
                            return self.eval(&arm.result);
                        }
                    }
                }
                if let Some(d) = default {
                    self.eval(d)
                } else {
                    // MATCH_ERROR reads the subject silently — for a
                    // deferred CV subject that's the var's value now
                    // (a cond's side effects may have set it).
                    let sv = match sv {
                        Some(v) => v,
                        None => {
                            let n = match subj_u {
                                Expr::Var(n) => Some(n.clone()),
                                Expr::VarVar(inner, _) if is_compile_const(inner) => {
                                    let v = self.eval(inner)?;
                                    Some(self.conv_str(&v)?)
                                }
                                _ => None,
                            };
                            n.and_then(|n| self.var_cell_opt(&n))
                                .map(|c| c.borrow().clone())
                                .unwrap_or(Value::Null)
                        }
                    };
                    // MATCH_ERROR sites at the subject's compiled end
                    // line under the jumptable (the MATCH op's own
                    // lineno); non-jumptable keeps the last compare's
                    // line.
                    let l = if jumptable {
                        Self::inner_end_line(subj_u)
                            .or_else(|| Self::marked_line(subject))
                            .unwrap_or(cv_site)
                    } else {
                        last_cmp
                    };
                    self.cur_line = l;
                    self.send_line = Some(l);
                    let e = self.exception(
                        "UnhandledMatchError",
                        &format!("Unhandled match case {}", match_case_desc(&sv)),
                    );
                    self.pending_exception = Some(e);
                    Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Throw,
                        message: "match".into(),
                        line: 0,
                    })
                }
            }
            Expr::Closure(c) => {
                // PHP 8.5 closures in constant expressions must be
                // static and can't import use() vars; `fn` arrow bodies
                // are never const-expr material (closure_const_expr/*).
                if self.in_const_expr > 0 {
                    if c.arrow {
                        return self.fail(PhpError::compile_fatal(
                            "Constant expression contains invalid operations",
                            c.decl.line,
                        ));
                    }
                    if !c.is_static {
                        return self.fail(PhpError::compile_fatal(
                            "Closures in constant expressions must be static",
                            c.decl.line,
                        ));
                    }
                    if !c.uses.is_empty() {
                        return self.fail(PhpError::compile_fatal(
                            "Cannot use(...) variables in constant expression",
                            c.decl.line,
                        ));
                    }
                }
                // Compile-time param checks for the closure's decl —
                // `{closure:FILE:LINE}():` names it (namespaces/ns_073).
                let cfile = if c.decl.file.is_empty() {
                    self.cur_file.clone()
                } else {
                    c.decl.file.clone()
                };
                // PHP 8.5 names a closure after its enclosing scope:
                // `{closure:Class::m():L}` inside a method,
                // `{closure:fn():L}` inside a function, `{closure:FILE:L}`
                // at top level, and `{closure:{closure:...}:L}` when
                // nested (iterable_003, closure_065). Class-init
                // initializers (prop/const/static-prop defaults) have no
                // enclosing function — file-based name regardless of the
                // runtime caller's frame. Param-default evals also set
                // const_self (for `self::` binds) but DO name the
                // enclosing callee — param_bind_ctx records the ambient
                // class-init level and only counts while a nested
                // initializer hasn't bumped it.
                let enclosing = if self.const_self.is_some()
                    && self.param_bind_ctx != Some(self.class_const_ctx)
                {
                    String::new()
                } else {
                    self.stack
                        .last()
                        .map(|f| {
                            if f.fn_name.is_empty() || f.fn_name == "{main}" {
                                String::new()
                            } else if f.fn_name.starts_with("{closure:") {
                                f.fn_name.clone()
                            } else {
                                match f.trait_origin.clone().or_else(|| {
                                    f.decl_class
                                        .as_ref()
                                        .or(f.scope_class.as_ref())
                                        .map(|c| c.name().to_string())
                                }) {
                                    Some(o) => format!("{}::{}", o, f.fn_name),
                                    None => f.fn_name.clone(),
                                }
                            }
                        })
                        .unwrap_or_default()
                };
                let fname = if enclosing.is_empty() {
                    format!("{{closure:{}:{}}}", cfile, c.decl.line)
                } else if enclosing.starts_with('{') {
                    format!("{{closure:{}:{}}}", enclosing, c.decl.line)
                } else {
                    format!("{{closure:{}():{}}}", enclosing, c.decl.line)
                };
                // The closure's decl.file = the file currently executing
                // — __FILE__/__DIR__ inside it must resolve to where it
                // was defined, not where it is later invoked (autoloaders).
                let mut decl = c.decl.clone();
                if decl.file.is_empty() {
                    decl.file = cfile;
                }
                decl.name = fname.clone();
                // A closure declared lexically inside a trait method
                // keeps the trait as its __TRAIT__ origin; the decl is
                // cloned per instance so the creating context stamps it
                // here (closure_trait_const).
                if decl.decl_in.is_none() {
                    decl.decl_in = self
                        .stack
                        .last()
                        .and_then(|f| f.trait_origin.clone())
                        .or_else(|| {
                            self.const_self
                                .as_ref()
                                .filter(|c| c.decl.kind == crate::ast::ClassKind::Trait)
                                .map(|c| c.name().to_string())
                        });
                }
                if let Err(e) = self.decl_type_checks(&fname, &decl, None) {
                    return Err(self.decl_fatal_ctx(e));
                }
                let mut captures = Vec::new();
                if c.arrow {
                    // `fn` captures whole scope by value — the zval
                    // share keeps the same array until a write, when
                    // cow_split separates it (cyclic self-refs stay
                    // intact; writes can't leak out).
                    let f = self.stack.last().unwrap_or(&self.globals);
                    for (n, cellv) in f.vars.iter() {
                        captures.push((n.clone(), cell(cellv.borrow().clone()), false));
                    }
                } else {
                    for (n, by_ref) in &c.uses {
                        let cap = if *by_ref {
                            // `use (&$x)` promotes the imported var to
                            // an IS_REFERENCE cell (bug52193's `&` in
                            // var_dump of the captures table).
                            let cellv = self.var_cell(n);
                            self.mark_ref(&cellv);
                            cellv
                        } else {
                            match self.var_cell_opt(n) {
                                Some(c) => cell(c.borrow().clone()),
                                // `use ($x)` on an undefined var warns
                                // and captures null; `use (&$x)` binds
                                // silently (closure_027).
                                None => {
                                    self.warn(&format!("Undefined variable ${}", n))?;
                                    cell(Value::Null)
                                }
                            }
                        };
                        captures.push((n.clone(), cap, *by_ref));
                    }
                }
                // `static function` never binds $this; `static::`
                // keeps the creating frame's late-bound class
                // (closure_049-052). Class-init initializers bind the
                // DECLARING class's scope instead (const_self), so a
                // prop-default closure can reach private props
                // (property_initializer_scope_*).
                let is_static = c.is_static;
                // Zend seeds a closure's static-variable table at
                // creation — each instance owns its own copy of the
                // compiled defaults (closure_const_expr/static_variable).
                let mut sv = Vec::new();
                closure_static_vars(&decl.body, &mut sv);
                let mut seed_frame = Frame::new(fname.clone());
                seed_frame.fn_line = decl.line;
                seed_frame.file = decl.file.clone();
                seed_frame.ns = decl.ns.clone();
                seed_frame.trait_origin = decl.decl_in.clone();
                let callable = self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    kind: CallableKind::Closure(Rc::new(decl)),
                    captures,
                    this_obj: if is_static {
                        None
                    } else {
                        self.stack.last().and_then(|f| f.this_obj.clone())
                    },
                    scope_class: self
                        .const_self
                        .clone()
                        .or_else(|| self.stack.last().and_then(|f| f.scope_class.clone())),
                    called_class: self
                        .const_self
                        .clone()
                        .or_else(|| self.stack.last().and_then(|f| f.called_class.clone())),
                    is_static,
                });
                if !sv.is_empty() {
                    let key = format!("{}\u{0}c{}", fname, callable.id.get());
                    let mut table = crate::value::FxMap::default();
                    // The seeded defaults compile against the CLOSURE's
                    // own scope: __FUNCTION__/__METHOD__ name it and
                    // __CLASS__ sees its bound scope — evaluating in
                    // the enclosing frame would stamp the caller's
                    // context in permanently.
                    seed_frame.scope_class = callable.scope_class.clone();
                    seed_frame.called_class = callable.called_class.clone();
                    let saved_line = self.cur_line;
                    self.stack.push(seed_frame);
                    for (n, d, sline) in sv {
                        // Only literal-only defaults are bound at
                        // creation — consts, `new`, calls and anything
                        // needing a runtime env stay NULL until the
                        // `static` statement first executes
                        // (closure_const_expr/bug79778).
                        let Some(e) = d else { continue };
                        if !literal_static_init(&e, &self.engine_consts) {
                            continue;
                        }
                        // __LINE__ resolves to the `static` statement's
                        // line inside the body (probe_sv_line).
                        self.cur_line = sline;
                        let Ok(v) = self.eval_const(&e) else {
                            continue;
                        };
                        table.insert(n, cell(v));
                    }
                    self.stack_pop();
                    self.cur_line = saved_line;
                    // Wholesale replace: a recycled handle id could
                    // otherwise expose a dead closure's stale table
                    // to this fresh instance.
                    self.statics.insert(key, table);
                }
                Ok(Value::Callable(callable))
            }
            Expr::New { class, args, site } => self.scoped_send(|s| {
                // Autoload inside class_name_of may push frames — they
                // site at this `new` (the class expr's first token).
                s.send_line = Some(*site);
                let lit = Self::is_lit_class_ref(class);
                let name = s.class_name_of(class)?;
                // `new self`/`static`/`parent` outside class scope is the
                // no-scope Error; literal `parent` inside a parentless
                // class is the catchable no-parent Error (traits/
                // closures defer here — p10new/m45 vs oracle).
                s.scope_kw_err(&name, lit)?;
                let cls = s.classes.get(&name.to_lowercase()).cloned();
                let found = cls
                    .as_ref()
                    .and_then(|c| s.find_method_in(c, "__construct"));
                let params = found
                    .as_ref()
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                // ctx is the ctor's diagnostic name — arg_cells wraps
                // it as "{ctx}()" for arg errors. Oracle prints the
                // ctor's DECLARING class (`P::__construct():` when C
                // inherits P's ctor), canonical declared case (not the
                // source spelling), and `X@anonymous():` for anon
                // classes — anon names carry a `\0FILE:LINE$seq`
                // suffix that must be truncated. A class with no ctor
                // has no arg diagnostics to name, so the fallback is
                // only for the not-found/unresolved edge.
                let owner = found
                    .map(|(_, o)| o.decl.name.clone())
                    .or_else(|| cls.map(|c| c.decl.name.clone()))
                    .unwrap_or_else(|| name.clone());
                let base = owner.split('\0').next().unwrap_or(&owner).to_string();
                let ctx = if base.contains("@anonymous") {
                    base
                } else {
                    format!("{base}::__construct")
                };
                let argvals = s.arg_cells(args, &params, &ctx, false, Some(*site), true)?;
                s.new_instance(&name, argvals)
            }),
            Expr::Prop {
                obj,
                name,
                nullsafe,
                site,
            } => self.prop_read(obj, name, *nullsafe, *site),
            Expr::MethodCall {
                obj,
                name,
                args,
                nullsafe,
                site,
            } => self.scoped_send(|s| s.method_call(obj, name, args, *nullsafe, Some(*site))),
            Expr::Paren(e) => self.eval(e),
            Expr::StaticProp { class, name } => self.static_prop_read(class, name),
            Expr::StaticCall {
                class,
                name,
                args,
                site,
            } => {
                // `parent::$prop::get()` — parent property hook call.
                if let Expr::StaticProp {
                    class: pc,
                    name: PropName::Name(pn),
                } = class.as_ref()
                {
                    if let Expr::Const(n) = pc.as_ref() {
                        if n.eq_ignore_ascii_case("parent")
                            && (name.eq_ignore_ascii_case("get")
                                || name.eq_ignore_ascii_case("set"))
                        {
                            return self.scoped_send(|s| {
                                s.hook_parent_call(
                                    pn,
                                    name.eq_ignore_ascii_case("get"),
                                    args,
                                    Some(*site),
                                )
                            });
                        }
                    }
                }
                self.scoped_send(|s| s.static_call(class, name, args, Some(*site)))
            }
            Expr::StaticCallDyn {
                class,
                name,
                args,
                site,
            } => self.scoped_send(|s| {
                // Class-name resolution may autoload — site those
                // frames at this call.
                s.send_line = Some(*site);
                // `C::$var(...)`: class resolves first, then the name.
                // Non-string names are a catchable Error
                // (call_static_004).
                let cls = s.class_of(class)?;
                let nv = s.eval(name)?;
                let n = match nv {
                    Value::Str(v) => Self::nul_trunc(&crate::value::lossy(&v)),
                    _ => {
                        return s.fail(PhpError::uncaught(
                            "Error",
                            "Method name must be a string",
                            0,
                        ))
                    }
                };
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                let argvals = s.arg_cells(
                    args,
                    &[],
                    &format!("{}::{{closure}}", cls.name()),
                    false,
                    Some(*site),
                    false,
                )?;
                s.static_invoke_vis(cls, &n, argvals, None, fwd)
            }),
            Expr::ClassConst { class, name } => self.class_const(class, name),
            Expr::Clone(e) => {
                let v = self.eval(e)?;
                self.builtin_clone(&v, None)
            }
            Expr::Cast { kind, e } => {
                let v = self.eval(e)?;
                self.cast(*kind, v)
            }
            Expr::Instanceof { obj, class } => {
                let v = self.eval(obj)?;
                let cname = self.class_name_of(class)?;
                match v {
                    Value::Object(o) => {
                        let cls = o.borrow().class.clone();
                        Ok(Value::Bool(self.is_a(&cls, &cname)))
                    }
                    // A closure literal IS a Closure object.
                    Value::Callable(_) => Ok(Value::Bool(cname.eq_ignore_ascii_case("closure"))),
                    _ => Ok(Value::Bool(false)),
                }
            }
            Expr::AnonClass(decl) => Ok(Value::str(self.anon_class_name(decl)?)),
        }
    }

    fn magic(&mut self, m: MagicConst) -> Value {
        // __FILE__/__DIR__ resolve against the DECLARING file of the
        // code that runs them — a closure defined in vendor/autoload.php
        // sees that file's dir even when invoked from elsewhere
        // (composer-style PSR-4 autoloaders depend on this).
        let decl_file = self
            .decl_file_ctx
            .clone()
            .or_else(|| {
                self.stack
                    .last()
                    .map(|f| f.file.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.cur_file.clone());
        match m {
            MagicConst::Line => Value::Int(self.cur_line as i64),
            MagicConst::File => Value::str(decl_file.clone()),
            MagicConst::Dir => Value::str({
                let d = std::path::Path::new(&decl_file)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                // A pseudo-path with no dir part ("Command line code",
                // "Standard input code", a top-level eval) resolves to
                // the process cwd, like Zend's `php -r`.
                if d.is_empty() {
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                } else {
                    d
                }
            }),
            MagicConst::Function => Value::str(
                self.stack
                    .last()
                    .map(|f| f.fn_name.clone())
                    .unwrap_or_default(),
            ),
            MagicConst::Method => {
                let f = self.stack.last();
                match f {
                    // Hook frame: owner is the declaring class/trait —
                    // `T::$prop::get` for trait-origin hooks (not the
                    // consuming class).
                    Some(f) if f.hook_prop.is_some() => {
                        let (_, _, _, owner) = f.hook_prop.as_ref().unwrap();
                        Value::str(format!("{}::{}", owner, f.fn_name))
                    }
                    // Inside a closure __METHOD__ is the closure's Zend
                    // name (`{closure:C::m():L}` — closure_033).
                    Some(f) if f.fn_name.starts_with("{closure:") => Value::str(f.fn_name.clone()),
                    Some(f) => {
                        // `T::m` when the method was merged from trait T
                        // (`__METHOD__` names the trait; `__CLASS__`
                        // names the consuming class).
                        let owner = f
                            .trait_origin
                            .clone()
                            .or_else(|| f.scope_class.as_ref().map(|c| c.name().to_string()));
                        match owner {
                            Some(o) => Value::str(format!("{}::{}", o, f.fn_name)),
                            None => Value::str(""),
                        }
                    }
                    None => Value::str(""),
                }
            }
            MagicConst::Class => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .or_else(|| self.const_self.as_ref().map(|c| c.name().to_string()))
                    .unwrap_or_default(),
            ),
            MagicConst::Namespace => Value::str(self.caller_ns()),
            // `__PROPERTY__` inside a hook names its prop; anywhere else
            // (methods, closures nested in a hook, top level) it is "".
            MagicConst::Property => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.hook_prop.as_ref().map(|(_, pn, _, _)| pn.clone()))
                    .unwrap_or_default(),
            ),
            // `__TRAIT__` names the trait a method was merged from;
            // "" inside class-defined methods and at top level.
            MagicConst::Trait => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.trait_origin.clone())
                    .unwrap_or_default(),
            ),
        }
    }

    /// Whether the expr is "set" — for isset()/empty() without warnings.
    /// `isset()` semantics returning the read value — `Some(v)` when the
    /// operand exists and isn't null. `??`/`empty` consume the value
    /// directly so calls and prop-getters evaluate exactly once.
    /// isset/empty/?? property semantics (mode):
    ///  0 = isset()  — last segment's __isset answers directly, no
    ///                 fetch; absent prop without __isset is just not
    ///                 set — __get never fires (bug44899).
    ///  1 = empty()  — __isset gates; truthy result then reads via
    ///                 __get for the falsy check; absent prop without
    ///                 __isset is empty, still no __get.
    ///  2 = ?? / intermediate segment — __isset gates then __get
    ///                 fetches; without __isset the read runs __get
    ///                 directly (bug71359).
    fn isset_val_mode(&mut self, e: &Expr, mode: u8) -> Result<Option<Value>, PhpError> {
        match e {
            Expr::Binary {
                op: "argline", r, ..
            } => self.isset_val_mode(r, mode),
            Expr::Var(n) => Ok(match self.var_cell_opt(n) {
                Some(c) => match &*c.borrow() {
                    Value::Null => None,
                    v => Some(v.clone()),
                },
                None => None,
            }),
            Expr::VarVar(inner, _) => {
                // isset/empty suppress the varname's CV read too —
                // `isset($$x)` is fully silent — but only a bare-CV
                // name: `isset(${$y . 'z'})` warns $y. Under `??` and
                // as an index/prop base the inner warns normally.
                let n = if mode < 2 {
                    match Self::unmark_rhs(inner) {
                        Expr::Var(n2) => match self.var_cell_opt(n2) {
                            Some(c) => c.borrow().clone(),
                            // The name can't be formed — the var doesn't
                            // exist; silent false.
                            None => return Ok(None),
                        },
                        _ => self.eval(inner)?,
                    }
                } else {
                    self.eval(inner)?
                };
                let name = self.conv_str(&n)?;
                Ok(match self.var_cell_opt(&name) {
                    Some(c) => match &*c.borrow() {
                        Value::Null => None,
                        v => Some(v.clone()),
                    },
                    None => None,
                })
            }
            Expr::Index { e, i } => {
                // `isset($this->uninitTyped['k'])` and `$x ?? y` must not
                // throw on uninitialized typed properties. The base
                // chains through isset semantics — absent segments
                // short-circuit without __get (bug71359).
                //
                // zend emits the dim chain's expression keys eagerly in
                // source order (calls run once, before any CV reads),
                // while a CV key reads inside its own FETCH op — a
                // dead chain still reads later CV keys (undef warn)
                // without running conversions on the dead container
                // (p9b/p9c).
                let mut keys: Vec<Option<&Expr>> = vec![i.as_deref()];
                let mut base_e: &Expr = e;
                while let Expr::Index { e: b, i: ik } = base_e {
                    keys.push(ik.as_deref());
                    base_e = b;
                }
                keys.reverse();
                let mut evald: Vec<Option<Option<Value>>> = Vec::with_capacity(keys.len());
                for k in &keys {
                    let kk = k.map(Self::unmark_rhs);
                    match kk {
                        Some(Expr::Var(_)) => evald.push(None),
                        Some(kk) => {
                            let q = std::mem::replace(&mut self.isset_quiet, 0);
                            let r = self.eval(kk);
                            self.isset_quiet = q;
                            evald.push(Some(Some(r?)));
                        }
                        None => evald.push(Some(None)),
                    }
                }
                self.isset_quiet += 1;
                let base = self.isset_val_mode(base_e, 2);
                self.isset_quiet -= 1;
                let mut cur = match base {
                    Ok(v) => v,
                    Err(err) if matches!(err.kind, ErrorKind::Throw) => {
                        if err
                            .message
                            .ends_with("must not be accessed before initialization")
                        {
                            None
                        } else {
                            return Err(err);
                        }
                    }
                    Err(_) => None,
                };
                for (idx, k) in keys.iter().enumerate() {
                    let key = match &evald[idx] {
                        Some(Some(v)) => v.clone(),
                        Some(None) => return Ok(None),
                        None => {
                            // CV key — reads fresh (undef warn
                            // surfaces) even on a dead chain.
                            let q = std::mem::replace(&mut self.isset_quiet, 0);
                            let r = self.eval(k.unwrap());
                            self.isset_quiet = q;
                            r?
                        }
                    };
                    if let Some(b) = &cur {
                        cur = self.isset_dim_fetch(b.clone(), key, mode, idx == keys.len() - 1)?;
                    }
                }
                Ok(cur)
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
                site: _,
            } => {
                // Missing/inaccessible props consult __isset first
                // (bug63462, bug44899); a re-entrant isset inside
                // __isset hits real storage only. The base evaluates
                // through isset semantics too: each segment of a chain
                // tests via __isset, then fetches via __get only when
                // set — never triggering __get on an absent segment
                // (bug71359).
                let pn = self.prop_name(name)?;
                let ov = match self.isset_val_mode(obj, 2)? {
                    Some(v) => v,
                    None => return Ok(None),
                };
                if let Value::Callable(_) = &ov {
                    // isset/empty/?? on a Closure prop are silent —
                    // the 'Undefined property: Closure::$x' warn only
                    // fires on a real read (bug50146).
                    self.check_prop_name(&pn)?;
                    return Ok(None);
                }
                if let Value::Object(o) = &ov {
                    let cls = o.borrow().class.clone();
                    // A declared prop checks its real slot unless it
                    // was unset() — only then does __isset fire
                    // (typed_properties_magic_set vs bug63462).
                    let was_unset = {
                        let ob = o.borrow();
                        ob.unset_props.contains(&pn)
                            || ob
                                .unset_props
                                .iter()
                                .any(|k| k.ends_with(&format!("\0{}", pn)))
                    };
                    let declared_live = self.decl_prop(o, &pn).is_some()
                        && !was_unset
                        && self.prop_visible(&cls, &pn);
                    let missing = match self.obj_prop_key(o, &pn) {
                        Some(_) => !self.prop_visible(&cls, &pn),
                        None => true,
                    } && !declared_live;
                    if missing {
                        // ARRAY_AS_PROPS: undeclared props resolve
                        // against storage — a non-null value ⇒ set.
                        if self.aap_active(o) {
                            let arr = self.ao_state(o).0;
                            let v = arr.borrow().get(&ArrKey::Str(Rc::from(pn.as_str())));
                            return match v {
                                Some(v) if !matches!(v, Value::Null) => {
                                    Ok(Some(if mode == 0 { Value::Bool(true) } else { v }))
                                }
                                _ => Ok(None),
                            };
                        }
                        if self.find_method_in(&cls, "__isset").is_some() {
                            let gkey = (Rc::as_ptr(o) as usize, 2u8, pn.clone());
                            if self.magic_guards.insert(gkey.clone()) {
                                let res = self.method_invoke(
                                    o.clone(),
                                    "__isset",
                                    CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                );
                                self.magic_guards.remove(&gkey);
                                if !res?.is_truthy() {
                                    return Ok(None);
                                }
                                // isset() takes __isset's answer — no
                                // fetch (bug44899). empty()/?? then
                                // read through __get with the SAME
                                // bound name (bug75420).
                                if mode == 0 {
                                    return Ok(Some(Value::Bool(true)));
                                }
                                self.isset_quiet += 1;
                                let v = self.prop_read_value(ov.clone(), &pn, false);
                                self.isset_quiet -= 1;
                                return match v {
                                    Ok(v) => Ok(if matches!(v, Value::Null) {
                                        None
                                    } else {
                                        Some(v)
                                    }),
                                    Err(e)
                                        if matches!(e.kind, ErrorKind::Throw)
                                            && e.message.ends_with(
                                                "must not be accessed before initialization",
                                            ) =>
                                    {
                                        Ok(None)
                                    }
                                    Err(e) if matches!(e.kind, ErrorKind::Throw) => Err(e),
                                    Err(_) => Ok(None),
                                };
                            } else {
                                return Ok(None);
                            }
                        }
                        // No __isset: isset()/empty() see an absent
                        // prop — __get stays quiet; ?? still reads it
                        // (bug71359).
                        if mode != 2 {
                            return Ok(None);
                        }
                    }
                }
                self.check_prop_name(&pn)?;
                self.isset_quiet += 1;
                let v = self.prop_read_value(ov.clone(), &pn, *nullsafe);
                self.isset_quiet -= 1;
                match v {
                    Ok(v) => Ok(if matches!(v, Value::Null) {
                        None
                    } else {
                        Some(v)
                    }),
                    // A hooked get runs inside isset — its exceptions
                    // escape (write-only prop throws through the
                    // try/catch, not `false`).
                    Err(e) if matches!(e.kind, ErrorKind::Throw) => {
                        // Uninitialized typed prop reads still mean
                        // "not set" for isset — hook Errors escape.
                        if e.message
                            .ends_with("must not be accessed before initialization")
                        {
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                    Err(_) => Ok(None),
                }
            }
            Expr::StaticProp { class, name } => {
                // Scoped isset — `isset(A::$priv)` is false when the
                // static exists but is invisible to the current scope
                // (closure_041-046, disallows_*).
                let pn = match self.prop_name(name) {
                    Ok(pn) => pn,
                    Err(_) => return Ok(None),
                };
                let cls = match self.class_of(class) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                };
                self.statics_init(&cls)?;
                let mut v = cls.statics.borrow().get(&pn).map(|c| c.borrow().clone());
                if v.is_none() {
                    // Shared slot materialized on the DECLARING class
                    // after this class's init (shared statics).
                    if let Some((_, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                        if !Rc::ptr_eq(&dcls, &cls) {
                            self.statics_init(&dcls)?;
                            v = dcls.statics.borrow().get(&pn).map(|c| c.borrow().clone());
                        }
                    }
                }
                let ok = match self.find_static_prop_decl(&cls, &pn) {
                    Some((pd, dcls)) => match pd.visibility {
                        crate::ast::Visibility::Public => true,
                        _ => {
                            let scope = self
                                .stack
                                .last()
                                .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
                                .map(|s| s.name().to_string());
                            match (pd.visibility, scope) {
                                (crate::ast::Visibility::Private, Some(s)) => s == dcls.name(),
                                (crate::ast::Visibility::Protected, Some(s)) => {
                                    self.is_a_str(&s, dcls.name()) || self.is_a_str(dcls.name(), &s)
                                }
                                _ => false,
                            }
                        }
                    },
                    None => true,
                };
                match (v, ok) {
                    (Some(v), true) if !matches!(v, Value::Null) => Ok(Some(v)),
                    _ => Ok(None),
                }
            }
            _ => {
                let v = self.eval(e)?;
                Ok(if matches!(v, Value::Null) {
                    None
                } else {
                    Some(v)
                })
            }
        }
    }

    fn prop_read_loose(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Prop {
                obj,
                name,
                nullsafe,
                site,
            } => self.prop_read(obj, name, *nullsafe, *site),
            _ => self.eval(e),
        }
    }

    /// Byte-faithful string coercion — strings pass through untouched;
    /// other scalars go through conv_str (their output is ASCII anyway).
    pub(in crate::interp) fn conv_bytes(&mut self, v: &Value) -> Result<Vec<u8>, PhpError> {
        if let Value::Str(s) = v {
            return Ok(s.to_vec());
        }
        Ok(self.conv_str(v)?.into_bytes())
    }

    /// Object→string with __toString, plus array warning.
    pub(in crate::interp) fn conv_str(&mut self, v: &Value) -> Result<String, PhpError> {
        match v {
            Value::Array(_) => {
                self.warn("Array to string conversion")?;
                Ok("Array".into())
            }
            Value::Object(o) => {
                let class = o.borrow().class.clone();
                // __toString may be inherited — walk the chain, not just
                // the leaf decl (AbstractString defines it for
                // UnicodeString).
                if self.find_method_in(&class, "__tostring").is_some() {
                    let r = self.method_invoke(o.clone(), "__tostring", CallArgs::empty())?;
                    Ok(r.to_php_string())
                } else {
                    let cname = class.name().to_string();
                    let e = self.exception(
                        "Error",
                        &format!("Object of class {} could not be converted to string", cname),
                    );
                    self.pending_exception = Some(e);
                    Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Throw,
                        message: "cast".into(),
                        line: 0,
                    })
                }
            }
            Value::Callable(_) => {
                let e = self.exception(
                    "Error",
                    "Object of class Closure could not be converted to string",
                );
                self.pending_exception = Some(e);
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Throw,
                    message: "cast".into(),
                    line: 0,
                })
            }
            Value::Float(f) => {
                let prec = self.ini_int("precision", 14);
                Ok(crate::value::format_float_prec(*f, prec))
            }
            _ => Ok(v.to_php_string()),
        }
    }

    fn const_read(&mut self, name: &str) -> Result<Value, PhpError> {
        // Bare `self`/`static`/`parent` outside `::` are just undefined
        // constants ('Undefined constant "self"' — p15/v vs oracle).
        let key = name.trim_start_matches('\\');
        // Error names the ns-qualified candidate for an unqualified
        // const inside a namespace (namespaces/ns_041).
        let mut miss_name = name.trim_start_matches('\\').to_string();
        if !name.contains('\\') {
            // Unqualified constant inside a namespace: `ns\NAME` first,
            // then the global constant (Zend/tests/namespaces).
            let ns = self.caller_ns();
            if !ns.is_empty() {
                miss_name = format!("{}\\{}", ns, key);
                if let Some(v) = self.constants.get(&miss_name) {
                    return Ok(v.clone());
                }
            }
        }
        if let Some(v) = self.constants.get(key).cloned() {
            // PHP 8.4+ keeps E_STRICT defined but deprecated on use.
            if key.eq_ignore_ascii_case("E_STRICT") {
                self.deprecated(
                    "Constant E_STRICT is deprecated since 8.4, the error level was removed",
                )?;
            }
            return Ok(v);
        }
        if let Some(v) = self.constants.get(name) {
            return Ok(v.clone());
        }
        let v = self.exception("Error", &format!("Undefined constant \"{}\"", miss_name));
        self.pending_exception = Some(v);
        Err(PhpError {
            trace: None,
            thrown_line: None,
            display_msg: None,
            kind: ErrorKind::Throw,
            message: "const".into(),
            line: 0,
        })
    }

    /// The CV-effective line of an operand node: a folded varvar's CV
    /// binds at its inner name's line (like `Expr::Var` at its own);
    /// any other node's line is its own first-token marker.
    pub(in crate::interp) fn cv_site_of(marked: &Expr, unmarked: &Expr) -> Option<usize> {
        match unmarked {
            Expr::VarVar(inner, _) if is_compile_const(inner) => {
                Self::marked_line(inner).or_else(|| Self::marked_line(marked))
            }
            _ => Self::marked_line(marked),
        }
    }

    /// Strip `argline` markers and parens from an assign's value to
    /// its root node — transparent wrappers don't break zend's
    /// direct-RHS folding rules.
    pub(in crate::interp) fn unmark_rhs(mut e: &Expr) -> &Expr {
        loop {
            e = match e {
                Expr::Paren(inner) => inner,
                Expr::Binary {
                    op: "argline", r, ..
                } => r,
                _ => return e,
            };
        }
    }

    /// A CV-shaped operand: bare `Expr::Var` or a folded varvar — zend
    /// folds a compile-time-constant `${expr}` name to a plain CV read
    /// with the inner's first-token line.
    pub(in crate::interp) fn is_cv(e: &Expr) -> bool {
        match e {
            Expr::Var(_) => true,
            Expr::VarVar(inner, _) => is_compile_const(inner),
            _ => false,
        }
    }

    /// Eval a CV-shaped expr binding its read to `site` — the enclosing
    /// op's lineno. A bare Var reads at the ambient cur_line; a folded
    /// varvar takes the site through `vv_rhs_site` (its inner's argline
    /// would otherwise re-site the warn at the inner's own line).
    pub(in crate::interp) fn eval_cv_at(
        &mut self,
        e: &Expr,
        site: usize,
    ) -> Result<Value, PhpError> {
        let prev = self.vv_rhs_site;
        self.vv_rhs_site = Some(site);
        self.cur_line = site;
        self.send_line = Some(site);
        let r = self.eval(e);
        self.vv_rhs_site = prev;
        r
    }

    /// The literal name a folded `${...}` resolves to — zend's
    /// zend_is_assign_to_self requires a VAR+ZVAL child (a scalar
    /// literal); a folded expression never self-assigns.
    fn lit_var_name(e: &Expr) -> Option<String> {
        match Self::unmark_rhs(e) {
            Expr::Str(s) => Some(s.clone()),
            Expr::Int(n) => Some(n.to_string()),
            Expr::Float(f) => Some(crate::value::format_float_repr(*f)),
            Expr::Interp(parts)
                if parts
                    .iter()
                    .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))) =>
            {
                Some(parts.iter().fold(String::new(), |mut a, p| {
                    if let crate::lexer::StringPart::Lit(b) = p {
                        a.push_str(&String::from_utf8_lossy(b));
                    }
                    a
                }))
            }
            _ => None,
        }
    }

    /// zend_is_assign_to_self's container-root probe: a dim write's
    /// postfix ops walk to their root — only a bare `$name` or a
    /// folded `${'lit'}` can self-assign a bare-CV RHS (prop, static
    /// and call roots read the RHS live like any other operand).
    fn dim_self_root(e: &Expr) -> Option<String> {
        match e {
            Expr::Index { e, .. } | Expr::Paren(e) => Self::dim_self_root(e),
            Expr::Binary {
                op: "argline", r, ..
            } => Self::dim_self_root(r),
            Expr::Var(n) => Some(n.clone()),
            Expr::VarVar(inner, _) => Self::lit_var_name(inner),
            _ => None,
        }
    }

    /// What zend's compile-time const scan (`zend_eval_const_expr`,
    /// run by `can_match_use_jumptable`/`determine_switch_jumptable_type`
    /// before the compare loop) makes of one match/switch cond:
    /// - `Literal(node, v)` — already a zval (a scalar literal, or a
    ///   concat zend's parser folded keeping op1's line): the compare
    ///   op keeps the node's own token line. `v` is the zval's value
    ///   for the int/string continuation check.
    /// - `Folded(v)` — rewrote to a fresh zval stamped with
    ///   `CG(zend_lineno)` at scan time — the subject's compiled end
    ///   line — and the compare sites there.
    /// - `Unfolded` — stays an expr: the scan stops and the compare
    ///   sites at the cond's own end line.
    ///
    /// `??`/`?:` splice: their surviving branch classifies instead —
    /// `zend_eval_const_expr` applies to it first, so a folded branch
    /// carries the subject stamp while a literal one keeps its own
    /// line.
    pub(in crate::interp) fn cond_fold<'e>(&mut self, e: &'e Expr) -> CondFold<'e> {
        let u = Self::unmark_rhs(e);
        match u {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => match self.eval_const(u) {
                Ok(v) => CondFold::Literal(e, v),
                Err(_) => CondFold::Unfolded(e),
            },
            Expr::Interp(parts) if parts.iter().all(|p| matches!(p, StringPart::Lit(_))) => {
                match self.eval_const(u) {
                    Ok(v) => CondFold::Literal(e, v),
                    Err(_) => CondFold::Unfolded(e),
                }
            }
            Expr::Binary { op: ".", l, r } if Self::zval_lit(l) && Self::zval_lit(r) => {
                // zend_ast_create_concat_op folds at parse — the
                // resulting zval keeps op1's lexer line, i.e. the
                // cond behaves like a literal.
                match self.eval_const(u) {
                    Ok(v) => CondFold::Literal(e, v),
                    Err(_) => CondFold::Unfolded(e),
                }
            }
            Expr::Null | Expr::Bool(_) => self.fold_eval(e),
            Expr::MagicConst(_) => {
                // `__LINE__` reads cur_line — evaluate it at the
                // cond's own marker line like zend's parse.
                let l = Self::marked_line(e).unwrap_or(self.cur_line);
                let prev = self.cur_line;
                self.cur_line = l;
                let r = self.eval_const(u);
                self.cur_line = prev;
                match r {
                    Ok(v) => CondFold::Folded(v),
                    Err(_) => CondFold::Unfolded(e),
                }
            }
            Expr::Const(n) if self.foldable_const(n) => self.fold_eval(e),
            Expr::Index {
                e: arr,
                i: Some(dim),
            } if self.foldable_shape(Self::unmark_rhs(arr))
                && self.foldable_shape(Self::unmark_rhs(dim)) =>
            {
                // zend's DIM fold only resolves when the key exists —
                // a miss stays unfolded (and must not warn here).
                self.silence += 1;
                let r = self.eval_const(u);
                self.silence -= 1;
                match r {
                    Ok(v) if !matches!(v, Value::Null) => CondFold::Folded(v),
                    _ => CondFold::Unfolded(e),
                }
            }
            Expr::Binary { op: "??", l, r } => match self.cond_fold(l) {
                // The lhs couldn't reduce to a zval — `??` stays,
                // the whole cond is the compare's operand.
                CondFold::Unfolded(_) => CondFold::Unfolded(e),
                k => match k.value() {
                    // Null lhs splices the (already scanned) rhs into
                    // the cond slot.
                    Value::Null => self.cond_fold(r),
                    _ => k,
                },
            },
            Expr::Ternary { c, t, f } => match self.cond_fold(c) {
                CondFold::Unfolded(_) => CondFold::Unfolded(e),
                k => {
                    if k.value().is_truthy() {
                        match t {
                            Some(t) => self.cond_fold(t),
                            // `?:` splices the (already scanned) cond.
                            None => k,
                        }
                    } else {
                        self.cond_fold(f)
                    }
                }
            },
            _ if self.foldable_shape(u) => {
                // ClassConst/Index lookups can legitimately miss at
                // compile time — suppress their diagnostics for the
                // speculative scan (a miss stays unfolded anyway).
                self.silence += 1;
                let r = self.eval_const(u);
                self.silence -= 1;
                match r {
                    Ok(v) => CondFold::Folded(v),
                    Err(_) => CondFold::Unfolded(e),
                }
            }
            _ => CondFold::Unfolded(e),
        }
    }

    /// What zend_eval_const_expr can reduce to a zval: pure compile
    /// consts plus const fetches zend resolves at compile time —
    /// engine/user constants, class constants, magic constants and
    /// const-array dims.
    fn foldable_shape(&self, e: &Expr) -> bool {
        match Self::unmark_rhs(e) {
            Expr::Const(n) => self.foldable_const(n),
            Expr::ClassConst { .. } | Expr::MagicConst(_) => true,
            Expr::Index { e: arr, i: Some(d) } => {
                self.foldable_shape(arr) && self.foldable_shape(d)
            }
            Expr::Binary { l, r, .. } => self.foldable_shape(l) && self.foldable_shape(r),
            Expr::Unary { e, .. } | Expr::Cast { e, .. } | Expr::Paren(e) => self.foldable_shape(e),
            Expr::ArrayLit(items) => items.iter().all(|(k, v)| {
                k.as_ref().map(|k| self.foldable_shape(k)).unwrap_or(true) && self.foldable_shape(v)
            }),
            _ => is_compile_const(e),
        }
    }

    /// A constant name zend's const fetch could resolve while
    /// compiling this file — the engine constants present in the
    /// table at compile start. User `const` decls and `define` calls
    /// are runtime ops: their names can't fold a cond even though
    /// the interp's table holds them by the time it runs.
    fn foldable_const(&self, n: &str) -> bool {
        self.engine_consts.contains(n.trim_start_matches('\\'))
    }

    /// A plain zval in zend's AST: scalar literals and literal-only
    /// concats (which `zend_ast_create_concat_op` folds at parse).
    fn zval_lit(e: &Expr) -> bool {
        match Self::unmark_rhs(e) {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => true,
            Expr::Interp(parts) => parts.iter().all(|p| matches!(p, StringPart::Lit(_))),
            Expr::Binary { op: ".", l, r } => Self::zval_lit(l) && Self::zval_lit(r),
            _ => false,
        }
    }

    /// A compile-foldable cond evaluates to a fresh stamped zval —
    /// or stays unfolded when the const eval can't resolve it.
    fn fold_eval<'e>(&mut self, e: &'e Expr) -> CondFold<'e> {
        match self.eval_const(e) {
            Ok(v) => CondFold::Folded(v),
            Err(_) => CondFold::Unfolded(e),
        }
    }

    /// Where CG(zend_lineno) stands after zend finishes compiling an
    /// UNFOLDED match/switch cond: each `zend_compile_expr` stamps it
    /// to the node's own line, so the last visited node wins — the
    /// rightmost leaf in zend's compile order. A leaf the const scan
    /// folded to a zval carries the subject-end stamp (`subj_site`);
    /// a literal or runtime leaf keeps its own line. `X ?? null` and
    /// `$a + null` end on the folded `null` (the stamp), `(1+2) + $a`
    /// ends on `$a` (a runtime read at its own line).
    pub(in crate::interp) fn unfold_tail_site(
        &mut self,
        e: &Expr,
        subj_site: usize,
    ) -> Option<usize> {
        match self.cond_fold(e) {
            CondFold::Folded(_) => Some(subj_site),
            CondFold::Literal(le, _) => Self::marked_line(le).or_else(|| Self::inner_end_line(le)),
            CondFold::Unfolded(inner) => match Self::unmark_rhs(inner) {
                Expr::Binary { r, .. } => self.unfold_tail_site(r, subj_site),
                Expr::Ternary { f, .. } => self.unfold_tail_site(f, subj_site),
                Expr::Assign { value, .. } => self.unfold_tail_site(value, subj_site),
                Expr::Paren(i)
                | Expr::PreInc(i)
                | Expr::PreDec(i)
                | Expr::PostInc(i)
                | Expr::PostDec(i)
                | Expr::Print(i)
                | Expr::Clone(i)
                | Expr::Unpack(i)
                | Expr::Fcc(i)
                | Expr::Throw(i)
                | Expr::YieldFrom(i)
                | Expr::Empty(i)
                | Expr::ByRef(i)
                | Expr::Unary { e: i, .. }
                | Expr::Cast { e: i, .. } => self.unfold_tail_site(i, subj_site),
                Expr::Call { args, site, .. }
                | Expr::MethodCall { args, site, .. }
                | Expr::StaticCall { args, site, .. }
                | Expr::StaticCallDyn { args, site, .. }
                | Expr::New { args, site, .. } => match args.last() {
                    Some(a) => self.unfold_tail_site(a, subj_site).or(Some(*site)),
                    None => Some(*site),
                },
                // FETCH_DIM emits after the dim's ops (a delayed CV
                // container binds inside it) — the dim is the tail.
                Expr::Index { e: arr, i } => i
                    .as_deref()
                    .and_then(|d| self.unfold_tail_site(d, subj_site))
                    .or_else(|| self.unfold_tail_site(arr, subj_site)),
                Expr::ArrayLit(items) => items
                    .iter()
                    .rev()
                    .find_map(|(_, v)| self.unfold_tail_site(v, subj_site)),
                Expr::Isset(args) => args
                    .last()
                    .and_then(|a| self.unfold_tail_site(a, subj_site)),
                Expr::Yield { key, val } => val
                    .as_deref()
                    .and_then(|v| self.unfold_tail_site(v, subj_site))
                    .or_else(|| {
                        key.as_deref()
                            .and_then(|k| self.unfold_tail_site(k, subj_site))
                    }),
                Expr::Prop { name, site, .. } => match name {
                    PropName::Expr(i) => self.unfold_tail_site(i, subj_site).or(Some(*site)),
                    _ => Some(*site),
                },
                // A nested match compiles its arms as real ops — CG
                // ends at the last arm's result, not the cond marker.
                Expr::Match { .. } => Self::inner_end_line(e)
                    .or_else(|| Self::marked_line(e))
                    .or_else(|| Self::inner_end_line(inner)),
                _ => Self::marked_line(e)
                    .or_else(|| Self::inner_end_line(e))
                    .or_else(|| Self::inner_end_line(inner)),
            },
        }
    }

    fn assign(
        &mut self,
        target: &Expr,
        op: &'static str,
        value: &Expr,
        aline: usize,
    ) -> Result<Value, PhpError> {
        // Op boundary for the armed dim throwable — see dim_op_outlet.
        let outer = self.dim_throw.take();
        let r = self.assign_inner(target, op, value, aline);
        self.dim_op_outlet(r, outer)
    }

    fn assign_inner(
        &mut self,
        target: &Expr,
        op: &'static str,
        value: &Expr,
        aline: usize,
    ) -> Result<Value, PhpError> {
        // `$this` may never be an assignment target (compile fatal,
        // bug24573); plain and compound assigns both route here.
        if let Expr::Var(n) = target {
            if n == "this" {
                return self.fail(PhpError::fatal("Cannot re-assign $this", 0));
            }
        }
        if op == "=&" {
            // Shape checks on the source see through the arg's
            // line marker (`$a =& ($b)` is still a Var source).
            let value_u = Self::unmark_rhs(value);
            // zend refuses the $GLOBALS table itself as a by-ref source
            // (compile error `Cannot acquire reference to $GLOBALS`) —
            // element access $GLOBALS['x'] is fine.
            if let Expr::Var(n) = value_u {
                if n == "GLOBALS" {
                    // Engine-side fatal — zend prints the
                    // `Stack trace:\n#0 {main}` block too.
                    return self.fail(PhpError::compile_fatal(
                        "Cannot acquire reference to $GLOBALS",
                        self.cur_line,
                    ));
                }
                if n == "this" {
                    // `$this` can never be IS_REFERENCE — `&$this`
                    // degrades to `=`; the object still aliases via
                    // its handle (bug72598).
                    let v = self.eval(value)?;
                    self.store(target, v.clone())?;
                    return Ok(v);
                }
            }
            // By-reference assignment: bind cells.
            let src = match value_u {
                Expr::Call { .. }
                | Expr::MethodCall { .. }
                | Expr::StaticCall { .. }
                | Expr::StaticCallDyn { .. } => {
                    let (c, was_ref) = self.eval_call_cell(value)?;
                    if !was_ref {
                        // Non-ref callee: zend warns and falls back to
                        // a plain assignment INTO the target's cell —
                        // the target keeps its identity (bug20175's
                        // `static $v = &f()`).
                        self.notice("Only variables should be assigned by reference")?;
                        let v = c.borrow().clone();
                        self.store(target, v.clone())?;
                        return Ok(v);
                    }
                    c
                }
                _ => {
                    // `=&` sources fetch under zend's BP_VAR_RW flag —
                    // dim fetches on non-indexable containers throw the
                    // reference-specific catchable matrix (probe4).
                    let was = std::mem::replace(&mut self.dim_by_ref, true);
                    let c = self.eval_cell(value);
                    self.dim_by_ref = was;
                    let c = c?;

                    // A `=&` source that is itself a typed-prop slot
                    // carries that prop's declared type into the
                    // conflict check (typed_properties_068/076). The
                    // receiver value comes from prop_cell's stash —
                    // re-evaluating obj would double `$o->m()`'s side
                    // effects (finding 6).
                    let decl = match value_u {
                        Expr::Prop { name, .. } => {
                            let ov = self.last_prop_ov.take().unwrap_or(Value::Null);
                            match (&ov, self.prop_name(name)) {
                                (Value::Object(o), Ok(pn)) => {
                                    self.decl_prop(o, &pn).map(|(pd, dc)| {
                                        let sk =
                                            self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn,
                                            SlotAnchor::Obj(Rc::downgrade(o), sk),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        Expr::StaticProp { class, name } => {
                            match (self.member_class_of(class), self.prop_name(name)) {
                                (Ok((cls, _)), Ok(pn)) => {
                                    self.find_static_prop_decl(&cls, &pn).map(|(pd, dc)| {
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn.clone(),
                                            SlotAnchor::Statics(dc.name().to_string(), pn),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some((Some(tys), dcn, dpn, anc)) = decl {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(p, (c.clone(), tys, dcn, dpn));
                        self.slot_anchor.insert(p, anc);
                    }
                    c
                }
            };
            let was = std::mem::replace(&mut self.dim_by_ref, true);
            let br = self.bind_cell(target, src.clone());
            self.dim_by_ref = was;
            // prop_cell's one-shot receiver stash is consumed by the
            // typed-prop check above only for `Expr::Prop` sources —
            // for any other source/target shape it would pin the
            // object's Rc past the op and root a dead cycle
            // (gc_006: `$a->a[0] =& $a` collects 0 instead of 2).
            self.last_prop_ov = None;
            br?;
            return Ok(src.borrow().clone());
        }
        if op == "=" {
            if let Expr::List(items) = target {
                if Self::list_has_ref(items) {
                    // `[$a, &$b] = $src` — `&` elements bind the source's
                    // real cells; literal sources already compile-fataled
                    // in the parser and other temps get zend's
                    // per-element 'set reference' notice (probe5f/5h).
                    let refable = matches!(
                        value,
                        Expr::Var(_)
                            | Expr::Index { .. }
                            | Expr::Prop { .. }
                            | Expr::StaticProp { .. }
                            | Expr::VarVar(..)
                    );
                    let v = self.eval(value)?;
                    return self.store_list(items, v, refable);
                }
            }
        }
        let needs_read = op != "=";
        // PHP evaluates the LHS lvalue chain (index exprs' side effects)
        // BEFORE the RHS: `$a[f()][g()] = rhs` calls f,g first
        // (engine_assignExecutionOrder_003). Dynamic names inside an object
        // property access ($o->{e}, $o->p[e]) are evaluated early for side
        // effects but Zend's temp register is then overwritten by the
        // assignment value — so the ACTUAL name/key becomes the RHS value
        // (engine_assignExecutionOrder_001).
        enum Late {
            Prop {
                ov: Value,
                name: Option<PropName>,
            },
            PropStr {
                ov: Value,
                pn: String,
            },
            Index {
                base: Cell,
                key: Option<Cell>,
                append: bool,
            },
            Static {
                class: Box<Expr>,
                pn: String,
            },
            Keyed {
                base: Cell,
                keys: Vec<DimArg>,
            },
            None,
        }
        let mut late = Late::None;
        // `??=` on an index target rooted at an UNDEFINED var must not
        // materialize it — zend's isset check leaves it missing so the
        // RHS read warns 'Undefined variable' (assign_coalesce_007).
        // The base is a detached Null cell re-resolved at write time.
        let mut undef_root: Option<String> = None;
        // A var-rooted dim write binds its container cell before the
        // key exprs run — a handler that rebinds the var detaches the
        // pending write onto the stale slot (assign_dim_014).
        let mut dim_root: Option<(DimDetach, Cell)> = None;
        // A self-assign's QM read warns on an undef CV — the
        // container's write-context eval binds it to Null silently,
        // so the name's undef-ness is captured before the bind.
        let mut root_undef_name: Option<String> = None;
        // Dynamic-prop slots materialized below record themselves so
        // the read side can replay zend's 'Undefined property' warns.
        self.fresh_dyn_props.clear();
        // A typed slot this write's fetch materializes reverts to
        // uninit if the auto-init gate fails — the marker lives from
        // here to the gate (a stale one from an earlier write would
        // wrongly revert a legitimately-committed NULL).
        self.last_fresh_cell = None;
        let target_cell = match target {
            Expr::Prop { obj, name, .. } => {
                // A compound op's RW prop fetch warns 'Undefined
                // variable' on an undef CV container — plain `=` and
                // by-ref fetches resolve it to null silently instead
                // (r($undef->p)).
                if needs_read && op != "??=" {
                    self.warn_undef_lvalue_obj(obj)?;
                }
                // zend fetches the object operand in write context —
                // an intermediate readonly prop holding a non-object
                // dies here, naming THAT prop (R3 finding 3).
                // A compound op's object fetch is a READ — zend warns
                // on the dim miss the write-context cell fetch
                // resolves to Null silently, and the read's throwable
                // kills the prop write outright.
                let ov = if needs_read
                    && op != "??="
                    && matches!(
                        &**obj,
                        Expr::Index { .. } | Expr::StaticProp { .. } | Expr::VarVar(..)
                    ) {
                    self.eval(obj)?
                } else {
                    self.eval_lvalue_obj(obj)?
                };
                // A {dynamic} name expr resolves now (side effects +
                // the var-var temp is read early, matching Zend);
                // a plain $var name reads late at write time.
                if matches!(name, PropName::Expr(_)) {
                    let pn = self.prop_name(name)?;
                    late = Late::PropStr { ov, pn };
                } else {
                    late = Late::Prop {
                        ov,
                        name: Some(name.clone()),
                    };
                }
                None
            }
            // A single-level prop-index takes this arm — a deeper
            // `$o->p[$u][$v]` chain needs the generic arm's per-level
            // traversal so every dim's read warn fires (x3).
            Expr::Index { e, i } if matches!(&**e, Expr::Prop { .. }) => {
                // Prop-chain index: the container resolves early; the dim
                // expr's own value is always the key — `$o->a[${f()}]` is a
                // variable-variable, not a register quirk
                // (engine_assignExecutionOrder_001 reads $name that way).
                match self.eval_cell(e) {
                    Ok(c) => {
                        // zend's ASSIGN_DIM dispatches the fetched leaf
                        // before OP2 reads: `=` on a scalar dies before
                        // the key evaluates; a false leaf warns then
                        // vivifies. Objects/strings defer to the write
                        // arm; compound/coalesce fetch the key first.
                        if op == "=" && !self.detached_dim {
                            self.dim_prekey_container(&c, false, false)?;
                        }
                        // Same per-op cache reset as the plain Index arm —
                        // a stale entry keyed by a CV's stable cell ptr
                        // would reuse last op's converted key ($a->p[$k]
                        // re-writing the first bound index).
                        if !self.in_handler {
                            self.dim_key_conv.clear();
                            self.dim_cv_bound.clear();
                        }
                        let key = match i.as_deref() {
                            Some(ie) => self.dim_key(ie)?,
                            None => None,
                        };
                        late = Late::Index {
                            base: c,
                            key,
                            append: i.is_none(),
                        };
                        None
                    }
                    Err(_) => None,
                }
            }
            Expr::Index { .. } => {
                // Zend evaluates the container expr FIRST — once — then
                // the dim exprs eagerly (innermost first), the RHS, and
                // only then traverses for the fetch/write
                // (cont()[dk()] += rhs() prints container,dimkey,RHS).
                // For a variable base eval_cell returns the live slot,
                // so the write still lands on its CURRENT value
                // (engine_assignExecutionOrder_003 mod() case).
                let mut dims = Vec::new();
                let mut base = target;
                while let Expr::Index { e, i } = base {
                    dims.push(i.as_deref());
                    base = e;
                }
                dims.reverse();
                let c = if op == "??=" {
                    match base {
                        Expr::Var(n) if self.var_cell_opt(n).is_none() => {
                            undef_root = Some(n.clone());
                            cell(Value::Null)
                        }
                        _ => {
                            let c = self.eval_cell(base)?;
                            if let Expr::Var(n) = base {
                                dim_root = Some((
                                    DimDetach {
                                        name: n.clone(),
                                        pre: DimPre::of(&c.borrow()),
                                        pre_cell: Rc::as_ptr(&c) as usize,
                                        coalesce: false,
                                    },
                                    c.clone(),
                                ));
                            }
                            c
                        }
                    }
                } else {
                    // A compound op fetches the container for
                    // read+write: an undef CV warns 'Undefined
                    // variable' at the fetch, then zend's
                    // unconditional ZVAL_ARR lands a fresh array on
                    // the slot — even over a handler's mid-warn
                    // rebind. `=`/`??=` keep silent fetch semantics.
                    if needs_read {
                        self.warn_undef_lvalue_obj(base)?;
                        if let Expr::Var(n) = base {
                            if self.var_lookup(n).is_none() {
                                let c = self.var_cell(n);
                                *c.borrow_mut() =
                                    Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                            }
                        }
                    }
                    if let Some(n) = Self::dim_self_root(base) {
                        if self.var_lookup(&n).is_none() {
                            root_undef_name = Some(n);
                        }
                    }
                    let c = self.eval_cell(base)?;
                    if let Expr::Var(n) = base {
                        dim_root = Some((
                            DimDetach {
                                name: n.clone(),
                                pre: DimPre::of(&c.borrow()),
                                pre_cell: Rc::as_ptr(&c) as usize,
                                coalesce: false,
                            },
                            c.clone(),
                        ));
                    }
                    c
                };
                // The per-op conversion cache resets for each pending
                // dim write — but a nested write issued inside the
                // error handler mid-key-eval must not erase the outer
                // op's cache (its conversions stay deduped).
                if !self.in_handler {
                    self.dim_key_conv.clear();
                    self.dim_cv_bound.clear();
                }
                let mut keys = Vec::with_capacity(dims.len());
                for d in dims {
                    match d {
                        // A throwing key expr propagates — swallowing
                        // it as `None` would silently append (finding 7).
                        Some(ie) => {
                            // A plain-$var key binds at its own dim op —
                            // zend's dim ops fetch the container level
                            // first (`$b['x'][$u]` on scalar $b['x']
                            // dies before reading $u) and each CV
                            // operand once (the read and the write see
                            // the same bound cell, warned once).
                            match Self::unmark_rhs(ie) {
                                Expr::Var(n) => {
                                    keys.push(DimArg::Cv(n.clone(), Self::marked_line(ie)))
                                }
                                _ => keys.push(DimArg::from(self.dim_key(ie)?)),
                            }
                        }
                        None => keys.push(DimArg::Append),
                    }
                }
                late = Late::Keyed { base: c, keys };
                None
            }
            Expr::StaticProp { class, name } => {
                // Static prop names evaluate BEFORE the RHS with their own
                // result (no register quirk: engine_assignExecutionOrder_001).
                match self.prop_name(name) {
                    Ok(pn) => {
                        late = Late::Static {
                            class: class.clone(),
                            pn,
                        };
                        None
                    }
                    Err(_) => None,
                }
            }
            Expr::List(_) => None,
            _ => {
                // A plain-CV compound assign fetches BP_VAR_RW — zend
                // warns the undef var at the fetch (`$undef += 1`).
                if needs_read && op != "??=" {
                    if let Expr::Var(n) = target {
                        if self.var_lookup(n).is_none() {
                            self.warn(&format!("Undefined variable ${}", n))?;
                        }
                    }
                }
                self.eval_cell(target).ok()
            }
        };
        // zend's delayed-compile RHS binds the DIRECT (paren/marker-
        // stripped) RHS-root CV inside the ASSIGN op at the ASSIGN's
        // own line — `$x = (\n$u\n)` warns at `=`'s line, not the var's
        // inner line. `=` and `??=` qualify; compound ops (.=, +=) read
        // the CV as an operand at its own line instead. A bare Var
        // evals the stripped root so an inner `argline` mark can't
        // re-site it; a folded varvar takes the site via vv_rhs_site.
        let rhs_u = Self::unmark_rhs(value);
        let pin_rhs = matches!(op, "=" | "??=") && Self::is_cv(rhs_u);
        // A `&` destructure element write-fetches its slot inside the
        // RHS container and aliases that live cell — bind the
        // container to its cell once here (the same auto-vivifying
        // fetch zend's FETCH_LIST emits), instead of a read clone
        // whose slots would alias nothing. A call-family RHS stays a
        // value (its temp can't hold refs; zend notices instead).
        let list_byref = match target {
            Expr::List(items) => Self::list_any_byref(items),
            _ => false,
        };
        let mut rhs_cell: Option<Cell> = None;
        // zend binds the ASSIGN_LIST op's delayed CV read at the FIRST
        // element's line — a multi-line `[\n$a,\n$b\n] = $v` warns at
        // element 0's line, not the `=` node's.
        let rhs_site = if pin_rhs {
            match target {
                Expr::List(items) => items
                    .iter()
                    .flatten()
                    .next()
                    .and_then(|(_, e)| crate::ast::start_line(e))
                    .unwrap_or(aline),
                _ => aline,
            }
        } else {
            aline
        };

        // Zend's ASSIGN_DIM_OP order: dim exprs eager (above) → the
        // RHS → the dim fetch (its `Undefined array key` warnings and
        // auto-vivification print AFTER any output the RHS produced) →
        // the write. `??=` is the exception: it isset()-checks BEFORE
        // the RHS, and a set key skips the RHS entirely — the value is
        // the fetched one.
        macro_rules! dim_read {
            ($quiet:expr, $det:expr) => {
                match &target_cell {
                    Some(c) => c.borrow().clone(),
                    None => {
                        // Dim targets read through compound_dim_read:
                        // zend's fetch-for-write emits per-level
                        // `Undefined array key` warnings on missing/null
                        // levels but stays silent when the write itself
                        // will throw (bug29893) — and never re-evaluates
                        // the dim key exprs. `??=` reads with isset()
                        // semantics (no warnings; typed_properties_103).
                        // Slots this write-fetch materialized warn
                        // 'Undefined property' first (finding 13).
                        if !$quiet {
                            let ws = std::mem::take(&mut self.fresh_dyn_props);
                            for (_, cn, pn) in ws {
                                self.warn(&format!("Undefined property: {}::${}", cn, pn))?;
                            }
                        }
                        match &late {
                            Late::Keyed { base, keys } => self.compound_dim_read(
                                base.clone(),
                                keys,
                                $quiet,
                                $det.as_ref(),
                                false,
                            )?,
                            Late::Index { base, key, .. } => {
                                let keys = [DimArg::from(key.clone())];
                                self.compound_dim_read(
                                    base.clone(),
                                    &keys,
                                    $quiet,
                                    $det.as_ref(),
                                    false,
                                )?
                            }
                            // Prop targets read through the CACHED object
                            // (evaluated once, above) — re-evaluating
                            // `$o->m()->p .= v`'s target would call m()
                            // twice (finding 6).
                            Late::Prop { ov, name } if matches!(ov, Value::Object(_)) => {
                                let pn = match name {
                                    Some(n) => self.prop_name(n)?,
                                    // Late::Prop always carries Some
                                    // (None was the dead register-quirk
                                    // arm).
                                    None => String::new(),
                                };
                                if $quiet {
                                    self.isset_quiet += 1;
                                }
                                let c = self.prop_read_value(ov.clone(), &pn, false);
                                if $quiet {
                                    self.isset_quiet -= 1;
                                    c.unwrap_or(Value::Null)
                                } else {
                                    // Compound reads propagate real
                                    // Errors — an uninit typed prop's
                                    // 'must not be accessed' beats the
                                    // operand TypeError (oracle).
                                    c?
                                }
                            }
                            Late::PropStr { ov, pn } if matches!(ov, Value::Object(_)) => {
                                if $quiet {
                                    self.isset_quiet += 1;
                                }
                                let c = self.prop_read_value(ov.clone(), pn, false);
                                if $quiet {
                                    self.isset_quiet -= 1;
                                    c.unwrap_or(Value::Null)
                                } else {
                                    c?
                                }
                            }
                            // A compound-read on a static prop runs
                            // the real read path — uninitialized typed
                            // statics surface 'must not be accessed
                            // before initialization' (`??=` keeps the
                            // silent isset-style read).
                            Late::Static { class, pn } => {
                                let c = self.static_prop_read(class, &PropName::Name(pn.clone()));
                                if $quiet {
                                    c.unwrap_or(Value::Null)
                                } else {
                                    c?
                                }
                            }
                            _ => {
                                // `$i->p += v` on a non-object base: the
                                // write throws zend's assign Error and
                                // the read never runs — no 'Attempt to
                                // read property' warning first (probe4i).
                                if matches!(&late, Late::Prop { .. } | Late::PropStr { .. }) {
                                    Value::Null
                                } else {
                                    if $quiet {
                                        self.isset_quiet += 1;
                                    }
                                    let c = self.eval(target);
                                    if $quiet {
                                        self.isset_quiet -= 1;
                                    }
                                    c.unwrap_or(Value::Null)
                                }
                            }
                        }
                    }
                }
            };
        }
        // zend's dim ops bind the container operand at op entry — `pre`
        // snapshots that value so a diagnostic mid-traversal that
        // rebinds the root var detaches the pending write onto the
        // stale slot (RHS-eval diags land inside `pre` — they can't
        // detach, the op hasn't started yet).
        let mut det: Option<DimDetach> = if op == "??=" {
            dim_root.as_ref().map(|(d, _)| DimDetach {
                coalesce: true,
                ..d.clone()
            })
        } else {
            None
        };
        if needs_read && op == "??=" {
            let cur = dim_read!(true, det);
            if !matches!(cur, Value::Null) {
                return Ok(cur);
            }
        }
        // A bare-CV RHS is the op's own operand — zend reads it inside
        // the op, after the dim fetch's diagnostics (an expression
        // compiles its own ops and keeps early eval). A rebound var is
        // seen post-fetch either way, so only the warn position moves.
        let rhs_var = {
            let mut v = value;
            while let Expr::Paren(inner) = v {
                v = inner.as_ref();
            }
            matches!(v, Expr::Var(_))
        };
        // `=`/`??=`'s dim-write defers the bare-CV operand even later:
        // zend's ASSIGN_DIM runs every key's bind+conversion inside
        // its FETCH_DIM_W walk BEFORE OP_DATA reads the RHS —
        // `$a[$k] = $u` warns/converts $k ahead of reading $u. The
        // write walk evals it at the last level; expr RHS and
        // non-dim targets keep the op-entry read (bug79599).
        // ...except zend_is_assign_to_self: `$a[...] = $a` compiles
        // the same-named bare RHS to an early QM_ASSIGN — the op-entry
        // read is frozen before the dim ops run, so the store never
        // sees the write's own vivifications (`$a[0] = $a` keeps the
        // pre-fetch $a). `??=` freezes its self-assign the same way
        // ($a warns ahead of the store's key re-read).
        let rhs_name = match rhs_u {
            Expr::Var(n) => Some(n.clone()),
            Expr::VarVar(inner, _) => Self::lit_var_name(inner),
            _ => None,
        };
        let rhs_self = rhs_name.is_some() && rhs_name == Self::dim_self_root(target);
        let rhs_late: Option<(&Expr, usize)> = if matches!(op, "=" | "??=")
            && pin_rhs
            && !rhs_self
            && matches!(&late, Late::Keyed { .. } | Late::Index { .. })
        {
            Some((rhs_u, rhs_site))
        } else {
            None
        };
        let rhs_r = if (needs_read && rhs_var) || rhs_late.is_some() {
            Ok(Value::Null)
        } else if pin_rhs {
            let r = if rhs_self && root_undef_name.is_some() {
                // zend's QM_ASSIGN warns on the undef CV — the
                // container's W-eval already bound it silently, so
                // emit the read's warn at the RHS's own site.
                self.cur_line = rhs_site;
                self.send_line = Some(rhs_site);
                if !self.is_quiet() {
                    let n = root_undef_name.as_deref().unwrap();
                    self.warn(&format!("Undefined variable ${n}"))?;
                }
                Ok(Value::Null)
            } else {
                self.eval_cv_at(rhs_u, rhs_site)
            };
            if list_byref {
                // The pinned CV is a bare Var/folded varvar — grab
                // its live cell too so `&` elements write-fetch
                // (auto-vivify) inside it (`[&$r] = $a` on null).
                rhs_cell = self.eval_cell(rhs_u).ok();
            }
            r
        } else if list_byref
            && matches!(
                rhs_u,
                Expr::Var(_)
                    | Expr::Index { .. }
                    | Expr::Prop { .. }
                    | Expr::StaticProp { .. }
                    | Expr::VarVar(..)
            )
        {
            match self.eval_cell(rhs_u) {
                Ok(cc) => {
                    let v = cc.borrow().clone();
                    rhs_cell = Some(cc);
                    Ok(v)
                }
                Err(e) => Err(e),
            }
        } else {
            self.eval(value)
        };
        let rhs = rhs_r?;
        // `&` elements on a call-family RHS: zend reports
        // 'Attempting to set reference...' only when the callee did
        // NOT declare `&` — a by-ref return binds plainly.
        let rhs_nonref_call = list_byref
            && matches!(
                rhs_u,
                Expr::Call { .. }
                    | Expr::MethodCall { .. }
                    | Expr::StaticCall { .. }
                    | Expr::StaticCallDyn { .. }
            )
            && !self.last_call_by_ref;

        if det.is_none() {
            // zend's ASSIGN_DIM materializes an undef/null container
            // slot to a fresh array AFTER the RHS but BEFORE the dim
            // operands evaluate — the pending write's detach sentinel
            // is that array's refcount: a handler rebind kills it.
            det = match dim_root.as_ref() {
                Some((d, c)) => {
                    if matches!(&*c.borrow(), Value::Null) {
                        // Typed slots gate array promotion —
                        // `Cannot auto-initialize an array inside ...`
                        // beats the pending write (typed_properties_083).
                        self.auto_init_gate(c)?;
                        if matches!(&*c.borrow(), Value::Null) {
                            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                        }
                    }
                    Some(DimDetach {
                        name: d.name.clone(),
                        pre: DimPre::of(&c.borrow()),
                        pre_cell: d.pre_cell,
                        coalesce: d.coalesce,
                    })
                }
                None => None,
            };
        }
        let dim_det = det.as_ref().is_some_and(|d| self.dim_detached(d));
        let cur = if needs_read {
            if op == "??=" {
                // Reaching here means the isset read found null/missing.
                Value::Null
            } else {
                dim_read!(dim_det, det)
            }
        } else {
            Value::Null
        };
        // zend's EG(exception) check at the fetch's end kills the
        // whole write opcode for a non-final-level defer: no gate,
        // op_data read, or operator runs — the defer unwinds now.
        if self
            .dim_throw
            .as_ref()
            .is_some_and(|(_, _, live, _)| !*live)
        {
            return self.dim_raise(Value::Null);
        }
        // A diagnostic mid-fetch can still kill the container — zend's
        // sentinel then aborts the op before its gate and its op_data
        // read, so re-check after the read too.
        let dim_det = dim_det
            || det
                .as_ref()
                .is_some_and(|d| !d.coalesce && self.dim_detached(d));
        // A static-prop compound assign gates set-visibility on the
        // RW fetch — AFTER the RHS and the fetch proper (an uninit
        // typed static's 'must not be accessed' wins) but BEFORE the
        // op (`C::$a += []` names 'indirectly modify', not operands).
        if needs_read && op != "??=" {
            if let Late::Static { class, pn } = &late {
                self.static_prop_indirect_gate(class, pn)?;
            }
        }
        // A compound op gates on the target container BEFORE the
        // operator evaluates — `$s[k] += v` throws the string-offset
        // gate (or the offset TypeError) rather than an operand error
        // on the fetched Null, and scalars/plain objects name their
        // 'Cannot use ... as array' Error ahead of the arith (p12h/p12k).
        if needs_read && op != "??=" && !dim_det {
            match &late {
                Late::Index { base, key, append } => {
                    // `[]` keeps its own gate: '[] operator not
                    // supported for strings' outranks the assign-op
                    // offset check (probe8).
                    if *append && matches!(&*base.borrow(), Value::Str(_)) {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "[] operator not supported for strings",
                            self.cur_line,
                        ));
                    }
                    self.compound_dim_gate(base, key)?
                }
                Late::Keyed { base, keys, .. } => {
                    let mut c = base.clone();
                    'gate: for k in keys {
                        let kc = match k {
                            DimArg::Append => None,
                            DimArg::Key(c) => Some(c.clone()),
                            DimArg::Cv(n, ln) => {
                                // The CV read sites at its own operand
                                // line — the dim op carries the key's
                                // marked line.
                                if let Some(l) = ln {
                                    self.cur_line = *l;
                                    self.send_line = Some(*l);
                                }
                                // zend's GC_ADDREF sentinel wraps the
                                // CV-read diagnostic — a handler's
                                // write separates instead of landing
                                // in place on the fetched container.
                                let _h = det.as_ref().and_then(|d| d.pre.value());
                                Some(self.dim_var_key(n)?)
                            }
                        };
                        // A rebind inside the bind's diagnostics
                        // detached the op — zend's fetch aborts and
                        // the whole op goes silent: no gates fire.
                        if det.as_ref().is_some_and(|d| self.dim_detached(d)) {
                            break 'gate;
                        }
                        if kc.is_none() && matches!(&*c.borrow(), Value::Str(_)) {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "[] operator not supported for strings",
                                self.cur_line,
                            ));
                        }
                        {
                            let _h = det.as_ref().and_then(|d| d.pre.value());
                            self.compound_dim_gate(&c, &kc)?;
                        }
                        if det.as_ref().is_some_and(|d| self.dim_detached(d)) {
                            break 'gate;
                        }
                        let nxt = {
                            let b = c.borrow();
                            match &*b {
                                Value::Array(rc) => {
                                    let rc = rc.clone();
                                    drop(b);
                                    kc.as_ref()
                                        .map(|kc| self.dim_arr_key(kc).unwrap_or(ArrKey::Tomb))
                                        .and_then(|ak| rc.borrow().get_cell(&ak))
                                }
                                _ => None,
                            }
                        };
                        match nxt {
                            Some(nc) => c = nc,
                            None => break,
                        }
                    }
                }
                _ => {}
            }
        }

        // The bare-CV RHS's own operand read — zend does it after the
        // fetch's diagnostics and the container gate (an aborting gate
        // or a detached fetch never reads op_data), before the operator
        // applies. A `??=` dim-target RHS defers further to OP_DATA
        // (rhs_late) — the store's dispatch dies before ever reading it.
        let rhs = if needs_read && rhs_var && rhs_late.is_none() {
            if dim_det {
                Value::Null
            } else {
                self.eval(value)?
            }
        } else {
            rhs
        };
        // A plain-ArrayAccess fetch that threw armed the pair with
        // `gate` — zend's op died at the read_dimension NULL, so the
        // operator never runs; the write-gate 'Cannot use object'
        // raises it (armed throwable chained) when dispatch reaches it.
        let op_dead = self.dim_throw.as_ref().is_some_and(|(_, _, _, gate)| *gate);
        let newv = match op {
            _ if op_dead => Value::Null,
            "=" => rhs,
            "+=" => self.arith("+", cur, rhs).or_else(|e| self.arm_binop(e))?,
            "-=" => self.arith("-", cur, rhs).or_else(|e| self.arm_binop(e))?,
            "*=" => self.arith("*", cur, rhs).or_else(|e| self.arm_binop(e))?,
            "/=" => self.arith("/", cur, rhs).or_else(|e| self.arm_binop(e))?,
            "%=" => self.arith("%", cur, rhs).or_else(|e| self.arm_binop(e))?,
            ".=" => {
                // zend_call_function's EG(exception) gate: __toString
                // can't run while a throwable is pending — the
                // conversion fails, the op dies, the armed one raises.
                if self.dim_throw.is_some() && matches!(&rhs, Value::Object(_)) {
                    return self.dim_raise(Value::Null);
                }
                let mut l = self.conv_bytes(&cur)?;
                let mut r = self.conv_bytes(&rhs)?;
                l.append(&mut r);
                let nv = Value::bytes(l);
                // zend emallocs the new zend_string buffer — tracked
                // so the freed old buffer credits back on overwrite.
                if let Value::Str(s) = &nv {
                    // erealloc on an unshared string grows in place
                    // (strong_count==2 = the cell's ref + `cur`).
                    if let Value::Str(o) = &cur {
                        if Rc::strong_count(&o.rc) == 2 {
                            self.mem_grow_str(&o.rc, &s.rc, s.len() as u64 + 25);
                        } else {
                            self.mem_track(&s.rc, s.len() as u64 + 25);
                        }
                    } else {
                        self.mem_track(&s.rc, s.len() as u64 + 25);
                    }
                }
                nv
            }
            "??=" => {
                if matches!(cur, Value::Null) {
                    rhs
                } else {
                    return Ok(cur);
                }
            }
            "&=" | "|=" | "^=" | "<<=" | ">>=" | "**=" => self
                .arith(&op[..op.len() - 1], cur, rhs)
                .or_else(|e| self.arm_binop(e))?,
            _ => {
                return self.fail(PhpError::fatal(
                    format!("unsupported assignment op {}", op),
                    0,
                ))
            }
        };
        // `$t = $GLOBALS` gets a private table: zend materializes
        // $GLOBALS reads into a zval that CoWs away on write, so the
        // copy must not keep the bound global cells or `$t['k']`
        // writes would reach the live globals.
        let mut newv = match newv {
            Value::Array(rc)
                if self
                    .globals_arr
                    .as_ref()
                    .is_some_and(|g| Rc::ptr_eq(&rc, g)) =>
            {
                let a = rc.borrow();
                Value::Array(Rc::new(RefCell::new({
                    let mut pa = PhpArray {
                        entries: a
                            .entries
                            .iter()
                            .map(|(k, c)| (k.clone(), cell(c.borrow().clone())))
                            .collect(),
                        next: a.next,
                        is_ref: false,
                        iter_pos: a.iter_pos,
                        packed: a.packed,
                        foreach_pos: Vec::new(),
                        mem_elems: 0,
                        key_bytes: 0,
                        idx: Default::default(),
                    };
                    crate::value::gc_root_note(1);
                    pa.mem_note_seed();
                    pa
                })))
            }
            v => v,
        };
        match late {
            Late::Prop { ov, name } => {
                let pn = match name {
                    Some(n) => self.prop_name(&n)?,
                    None => self.conv_str(&newv)?,
                };
                newv = self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::PropStr { ov, pn } => {
                newv = self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::Index { base, key, append } => {
                // A clobbered (call-result) dim falls back to the RHS
                // value; a real `[]` always appends (bug21961).
                let aa_obj = {
                    let b = base.borrow();
                    match &*b {
                        Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => Some(o.clone()),
                        _ => None,
                    }
                };
                if let Some(o) = aa_obj {
                    // `$o[k] = v` on ArrayAccess -> offsetSet. A
                    // deferred bare-CV RHS (zend's OP_DATA) reads
                    // before the dispatch.
                    newv = match rhs_late {
                        Some((e, s)) => self.eval_cv_at(e, s)?,
                        None => newv,
                    };
                    let kv = key
                        .as_ref()
                        .map(|kc| kc.borrow().clone())
                        .unwrap_or_else(|| newv.clone());
                    // With a throwable pending the write dispatch dies
                    // on the container itself — zend reports 'Cannot
                    // use object of type C as array', not the defer.
                    // The gate is spl-dispatch, not offsetSet nativity:
                    // a subclass's userland override still routes
                    // through spl's write_dimension (EG(exception)
                    // skips the method call itself below).
                    if self.dim_throw.is_some()
                        && !matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }))
                    {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Cannot use object of type {} as array",
                                o.borrow().class.name()
                            ),
                            self.cur_line,
                        ));
                    }
                    // A pending throwable kills the userland
                    // zend_call_method — spl's own write_dimension
                    // (native stub) still lands; its own throw loses
                    // to the already-pending first one (zend keeps
                    // the earliest EG(exception)).
                    if self.dim_throw.is_none() || self.is_native_offset(&o, "offsetSet") {
                        match self.method_invoke(
                            o,
                            "offsetSet",
                            CallArgs::positional(vec![cell(kv), cell(newv.clone())]),
                        ) {
                            Ok(_) => {}
                            Err(e) => {
                                if self.dim_throw.is_some() {
                                    return self.dim_raise(newv);
                                }
                                return Err(e);
                            }
                        }
                    }
                    return self.dim_raise(newv);
                }
                if matches!(&*base.borrow(), Value::Null) {
                    self.auto_init_gate(&base)?;
                    let mut b = base.borrow_mut();
                    if matches!(*b, Value::Null) {
                        // zend: auto-init HashTable alloc.
                        let rc = Rc::new(RefCell::new(PhpArray::new()));
                        self.mem_track(&rc, Self::ht_req(0, true));
                        *b = Value::Array(rc);
                    }
                }
                if append && matches!(&*base.borrow(), Value::Str(_)) {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "[] operator not supported for strings",
                        self.cur_line,
                    ));
                }
                if needs_read {
                    self.compound_dim_gate(&base, &key)?;
                }
                {
                    let mut b = base.borrow_mut();
                    if let Value::Array(_) = &mut *b {
                        // Shared zend_array: CoW-separate before the
                        // write — an overloaded prop's fetched temp
                        // must not write through into the getter's
                        // backing store (bug32660).
                        self.cow_split(&mut b);
                        let rc = match &*b {
                            Value::Array(rc) => rc.clone(),
                            _ => unreachable!(),
                        };
                        drop(b);
                        // Convert before borrowing the array — the
                        // conversion deprecation's error handler could
                        // not re-borrow it otherwise (B1).
                        let ak = match key.as_ref() {
                            Some(k) => Some(self.dim_arr_key(k)?),
                            None => None,
                        };
                        // zend's OP_DATA read sits between the key
                        // conversion and the store — a deferred
                        // bare-CV RHS evals now.
                        newv = match rhs_late {
                            Some((e, s)) => self.eval_cv_at(e, s)?,
                            None => newv,
                        };
                        let mut arr = rc.borrow_mut();
                        let grew = if append {
                            arr.push(newv.clone());
                            true
                        } else {
                            let key = match ak {
                                Some(k) => k,
                                None => to_key(&newv),
                            };
                            let grew = arr.get_cell(&key).is_none();
                            arr.set(key, newv.clone());
                            grew
                        };
                        if grew {
                            // zend: new bucket (arData capacity
                            // realloc — the OOM request names the
                            // grown table, not the element).
                            let req = Self::ht_req(arr.entries.len(), arr.packed);
                            drop(arr);
                            self.mem_realloc(&rc, req);
                        }
                    }
                }
                if matches!(&*base.borrow(), Value::Str(_)) {
                    // $o->p[k] = v on a string leaf:
                    // zend_check_string_offset key validation then the
                    // byte splice (never array-ified).
                    let mut bytes = match &*base.borrow() {
                        Value::Str(s) => s.to_vec(),
                        _ => Vec::new(),
                    };
                    let kv = key.as_ref().map(|c| c.borrow().clone());
                    let off = self.str_offset_key(kv.as_ref())?;
                    if self.detached_dim {
                        // Dead string — zend bails before the
                        // assign-op refusal and the byte splice.
                        return Ok(Value::Null);
                    }
                    // `??=` writes like `=` once the offset shows
                    // unset — only real compound ops are refused.
                    if op != "=" && op != "??=" {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot use assign-op operators with string offsets",
                            self.cur_line,
                        ));
                    }
                    // zend's OP_DATA read sits between the offset
                    // check and the byte splice.
                    newv = match rhs_late {
                        Some((e, s)) => self.eval_cv_at(e, s)?,
                        None => newv,
                    };
                    if let OffWrite::Stored(byte) = self.str_offset_write(&mut bytes, off, &newv)? {
                        let mut b = base.borrow_mut();
                        if let Value::Str(s) = &mut *b {
                            *s = bytes.into();
                        }
                        return Ok(Value::bytes(vec![byte]));
                    }
                }
                // zend's `=` dies at ASSIGN_DIM's write dispatch on a
                // non-writable leaf — after the OP_DATA read (p_pobj).
                if !self.detached_dim {
                    let m = {
                        let b = base.borrow();
                        match &*b {
                            Value::Object(o) => Some(format!(
                                "Cannot use object of type {} as array",
                                o.borrow().class.name()
                            )),
                            Value::Callable(_) => {
                                Some("Cannot use object of type Closure as array".to_string())
                            }
                            _ => None,
                        }
                    };
                    if let Some(m) = m {
                        if let Some((e, s)) = rhs_late {
                            // OP_DATA reads before the dispatch throws.
                            self.eval_cv_at(e, s)?;
                        }
                        return self.fail(PhpError::uncaught("Error", m, self.cur_line));
                    }
                }
            }
            Late::Static { class, pn } => {
                // A plain `=` on a set-restricted static is 'Cannot
                // modify ... (set)' — the cell path below reports
                // 'indirectly modify' for compound/dim/by-ref writes.
                if !needs_read {
                    if let Expr::StaticProp { .. } = target {
                        if let Ok((cls, _)) = self.member_class_of(&class) {
                            if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                                if let Some(sv) = pd.set_vis {
                                    if self.set_vis_scope_denied(&dcls, sv) {
                                        return self.set_visibility_error(&dcls, &pd.name, sv);
                                    }
                                }
                            }
                        }
                    }
                }
                // `??=`'s null-slot store is zend's plain assign —
                // 'Cannot modify' — while compound/dim writes stay
                // 'indirectly modify'.
                let c = self.static_prop_named_ctx(&class, &pn, needs_read && op != "??=")?;
                // Static prop writes coerce to the declared type like
                // instance props (typed_properties_023).
                if let Ok((cls, _)) = self.member_class_of(&class) {
                    if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                        newv = self.prop_typed_write_check(&pd, &dcls, newv)?;
                    }
                }
                let nv = self.typed_slot_store(&c, newv.clone())?;
                self.cell_store(&c, nv)?;
            }
            Late::Keyed { base, keys } => {
                // A `??=` detached root re-resolves AFTER the RHS — the
                // RHS may have created the var (`$a[0] ??= ($a = [5])`),
                // else this finally materializes it for the write.
                // A handler-rebound container detaches the pending
                // write (zend's refcount sentinel aborts mid-key). For
                // `=`/`+=` the write lands on a scratch holding the
                // op-entry value — invisible, conversions silenced.
                // Checked BEFORE undef_root materializes the var: a
                // still-undef root is bound, and materializing it would
                // masquerade as a handler bind.
                let detached = det.as_ref().is_some_and(|d| self.dim_detached(d));
                let base = match undef_root.take() {
                    Some(n) => self.var_cell(&n),
                    None => base.clone(),
                };
                let mut det = det;
                let (target, silence) = if detached {
                    let d = det.as_ref().unwrap();
                    if d.coalesce {
                        // `??=`'s ASSIGN_DIM is a separate op that
                        // refetches the container and dispatches on the
                        // CURRENT value with its own diagnostics —
                        // except a non-array op-start, whose write
                        // aborts silently (q19a/b, q26a-c).
                        // The op-start type tag survives a dead weak —
                        // match the DimPre variant, not the upgrade.
                        match &d.pre {
                            DimPre::Arr(_)
                            | DimPre::Obj(_)
                            | DimPre::Scalar(Value::Null)
                            | DimPre::Scalar(Value::Bool(false)) => {
                                let cur = self
                                    .var_cell_opt(&d.name)
                                    .map(|c| c.borrow().clone())
                                    .unwrap_or(Value::Null);
                                // Scalar CURRENT throws zend's
                                // container check before the dim
                                // operand is even read.
                                if matches!(
                                    cur,
                                    Value::Int(_) | Value::Float(_) | Value::Bool(true)
                                ) {
                                    return self.fail(PhpError::uncaught(
                                        "Error",
                                        "Cannot use a scalar value as an array",
                                        self.cur_line,
                                    ));
                                }
                                // The assign re-reads CV keys — fresh
                                // binds warn again; arrays land the
                                // write on the live container (visible),
                                // strings/scalars write a scratch.
                                self.dim_cv_bound.clear();
                                self.dim_key_conv.clear();
                                det = Some(DimDetach {
                                    name: d.name.clone(),
                                    pre: DimPre::of(&cur),
                                    pre_cell: 0,
                                    // Stays `coalesce` so the write's
                                    // CV keys re-read + re-warn (the
                                    // ASSIGN_DIM is a second fetch).
                                    coalesce: true,
                                });
                                match cur {
                                    Value::Array(_) | Value::Object(_) => (base.clone(), false),
                                    _ => (cell(cur), false),
                                }
                            }
                            _ => return self.dim_raise(newv),
                        }
                    } else {
                        // zend's `=` on a false container vivifies it
                        // to array with a deprecation — the write
                        // fetch's sentinel then aborts before the dim
                        // operand is read (no further diagnostics).
                        if matches!(&d.pre, DimPre::Scalar(Value::Bool(false))) {
                            self.deprecated_ns(
                                "Automatic conversion of false to array is deprecated",
                            )?;
                            return self.dim_raise(newv);
                        }
                        (d.scratch(), true)
                    }
                } else {
                    (base, false)
                };
                // For ASSIGN_DIM_OP the container dispatch ran at op
                // entry: a non-array start lands in zend's slow path,
                // which gates on the CURRENT container type — strings
                // get 'Cannot use assign-op operators with string
                // offsets', everything else 'Cannot use a scalar value
                // as an array' (even a rebound-TO-array).
                if detached
                    && needs_read
                    && op != "??="
                    && det.as_ref().is_some_and(|d| {
                        !matches!(
                            &d.pre,
                            DimPre::Arr(_)
                                | DimPre::Obj(_)
                                | DimPre::Scalar(Value::Null)
                                | DimPre::Scalar(Value::Bool(false))
                        )
                    })
                {
                    let cur = det
                        .as_ref()
                        .and_then(|d| self.var_cell_opt(&d.name))
                        .map(|c| c.borrow().clone());
                    let m = if matches!(cur, Some(Value::Str(_))) {
                        // `zend_binary_assign_op_dim_slow` runs
                        // `zend_check_string_offset` — its conversion
                        // diagnostics dispatch before the op TypeError.
                        if let Some(kc) = keys.last().and_then(|k| match k {
                            DimArg::Key(c) => Some(c.clone()),
                            DimArg::Cv(n, _) => self.var_cell_opt(n),
                            DimArg::Append => None,
                        }) {
                            let kb = kc.borrow();
                            match &*kb {
                                Value::Bool(_) | Value::Float(_) | Value::Null => {
                                    drop(kb);
                                    self.warn_ns("String offset cast occurred")?;
                                }
                                Value::Str(ks) => match Self::str_off_key(ks) {
                                    StrOffKey::Junk(_) => {
                                        let jn = format!(
                                            "Illegal string offset \"{}\"",
                                            crate::value::lossy(ks)
                                        );
                                        drop(kb);
                                        self.warn(&jn)?;
                                    }
                                    StrOffKey::Bad | StrOffKey::Int(_) => {}
                                },
                                _ => {}
                            }
                        }
                        "Cannot use assign-op operators with string offsets"
                    } else {
                        "Cannot use a scalar value as an array"
                    };
                    return self.fail(PhpError::uncaught("Error", m, self.cur_line));
                }
                // A stale scalar slot takes no dim write — the op
                // vanishes where zend would throw on the live slot.
                if detached
                    && matches!(
                        &*target.borrow(),
                        Value::Int(_) | Value::Float(_) | Value::Bool(_)
                    )
                {
                    return self.dim_raise(newv);
                }
                let was_detached = std::mem::replace(&mut self.detached_dim, silence);
                let r =
                    self.assign_index_path(target, &keys, newv, needs_read, det.as_ref(), rhs_late);
                self.detached_dim = was_detached;
                newv = r?;
            }
            Late::None => match target_cell {
                Some(c) => {
                    let nv = self.typed_slot_store(&c, newv.clone())?;
                    self.cell_store(&c, nv)?;
                }
                None => {
                    // `list()`/`[]` destructure: zend's first FETCH_LIST
                    // op emits at the RHS's post-eval line (later
                    // fetches interleave at each element's own line
                    // — handled inside store()).
                    if matches!(target, Expr::List(_)) {
                        // A pinned CV RHS left CG(zend_lineno) at the
                        // ASSIGN's own line — inner_end_line would
                        // re-read the dropped `argline` mark instead.
                        let l = if pin_rhs {
                            aline
                        } else {
                            Self::inner_end_line(value).unwrap_or(aline)
                        };
                        self.cur_line = l;
                        self.send_line = Some(l);
                    }
                    self.store_rhs(target, newv.clone(), rhs_cell, rhs_nonref_call)?
                }
            },
        }
        self.dim_raise(newv)
    }

    /// Evaluate the object operand of a Prop write target — zend
    /// fetches it in write context, so an intermediate readonly prop
    /// holding a non-object dies with 'Cannot indirectly modify
    /// readonly property' naming the INTERMEDIATE prop (R3 finding 3:
    /// `$c->a['x']->p = 5` gates on `a`). Non-chain forms read normally.
    pub(in crate::interp) fn eval_lvalue_obj(&mut self, obj: &Expr) -> Result<Value, PhpError> {
        match obj {
            Expr::Prop { .. } | Expr::Index { .. } | Expr::StaticProp { .. } | Expr::VarVar(..) => {
                Ok(self.eval_cell(obj)?.borrow().clone())
            }
            Expr::Paren(inner) => match &**inner {
                Expr::Prop { .. }
                | Expr::Index { .. }
                | Expr::StaticProp { .. }
                | Expr::VarVar(..) => Ok(self.eval_cell(inner)?.borrow().clone()),
                Expr::Var(n) => Ok(self
                    .var_lookup(n)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null)),
                _ => self.eval(obj),
            },
            // zend's _W prop/dim fetches resolve an undef CV container to
            // null silently — the access Error ('Attempt to modify
            // property "p" on null') fires without a 'Undefined variable'
            // warn (r($undef->p)).
            Expr::Var(n) => Ok(self
                .var_lookup(n)
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null)),
            _ => self.eval(obj),
        }
    }

    /// Evaluate to a cell (for by-ref semantics): vars and array elements
    /// and object props alias their storage.
    pub(in crate::interp) fn eval_cell(&mut self, e: &Expr) -> Result<Cell, PhpError> {
        match e {
            // `argline` marker: set the line, then keep binding a real
            // cell for the inner shape (by-ref args, list targets).
            Expr::Binary {
                op: "argline",
                l,
                r,
            } => {
                if let Expr::Int(n) = l.as_ref() {
                    self.cur_line = *n as usize;
                    self.send_line = Some(*n as usize);
                }
                self.eval_cell(r)
            }
            // `(...)` parens are transparent for cell binding too —
            // `return (C::$p)` binds the static prop's cell.
            Expr::Paren(inner) => self.eval_cell(inner),
            Expr::Var(n) => Ok(self.var_cell(n)),
            Expr::Index { e, i } => self.index_cell(e, i.as_deref()),
            Expr::Prop {
                obj,
                name,
                nullsafe,
                site,
            } => self.prop_cell(obj, name, *nullsafe, *site),
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                Ok(self.var_cell(&name))
            }
            Expr::StaticProp { class, name } => self.static_prop_cell(class, name),
            Expr::Call { .. }
            | Expr::MethodCall { .. }
            | Expr::StaticCall { .. }
            | Expr::StaticCallDyn { .. } => Ok(self.eval_call_cell(e)?.0),
            _ => {
                // Function calls returning by-ref, etc.: evaluate to temp cell.
                let v = self.eval(e)?;
                Ok(cell(v))
            }
        }
    }

    /// Evaluate a call expression, keeping the callee's returned cell when the
    /// function was declared `&name()` (returns by reference).
    pub(in crate::interp) fn eval_call_cell(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
        let (c, was_ref) = self.eval_call_cell_inner(e)?;
        if was_ref {
            self.mark_ref(&c);
        }
        Ok((c, was_ref))
    }

    fn eval_call_cell_inner(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
        self.last_ret_cell = None;
        self.last_call_by_ref = false;
        let v = self.eval(e)?;
        let declared = self.last_call_by_ref;
        match self.last_ret_cell.take() {
            Some(c) => Ok((c, true)),
            // A function declared `&` that returns a non-variable binds a temp
            // cell — the caller does not warn (the callee warned at `return`).
            None => Ok((cell(v), declared)),
        }
    }

    /// `$target =& $cell`
    /// A `=&` bind displaced this slot's previous cell: Zend decrefs
    /// it only after the new binding is visible, so its __destruct
    /// reads and writes the shared cell (gh10168). A displaced cell
    /// that still has live owners (another prop/static alias) keeps
    /// its zval — Zend frees the zval only with the cell itself, so
    /// `Test::$test =& $box->value` + `$box->value =& $tmp` leaves the
    /// old shared cell alive under the static (assign_prop_ref_with_
    /// prop_ref).
    fn destruct_displaced(&mut self, old: Option<Cell>) -> Result<(), PhpError> {
        if let Some(old) = old {
            // Release dead bookkeeping refs first, then require the
            // cell be truly orphaned — otherwise its zval stays alive.
            self.prune_typed_slot(Rc::as_ptr(&old) as usize);

            if Rc::strong_count(&old) != 1 {
                return Ok(());
            }
            let v = old.borrow().clone();
            drop(old);
            self.destruct_dying_value(&v)?;
        }
        Ok(())
    }

    pub(in crate::interp) fn bind_cell(
        &mut self,
        target: &Expr,
        src: Cell,
    ) -> Result<(), PhpError> {
        // `=&` creates Zend's IS_REFERENCE — writes through it say
        // "a reference held by property", not "property" (034/078).
        self.mark_ref(&src);
        match target {
            Expr::Var(n) => {
                // Global scope: a synced name's $GLOBALS slot must
                // re-point to the bound cell too — the two are one
                // symbol table (030's `$x =& $y` alias survives sync).
                let synced = self.stack.is_empty() && self.globals_synced.contains(n.as_str());
                let old = self.cur().vars.insert(n.clone(), src.clone());
                if synced {
                    if let Some(a) = &self.globals_arr {
                        a.borrow_mut().bind_cell(ArrKey::Str(n.clone().into()), src);
                    }
                }
                self.destruct_displaced(old)
            }
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.cur().vars.insert(name, src);
                Ok(())
            }
            Expr::Paren(inner) => self.bind_cell(inner, src),
            Expr::Index { e, i: _ } => {
                // zend binds each dim op's container before its key —
                // a scalar mid dies 'Cannot use a scalar value as an
                // array' before an undefined CV key warns (m12e).
                let mut dims = Vec::new();
                let mut base = target;
                while let Expr::Index { e: ie, i: ik } = base {
                    dims.push(ik.as_deref());
                    base = ie;
                }
                dims.reverse();
                if !self.in_handler {
                    self.dim_key_conv.clear();
                    self.dim_cv_bound.clear();
                }
                // `=&` can join the bound array into a cycle — register
                // it as a GC node (gc_021's `$a[0] =& $a`), the way the
                // pre-dim-chain bind_cell did.
                self.reg_arr_ref(&src);
                let mut slot = self.eval_cell(base)?;
                // Expression keys evaluate eagerly (zend evals dim
                // exprs at op entry); CV keys bind inside their own
                // dim op — after the container's writability check.
                let last = dims.len() - 1;
                let mut key: Option<Value> = None;
                for (lv, d) in dims.iter().enumerate() {
                    let kc: Option<Cell> = match d {
                        Some(ie) => {
                            let ve = Self::unmark_rhs(ie);
                            match ve {
                                Expr::Var(n2) => {
                                    let bad = {
                                        let b = slot.borrow();
                                        match &*b {
                                            Value::Null
                                            | Value::Array(_)
                                            | Value::Str(_)
                                            | Value::Bool(false) => None,
                                            Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => {
                                                None
                                            }
                                            Value::Object(o) => Some(format!(
                                                "Cannot use object of type {} as array",
                                                o.borrow().class.name()
                                            )),
                                            Value::Callable(_) => Some(
                                                "Cannot use object of type Closure as array"
                                                    .to_string(),
                                            ),
                                            _ => Some(
                                                "Cannot use a scalar value as an array".to_string(),
                                            ),
                                        }
                                    };
                                    if let Some(m) = bad {
                                        return self.fail(PhpError::uncaught(
                                            "Error",
                                            m,
                                            self.cur_line,
                                        ));
                                    }
                                    Some(self.dim_var_key(n2)?)
                                }
                                _ => self.dim_key(ie)?,
                            }
                        }
                        None => None,
                    };
                    if lv == last {
                        key = kc.map(|c| c.borrow().clone());
                    } else {
                        slot = self.index_into_key(slot, kc)?;
                    }
                }
                let gk = if matches!(&**e, Expr::Var(n) if n == "GLOBALS") {
                    match &key {
                        Some(Value::Str(s)) => Some(String::from_utf8_lossy(s).into_owned()),
                        _ => None,
                    }
                } else {
                    None
                };
                let mut b = slot.borrow_mut();
                match &mut *b {
                    Value::Null => {
                        let mut arr = PhpArray::new();
                        match &key {
                            Some(k) => arr.bind_cell(self.arr_key(k)?, src),
                            None => arr.bind_cell(ArrKey::Int(arr.next), src),
                        };
                        // zend: auto-init HashTable alloc.
                        let rc = Rc::new(RefCell::new(arr));
                        self.mem_track(&rc, Self::ht_req(0, true));
                        *b = Value::Array(rc);
                    }
                    Value::Array(_) => {
                        // `$b = $a; $b['k'] =& $r` — bind through a COW
                        // split or the write corrupts the shared table.
                        self.cow_split(&mut b);
                        let rc = match &*b {
                            Value::Array(rc) => rc.clone(),
                            _ => unreachable!(),
                        };
                        let is_globals = self
                            .globals_arr
                            .as_ref()
                            .map(|g| Rc::ptr_eq(g, &rc))
                            .unwrap_or(false);
                        let bk = match &key {
                            Some(k) => Some(self.arr_key(k)?),
                            None => None,
                        };
                        let mut arr = rc.borrow_mut();
                        let old = match bk {
                            Some(ak) => arr.bind_cell(ak, src.clone()),
                            None => {
                                let k = ArrKey::Int(arr.next);
                                arr.bind_cell(k, src.clone())
                            }
                        };
                        if old.is_none() {
                            // zend: new bucket (arData realloc).
                            let req = Self::ht_req(arr.entries.len(), arr.packed);
                            drop(arr);
                            self.mem_realloc(&rc, req);
                        } else {
                            drop(arr);
                        }
                        self.destruct_displaced(old)?;
                        // `$GLOBALS['x'] =& y` — keep the vars table
                        // entry pointed at the bound cell too.
                        if is_globals {
                            if let Some(n) = gk {
                                self.globals.vars.insert(n.clone(), src);
                                self.globals_synced.insert(n);
                            }
                        }
                    }
                    _ => {
                        drop(b);
                        // `=&` onto a non-indexable container — zend's
                        // catchable matrix (probe4a/4e): scalars and
                        // Closures name the generic errors, objects their
                        // class, string offsets split append/str-key/
                        // int-key, ArrayAccess dies with the overloaded
                        // notice after an offsetGet read.
                        let (class, msg) = match &*slot.borrow() {
                            Value::Str(_) => match &key {
                                None => {
                                    ("Error", "[] operator not supported for strings".to_string())
                                }
                                Some(k) => match k {
                                    Value::Str(ks) => match Self::str_off_key(ks) {
                                        StrOffKey::Bad => (
                                            "TypeError",
                                            "Cannot access offset of type string on string"
                                                .to_string(),
                                        ),
                                        StrOffKey::Junk(_) => {
                                            self.warn(&format!(
                                                "Illegal string offset \"{}\"",
                                                crate::value::lossy(ks)
                                            ))?;
                                            (
                                                "Error",
                                                "Cannot create references to/from string offsets"
                                                    .to_string(),
                                            )
                                        }
                                        StrOffKey::Int(_) => (
                                            "Error",
                                            "Cannot create references to/from string offsets"
                                                .to_string(),
                                        ),
                                    },
                                    _ => (
                                        "Error",
                                        "Cannot create references to/from string offsets"
                                            .to_string(),
                                    ),
                                },
                            },
                            Value::Callable(_) => (
                                "Error",
                                "Cannot use object of type Closure as array".to_string(),
                            ),
                            Value::Object(o) => {
                                // spl ArrayObject storage binds `src`
                                // into the named bucket; everything
                                // else — userland ArrayAccess dims and
                                // the unaddressable append form — gets
                                // the offsetGet read + the
                                // indirect-modification notice, then
                                // the assign-by-ref catchable Error
                                // (array_access_012).
                                let spl_arr = if matches!(
                                    o.borrow().internal,
                                    Some(ObjectInternal::ArrayIter { .. })
                                ) {
                                    Some(self.ao_arr(o))
                                } else {
                                    None
                                };
                                let is_spl = spl_arr.is_some();
                                // zend routes `=&` dims on spl classes
                                // through a userland offsetGet override
                                // (the generic overloaded-dim flow);
                                // only the native stub reads/writes the
                                // backing store directly.
                                let off_get = self.find_method_in(&o.borrow().class, "offsetGet");
                                let native_stub = is_spl
                                    && off_get
                                        .as_ref()
                                        .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                                        .unwrap_or(true);
                                if native_stub {
                                    if let (Some(arr), Some(k)) = (spl_arr, &key) {
                                        let ak = to_key(k);
                                        let had = arr.borrow().get_cell(&ak).is_some();
                                        let old = arr.borrow_mut().bind_cell(ak, src);
                                        if !had {
                                            // arData grow charge like
                                            // the plain =& dim paths.
                                            let req = {
                                                let b = arr.borrow();
                                                Self::ht_req(b.entries.len(), b.packed)
                                            };
                                            self.mem_realloc(&arr, req);
                                        }
                                        // A `=&` element can close a
                                        // cycle through the backing
                                        // store — put it in the GC
                                        // universe like the plain `=&`
                                        // binds do (`$ao[0] =& $ao`
                                        // collects 2).
                                        self.arr_handles.push(std::rc::Rc::downgrade(&arr));
                                        return self.destruct_displaced(old);
                                    }
                                }
                                if self.obj_is_a(o, "ArrayAccess") {
                                    let mut res = Ok(Value::Null);
                                    if !native_stub {
                                        // zend fetches the element (the
                                        // `&offsetGet` / overloaded
                                        // notice rule applies) before
                                        // the assign-by-ref catchable
                                        // Error. The engine invokes
                                        // offsetGet (zend_call_method +
                                        // handle_undef_args): spl `[]`
                                        // passes an UNDEF arg, a
                                        // variadic or required param at
                                        // that slot arity-errors as
                                        // `{Decl}::offsetGet():
                                        // Argument #N[ ($x)] not
                                        // passed`.
                                        res = self.aa_offset_get(
                                            o,
                                            key.clone().map(cell),
                                            is_spl,
                                            true,
                                        );
                                    }
                                    // zend's overloaded-dim notice only
                                    // fires when the fetch yielded no
                                    // usable reference — a `&`-declared
                                    // offsetGet leaves a real ref cell
                                    // and the assign-by-ref Error lands
                                    // alone; on an engine error it
                                    // still fires (pending-error
                                    // ordering).
                                    let yielded_ref = !native_stub
                                        && self.last_ret_cell.take().is_some()
                                        && res.is_ok();
                                    if !yielded_ref {
                                        let cn = o.borrow().class.name().to_string();
                                        self.notice(&format!(
                                            "Indirect modification of overloaded element of {} has no effect",
                                            cn
                                        ))?;
                                    }
                                    // `fail` converts an uncaught-kind
                                    // error into a real throwable so
                                    // userland catch(Error) intercepts
                                    // it (raw Err would be fatal).
                                    if let Err(e) = res {
                                        return self.fail(e);
                                    }
                                    return self.fail(PhpError::uncaught(
                                        "Error",
                                        "Cannot assign by reference to an array dimension of an object",
                                        self.cur_line,
                                    ));
                                }
                                (
                                    "Error",
                                    format!(
                                        "Cannot use object of type {} as array",
                                        o.borrow().class.name()
                                    ),
                                )
                            }
                            _ => ("Error", "Cannot use a scalar value as an array".to_string()),
                        };
                        return self.fail(PhpError::uncaught(class, msg, self.cur_line));
                    }
                }
                Ok(())
            }
            Expr::Prop { obj, name, .. } => {
                // `=&` on a hooked prop without `&get` — the engine
                // reports the overloaded-object error, not the
                // indirect-modification one (get_by_ref_auto).
                if let Ok(Value::Object(o)) = self.eval_lvalue_obj(obj) {
                    if let Ok(pn) = self.prop_name(name) {
                        if let Some((_pd, hs)) = self.hooked_prop(&o, &pn) {
                            let has_ref_get = hs
                                .iter()
                                .any(|(h, _)| h.is_get && h.by_ref && h.body.is_some());
                            if !has_ref_get {
                                let v = self.exception(
                                    "Error",
                                    "Cannot assign by reference to overloaded object",
                                );
                                let e = self.throw(v);
                                return self.fail(e);
                            }
                        }
                    }
                }
                let ov = self.eval_lvalue_obj(obj)?;
                if let Value::Object(o) = &ov {
                    if let Ok(pn) = self.prop_name(name) {
                        // `=&` into an overloaded prop (missing slot +
                        // __get) still fetches through __get — then the
                        // indirect-modification notice and the
                        // cannot-assign-by-reference Error (bug32660).
                        {
                            let was_unset = {
                                let ob = o.borrow();
                                ob.unset_props.contains(&pn)
                                    || ob
                                        .unset_props
                                        .iter()
                                        .any(|k| k.ends_with(&format!("\0{}", pn)))
                            };
                            let cls = o.borrow().class.clone();
                            let declared_live = self.decl_prop(o, &pn).is_some()
                                && !was_unset
                                && self.prop_visible(&cls, &pn);
                            let inaccessible = match self.obj_prop_key(o, &pn) {
                                Some(_) => !self.prop_visible(&cls, &pn),
                                None => true,
                            };
                            if inaccessible
                                && !declared_live
                                && self.find_method_in(&cls, "__get").is_some()
                            {
                                let gkey = (Rc::as_ptr(o) as usize, 0u8, pn.clone());
                                if self.magic_guards.insert(gkey.clone()) {
                                    let res = self.method_invoke(
                                        o.clone(),
                                        "__get",
                                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                    );
                                    self.magic_guards.remove(&gkey);
                                    res?;
                                }
                                self.notice(&format!(
                                    "Indirect modification of overloaded property {}::${} has no effect",
                                    cls.name(),
                                    pn
                                ))?;
                                let v = self.exception(
                                    "Error",
                                    "Cannot assign by reference to overloaded object",
                                );
                                let e = self.throw(v);
                                return self.fail(e);
                            }
                        }
                        // `=&` installs the source cell as the prop's
                        // slot itself — later writes through either name
                        // hit the same storage; a missing dynamic prop
                        // materializes a real slot (oss-fuzz-382922236).
                        // The array itself stays unmarked — zend's ref
                        // is a property of the zval, not the array; a
                        // later value-copy still CoW-separates
                        // (bug39775).
                        // Binding a ref into a TYPED prop validates the
                        // source (076/068 conflict); the shared cell
                        // then stays gated through typed_slots (071).
                        // `X =& $v` on a set-restricted prop is an
                        // indirect write — 'Cannot indirectly modify'.
                        if let Some((pd, dcls)) = self.decl_prop(o, &pn) {
                            if let Some(sv) = pd.set_vis {
                                if !self.hook_scope_allows(o, &dcls, &pn, sv) {
                                    return self.set_visibility_indirect_error(&dcls, &pd.name, sv);
                                }
                            }
                        }
                        let mut merged: Option<Vec<String>> = None;
                        let mut owner_ty: Option<Vec<String>> = None;
                        if let Some((pd, dcls)) = self.decl_prop(o, &pn) {
                            if let Some(pt) = &pd.ty {
                                owner_ty = Some(pt.clone());
                                merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                            }
                        }
                        let key = self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                        let mut ob = o.borrow_mut();
                        if !ob.prop_order.contains(&key) {
                            ob.prop_order.push(key.clone());
                        }

                        let old = ob.props.insert(key.clone(), src.clone());
                        drop(ob);
                        if old.is_none() {
                            // zend: a new dynamic-prop bucket allocs.
                            self.mem_track(o, OBJ_SLOT_REQ);
                        }
                        self.destruct_displaced(old)?;
                        if let (Some(m), Some((_, dcls))) = (merged, self.decl_prop(o, &pn)) {
                            let sptr = Rc::as_ptr(&src) as usize;
                            // The FIRST owner is the ref's holder for
                            // "held by property X of type T" messages —
                            // later merges only narrow slot_merged.
                            self.typed_slots.entry(sptr).or_insert_with(|| {
                                (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                            });
                            self.slot_anchor
                                .entry(sptr)
                                .or_insert_with(|| SlotAnchor::Obj(Rc::downgrade(o), key.clone()));
                            self.slot_merged.insert(sptr, m);
                            // Owners keep their *declared* type — a
                            // shared write must satisfy each and yield
                            // one consistent result.
                            let ot = owner_ty.unwrap_or_default();
                            let dcn = dcls.name().to_string();
                            let owners = self.slot_owners.entry(sptr).or_default();
                            if !owners
                                .iter()
                                .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                            {
                                owners.push((
                                    ot,
                                    dcn,
                                    pn.clone(),
                                    SlotAnchor::Obj(Rc::downgrade(o), key.clone()),
                                ));
                            }
                        }
                        return Ok(());
                    }
                }
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
                Ok(())
            }
            Expr::StaticProp { class, name } => {
                let pn = self.prop_name(name)?;
                let (cls, _t) = self.member_class_of(class)?;
                self.statics_init(&cls)?;
                // `X =& $v` on a set-restricted static — 'Cannot
                // indirectly modify'.
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                    if let Some(sv) = pd.set_vis {
                        if self.set_vis_scope_denied(&dcls, sv) {
                            return self.set_visibility_indirect_error(&dcls, &pd.name, sv);
                        }
                    }
                }
                let mut merged: Option<Vec<String>> = None;
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                    if pd.ty.is_some() {
                        // Binding a ref to a typed static validates the
                        // source (catchable TypeError — 068/069); the
                        // cell then stays gated via typed_slots.
                        merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                    }
                }
                // The bound cell lands on the DECLARING class's slot —
                // inherited statics are one shared storage (D::$a =&
                // aliases C::$a).
                let old = match self.find_static_prop_decl(&cls, &pn) {
                    Some((_, dcls)) => dcls.statics.borrow_mut().insert(pn.clone(), src.clone()),
                    None => cls.statics.borrow_mut().insert(pn.clone(), src.clone()),
                };
                self.destruct_displaced(old)?;

                if let (Some(m), Some((pd, dcls))) = (merged, self.find_static_prop_decl(&cls, &pn))
                {
                    let sptr = Rc::as_ptr(&src) as usize;
                    self.typed_slots.entry(sptr).or_insert_with(|| {
                        (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                    });
                    self.slot_anchor.entry(sptr).or_insert_with(|| {
                        SlotAnchor::Statics(dcls.name().to_string(), pn.clone())
                    });
                    self.slot_merged.insert(sptr, m);
                    let ot = pd.ty.clone().unwrap_or_default();
                    let dcn = dcls.name().to_string();
                    let owners = self.slot_owners.entry(sptr).or_default();
                    if !owners
                        .iter()
                        .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                    {
                        owners.push((
                            ot,
                            dcn,
                            pn.clone(),
                            SlotAnchor::Statics(dcls.name().to_string(), pn.clone()),
                        ));
                    }
                }
                Ok(())
            }
            _ => self.fail(PhpError::fatal("Cannot create reference to expression", 0)),
        }
    }

    /// A destructure whose elements include a `&` alias anywhere
    /// (a nested list's `&` element counts — its slots fetch inside
    /// the parent's write-fetch).
    fn list_any_byref(items: &[Option<(Option<Expr>, Expr)>]) -> bool {
        items.iter().flatten().any(|(_, t)| {
            let mut e = t;
            loop {
                e = match e {
                    Expr::Binary {
                        op: "argline", r, ..
                    } => r,
                    Expr::Paren(inner) => inner,
                    _ => break,
                };
            }
            match e {
                Expr::ByRef(_) => true,
                Expr::List(inner) => Self::list_any_byref(inner),
                _ => false,
            }
        })
    }

    /// The innermost `argline` mark on a destructure element — its
    /// first-token line, where zend sites the element's fetch op.
    fn elem_mark_line(t: &Expr) -> Option<usize> {
        let mut e = t;
        let mut mark = None;
        loop {
            e = match e {
                Expr::Binary {
                    op: "argline",
                    l,
                    r,
                    ..
                } => {
                    if let Expr::Int(n) = l.as_ref() {
                        mark = Some(*n as usize);
                    }
                    r
                }
                Expr::Paren(inner) => inner,
                Expr::Binary {
                    op: "listkey", r, ..
                } => r,
                _ => break,
            };
        }
        mark
    }

    /// WRITE-mode slot fetch for a `&` element on a RHS that has no
    /// container cell (eval'd temp) — array slots vivify missing
    /// keys; strings/scalars/objects throw zend's write-fetch
    /// errors. `arrk` sites the slot at the element's position.
    fn list_ref_slot(&mut self, v: &Value, arrk: &ArrKey) -> Result<Cell, PhpError> {
        match v {
            Value::Array(a) => {
                let mut arr = a.borrow_mut();
                Ok(match arr.get_cell(arrk) {
                    Some(c) => c,
                    None => {
                        let c = cell(Value::Null);
                        arr.bind_cell(arrk.clone(), c.clone());
                        c
                    }
                })
            }
            Value::Null => Ok(cell(Value::Null)),
            Value::Str(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot create references to/from string offsets",
                self.cur_line,
            )),
            Value::Object(o) => {
                let cn = o.borrow().class.name().to_string();
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Cannot use object of type {} as array", cn),
                    self.cur_line,
                ))
            }
            Value::Callable(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use object of type Closure as array",
                self.cur_line,
            )),
            _ => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use a scalar value as an array",
                self.cur_line,
            )),
        }
    }

    pub(in crate::interp) fn store(&mut self, target: &Expr, v: Value) -> Result<(), PhpError> {
        self.store_rhs(target, v, None, false)
    }

    /// `store` carrying the eval'd RHS container's live cell (a `&`
    /// destructure write-fetches slots inside it) and whether the
    /// RHS was a call-family expr whose result isn't by-ref (zend
    /// emits 'Attempting to set reference to non referenceable
    /// value' per `&` element, then assigns the plain value).
    fn store_rhs(
        &mut self,
        target: &Expr,
        v: Value,
        container: Option<Cell>,
        rhs_nonref_call: bool,
    ) -> Result<(), PhpError> {
        // Destructure elements keep their `argline` mark — zend sites
        // each element's store op at the element's own line (a nested
        // list keeps the running line; its own elements re-site as
        // they store). Marks can stack (`($x)` adds a paren mark over
        // the element's): peel them all like `unmark_rhs`; the
        // INNERMOST mark is the element's first-token line.
        let mut mark = None;
        let mut target = target;
        loop {
            target = match target {
                Expr::Binary {
                    op: "argline",
                    l,
                    r,
                } => {
                    if let Expr::Int(n) = l.as_ref() {
                        mark = Some(*n as usize);
                    }
                    r.as_ref()
                }
                Expr::Paren(inner) => inner.as_ref(),
                _ => break,
            };
        }
        if let Some(l) = mark {
            if !matches!(target, Expr::List(_)) {
                self.cur_line = l;
                self.send_line = Some(l);
            }
        }
        match target {
            Expr::Var(name) => {
                // `$this = x` is a compile-time fatal in Zend (bug24573).
                if name == "this" {
                    return self.fail(PhpError::fatal("Cannot re-assign $this", 0));
                }
                self.var_set(name, v)?;
                Ok(())
            }
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_set(&name, v)?;
                Ok(())
            }
            Expr::Index { e, i } => {
                // A multi-level target walks zend's per-level
                // FETCH_DIM_W chain — container-before-CV-key at each
                // level, every scalar mid an uncaught scalar-as-array
                // Error (list/foreach dim targets match `=`).
                let mut depth = 1usize;
                let mut b0 = e.as_ref();
                while let Expr::Index { e: ie, .. } = b0 {
                    depth += 1;
                    b0 = ie;
                }
                if depth == 1 {
                    return self.set_index(e, i.as_deref(), v);
                }
                let mut dims = Vec::with_capacity(depth);
                let mut base = target;
                while let Expr::Index { e: ie, i: ik } = base {
                    dims.push(ik.as_deref());
                    base = ie;
                }
                dims.reverse();
                let c = self.eval_cell(base)?;
                let det = match base {
                    Expr::Var(n) => Some(self.dim_detach_var(n, false)),
                    _ => None,
                };
                if !self.in_handler {
                    self.dim_key_conv.clear();
                    self.dim_cv_bound.clear();
                }
                let mut keys = Vec::with_capacity(dims.len());
                for d in dims {
                    match d {
                        Some(ie) => match Self::unmark_rhs(ie) {
                            Expr::Var(n) => keys.push(DimArg::Cv(n.clone(), Self::marked_line(ie))),
                            _ => keys.push(DimArg::from(self.dim_key(ie)?)),
                        },
                        None => keys.push(DimArg::Append),
                    }
                }
                self.assign_index_path(c, &keys, v, false, det.as_ref(), None)
                    .map(|_| ())
            }
            Expr::StaticProp { class, name } => {
                // A plain `=` on a set-visibility-restricted static is
                // 'Cannot modify ... (set)' — gated before the cell
                // fetch (the cell path reports 'indirectly modify').
                let pname0 = self.prop_name(name)?;
                let (cls0, _) = self.member_class_of(class)?;
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls0, &pname0) {
                    if let Some(sv) = pd.set_vis {
                        if self.set_vis_scope_denied(&dcls, sv) {
                            return self.set_visibility_error(&dcls, &pd.name, sv);
                        }
                    }
                }
                let c = self.static_prop_cell(class, name)?;
                let (cls, _) = self.member_class_of(class)?;
                let pname = self.prop_name(name)?;
                // The stored cell may be a `=&`-bound cell shared with a
                // DIFFERENT typed prop — typed_slots gates that write
                // (typed_properties_107).
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pname) {
                    let v2 = self.prop_typed_write_check(&pd, &dcls, v)?;
                    let nv = self.typed_slot_store(&c, v2)?;
                    self.cell_store(&c, nv)?;
                } else {
                    let nv = self.typed_slot_store(&c, v)?;
                    self.cell_store(&c, nv)?;
                }
                Ok(())
            }
            Expr::List(items) => {
                // Zend interleaves FETCH_LIST[dim] + element ASSIGN
                // per slot: an unkeyed element reads [i] positionally
                // (a missing key warns "Undefined array key i", a
                // non-array warns "Cannot use T as array" —
                // engine_assignExecutionOrder_002) at the running op
                // line, then the element's own store re-sites it via
                // the element's mark. A `k => elem` keyed entry reads
                // its declared key instead of the position.
                for (i, slot) in items.iter().enumerate() {
                    let Some((key_e, t)) = slot else { continue };
                    let (arrk, key_val) = match key_e {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            (crate::value::to_key(&kv), Some(kv))
                        }
                        None => (ArrKey::Int(i as i64), Some(Value::Int(i as i64))),
                    };
                    // `&` element: the slot write-fetches inside the
                    // container (auto-vivifies null/missing keys,
                    // string/scalar/object throw zend's errors) and
                    // the element aliases that live cell. A non-ref
                    // call RHS has no cell to give — zend notices
                    // and assigns the fetched value plainly.
                    if let Expr::ByRef(inner) = Self::unmark_rhs(t) {
                        if let Some(l) = Self::elem_mark_line(t) {
                            self.cur_line = l;
                            self.send_line = Some(l);
                        }
                        let inner_u = Self::unmark_rhs(inner);
                        if rhs_nonref_call {
                            self.notice("Attempting to set reference to non referenceable value")?;
                            let vi = match &v {
                                Value::Array(a) => match a.borrow().get(&arrk) {
                                    Some(v) => v,
                                    None => {
                                        let shown = match &arrk {
                                            ArrKey::Int(i) => i.to_string(),
                                            ArrKey::Str(s) => format!("\"{}\"", s),
                                            ArrKey::Tomb => String::new(),
                                        };
                                        self.warn(&format!("Undefined array key {}", shown))?;
                                        Value::Null
                                    }
                                },
                                Value::Null => Value::Null,
                                other => {
                                    self.warn(&format!(
                                        "Cannot use {} as array",
                                        other.debug_type()
                                    ))?;
                                    Value::Null
                                }
                            };
                            self.store(inner, vi)?;
                            continue;
                        }
                        let src = match &container {
                            Some(cc) => {
                                // zend's ref-fetch on a string is
                                // 'Cannot create references to/from
                                // string offsets' — the FETCH_DIM_REF
                                // variant, not the plain write one.
                                let was = std::mem::replace(&mut self.dim_by_ref, true);
                                let r = self.index_into_key(cc.clone(), key_val.clone().map(cell));
                                self.dim_by_ref = was;
                                r?
                            }
                            None => self.list_ref_slot(&v, &arrk)?,
                        };
                        self.bind_cell(inner_u, src)?;
                        continue;
                    }
                    // A nested list carrying `&` elements write-
                    // fetches its own slot (auto-vivifying) so the
                    // inner aliases land in real cells; on a non-ref
                    // call temp that fetch is the same 'Attempting
                    // to set reference' notice.
                    let sub_container = match (&container, Self::unmark_rhs(t)) {
                        (Some(cc), Expr::List(inner)) if Self::list_any_byref(inner) => {
                            Some(self.index_into_key(cc.clone(), key_val.clone().map(cell))?)
                        }
                        _ => None,
                    };
                    if rhs_nonref_call
                        && matches!(
                            Self::unmark_rhs(t),
                            Expr::List(inner) if Self::list_any_byref(inner)
                        )
                    {
                        self.notice("Attempting to set reference to non referenceable value")?;
                    }
                    // The write-fetch vivified the slot — the nested
                    // list reads IT (a fresh auto-viv'd array), not a
                    // read-mode fetch that would warn 'Undefined key'.
                    let vi = match &sub_container {
                        Some(cc) => cc.borrow().clone(),
                        None => match &v {
                            Value::Array(a) => match a.borrow().get(&arrk) {
                                Some(v) => v,
                                None => {
                                    let shown = match &arrk {
                                        ArrKey::Int(i) => i.to_string(),
                                        ArrKey::Str(s) => format!("\"{}\"", s),
                                        ArrKey::Tomb => String::new(),
                                    };
                                    self.warn(&format!("Undefined array key {}", shown))?;
                                    Value::Null
                                }
                            },
                            Value::Null => Value::Null,
                            other => {
                                self.warn(&format!("Cannot use {} as array", other.debug_type()))?;
                                Value::Null
                            }
                        },
                    };
                    self.store_rhs(t, vi, sub_container, rhs_nonref_call)?;
                }
                Ok(())
            }
            Expr::Prop {
                obj,
                name,
                nullsafe: _,
                site,
            } => {
                let pn = self.prop_name(name)?;
                let ov = self.eval_lvalue_obj(obj)?;
                self.cur_line = match name {
                    PropName::Expr(inner) => Self::inner_end_line(inner).unwrap_or(*site),
                    _ => *site,
                };
                self.send_line = Some(self.cur_line);
                self.store_prop(ov, &pn, v).map(|_| ())
            }
            _ => self.fail(PhpError::fatal(
                "Cannot assign to this expression",
                mark.unwrap_or(0),
            )),
        }
    }

    /// Any `&` element in a destructure — nested lists count
    /// (`list(list(&$x))` and `[[$x, &$y]]` bind references too).
    pub(in crate::interp) fn list_has_ref(items: &[Option<(Option<Expr>, Expr)>]) -> bool {
        items
            .iter()
            .flatten()
            .any(|(_, t)| match Self::unmark_rhs(t) {
                Expr::ByRef(_) => true,
                Expr::List(sub) => Self::list_has_ref(sub),
                _ => false,
            })
    }

    /// The `Undefined array key` warning for a missing element —
    /// zend quotes string keys (`"k"`), ints print bare.
    fn list_missing_key(&mut self, key: &ArrKey) -> Result<(), PhpError> {
        match key {
            ArrKey::Str(s) => self.warn(&format!("Undefined array key \"{}\"", s)),
            ArrKey::Int(i) => self.warn(&format!("Undefined array key {}", i)),
            ArrKey::Tomb => Ok(()),
        }
    }

    /// Op-end drain for a deferred dim-fetch throwable — zend checks
    /// EG(exception) once the op's write has landed, so a pending
    /// dim_throw outranks the op's own result value. The thrown value
    /// rode the pair so `pending_exception` is whole again the moment
    /// the throwable actually raises.
    fn dim_raise<T>(&mut self, v: T) -> Result<T, PhpError> {
        match self.dim_throw.take() {
            Some((tv, e, _, _)) => {
                self.pending_exception = Some(tv);
                Err(e)
            }
            None => Ok(v),
        }
    }

    /// zend_binop_error's `if (EG(exception)) return`: an operand-type
    /// TypeError under a pending throwable is swallowed — the binary op
    /// fails, the write never runs, and the armed pair raises in its
    /// place. Division errors throw unconditionally and keep the arm
    /// as their $previous (handled by fail()).
    fn arm_binop(&mut self, e: PhpError) -> Result<Value, PhpError> {
        // The class may ride in lazily (Uncaught) or already
        // materialized on pending_exception (Throw) after fail().
        let type_err = match &e.kind {
            ErrorKind::Uncaught { class } => *class == "TypeError",
            ErrorKind::Throw => matches!(
                &self.pending_exception,
                Some(Value::Object(o)) if o.borrow().class.name() == "TypeError"
            ),
            _ => false,
        };
        if self.dim_throw.is_some() && type_err {
            self.dim_raise(Value::Null)
        } else {
            Err(e)
        }
    }

    /// Op-boundary drain for the armed dim throwable — every `?`/Err
    /// or early `return Ok` between the fetch's defer and `dim_raise`
    /// would strand it for a LATER op to re-raise (or a next op's
    /// entry-clear to silently drop it). zend has a single
    /// EG(exception) slot: an error already unwinding is the newer
    /// throwable and wins; a quiet op end raises the deferred one.
    /// `outer` is the arm a nested op inherited — zend clears
    /// EG(exception) around the nested call, so a clean nested run
    /// restores it for the outer op's own end.
    fn dim_op_outlet<T>(
        &mut self,
        r: Result<T, PhpError>,
        outer: Option<(crate::value::Value, PhpError, bool, bool)>,
    ) -> Result<T, PhpError> {
        match r {
            Err(e) => {
                self.dim_throw = None;
                Err(e)
            }
            Ok(v) => {
                let r = self.dim_raise(v);
                if self.dim_throw.is_none() {
                    self.dim_throw = outer;
                }
                r
            }
        }
    }

    /// `[$a, 'k' => $b, &$c] = $src` / `list(..)` destructuring.
    /// Keyed elements evaluate their key expr to an ArrKey at write
    /// time. `&` elements bind the source's real cells — a missing
    /// key auto-creates `&NULL` like foreach (probe5o) — when the
    /// source is referenceable (`refable`); temps and call results
    /// take zend's 'Attempting to set reference to non referenceable
    /// value' notice per `&` and assign by value (probe5h/5p). A
    /// nested list carrying `&` fetches its intermediate slot by
    /// write-reference: missing/NULL auto-vivifies silently and inner
    /// plain reads stay silent on the fresh array, while
    /// scalars/objects hit zend's 'Cannot use T as array' Errors
    /// (kd_nested*/kd_scalar_inter probes). Non-array sources run
    /// zend's matrix: objects (and Closures) error on ANY element,
    /// scalars/strings warn on plain elements and error on `&`, null
    /// destructures silently (probe5q/5r).
    fn store_list(
        &mut self,
        items: &[Option<(Option<Expr>, Expr)>],
        v: Value,
        refable: bool,
    ) -> Result<Value, PhpError> {
        self.store_list_q(items, v, refable, false)
    }

    /// `quiet` suppresses 'Undefined array key' — set when the source
    /// array was just auto-vivified for a nested `&` list (fresh
    /// arrays are empty; zend stays silent there).
    fn store_list_q(
        &mut self,
        items: &[Option<(Option<Expr>, Expr)>],
        v: Value,
        refable: bool,
        quiet: bool,
    ) -> Result<Value, PhpError> {
        for (i, slot) in items.iter().enumerate() {
            let Some((ke, t)) = slot else { continue };
            // Keyed elements evaluate their key expr; positional
            // elements index by slot.
            let (key, keyv) = match ke {
                Some(ke) => {
                    let kv = self.eval(ke)?;
                    (self.arr_key(&kv)?, kv)
                }
                None => (ArrKey::Int(i as i64), Value::Int(i as i64)),
            };
            let tu = Self::unmark_rhs(t);
            match &v {
                Value::Array(a) => match tu {
                    Expr::ByRef(inner) if refable => {
                        let c = {
                            let mut arr = a.borrow_mut();
                            match arr.get_cell(&key) {
                                Some(c) => c,
                                None => {
                                    let nc = cell(Value::Null);
                                    arr.bind_cell(key.clone(), nc.clone());
                                    nc
                                }
                            }
                        };
                        self.bind_cell(Self::unmark_rhs(inner), c)?;
                    }
                    Expr::ByRef(inner) => {
                        self.notice("Attempting to set reference to non referenceable value")?;
                        let vi = match a.borrow().get(&key) {
                            Some(vi) => vi,
                            None => {
                                if !quiet {
                                    self.list_missing_key(&key)?;
                                }
                                Value::Null
                            }
                        };
                        self.store(inner, vi)?;
                    }
                    Expr::List(sub) if Self::list_has_ref(sub) => {
                        // Intermediate fetch by write-reference:
                        // missing/NULL auto-vivifies to `[]` silently
                        // and inner plain reads stay silent on the
                        // fresh array.
                        enum Inter {
                            Fresh(Rc<RefCell<PhpArray>>),
                            Existing(Rc<RefCell<PhpArray>>),
                            ScalarErr,
                            StrOffsetErr,
                            ObjectErr(String),
                        }
                        let inter = {
                            let mut arr = a.borrow_mut();
                            match arr.get_cell(&key) {
                                Some(c) => {
                                    // The clone drops the cell borrow
                                    // before the Null arm re-borrows.
                                    let cv = c.borrow().clone();
                                    match cv {
                                        Value::Array(rc) => Inter::Existing(rc),
                                        Value::Null => {
                                            let rc = Rc::new(RefCell::new(PhpArray::new()));
                                            *c.borrow_mut() = Value::Array(rc.clone());
                                            Inter::Fresh(rc)
                                        }
                                        Value::Str(_) => Inter::StrOffsetErr,
                                        Value::Object(o) => {
                                            Inter::ObjectErr(o.borrow().class.name().to_string())
                                        }
                                        Value::Callable(_) => {
                                            Inter::ObjectErr("Closure".to_string())
                                        }
                                        _ => Inter::ScalarErr,
                                    }
                                }
                                None => {
                                    let rc = Rc::new(RefCell::new(PhpArray::new()));
                                    arr.set_cell(key.clone(), cell(Value::Array(rc.clone())));
                                    Inter::Fresh(rc)
                                }
                            }
                        };
                        match inter {
                            Inter::Fresh(rc) => {
                                self.store_list_q(sub, Value::Array(rc), refable, true)?;
                            }
                            Inter::Existing(rc) => {
                                self.store_list_q(sub, Value::Array(rc), refable, quiet)?;
                            }
                            Inter::ScalarErr => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    "Cannot use a scalar value as an array",
                                    self.cur_line,
                                ));
                            }
                            Inter::StrOffsetErr => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    "Cannot create references to/from string offsets",
                                    self.cur_line,
                                ));
                            }
                            Inter::ObjectErr(cn) => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!("Cannot use object of type {} as array", cn),
                                    self.cur_line,
                                ));
                            }
                        }
                    }
                    _ => {
                        let vi = match a.borrow().get(&key) {
                            Some(vi) => vi,
                            None => {
                                if !quiet {
                                    self.list_missing_key(&key)?;
                                }
                                Value::Null
                            }
                        };
                        self.store(t, vi)?;
                    }
                },
                // `[$a] = null` is silent (probe5s) — `&` still binds a
                // fresh NULL cell.
                Value::Null => match Self::unmark_rhs(t) {
                    Expr::ByRef(inner) => {
                        self.bind_cell(Self::unmark_rhs(inner), cell(Value::Null))?
                    }
                    _ => self.store(t, Value::Null)?,
                },
                other => {
                    let (inner, by_ref) = match tu {
                        Expr::ByRef(inner) => (inner.as_ref(), true),
                        _ => (t, false),
                    };
                    // ArrayAccess sources read every element through
                    // offsetGet; `&` adds the overloaded notice
                    // (probe5q).
                    if let Value::Object(o) = other {
                        if self.obj_is_a(o, "ArrayAccess") {
                            let iv = self
                                .method_invoke(
                                    o.clone(),
                                    "offsetGet",
                                    CallArgs::positional(vec![cell(keyv)]),
                                )
                                .unwrap_or(Value::Null);
                            if by_ref {
                                let cn = o.borrow().class.name().to_string();
                                self.notice(&format!(
                                    "Indirect modification of overloaded element of {} has no effect",
                                    cn
                                ))?;
                            }
                            self.store(inner, iv)?;
                            continue;
                        }
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Cannot use object of type {} as array",
                                o.borrow().class.name()
                            ),
                            self.cur_line,
                        ));
                    }
                    if let Value::Callable(_) = other {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot use object of type Closure as array",
                            self.cur_line,
                        ));
                    }
                    if by_ref && refable {
                        let msg = if matches!(other, Value::Str(_)) {
                            "Cannot create references to/from string offsets"
                        } else {
                            "Cannot use a scalar value as an array"
                        };
                        return self.fail(PhpError::uncaught(
                            "Error",
                            msg.to_string(),
                            self.cur_line,
                        ));
                    }
                    if by_ref {
                        self.notice("Attempting to set reference to non referenceable value")?;
                    }
                    self.warn(&format!("Cannot use {} as array", other.type_name()))?;
                    self.store(inner, Value::Null)?;
                }
            }
        }
        Ok(v.clone())
    }

    /// Write `$ov->$pn = v` — private-slot, `__set` or dynamic-prop rules.
    /// Write `$ov->$pn = v`; returns the STORED value (typed props
    /// coerce — the assign expr yields the coerced result, 077).
    /// `$cell[] = v` / `$cell[k] = v` on a null typed slot: Zend
    /// auto-initializes an array only when `array` fits the merged
    /// member type; otherwise `Cannot auto-initialize an array inside
    /// ...` TypeError and a just-materialized slot reverts to
    /// uninitialized (typed_properties_083).
    fn auto_init_gate(&mut self, c: &Cell) -> Result<(), PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let gated = self
            .slot_merged
            .get(&ptr)
            .cloned()
            .or_else(|| self.typed_slots.get(&ptr).map(|(_, t, _, _)| t.clone()));
        let Some(tys) = gated else {
            return Ok(());
        };
        if tys.iter().any(|m| self.ty_member_is_a("array", m)) {
            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
            return Ok(());
        }
        let (cn, pn) = self
            .typed_slots
            .get(&ptr)
            .map(|(_, _, n, p)| (n.clone(), p.clone()))
            .unwrap_or_default();
        let where_ = if self.is_ref_ptr(ptr) {
            "a reference held by property"
        } else {
            "property"
        };
        let msg = format!(
            "Cannot auto-initialize an array inside {} {}::${} of type {}",
            where_,
            cn,
            pn,
            ty_disp(&tys)
        );
        // A slot materialized for this write goes back to
        // uninitialized — reads must still raise the uninit Error.
        if self.last_fresh_cell == Some(ptr) {
            if let Some(anc) = self.slot_anchor.get(&ptr).cloned() {
                match anc {
                    SlotAnchor::Obj(w, key) => {
                        if let Some(o) = w.upgrade() {
                            o.borrow_mut().props.remove(&key);
                        }
                    }
                    SlotAnchor::Statics(ccn, ppn) => {
                        if let Some(cc) = self.classes.get(&ccn.to_lowercase()) {
                            cc.statics.borrow_mut().remove(&ppn);
                        }
                    }
                    SlotAnchor::None => {}
                }
            }
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
            self.slot_owners.remove(&ptr);
            self.slot_merged.remove(&ptr);
            self.last_fresh_cell = None;
        }
        let mut e = PhpError::uncaught("TypeError", msg, 0);
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Clone an object: copy prop cells (shared `&`-cells rebind to the
    /// clone's slot owners) and run `__clone` when declared. Shared by
    /// the `clone $o` operator and `clone($o, [...])`.
    pub(crate) fn clone_object(&mut self, o: &Rc<RefCell<PhpObject>>) -> Result<Value, PhpError> {
        let ob = o.borrow();
        let mut props = HashMap::new();
        // References survive clone — the clone's prop
        // shares the same zval and keeps the typed gate
        // (typed_properties_081).
        let mut shared: Vec<(String, Cell)> = Vec::new();
        for (k, c) in ob.props.iter() {
            if self.is_ref_cell(c) && Rc::strong_count(c) > 1 {
                shared.push((k.clone(), c.clone()));
            }
            props.insert(k.clone(), cell(c.borrow().clone()));
        }
        let new_obj = PhpObject {
            class: ob.class.clone(),
            props,
            prop_order: ob.prop_order.clone(),
            id: 0,
            internal: match &ob.internal {
                Some(ObjectInternal::Exception {
                    file,
                    line,
                    trace,
                    thrown,
                    full_msg,
                    eval_ctx,
                    frames,
                    previous,
                }) => Some(ObjectInternal::Exception {
                    file: file.clone(),
                    line: *line,
                    trace: trace.clone(),
                    thrown: *thrown,
                    full_msg: full_msg.clone(),
                    eval_ctx: *eval_ctx,
                    frames: frames.clone(),
                    previous: previous.clone(),
                }),
                _ => None,
            },
            unset_props: ob.unset_props.clone(),
        };
        drop(ob);
        let nv = Value::Object(self.alloc_obj(new_obj));
        // Rebind shared cells into the clone and give it
        // its own slot owner so type checks keep
        // resolving against the clone's prop.
        if let Value::Object(no) = &nv {
            for (k, c) in shared {
                let ptr = Rc::as_ptr(&c) as usize;
                no.borrow_mut().props.insert(k.clone(), c);
                if let Some(a) = self.slot_anchor.get_mut(&ptr) {
                    if let SlotAnchor::Obj(w, sk) = a {
                        if sk == &k && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, o)) {
                            *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                        }
                    }
                }
                if let Some(owners) = self.slot_owners.get_mut(&ptr) {
                    for (_, _, _, a) in owners.iter_mut() {
                        // Repoint owners that anchored
                        // the SOURCE object's prop to
                        // the clone's slot.
                        if let SlotAnchor::Obj(w, sk) = a {
                            if sk == &k && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, o)) {
                                *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                            }
                        }
                    }
                }
            }
        }
        if let Value::Object(no) = &nv {
            let ncls = no.borrow().class.clone();
            if self.find_method_in(&ncls, "__clone").is_some() {
                self.method_invoke(no.clone(), "__clone", CallArgs::empty())?;
            }
        }
        Ok(nv)
    }

    /// The `clone()` builtin's dispatch: objects clone (with optional
    /// with-properties), closures re-clone like the keyword form,
    /// anything else is the arg-1 TypeError.
    pub(crate) fn builtin_clone(
        &mut self,
        v: &Value,
        with: Option<&Rc<RefCell<PhpArray>>>,
    ) -> Result<Value, PhpError> {
        match v {
            Value::Object(o) => self.clone_with(o, with),
            Value::Callable(c) => Ok(Value::Callable(self.new_callable((**c).clone()))),
            other => self.fail(PhpError::uncaught(
                "TypeError",
                format!(
                    "clone(): Argument #1 ($object) must be of type object, {} given",
                    other.debug_type()
                ),
                0,
            )),
        }
    }

    /// `clone($o, ['k' => v, ...])` — PHP 8.5's second arg applies
    /// prop writes AFTER `__clone` runs, through the normal write
    /// path except readonly's init-once gate: a with-write may
    /// overwrite an already-initialized readonly prop, but the
    /// set-visibility scope check still applies ('Cannot modify
    /// protected(set) readonly property ... from global scope').
    pub(crate) fn clone_with(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        with: Option<&Rc<RefCell<PhpArray>>>,
    ) -> Result<Value, PhpError> {
        let nv = self.clone_object(o)?;
        if let (Value::Object(no), Some(w)) = (&nv, with) {
            let entries: Vec<(ArrKey, Value)> = w
                .borrow()
                .iter()
                .map(|(k, c)| (k.clone(), c.borrow().clone()))
                .collect();
            self.clone_write = true;
            let res = (|| {
                for (k, v) in entries {
                    let pn = match k {
                        ArrKey::Int(i) => i.to_string(),
                        ArrKey::Str(s) => s.to_string(),
                        ArrKey::Tomb => continue,
                    };
                    self.store_prop(Value::Object(no.clone()), &pn, v)?;
                }
                Ok(())
            })();
            self.clone_write = false;
            res?;
        }
        Ok(nv)
    }

    /// Dynamic property names starting with `\0` hit zend's
    /// private-name-mangle check — a catchable Error, not magic
    /// (bug52484).
    pub(in crate::interp) fn check_prop_name(&mut self, pn: &str) -> Result<(), PhpError> {
        if pn.starts_with('\0') {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot access property starting with \"\\0\"",
                0,
            ));
        }
        Ok(())
    }

    pub(in crate::interp) fn store_prop(
        &mut self,
        ov: Value,
        pn: &str,
        mut v: Value,
    ) -> Result<Value, PhpError> {
        match ov {
            Value::Object(o) => {
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        {
                            self.hook_write(&o, &pd, &hs, v.clone())?;
                            return Ok(v);
                        }
                    }
                    if let Some((pd, dcls)) = self.decl_prop(&o, pn) {
                        if let Some(sv) = pd.set_vis {
                            if !self.hook_scope_allows(&o, &dcls, pn, sv) {
                                return self.set_visibility_error(&dcls, &pd.name, sv);
                            }
                        }
                    }
                }
                if let Some((pd, dcls)) = self.decl_prop(&o, pn) {
                    if pd.readonly {
                        // readonly implies protected(set): one-time init
                        // from the declaring scope only; later writes
                        // always fail (readonly_property tests). Inside
                        // `clone($o, [...])` with-writes the init-once
                        // gate is lifted — only the scope check below
                        // still applies (R3 finding 13).
                        let key = self.obj_prop_key(&o, pn).unwrap_or_else(|| pn.to_string());
                        if !self.clone_write && o.borrow().props.contains_key(&key) {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Cannot modify readonly property {}::${}",
                                    dcls.name(),
                                    pd.name
                                ),
                                0,
                            ));
                        }
                        let scope = self.caller_scope_name();
                        if scope.as_deref() != Some(dcls.name()) {
                            let from = scope
                                .map(|s| format!("scope {}", s))
                                .unwrap_or_else(|| "global scope".to_string());
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Cannot modify protected(set) readonly property {}::${} from {}",
                                    dcls.name(),
                                    pd.name,
                                    from
                                ),
                                0,
                            ));
                        }
                    }
                    v = self.prop_typed_write_check(&pd, &dcls, v)?;
                }
                // Declared-prop slot resolution: private decls write
                // their mangled `\0C\0p` slot even on first write (the
                // promoted-ctor write reaches here); undeclared names
                // fall through to __set/dynamic.
                let cls = o.borrow().class.clone();
                let k = self.obj_prop_key(&o, pn).or_else(|| {
                    self.decl_prop(&o, pn).map(|(pd, dcls)| {
                        if pd.visibility == crate::ast::Visibility::Private {
                            format!("\0{}\0{}", dcls.name(), pd.name)
                        } else {
                            pd.name.clone()
                        }
                    })
                });
                // A slot the caller can't see is __set territory —
                // zend never writes it directly from an outside scope
                // (overloaded_prop_assign_op_refs).
                let k = match k {
                    Some(k) if self.prop_visible(&cls, pn) => Some(k),
                    _ => None,
                };
                // ARRAY_AS_PROPS: undeclared prop writes go into the
                // storage hash — spl write_property bypasses __set and
                // the prop table for names std doesn't know.
                if k.is_none()
                    && self.aap_active(&o)
                    && self.decl_prop(&o, pn).is_none()
                    && !o.borrow().props.contains_key(pn)
                {
                    // AAP prop writes route through spl_array_write_
                    // dimension in zend — the mid-sort guard applies.
                    if let Some(e) = self.ao_sorting_err(&o) {
                        return self.fail(e);
                    }
                    let arr = self.ao_state(&o).0;
                    let k = ArrKey::Str(Rc::from(pn));
                    // Object-backed: storage IS the prop table — the
                    // write lands a dynamic prop on the backing object.
                    if let Some(src) = self.ao_src_obj(&o) {
                        self.ao_obj_dim_write(&o, &src, &arr, k, v.clone());
                    } else {
                        arr.borrow_mut().set(k, v.clone());
                    }
                    return Ok(v);
                }
                if let Some(k) = k {
                    let mut ob = o.borrow_mut();
                    // Write into the existing slot — a `&`-bound
                    // reference must see the update (typed_properties_010).
                    if let Some(existing) = ob.props.get(&k) {
                        let existing = existing.clone();
                        drop(ob);
                        // The slot may be shared with a DIFFERENT typed
                        // prop via `=&` — that prop's type still gates
                        // the write (typed_properties_062).
                        let nv = self.typed_slot_store(&existing, v)?;
                        self.cell_store(&existing, nv.clone())?;
                        v = nv;
                    } else {
                        // A declared prop that was unset() is
                        // inaccessible — writes go through __set
                        // (typed_properties_magic_set). The in-set
                        // guard writes re-entrant assignments to real
                        // storage instead of recursing (bug63462).
                        let gkey = (Rc::as_ptr(&o) as usize, 1u8, pn.to_string());
                        if ob.unset_props.contains(&k)
                            && self.find_method_in(&cls, "__set").is_some()
                            && self.magic_guards.insert(gkey.clone())
                        {
                            drop(ob);
                            let res = self.method_invoke(
                                o.clone(),
                                "__set",
                                CallArgs::positional(vec![
                                    cell(Value::str(pn.to_string())),
                                    cell(v.clone()),
                                ]),
                            );
                            self.magic_guards.remove(&gkey);
                            res?;
                            return Ok(v);
                        }
                        if !ob.prop_order.contains(&k) {
                            ob.prop_order.push(k.clone());
                        }
                        ob.props.insert(k, cell(v.clone()));
                        drop(ob);
                        // zend: a new declared-slot materialization allocs.
                        self.mem_track(&o, OBJ_SLOT_REQ);
                    }
                    Ok(v)
                } else if self.find_method_in(&cls, "__set").is_some()
                    && self
                        .magic_guards
                        .insert((Rc::as_ptr(&o) as usize, 1u8, pn.to_string()))
                {
                    let res = self.method_invoke(
                        o.clone(),
                        "__set",
                        CallArgs::positional(vec![
                            cell(Value::str(pn.to_string())),
                            cell(v.clone()),
                        ]),
                    );
                    self.magic_guards
                        .remove(&(Rc::as_ptr(&o) as usize, 1u8, pn.to_string()));
                    res?;
                    Ok(v)
                } else {
                    // Writing a DECLARED prop the scope can't see is
                    // `Cannot access private/protected property` — the
                    // declared name can't be shadowed by a dynamic prop
                    // (bug38461's re-entrant __set write; direct writes
                    // without __set too).
                    if let Some(e) = self.hidden_decl_error(&o, pn) {
                        return self.fail(e);
                    }
                    // `\0` names error on the real-storage path before
                    // any deprecation (bug52484_2).
                    self.check_prop_name(pn)?;
                    // E_DEPRECATED on first write to an undeclared prop
                    // (PHP 8.2+; stdClass is exempt).
                    let is_new = {
                        let ob = o.borrow();
                        !ob.props.contains_key(pn)
                    };
                    // stdClass (and its subclasses) plus
                    // #[AllowDynamicProperties] opt out of the
                    // deprecation (property_hooks/foreach_002).
                    let exempt = self.obj_is_a(&o, "stdclass")
                        || cls.decl.attrs.iter().any(|a| {
                            a.name
                                .rsplit('\\')
                                .next()
                                .unwrap_or(&a.name)
                                .eq_ignore_ascii_case("AllowDynamicProperties")
                        });
                    if is_new && !exempt {
                        self.deprecated(&format!(
                            "Creation of dynamic property {}::${} is deprecated",
                            cls.name(),
                            pn
                        ))?;
                    }
                    let mut ob = o.borrow_mut();
                    let pn = pn.to_string();
                    if !ob.prop_order.contains(&pn) {
                        ob.prop_order.push(pn.clone());
                    }
                    ob.props.insert(pn, cell(v.clone()));
                    drop(ob);
                    if is_new {
                        // zend: a new dynamic-prop bucket allocs.
                        self.mem_track(&o, OBJ_SLOT_REQ);
                    }
                    Ok(v)
                }
            }
            Value::Callable(_) => {
                // Closures have no prop storage — writes are a
                // catchable Error, not a dynamic-prop create
                // (closure_022, closure_write_prop).
                let e = PhpError::uncaught(
                    "Error",
                    format!("Cannot create dynamic property Closure::${}", pn),
                    0,
                );
                self.fail(e)
            }
            _ => {
                // `=` on a prop whose base isn't an object — zend throws
                // the assign-verb Error, no auto-viv (probe4h).
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Attempt to assign property \"{}\" on {}",
                        pn,
                        self.zval_type_name(&ov)
                    ),
                    self.cur_line,
                ))
            }
        }
    }

    /// zval-level identity — zend's refcount sentinel aborts the
    /// pending write when the op-start container was freed mid-eval:
    /// a rebind always replaces the Rc (even to equal content — a
    /// rebound 'rebound' string is a new alloc), while `$a=$a` and
    /// `$tmp=$a;$a=$tmp` preserve it (shared zval stays alive). Scalar
    /// containers have no sentinel — equal values stay bound.
    fn same_container(pre: &DimPre, cur: &Value) -> bool {
        match (pre, cur) {
            (DimPre::Arr(w), Value::Array(rc)) => w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, rc)),
            (DimPre::Obj(w), Value::Object(rc)) => w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, rc)),
            (DimPre::Str(w), Value::Str(rc)) => w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, &rc.rc)),
            (DimPre::Callable(w), Value::Callable(rc)) => {
                w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, rc))
            }
            (DimPre::Res(w), Value::Resource(rc)) => {
                w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, rc))
            }
            (DimPre::Scalar(a), b) => {
                matches!(
                    (a, b),
                    (Value::Null, Value::Null)
                        | (Value::Int(_), Value::Int(_))
                        | (Value::Float(_), Value::Float(_))
                        | (Value::Bool(_), Value::Bool(_))
                ) && Self::scalar_eq(a, b)
            }
            _ => false,
        }
    }

    fn scalar_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            _ => false,
        }
    }

    /// Did a diagnostic inside the dim op rebind/destroy the root
    /// container? zend's refcount sentinel: the captured zval's
    /// refcount hits zero → the pending write aborts (silent +
    /// invisible). A destroyed Weak counts as a rebind.
    /// The detach sentinel for a `$var` root: `pre_cell = usize::MAX`
    /// when the var was UNDEF at op entry — zend captured the IS_UNDEF
    /// slot, so a handler that then binds it detaches the pending
    /// write (invisible + conversion-silent, assign_dim_014); a var
    /// still undef at write time stays bound (auto-viv path).
    fn dim_detach_var(&mut self, n: &str, coalesce: bool) -> DimDetach {
        match self.var_cell_opt(n) {
            Some(c) => DimDetach {
                name: n.to_string(),
                pre: DimPre::of(&c.borrow()),
                pre_cell: Rc::as_ptr(&c) as usize,
                coalesce,
            },
            None => DimDetach {
                name: n.to_string(),
                pre: DimPre::Scalar(Value::Null),
                pre_cell: usize::MAX,
                coalesce,
            },
        }
    }

    fn dim_detached(&mut self, det: &DimDetach) -> bool {
        if det.name == "GLOBALS" {
            // $GLOBALS resolves to a fresh wrapper cell per fetch over
            // one immortal backing table — never detached.
            return false;
        }
        match self.var_cell_opt(&det.name) {
            Some(c) => {
                if det.pre_cell == usize::MAX {
                    return true;
                }
                // A non-refcounted op-start slot detaches only when the
                // var cell itself was replaced — auto-viv (Null →
                // Array) and in-place scalar writes mutate the same
                // cell and stay bound.
                if det.pre_cell != 0 && matches!(det.pre, DimPre::Scalar(_)) {
                    return Rc::as_ptr(&c) as usize != det.pre_cell;
                }
                !Self::same_container(&det.pre, &c.borrow())
            }
            None => det.pre_cell != usize::MAX,
        }
    }

    /// `arr_key` through the op's conversion cache — a dim operand's
    /// cast diagnostics (null-offset deprecation, float truncation,
    /// resource-id warn) emit once per assign op even though the
    /// compound read, the write gate and the write itself all convert
    /// it (zend casts the operand once per op). The clone also keeps
    /// no borrow alive across the diagnostic dispatch (B1).
    fn dim_arr_key(&mut self, kc: &Cell) -> Result<ArrKey, PhpError> {
        let p = Rc::as_ptr(kc) as usize;
        if let Some((_, ak)) = self.dim_key_conv.get(&p) {
            return Ok(ak.clone());
        }
        let v = kc.borrow().clone();
        let ak = self.arr_key(&v)?;
        // Keep the cell Rc alive in the cache — a dropped cell's
        // address would be reused and mis-key a later operand's
        // conversion (method_call_variation_001).
        self.dim_key_conv.insert(p, (kc.clone(), ak.clone()));
        Ok(ak)
    }

    /// dim_key's Var arm — a plain-`$var` key binds to its live cell
    /// (post-eval mutations stay visible at write), warning once when
    /// undefined. Pulled out so `DimArg::Cv` binds at its own dim op.
    /// Fetch a dim operand's var cell, bound once per dim op — zend
    /// reads each CV operand once: the gate, the fetch and the write
    /// all reuse that read (one 'Undefined variable' warn). `??=`'s
    /// assign is a separate op — it re-reads (warns again).
    fn dim_var_key(&mut self, n: &str) -> Result<Cell, PhpError> {
        if let Some(c) = self.dim_cv_bound.get(n) {
            return Ok(c.clone());
        }
        let c = match self.var_lookup(n) {
            Some(c) => c,
            None => {
                if !self.is_quiet() {
                    self.warn(&format!("Undefined variable ${}", n))?;
                }
                // The fresh Null binding stays IS_UNDEF in zend —
                // each dim fetch's conversion warns + deprecates over
                // it again (recorded by cell ptr).
                let c = cell(Value::Null);
                self.dim_undef_cells.insert(Rc::as_ptr(&c) as usize);
                c
            }
        };
        self.dim_cv_bound.insert(n.to_string(), c.clone());
        Ok(c)
    }

    /// One FETCH_DIM_IS/?? level on a resolved base: the offset-key
    /// gate, then the array/string/ArrayAccess probe — isset consults
    /// offsetExists alone while ??/empty chain offsetGet (bug31683).
    fn isset_dim_fetch(
        &mut self,
        base: Value,
        key: Value,
        mode: u8,
        last: bool,
    ) -> Result<Option<Value>, PhpError> {
        // ArrayAccess containers see any key type (offsetExists);
        // string bases take the isset/empty/?? dim matrix —
        // composite keys are silent there, never 'on array' Errors.
        if !matches!(&base, Value::Object(o) if self.obj_is_a(o, "ArrayAccess"))
            && !matches!(&base, Value::Str(_))
        {
            self.check_offset_key(&key)?;
        }
        match base {
            Value::Array(a) => Ok(match a.borrow().get(&self.arr_key(&key)?) {
                Some(v) => match v {
                    Value::Null => None,
                    _ => Some(v.clone()),
                },
                None => None,
            }),
            Value::Str(s) => self.str_offset_dim(&s, &key, mode),
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    match self.method_invoke(
                        o.clone(),
                        "offsetExists",
                        CallArgs::positional(vec![cell(key.clone())]),
                    ) {
                        // isset() consults offsetExists alone on the
                        // LAST level — intermediate levels still chain
                        // offsetGet to descend (bug71731); ??/empty
                        // chain offsetGet at every level (bug31683).
                        Ok(v) if v.is_truthy() && mode == 0 && last => Ok(Some(Value::Bool(true))),
                        Ok(v) if v.is_truthy() => {
                            match self.method_invoke(
                                o,
                                "offsetGet",
                                CallArgs::positional(vec![cell(key)]),
                            ) {
                                Ok(v) => Ok(if matches!(v, Value::Null) {
                                    None
                                } else {
                                    Some(v)
                                }),
                                Err(e) => Err(e),
                            }
                        }
                        Ok(_) => Ok(None),
                        Err(e) => Err(e),
                    }
                } else {
                    // `isset($o[k])`/`$o[k] ?? x` on a plain
                    // object still throws zend's catchable
                    // Error — isset doesn't exempt objects.
                    let cn = o.borrow().class.name().to_string();
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot use object of type {} as array", cn),
                        self.cur_line,
                    ))
                }
            }
            _ => Ok(None),
        }
    }

    /// `$arr[$k] = v` / `$arr[] = v`.
    fn set_index(&mut self, e: &Expr, i: Option<&Expr>, v: Value) -> Result<(), PhpError> {
        // zend's ASSIGN_DIM reads the container at op entry — a handler
        // inside the key eval or conversion that rebinds the var
        // detaches the pending write onto the stale slot: invisible,
        // and with the array-key conversions silenced (assign_dim_014).
        // The op-entry snapshot is an O(1) value clone — writes compare
        // zval identity, not cell identity, so $GLOBALS's per-fetch
        // wrapper cells and self-assigns stay bound.
        if !self.in_handler {
            self.dim_key_conv.clear();
            self.dim_cv_bound.clear();
        }
        // A CV key binds inside the write op (zend's ASSIGN_DIM fetches
        // the container operand first, then converts the dim) — a scalar
        // container dies 'Cannot use a scalar value as an array' before
        // the key's 'Undefined variable' even runs.
        let cv_key = match i {
            Some(ie) => match Self::unmark_rhs(ie) {
                Expr::Var(n) => Some(n.clone()),
                _ => None,
            },
            None => None,
        };
        let key = match (&i, &cv_key) {
            (_, Some(_)) => None,
            (Some(ie), None) => Some(self.eval(ie)?),
            (None, _) => None,
        };
        // zend binds the container at op entry — after eager key exprs,
        // before CV binds/conversions: the sentinel snapshot lives here.
        let det = match e {
            Expr::Var(n) => Some(self.dim_detach_var(n, false)),
            _ => None,
        };
        let mut key = key;
        if let Some(n) = &cv_key {
            if let Some(d) = &det {
                let bad = {
                    let bc = self.var_cell_opt(&d.name);
                    let bb = bc.as_ref().map(|c| c.borrow());
                    match bb.as_deref() {
                        // An undef root reads as writable-Null
                        // (auto-viv); it must NOT be created here —
                        // the detach sentinel treats "now exists" as
                        // "handler bound it mid-op".
                        None | Some(Value::Null) | Some(Value::Array(_)) | Some(Value::Str(_)) => {
                            None
                        }
                        Some(Value::Object(o)) if self.obj_is_a(o, "ArrayAccess") => None,
                        Some(Value::Object(o)) => Some(format!(
                            "Cannot use object of type {} as array",
                            o.borrow().class.name()
                        )),
                        Some(Value::Callable(_)) => {
                            Some("Cannot use object of type Closure as array".to_string())
                        }
                        Some(_) => Some("Cannot use a scalar value as an array".to_string()),
                    }
                };
                if let Some(m) = bad {
                    return self.fail(PhpError::uncaught("Error", m, self.cur_line));
                }
            }
            let kc = {
                let _hold = det.as_ref().and_then(|d| d.pre.value());
                self.dim_var_key(n)?
            };
            key = Some(kc.borrow().clone());
        }
        if let Some(d) = &det {
            if let Some(kv) = &key {
                if self.dim_detached(d) {
                    return self.detached_dim_write(d, key, None, v);
                }
                // The key conversion diagnostics run as part of the
                // write op — their error handler can still detach the
                // slot, so convert BEFORE the detach re-check and carry
                // the ArrKey through (the stale write stays silent).
                let converts_early = matches!(
                    d.pre.value().as_ref(),
                    Some(Value::Null | Value::Array(_)) | None
                );
                let ak = if converts_early {
                    Some(self.arr_key(kv)?)
                } else {
                    None
                };
                if self.dim_detached(d) {
                    return self.detached_dim_write(d, key, ak, v);
                }
                return self.set_index_val(e, key, ak, v);
            }
        }
        self.set_index_val(e, key, None, v)
    }

    /// A detached pending dim write: runs its machinery on a scratch
    /// cell holding the op-start container — the store dies there
    /// (invisible) while the write-path diagnostics (string-offset
    /// casts, typed-slot gates) still emit against the stale value.
    fn detached_dim_write(
        &mut self,
        d: &DimDetach,
        key: Option<Value>,
        ak: Option<ArrKey>,
        v: Value,
    ) -> Result<(), PhpError> {
        let scratch = d.scratch();
        let prev = std::mem::replace(&mut self.detached_dim, true);
        let r = self.set_index_var(&scratch, key, ak, v);
        self.detached_dim = prev;
        r
    }

    /// `$e[k1][k2]... = v` with already-evaluated keys, traversed at write
    /// time (Zend ASSIGN_DIM semantics): intermediate scalar levels produce
    /// "Cannot use T as array" warnings; a scalar base for a single-level
    /// write throws "Cannot use a scalar value as an array".
    /// `compound` marks assign-op writes (Zend ASSIGN_DIM_OP): the same
    /// failures then throw catchable `Error`s at EVERY level — scalar
    /// "Cannot use a scalar value as an array", object "Cannot use
    /// object of type C as array" — string containers refuse the op
    /// entirely ("Cannot use assign-op operators with string offsets"
    /// or the `Cannot access offset of type K on string` TypeError for
    /// illegal key types), and array/object keys on an array container
    /// become `Cannot access offset of type K on array` (bug29893).
    /// Returns the effective stored value: string-offset writes return the
    /// byte actually stored, everything else echoes `v` (bug22592: chained
    /// `$a[i] = $a[j] = $s` only warns for the first write).
    fn assign_index_path(
        &mut self,
        mut c: Cell,
        keys: &[DimArg],
        v: Value,
        compound: bool,
        det: Option<&DimDetach>,
        rhs_late: Option<(&Expr, usize)>,
    ) -> Result<Value, PhpError> {
        // zend GC_ADDREFs the op-start container only across each
        // diagnostic dispatch — a handler's in-place dim write then
        // separates instead of mutating the table being walked. The
        // hold is per-diagnostic (a whole-op hold would cow-split
        // every write).
        let last = keys.len() - 1;
        // Each level's container cell — a `=` RHS aliasing an ancestor
        // (`$b[0][0] = $b`) separates it on the final write, zend-style
        // (bug38469).
        let mut anc: Vec<Cell> = Vec::with_capacity(keys.len());
        let mut level_k: Vec<Option<Cell>> = Vec::with_capacity(keys.len());
        for (n, ka) in keys.iter().enumerate() {
            // The CV read sites at its own operand line — the dim op
            // carries the key's marked line.
            if let DimArg::Cv(_, Some(l)) = ka {
                self.cur_line = *l;
                self.send_line = Some(*l);
            }
            // zend's ASSIGN_DIM dispatches on the container BEFORE its
            // dim operand reads: `=` dies on a scalar untouched (the
            // key's warns/conversions never run) and vivifies a false
            // container warning first — a compound op reads the dim
            // before its own gate instead (ASSIGN_DIM_OP checks the
            // container after OP2). Objects and strings defer their
            // Error to the write arm.
            if !self.detached_dim {
                {
                    let _h = det.and_then(|d| d.pre.value());
                    self.dim_prekey_container(&c, compound, det.is_some_and(|d| d.coalesce))?;
                }
                if let Some(d) = det {
                    if !self.detached_dim && self.dim_detached(d) {
                        self.detached_dim = true;
                        if n == 0 {
                            c = d.scratch();
                        }
                    }
                }
            }
            // A deferred plain-$var key binds at ITS dim op: the
            // container's write check runs first (zend's FETCH_DIM_W
            // on a scalar dies before the CV is read), then the bind —
            // whose diagnostics can rebind the root and detach the
            // rest of the write.
            let k: Option<Cell> = match ka {
                DimArg::Append => None,
                DimArg::Key(kc) => {
                    // Conversion diagnostics run before the fetch — a
                    // rebind inside detaches the write entirely (zend
                    // aborts the fetch before touching the table).
                    if !self.detached_dim {
                        {
                            let _h = det.and_then(|d| d.pre.value());
                            // Only an arrish container converts the key
                            // — a Str offset casts through
                            // zend_check_string_offset and objects pass
                            // their dim raw to the write's dispatch.
                            if matches!(
                                &*c.borrow(),
                                Value::Null | Value::Array(_) | Value::Bool(false)
                            ) {
                                let _ = self.dim_arr_key(kc)?;
                            }
                        }
                        if let Some(d) = det {
                            if self.dim_detached(d) {
                                self.detached_dim = true;
                                if n == 0 {
                                    c = d.scratch();
                                }
                            }
                        }
                    }
                    Some(kc.clone())
                }
                DimArg::Cv(name, _ln) => {
                    let kc = {
                        let _h = det.and_then(|d| d.pre.value());
                        self.dim_var_key(name)?
                    };
                    // `??=`'s ASSIGN_DIM is a second dim fetch — zend
                    // converts the key per op, so the conversion
                    // diagnostics (null-offset deprecation, ...) re-fire
                    // on the store; drop the op's dedupe entry.
                    if det.is_some_and(|d| d.coalesce) {
                        self.dim_key_conv.remove(&(Rc::as_ptr(&kc) as usize));
                    }
                    // `??=`'s ASSIGN_DIM is a second dim fetch — a
                    // still-undef CV key warns again (the op re-reads
                    // CV operands per fetch).
                    if det.is_some_and(|d| d.coalesce)
                        && self.dim_undef_cells.contains(&(Rc::as_ptr(&kc) as usize))
                        && !self.is_quiet()
                    {
                        self.warn(&format!("Undefined variable ${name}"))?;
                    }
                    if let Some(d) = det {
                        // The bind's diagnostics may have run a handler
                        // that rebound the root — the rest of the write
                        // lands on the stale op-start container, silent.
                        if !self.detached_dim && self.dim_detached(d) {
                            self.detached_dim = true;
                            if n == 0 {
                                c = d.scratch();
                            }
                        }
                    }
                    Some(kc)
                }
            };
            anc.push(c.clone());
            level_k.push(k.clone());
            // Auto-init gate: writing through a typed slot that is
            // null (or a just-materialized uninit slot) must produce
            // `Cannot auto-initialize an array inside property ...`
            // unless its type accepts an array (typed_properties_083).
            if matches!(&*c.borrow(), Value::Null) {
                self.auto_init_gate(&c)?;
            }
            // `$s[] op= v`: zend's append gate precedes the compound
            // offset gate — `[] operator not supported for strings`
            // wins over the assign-op string-offset check.
            if k.is_none() && matches!(&*c.borrow(), Value::Str(_)) {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "[] operator not supported for strings",
                    self.cur_line,
                ));
            }
            if compound && !self.detached_dim {
                // Container/key checks fire BEFORE the dim write in a
                // compound assign — zend throws at whatever level fails.
                self.compound_dim_gate(&c, &k)?;
            }
            // ArrayAccess object offset path: `$o[k]` dispatches to
            // offsetSet/offsetGet instead of writing through a cell.
            let as_obj = {
                let b = c.borrow();
                match &*b {
                    Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => Some(o.clone()),
                    _ => None,
                }
            };
            // A string dim goes through zend_check_string_offset, not
            // index_into_key: the key validates first (TypeError / []
            // / cast-warning parity), then a non-final dim is the
            // 'Cannot use string offset as an array' Error, else the
            // byte splices in. Compound assigns are refused outright.
            if matches!(*c.borrow(), Value::Str(_)) {
                let kv = k.as_ref().map(|c| c.borrow().clone());
                let off = self.str_offset_key(kv.as_ref())?;
                if n != last {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot use string offset as an array",
                        self.cur_line,
                    ));
                }
                if compound {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot use assign-op operators with string offsets",
                        self.cur_line,
                    ));
                }
                if self.detached_dim {
                    // assign_to_string_offset's GC_DELREF bail: the
                    // separated string died inside the offset's
                    // diagnostics — value checks and the write never
                    // run (str_offset_006's rebound-container NULL).
                    return Ok(Value::Null);
                }
                let mut bytes = {
                    let b = c.borrow();
                    match &*b {
                        Value::Str(s) => s.to_vec(),
                        _ => Vec::new(),
                    }
                };
                // zend's OP_DATA read sits between the offset check
                // and the write — a deferred bare-CV RHS evals now.
                let v = match rhs_late {
                    Some((e, s)) => self.eval_cv_at(e, s)?,
                    None => v,
                };
                match self.str_offset_write(&mut bytes, off, &v)? {
                    OffWrite::Skipped => return Ok(v),
                    OffWrite::Stored(byte) => {
                        let n = bytes.len() as u64 + 25;
                        let rc: Rc<[u8]> = bytes.into();
                        self.mem_track(&rc, n);
                        let mut b = c.borrow_mut();
                        if let Value::Str(s) = &mut *b {
                            *s = PhpStr::adopt(rc);
                        }
                        return Ok(Value::bytes(vec![byte]));
                    }
                }
            }
            // Illegal offset types must not fall into the string-offset
            // fallback — the key Error propagates
            // (closure_array_offset_error). ArrayAccess containers see
            // any key type — offsetSet receives it raw.
            if let Some(kv) = &k {
                if as_obj.is_none() && !self.detached_dim {
                    {
                        let _h = det.and_then(|d| d.pre.value());
                        self.check_offset_key(&kv.borrow())?;
                    }
                    if let Some(d) = det {
                        if !self.detached_dim && self.dim_detached(d) {
                            self.detached_dim = true;
                            if n == 0 {
                                c = d.scratch();
                            }
                        }
                    }
                }
            }
            if let Some(o) = as_obj {
                let kval = k
                    .as_ref()
                    .map(|kc| kc.borrow().clone())
                    .unwrap_or(Value::Null);
                if n == last {
                    let v = match rhs_late {
                        Some((e, s)) => self.eval_cv_at(e, s)?,
                        None => v,
                    };
                    // With a throwable pending the write dispatch dies
                    // on the container itself — zend reports 'Cannot
                    // use object of type C as array', not the defer.
                    if self.dim_throw.is_some()
                        && !matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }))
                    {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Cannot use object of type {} as array",
                                o.borrow().class.name()
                            ),
                            self.cur_line,
                        ));
                    }
                    // A pending throwable kills the userland
                    // zend_call_method — spl's own write_dimension
                    // (native stub) still lands.
                    if self.dim_throw.is_none() || self.is_native_offset(&o, "offsetSet") {
                        match self.method_invoke(
                            o,
                            "offsetSet",
                            CallArgs::positional(vec![cell(kval), cell(v.clone())]),
                        ) {
                            Ok(_) => return Ok(v),
                            Err(e) => {
                                // Under a pending throwable the first
                                // EG(exception) wins — the write's own
                                // throw is suppressed.
                                if self.dim_throw.is_some() {
                                    return Ok(v);
                                }
                                return Err(e);
                            }
                        }
                    }
                    return Ok(v);
                }
                // Internal spl storage (ArrayObject & friends) hands back
                // the real bucket cell — writes through it reach the
                // object, like zend's by-ref spl read_dimension.
                if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) {
                    let arr = self.ao_arr(&o);
                    let k2 = to_key(&kval);
                    let mut a = arr.borrow_mut();
                    c = match a.get_cell(&k2) {
                        Some(cc) => cc,
                        None => {
                            let cc = cell(Value::Null);
                            a.bind_cell(k2, cc.clone());
                            cc
                        }
                    };
                    continue;
                }
                // Userland ArrayAccess — the same write-context
                // read_dimension zend runs: a `&offsetGet` hands the
                // real storage cell back (writes through it reach the
                // object); a value return yields a throwaway cell and
                // notices only when it isn't an object. A pending
                // throwable kills the dispatch — zend reused the cell
                // its own fetch left.
                if self.dim_throw.is_none() {
                    c = self.index_cell_object(&c, k.clone())?;
                }
                continue;
            }
            // OP_DATA belongs to the last dim — intermediate levels
            // never see the deferred RHS.
            let rr = self.index_into_key_last(
                c.clone(),
                k.clone(),
                det,
                if n == last { rhs_late } else { None },
                &anc,
                &level_k,
            );
            match rr {
                Ok((nc, ev)) => {
                    if n == last {
                        // zend's OP_DATA read sits between the fetch
                        // walk and the store — a deferred bare-CV RHS
                        // evals now (index_into_key_last may have run
                        // it early: after key diagnostics, before the
                        // write's separation — bug38469). A detached
                        // fetch aborts before OP_DATA (a handler's
                        // rebind silences the RHS read too) — EXCEPT an
                        // object op-start: zend's detach check only
                        // covers non-object operands, offsetSet still
                        // fires on the stale object.
                        let v = match ev {
                            Some(v) => v,
                            None => match rhs_late {
                                Some((e, s))
                                    if !self.detached_dim
                                        || det
                                            .is_some_and(|d| matches!(&d.pre, DimPre::Obj(_))) =>
                                {
                                    self.eval_cv_at(e, s)?
                                }
                                _ => v,
                            },
                        };
                        // `$ref[k] = v` where the element cell is bound to
                        // a typed prop stays type-gated (064). The store
                        // goes through cell_store so the displaced zval's
                        // destructor runs (a shared cell can alias vars).
                        let nv = self.typed_slot_store(&nc, v.clone())?;
                        // Never panic on a re-entrant borrow — a live
                        // upstream borrow (a handler's write reaching
                        // this slot) drops the write invisibly (B1).
                        if let Ok(mut nb) = nc.try_borrow_mut() {
                            let old = std::mem::replace(&mut *nb, nv);
                            drop(nb);
                            // The displaced zval's destructor runs
                            // (cell_store semantics) — a shared cell can
                            // alias vars.
                            self.destruct_dying_value(&old)?;
                        }
                        return Ok(v);
                    }
                    c = nc;
                }
                Err(e) => {
                    // A catchable throwable raised mid-traversal (a
                    // non-ArrayAccess object, an illegal offset key)
                    // is already the right zend error — propagate.
                    if matches!(e.kind, ErrorKind::Throw) {
                        return Err(e);
                    }
                    let is_str = matches!(*c.borrow(), Value::Str(_));
                    if is_str {
                        // `$s[] = v` — strings have no append (zend
                        // throws a catchable Error).
                        if k.is_none() {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "[] operator not supported for strings",
                                self.cur_line,
                            ));
                        }
                        // String-offset writes split like reads: a key
                        // whose leading int resolves (`'0idx'`) warns
                        // 'Illegal string offset' and writes at that
                        // index; keys with no leading int are the
                        // TypeError (probe4d, bug19943).
                        if let Some(kc) = k.as_ref() {
                            let kb = kc.borrow();
                            match &*kb {
                                Value::Str(ks) => match Self::str_off_key(ks) {
                                    StrOffKey::Bad => {
                                        drop(kb);
                                        return self.fail(PhpError::uncaught(
                                            "TypeError",
                                            "Cannot access offset of type string on string",
                                            self.cur_line,
                                        ));
                                    }
                                    StrOffKey::Junk(_) => {
                                        let m = format!(
                                            "Illegal string offset \"{}\"",
                                            crate::value::lossy(ks)
                                        );
                                        drop(kb);
                                        self.warn(&m)?;
                                    }
                                    StrOffKey::Int(_) => {}
                                },
                                Value::Bool(_) | Value::Float(_) | Value::Null => {
                                    drop(kb);
                                    self.warn_ns("String offset cast occurred")?;
                                }
                                _ => {}
                            }
                        }
                        // String offset write (final level only).
                        let mut b = match c.try_borrow_mut() {
                            Ok(b) => b,
                            Err(_) => return Ok(v),
                        };
                        let mut bytes = match &*b {
                            Value::Str(s) => s.to_vec(),
                            _ => Vec::new(),
                        };
                        if matches!(*b, Value::Str(_)) {
                            // Raw bytes, not conv_str — a byte like \xff
                            // must not round through UTF-8 lossiness.
                            let vs = self.conv_bytes(&v).unwrap_or_default();
                            let byte = vs.first().copied().unwrap_or(b' ');
                            // PHP 8: negative offsets index from the end;
                            // beyond -len stays illegal (bug22592).
                            let idx_i = k
                                .as_ref()
                                .map(|k| k.borrow().to_int())
                                .unwrap_or(bytes.len() as i64);
                            let idx_i = if idx_i < 0 {
                                idx_i + bytes.len() as i64
                            } else {
                                idx_i
                            };
                            if idx_i < 0 {
                                drop(b);
                                let orig =
                                    k.as_ref().map(|k| k.borrow().to_int()).unwrap_or_default();
                                self.warn(&format!("Illegal string offset {}", orig))?;
                                return Ok(v);
                            }
                            let idx = idx_i as usize;
                            if idx >= bytes.len() {
                                bytes.resize(idx + 1, b' ');
                            }
                            bytes[idx] = byte;
                            if vs.len() > 1 {
                                drop(b);
                                self.warn(
                                    "Only the first byte will be assigned to the string offset",
                                )?;
                                b = match c.try_borrow_mut() {
                                    Ok(b) => b,
                                    Err(_) => return Ok(v),
                                };
                            }
                            if let Value::Str(s) = &mut *b {
                                *s = bytes.clone().into();
                            }
                            return Ok(Value::str(String::from_utf8_lossy(&[byte]).into_owned()));
                        }
                        return Ok(v);
                    }
                    // zend throws the same catchable Error at every
                    // level of the write traversal — `$i[a][b] = v` is
                    // not a warning (probe4g vs oracle).
                    let msg = match &*c.borrow() {
                        Value::Callable(_) => {
                            "Cannot use object of type Closure as array".to_string()
                        }
                        _ => "Cannot use a scalar value as an array".to_string(),
                    };
                    return self.fail(PhpError::uncaught("Error", msg, self.cur_line));
                }
            }
        }
        Ok(v)
    }

    /// zend's dim-write dispatches the leaf container BEFORE its dim
    /// operand reads: `=` dies on a scalar untouched (the key's
    /// warns/conversions never run) and a false container warns then
    /// vivifies. A compound op reads the dim before its own gate
    /// (ASSIGN_DIM_OP checks the container after OP2), so `compound`
    /// skips the scalar arm. `??=`'s ASSIGN_DIM is a second fetch —
    /// zend checks the CURRENT container before re-reading the dim
    /// operand, so `coalesce` additionally dies on a non-ArrayAccess
    /// object leaf ahead of the key's second warn/conversion. Objects
    /// under `=` and strings defer their Error to the write arms.
    fn dim_prekey_container(
        &mut self,
        c: &Cell,
        compound: bool,
        coalesce: bool,
    ) -> Result<(), PhpError> {
        let scalar = !matches!(
            &*c.borrow(),
            Value::Null
                | Value::Array(_)
                | Value::Str(_)
                | Value::Bool(false)
                | Value::Object(_)
                | Value::Callable(_)
        );
        if scalar && (!compound || coalesce) {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use a scalar value as an array",
                self.cur_line,
            ));
        }
        if matches!(&*c.borrow(), Value::Bool(false)) {
            self.deprecated_ns("Automatic conversion of false to array is deprecated")?;
            if let Ok(mut b) = c.try_borrow_mut() {
                *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
            }
        }
        if coalesce {
            let bad = {
                let b = c.borrow();
                match &*b {
                    Value::Object(o) if !self.obj_is_a(o, "ArrayAccess") => Some(format!(
                        "Cannot use object of type {} as array",
                        o.borrow().class.name()
                    )),
                    Value::Callable(_) => {
                        Some("Cannot use object of type Closure as array".to_string())
                    }
                    _ => None,
                }
            };
            if let Some(m) = bad {
                return self.fail(PhpError::uncaught("Error", m, self.cur_line));
            }
        }
        Ok(())
    }

    /// Zend's compound-assign write gate (`ASSIGN_DIM_OP`): the
    /// container/key failures that throw catchable Errors before the
    /// dim write — scalar "Cannot use a scalar value as an array",
    /// object "Cannot use object of type C as array", string
    /// "Cannot use assign-op operators with string offsets" (or the
    /// `Cannot access offset of type K on string` TypeError for illegal
    /// key types), and `Cannot access offset of type K on array` for
    /// array/object keys on an array (or to-be-vivified null)
    /// container (bug29893).
    fn compound_dim_gate(&mut self, c: &Cell, k: &Option<Cell>) -> Result<(), PhpError> {
        let bad = {
            let b = c.borrow();
            match &*b {
                Value::Object(o) if !self.obj_is_a(o, "ArrayAccess") => {
                    Some(format!("object of type {}", o.borrow().class.name()))
                }
                Value::Callable(_) => Some("object of type Closure".to_string()),
                Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Resource(_) => {
                    Some("scalar".to_string())
                }
                _ => None,
            }
        };
        if let Some(t) = bad {
            let msg = if t == "scalar" {
                "Cannot use a scalar value as an array".to_string()
            } else {
                format!("Cannot use {} as array", t)
            };
            return self.fail(PhpError::uncaught("Error", msg, self.cur_line));
        }
        if matches!(&*c.borrow(), Value::Str(_)) {
            // `zend_binary_assign_op_dim_slow` runs
            // zend_check_string_offset first — its key-conversion
            // diagnostics (undef/deprecation/cast) fire ahead of the
            // op error itself (q20a oracle).
            match k.as_ref().map(|kv| kv.borrow().clone()) {
                Some(Value::Str(ks)) => match Self::str_off_key(&ks) {
                    StrOffKey::Int(_) => {}
                    StrOffKey::Junk(_) => {
                        self.warn(&format!(
                            "Illegal string offset \"{}\"",
                            crate::value::lossy(&ks)
                        ))?;
                    }
                    StrOffKey::Bad => {
                        return self.fail(PhpError::uncaught(
                            "TypeError",
                            "Cannot access offset of type string on string",
                            self.cur_line,
                        ));
                    }
                },
                Some(v) => {
                    if let Some(kt) = Self::illegal_offset_ty(&v) {
                        return self.fail(PhpError::uncaught(
                            "TypeError",
                            format!("Cannot access offset of type {} on string", kt),
                            self.cur_line,
                        ));
                    }
                    if matches!(v, Value::Bool(_) | Value::Float(_) | Value::Null) {
                        self.warn_ns("String offset cast occurred")?;
                    }
                }
                None => {}
            }
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use assign-op operators with string offsets",
                self.cur_line,
            ));
        }
        // A null container auto-vivifies to an array in the write, so
        // the key check reports `on array`.
        if matches!(&*c.borrow(), Value::Null | Value::Array(_)) {
            if let Some(kt) = k
                .as_ref()
                .and_then(|kc| Self::illegal_offset_ty(&kc.borrow()))
            {
                return self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot access offset of type {} on array", kt),
                    self.cur_line,
                ));
            }
        }
        Ok(())
    }

    /// Zend's compound-assign dim fetch (`ASSIGN_DIM_OP`): descends a
    /// container with already-evaluated keys — missing levels warn
    /// `Undefined array key K` (a null container auto-vivifies, so every
    /// key reports), ArrayAccess reads through offsetGet, and a level
    /// whose write will throw (scalar, string, plain object, illegal
    /// key) yields Null silently — the error comes from the write path
    /// instead (bug29893). `quiet` is `??=`'s isset-style read.
    /// `viv_last`: `++`/`--` fetch the last dim read-write — zend
    /// materializes its miss bucket like an intermediate level's
    /// (assign ops leave the last-dim slot to the write).
    fn compound_dim_read(
        &mut self,
        mut c: Cell,
        keys: &[DimArg],
        quiet: bool,
        det: Option<&DimDetach>,
        viv_last: bool,
    ) -> Result<Value, PhpError> {
        // Hold the op-start container for the traversal — zend's
        // sentinel refcount-adds it across each diagnostic, so a
        // handler's in-place dim write cow-splits instead of mutating
        // the very table this fetch is walking.
        let _hold = det.and_then(|d| d.pre.value());
        // An append has no slot for `??=`'s isset read — zend's compiler
        // rejects `[]` there ("Cannot use [] for reading"). Other compound
        // ops read via ASSIGN_DIM_OP, where appending stays legal.
        if quiet && keys.iter().any(|k| matches!(k, DimArg::Append)) {
            return self.fail(PhpError::compile_fatal(
                "Cannot use [] for reading",
                self.cur_line,
            ));
        }
        for (n, ka) in keys.iter().enumerate() {
            // A diagnostic fired inside this fetch (CV bind, offset
            // conversion) may have rebound the root container — zend's
            // refcount sentinel then aborts the fetch: remaining key
            // conversions and read warnings go silent while the
            // already-fetched cells keep walking the stale graph.
            let detached = det.is_some_and(|d| {
                // A `??=` IS-read's conversions dispatch even after the
                // op-start container died — zend's IS-fetch aborts the
                // slot lookup, not the conversion diagnostics.
                !d.coalesce && self.dim_detached(d)
            });
            if detached {
                self.detached_dim = true;
            }
            enum Step {
                Cell(Cell),
                Missing,
                Stop,
                Done(Value),
            }
            // The fetch-for-write auto-vivifies a null container BEFORE
            // the key is read — an aliased key (`$v[$v]`) then reports
            // the post-viv type (illegal-key TypeError, no warning).
            // `??=`'s isset fetch stays non-mutating. A false container
            // does the same through ASSIGN_DIM_OP's `<= IS_FALSE`
            // dispatch — the deprecation precedes the dim read.
            if !quiet {
                if matches!(&*c.borrow(), Value::Null) {
                    self.auto_init_gate(&c)?;
                    *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                } else if matches!(&*c.borrow(), Value::Bool(false)) {
                    self.deprecated_ns("Automatic conversion of false to array is deprecated")?;
                    if let Ok(mut b) = c.try_borrow_mut() {
                        *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                    }
                }
            }
            let k: Option<Cell> = match ka {
                DimArg::Append => None,
                DimArg::Key(kc) => Some(kc.clone()),
                DimArg::Cv(name, ln) => {
                    // The CV read sites at its own operand line — the
                    // dim op carries the key's marked line.
                    if let Some(l) = ln {
                        self.cur_line = *l;
                        self.send_line = Some(*l);
                    }
                    // The fetch reads its dim operand unconditionally —
                    // zend's ASSIGN_DIM_OP checks the container after
                    // the CV warns (a write-failing level then yields
                    // Null; the write path throws).
                    let kc = self.dim_var_key(name)?;
                    if !self.detached_dim
                        && det.is_some_and(|d| !d.coalesce && self.dim_detached(d))
                    {
                        self.detached_dim = true;
                    }
                    Some(kc)
                }
            };
            let (step, key_v) = {
                let b = c.borrow();
                match &*b {
                    Value::Array(rc) => {
                        // Illegal keys throw in the write — the dim is
                        // never read.
                        if k.as_ref()
                            .is_some_and(|k| Self::illegal_offset_ty(&k.borrow()).is_some())
                        {
                            (Step::Stop, None)
                        } else {
                            let rc = rc.clone();
                            // Conversion diagnostics dispatch the user
                            // error handler — never hold c's borrow
                            // across them: a handler writing this cell
                            // would panic the re-borrow (B1).
                            drop(b);
                            let key = match k.as_ref() {
                                Some(kc) => Some(self.dim_arr_key(kc)?),
                                None => None,
                            };
                            match key {
                                Some(ArrKey::Int(_)) | Some(ArrKey::Str(_)) => {
                                    let key = key.unwrap();
                                    match rc.borrow().get_cell(&key) {
                                        Some(nc) => (Step::Cell(nc), None),
                                        None => (Step::Missing, Some(key)),
                                    }
                                }
                                _ => (Step::Stop, None),
                            }
                        }
                    }
                    Value::Null => {
                        if k.as_ref()
                            .is_some_and(|k| Self::illegal_offset_ty(&k.borrow()).is_some())
                        {
                            // Auto-viv turns the write's container into
                            // an array, which then throws the
                            // illegal-offset TypeError — no read.
                            (Step::Stop, None)
                        } else {
                            drop(b);
                            (
                                Step::Missing,
                                match k.as_ref() {
                                    Some(kc) => Some(self.dim_arr_key(kc)?),
                                    None => None,
                                }
                                .filter(|k| !matches!(k, ArrKey::Tomb)),
                            )
                        }
                    }
                    Value::Str(s) => {
                        // String offsets fetch during compound writes
                        // too: leading-junk keys warn 'Illegal string
                        // offset' and read the resolved index — the op
                        // gate then throws, or `??=`'s isset check sees
                        // the byte and skips the write (bug19943, p12j).
                        let s = s.clone();
                        drop(b);
                        let kv = k.as_ref().map(|kc| kc.borrow().clone());
                        let (idx, junk) = match &kv {
                            Some(Value::Str(ks)) => match Self::str_off_key(ks) {
                                StrOffKey::Int(i) => (Some(i), None),
                                StrOffKey::Junk(i) => (Some(i), Some(crate::value::lossy(ks))),
                                StrOffKey::Bad => (None, None),
                            },
                            Some(Value::Int(i)) => (Some(*i), None),
                            Some(v @ (Value::Bool(_) | Value::Float(_))) => {
                                // The compound op throws next — the
                                // offset cast still warns first.
                                (Some(v.to_int()), Some("".into()))
                            }
                            // Null/undef dims convert to "" — offset 0
                            // silently (isset($s[null]) is quiet).
                            Some(Value::Null) => (Some(0), None),
                            _ => (None, None),
                        };
                        if let Some(jn) = junk {
                            if jn.is_empty() {
                                self.warn_ns("String offset cast occurred")?;
                            } else {
                                self.warn(&format!("Illegal string offset \"{}\"", jn))?;
                            }
                        }
                        match (n + 1 == keys.len(), idx) {
                            (true, Some(i)) => {
                                let bytes: &[u8] = &s[..];
                                let i = if i < 0 { i + bytes.len() as i64 } else { i };
                                let v = if i >= 0 && (i as usize) < bytes.len() {
                                    Value::bytes(bytes[i as usize..i as usize + 1].to_vec())
                                } else {
                                    Value::Null
                                };
                                (Step::Done(v), None)
                            }
                            _ => (Step::Stop, None),
                        }
                    }
                    Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => {
                        let o = o.clone();
                        drop(b);
                        let spl_iter =
                            matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }));

                        if quiet {
                            // `??=` is isset()-based: offsetExists gates
                            // — a hit fetches via offsetGet, a miss is a
                            // Null read so the RHS/write path runs; zend
                            // never calls offsetGet for the check.
                            let kv = k
                                .as_ref()
                                .map(|kc| kc.borrow().clone())
                                .unwrap_or(Value::Null);
                            let exists = self
                                .method_invoke(
                                    o.clone(),
                                    "offsetExists",
                                    CallArgs::positional(vec![cell(kv.clone())]),
                                )
                                .map(|v| v.is_truthy())?;
                            if !exists {
                                return Ok(Value::Null);
                            }
                        }
                        // Op-start storage identity — a thrown handler
                        // may have swapped the table underneath; zend's
                        // read_dimension materializes the miss bucket
                        // onto the table it actually fetched.
                        let storage = if spl_iter {
                            Some(self.ao_storage_id(&o))
                        } else {
                            None
                        };
                        let iv = if let Some(kc) = k {
                            let kv = kc.borrow().clone();
                            match self.method_invoke(
                                o.clone(),
                                "offsetGet",
                                CallArgs::positional(vec![cell(kv.clone())]),
                            ) {
                                Ok(v) => v,
                                // A compound op's throwing offsetGet
                                // defers: EG(exception) is checked at
                                // the opcode boundary, so the rest of
                                // this fetch goes silent, remaining
                                // dims are skipped, and the thrown
                                // value rides the pair until dim_raise
                                // restores pending_exception. spl's
                                // native read still materializes the
                                // miss bucket mechanically — onto the
                                // pre-throw table only (a handler-side
                                // swap leaves it dead; zend writes
                                // there anyway, so skip). `??=`'s isset
                                // read can't decide without the value —
                                // its exception propagates now.
                                Err(e) if !quiet && matches!(e.kind, ErrorKind::Throw) => {
                                    let tv = self.pending_exception.take().unwrap_or(Value::Null);
                                    let live = n + 1 == keys.len();
                                    if (!live || viv_last)
                                        && spl_iter
                                        && self.is_native_offset(&o, "offsetGet")
                                        // zend creates the bucket only
                                        // after the dim converted — an
                                        // illegal key dies at conversion.
                                        && Self::illegal_offset_ty(&kv).is_none()
                                        && storage.is_some_and(|id| self.ao_storage_id(&o) == id)
                                    {
                                        // Conversion diagnostics ran
                                        // inside offsetGet's own arg
                                        // handling — re-key silently.
                                        let was = std::mem::replace(&mut self.detached_dim, true);
                                        let ak = self
                                            .ao_dim_key(&o, &kv)
                                            .unwrap_or_else(|_| to_key(&kv));
                                        self.detached_dim = was;
                                        let arr = self.ao_arr(&o);
                                        match self.ao_src_obj(&o) {
                                            Some(src) => self.ao_obj_dim_write(
                                                &o,
                                                &src,
                                                &arr,
                                                ak,
                                                Value::Null,
                                            ),
                                            None => {
                                                arr.borrow_mut().bind_cell(ak, cell(Value::Null));
                                            }
                                        }
                                    }
                                    // A pure-ArrayAccess dispatch arms
                                    // with `gate`: zend's fetch death
                                    // means the operator never runs —
                                    // the write-gate 'Cannot use object'
                                    // raises it (chained) at dispatch.
                                    self.dim_throw = Some((tv, e, live, !spl_iter));
                                    c = cell(Value::Null);
                                    break;
                                }
                                Err(e) => return Err(e),
                            }
                        } else {
                            // spl `[]` reads through the UNDEF-arg
                            // engine call — its arity error is real
                            // (zend's handle_undef_args), it must not be
                            // swallowed like a missing-bucket miss.
                            self.aa_offset_get(&o, None, spl_iter, true)?
                        };
                        c = cell(iv);
                        continue;
                    }
                    _ => {
                        // zend's ISSET_DIM fetch dies on a non-ArrayAccess
                        // object/Closure itself — a `??=` store never
                        // reaches its RHS or the write's dispatch
                        // (oracle dies inside the quiet read). Scalars
                        // quietly miss and keep walking.
                        let bad = if quiet && det.is_none_or(|d| d.coalesce) {
                            match &*b {
                                Value::Object(o) if !self.obj_is_a(o, "ArrayAccess") => {
                                    Some(format!(
                                        "Cannot use object of type {} as array",
                                        o.borrow().class.name()
                                    ))
                                }
                                Value::Callable(_) => {
                                    Some("Cannot use object of type Closure as array".to_string())
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };
                        match bad {
                            Some(m) => {
                                drop(b);
                                return self.fail(PhpError::uncaught("Error", m, self.cur_line));
                            }
                            None => (Step::Stop, None),
                        }
                    }
                }
            };
            match step {
                Step::Cell(nc) => c = nc,
                Step::Missing => {
                    if !quiet && !self.detached_dim {
                        if let Some(key) = key_v {
                            match key {
                                ArrKey::Str(s) => self.warn(&format!(
                                    "Undefined array key \"{}\"",
                                    crate::value::lossy(&*s)
                                ))?,
                                ArrKey::Int(i) => {
                                    self.warn(&format!("Undefined array key {}", i))?
                                }
                                ArrKey::Tomb => {}
                            }
                        }
                    }
                    // zend's undefined-offset-write sentinel: the
                    // array dying under the miss warn's handler aborts
                    // the whole fetch — the op's remaining reads and
                    // op_data go silent.
                    if det.is_some_and(|d| !d.coalesce && self.dim_detached(d)) {
                        self.detached_dim = true;
                    }
                    c = cell(Value::Null);
                }
                Step::Stop => return Ok(Value::Null),
                Step::Done(v) => return Ok(v),
            }
        }
        Ok(c.borrow().clone())
    }

    /// A dim key evaluates eagerly (zend orders dims before the RHS)
    /// but a variable key keeps its cell — the write lands on the key's
    /// CURRENT value, so an auto-vivified container that shares a name
    /// with its key (`$v[$v] -= 1`) throws the illegal-key TypeError.
    fn dim_key(&mut self, ie: &Expr) -> Result<Option<Cell>, PhpError> {
        // Parsed dim keys arrive `argline`-wrapped (parens nest inside):
        // diagnostics site at the key's own marked line, and CV/VarVar
        // classification must see through the wrap.
        if let Some(l) = Self::marked_line(ie) {
            self.cur_line = l;
            self.send_line = Some(l);
        }
        let ie = Self::unmark_rhs(ie);
        match ie {
            Expr::Var(n) => match self.var_lookup(n) {
                Some(c) => Ok(Some(c)),
                None => {
                    if !self.is_quiet() {
                        self.warn(&format!("Undefined variable ${}", n))?;
                    }
                    Ok(Some(cell(Value::Null)))
                }
            },
            Expr::VarVar(inner, _) => {
                let v = self.eval(inner)?;
                let name = self.conv_str(&v)?;
                match self.var_lookup(&name) {
                    Some(c) => Ok(Some(c)),
                    None => {
                        if !self.is_quiet() {
                            self.warn(&format!("Undefined variable ${}", name))?;
                        }
                        Ok(Some(cell(Value::Null)))
                    }
                }
            }
            _ => self.eval(ie).map(|v| Some(cell(v))),
        }
    }

    /// Zend type name for keys that are illegal array offsets — the
    /// `Cannot access offset of type K on T` TypeError wording.
    fn illegal_offset_ty(v: &Value) -> Option<String> {
        match v {
            Value::Array(_) => Some("array".to_string()),
            Value::Object(o) => Some(o.borrow().class.name().to_string()),
            Value::Callable(_) => Some("Closure".to_string()),
            _ => None,
        }
    }

    /// A string-offset key classifies like zend's offset reads: a full
    /// int indexes cleanly, leading-numeric junk (`'0idx'`) warns
    /// 'Illegal string offset' and still indexes, and anything else is
    /// the illegal-offset TypeError (bug19943).
    fn str_off_key(b: &[u8]) -> StrOffKey {
        let mut i = usize::from(b.first() == Some(&b'-'));
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == b.len() && i > start {
            StrOffKey::Int(crate::value::lossy(b).parse().unwrap_or(0))
        } else if i > start && matches!(numeric(b), Numeric::Leading(_, _)) {
            StrOffKey::Junk(crate::value::lossy(&b[..i]).parse().unwrap_or(0))
        } else {
            StrOffKey::Bad
        }
    }

    /// `$a[$k]` keys: array/object/closure keys are a catchable
    /// TypeError naming the type (closure_array_key_error/offset_error).
    fn check_offset_key(&mut self, v: &Value) -> Result<(), PhpError> {
        if let Some(tn) = Self::illegal_offset_ty(v) {
            return self.fail(PhpError::uncaught(
                "TypeError",
                format!("Cannot access offset of type {} on array", tn),
                0,
            ));
        }
        Ok(())
    }

    /// `$a[$k]` key coercion — zend runs the same offset cast at every
    /// implicit array-key site (reads, writes, isset, unset, literals,
    /// SPL dims): array/object keys are a TypeError ('Cannot access
    /// offset of type K on array'), an in-range fractional float
    /// truncates with Deprecated, an out-of-range float warns 'not
    /// representable' (NaN emits both), null is Deprecated, and a
    /// resource warns and casts to its id int.
    pub(crate) fn arr_key(&mut self, v: &Value) -> Result<ArrKey, PhpError> {
        // A detached dim write (its container slot was rebound by an
        // error handler inside the key eval) lands on the stale slot —
        // zend converts the key without diagnostics (assign_dim_014).
        if self.detached_dim {
            return Ok(to_key(v));
        }
        match v {
            Value::Array(_) | Value::Object(_) | Value::Callable(_) => {
                let tn = Self::illegal_offset_ty(v).unwrap();
                self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot access offset of type {} on array", tn),
                    0,
                ))
            }
            Value::Float(f) => {
                if !f.is_finite() || *f >= i64::MAX as f64 || *f < i64::MIN as f64 {
                    let mut werr = None;
                    let i = coerce_float(*f, |m| {
                        if let Err(e) = self.warn_ns(m) {
                            werr = Some(e);
                        }
                    });
                    if let Some(e) = werr {
                        return Err(e);
                    }
                    // NaN is the only non-finite zend also flags for
                    // precision loss ('Implicit conversion from float
                    // NAN to int loses precision').
                    if f.is_nan() {
                        self.deprecated_ns(&format!(
                            "Implicit conversion from float {} to int loses precision",
                            Value::Float(*f).to_php_string()
                        ))?;
                    }
                    return Ok(ArrKey::Int(i));
                }
                if f.fract() != 0.0 {
                    self.deprecated_ns(&format!(
                        "Implicit conversion from float {} to int loses precision",
                        Value::Float(*f).to_php_string()
                    ))?;
                }
                Ok(to_key(v))
            }
            Value::Null => {
                // `unset()` maps null offsets to "" silently — the
                // deprecation is only for read/write fetches. An
                // IS_UNDEF key converts like null (zend's undef arm
                // falls through to the IS_NULL deprecation).
                if !self.unset_ctx {
                    self.deprecated_ns(
                        "Using null as an array offset is deprecated, use an empty string instead",
                    )?;
                }
                Ok(to_key(v))
            }
            Value::Resource(r) => {
                let id = r.borrow().id();
                self.warn_ns(&format!(
                    "Resource ID#{} used as offset, casting to integer ({})",
                    id, id
                ))?;
                Ok(ArrKey::Int(id as i64))
            }
            _ => Ok(to_key(v)),
        }
    }

    /// `$cell[$key] = v` write into a var cell — shared by the plain
    /// `Expr::Var` path and detached dim writes (a scratch cell stands
    /// in for a container slot an error handler rebound mid-key-eval).
    fn set_index_var(
        &mut self,
        arr_cell: &Cell,
        key: Option<Value>,
        ak: Option<ArrKey>,
        v: Value,
    ) -> Result<(), PhpError> {
        // Null/Array slots: convert the key BEFORE taking the write
        // borrow — the conversion's diagnostics dispatch the user
        // handler and a handler re-entering this slot could not
        // re-borrow it (B1).
        if matches!(&*arr_cell.borrow(), Value::Null | Value::Array(_)) {
            let ak: Option<ArrKey> = match ak {
                Some(ak) => Some(ak),
                None => match &key {
                    Some(k) => Some(self.arr_key(k)?),
                    None => None,
                },
            };
            let mut b = match arr_cell.try_borrow_mut() {
                Ok(b) => b,
                // A live upstream borrow (a handler's nested write
                // reached this slot) drops the write invisibly — never
                // panic on a re-entrant borrow.
                Err(_) => return Ok(()),
            };
            match &mut *b {
                Value::Null => {
                    let mut arr = PhpArray::new();
                    match ak {
                        Some(ak) => arr.set(ak, v),
                        None => arr.push(v),
                    }
                    // zend: auto-init HashTable alloc.
                    let rc = Rc::new(RefCell::new(arr));
                    self.mem_track(&rc, Self::ht_req(0, true));
                    *b = Value::Array(rc);
                }
                Value::Array(_) => {
                    // CoW: shared arrays get replaced wholesale by
                    // callers through the cell, so mutate in place via
                    // borrow_mut — PHP separates unreferenced copies;
                    // our Rc aliases share.
                    self.cow_split(&mut b);
                    let rc = match &*b {
                        Value::Array(rc) => rc.clone(),
                        _ => unreachable!(),
                    };
                    // Drop the container borrow before mutating: an
                    // element cell can alias this very cell
                    // ($a = [&$a]) and the write-through needs it
                    // unborrowed.
                    drop(b);
                    let mut arr = rc.borrow_mut();
                    let grew = match ak {
                        Some(ak) => {
                            let grew = arr.get_cell(&ak).is_none();
                            arr.set(ak, v);
                            grew
                        }
                        None => {
                            arr.push(v);
                            true
                        }
                    };
                    if grew {
                        // zend: bucket + amortized arData growth —
                        // the OOM 'tried' names the realloc request,
                        // not the element.
                        let req = Self::ht_req(arr.entries.len(), arr.packed);
                        drop(arr);
                        self.mem_realloc(&rc, req);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        let mut b = match arr_cell.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        };
        match &mut *b {
            Value::Str(s) => {
                let mut bytes = s.to_vec();
                drop(b);
                // zend_check_string_offset validates the key first —
                // bad keys throw 'on string' TypeErrors or the []
                // Error before the byte splice runs.
                let off = self.str_offset_key(key.as_ref())?;
                // Dead separated string — bail after the key
                // diagnostics like zend's GC_DELREF check.
                if !self.detached_dim {
                    if let OffWrite::Stored(_) = self.str_offset_write(&mut bytes, off, &v)? {
                        let nv = Value::str(String::from_utf8_lossy(&bytes).into_owned());
                        if let Value::Str(s) = &nv {
                            self.mem_track(&s.rc, s.len() as u64 + 25);
                        }
                        *arr_cell.borrow_mut() = nv;
                    }
                }
            }
            _ => {
                drop(b);
                // Catchable Error: `$int[0] = x` (engine_..._002).
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use a scalar value as an array",
                    self.cur_line,
                ));
            }
        }
        Ok(())
    }

    /// zend_check_string_offset — validates a key used to offset into a
    /// string: int passes through; a numeric string (leading ws/sign
    /// ok) uses its value silently, a leading-int-with-junk string
    /// warns 'Illegal string offset' and uses the int part; float,
    /// bool and null cast with a 'String offset cast occurred'
    /// warning; other types (incl. non-numeric strings) are a
    /// TypeError naming the type *on string*. `None` (the `[]` dim)
    /// is the '[] operator not supported for strings' Error.
    fn str_offset_key(&mut self, k: Option<&Value>) -> Result<i64, PhpError> {
        match k {
            None => self.fail(PhpError::uncaught(
                "Error",
                "[] operator not supported for strings",
                self.cur_line,
            )),
            Some(Value::Int(i)) => Ok(*i),
            Some(Value::Str(s)) => match numeric(s) {
                Numeric::Int(i) => Ok(i),
                Numeric::Leading(f, true) => {
                    let orig = crate::value::lossy(s).into_owned();
                    self.warn(&format!("Illegal string offset \"{}\"", orig))?;
                    Ok(f as i64)
                }
                _ => self.fail(PhpError::uncaught(
                    "TypeError",
                    "Cannot access offset of type string on string",
                    self.cur_line,
                )),
            },
            Some(v @ (Value::Float(_) | Value::Bool(_) | Value::Null)) => {
                self.warn("String offset cast occurred")?;
                Ok(v.to_int())
            }
            Some(v) => {
                let ty = match v {
                    Value::Array(_) => "array".to_string(),
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Callable(_) => "Closure".to_string(),
                    _ => "resource".to_string(),
                };
                self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot access offset of type {} on string", ty),
                    self.cur_line,
                ))
            }
        }
    }

    /// isset/empty/?? on a string offset. zend_isset_dim_slow and
    /// zend_isempty_dim_slow accept a LONG dim, a scalar (floats go
    /// through zval_get_long_ex — the lossy-precision deprecation still
    /// fires) or a fully int-numeric string; anything else is silently
    /// not-set/empty — composite keys included. The ?? value read
    /// (FETCH_DIM_IS) instead warns 'Illegal string offset' on
    /// leading-junk numerics, silently casts scalars, yields NULL for
    /// non-numeric strings, and still raises 'on string' TypeErrors for
    /// composite keys (zend_illegal_string_offset runs with BP_VAR_R).
    /// Negative offsets wrap from the end in both paths.
    fn str_offset_dim(
        &mut self,
        s: &crate::value::PhpStr,
        key: &Value,
        mode: u8,
    ) -> Result<Option<Value>, PhpError> {
        let len = s.len() as i64;
        let off = if mode == 2 {
            match key {
                Value::Int(i) => *i,
                Value::Str(str) => match numeric(str) {
                    Numeric::Int(i) => i,
                    Numeric::Leading(f, true) => {
                        let orig = crate::value::lossy(str).into_owned();
                        self.warn(&format!("Illegal string offset \"{}\"", orig))?;
                        f as i64
                    }
                    // Non-numeric string keys are NULL, not an error.
                    _ => return Ok(None),
                },
                // Scalars cast silently — no 'String offset cast
                // occurred' warning and no lossy-float deprecation.
                Value::Float(_) | Value::Bool(_) | Value::Null => key.to_int(),
                _ => {
                    let ty = match key {
                        Value::Array(_) => "array".to_string(),
                        Value::Object(o) => o.borrow().class.name().to_string(),
                        Value::Callable(_) => "Closure".to_string(),
                        _ => "resource".to_string(),
                    };
                    return self.fail(PhpError::uncaught(
                        "TypeError",
                        format!("Cannot access offset of type {} on string", ty),
                        self.cur_line,
                    ));
                }
            }
        } else {
            match key {
                Value::Int(i) => *i,
                Value::Float(f) => {
                    // zval_get_long_ex(is_strict): fractional floats warn.
                    if f.fract() != 0.0 {
                        self.deprecated(&format!(
                            "Implicit conversion from float {} to int loses precision",
                            format_float_repr(*f)
                        ))?;
                    }
                    *f as i64
                }
                Value::Bool(_) | Value::Null => key.to_int(),
                Value::Str(str) => match numeric(str) {
                    Numeric::Int(i) => i,
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        };
        // Negative offsets wrap from the end (isset($s[-1]) → last
        // byte); still-out-of-range is not-set in every mode.
        let idx = if off < 0 {
            off.checked_add(len).unwrap_or(i64::MIN)
        } else {
            off
        };
        if idx >= 0 && idx < len {
            Ok(Some(Value::bytes(vec![s[idx as usize]])))
        } else {
            Ok(None)
        }
    }

    /// The zend string-offset byte splice on an already-validated
    /// offset: below -len warns 'Illegal string offset' and writes
    /// nothing; the value converts byte-faithfully — empty is the
    /// 'Cannot assign an empty string to a string offset' Error and a
    /// multi-byte value warns 'Only the first byte...' then writes
    /// byte 0. Past-the-end offsets space-pad.
    fn str_offset_write(
        &mut self,
        s: &mut Vec<u8>,
        off: i64,
        v: &Value,
    ) -> Result<OffWrite, PhpError> {
        let len = s.len() as i64;
        // The bounds check runs before the value checks — $s[-9] = ''
        // warns Illegal only, no empty-string Error (oracle-verified).
        if off < -len {
            self.warn(&format!("Illegal string offset {}", off))?;
            return Ok(OffWrite::Skipped);
        }
        let vs = self.conv_bytes(v)?;
        if vs.is_empty() {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot assign an empty string to a string offset",
                self.cur_line,
            ));
        }
        if vs.len() > 1 {
            self.warn("Only the first byte will be assigned to the string offset")?;
        }
        let idx = if off < 0 { off + len } else { off } as usize;
        if idx >= s.len() {
            s.resize(idx + 1, b' ');
        }
        s[idx] = vs[0];
        Ok(OffWrite::Stored(vs[0]))
    }

    /// `set_index` with an already-evaluated key; `ak` is the
    /// pre-converted ArrKey when conversion ran before a detach check.
    fn set_index_val(
        &mut self,
        e: &Expr,
        key: Option<Value>,
        ak: Option<ArrKey>,
        v: Value,
    ) -> Result<(), PhpError> {
        if let Some(k) = &key {
            // ArrayAccess containers take any key type — zend hands it
            // to offsetSet untouched (SplObjectStorage keys on objects).
            let obj_container = match e {
                Expr::Var(n) => matches!(
                    self.var_cell_opt(n).map(|c| c.borrow().clone()),
                    Some(Value::Object(o)) if self.obj_is_a(&o, "ArrayAccess")
                ),
                _ => false,
            };
            // A string container validates keys via
            // zend_check_string_offset ('on string' errors), and nested
            // containers through index_into_key's arm — the generic
            // check would misname the error 'on array'.
            let self_validating = match e {
                Expr::Var(n) => matches!(
                    self.var_cell_opt(n).map(|c| c.borrow().clone()),
                    Some(Value::Str(_))
                ),
                _ => true,
            };
            if !obj_container && !self_validating {
                self.check_offset_key(k)?;
            }
        }
        match e {
            Expr::Var(name) => {
                let arr_cell = self.var_cell(name);
                self.set_index_var(&arr_cell, key, ak, v)
            }
            // Nested lvalue bases ($a[0][1], $a->b[0], C::$a[0], $$v[0]):
            // resolve the element cell generically and write into it.
            Expr::Index { .. } | Expr::Prop { .. } | Expr::StaticProp { .. } | Expr::VarVar(..) => {
                match self.index_cell_key(e, key.clone()) {
                    Ok(c) => {
                        if let Ok(mut cb) = c.try_borrow_mut() {
                            *cb = v;
                        }
                        Ok(())
                    }
                    // String offsets can't be cells — splice the byte in place.
                    Err(e2) => match self.eval_cell(e) {
                        Ok(bc) if matches!(*bc.borrow(), Value::Str(_)) => {
                            // e's own dims may have consumed a string
                            // offset mid-path ($a['k'][0]['j']): the
                            // intermediate 'Cannot use string offset as
                            // an array' is the zend diagnostic — key
                            // never re-validates.
                            let inner_str = match e {
                                Expr::Index { e: inner, .. } => self
                                    .eval_cell(inner)
                                    .map(|ic| matches!(*ic.borrow(), Value::Str(_)))
                                    .unwrap_or(false),
                                _ => false,
                            };
                            if inner_str {
                                return Err(e2);
                            }
                            let mut bytes = match &*bc.borrow() {
                                Value::Str(s) => s.to_vec(),
                                _ => Vec::new(),
                            };
                            let off = self.str_offset_key(key.as_ref())?;
                            if !self.detached_dim {
                                if let OffWrite::Stored(_) =
                                    self.str_offset_write(&mut bytes, off, &v)?
                                {
                                    let mut b = bc.borrow_mut();
                                    if let Value::Str(s) = &mut *b {
                                        *s = bytes.into();
                                    }
                                }
                            }
                            Ok(())
                        }
                        // ArrayAccess object: `$o[k] = v` -> offsetSet.
                        Ok(bc)
                            if matches!(*bc.borrow(), Value::Object(ref o)
                                if self.obj_is_a(o, "ArrayAccess")) =>
                        {
                            let o = match &*bc.borrow() {
                                Value::Object(o) => o.clone(),
                                _ => unreachable!(),
                            };
                            let kv = key.clone().unwrap_or(Value::Null);
                            match self.method_invoke(
                                o,
                                "offsetSet",
                                CallArgs::positional(vec![cell(kv), cell(v)]),
                            ) {
                                Ok(_) => Ok(()),
                                Err(e) => Err(e),
                            }
                        }
                        // Nested dim on a scalar is a Warning, not an Error
                        // (engine_assignExecutionOrder_002) — write is skipped.
                        Ok(bc) => {
                            let t = bc.borrow().type_name().to_string();
                            drop(bc);
                            self.warn(&format!("Cannot use {} as array", t))?;
                            Ok(())
                        }

                        _ => Err(e2),
                    },
                }
            }
            _ => self.fail(PhpError::fatal("Cannot use expression as array", 0)),
        }
    }

    /// Writable array handle for by-ref builtin args: PHP COW-separates
    /// a shared array at the callee boundary, so a builtin mutating
    /// `&$array` replaces the caller's slot with a private copy while
    /// other variables keep the old contents. is_ref arrays (true `=&`
    /// bindings) write through to every alias instead.
    pub fn arr_mut(&self, c: &Cell) -> Option<Rc<RefCell<PhpArray>>> {
        let mut b = c.borrow_mut();
        if let Value::Array(rc) = &mut *b {
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = self.dup_array(&rc.borrow());
                *b = Value::Array(Rc::new(RefCell::new(fresh)));
            }
            if let Value::Array(rc) = &*b {
                return Some(rc.clone());
            }
        }
        None
    }

    /// PHP copy-on-write separation: a shared zend_array is replaced
    /// by a fresh table on write. IS_REFERENCE elements (cells bound
    /// by `=&`/by-ref constructs, tracked in `ref_cells`) stay shared
    /// with the source — everything else copies by value.
    pub(in crate::interp) fn cow_split(&self, v: &mut Value) {
        if let Value::Array(rc) = v {
            // Deliberately-shared tables ($GLOBALS, &-bound storage,
            // arrays under a live by-ref foreach) never separate.
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = self.dup_array(&rc.borrow());
                *v = Value::Array(Rc::new(RefCell::new(fresh)));
            }
        }
    }

    /// The separated copy for CoW / array-copy contexts (`=`, exchange
    /// values): ref-marked cells are re-bound, the rest duplicated.
    pub fn dup_array(&self, a: &PhpArray) -> PhpArray {
        let mut copy = PhpArray {
            entries: Vec::with_capacity(a.entries.len()),
            next: a.next,
            is_ref: a.is_ref,
            iter_pos: a.iter_pos,
            foreach_pos: Vec::new(),
            packed: a.packed,
            mem_elems: 0,
            key_bytes: 0,
            idx: Default::default(),
        };
        crate::value::gc_root_note(1);
        for (k, c) in &a.entries {
            // zend unwraps a refcount-1 IS_REFERENCE bucket on copy;
            // only cells still aliased elsewhere re-bind (a stale
            // mark from a dead foreach/binding copies by value).
            let nc = if self.is_ref_cell(c) && Rc::strong_count(c) > 1 {
                c.clone()
            } else {
                cell(c.borrow().clone())
            };
            copy.mem_note_key(k);
            copy.entries.push((k.clone(), nc));
            copy.mem_note_append();
        }
        copy
    }

    /// Index into `c`'s array value, taking a cell for `key`/`[]`.
    fn index_into_key(&mut self, c: Cell, key: Option<Cell>) -> Result<Cell, PhpError> {
        if let Some(kc) = &key {
            // Objects route to index_cell_object (ArrayAccess accepts
            // any key); string containers validate keys via their own
            // arm ('on string' errors); only true arrays reject object
            // keys here.
            if !matches!(&*c.borrow(), Value::Object(_) | Value::Str(_)) && !self.detached_dim {
                self.check_offset_key(&kc.borrow())?;
            }
        }
        let mut b = match c.try_borrow_mut() {
            Ok(b) => b,
            // A live upstream borrow (a handler's nested write reached
            // this cell mid-read) never panics — the pending write
            // lands on a dead scratch cell instead (B1).
            Err(_) => return Ok(cell(Value::Null)),
        };
        // zend auto-vivifies a false container to array on dim write
        // with a deprecation (null vivifies silently).
        if matches!(*b, Value::Bool(false)) {
            drop(b);
            self.deprecated_ns("Automatic conversion of false to array is deprecated")?;
            b = match c.try_borrow_mut() {
                Ok(b) => b,
                Err(_) => return Ok(cell(Value::Null)),
            };
        }
        if matches!(*b, Value::Null | Value::Bool(false)) {
            // zend: auto-init HashTable alloc.
            let rc = Rc::new(RefCell::new(PhpArray::new()));
            self.mem_track(&rc, Self::ht_req(0, true));
            *b = Value::Array(rc);
        }
        if let Value::Array(_) = &mut *b {
            // Deliberately-shared arrays ($GLOBALS, &-bound storage)
            // keep their bound cells through the split (ref_cells);
            // an ordinary shared zend_array still separates on write
            // (bug32660).
            self.cow_split(&mut b);
            let rc = match &*b {
                Value::Array(rc) => rc.clone(),
                _ => unreachable!(),
            };
            drop(b);
            let key = match key {
                Some(kc) => self.dim_arr_key(&kc)?,
                None => {
                    let mut arr = rc.borrow_mut();
                    let k = ArrKey::Int(arr.next);
                    let c = cell(Value::Null);
                    arr.bind_cell(k.clone(), c.clone());
                    // zend: new bucket growth.
                    let req = Self::ht_req(arr.entries.len(), arr.packed);
                    drop(arr);
                    self.mem_realloc(&rc, req);
                    return Ok(c);
                }
            };
            let mut arr = rc.borrow_mut();
            match arr.get_cell(&key) {
                Some(c) => Ok(c),
                None => {
                    let c = cell(Value::Null);
                    arr.bind_cell(key, c.clone());
                    let req = Self::ht_req(arr.entries.len(), arr.packed);
                    drop(arr);
                    self.mem_realloc(&rc, req);
                    Ok(c)
                }
            }
        } else if matches!(*b, Value::Object(_) | Value::Callable(_)) {
            let cn = match &*b {
                Value::Object(o) => o.borrow().class.name().to_string(),
                _ => "Closure".to_string(),
            };
            let is_ao = matches!(&*b, Value::Object(o) if self.obj_is_a(o, "ArrayAccess"));
            drop(b);
            if is_ao {
                self.index_cell_object(&c, key)
            } else {
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Cannot use object of type {} as array", cn),
                    self.cur_line,
                ))
            }
        } else {
            let is_str = matches!(*b, Value::Str(_));
            drop(b);
            if is_str {
                // zend_check_string_offset validates the key first —
                // bad keys throw 'on string' TypeErrors or the []
                // Error; a valid offset on a non-final dim errors —
                // 'as an array' on write, 'Cannot create references'
                // in a =& context (FETCH_DIM_REF).
                let kv = key.as_ref().map(|c| c.borrow().clone());
                self.str_offset_key(kv.as_ref())?;
                self.fail(PhpError::uncaught(
                    "Error",
                    if self.dim_by_ref {
                        "Cannot create references to/from string offsets"
                    } else {
                        "Cannot use string offset as an array"
                    },
                    self.cur_line,
                ))
            } else {
                self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use a scalar value as an array",
                    self.cur_line,
                ))
            }
        }
    }

    /// Last-level dim of a `=` write, same contract as index_into_key
    /// plus the deferred OP_DATA read: zend runs it after the dim's
    /// key-side diagnostics (offset validation, conversion) and the
    /// container's vivify diagnostics, but BEFORE the write's
    /// separation — the read's extra ref is what makes `$a[0] = $a`
    /// store a snapshot of the pre-write table (bug38469). A rebind
    /// inside the read detaches: the write lands on the stale operand,
    /// invisible (probe_rebind_rhs). Non-ArrayAccess objects are the
    /// one container whose 'as array' Error follows the read; scalars
    /// and string-offset checks die at fetch, before it (probe_rhs_err).
    /// Returns the slot cell and the materialized RHS, if it ran.
    /// `anc`/`level_k` carry the op's walked ancestor cells and keys —
    /// a RHS whose table aliases an ancestor separates it (bug38469).
    fn index_into_key_last(
        &mut self,
        c: Cell,
        key: Option<Cell>,
        det: Option<&DimDetach>,
        rhs: Option<(&Expr, usize)>,
        anc: &[Cell],
        level_k: &[Option<Cell>],
    ) -> Result<(Cell, Option<Value>), PhpError> {
        let Some((e, s)) = rhs else {
            return self.index_into_key(c, key).map(|c| (c, None));
        };
        if self.detached_dim {
            // Detached non-object roots abort before OP_DATA — the
            // deferred read never runs.
            return self.index_into_key(c, key).map(|c| (c, None));
        }
        // The borrow must end before `c` moves into the delegate calls.
        enum Shape {
            ObjPlain,
            Callable,
            Arrish,
            Other,
        }
        let shape = {
            let b = c.borrow();
            match &*b {
                Value::Object(o) if !self.obj_is_a(o, "ArrayAccess") => Shape::ObjPlain,
                Value::Callable(_) => Shape::Callable,
                Value::Array(_) | Value::Null | Value::Bool(false) => Shape::Arrish,
                _ => Shape::Other,
            }
        };
        match shape {
            Shape::ObjPlain | Shape::Callable => {
                // 'Cannot use object as array' is a write-phase Error —
                // OP_DATA reads first.
                let v = self.eval_cv_at(e, s)?;
                self.index_into_key(c, key).map(|cc| (cc, Some(v)))
            }
            Shape::Arrish => {
                // Mirrors index_into_key's pre-separation sequence:
                // offset-key validation, the false→array deprecation,
                // vivify, key conversion — each ahead of OP_DATA.
                if let Some(kc) = &key {
                    self.check_offset_key(&kc.borrow())?;
                }
                let mut b = match c.try_borrow_mut() {
                    Ok(b) => b,
                    Err(_) => return Ok((cell(Value::Null), None)),
                };
                if matches!(*b, Value::Bool(false)) {
                    drop(b);
                    self.deprecated_ns("Automatic conversion of false to array is deprecated")?;
                    b = match c.try_borrow_mut() {
                        Ok(b) => b,
                        Err(_) => return Ok((cell(Value::Null), None)),
                    };
                }
                if matches!(*b, Value::Null | Value::Bool(false)) {
                    let rc = Rc::new(RefCell::new(PhpArray::new()));
                    self.mem_track(&rc, Self::ht_req(0, true));
                    *b = Value::Array(rc);
                }
                let stale_rc = match &*b {
                    Value::Array(r) => r.clone(),
                    _ => unreachable!(),
                };
                drop(b);
                if let Some(kc) = &key {
                    // The dim_arr_key cache dedupes the inner call.
                    self.dim_arr_key(kc)?;
                }
                let v = self.eval_cv_at(e, s)?;
                let rebound = !matches!(&*c.borrow(), Value::Array(r) if Rc::ptr_eq(r, &stale_rc))
                    || det.is_some_and(|d| self.dim_detached(d));
                if rebound {
                    // Land the write on the stale table — invisible to
                    // the var, still real for other aliases of it.
                    let ak = match &key {
                        Some(kc) => Some(self.dim_arr_key(kc)?),
                        None => None,
                    };
                    let mut arr = stale_rc.borrow_mut();
                    let cc = match ak {
                        Some(ak) => match arr.get_cell(&ak) {
                            Some(cc) => cc,
                            None => {
                                let cc = cell(Value::Null);
                                arr.bind_cell(ak, cc.clone());
                                cc
                            }
                        },
                        None => {
                            let nx = arr.next;
                            let cc = cell(Value::Null);
                            arr.bind_cell(ArrKey::Int(nx), cc.clone());
                            cc
                        }
                    };
                    drop(arr);
                    return Ok((cc, Some(v)));
                }
                // Our sentinel hold is over — drop it or the inner
                // cow_split counts it as a real share and separates
                // (dup_array) on EVERY deferred-RHS write: O(N) per
                // append → the `$a[]=$v` loop goes quadratic.
                drop(stale_rc);
                // `$b[0][0] = $b`: op_data's read shares an ancestor's
                // table — the write separates it, and every deeper
                // fetch re-resolves on the fresh copies (each inner
                // table shared with the old one separates in turn), so
                // the snapshot stays free of this bind (bug38469). The
                // last level itself is the normal path — its own
                // cow_split sees the inflated ref.
                if let Value::Array(vr) = &v {
                    if let Some(pos) = anc.iter().position(
                        |ac| matches!(&*ac.borrow(), Value::Array(r) if Rc::ptr_eq(r, vr)),
                    ) {
                        if pos < anc.len() - 1 {
                            self.cow_split(&mut anc[pos].borrow_mut());
                            let mut c2 = anc[pos].clone();
                            for kj in level_k.iter().skip(pos) {
                                c2 = self.index_into_key(c2, kj.clone())?;
                            }
                            return Ok((c2, Some(v)));
                        }
                    }
                }
                // The read's ref is now live on the table — the inner
                // cow_split sees it and separates.
                self.index_into_key(c, key).map(|cc| (cc, Some(v)))
            }
            // Scalars/strings die at fetch before OP_DATA.
            Shape::Other => self.index_into_key(c, key).map(|cc| (cc, None)),
        }
    }

    /// Native-stub spl offsetGet (empty decl body, engine-declared) —
    /// zend's own read_dimension machinery handles it; a userland
    /// override takes the engine-call path instead.
    /// Native-stub spl offset method — a deferred throwable suppresses
    /// userland zend_call_method but not zend's own read/write_dimension.
    fn is_native_offset(&mut self, o: &Rc<RefCell<PhpObject>>, name: &str) -> bool {
        self.find_method_in(&o.borrow().class, name)
            .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
            .unwrap_or(true)
    }

    /// Write-context cell fetch on an ArrayAccess object (`$x =&
    /// $o['k']`, `foreach (&$o['k'])`): zend's spl read_dimension is
    /// by-ref, so `&offsetGet` hands back the storage cell through
    /// last_ret_cell. A value-returning offsetGet yields a throwaway
    /// cell — plain `=` writes route through offsetSet elsewhere.
    fn index_cell_object(&mut self, c: &Cell, key: Option<Cell>) -> Result<Cell, PhpError> {
        let Value::Object(o) = c.borrow().clone() else {
            unreachable!()
        };
        // `Cannot use object of type C as array` (probe4b/4d); zend
        // never looks for offsetGet on a plain object.
        if !self.obj_is_a(&o, "ArrayAccess") {
            return self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Cannot use object of type {} as array",
                    o.borrow().class.name()
                ),
                self.cur_line,
            ));
        }
        let spl_iter = matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }));
        // A native-stub spl offsetGet returns the store's own cell —
        // a named `&$o['k']` binds the bucket silently (zend's
        // spl_array_read_dimension_ex never notices on this path; only
        // `[]` falls to the overloaded-dim notice + Error below).
        if spl_iter {
            if let Some(kc) = &key {
                if self.is_native_offset(&o, "offsetGet") {
                    let arr = self.ao_arr(&o);
                    let ak = self.arr_key(&kc.borrow())?;
                    // `=&` binds the bucket itself — zend marks the
                    // store cell IS_REFERENCE (dumps show `&int`).
                    if let Some(cc) = arr.borrow().get_cell(&ak) {
                        self.mark_ref(&cc);
                        return Ok(cc);
                    }
                    let c = cell(Value::Null);
                    arr.borrow_mut().set_cell(ak, c.clone());
                    self.mark_ref(&c);
                    return Ok(c);
                }
            }
        }
        self.last_ret_cell = None;
        // zend evaluates this read as BP_VAR_RW — a missing bucket is
        // created silently inside offsetGet.
        let was = std::mem::replace(&mut self.dim_by_ref, true);
        let rv = self.aa_offset_get(&o, key, spl_iter, true);
        self.dim_by_ref = was;
        match self.last_ret_cell.take() {
            Some(rc) => {
                self.mark_ref(&rc);
                Ok(rc)
            }
            None => {
                // spl's overloaded-dim machinery notices the indirect
                // modification even when the engine call threw (zend
                // emits it while the error is pending); a plain
                // ArrayAccess only notices when the fetched value
                // isn't itself an object (object-typed elements carry
                // their own storage).
                match rv {
                    Err(e) => {
                        if spl_iter {
                            let cn = o.borrow().class.name().to_string();
                            self.notice(&format!(
                                "Indirect modification of overloaded element of {} has no effect",
                                cn
                            ))?;
                        }
                        Err(e)
                    }
                    Ok(v) => {
                        if !matches!(v, Value::Object(_)) {
                            let cn = o.borrow().class.name().to_string();
                            self.notice(&format!(
                                "Indirect modification of overloaded element of {} has no effect",
                                cn
                            ))?;
                        }
                        Ok(cell(v))
                    }
                }
            }
        }
    }

    /// Engine-invoked `offsetGet` — zend calls the override through
    /// `zend_call_method`; on spl classes `[]` hands it an UNDEF arg
    /// (`spl_array_read_dimension_ex` → `zend_handle_undef_args`) so
    /// a required — or variadic — param at that slot arity-errors
    /// `{DeclCls}::offsetGet(): Argument #N[ ($x)] not passed`, a
    /// default fills in, and native storage returns uninitialized.
    /// Other ArrayAccess objects get a defined NULL for `[]`.
    fn aa_offset_get(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        key: Option<Cell>,
        spl: bool,
        undef: bool,
    ) -> Result<Value, PhpError> {
        let Some((m, dc)) = self.find_method_in(&o.borrow().class, "offsetGet") else {
            return Ok(Value::Null);
        };
        let undef = undef && spl && key.is_none();
        if undef && m.decl.body.is_empty() && m.decl.line == 0 {
            return Ok(Value::Null);
        }
        // Arity errors only exist on the UNDEF-arg engine call (zend
        // handle_undef_args); a defined arg — even NULL on `[]` writes —
        // always counts as passed.
        let given = usize::from(key.is_some()) | usize::from(!undef);
        if let Some((i, pname)) = m
            .decl
            .params
            .iter()
            .enumerate()
            .find(|(i, p)| {
                *i >= given && p.default.is_none() && (!p.variadic || (undef && *i == given))
            })
            .map(|(i, p)| {
                (
                    i,
                    if p.variadic {
                        String::new()
                    } else {
                        format!(" (${})", p.name)
                    },
                )
            })
        {
            let mut e = PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "{}::offsetGet(): Argument #{}{} not passed",
                    dc.name(),
                    i + 1,
                    pname
                ),
                self.cur_line,
            );
            e.thrown_line = Some(m.decl.line);
            // zend's engine-invoked ACE is a real throwable — `fail`
            // installs it as pending_exception so userland catch sees
            // it (a raw Err would die uncatchable).
            return self.fail(e);
        }
        let args = if undef {
            CallArgs::empty()
        } else {
            CallArgs::positional(vec![key.unwrap_or_else(|| cell(Value::Null))])
        };
        let rv = self.method_invoke(o.clone(), "offsetGet", args);
        // A value-returning offsetGet can't bind — only a `&`-declared
        // callee leaves a consumable return cell; anything still in
        // last_ret_cell after a non-`&` call is a stale cell the callee
        // read internally (e.g. an inner `$this->x['k']`), and honoring
        // it would bind the caller's `=&` into that inner bucket.
        if !m.decl.by_ref {
            self.last_ret_cell = None;
        }
        rv
    }

    fn index_cell(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Cell, PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
        self.index_cell_key(e, key)
    }

    /// `index_cell` with an already-evaluated key.
    fn index_cell_key(&mut self, e: &Expr, key: Option<Value>) -> Result<Cell, PhpError> {
        let key = key.map(cell);
        match e {
            Expr::Var(name) => {
                let c = self.var_cell(name);
                self.index_into_key(c, key)
            }
            Expr::Index { e: inner, i: ii } => {
                let c = self.index_cell(inner, ii.as_deref())?;
                self.index_into_key(c, key)
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
                site,
            } => {
                let c = self.prop_cell(obj, name, *nullsafe, *site)?;
                self.index_into_key(c, key)
            }
            Expr::StaticProp { class, name } => {
                let c = self.static_prop_cell(class, name)?;
                self.index_into_key(c, key)
            }
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                let c = self.var_cell(&name);
                self.index_into_key(c, key)
            }
            _ => {
                // e.g. function call result index — read-only path.
                let v = match key {
                    Some(k) => self.index_read_val(e, Some(k.borrow().clone()))?,
                    None => self.index_read(e, None)?,
                };
                Ok(cell(v))
            }
        }
    }

    /// The line a sub-expression's `argline` marker records, if any.
    pub(in crate::interp) fn marked_line(e: &Expr) -> Option<usize> {
        match e {
            Expr::Binary {
                op: "argline", l, ..
            } => match l.as_ref() {
                Expr::Int(n) => Some(*n as usize),
                _ => None,
            },
            _ => None,
        }
    }

    fn index_read(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Value, PhpError> {
        // `$GLOBALS['x']` reads the global slot by name — a miss warns
        // `Undefined global variable $x`, not the array-key wording.
        if let Expr::Var(n) = Self::unmark_rhs(e) {
            if n == "GLOBALS" {
                if let Some(ie) = i {
                    let k = self.eval(ie)?;
                    let kn = k.to_php_string();
                    if let Some(c) = self.global_var_cell(&kn) {
                        return Ok(c.borrow().clone());
                    }
                    self.warn(&format!("Undefined global variable ${kn}"))?;
                    return Ok(Value::Null);
                }
            }
        }
        // zend_compile_dim emits the dim expression's ops BEFORE the
        // container's for delayed containers — a CV (or varnode: varvar/
        // prop/static-prop) base binds inside the FETCH_DIM op, so its
        // warnings follow the dim's and site at the dim's line.
        // Non-delayable containers (calls, subscript results, literals)
        // are real ops that emit first — base then dim.
        let dim_simple = i.map(|i| {
            let i = Self::unmark_rhs(i);
            matches!(i, Expr::Var(_)) || is_compile_const(i)
        });
        let delayed = matches!(
            Self::unmark_rhs(e),
            Expr::Var(_) | Expr::VarVar(..) | Expr::Prop { .. } | Expr::StaticProp { .. }
        );
        if delayed && dim_simple != Some(true) {
            // Complex dim: its ops run first (inner warnings at their
            // own lines). A CV base (bare or folded-varvar) binds
            // inside the FETCH_DIM at the dim's line; varnode bases
            // (dyn varvar/prop/static-prop) emit their own op after
            // the dim's at the container's own line.
            let (base_line, base_send) = (self.cur_line, self.send_line);
            let key = match i {
                Some(ie) => self.eval(ie)?,
                None => {
                    return self.fail(PhpError::compile_fatal(
                        "Cannot use [] for reading",
                        self.cur_line,
                    ));
                }
            };
            let prev_vv = self.vv_rhs_site;
            match e {
                Expr::Var(_) => {
                    if let Some(l) = i.and_then(Self::marked_line) {
                        self.cur_line = l;
                        self.send_line = Some(l);
                    }
                }
                Expr::VarVar(inner, _) if is_compile_const(inner) => {
                    self.vv_rhs_site = i.and_then(Self::marked_line).or(Some(base_line));
                }
                _ => {
                    self.cur_line = base_line;
                    self.send_line = base_send;
                }
            }
            let base = self.eval(e);
            self.vv_rhs_site = prev_vv;
            let base = base?;
            // The FETCH's own diagnostics (offset warnings, key checks)
            // site at the dim's line.
            if let Some(l) = i.and_then(Self::marked_line) {
                self.cur_line = l;
                self.send_line = Some(l);
            }
            return self.index_read_base(base, key);
        }
        // Simple dim (CV/const binds inside the op): the FETCH_DIM sites
        // a CV container's warning at the dim's line — a folded varvar
        // does the same through vv_rhs_site.
        let prev_vv = self.vv_rhs_site;
        match e {
            Expr::Var(_) => {
                if let Some(l) = i.and_then(Self::marked_line) {
                    self.cur_line = l;
                    self.send_line = Some(l);
                }
            }
            Expr::VarVar(inner, _) if is_compile_const(inner) => {
                self.vv_rhs_site = i.and_then(Self::marked_line);
            }
            _ => {}
        }
        // Base evaluates before the index expr (left-to-right).
        let base = self.eval(e);
        self.vv_rhs_site = prev_vv;
        let base = base?;
        let key = match i {
            Some(ie) => self.eval(ie)?,
            None => {
                return self.fail(PhpError::compile_fatal(
                    "Cannot use [] for reading",
                    self.cur_line,
                ));
            }
        };
        self.index_read_base(base, key)
    }

    /// `index_read` for a caller that already evaluated `e` and the key.
    fn index_read_val(&mut self, e: &Expr, key: Option<Value>) -> Result<Value, PhpError> {
        let base = self.eval(e)?;
        match key {
            Some(k) => self.index_read_base(base, k),
            None => self.fail(PhpError::compile_fatal(
                "Cannot use [] for reading",
                self.cur_line,
            )),
        }
    }

    fn index_read_base(&mut self, base: Value, key: Value) -> Result<Value, PhpError> {
        // ArrayAccess containers see any key type (offsetGet); string
        // bases validate keys via str_offset_key ('on string' errors).
        if !matches!(&base, Value::Object(o) if self.obj_is_a(o, "ArrayAccess"))
            && !matches!(&base, Value::Str(_))
        {
            self.check_offset_key(&key)?;
        }
        match base {
            Value::Array(rc) => {
                let k = self.arr_key(&key)?;
                let arr = rc.borrow();
                match arr.get(&k) {
                    Some(v) => Ok(v),
                    None => {
                        // zend prints the converted offset — a null key
                        // names `""`, not an unquoted empty string.
                        let shown = match &k {
                            ArrKey::Str(s) => format!("\"{}\"", s),
                            ArrKey::Int(i) => format!("{}", i),
                            ArrKey::Tomb => "\"\"".to_string(),
                        };
                        if !self.is_quiet() {
                            self.warn(&format!("Undefined array key {}", shown))?;
                        }
                        Ok(Value::Null)
                    }
                }
            }
            Value::Str(s) => {
                // zend_check_string_offset — same key matrix as the
                // write path (' 2'/'+2' accepted, leading-int junk
                // warns, casts warn, objects/arrays TypeError).
                let idx = self.str_offset_key(Some(&key))?;

                let bytes: &[u8] = &s[..];
                let idx = if idx < 0 {
                    idx + bytes.len() as i64
                } else {
                    idx
                };
                if idx < 0 || idx as usize >= bytes.len() {
                    if !self.is_quiet() {
                        self.warn(&format!("Uninitialized string offset {}", key.to_int()))?;
                    }
                    // zend reads an empty string out of an
                    // uninitialized offset, not NULL.
                    Ok(Value::str(String::new()))
                } else {
                    Ok(Value::bytes(bytes[idx as usize..idx as usize + 1].to_vec()))
                }
            }
            Value::Null => {
                if !self.is_quiet() {
                    // PHP 8.5 dropped "value of type" from this message
                    // (bug25922, passByReference_003).
                    self.warn("Trying to access array offset on null")?;
                }
                Ok(Value::Null)
            }
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    return match self.method_invoke(
                        o,
                        "offsetGet",
                        CallArgs::positional(vec![cell(key)]),
                    ) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(e),
                    };
                }
                // A plain-object dim fetch throws zend's catchable
                // `Error: Cannot use object of type C as array` — even
                // inside isset()/empty()/`@` (probe_r3).
                let cn = o.borrow().class.name().to_string();
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Cannot use object of type {} as array", cn),
                    self.cur_line,
                ))
            }
            _ => {
                if !self.is_quiet() {
                    // PHP 8.5 names the scalar itself: int/float/null and
                    // the literal true|false (no "value of type").
                    let what = match &base {
                        Value::Bool(b) => b.to_string(),
                        _ => base.type_name().to_lowercase(),
                    };
                    self.warn(&format!("Trying to access array offset on {}", what))?;
                }
                Ok(Value::Null)
            }
        }
    }

    /// An unset dim key: a plain-$var key binds inside its own
    /// FETCH_DIM_UNSET — zend reads the container operand first and the
    /// CV operand second (a missing-root unset still runs every CV
    /// read's warn) — while every other key expr is a delayed dim,
    /// evaluated ahead of the fetch chain.
    pub(in crate::interp) fn unset_index(
        &mut self,
        e: &Expr,
        i: Option<&Expr>,
    ) -> Result<(), PhpError> {
        // Peel e's own dims first — `unset(root[d0][d1]...[k])` descends
        // the d-chain then unsets k; `cur` is the root container expr.
        // (AST-only; no eval order change vs zend.)
        let mut idxs: Vec<Option<&Expr>> = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                Expr::Index { e: b, i: ix } => {
                    idxs.push(ix.as_deref());
                    cur = b;
                }
                Expr::Paren(inner) => cur = inner.as_ref(),
                _ => break,
            }
        }
        idxs.reverse();
        self.unset_index_inner(cur, &idxs, i)
    }

    fn unset_index_inner(
        &mut self,
        cur: &Expr,
        idxs: &[Option<&Expr>],
        i: Option<&Expr>,
    ) -> Result<(), PhpError> {
        // zend's delayed dims: roots without storage cells evaluate
        // FIRST — ahead of every key expr (`unset(ret()[lit()][$u])`
        // prints RET,LIT,warn-u) — then non-CV key exprs evaluate in
        // order; plain-$var keys defer to their own fetch level.
        let mut scratch: Option<Cell> = None;
        match cur {
            Expr::Var(_) | Expr::Prop { .. } | Expr::StaticProp { .. } | Expr::VarVar(..) => {}
            _ => {
                let v = match self.eval(cur) {
                    Ok(v) => v,
                    Err(e) => return self.fail::<()>(e),
                };
                scratch = Some(cell(v));
            }
        }
        let mut keys: Vec<UnsetKey> = Vec::with_capacity(idxs.len() + 1);
        for ix in idxs.iter().copied().chain(std::iter::once(i)) {
            // `markline` wraps every parsed dim key in `argline`
            // (parens nest inside it) — unwrap before classifying or a
            // CV key silently becomes an early-evaluated Val.
            keys.push(match ix.map(Self::unmark_rhs) {
                Some(Expr::Var(n)) => UnsetKey::Cv(n.clone(), ix.and_then(Self::marked_line)),
                _ => UnsetKey::Val(match ix {
                    Some(ie) => match self.eval(ie) {
                        Ok(v) => v,
                        Err(e) => return self.fail::<()>(e),
                    },
                    None => Value::Null,
                }),
            });
        }
        // The first fetch's container resolution: real storage cells
        // bind live (prop_cell runs __get once); a missing plain var
        // warns once then the whole chain walks zend's undef sentinel —
        // a null scratch that no-ops but still reads every later key.
        let mut c = match scratch {
            Some(c) => c,
            None => match cur {
                Expr::Var(n) => match self.var_cell_opt(n) {
                    Some(c) => c,
                    None => {
                        if !self.is_quiet() {
                            self.warn(&format!("Undefined variable ${}", n))?;
                        }
                        cell(Value::Null)
                    }
                },
                Expr::Prop {
                    obj,
                    name,
                    nullsafe,
                    site,
                } => match self.prop_cell(obj, name, *nullsafe, *site) {
                    Ok(c) => c,
                    Err(e) => return self.fail::<()>(e),
                },
                Expr::StaticProp { class, name } => match self.static_prop_cell(class, name) {
                    Ok(c) => c,
                    Err(e) => return self.fail::<()>(e),
                },
                Expr::VarVar(inner, _) => {
                    let n = self.eval(inner)?;
                    let name = self.conv_str(&n)?;
                    self.var_cell(&name)
                }
                _ => unreachable!("non-cell roots evaluated above"),
            },
        };
        let last = keys.len() - 1;
        // The spl array-object storage the chain entered through —
        // zend's spl_array_unset_dimension throws mid-sort.
        let mut last_ao: Option<Rc<RefCell<PhpObject>>> = None;
        // Once an ancestor level's operand detached (a key-read
        // handler rebound the root), the whole remaining walk is on
        // the stale operand — deeper conversions stay silent too.
        let mut det = false;
        for (n, ka) in keys.iter().enumerate() {
            // zend addrefs the level's container operand before reading
            // the key: a handler rebind inside the key's diagnostics
            // leaves the fetch on `held`; in-place writes stay visible.
            let held = c.borrow().clone();
            if n < last {
                // FETCH_DIM_UNSET checks the container before touching
                // the key operand — scalars die without reading it,
                // `false` deprecates first.
                match held {
                    Value::Int(_) | Value::Float(_) | Value::Bool(true) | Value::Resource(_) => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot unset offset in a non-array variable",
                            self.cur_line,
                        ));
                    }
                    Value::Bool(false) => {
                        self.deprecated_ns("Automatic conversion of false to array is deprecated")?;
                    }
                    _ => {}
                }
            }
            let key = match ka {
                UnsetKey::Cv(nm, ln) => {
                    // The CV read sites at its own operand line (zend
                    // runs the CV_CHECK inside the dim op at the key's
                    // marked line).
                    if let Some(l) = ln {
                        self.cur_line = *l;
                        self.send_line = Some(*l);
                    }
                    match self.var_lookup(nm) {
                        Some(kc) => kc.borrow().clone(),
                        None => {
                            if !self.is_quiet() {
                                self.warn(&format!("Undefined variable ${}", nm))?;
                            }
                            Value::Null
                        }
                    }
                }
                UnsetKey::Val(v) => v.clone(),
            };
            if n < last {
                c = match self.unset_dim_step(c, held, key, &mut last_ao, &mut det)? {
                    Some(nc) => nc,
                    // Missing levels keep walking a null sentinel —
                    // later keys still read, nothing unsets.
                    None => cell(Value::Null),
                };
            } else {
                return self.unset_last(c, held, key, &mut last_ao, det);
            }
        }
        Ok(())
    }

    /// One intermediate dim down for `unset`, operating on the captured
    /// `held` operand: plain arrays yield the bucket cell
    /// (cow-separating a shared table first, like a write), spl
    /// array-objects yield the live storage cell (missing → the
    /// "Indirect modification" notice, bug66127), other ArrayAccess
    /// objects fetch through offsetGet. Null/false and missing buckets
    /// yield `None` — the walk continues on a null sentinel so later
    /// keys still read. Strings and non-ArrayAccess objects throw
    /// zend's catchable unset `Error`s; scalars were already rejected
    /// before the key read.
    fn unset_dim_step(
        &mut self,
        c: Cell,
        held: Value,
        key: Value,
        last_ao: &mut Option<Rc<RefCell<PhpObject>>>,
        det: &mut bool,
    ) -> Result<Option<Cell>, PhpError> {
        match held {
            Value::Array(rc) => {
                // A key-read handler rebind detaches the slot: zend
                // walks the stale operand and converts SILENTLY (the
                // assign_dim_014 rule — no null-offset deprecation).
                // An ancestor level that already detached makes the
                // whole remaining operand stale too.
                let detached = *det
                    || !Self::same_container(&DimPre::of(&Value::Array(rc.clone())), &c.borrow());
                *det |= detached;
                let was = self.detached_dim;
                self.detached_dim = was || detached;
                // Convert before borrowing the table — the conversion's
                // diagnostics can dispatch the user handler (a nested
                // write to this array would panic a live borrow).
                let ak_r = self.arr_key(&key);
                self.detached_dim = was;
                let ak = ak_r?;
                let c = if detached {
                    cell(Value::Array(rc))
                } else {
                    // Drop the held pin before cow_split: its extra
                    // ref would force a gratuitous split every level.
                    drop(rc);
                    c
                };
                let mut b = c.borrow_mut();
                self.cow_split(&mut b);
                let rc = match &*b {
                    Value::Array(rc) => rc.clone(),
                    _ => unreachable!(),
                };
                drop(b);
                let found = rc.borrow().get_cell(&ak);
                Ok(found)
            }
            Value::Null | Value::Bool(false) => Ok(None),
            Value::Str(_) => {
                // String-offset read diagnostics, then the descend
                // Error keyed by the offset's shape (s3/s6/v8). An
                // ancestor-detached walk converts silently.
                match &key {
                    Value::Str(ks) => match Self::str_off_key(ks) {
                        StrOffKey::Bad => {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Cannot unset string offsets",
                                self.cur_line,
                            ));
                        }
                        StrOffKey::Junk(_) => {
                            if !*det {
                                self.warn(&format!(
                                    "Illegal string offset \"{}\"",
                                    crate::value::lossy(ks)
                                ))?;
                            }
                        }
                        StrOffKey::Int(_) => {}
                    },
                    Value::Int(_) => {}
                    _ => {
                        if !*det {
                            self.warn_ns("String offset cast occurred")?;
                        }
                    }
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use string offset as an array",
                    self.cur_line,
                ))
            }
            Value::Object(o) => {
                if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) {
                    let arr = self.ao_arr(&o);
                    let was = self.detached_dim;
                    self.detached_dim = was || *det;
                    let ak_r = self.arr_key(&key);
                    self.detached_dim = was;
                    let ak = ak_r?;
                    let found = arr.borrow().get_cell(&ak);
                    match found {
                        Some(cc) => {
                            let mut b = cc.borrow_mut();
                            // The fetched array may alias other slots
                            // (assigned by value elsewhere) — cow-split
                            // like the plain-array arm does.
                            self.cow_split(&mut b);
                            drop(b);
                            *last_ao = Some(o.clone());
                            Ok(Some(cc))
                        }
                        None => {
                            if !self.is_quiet() {
                                let cn = o.borrow().class.name().to_string();
                                self.notice(&format!(
                                    "Indirect modification of overloaded element of {} has no effect",
                                    cn
                                ))?;
                            }
                            Ok(None)
                        }
                    }
                } else if self.obj_is_a(&o, "ArrayAccess") {
                    // index_cell_object owns the indirect-modification
                    // notice (non-ref, non-object fetched element —
                    // zend's zend_fetch_dimension_address rule, which
                    // unset shares). `held` is the operand — offsetGet
                    // runs on it even if the slot rebound mid-read.
                    let cc = self.index_cell_object(&cell(Value::Object(o)), Some(cell(key)))?;
                    Ok(Some(cc))
                } else {
                    let cn = o.borrow().class.name().to_string();
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot use object of type {} as array", cn),
                        self.cur_line,
                    ))
                }
            }
            Value::Callable(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use object of type Closure as array",
                self.cur_line,
            )),
            _ => unreachable!("scalar intermediates rejected before the key read"),
        }
    }

    /// Final dim of `unset(cell[key])` on the captured `held` operand —
    /// the key is already read: arrays cow-separate then drop the key
    /// (illegal keys throw "Cannot unset offset of type K on array",
    //  a distinct message from the fetch's "Cannot access …"), null is
    /// a silent no-op, `false` deprecates, strings/scalars/non-
    /// ArrayAccess objects throw zend's catchable unset `Error`s, and
    /// ArrayAccess objects dispatch offsetUnset.
    fn unset_last(
        &mut self,
        c: Cell,
        held: Value,
        key: Value,
        last_ao: &mut Option<Rc<RefCell<PhpObject>>>,
        ancestor_det: bool,
    ) -> Result<(), PhpError> {
        // Borrowed, not destructured: in the non-detached arm `held`'s
        // pin must die BEFORE destruct_dying_value — its slot keeps the
        // evicted payload looking still-shared and __destruct never
        // runs (bug65051).
        let pre = DimPre::of(&held);
        match held {
            Value::Array(rc) => {
                if let Some(t) = Self::illegal_offset_ty(&key) {
                    return self.fail(PhpError::uncaught(
                        "TypeError",
                        format!("Cannot unset offset of type {} on array", t),
                        self.cur_line,
                    ));
                }
                // Same operand-vs-rebind rule as the intermediates:
                // a key-read handler rebind lands the delete on `rc`
                // (stale) and converts silently; in-place edits stay
                // visible. An ancestor's detach makes the operand
                // stale wholesale.
                let detached = ancestor_det || !Self::same_container(&pre, &c.borrow());
                // `unset()` maps null offsets to "" WITHOUT the 'Using
                // null as an array offset' deprecation; every other
                // cast still diagnoses. Scoped so conversions inside
                // nested userland (offsetGet/dtors) still warn (r8).
                let was_ctx = std::mem::replace(&mut self.unset_ctx, true);
                let was_det = self.detached_dim;
                self.detached_dim = was_det || detached;
                let ak_r = self.arr_key(&key);
                self.unset_ctx = was_ctx;
                self.detached_dim = was_det;
                let ak = ak_r?;
                let c = if detached {
                    cell(Value::Array(rc))
                } else {
                    // Drop the held pin before cow_split: its extra ref
                    // would force a split and keep the dead table's
                    // element slots alive past the payload eviction.
                    drop(rc);
                    c
                };
                // zend's spl_array_unset_dimension throws mid-sort.
                if let Some(o) = last_ao {
                    if let Some(e) = self.ao_sorting_err(o) {
                        return self.fail(e);
                    }
                }
                let mut b = c.borrow_mut();
                // `unset($copy[$k])` must cow-separate a shared array
                // like a write does — PHP copies `$a = $b` lazily;
                // mutating the shared table would corrupt the source.
                self.cow_split(&mut b);
                if let Value::Array(rc) = &*b {
                    let rc = rc.clone();
                    drop(b);
                    // The evicted payload's last ref dies with the
                    // cell — held objects/gens destruct now (zend
                    // destroys the zval's contents). The borrow_mut
                    // ends before userland dtors run: an element by-ref
                    // aliasing this array reads it inside __destruct
                    // (bug65051).
                    let evicted = rc.borrow_mut().unset(&ak);
                    if let Some(v) = evicted {
                        self.destruct_dying_value(&v)?;
                    }
                }
                Ok(())
            }
            Value::Null => Ok(()),
            // zend's unset emits the false->array conversion deprecation
            // then no-ops (the var stays false) — unlike reads/writes it
            // never actually vivifies.
            Value::Bool(false) => {
                self.deprecated_ns("Automatic conversion of false to array is deprecated")
            }
            Value::Str(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot unset string offsets",
                self.cur_line,
            )),
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    self.method_invoke(o, "offsetUnset", CallArgs::positional(vec![cell(key)]))?;
                    Ok(())
                } else {
                    let cn = o.borrow().class.name().to_string();
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot use object of type {} as array", cn),
                        self.cur_line,
                    ))
                }
            }
            Value::Callable(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use object of type Closure as array",
                self.cur_line,
            )),
            _ => self.fail(PhpError::uncaught(
                "Error",
                "Cannot unset offset in a non-array variable",
                self.cur_line,
            )),
        }
    }

    /// Catch-bind checking: strict member fit only — no __toString or
    /// scalar coercion (int still widens to float)
    /// (typed_properties_108).
    fn slot_write_strict(&mut self, tys: &[String], v: &Value) -> Option<Value> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Some(Value::Float(*i as f64));
                }
            }
            return Some(v.clone());
        }
        None
    }

    /// The write result one typed-slot owner would produce — exact
    /// match widens `int` into a `float` member, weak files coerce;
    /// `None` when the type can't be satisfied (union_types/prop_ref_assign).
    fn slot_write_one(&mut self, tys: &[String], v: &Value) -> Result<Option<Value>, PhpError> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Ok(Some(Value::Float(*i as f64)));
                }
            }
            return Ok(Some(v.clone()));
        }
        if !self.exec_file_strict() {
            // Object with __toString coerces into a `string` slot (107).
            if let Value::Object(o) = v {
                if tys.iter().any(|t| t.eq_ignore_ascii_case("string"))
                    && (self
                        .find_method_in(&o.borrow().class, "__tostring")
                        .is_some()
                        || self
                            .find_method_in(&o.borrow().class, "__toString")
                            .is_some())
                {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    let svs = self.conv_str(&sv)?;
                    return Ok(Some(Value::str(svs)));
                }
            }
            if let Some(cv) = weak_ty_coerce(tys, v) {
                self.deprecate_lossy_int(tys, v, &cv);
                return Ok(Some(cv));
            }
        }
        Ok(None)
    }

    /// An owner is stale when the prop it names no longer holds this
    /// cell — static rebinds (082), unsets, or a dead object (094).
    fn slot_anchor_alive(&self, ptr: usize, anc: &SlotAnchor) -> bool {
        match anc {
            // A destructed object is pinned in `self.destructed` — its
            // prop table still holds the cell but no longer anchors it
            // (tp094's post-unset write must not gate).
            SlotAnchor::Obj(w, key) => w
                .upgrade()
                .map(|o| {
                    !self.destructed.contains_key(&(Rc::as_ptr(&o) as usize))
                        && o.borrow()
                            .props
                            .get(key)
                            .map(|c| Rc::as_ptr(c) as usize == ptr)
                            .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::Statics(cn, pn) => self
                .classes
                .get(&cn.to_lowercase())
                .map(|c| {
                    c.statics
                        .borrow()
                        .get(pn)
                        .map(|c2| Rc::as_ptr(c2) as usize == ptr)
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::None => true,
        }
    }

    /// Drop owners whose prop no longer points at `ptr`, then rebuild
    /// the merged constraint from the live owners' declared types.
    pub(in crate::interp) fn prune_typed_slot(&mut self, ptr: usize) {
        let prim_dead = self
            .slot_anchor
            .get(&ptr)
            .map(|a| !self.slot_anchor_alive(ptr, a))
            .unwrap_or(false);
        if prim_dead {
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let dead: Vec<usize> = os
                .iter()
                .enumerate()
                .filter(|(_, (_, _, _, a))| !self.slot_anchor_alive(ptr, a))
                .map(|(i, _)| i)
                .collect();
            if !dead.is_empty() {
                let os = self.slot_owners.get_mut(&ptr).unwrap();
                for i in dead.into_iter().rev() {
                    os.remove(i);
                }
                if os.is_empty() {
                    self.slot_owners.remove(&ptr);
                }
            }
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let mut acc: Vec<String> = Vec::new();
            for (i, (t, _, _, _)) in os.iter().enumerate() {
                acc = if i == 0 {
                    t.clone()
                } else {
                    self.ty_bind_merge(&acc, t).unwrap_or_default()
                };
            }
            self.slot_merged.insert(ptr, acc);
        } else if let Some((_, t, _, _)) = self.typed_slots.get(&ptr) {
            self.slot_merged.insert(ptr, t.clone());
        } else {
            self.slot_merged.remove(&ptr);
        }
    }

    /// By-ref foreach bind onto a readonly prop cell shared through an
    /// ArrayIterator — Error "Cannot acquire reference to readonly
    /// property C::$p" (typed_properties_115).
    pub(in crate::interp) fn readonly_ref_error(&mut self, c: &Cell) -> Option<Flow> {
        let (cn, pn) = self.readonly_cells.get(&(Rc::as_ptr(c) as usize))?;
        let v = self.exception(
            "Error",
            &format!("Cannot acquire reference to readonly property {cn}::${pn}"),
        );
        let e = self.throw(v);
        Some(self.err_flow(e))
    }

    fn typed_slot_store(&mut self, c: &Cell, v: Value) -> Result<Value, PhpError> {
        self.typed_slot_store_mode(c, v, false)
    }

    /// `strict` = catch-binding semantics: no __toString/scalar
    /// coercion — the value must fit the declared type as-is
    /// (typed_properties_108).
    pub(in crate::interp) fn typed_slot_store_mode(
        &mut self,
        c: &Cell,
        v: Value,
        strict: bool,
    ) -> Result<Value, PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let owners: Vec<(Vec<String>, String, String)> =
            if let Some(os) = self.slot_owners.get(&ptr) {
                os.iter()
                    .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                    .collect()
            } else {
                match self
                    .typed_slots
                    .get(&ptr)
                    .map(|(_, t, n, p)| (t.clone(), n.clone(), p.clone()))
                {
                    Some(o) => vec![o],
                    None => return Ok(v),
                }
            };
        // Overflowing ++/-- through a ref held by a typed-int prop:
        // zend rejects on the inc/dec verb before the float coercion
        // is even considered (typed_properties_064).
        if let Some((dir, bound)) = self.incdec_ref_ctx {
            if self.is_ref_ptr(ptr) {
                if let Some((tys, cn, pn)) = owners.iter().find(|(tys, _, _)| {
                    tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                        && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                }) {
                    let mut e = PhpError::uncaught(
                        "TypeError",
                        format!(
                            "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            pn,
                            ty_disp(tys),
                            bound
                        ),
                        0,
                    );
                    e.thrown_line = Some(self.cur_line);
                    return self.fail(e);
                }
            }
        }
        let mut results: Vec<Option<Value>> = Vec::with_capacity(owners.len());
        for (tys, _, _) in &owners {
            results.push(if strict {
                self.slot_write_strict(tys, &v)
            } else {
                self.slot_write_one(tys, &v)?
            });
        }
        // A value an owner rejects outright reports that owner; only
        // all-accepted-but-divergent coercions are "inconsistent"
        // (typed_reference).
        let consistent = results.iter().all(|r| r.is_some())
            && results
                .iter()
                .map(|r| r.clone().unwrap())
                .all(|rv| Self::value_identical(&rv, results[0].as_ref().unwrap()));
        if consistent {
            return Ok(results.into_iter().next().unwrap().unwrap());
        }
        if let Some(bad) = results.iter().position(|r| r.is_none()) {
            let (tys, cn, pn) = &owners[bad];
            let where_ = if self.is_ref_ptr(ptr) {
                "reference held by property"
            } else {
                "property"
            };
            let mut e = PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            );
            e.thrown_line = Some(self.cur_line);
            return self.fail(e);
        }
        let mut e = if owners.len() == 1 {
            let (tys, cn, pn) = &owners[0];
            let where_ = if self.is_ref_ptr(ptr) {
                "reference held by property"
            } else {
                "property"
            };
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            )
        } else {
            let held: Vec<String> = owners
                .iter()
                .map(|(tys, cn, pn)| format!("property {}::${} of type {}", cn, pn, ty_disp(tys)))
                .collect();
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to reference held by {}, as this would result in an inconsistent type conversion",
                    self.zval_type_name(&v),
                    held.join(" and ")
                ),
                0,
            )
        };
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Synthesized signature of a magic-method trampoline FCC:
    /// `mixed ...$arguments` (trampoline_closure_named_arguments).
    fn trampoline_decl() -> Rc<crate::ast::FunctionDecl> {
        Rc::new(crate::ast::FunctionDecl {
            name: "{trampoline}".into(),
            params: vec![crate::ast::Param {
                name: "arguments".into(),
                default: None,
                by_ref: false,
                variadic: true,
                ty: Some(vec!["mixed".into()]),
                promoted: false,
                vis: None,
                readonly: false,
                is_final: false,
                set_vis: None,
                hooks: None,
            }],
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        })
    }

    /// Decl for the function a callable value points at — builtins
    /// synthesize one from builtin_sig. Shared by the function and
    /// method (Closure::__invoke) reflector paths.
    pub(in crate::interp) fn callable_decl(
        &mut self,
        v: &Value,
    ) -> Option<Rc<crate::ast::FunctionDecl>> {
        match v {
            Value::Str(s) => {
                let n = String::from_utf8_lossy(s).to_lowercase();
                self.functions
                    .get(&n)
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n))
            }
            Value::Callable(c) => match &c.kind {
                crate::value::CallableKind::Closure(d) => Some(d.clone()),
                crate::value::CallableKind::Named(n) => self
                    .functions
                    .get(&n.to_lowercase())
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n.to_lowercase())),
                crate::value::CallableKind::Method { name, obj, class } => {
                    let c = class
                        .clone()
                        .or_else(|| obj.as_ref().map(|o| o.borrow().class.clone()));
                    match c {
                        Some(c) => self
                            .find_method_in(&c, name)
                            .map(|(m, _)| Rc::new(m.decl.clone()))
                            // A magic-method trampoline (`C::undef(...)`
                            // on __callStatic / `$o->undef(...)` on
                            // __call) reflects as `mixed ...$arguments`
                            // (trampoline_closure_named_arguments).
                            .or_else(|| {
                                let magic = if obj.is_some() {
                                    "__call"
                                } else {
                                    "__callstatic"
                                };
                                self.find_method_in(&c, magic)
                                    .is_some()
                                    .then(Self::trampoline_decl)
                            }),
                        None => None,
                    }
                }
            },
            _ => None,
        }
    }

    /// Synthetic decl for an internal function, from builtin_sig +
    /// builtin_param_ty — lets reflectors report param names,
    /// required flags and declared types for builtins (bug69802_2).
    fn builtin_decl(lname: &str) -> Option<Rc<crate::ast::FunctionDecl>> {
        let sig = crate::builtins::builtin_sig(lname)?;
        Some(Rc::new(crate::ast::FunctionDecl {
            name: lname.into(),
            params: sig
                .into_iter()
                .map(|(name, req)| crate::ast::Param {
                    default: if req {
                        None
                    } else {
                        Some(crate::ast::Expr::Null)
                    },
                    ty: crate::builtins::builtin_param_ty(lname, &name),
                    name,
                    by_ref: false,
                    variadic: false,
                    promoted: false,
                    vis: None,
                    readonly: false,
                    is_final: false,
                    set_vis: None,
                    hooks: None,
                })
                .collect(),
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        }))
    }

    /// Same-type same-value — owners must agree on the *exact* result
    /// (int(42) vs float(42.0) is inconsistent).
    fn value_identical(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
            (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    /// Warn 'Undefined variable' once for the innermost CV of an
    /// lvalue chain under zend's RW fetch — nested prop/dim links
    /// evaluate in the same write context, so `$u->a->a += 1` and
    /// `$u->p[k] += 1` warn $u (bug78531).
    fn warn_undef_lvalue_obj(&mut self, obj: &Expr) -> Result<(), PhpError> {
        let mut b: &Expr = obj;
        loop {
            match b {
                Expr::Paren(i) => b = i.as_ref(),
                Expr::Prop { obj: o, .. } => b = o.as_ref(),
                Expr::Index { e, .. } => b = e.as_ref(),
                _ => break,
            }
        }
        if let Expr::Var(n) = b {
            if self.var_lookup(n).is_none() {
                self.warn(&format!("Undefined variable ${}", n))?;
            }
        }
        Ok(())
    }

    fn incdec(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Value, PhpError> {
        // Op boundary for the armed dim throwable — see dim_op_outlet.
        let outer = self.dim_throw.take();
        let r = self.incdec_inner(target, delta, post);
        self.dim_op_outlet(r, outer)
    }

    fn incdec_inner(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Value, PhpError> {
        // PHP warns on undefined vars/props/keys during ++/-- (bug25547).
        let mut ro_target: Option<(Rc<RefCell<PhpObject>>, String)> = None;
        let old = match target {
            Expr::Var(name) => self.var_get(name).unwrap_or(Value::Null),
            Expr::Index { e, i } => {
                // zend's FETCH_DIM_RW+PRE/POST_INC is ONE op: the
                // container binds at op entry and each dim operand
                // evaluates once, then the fetched slot is the store
                // target (a mid-eval rebind drops the write silently).
                // Peel the dim chain; var roots take the pending-write
                // path so keys evaluate exactly once.
                let mut idxs: Vec<Option<&Expr>> = Vec::new();
                let mut root = e.as_ref();
                while let Expr::Index { e: b, i: ix } = root {
                    idxs.push(ix.as_deref());
                    root = b.as_ref();
                }
                idxs.reverse();
                idxs.push(i.as_deref());
                if let Expr::Var(root_n) = root {
                    return self.incdec_dim(root_n, &idxs, delta, post);
                }
                let base = self.eval(e)?;
                let key = match i {
                    Some(ie) => Some(self.eval(ie)?),
                    None => None,
                };
                // `++`/`--` on an ArrayAccess offset writes the by-ref
                // offsetGet cell directly — no offsetSet call
                // (typed_properties_065); `[]` appends into a temp.
                if let Value::Object(o) = &base {
                    if self.obj_is_a(o, "ArrayAccess") {
                        return self.incdec_aa(o.clone(), key, delta, post);
                    }
                }
                let Some(key) = key else {
                    return self.fail(PhpError::compile_fatal(
                        "Cannot use [] for reading",
                        self.cur_line,
                    ));
                };
                // `++`/`--` never lands on a string offset — zend
                // validates the key, then the catchable Error beats
                // the non-numeric-increment deprecation the byte
                // value would otherwise take (finding 15).
                if matches!(base, Value::Str(_)) {
                    self.str_offset_key(Some(&key))?;
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot increment/decrement string offsets",
                        self.cur_line,
                    ));
                }
                // Errors from the read propagate — a plain-object dim
                // is zend's catchable Error, not a silent null.
                self.index_read_base(base, key)?
            }
            // ++/-- reads through __get first — its exceptions
            // propagate (the __set is never reached, bug38624).
            Expr::Prop { obj, name, .. } => {
                // The RW prop fetch warns 'Undefined variable' on an
                // undef CV container like a compound assign
                // (bug78531) — eval_lvalue_obj stays silent for the
                // _W callers (plain `=`, by-ref reads).
                self.warn_undef_lvalue_obj(obj)?;
                // `++`/`--` on a prop of a non-object dies with the
                // incdec verb before the loose-read warn (probe4i).
                let ov = self.eval_lvalue_obj(obj)?;
                if !matches!(ov, Value::Object(_)) {
                    let pn = self.prop_name(name)?;
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Attempt to increment/decrement property \"{}\" on {}",
                            pn,
                            self.zval_type_name(&ov)
                        ),
                        self.cur_line,
                    ));
                }
                // zend's rw prop fetch materializes the dynamic slot
                // first, so on a plain missing prop 'Creation of
                // dynamic property' precedes 'Undefined property'
                // (probe_r2). Magic __get props keep loose-read.
                if let Value::Object(o) = &ov {
                    let pn = self.prop_name(name)?;
                    ro_target = Some((o.clone(), pn.clone()));
                    let cls = o.borrow().class.clone();
                    if self.decl_prop(o, &pn).is_none()
                        && !o.borrow().props.contains_key(&pn)
                        && self.find_method_in(&cls, "__get").is_none()
                    {
                        // ARRAY_AS_PROPS: an undeclared prop reads the
                        // storage hash — a missing bucket warns
                        // 'Undefined array key' like spl's
                        // read_property (gh18304).
                        if self.aap_active(o) {
                            let arr = self.ao_state(o).0;
                            let v = arr.borrow().get(&ArrKey::Str(Rc::from(pn.as_str())));
                            match v {
                                Some(v) => v,
                                None => {
                                    self.warn(&format!("Undefined array key \"{}\"", pn))?;
                                    Value::Null
                                }
                            }
                        } else {
                            // A DECLARED prop the scope can't see is
                            // `Cannot access private/protected property`,
                            // not a dynamic-prop materialization
                            // (closure_038/closure_039).
                            if let Some(e) = self.hidden_decl_error(o, &pn) {
                                return self.fail(e);
                            }
                            let cn = o.borrow().class.name().to_string();
                            if self.dyn_prop_deprecated(o, &pn, &pn) {
                                self.deprecated(&format!(
                                    "Creation of dynamic property {}::${} is deprecated",
                                    cn, pn
                                ))?;
                            }
                            self.warn(&format!("Undefined property: {}::${}", cn, pn))?;
                            let mut ob = o.borrow_mut();
                            if !ob.prop_order.contains(&pn) {
                                ob.prop_order.push(pn.clone());
                            }
                            ob.props
                                .entry(pn.clone())
                                .or_insert_with(|| cell(Value::Null));
                            Value::Null
                        }
                    } else {
                        self.prop_read_loose(target)?
                    }
                } else {
                    self.prop_read_loose(target)?
                }
            }
            Expr::VarVar(inner, _) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_get(&name).unwrap_or(Value::Null)
            }
            Expr::StaticProp { class, name } => {
                // Static-prop reads are real catchable Errors — an
                // uninitialized typed static reports 'must not be
                // accessed', not a silent NULL.
                self.static_prop_read(class, name)?
            }
            _ => {
                return self.fail(PhpError::fatal(
                    "Cannot increment/decrement non-variable",
                    0,
                ))
            }
        };
        // ++/-- is a read-write fetch: set-visibility gates there —
        // statics name 'Cannot indirectly modify', instance props
        // 'Cannot modify' (the same fetch already produced `old`, so
        // uninit-typed 'must not be accessed' still wins).
        match target {
            Expr::StaticProp { class, name } => {
                if let Ok(pn) = self.prop_name(name) {
                    if let Ok((cls, _)) = self.member_class_of(class) {
                        if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                            if let Some(sv) = pd.set_vis {
                                if self.set_vis_scope_denied(&dcls, sv) {
                                    return self.set_visibility_indirect_error(&dcls, &pd.name, sv);
                                }
                            }
                        }
                    }
                }
            }
            Expr::Prop { .. } => {
                if let Some((o, pn)) = &ro_target {
                    if let Some((pd, dcls)) = self.decl_prop(o, pn) {
                        if let Some(sv) = pd.set_vis {
                            if !self.hook_scope_allows(o, &dcls, pn, sv) {
                                return self.set_visibility_error(&dcls, &pd.name, sv);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        // `++`/`--` on a declared readonly prop is a direct slot write:
        // zend's 'Cannot modify readonly property' precedes the
        // increment verb — `$c->o++` on an object-held readonly prop
        // must not reach 'Cannot increment Inner' as the thrown error,
        // but zend still runs the increment-ability check and chains
        // its TypeError as `previous` (R3 finding 12, R4 finding 4 —
        // getPrevious()/the leading `Uncaught TypeError:` block).
        // Runs post-read so an uninitialized typed prop still reports
        // 'must not be accessed before initialization' first.
        if let Some((o, pn)) = &ro_target {
            if let Some((pd, dcls)) = self.decl_prop(o, pn) {
                if pd.readonly {
                    let dk = if pd.visibility == crate::ast::Visibility::Private {
                        format!("\0{}\0{}", dcls.name(), pd.name)
                    } else {
                        pd.name.clone()
                    };
                    if o.borrow().props.contains_key(&dk) {
                        let prev = if matches!(
                            old,
                            Value::Array(_)
                                | Value::Object(_)
                                | Value::Resource(_)
                                | Value::Callable(_)
                        ) {
                            let dir = if delta > 0 { "increment" } else { "decrement" };
                            let what = match &old {
                                Value::Array(_) => "array".to_string(),
                                Value::Object(ob) => ob.borrow().class.name().to_string(),
                                Value::Callable(_) => "Closure".to_string(),
                                _ => "resource".to_string(),
                            };
                            Some(self.exception("TypeError", &format!("Cannot {} {}", dir, what)))
                        } else {
                            None
                        };
                        let err = self.exception(
                            "Error",
                            &format!(
                                "Cannot modify readonly property {}::${}",
                                dcls.name(),
                                pd.name
                            ),
                        );
                        if let (Value::Object(eo), Some(p)) = (&err, prev) {
                            if let Some(ObjectInternal::Exception { previous, .. }) =
                                &mut eo.borrow_mut().internal
                            {
                                *previous = Some(p);
                            }
                        }
                        return Err(self.throw(err));
                    }
                }
            }
        }
        // Typed `int` prop can't overflow to float — a dedicated Error
        // instead of the generic assign TypeError (typed_properties_019).
        // The target's storage cell carries the owning prop's type —
        // "property" when the target IS that prop, "a reference held
        // by property" when reached through an alias/bound ref.
        // ++/-- on an *overloaded* prop (__get/__set magic) never
        // touches the backing cell — it reads a value and calls __set
        // (typed_properties_061); skip the ref-cell overflow check.
        let prop_overloaded = match target {
            Expr::Prop { obj, name, .. } => {
                let ov = self.eval(obj)?;
                let pn = self.prop_name(name)?;
                match &ov {
                    Value::Object(o) => self.decl_prop(o, &pn).is_none(),
                    _ => false,
                }
            }
            _ => false,
        };
        if !prop_overloaded && matches!(old, Value::Int(i) if i.checked_add(delta).is_none()) {
            let ent = self.eval_cell(target).ok().map(|c| {
                // Unaliased count is 3 (storage + typed_slots + this
                // temp); a `=&` bind adds a var slot -> "a reference
                // held by" (union_types/incdec_prop).
                let shared = Rc::strong_count(&c) > 3;
                let e = self
                    .typed_slots
                    .get(&(Rc::as_ptr(&c) as usize))
                    .map(|(cc, t, n, p)| (cc.clone(), t.clone(), n.clone(), p.clone()));
                (shared, e)
            });
            if let Some((shared, Some((_, tys, cn, cpn)))) = ent {
                if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                    && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                {
                    let dir = if delta > 0 { "increment" } else { "decrement" };
                    let bound = if delta > 0 { "maximal" } else { "minimal" };
                    // "a reference held by" once the slot is aliased —
                    // a `&$prop` bind makes the storage cell shared
                    // (union_types/incdec_prop).
                    let own = !shared
                        && match target {
                            Expr::Prop { name, .. } | Expr::StaticProp { name, .. } => {
                                self.prop_name(name).map(|pn| pn == cpn).unwrap_or(false)
                            }
                            _ => false,
                        };
                    let msg = if own {
                        format!(
                            "Cannot {} property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    } else {
                        format!(
                            "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    };
                    let mut e = PhpError::uncaught("TypeError", msg, 0);
                    e.thrown_line = Some(self.cur_line);
                    return self.fail(e);
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        self.store(target, new.clone())?;
        Ok(if post { old } else { new })
    }

    /// Var-rooted `$a[k1][k2]++`: zend's FETCH_DIM_RW+INC is one op —
    /// the container binds at op entry (a mid-eval rebind detaches the
    /// write onto the stale slot, silent and invisible) and each dim
    /// operand evaluates exactly once.
    fn incdec_dim(
        &mut self,
        root: &str,
        idxs: &[Option<&Expr>],
        delta: i64,
        post: bool,
    ) -> Result<Value, PhpError> {
        if !self.in_handler {
            self.dim_key_conv.clear();
            self.dim_cv_bound.clear();
            self.dim_undef_cells.clear();
        }
        // zend fetches the container AFTER the dim operands eval: a
        // throwing key (`$a[k]++`, k undef) wins over the fresh-var
        // 'Undefined variable' warning, which fires between them
        // ($a['k']++ warns $a then the missing key).
        let root_fresh = self.var_lookup(root).is_none();
        let arr_cell = self.var_cell(root);
        let det = DimDetach {
            name: root.to_string(),
            pre: DimPre::of(&arr_cell.borrow()),
            pre_cell: Rc::as_ptr(&arr_cell) as usize,
            coalesce: false,
        };
        // Container dispatch precedes the key eval (zend's FETCH_DIM_RW):
        // strings die with the incdec verb, ArrayAccess offsets go
        // through the by-ref offsetGet cell, scalars are the Error.
        match &*arr_cell.borrow() {
            Value::Str(_) => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot increment/decrement string offsets",
                    self.cur_line,
                ))
            }
            Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => {
                let o = o.clone();
                let key = match idxs {
                    [Some(ie)] => Some(self.eval(ie)?),
                    // `[]` on an overloaded object reads through the
                    // engine's dim machinery like `+=` — not a fatal
                    // (the temp result just can't write back).
                    [None] => None,
                    _ => {
                        return self.fail(PhpError::compile_fatal(
                            "Cannot use [] for reading",
                            self.cur_line,
                        ))
                    }
                };
                return self.incdec_aa(o, key, delta, post);
            }
            Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use a scalar value as an array",
                    self.cur_line,
                ))
            }
            _ => {}
        }
        // Eval each dim operand once — var keys bind inside the fetch
        // (zend's CV fetch warns once, at its op).
        let mut keys = Vec::with_capacity(idxs.len());
        for ix in idxs {
            match ix {
                Some(ie) => match Self::unmark_rhs(ie) {
                    Expr::Var(n) => keys.push(DimArg::Cv(n.clone(), Self::marked_line(ie))),
                    _ => keys.push(DimArg::from(self.dim_key(ie)?)),
                },
                None => keys.push(DimArg::Append),
            }
        }
        if root_fresh && !self.is_quiet() {
            self.warn(&format!("Undefined variable ${root}"))?;
        }
        let old = self.compound_dim_read(arr_cell.clone(), &keys, false, Some(&det), true)?;
        // zend's EG(exception) check at the fetch's end kills the
        // whole op for a non-final-level defer — no incdec, no write.
        if self
            .dim_throw
            .as_ref()
            .is_some_and(|(_, _, live, _)| !*live)
        {
            return self.dim_raise(old);
        }
        let new = self.incdec_value(&old, delta)?;
        // int-boundary overflow writes a float back — zend words the
        // typed-ref rejection `Cannot increment/decrement a reference
        // held by property ... past its {maximal,minimal} value`
        // (typed_properties_064); slot_write picks the context up.
        let oob = matches!(old, Value::Int(i) if i.checked_add(delta).is_none());
        let saved_ctx = std::mem::replace(
            &mut self.incdec_ref_ctx,
            if oob {
                Some(if delta > 0 {
                    ("increment", "maximal")
                } else {
                    ("decrement", "minimal")
                })
            } else {
                None
            },
        );
        let detached = self.dim_detached(&det);
        let target = if detached {
            cell(det.pre.value().unwrap_or(Value::Null))
        } else {
            arr_cell
        };
        let was = std::mem::replace(&mut self.detached_dim, detached);
        // INC_DIM's store-write dies under a pending throwable — the
        // slot the read side materialized stays NULL (oracle), so no
        // write may run.
        let r = if self.dim_throw.is_none() {
            self.assign_index_path(target, &keys, new.clone(), true, Some(&det), None)
        } else {
            Ok(Value::Null)
        };
        self.detached_dim = was;
        self.incdec_ref_ctx = saved_ctx;
        r?;
        self.dim_raise(if post { old } else { new })
    }

    /// `$o[k]++` on an ArrayAccess: `&offsetGet` hands back the real
    /// backing cell and ++/-- writes through it; a value-returning
    /// offsetGet yields a temp zend can't write back — it never falls
    /// back to offsetSet for ++/--, it just notices the indirect
    /// modification (spl and plain ArrayAccess alike).
    fn incdec_aa(
        &mut self,
        o: Rc<RefCell<PhpObject>>,
        key: Option<Value>,
        delta: i64,
        post: bool,
    ) -> Result<Value, PhpError> {
        let spl_iter = matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }));
        // A native-stub spl offsetGet reads through zend's
        // read_dimension(BP_VAR_RW), which hands ++/-- the store's own
        // cell — the write lands on storage directly (no offsetSet
        // dispatch, no overloaded-dim notice, no IS_REFERENCE mark).
        // Only a userland offsetGet override takes the engine path.
        let native = spl_iter && key.is_some() && self.is_native_offset(&o, "offsetGet");
        self.last_ret_cell = None;
        let was = std::mem::replace(&mut self.dim_by_ref, true);
        let r = if native {
            (|| -> Result<Value, PhpError> {
                let arr = self.ao_arr(&o);
                // Storage identity at fetch time — zend materializes
                // the miss bucket onto the table it read, so a
                // handler-side swap leaves the write on dead storage.
                let id = self.ao_storage_id(&o);
                let ak = self.ao_dim_key(&o, key.as_ref().unwrap())?;
                if let Some(e) = self.ao_sorting_err(&o) {
                    return self.fail(e);
                }
                if let Some(c) = arr.borrow().get_cell(&ak) {
                    self.last_ret_cell = Some(c.clone());
                    return Ok(c.borrow().clone());
                }
                // A missed ++/-- fetch still warns Undefined array key
                // (the by-ref create inside offsetGet stays silent) —
                // the new bucket then materializes for the write-back.
                // zend_error returns even when the handler throws (the
                // exception stays pending), so the slot materializes
                // before the warn's error propagates — a handler that
                // wrote the key is still overwritten by the NULL.
                let w = self.list_missing_key(&ak);
                if self.ao_storage_id(&o) == id {
                    match self.ao_src_obj(&o) {
                        Some(src) => {
                            self.ao_obj_dim_write(&o, &src, &arr, ak.clone(), Value::Null);
                        }
                        None => {
                            arr.borrow_mut().bind_cell(ak.clone(), cell(Value::Null));
                        }
                    }
                    self.last_ret_cell = arr.borrow().get_cell(&ak);
                } else {
                    // The fetched table died — zend's bucket went with
                    // it; the write-through only needs a scratch cell.
                    self.last_ret_cell = Some(cell(Value::Null));
                }
                w?;
                Ok(Value::Null)
            })()
        } else {
            self.aa_offset_get(&o, key.clone().map(cell), spl_iter, true)
        };
        self.dim_by_ref = was;
        // spl's overloaded-dim notice fires even when the engine call
        // threw — zend emits it while the error is still pending. A
        // native-path failure (key conversion, sort guard) never
        // reaches the overloaded-dim diagnostic.
        let rv = match r {
            Err(e) if spl_iter && !native => {
                let cn = o.borrow().class.name().to_string();
                self.notice(&format!(
                    "Indirect modification of overloaded element of {} has no effect",
                    cn
                ))?;
                return Err(e);
            }
            r => r?,
        };
        let rc = self.last_ret_cell.take();
        // A value-returning offsetGet can't be aliased — zend emits
        // the indirect-modification notice up front (object-typed
        // elements carry their own storage, none fires), still runs
        // incdec_value's own diagnostics (-- on null warns), and never
        // dispatches offsetSet (v7).
        if rc.is_none() && !matches!(&rv, Value::Object(_)) {
            let cn = o.borrow().class.name().to_string();
            self.notice(&format!(
                "Indirect modification of overloaded element of {} has no effect",
                cn
            ))?;
        }
        if let Some(c) = &rc {
            // Only a `&`-declared fetch marks — the native store cell
            // stays a plain zval through ++/-- (`&` shows up for `=&`
            // binds, not inc/dec).
            if !native {
                self.mark_ref(c);
            }
        }
        let old = rc.as_ref().map(|c| c.borrow().clone()).unwrap_or(rv);
        // int-typed backing cell can't overflow to float — dedicated
        // "past its minimal/maximal value" Error (065).
        if let (Value::Int(iv), Some(c)) = (old.clone(), rc.clone()) {
            if iv.checked_add(delta).is_none() {
                let ptr = Rc::as_ptr(&c) as usize;
                self.prune_typed_slot(ptr);
                let owners: Vec<(Vec<String>, String, String)> = self
                    .slot_owners
                    .get(&ptr)
                    .map(|os| {
                        os.iter()
                            .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                            .collect()
                    })
                    .or_else(|| {
                        self.typed_slots
                            .get(&ptr)
                            .map(|(_, t, n, p)| vec![(t.clone(), n.clone(), p.clone())])
                    })
                    .unwrap_or_default();
                for (tys, cn, cpn) in &owners {
                    if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                        && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                    {
                        let dir = if delta > 0 { "increment" } else { "decrement" };
                        let bound = if delta > 0 { "maximal" } else { "minimal" };
                        let mut e = PhpError::uncaught(
                            "TypeError",
                            format!(
                                "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                                dir,
                                cn,
                                cpn,
                                ty_disp(tys),
                                bound
                            ),
                            0,
                        );
                        e.thrown_line = Some(self.cur_line);
                        return self.fail(e);
                    }
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        // A non-ref offsetGet's ++/-- value dies with the op — zend
        // never dispatches offsetSet for it (v7). Append's missing-cell
        // notice already fired with the upfront rc.is_none() check.
        if let Some(c) = rc {
            let nv = self.typed_slot_store(&c, new.clone())?;
            *c.borrow_mut() = nv;
        }
        Ok(if post { old } else { new })
    }

    /// PHP inc/dec semantics: null++ = 1, null-- = null, strings increment
    /// alphanumerically (Perl-style), numeric strings go numeric.
    pub(in crate::interp) fn incdec_value(
        &mut self,
        v: &Value,
        delta: i64,
    ) -> Result<Value, PhpError> {
        Ok(match v {
            Value::Null => {
                if delta > 0 {
                    Value::Int(1)
                } else {
                    self.warn("Decrement on type null has no effect, this will change in the next major version of PHP")?;
                    Value::Null
                }
            }
            Value::Bool(_) => {
                // bools don't change, but PHP 8.3+ warns on inc/dec.
                let dir = if delta > 0 { "Increment" } else { "Decrement" };
                self.warn(&format!(
                    "{} on type bool has no effect, this will change in the next major version of PHP",
                    dir
                ))?;
                v.clone()
            }
            Value::Array(_) | Value::Object(_) | Value::Resource(_) | Value::Callable(_) => {
                let dir = if delta > 0 { "increment" } else { "decrement" };
                let what = match v {
                    Value::Array(_) => "array".to_string(),
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Callable(_) => "Closure".to_string(),
                    _ => "resource".to_string(),
                };
                return self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot {} {}", dir, what),
                    0,
                ));
            }
            // Int overflow on ++ promotes to float (postinc_basiclong_64bit).
            Value::Int(i) => match i.checked_add(delta) {
                Some(n) => Value::Int(n),
                None => Value::Float(*i as f64 + delta as f64),
            },
            Value::Float(f) => Value::Float(f + delta as f64),
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => match i.checked_add(delta) {
                    Some(n) => Value::Int(n),
                    None => Value::Float(i as f64 + delta as f64),
                },
                Numeric::Float(f) => Value::Float(f + delta as f64),
                Numeric::Leading(_, _) | Numeric::NonNumeric => {
                    // PHP 8.3+: inc/dec on a non-well-formed numeric or
                    // non-numeric string is deprecated and uses Perl-style
                    // alphanumeric increment (never decrements).
                    if delta > 0 {
                        self.deprecated(
                            "Increment on non-numeric string is deprecated, use str_increment() instead",
                        )?;
                        Value::bytes(perl_inc(s))
                    } else {
                        self.deprecated(
                            "Decrement on non-numeric string has no effect and is deprecated",
                        )?;
                        v.clone()
                    }
                }
            },
        })
    }

    fn unary(&mut self, op: &'static str, e: &Expr) -> Result<Value, PhpError> {
        match op {
            "!" => {
                let v = self.eval(e)?;
                Ok(Value::Bool(!v.is_truthy()))
            }
            "-" => {
                let v = self.eval(e)?;
                Ok(match v {
                    // -PHP_INT_MIN overflows → float.
                    Value::Int(i) => match i.checked_neg() {
                        Some(n) => Value::Int(n),
                        None => Value::Float(-(i as f64)),
                    },
                    Value::Float(f) => Value::Float(-f),
                    Value::Str(s) => match numeric(&s) {
                        Numeric::Int(i) => match i.checked_neg() {
                            Some(n) => Value::Int(n),
                            None => Value::Float(-(i as f64)),
                        },
                        Numeric::Float(f) => Value::Float(-f),
                        Numeric::Leading(f, is_int) => {
                            self.warn("A non-numeric value encountered")?;
                            if is_int {
                                Value::Int(-(f as i64))
                            } else {
                                Value::Float(-f)
                            }
                        }
                        Numeric::NonNumeric => {
                            // Unary minus lowers to `$s * -1`.
                            return self.fail(PhpError::uncaught(
                                "TypeError",
                                "Unsupported operand types: string * int",
                                0,
                            ));
                        }
                    },
                    _ => Value::Float(-v.to_float()),
                })
            }
            "+" => {
                let v = self.eval(e)?;
                Ok(match v {
                    Value::Int(_) | Value::Float(_) => v,
                    other => match numeric(&other.to_php_bytes()) {
                        Numeric::Int(i) => Value::Int(i),
                        Numeric::Float(f) => Value::Float(f),
                        Numeric::Leading(f, is_int) => {
                            self.warn("A non-numeric value encountered")?;
                            if is_int {
                                Value::Int(f as i64)
                            } else {
                                Value::Float(f)
                            }
                        }
                        Numeric::NonNumeric => {
                            return self.fail(PhpError::uncaught(
                                "TypeError",
                                format!(
                                    "Unsupported operand types: {} * int",
                                    other.operand_type_name()
                                ),
                                0,
                            ))
                        }
                    },
                })
            }
            "~" => {
                let v = self.eval(e)?;
                match &v {
                    // ~"abc" negates bytes.
                    Value::Str(s) => Ok(Value::bytes(s.iter().map(|b| !b).collect::<Vec<u8>>())),
                    Value::Int(_) | Value::Float(_) => Ok(Value::Int(!self.coerce_int(&v))),
                    // Zend throws a TypeError for non-int/string
                    // operands (unary_operators), not a coercion.
                    _ => self.fail(PhpError::uncaught(
                        "TypeError",
                        format!("Cannot perform bitwise not on {}", self.zval_type_name(&v)),
                        0,
                    )),
                }
            }
            "@" => {
                self.silence += 1;
                let v = self.eval(e);
                self.silence -= 1;
                v
            }
            _ => unreachable!(),
        }
    }

    fn binary(&mut self, op: &'static str, l: &Expr, r: &Expr) -> Result<Value, PhpError> {
        match op {
            "&&" => {
                let lv = self.eval(l)?;
                if !lv.is_truthy() {
                    return Ok(Value::Bool(false));
                }
                let rv = self.eval(r)?;
                Ok(Value::Bool(rv.is_truthy()))
            }
            "||" => {
                let lv = self.eval(l)?;
                if lv.is_truthy() {
                    return Ok(Value::Bool(true));
                }
                let rv = self.eval(r)?;
                Ok(Value::Bool(rv.is_truthy()))
            }
            "xor" => {
                let lv = self.eval(l)?.is_truthy();
                let rv = self.eval(r)?.is_truthy();
                Ok(Value::Bool(lv ^ rv))
            }
            "??" => {
                // isset() semantics: undefined vars, missing offsets and
                // uninitialized typed props fall through to the right.
                // isset_val returns the read value so calls and getters
                // evaluate exactly once.
                match self.isset_val_mode(l, 2)? {
                    Some(v) => Ok(v),
                    None => self.eval(r),
                }
            }
            "." => {
                let (lv, rv) = self.binary_operands(l, r)?;
                // zend ereallocs a uniquely-held LHS in place
                // (concat_function's refcount==1 fast path) — model
                // as a grow, not a fresh emalloc (cat3's oracle).
                let grow = matches!(&lv, Value::Str(s) if Rc::strong_count(&s.rc) == 1);
                let mut ls = self.conv_bytes(&lv)?;
                let rs = self.conv_bytes(&rv)?;
                ls.extend_from_slice(&rs);
                let nv = Value::bytes(ls);
                // zend emallocs the concat result zend_string.
                if let Value::Str(s) = &nv {
                    match &lv {
                        Value::Str(os) if grow => {
                            self.mem_grow_str(&os.rc, &s.rc, s.len() as u64 + 25)
                        }
                        _ => self.mem_track(&s.rc, s.len() as u64 + 25),
                    }
                }
                Ok(nv)
            }
            "==" | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
                let (lv, rv) = self.binary_operands(l, r)?;
                self.compare_op(op, l, r, &lv, &rv)
            }
            "named" => self.eval(r), // named-arg marker: value passthrough
            // `foldlit` (parse-fixed __LINE__/__NAMESPACE__): the value
            // is l; the marker keeps it non-literal for zend's scan
            // classification and literal-only compile checks.
            "foldlit" => self.eval(l),
            // `argline` (line marker on call args and their inner
            // sub-expressions): diagnostics during the eval attribute
            // to the operand's own first-token line (zend per-op
            // lines); `send_line` tracks it so engine throws and
            // engine-dispatched callbacks site there too.
            "argline" => {
                if let Expr::Int(n) = l {
                    self.cur_line = *n as usize;
                    self.send_line = Some(*n as usize);
                }
                self.eval(r)
            }
            _ => {
                let (lv, rv) = self.binary_operands(l, r)?;
                self.arith(op, lv, rv)
            }
        }
    }

    /// Operands of a binary op: Zend binds plain CVs at op-execution —
    /// i.e. after the right operand has run — so `$a . ($a=$b)` sees the
    /// assigned value. Other left expressions evaluate normally first
    /// (execution_order).
    fn binary_operands(&mut self, l: &Expr, r: &Expr) -> Result<(Value, Value), PhpError> {
        // The operand line markers must not mask the plain-CV shape
        // (or the deferred read would warn at the var's own line
        // instead of the op's right-operand line).
        let l_u = Self::unmark_rhs(l);
        // A folded varvar defers exactly like a CV: its name is
        // compile-time-constant, so it can be computed before `r`
        // runs (the const inner emits no ops — this early eval has
        // no observable effect).
        let name = match l_u {
            Expr::Var(n) => Some(n.clone()),
            Expr::VarVar(inner, _) if is_compile_const(inner) => {
                let n = self.eval(inner)?;
                Some(self.conv_str(&n)?)
            }
            _ => None,
        };
        if let Some(n) = name {
            // A CV right operand fuses into the same op too — zend
            // reads op1 then op2 inside it, both at the op's line
            // (the right operand's first-token line).
            let r_name = match Self::unmark_rhs(r) {
                Expr::Var(n) => Some(n.clone()),
                Expr::VarVar(inner, _) if is_compile_const(inner) => {
                    let v = self.eval(inner)?;
                    Some(self.conv_str(&v)?)
                }
                _ => None,
            };
            if let Some(rn) = r_name {
                let r_u = Self::unmark_rhs(r);
                let op = match self.scan_stamp {
                    Some(stamp) => self
                        .unfold_tail_site(r, stamp)
                        .or_else(|| Self::cv_site_of(r, r_u))
                        .unwrap_or(self.cur_line),
                    None => Self::cv_site_of(r, r_u).unwrap_or(self.cur_line),
                };
                self.cur_line = op;
                self.send_line = Some(op);
                let lv = match self.var_cell_opt(&n) {
                    Some(c) => c.borrow().clone(),
                    None => self.var_get(&n)?,
                };
                let rv = match self.var_cell_opt(&rn) {
                    Some(c) => c.borrow().clone(),
                    None => self.var_get(&rn)?,
                };
                return Ok((lv, rv));
            }
            let rv = self.eval(r)?;
            if let Some(stamp) = self.scan_stamp {
                // A scanned match/switch cond: the op stamps at
                // CG(zend_lineno) after the right operand's compile —
                // a folded leaf there carries the subject-end stamp.
                if let Some(l2) = self.unfold_tail_site(r, stamp) {
                    self.cur_line = l2;
                    self.send_line = Some(l2);
                }
            }
            // Re-lookup AFTER the right operand — `&`-binds can swap
            // the CV's cell (`$x == $x =& $z` reads $x post-bind).
            let lv = match self.var_cell_opt(&n) {
                Some(c) => c.borrow().clone(),
                None => self.var_get(&n)?,
            };
            return Ok((lv, rv));
        }
        let lv = self.eval(l)?;
        let rv = self.eval(r)?;
        if let Some(stamp) = self.scan_stamp {
            // Same scanned-cond stamp as the CV-left path: the op
            // sites at CG(zend_lineno) after the right operand's
            // compile (a folded leaf there = subject-end stamp).
            if let Some(l2) = self.unfold_tail_site(r, stamp) {
                self.cur_line = l2;
                self.send_line = Some(l2);
            }
        }
        Ok((lv, rv))
    }

    fn compare_op(
        &mut self,
        op: &str,
        l: &Expr,
        r: &Expr,
        a: &Value,
        b: &Value,
    ) -> Result<Value, PhpError> {
        // pass_two (zend_vm_set_opcode_handler) swaps the operands of the
        // COMMUTATIVE ops IS_EQUAL/IS_NOT_EQUAL/IS_IDENTICAL/
        // IS_NOT_IDENTICAL when op1's znode type ranks below op2's
        // (IS_CONST < IS_TMP_VAR < IS_VAR < IS_CV). The cyclic/protected
        // operand of zend_hash_compare is always the compare's left, so a
        // literal-left compare like `[[1,2]] == cyc()` actually runs
        // compare(cyc_result, literal) and the self-referencing array is
        // the marked one — re-entry throws "Nesting level too deep".
        // `<`/`<=`/`<=>` aren't commutative (source order kept); `>`/`>=`
        // emit as IS_SMALLER(_OR_EQUAL) on the reversed nodes.
        let (a, b) = match op {
            "==" | "!=" | "===" | "!=="
                if compare_operand_rank(Self::unmark_rhs(l))
                    < compare_operand_rank(Self::unmark_rhs(r)) =>
            {
                (b, a)
            }
            _ => (a, b),
        };
        crate::value::clear_cmp_depth_err();
        let v = match op {
            "===" => Value::Bool(identical(a, b)),
            "!==" => Value::Bool(!identical(a, b)),
            "==" => Value::Bool(compare(a, b) == Ordering::Equal),
            "!=" => Value::Bool(compare(a, b) != Ordering::Equal),
            "<=>" => Value::Int(match compare(a, b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }),
            "<" => Value::Bool(compare(a, b) == Ordering::Less),
            "<=" => Value::Bool(compare(a, b) != Ordering::Greater),
            // zend compiles `>`/`>=` as IS_SMALLER(_OR_EQUAL) with the
            // operands swapped — the RHS is the compare's protected
            // left operand for the cyclic depth check.
            ">" => Value::Bool(compare(b, a) == Ordering::Less),
            ">=" => Value::Bool(compare(b, a) != Ordering::Greater),
            _ => unreachable!(),
        };
        // Notices queued inside the compare (object→number casts)
        // already fired — emit them before a cyclic depth throw.
        self.emit_cmp_notices()?;
        if crate::value::cmp_depth_err() {
            return self.fail(PhpError::uncaught(
                "Error",
                "Nesting level too deep - recursive dependency?",
                self.cur_line,
            ));
        }
        Ok(v)
    }

    /// Arithmetic / bitwise with PHP numeric-string coercion.
    pub(in crate::interp) fn arith(
        &mut self,
        op: &str,
        l: Value,
        r: Value,
    ) -> Result<Value, PhpError> {
        match op {
            "&" | "|" | "^" => {
                if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
                    return Ok(Value::bytes(bitwise_str(op, a, b)));
                }
                // Zend's bitwise ops take the object-operator path when
                // either operand is an object: the error names the
                // OBJECT first and no scalar coercion/warning on the
                // other operand ever runs ('1 & obj' → 'stdClass & int').
                if matches!(l, Value::Object(_) | Value::Callable(_))
                    || matches!(r, Value::Object(_) | Value::Callable(_))
                {
                    let (a, b) = if matches!(l, Value::Object(_) | Value::Callable(_)) {
                        (&l, &r)
                    } else {
                        (&r, &l)
                    };
                    return self.fail(PhpError::uncaught(
                        "TypeError",
                        format!(
                            "Unsupported operand types: {} {} {}",
                            a.operand_type_name(),
                            op,
                            b.operand_type_name()
                        ),
                        0,
                    ));
                }
                let li = match self.bit_operand(op, &l, &r, false) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                let ri = match self.bit_operand(op, &r, &l, true) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                return Ok(Value::Int(match op {
                    "&" => li & ri,
                    "|" => li | ri,
                    _ => li ^ ri,
                }));
            }
            // PHP: shift < 0 → ArithmeticError; >= 64 → 0.
            "<<" | ">>" => {
                // PHP checks operand types left-to-right before shifting.
                let v = match self.bit_operand(op, &l, &r, false) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                let s = match self.bit_operand(op, &r, &l, true) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                if s < 0 {
                    return self.fail(PhpError::uncaught(
                        "ArithmeticError",
                        "Bit shift by negative number",
                        0,
                    ));
                }
                if s >= 64 {
                    return Ok(Value::Int(if op == "<<" { 0 } else { v >> 63 }));
                }
                return Ok(Value::Int(if op == "<<" {
                    v.wrapping_shl(s as u32)
                } else {
                    v.wrapping_shr(s as u32)
                }));
            }
            _ => {}
        }

        // `array + array` is PHP's union operator: lhs keys win and rhs
        // supplies only missing keys (not arithmetic — arrays never
        // reach the numeric path).
        if op == "+" {
            if let (Value::Array(a), Value::Array(b)) = (&l, &r) {
                let mut out = a.borrow().clone();
                for (k, c) in b.borrow().entries.iter() {
                    if out.get_cell(k).is_none() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                }
                return Ok(Value::Array(Rc::new(RefCell::new(out))));
            }
        }

        // `%` coerces each operand to int LEFT-to-RIGHT — the left's
        // float deprecation fires before the right's type check
        // ('1.5 % "x"' deprecates then 'float % string'), and a bad
        // left operand short-circuits the right's conversion
        // ('"x" % 1.5' throws 'string % float', no deprecation).
        if op == "%" {
            let a = match self.bit_operand(op, &l, &r, false) {
                Ok(i) => i,
                Err(e) => return self.fail(e),
            };
            let b = match self.bit_operand(op, &r, &l, true) {
                Ok(i) => i,
                Err(e) => return self.fail(e),
            };
            if b == 0 {
                return self.fail(PhpError::uncaught(
                    "DivisionByZeroError",
                    "Modulo by zero",
                    0,
                ));
            }
            // i64::MIN % -1 is 0 in PHP (no overflow panic).
            return Ok(Value::Int(a.wrapping_rem(b)));
        }

        let (ln, warn_l) = self.num(&l);
        let (rn, warn_r) = self.num(&r);
        if warn_l {
            self.warn("A non-numeric value encountered")?;
        }
        if warn_r {
            self.warn("A non-numeric value encountered")?;
        }
        let (ln, rn) = match (ln, rn) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return self.fail(PhpError::uncaught(
                    "TypeError",
                    format!(
                        "Unsupported operand types: {} {} {}",
                        l.operand_type_name(),
                        op,
                        r.operand_type_name()
                    ),
                    0,
                ))
            }
        };
        Ok(match op {
            "+" => num_bin(ln, rn, i64::checked_add, |a, b| a + b),
            "-" => num_bin(ln, rn, i64::checked_sub, |a, b| a - b),
            "*" => num_bin(ln, rn, i64::checked_mul, |a, b| a * b),
            "/" => {
                if rn.to_float() == 0.0 {
                    return self.fail(PhpError::uncaught(
                        "DivisionByZeroError",
                        "Division by zero",
                        0,
                    ));
                }
                match (ln, rn) {
                    (Num::I(a), Num::I(b)) if b != 0 => match a.checked_div(b) {
                        Some(q) if q * b == a => Value::Int(q),
                        _ => Value::Float(a as f64 / b as f64),
                    },
                    (a, b) => Value::Float(a.to_float() / b.to_float()),
                }
            }
            "**" => match (&ln, &rn) {
                // int ** int (exp >= 0) stays int when it fits
                // (sebastian/diff's footprint calc feeds an int return
                // type under strict_types).
                (Num::I(bi), Num::I(ei)) if *ei >= 0 => match bi.checked_pow(*ei as u32) {
                    Some(v) => Value::Int(v),
                    None => Value::Float(ln.to_float().powf(rn.to_float())),
                },
                _ => Value::Float(ln.to_float().powf(rn.to_float())),
            },
            _ => return self.fail(PhpError::fatal(format!("unsupported operator {}", op), 0)),
        })
    }

    /// Coerce a value to a number per PHP rules.
    /// Returns (numeric, "leading-numeric warning needed").
    fn num(&mut self, v: &Value) -> (Option<Num>, bool) {
        match v {
            Value::Int(i) => (Some(Num::I(*i)), false),
            Value::Float(f) => (Some(Num::F(*f)), false),
            Value::Bool(b) => (Some(Num::I(*b as i64)), false),
            Value::Null => (Some(Num::I(0)), false),
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => (Some(Num::I(i)), false),
                Numeric::Float(f) => (Some(Num::F(f)), false),
                Numeric::Leading(f, is_int) => {
                    if is_int {
                        (Some(Num::I(f as i64)), true)
                    } else {
                        (Some(Num::F(f)), true)
                    }
                }
                Numeric::NonNumeric => (None, false),
            },
            _ => (None, false),
        }
    }

    /// Operand coercion for integer-only binary ops (`& | ^ << >>`):
    /// leading-numeric strings warn "A non-numeric value encountered";
    /// non-numeric strings raise a catchable TypeError.
    fn bit_operand(
        &mut self,
        op: &str,
        v: &Value,
        other: &Value,
        swapped: bool,
    ) -> Result<i64, PhpError> {
        // 'Unsupported operand types' prints in source order — for the
        // right-operand check the args arrive swapped.
        let operand_err = |v: &Value, other: &Value| {
            let (a, b) = if swapped { (other, v) } else { (v, other) };
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Unsupported operand types: {} {} {}",
                    a.operand_type_name(),
                    op,
                    b.operand_type_name()
                ),
                0,
            )
        };
        if let Value::Str(s) = v {
            match numeric(s) {
                Numeric::Int(i) => return Ok(i),
                Numeric::Float(f) => {
                    let mut werr = None;
                    let i = coerce_float(f, |m| {
                        if let Err(e) = self.warn(m) {
                            werr = Some(e);
                        }
                    });
                    if let Some(e) = werr {
                        return Err(e);
                    }
                    if f.fract() != 0.0 {
                        self.deprecated(&format!(
                            "Implicit conversion from float-string \"{}\" to int loses precision",
                            String::from_utf8_lossy(s)
                        ))?;
                    }
                    return Ok(i);
                }
                Numeric::Leading(f, _) => {
                    self.warn("A non-numeric value encountered")?;
                    let mut werr = None;
                    let i = coerce_float(f, |m| {
                        if let Err(e) = self.warn(m) {
                            werr = Some(e);
                        }
                    });
                    if let Some(e) = werr {
                        return Err(e);
                    }
                    if f.fract() != 0.0 {
                        self.deprecated(&format!(
                            "Implicit conversion from float-string \"{}\" to int loses precision",
                            String::from_utf8_lossy(s)
                        ))?;
                    }
                    return Ok(i);
                }
                Numeric::NonNumeric => {
                    return Err(operand_err(v, other));
                }
            }
        }
        if let Value::Float(f) = v {
            if f.fract() != 0.0 && f.is_finite() && *f < i64::MAX as f64 && *f > i64::MIN as f64 {
                self.deprecated(&format!(
                    "Implicit conversion from float {} to int loses precision",
                    format_float_repr(*f)
                ))?;
            }
        }
        // Bitwise ops take int|string only: arrays, objects and other
        // containers are zend's 'Unsupported operand types' TypeError.
        match v {
            Value::Array(_) | Value::Object(_) | Value::Callable(_) => {
                return Err(operand_err(v, other));
            }
            _ => {}
        }
        Ok(self.coerce_int(v))
    }

    /// Int coercion for integer-only contexts (bitwise ops, shifts, casts).
    /// Out-of-range floats emit PHP's "not representable as an int" warning;
    /// conversion wraps modulo 2^64 (zend_dtoi64), NaN/INF → 0.
    pub(in crate::interp) fn coerce_int(&mut self, v: &Value) -> i64 {
        let f = match v {
            Value::Float(f) => *f,
            Value::Str(s) => match numeric(s) {
                Numeric::Float(f) | Numeric::Leading(f, _) => f,
                _ => return v.to_int(),
            },
            _ => return v.to_int(),
        };
        coerce_float(f, |msg| {
            let _ = self.warn(msg);
        })
    }

    /// `(type)expr` cast.
    fn cast(&mut self, kind: CastKind, v: Value) -> Result<Value, PhpError> {
        Ok(match kind {
            CastKind::Int => Value::Int(self.coerce_int(&v)),
            CastKind::Float => Value::Float(v.to_float()),
            CastKind::Bool => Value::Bool(v.is_truthy()),
            CastKind::Unset => Value::Null,
            CastKind::String => match &v {
                // Identity on strings — conv_str's UTF-8 decode would
                // mangle non-UTF-8 bytes.
                Value::Str(_) => v,
                _ => Value::str(self.conv_str(&v)?),
            },
            CastKind::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
                // `(array) $obj` exposes raw slots under their (possibly
                // mangled) keys — hooks are not run (dump.phpt).
                Value::Object(o) => {
                    let mut a = PhpArray::new();
                    // spl array-objects cast their STORAGE hash, not
                    // the object's own props (zend get_properties_for).
                    if matches!(
                        o.borrow().internal,
                        Some(crate::value::ObjectInternal::ArrayIter { .. })
                    ) {
                        let arr = self.ao_arr(&o);
                        for (k, c) in arr.borrow().iter() {
                            if let ArrKey::Tomb = k {
                                continue;
                            }
                            // zend's cast shares reference zvals — a
                            // `&`-bound bucket aliases into the cast
                            // array (writes through it hit the store);
                            // non-ref buckets copy.
                            if self.is_ref_cell(c) {
                                a.set_cell(k.clone(), c.clone());
                            } else {
                                a.set(k.clone(), c.borrow().clone());
                            }
                        }
                        return Ok(Value::Array(Rc::new(RefCell::new(a))));
                    }
                    let ob = o.borrow();
                    for n in &ob.prop_order {
                        if let Some(c) = ob.props.get(n) {
                            // Int-keyed buckets decode to int keys like
                            // zend's property-HT int slots.
                            let k = match crate::value::int_prop_index(n) {
                                Some(i) => ArrKey::Int(i),
                                None => ArrKey::Str(n.clone().into()),
                            };
                            a.set(k, c.borrow().clone());
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                other => {
                    let mut a = PhpArray::new();
                    a.push(other);
                    Value::Array(Rc::new(RefCell::new(a)))
                }
            },
            CastKind::Object => match &v {
                Value::Object(_) => v,
                _ => {
                    let mut props = HashMap::new();
                    let mut order = Vec::new();
                    match &v {
                        Value::Array(a) => {
                            for (k, c) in a.borrow().iter() {
                                let name = match k {
                                    ArrKey::Int(i) => i.to_string(),
                                    ArrKey::Str(s) => s.to_string(),
                                    ArrKey::Tomb => continue,
                                };
                                props.insert(name.clone(), cell(c.borrow().clone()));
                                order.push(name);
                            }
                        }
                        Value::Null => {}
                        _ => {
                            props.insert("scalar".into(), cell(v.clone()));
                            order.push("scalar".into());
                        }
                    }
                    let cls = self
                        .classes
                        .get("stdclass")
                        .cloned()
                        .expect("stdClass registered");
                    Value::Object(self.alloc_obj(PhpObject {
                        class: cls,
                        props,
                        prop_order: order,
                        id: 0,
                        internal: None,
                        unset_props: std::collections::HashSet::new(),
                    }))
                }
            },
        })
    }
}

/// String-offset key classes (zend `zend_check_string_offset`): a clean
/// int, a leading-int-with-junk that warns and still indexes, or an
/// illegal key.
enum StrOffKey {
    Int(i64),
    Junk(i64),
    Bad,
}

enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn to_float(&self) -> f64 {
        match self {
            Num::I(i) => *i as f64,
            Num::F(f) => *f,
        }
    }
}

fn num_bin(a: Num, b: Num, fi: fn(i64, i64) -> Option<i64>, ff: fn(f64, f64) -> f64) -> Value {
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
fn perl_inc(s: &[u8]) -> Vec<u8> {
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
fn coerce_float(f: f64, mut warn: impl FnMut(&str)) -> i64 {
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

fn bitwise_str(op: &str, a: &[u8], b: &[u8]) -> Vec<u8> {
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

/// `static` declarations anywhere in a body, with their default exprs
/// and the `static` keyword's line — nested function/class bodies
/// declare their own.
fn closure_static_vars(stmts: &[Stmt], out: &mut Vec<(String, Option<Expr>, usize)>) {
    use crate::ast::Stmt;
    for st in stmts {
        match st {
            Stmt::Static { vars, .. } => {
                for (n, d, vl) in vars {
                    if !out.iter().any(|(x, ..)| x == n) {
                        out.push((n.clone(), d.clone(), *vl));
                    }
                }
            }
            Stmt::Block(b) => closure_static_vars(b, out),
            Stmt::If { then, else_, .. } => {
                closure_static_vars(then, out);
                closure_static_vars(else_, out);
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::Foreach { body, .. } => closure_static_vars(body, out),
            Stmt::Switch { cases, .. } => {
                for (_, b) in cases {
                    closure_static_vars(b, out);
                }
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                closure_static_vars(body, out);
                for c in catches {
                    closure_static_vars(&c.body, out);
                }
                if let Some(f) = finally {
                    closure_static_vars(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Compile-time bindable `static` initializer: literals and ops on
/// literals only. Zend resolves user consts/`new`/calls when the
/// `static` statement runs, not at closure creation (probe_sv3), so
/// those stay NULL in the seeded table — except engine consts
/// (PHP_VERSION, ...), which Zend binds at creation (probe_sv_engine).
fn literal_static_init(e: &Expr, engine: &std::collections::HashSet<String>) -> bool {
    match e {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::MagicConst(_) => true,
        Expr::Const(n) => engine.contains(n.trim_start_matches('\\')),
        Expr::ArrayLit(items) => items.iter().all(|(k, v)| {
            k.as_ref()
                .map(|k| literal_static_init(k, engine))
                .unwrap_or(true)
                && literal_static_init(v, engine)
        }),
        Expr::Interp(parts) => parts
            .iter()
            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))),
        Expr::Paren(inner) | Expr::ByRef(inner) | Expr::Unary { e: inner, .. } => {
            literal_static_init(inner, engine)
        }
        Expr::Cast { e: inner, .. } => literal_static_init(inner, engine),
        Expr::Binary { l, r, .. } => {
            literal_static_init(l, engine) && literal_static_init(r, engine)
        }
        Expr::Ternary { c, t, f, .. } => {
            literal_static_init(c, engine)
                && t.as_ref()
                    .map(|t| literal_static_init(t, engine))
                    .unwrap_or(true)
                && literal_static_init(f, engine)
        }
        _ => false,
    }
}

/// str_offset_write's outcome — the byte stored, or nothing when the
/// 'Illegal string offset' warning fires (zend leaves the write out).
enum OffWrite {
    Stored(u8),
    Skipped,
}
