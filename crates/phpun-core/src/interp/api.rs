//! Builtin-facing surface: output buffering (`ob_*` machinery and
//! handlers), shutdown/error/exception handler stacks, and the small
//! public helpers `builtins.rs`/`serve.rs` call through.

use super::util::*;
use super::*;
use crate::value::{trace_frame_hidden, trace_frame_str_at};

impl<'a> Interp<'a> {
    /// Flush all output buffers at script end, innermost first so each
    /// level's handler output lands in its parent's buffer (bug24951).
    pub(in crate::interp) fn flush_ob_all(&mut self) {
        // Still-open buffers a suspended gen opened are real Zend
        // stack levels — restore the ones the consumer's cursor
        // actually reached (in stack order) so the end-of-request
        // flush covers them; dead capture mirrors (already popped by
        // the body) and windows whose open tag was never passed are
        // dropped — Zend never ran those resumes.
        let sus = std::mem::take(&mut self.suspended_obs);
        self.ob_stack.extend(sus.into_iter().filter(|l| {
            l.gen_close.is_none()
                && l.gen_open
                    .is_some_and(|o| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos >= o))
        }));
        while !self.ob_stack.is_empty() {
            // A killed gen's window: journaled tail bytes that merged
            // into buf before the kill never ran in Zend's frame —
            // rebuild the real content like the dead-gen teardown.
            if let Some(l) = self.ob_stack.last_mut() {
                let dead = l
                    .gen_q
                    .as_ref()
                    .map(|q| {
                        let f = q.borrow();
                        (f.vis_pos, f.killed, f.finished && f.vis_pos >= f.total)
                    })
                    .unwrap_or((usize::MAX, false, true));
                let dead = (
                    dead.0,
                    dead.1
                        || (!dead.2 && l.gen_state.as_ref().is_some_and(|w| w.upgrade().is_none())),
                );

                if dead.1 && !l.drained_segs.is_empty() {
                    let v = Self::ob_level_content(l, dead.0, true);
                    l.buf = v;
                    l.drained_segs.clear();
                    l.gen_drained = 0;
                }
                // A live gen window's buffer resolves at teardown:
                // consumer captures splice into cursor position —
                // the body's eager reads of the stale content
                // (ob_get_contents & friends stored in CVs) rewrite
                // to the resolved value, and the flush itself
                // carries the spliced ordering.
                if l.gen_q.is_some() && !l.caps.is_empty() {
                    let caps = std::mem::take(&mut l.caps);
                    let old_v = Self::ob_level_content(l, dead.0, dead.1);
                    l.caps = caps;
                    let new_v = Self::ob_level_content(l, dead.0, dead.1);
                    if let Some(gs) = &l.gen_state {
                        if let Some(st) = gs.upgrade() {
                            Self::gen_patch_values(&mut st.borrow_mut(), &old_v, &new_v);
                        }
                    }
                    l.buf = new_v;
                    l.drained_segs.clear();
                    l.cap_segs.clear();
                    l.gen_drained = 0;
                }
                // Eager buffer reads the body stored (ob_get_contents
                // & friends) resolve against the captures each read's
                // resume had seen — the same close-time rewrite a
                // pop mirror performs.
                if l.gen_q.is_some() && !l.read_vals.is_empty() {
                    let reads = std::mem::take(&mut l.read_vals);
                    if let Some(gs) = &l.gen_state {
                        if let Some(st) = gs.upgrade() {
                            let mut st = st.borrow_mut();
                            for (k, rhead, rsegs) in &reads {
                                let caps: Vec<(usize, Vec<u8>)> =
                                    l.caps.iter().filter(|(t, _)| *t < *k).cloned().collect();
                                let mut stale = rhead.clone();
                                for (_, s) in rsegs {
                                    stale.extend_from_slice(s);
                                }
                                let mut v = rhead.clone();
                                v.extend_from_slice(&crate::interp::ob_splice(&[], rsegs, &caps));
                                Self::gen_patch_values(&mut st, &stale, &v);
                            }
                        }
                    }
                }
            }

            let r = self.ob_invoke(8);
            self.ob_stack.pop();
            if let Ok(Some(s)) = r {
                self.emit_bytes(&s);
            }
        }
    }

    /// Invoke the top level's handler with `mode | START` on first call,
    /// clearing the buffer first (bug24951 flag semantics:
    /// START=1, CLEAN=2, FLUSH=4, FINAL=8). Returns the handler's output
    /// — or the raw buffer when there is no handler.
    fn ob_invoke(&mut self, mode: i64) -> Result<Option<Vec<u8>>, PhpError> {
        // Cursor-past journaled gen captures join the buffer first.
        self.ob_drain_pending();
        let (handler, buf, already) = match self.ob_stack.last_mut() {
            Some(l) => {
                let buf = std::mem::take(&mut l.buf);
                let st = l.started;
                l.started = true;
                (l.handler.clone(), buf, st)
            }
            None => return Ok(None),
        };
        match handler {
            Some(h) => {
                let m = mode | if already { 0 } else { 1 };
                // A handler throwing inside ob_end_clean propagates as an
                // uncaught exception (bug32828).
                self.internal_cb += 1;
                let out = self.call_value(
                    &h,
                    CallArgs::positional(vec![cell(Value::bytes(buf)), cell(Value::Int(m))]),
                );
                self.internal_cb -= 1;
                Ok(Some(out?.to_php_bytes()))
            }
            None => Ok(Some(buf)),
        }
    }

    /// String conversion for builtins (__toString-aware, never errors → "" on failure).
    pub fn to_string_of(&mut self, v: &Value) -> String {
        self.conv_str(v).unwrap_or_else(|_| v.to_php_string())
    }
    /// Byte-faithful variant — for binary-safe builtins.
    pub fn to_bytes_of(&mut self, v: &Value) -> Vec<u8> {
        self.conv_bytes(v).unwrap_or_else(|_| v.to_php_bytes())
    }
    /// Fallible byte cast — array_diff/intersect emulate Zend's
    /// `zval_get_tmp_string`, which yields "" with the cast Error left
    /// pending instead of aborting the builtin.
    pub fn try_conv_bytes(&mut self, v: &Value) -> Result<Vec<u8>, PhpError> {
        self.conv_bytes(v)
    }
    /// Take the throwable object a failed cast stashed (ErrorKind::Throw).
    pub fn take_pending_exception(&mut self) -> Option<Value> {
        self.pending_exception.take()
    }
    /// Re-raise a deferred throwable as a builtin's error result.
    pub fn throw_value(&mut self, v: Value) -> PhpError {
        self.throw(v)
    }
    /// Variable lookup for compact() — reads current scope quietly.
    pub fn lookup_var(&mut self, name: &str) -> Option<Value> {
        self.var_cell_opt(name).map(|c| c.borrow().clone())
    }
    pub fn error_handler(&self) -> Option<Value> {
        self.error_handler.clone()
    }
    pub fn exception_handler(&self) -> Option<Value> {
        self.exception_handler.clone()
    }
    /// `$obj instanceof X` helper for builtins.
    pub fn obj_is_a(&mut self, o: &Rc<RefCell<PhpObject>>, name: &str) -> bool {
        let cls = o.borrow().class.clone();
        self.is_a(&cls, name)
    }
    /// class-name-string is-a check for is_subclass_of.
    pub fn obj_is_a_str(&mut self, cls_name: &str, name: &str) -> bool {
        match self.classes.get(&cls_name.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, name),
            None => false,
        }
    }
    /// `is_subclass_of`: subject may be a class OR interface name —
    /// the target is a parent class or any transitively implemented /
    /// extended interface. Self-match is false, and the subject name
    /// autoloads (zend behavior).
    pub fn is_subclass_name(&mut self, sub: &str, target: &str) -> Result<bool, PhpError> {
        let sub_l = sub.trim_start_matches('\\').to_lowercase();
        let tgt_l = target.trim_start_matches('\\').to_lowercase();
        if sub_l == tgt_l {
            return Ok(false);
        }
        // Only an UNREGISTERED name autoloads (zend lookup_class) —
        // builtin classes/interfaces resolve without a loader call,
        // and the loader receives the name as written, not lowered.
        if !self.classes.contains_key(&sub_l) && !self.interfaces.contains_key(&sub_l) {
            self.run_autoload(sub.trim_start_matches('\\'))?;
        }
        if let Some(c) = self.classes.get(&sub_l).cloned() {
            return Ok(self.is_a(&c, &tgt_l));
        }
        // Interface subject: its parent interfaces live in
        // `implements` (zend stores them as interface parents).
        if let Some(iface) = self.interfaces.get(&sub_l).cloned() {
            let mut stack = vec![iface];
            while let Some(f) = stack.pop() {
                for p in &f.implements {
                    if p.trim_start_matches('\\').eq_ignore_ascii_case(&tgt_l) {
                        return Ok(true);
                    }
                    if let Some(ff) = self.interfaces.get(&p.to_lowercase()) {
                        stack.push(ff.clone());
                    }
                }
            }
        }
        Ok(false)
    }
    // public helpers for builtins
    pub fn ob_push(&mut self, handler: Option<Value>) {
        // A buffer opened inside a generator body is a real global
        // buffer in Zend — it survives the body's suspends and
        // captures consumer writes too. Tag the level with the
        // cursor position its ob_start ran at: it stays on the stack
        // during the body run, detaches at suspend, and
        // rematerializes once the consumer's cursor reaches it
        // (ob_suspend/ob_promote).
        let (gen_q, gen_open, gen_state) = match &self.gen_run_state {
            Some(s) => {
                let done = self
                    .gen_sink
                    .as_ref()
                    .map(|k| k.borrow().len())
                    .unwrap_or(0);
                // A send()/throw() prefix re-run replays this push —
                // the level the first run parked is the same buffer
                // (Zend's stack persists): reactivate it so the
                // replay's writes capture there. A replayed push
                // whose level was already popped falls through to a
                // live level — the replayed pop folds it back.
                if self.gen_horizon_suppresses(done) {
                    let fq = s.borrow().fin_q.clone();
                    if let Some(p) = self.suspended_obs.iter().position(|m| {
                        m.gen_q
                            .as_ref()
                            .is_some_and(|mq| std::rc::Rc::ptr_eq(mq, &fq))
                            && m.gen_open == Some(done)
                            && m.gen_close.is_none()
                            && m.pop_head.is_none()
                    }) {
                        let mut m = self.suspended_obs.remove(p);

                        // The re-run regenerates its journaled
                        // captures — stale ones would echo twice.
                        m.gen_pending.clear();
                        m.gen_drained = 0;
                        m.drained_segs.clear();
                        self.ob_stack.push(m);
                        return;
                    }
                    let live = self.ob_stack.iter().any(|m| {
                        m.gen_q
                            .as_ref()
                            .is_some_and(|mq| std::rc::Rc::ptr_eq(mq, &fq))
                            && m.gen_open == Some(done)
                            && m.pop_head.is_none()
                    });
                    if live {
                        return;
                    }
                }
                (
                    Some(s.borrow().fin_q.clone()),
                    Some(done),
                    Some(std::rc::Rc::downgrade(s)),
                )
            }
            None => (None, None, None),
        };
        self.ob_stack.push(ObLevel {
            buf: Vec::new(),
            handler,
            started: false,
            gen_q,
            gen_open,
            gen_close: None,
            gen_pending: Vec::new(),
            gen_drained: 0,
            pop_head: None,
            pop_segs: Vec::new(),
            drained_segs: Vec::new(),
            caps: Vec::new(),
            cap_segs: Vec::new(),
            read_vals: Vec::new(),
            suspend_base: 0,
            gen_state,
        });
    }

    /// Pop the top buffer — a gen-opened one popped by the body
    /// keeps a suspended capture window until the consumer's cursor
    /// passes its close tag (Zend's global buffer still exists
    /// between the body's yield and its pop). A body re-run that
    /// replays the pop folds the live mirror's consumer captures
    /// into the returned value instead of registering a second
    /// window.
    fn ob_pop(&mut self) -> Option<ObLevel> {
        self.ob_pop_snap(None)
    }

    /// `snap` supplies the buffer contents for the mirror split when
    /// the caller already consumed `buf` (ob_end_flush's handler
    /// invoke empties it before the pop).
    fn ob_pop_snap(&mut self, snap: Option<Vec<u8>>) -> Option<ObLevel> {
        // Pop mirrors are bookkeeping for a buffer the body already
        // consumed — Zend's stack has no such level, so every pop
        // (body or consumer) lands on the topmost real level.
        let i = self
            .ob_stack
            .iter()
            .rposition(|l| l.pop_head.is_none())
            .or_else(|| {
                // Consumer-side pops reach into a suspended gen
                // window — at the consumer's cursor the body's pop
                // hasn't logically run, so the mirror IS the real
                // buffer for them.
                if self.gen_run_state.is_none() {
                    self.ob_stack.iter().rposition(|l| l.gen_q.is_some())
                } else {
                    None
                }
            })?;
        let mut l = self.ob_stack.remove(i);
        if l.pop_head.is_some() {
            // The consumer stole the window: return its real-time
            // content; the body's pop Zend-wise now sees whatever
            // remains (here: nothing) — clear the journaled value.
            let mut v = l.pop_head.clone().unwrap_or_default();
            for (_, s) in &l.pop_segs {
                v.extend_from_slice(s);
            }
            v.extend_from_slice(&crate::interp::ob_splice(&[], &[], &l.caps));
            let old_v = v.clone();
            let min_tag = l.gen_close.unwrap_or(0).saturating_sub(1);
            if !old_v.is_empty() {
                if let Some(gs) = &l.gen_state {
                    if let Some(st2) = gs.upgrade() {
                        let mut st2 = st2.borrow_mut();
                        for (t, b, ..) in &mut st2.pending_out {
                            if *t >= min_tag {
                                Self::bytes_replace(b, &old_v, &[]);
                            }
                        }
                        Self::gen_patch_values(&mut st2, &old_v, &[]);
                    }
                }
                if let Some(q) = &l.gen_q {
                    for (t, b, _) in &mut q.borrow_mut().bytes {
                        if *t >= min_tag {
                            Self::bytes_replace(b, &old_v, &[]);
                        }
                    }
                }
            }
            l.buf = v;
            return Some(l);
        }
        let content = snap.unwrap_or_else(|| l.buf.clone());
        if l.gen_open.is_some() && self.gen_run_state.is_some() && l.gen_close.is_none() {
            let close = self.gen_sink.as_ref().map(|s| s.borrow().len());
            let existing = match (&l.gen_q, close) {
                (Some(q), Some(c)) => self
                    .ob_stack
                    .iter()
                    .chain(self.suspended_obs.iter())
                    .find(|m| {
                        m.gen_q
                            .as_ref()
                            .is_some_and(|mq| std::rc::Rc::ptr_eq(mq, q))
                            && m.gen_close == Some(c)
                    })
                    .map(|m| (m.pop_head.clone(), m.caps.clone())),
                _ => None,
            };
            // Split `content` (a clone of the level's buf at pop)
            // into the bytes before the drained stream and per-tag
            // segments — drained_segs carry their recorded offsets so
            // mid-buffer drains (consumer writes between segs) stay
            // positional.
            let split_parts = |content: &[u8]| -> (Vec<u8>, Vec<(usize, Vec<u8>)>) {
                crate::interp::ob_split_view(&l, content)
            };
            if let Some((_mhead, caps)) = existing {
                // Replayed pop: the mirror already captured the
                // consumer writes the shared Zend buffer also held —
                // the returned value splices them into this pop's
                // own segment stream in real write order.
                let (head, segs) = split_parts(&content);
                let mut v = head;
                v.extend_from_slice(&crate::interp::ob_splice(&[], &segs, &caps));
                l.buf = v;
            } else {
                // Register the window mirror: keep the pop value
                // split into a head and per-tag segments so consumer
                // captures splice in real write order at close.
                let (head, mut segs) = split_parts(&content);
                let mut read_vals = l.read_vals.clone();
                if !content.is_empty() {
                    let k = self
                        .gen_sink
                        .as_ref()
                        .map(|s| s.borrow().len())
                        .unwrap_or(0);
                    read_vals.push((k, head.clone(), segs.clone()));
                }
                if let Some(prev) = segs.last_mut() {
                    if prev.0 == usize::MAX {
                        prev.0 = close.unwrap_or(0).saturating_sub(1);
                    }
                }
                let gen_state = self.gen_run_state.as_ref().map(std::rc::Rc::downgrade);
                self.suspended_obs.push(ObLevel {
                    buf: Vec::new(),
                    handler: None,
                    started: true,
                    gen_q: l.gen_q.clone(),
                    gen_open: l.gen_open,
                    gen_close: close,
                    gen_pending: Vec::new(),
                    gen_drained: 0,
                    pop_head: Some(head),
                    pop_segs: segs,
                    drained_segs: Vec::new(),
                    caps: Vec::new(),
                    cap_segs: Vec::new(),
                    // The pop's own returned content is itself a
                    // stale view — consumer captures arriving before
                    // the window closes belong inside it.
                    read_vals,
                    suspend_base: 0,
                    gen_state,
                });
            }
        }
        Some(l)
    }
    /// ob_end_clean: handler(mode=CLEAN|FINAL) result discarded, pop.
    pub fn ob_end_clean(&mut self) -> Result<(), PhpError> {
        self.ob_invoke(10)?;
        self.ob_pop();
        Ok(())
    }
    /// ob_end_flush: handler(mode=FINAL) result emitted to parent, pop.
    pub fn ob_end_flush(&mut self) -> Result<(), PhpError> {
        self.ob_drain_pending();
        let raw = self.ob_stack.last().map(|l| l.buf.clone());
        let r = self.ob_invoke(8)?;
        self.ob_pop_snap(raw);
        if let Some(s) = r {
            self.emit_bytes(&s);
        }
        Ok(())
    }
    /// ob_flush: handler(mode=FLUSH) result emitted to the PARENT level
    /// (the level is briefly popped so emit can't feed back into it),
    /// buffer cleared, level stays open (bug24951).
    pub fn ob_flush(&mut self) -> Result<(), PhpError> {
        if let Some(s) = self.ob_invoke(4)? {
            let level = self.ob_stack.pop();
            self.emit_bytes(&s);
            if let Some(l) = level {
                self.ob_stack.push(l);
            }
        }
        Ok(())
    }
    /// ob_clean: handler(mode=CLEAN) result discarded, buffer cleared.
    pub fn ob_clean(&mut self) -> Result<(), PhpError> {
        self.ob_invoke(2)?;
        Ok(())
    }
    /// ob_get_clean: raw buffer, NO handler invocation, pop.
    pub fn ob_get_clean(&mut self) -> Value {
        self.ob_drain_pending();
        self.ob_pop()
            .map(|l| Value::bytes(l.buf))
            .unwrap_or(Value::Bool(false))
    }
    /// ob_get_flush: handler(mode=FINAL) result emitted, RAW buffer
    /// returned, level popped.
    pub fn ob_get_flush(&mut self) -> Result<Value, PhpError> {
        self.ob_drain_pending();
        let raw = self.ob_stack.last().map(|l| l.buf.clone());
        let r = self.ob_invoke(8)?;
        self.ob_pop_snap(raw.clone());
        if let Some(s) = r {
            self.emit_bytes(&s);
        }
        Ok(raw.map(Value::bytes).unwrap_or(Value::Bool(false)))
    }
    pub fn ob_top(&mut self) -> Option<&Vec<u8>> {
        self.ob_promote();
        self.ob_drain_pending();
        if self.gen_run_state.is_some() {
            if let Some(l) = self.ob_stack.last_mut() {
                // A buffer view the body materializes eagerly —
                // consumer captures landing between the suspend and
                // this read's resume rewrite it at window close.
                if l.gen_q.is_some() && !l.buf.is_empty() {
                    let k = self
                        .gen_sink
                        .as_ref()
                        .map(|s| s.borrow().len())
                        .unwrap_or(0);
                    let (head, segs) = crate::interp::ob_split_view(l, &l.buf.clone());
                    l.read_vals.push((k, head, segs));
                }
            }
        }
        self.ob_stack.last().map(|l| &l.buf)
    }
    pub fn ob_len(&mut self) -> usize {
        self.ob_promote();
        // Suspended gen-owned buffers stay on Zend's shared stack
        // until their window closes — count the live ones too.
        self.ob_stack.len() + self.ob_suspended_visible()
    }
    pub fn register_shutdown(&mut self, f: Value, args: Vec<Cell>) {
        self.shutdown_fns.push((f, args));
    }
    pub fn set_error_handler(&mut self, f: Option<Value>) {
        // Zend keeps a stack: set pushes; restore pops the previous top.
        if let Some(cur) = self.error_handler.take() {
            self.error_handler_stack.push(cur);
        }
        self.error_handler = f;
    }

    pub fn restore_error_handler(&mut self) {
        self.error_handler = self.error_handler_stack.pop();
    }
    pub fn set_exception_handler(&mut self, f: Option<Value>) {
        if let Some(cur) = self.exception_handler.take() {
            self.exception_handler_stack.push(cur);
        }
        self.exception_handler = f;
    }

    pub fn restore_exception_handler(&mut self) {
        self.exception_handler = self.exception_handler_stack.pop();
    }
    pub fn cur_frame(&mut self) -> Option<&Frame> {
        self.stack.last()
    }
    /// Args of the currently-executing function.
    /// True while executing inside a function/method call frame.
    pub fn in_call(&self) -> bool {
        !self.stack.is_empty()
    }

    pub fn frame_args(&self) -> &[Cell] {
        self.stack.last().map(|f| f.args.as_slice()).unwrap_or(&[])
    }
    pub fn define_const(&mut self, name: &str, v: Value) {
        self.constants.insert(name.to_string(), v);
    }
    pub fn const_defined(&self, name: &str) -> bool {
        self.constants.contains_key(name)
            || self.constants.contains_key(name.trim_start_matches('\\'))
    }
    pub fn const_get(&self, name: &str) -> Option<Value> {
        self.constants
            .get(name)
            .or_else(|| self.constants.get(name.trim_start_matches('\\')))
            .cloned()
    }
    pub fn next_res_id(&mut self) -> u64 {
        self.res_counter += 1;
        self.res_counter
    }
    pub fn set_resource(&mut self, _r: PhpResource) {}
    pub fn lookup_class(&self, name: &str) -> Option<Rc<PhpClass>> {
        self.classes
            .get(&name.trim_start_matches('\\').to_lowercase())
            .cloned()
    }
    /// Drop any pending thrown exception (native-failure soft-error
    /// paths that warn + return false instead of propagating).
    pub fn clear_pending_exception(&mut self) {
        self.pending_exception = None;
    }
    /// get_called_class(): late-static-binding class of the current
    /// frame, `false` outside a called-class context (static_get_called_class).
    pub fn called_class_name(&mut self) -> Value {
        match self.stack.last().and_then(|f| f.called_class.clone()) {
            Some(c) => Value::str(c.name().to_string()),
            None => Value::Bool(false),
        }
    }

    /// get_class() (no args): the executing class scope — the current
    /// method's DECLARING class, None outside a class context.
    pub fn executed_scope_name(&self) -> Option<String> {
        self.stack
            .last()
            .and_then(|f| f.scope_class.as_ref().or(f.decl_class.as_ref()).cloned())
            .map(|c| c.name().to_string())
    }

    /// property_exists(): instance prop declared on the class or any
    /// ancestor (property002).
    pub fn class_has_prop(&self, c: &Rc<PhpClass>, name: &str) -> bool {
        let mut cur = Some(c.clone());
        while let Some(k) = cur {
            if k.decl.props.iter().any(|p| p.name == name) {
                return true;
            }
            cur = k
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }
    pub fn instantiate_class(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        self.new_instance(name, CallArgs::positional(args))
    }
    pub fn call_closure(
        &mut self,
        c: &Rc<PhpCallable>,
        args: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        self.call_value(&Value::Callable(c.clone()), CallArgs::positional(args))
    }
    pub fn var_name_set(&mut self, name: &str, v: Value) {
        self.var_set(name, v);
    }
    pub fn warn_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.warn(msg)
    }

    /// Run `f` with diagnostics suppressed — zend's inner stream opens
    /// (the php://filter wrapper's resource= target) fail silently; the
    /// wrapper reports the generic failure itself.
    pub fn silenced_pub<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.silence += 1;
        let r = f(self);
        self.silence -= 1;
        r
    }

    /// Backtrace frames (innermost first) for an E_ERROR raised inside a
    /// builtin — Zend attaches the call stack to runtime fatals.
    pub fn fatal_frames(&self) -> Vec<String> {
        self.call_trace
            .iter()
            .rev()
            .filter(|f| !trace_frame_hidden(f))
            .enumerate()
            .map(|(i, f)| trace_frame_str_at(f, i))
            .collect()
    }

    pub fn deprecated_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.deprecated(msg)
    }

    pub fn notice_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.notice(msg)
    }

    /// Diagnostic at a caller-selected E_USER_* level (trigger_error).
    /// Respects error_reporting masking + the silence (@) counter.
    pub fn emit_diag_pub(&mut self, level: i64, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & level == 0 {
            return Ok(());
        }
        let (name, errno) = match level {
            512 => ("Warning", 512),
            16384 => ("Deprecated", 16384),
            // E_USER_ERROR=256 is uncatchable in PHP 8.4+ and aborts.
            256 => return self.fail(PhpError::fatal(msg.to_string(), self.cur_line)),
            _ => ("Notice", level),
        };
        self.emit_diag(name, errno, msg)
    }
    pub fn invoke_callable_str(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        self.call_value(&Value::str(name), CallArgs::positional(args))
    }
    pub fn prop_set(&mut self, obj: &Rc<RefCell<PhpObject>>, name: &str, v: Value) {
        obj.borrow_mut().props.insert(name.to_string(), cell(v));
    }
    pub fn obj_class_name(&self, o: &Rc<RefCell<PhpObject>>) -> String {
        o.borrow().class.name().to_string()
    }
}
