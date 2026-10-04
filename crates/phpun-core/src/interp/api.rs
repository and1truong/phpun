//! Builtin-facing surface: output buffering (`ob_*` machinery and
//! handlers), shutdown/error/exception handler stacks, and the small
//! public helpers `builtins.rs`/`serve.rs` call through.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    /// Flush all output buffers at script end, innermost first so each
    /// level's handler output lands in its parent's buffer (bug24951).
    pub(in crate::interp) fn flush_ob_all(&mut self) {
        while !self.ob_stack.is_empty() {
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
    pub(in crate::interp) fn ob_invoke(&mut self, mode: i64) -> Result<Option<Vec<u8>>, PhpError> {
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
    // public helpers for builtins
    pub fn ob_push(&mut self, handler: Option<Value>) {
        self.ob_stack.push(ObLevel {
            buf: Vec::new(),
            handler,
            started: false,
        });
    }
    /// ob_end_clean: handler(mode=CLEAN|FINAL) result discarded, pop.
    pub fn ob_end_clean(&mut self) -> Result<(), PhpError> {
        self.ob_invoke(10)?;
        self.ob_stack.pop();
        Ok(())
    }
    /// ob_end_flush: handler(mode=FINAL) result emitted to parent, pop.
    pub fn ob_end_flush(&mut self) -> Result<(), PhpError> {
        let r = self.ob_invoke(8)?;
        self.ob_stack.pop();
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
        self.ob_stack
            .pop()
            .map(|l| Value::bytes(l.buf))
            .unwrap_or(Value::Bool(false))
    }
    /// ob_get_flush: handler(mode=FINAL) result emitted, RAW buffer
    /// returned, level popped.
    pub fn ob_get_flush(&mut self) -> Result<Value, PhpError> {
        let raw = self.ob_stack.last().map(|l| l.buf.clone());
        let r = self.ob_invoke(8)?;
        self.ob_stack.pop();
        if let Some(s) = r {
            self.emit_bytes(&s);
        }
        Ok(raw.map(Value::bytes).unwrap_or(Value::Bool(false)))
    }
    pub fn ob_top(&self) -> Option<&Vec<u8>> {
        self.ob_stack.last().map(|l| &l.buf)
    }
    pub fn ob_len(&self) -> usize {
        self.ob_stack.len()
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
    /// get_called_class(): late-static-binding class of the current
    /// frame, `false` outside a called-class context (static_get_called_class).
    pub fn called_class_name(&mut self) -> Value {
        match self.stack.last().and_then(|f| f.called_class.clone()) {
            Some(c) => Value::str(c.name().to_string()),
            None => Value::Bool(false),
        }
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

    pub fn deprecated_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.deprecated(msg)
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
