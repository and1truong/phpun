//! Property access + hooks: prop read/write cells, PHP 8.4
//! property hooks (`get`/`set`), typed-slot write checks and
//! backed/virtual helpers, prop visibility rules.

use super::*;

impl<'a> Interp<'a> {
    // ----- property hooks (PHP 8.4, Zend/tests/property_hooks) -----

    /// True while `o`'s own hook on `pn` is running — inside a hook body
    /// `$this->pn` is the backing slot, not a re-entry into the hook.
    pub(in crate::interp) fn in_own_hook(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> bool {
        self.stack
            .last()
            .and_then(|f| f.hook_prop.as_ref())
            .is_some_and(|(id, n, _, _)| *id == o.borrow().id && n == pn)
    }

    /// A private prop of the caller's scope class: it is a *distinct*
    /// property from same-name decls elsewhere in the chain and wins
    /// outright when the caller's scope declares it (private_override).
    fn scope_private_prop(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let scope = self.caller_scope_name()?;
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.name() == scope {
                if let Some(p) = c
                    .decl
                    .props
                    .iter()
                    .find(|p| p.name == pn && p.visibility == crate::ast::Visibility::Private)
                {
                    return Some((p.clone(), c.clone()));
                }
                break;
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    /// The effective hooked prop for `pn`: the nearest PropDecl (for
    /// name/type/visibility) plus hooks merged along the chain — each
    /// hook kind resolves to the nearest decl that provides it (a plain
    /// child redecl still inherits parent hooks; per-hook origin class
    /// drives `__METHOD__` and decl_class). Private props of other
    /// scopes are skipped — they are different properties entirely.
    pub(in crate::interp) fn hooked_prop(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<(PropDecl, MergedHooks)> {
        if let Some((p, c)) = self.scope_private_prop(o, pn) {
            return p.hooks.as_ref().map(|hs| {
                (
                    p.clone(),
                    hs.iter().cloned().map(|h| (h, c.clone())).collect(),
                )
            });
        }
        let mut nearest: Option<PropDecl> = None;
        let mut hooks: MergedHooks = Vec::new();
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name != pn {
                    continue;
                }
                if p.visibility == crate::ast::Visibility::Private {
                    continue;
                }
                if nearest.is_none() {
                    nearest = Some(p.clone());
                }
                if let Some(hs) = &p.hooks {
                    for h in hs {
                        if !hooks.iter().any(|(x, _)| x.is_get == h.is_get) {
                            hooks.push((h.clone(), c.clone()));
                        }
                    }
                }
            }
            let parent = c.decl.parent.clone();
            cur = parent.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let pd = nearest?;
        if hooks.is_empty() {
            return None;
        }
        Some((pd, hooks))
    }

    /// The PropDecl owning a prop_order slot key — mangled `\0Cls\0p`
    /// private keys resolve to that class's decl, plain keys to the
    /// nearest non-private decl (var_dump's `uninitialized(T)`).
    pub fn decl_for_slot(&self, o: &Rc<RefCell<PhpObject>>, key: &str) -> Option<PropDecl> {
        let mut chain = Vec::new();
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        if let Some(r) = key.strip_prefix('\0') {
            let mut it = r.split('\0');
            let cn = it.next()?;
            let pn = it.next()?;
            for c in &chain {
                if c.decl.name == cn {
                    return c
                        .decl
                        .props
                        .iter()
                        .find(|p| p.name == pn && !p.is_static)
                        .cloned();
                }
            }
            return None;
        }
        for c in &chain {
            if let Some(p) = c.decl.props.iter().find(|p| {
                p.name == key && !p.is_static && p.visibility != crate::ast::Visibility::Private
            }) {
                return Some(p.clone());
            }
        }
        None
    }

    /// Instance-prop decl lookup on a class (Reflection, no object) —
    /// any visibility, walking the parent chain.
    pub(in crate::interp) fn find_prop_decl(
        &self,
        cls: &Rc<PhpClass>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(pd) = c.decl.props.iter().find(|p| p.name == pn && !p.is_static) {
                return Some((pd.clone(), c.clone()));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    pub(in crate::interp) fn decl_prop(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        if let Some(x) = self.scope_private_prop(o, pn) {
            return Some(x);
        }
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name == pn && p.visibility != crate::ast::Visibility::Private {
                    return Some((p.clone(), c.clone()));
                }
            }
            let parent = c.decl.parent.clone();
            cur = parent.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    /// Would creating dynamic prop `key`/`pn` on `o` emit the
    /// E_DEPRECATED? True when no declared prop exists and the class
    /// isn't exempt (stdClass / #[AllowDynamicProperties]).
    pub(in crate::interp) fn dyn_prop_deprecated(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
        key: &str,
    ) -> bool {
        !o.borrow().props.contains_key(key)
            && self.decl_prop(o, pn).is_none()
            && !self.obj_is_a(o, "stdclass")
            && !o.borrow().class.decl.attrs.iter().any(|a| {
                a.name
                    .rsplit('\\')
                    .next()
                    .unwrap_or(&a.name)
                    .eq_ignore_ascii_case("AllowDynamicProperties")
            })
    }

    /// Backedness of the *merged* runtime prop: any merged hook body
    /// referencing `$this->prop`, or any plain (unhooked) decl for it
    /// anywhere in the chain — a hooked redecl over a plain parent prop
    /// still shares its backing (parent_get_plain).
    pub(in crate::interp) fn backed_for(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
    ) -> bool {
        if hs.iter().any(|(h, _)| {
            h.body
                .as_deref()
                .is_some_and(|b| Self::stmts_use_this_prop(b, pn))
        }) {
            return true;
        }
        // Ancestor decls: a plain prop or a hook whose body relies on
        // the implicit backing store (incl. `set => expr`, which the
        // parser desugars to `$this->prop = e`) makes the effective
        // property backed even when the nearest impl looks virtual —
        // the slot already exists (gh20270: parent arrow-set read).
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.decl
                .props
                .iter()
                .any(|p| p.name == pn && (p.hooks.is_none() || Self::prop_is_backed(p)))
            {
                return true;
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }

    /// Decl-time backedness via ancestors: nearest ancestor decl named
    /// `name` — a plain prop is backed; a hooked one checks its bodies;
    /// a virtual one defers deeper.
    pub(in crate::interp) fn chain_prop_backed(&self, d: &ClassDecl, name: &str) -> bool {
        let mut an = d.parent.clone();
        while let Some(pn) = an {
            let Some(pc) = self.classes.get(&pn.to_lowercase()) else {
                break;
            };
            if let Some(ap) = pc.decl.props.iter().find(|x| x.name == name) {
                if ap.hooks.is_none() || Self::prop_is_backed(ap) {
                    return true;
                }
            }
            an = pc.decl.parent.clone();
        }
        false
    }

    /// A hooked prop is *backed* (has a backing slot) iff some hook body
    /// references `$this->prop`; otherwise it's virtual and has none.
    pub(in crate::interp) fn prop_is_backed(p: &PropDecl) -> bool {
        p.hooks.as_ref().is_some_and(|hs| {
            hs.iter().any(|h| {
                h.body
                    .as_ref()
                    .is_some_and(|b| Self::stmts_use_this_prop(b, &p.name))
            })
        })
    }

    fn stmts_use_this_prop(v: &[Stmt], pn: &str) -> bool {
        v.iter().any(|s| Self::stmt_uses_this_prop(s, pn))
    }

    fn stmt_uses_this_prop(s: &Stmt, pn: &str) -> bool {
        match s {
            Stmt::Echo(v) | Stmt::Unset(v) | Stmt::Global(v) => {
                v.iter().any(|e| Self::expr_uses_this_prop(e, pn))
            }
            Stmt::Expr(e) => Self::expr_uses_this_prop(e, pn),
            Stmt::Block(b) => Self::stmts_use_this_prop(b, pn),
            Stmt::If { cond, then, else_ } => {
                Self::expr_uses_this_prop(cond, pn)
                    || Self::stmts_use_this_prop(then, pn)
                    || Self::stmts_use_this_prop(else_, pn)
            }
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                Self::expr_uses_this_prop(cond, pn) || Self::stmts_use_this_prop(body, pn)
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                init.iter()
                    .chain(cond.iter())
                    .chain(inc.iter())
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(body, pn)
            }
            Stmt::Return(Some(e)) | Stmt::Break(Some(e)) | Stmt::Continue(Some(e)) => {
                Self::expr_uses_this_prop(e, pn)
            }
            Stmt::Switch { cond, cases } => {
                Self::expr_uses_this_prop(cond, pn)
                    || cases.iter().any(|(c, b)| {
                        c.as_ref().is_some_and(|e| Self::expr_uses_this_prop(e, pn))
                            || Self::stmts_use_this_prop(b, pn)
                    })
            }
            Stmt::Foreach { arr, body, .. } => {
                Self::expr_uses_this_prop(arr, pn) || Self::stmts_use_this_prop(body, pn)
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                Self::stmts_use_this_prop(body, pn)
                    || catches
                        .iter()
                        .any(|c| Self::stmts_use_this_prop(&c.body, pn))
                    || finally
                        .as_ref()
                        .is_some_and(|f| Self::stmts_use_this_prop(f, pn))
            }
            Stmt::Function(d) => {
                d.params
                    .iter()
                    .filter_map(|p| p.default.as_ref())
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(&d.body, pn)
            }
            Stmt::Static { vars, .. } => vars
                .iter()
                .filter_map(|(_, d, _)| d.as_ref())
                .any(|e| Self::expr_uses_this_prop(e, pn)),
            Stmt::Declare { value, .. } => Self::expr_uses_this_prop(value, pn),
            _ => false,
        }
    }

    fn expr_uses_this_prop(e: &Expr, pn: &str) -> bool {
        match e {
            Expr::Prop { obj, name, .. } => {
                (matches!(obj.as_ref(), Expr::Var(v) if v == "this")
                    && matches!(name, PropName::Name(n) if n == pn))
                    || Self::expr_uses_this_prop(obj, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
            }
            Expr::Assign { target, value, .. } => {
                Self::expr_uses_this_prop(target, pn) || Self::expr_uses_this_prop(value, pn)
            }
            Expr::Binary { l, r, .. } => {
                Self::expr_uses_this_prop(l, pn) || Self::expr_uses_this_prop(r, pn)
            }
            Expr::Unary { e, .. }
            | Expr::Clone(e)
            | Expr::ByRef(e)
            | Expr::PreInc(e)
            | Expr::PreDec(e)
            | Expr::PostInc(e)
            | Expr::PostDec(e)
            | Expr::Empty(e)
            | Expr::Print(e)
            | Expr::VarVar(e)
            | Expr::Cast { e, .. }
            | Expr::Throw(e)
            | Expr::Include { e, .. } => Self::expr_uses_this_prop(e, pn),
            Expr::Ternary { c, t, f } => {
                Self::expr_uses_this_prop(c, pn)
                    || t.as_ref().is_some_and(|t| Self::expr_uses_this_prop(t, pn))
                    || Self::expr_uses_this_prop(f, pn)
            }
            Expr::Call { name, args } => {
                Self::expr_uses_this_prop(name, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::StaticCallDyn { class, name, args } => {
                Self::expr_uses_this_prop(class, pn)
                    || Self::expr_uses_this_prop(name, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::Index { e, i } => {
                Self::expr_uses_this_prop(e, pn)
                    || i.as_ref().is_some_and(|i| Self::expr_uses_this_prop(i, pn))
            }
            Expr::Isset(v) => v.iter().any(|e| Self::expr_uses_this_prop(e, pn)),
            Expr::Exit(Some(e)) => Self::expr_uses_this_prop(e, pn),
            Expr::ArrayLit(items) => items.iter().any(|(k, v)| {
                k.as_ref().is_some_and(|k| Self::expr_uses_this_prop(k, pn))
                    || Self::expr_uses_this_prop(v, pn)
            }),
            Expr::List(items) => items.iter().flatten().any(|(k, e)| {
                k.as_ref().is_some_and(|k| Self::expr_uses_this_prop(k, pn))
                    || Self::expr_uses_this_prop(e, pn)
            }),
            Expr::Match { subject, arms } => {
                Self::expr_uses_this_prop(subject, pn)
                    || arms.iter().any(|a| {
                        a.conds.iter().any(|c| Self::expr_uses_this_prop(c, pn))
                            || Self::expr_uses_this_prop(&a.result, pn)
                    })
            }
            Expr::Closure(c) => {
                c.decl
                    .params
                    .iter()
                    .filter_map(|p| p.default.as_ref())
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(&c.decl.body, pn)
            }
            Expr::New { class, args } => {
                Self::expr_uses_this_prop(class, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => {
                Self::expr_uses_this_prop(obj, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::StaticProp { class, name } => {
                Self::expr_uses_this_prop(class, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
            }
            Expr::StaticCall { class, args, .. } => {
                Self::expr_uses_this_prop(class, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::ClassConst { class, .. } => Self::expr_uses_this_prop(class, pn),
            Expr::Instanceof { obj, class } => {
                Self::expr_uses_this_prop(obj, pn) || Self::expr_uses_this_prop(class, pn)
            }
            Expr::Interp(parts) => parts.iter().any(|p| match p {
                // Interpolated `{$expr}` parts are source strings — a
                // substring check for `$this->prop` is close enough for
                // the backed-prop heuristic.
                crate::lexer::StringPart::Expr(s) => {
                    s.contains(&format!("this->{pn}")) || s.contains(&format!("this->${pn}"))
                }
                _ => false,
            }),
            _ => false,
        }
    }

    /// Caller-scope check for a hook's effective visibility (the hook's
    /// own `private get`/`protected set` or the prop's).
    /// The class a *protected* prop is scoped to — the FURTHEST
    /// ancestor in the object's chain declaring it (GH-19044: the check
    /// uses the prototype's scope, so sibling subclasses descending
    /// from that ancestor can access each other's instances).
    fn prop_scope_class(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<Rc<PhpClass>> {
        let mut found = None;
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.decl
                .props
                .iter()
                .any(|p| p.name == pn && p.visibility != crate::ast::Visibility::Private)
            {
                found = Some(c.clone());
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        found
    }

    pub(in crate::interp) fn hook_scope_allows(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        dcls: &Rc<PhpClass>,
        pn: &str,
        vis: crate::ast::Visibility,
    ) -> bool {
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        match (vis, scope) {
            (crate::ast::Visibility::Public, _) => true,
            (crate::ast::Visibility::Private, Some(s)) => s == dcls.name(),
            (crate::ast::Visibility::Protected, Some(s)) => {
                let pcls = self.prop_scope_class(o, pn).unwrap_or_else(|| dcls.clone());
                self.is_a_str(&s, pcls.name()) || self.is_a_str(pcls.name(), &s)
            }
            _ => false,
        }
    }

    fn hook_visibility_error<T>(
        &mut self,
        dcls: &Rc<PhpClass>,
        pname: &str,
        vis: crate::ast::Visibility,
    ) -> Result<T, PhpError> {
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Cannot access {} property {}::${}",
                match vis {
                    crate::ast::Visibility::Private => "private",
                    crate::ast::Visibility::Protected => "protected",
                    crate::ast::Visibility::Public => "public",
                },
                dcls.name(),
                pname
            ),
            0,
        ))
    }

    /// Run a `get`/`set` hook body: a method-like frame whose `hook_prop`
    /// marker lets `$this->prop` hit the backing slot directly. `dcls` is
    /// the class that declared this hook — it becomes the frame's
    /// decl_class and names `__METHOD__`'s scope (via decl_in for traits).
    pub(in crate::interp) fn run_hook(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        dcls: &Rc<PhpClass>,
        pname: &str,
        hook: &crate::ast::PropHook,
        arg: Option<Cell>,
    ) -> Result<Value, PhpError> {
        let kind = if hook.is_get { "get" } else { "set" };
        let params = if hook.is_get {
            Vec::new()
        } else if hook.params.is_empty() {
            vec![crate::ast::Param {
                name: "value".into(),
                default: None,
                by_ref: false,
                variadic: false,
                ty: None,
                promoted: false,
                vis: None,
                readonly: false,
                is_final: false,
                set_vis: None,
                hooks: None,
            }]
        } else {
            hook.params.clone()
        };
        // PHP names hooks `$prop::set` — `__METHOD__` then composes the
        // declaring class into `C::$prop::set` (backed_implicit_get).
        let decl = Rc::new(FunctionDecl {
            ret: None,
            name: format!("${}::{}", pname, kind),
            params,
            body: hook.body.clone().unwrap_or_default(),
            attrs: vec![],
            by_ref: hook.by_ref,
            line: self.cur_line,
            end_line: self.cur_line,
            file: self.cur_file.clone(),
            ns: String::new(),
            decl_in: None,
        });
        let owner = decl_owner(dcls, pname);
        let args = arg.into_iter().collect::<Vec<Cell>>();
        self.pending_decl_class = Some(dcls.clone());
        self.pending_hook_prop = Some((o.borrow().id, pname.to_string(), hook.is_get, owner));
        let called = o.borrow().class.clone();
        self.pending_called_class = Some(called);
        let r = self.invoke_fn(
            &decl,
            CallArgs::positional(args),
            Some(o.clone()),
            Some(dcls.clone()),
        );
        self.pending_decl_class = None;
        self.pending_called_class = None;
        self.pending_hook_prop = None;
        r
    }

    /// Read through a hooked prop: `get` hook, backing slot, or the
    /// write-only error.
    pub(in crate::interp) fn hook_read(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
    ) -> Result<Value, PhpError> {
        let get = hs.iter().find(|(h, _)| h.is_get && h.body.is_some());
        // Visibility: an explicit `private get`/`protected get` wins,
        // else the prop's own visibility governs reads.
        let vis = get.and_then(|(h, _)| h.visibility).unwrap_or(p.visibility);
        let dcls = &get
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| o.borrow().class.clone());
        if !self.hook_scope_allows(o, dcls, &p.name, vis) {
            return self.hook_visibility_error(dcls, &p.name, vis);
        }
        if let Some((h, c)) = get {
            let v = self.run_hook(o, c, &p.name, h, None)?;
            return self.hook_get_typecheck(p, dcls, v);
        }
        if self.backed_for(o, &p.name, hs) {
            return Ok(o
                .borrow()
                .props
                .get(&p.name)
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null));
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Property {}::${} is write-only", dcls.name(), p.name),
            0,
        ))
    }

    /// Strict type membership for the type-check-then-weakly-coerce
    /// pattern (unlike `param_type_match`, scalars don't loosely pass).
    pub(in crate::interp) fn ty_exact(&mut self, tys: &[String], v: &Value) -> bool {
        tys.iter().any(|t| {
            let l = t.to_lowercase();
            match l.as_str() {
                "null" => matches!(v, Value::Null),
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_) | Value::Int(_)),
                "string" => matches!(v, Value::Str(_)),
                "bool" => matches!(v, Value::Bool(_)),
                _ => self.param_type_match(t, v),
            }
        })
    }

    /// Zend's "Implicit conversion from float X to int loses precision"
    /// Deprecated on a lossy float->int weak coercion.
    pub(in crate::interp) fn deprecate_lossy_int(&mut self, tys: &[String], v: &Value, c: &Value) {
        if !matches!(c, Value::Int(_)) || !tys.iter().any(|t| t.eq_ignore_ascii_case("int")) {
            return;
        }
        match v {
            Value::Float(f) if f.fract() != 0.0 => {
                let _ = self.emit_diag(
                    "Deprecated",
                    8192,
                    &format!(
                        "Implicit conversion from float {} to int loses precision",
                        format_float_repr(*f)
                    ),
                );
            }
            // Float-strings name the value `float-string "1.5"`
            // (scalar_return_basic_64bit).
            Value::Str(b) => {
                if let Numeric::Float(f) = numeric(b) {
                    if f.fract() != 0.0 {
                        let _ = self.emit_diag(
                            "Deprecated",
                            8192,
                            &format!(
                                "Implicit conversion from float-string \"{}\" to int loses precision",
                                String::from_utf8_lossy(b)
                            ),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// A `get` hook's return is coerced to the property's declared type
    /// in weak mode ("C::$p::get(): Return value must be of type int,
    /// string returned" TypeError otherwise).
    pub(in crate::interp) fn hook_get_typecheck(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(tys) = &p.ty else { return Ok(v) };
        // self/parent/static in a prop type resolve against the
        // DECLARING class (typed_properties_043); keep the literal
        // members for error display.
        let resolved = self.resolved_ty(tys, dcls.as_ref());
        // Object with __toString coerces into a `string` prop weakly
        // (typed_properties_051).
        if let Value::Object(o) = &v {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("string")) {
                let tcls = o.borrow().class.clone();
                let tostr = self.find_method_in(&tcls, "__tostring").is_some();
                if tostr {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    return Ok(sv);
                }
            }
        }
        let tys = &resolved;
        if self.ty_exact(tys, &v) {
            return Ok(v);
        }
        // weak-mode coercion for scalar targets
        if let Some(c) = weak_ty_coerce(tys, &v) {
            self.deprecate_lossy_int(tys, &v, &c);
            return Ok(c);
        }
        let want = tys.join("|");
        let got = self.zval_type_name(&v);
        self.fail(PhpError::uncaught(
            "TypeError",
            format!(
                "{}::${}::get(): Return value must be of type {}, {} returned",
                dcls.name(),
                p.name,
                want,
                got
            ),
            0,
        ))
    }

    /// Resolve `self`/`parent`/`static` members of a declared type to
    /// concrete class names for the declaring class.
    fn resolved_ty(&self, tys: &[String], dcls: &PhpClass) -> Vec<String> {
        tys.iter()
            .map(|m| {
                let l = m.to_lowercase();
                match l.as_str() {
                    "self" | "static" => dcls.name().to_string(),
                    "parent" => dcls
                        .decl
                        .parent
                        .clone()
                        .unwrap_or_else(|| "\\0parent".to_string()),
                    _ => m.clone(),
                }
            })
            .collect()
    }

    /// Intersect two single type members for `=&` slot-compat: same
    /// name → itself; class types narrow by hierarchy; `object` accepts
    /// any class; `iterable` accepts array/Traversable. Disjoint atoms
    /// (`int` ∩ `float`) return None (typed_properties_076).
    fn ty_member_intersect(&mut self, a: &str, b: &str) -> Option<String> {
        let al = a.to_lowercase();
        let bl = b.to_lowercase();
        if al == bl {
            return Some(a.to_string());
        }
        if al == "mixed" {
            return Some(b.to_string());
        }
        if bl == "mixed" {
            return Some(a.to_string());
        }
        if al.contains('&') || bl.contains('&') {
            // `(X&Y) ∩ (X&Z)` = `X&Y&Z` — conjunct sets merge
            // (typed_reference). A scalar builtin can't coexist with
            // class conjuncts; `object`/`mixed` absorb; two unrelated
            // concrete classes can't both hold.
            let scalarish = |c: &str| {
                matches!(
                    c.to_lowercase().as_str(),
                    "int"
                        | "float"
                        | "string"
                        | "bool"
                        | "array"
                        | "null"
                        | "false"
                        | "true"
                        | "void"
                        | "never"
                        | "resource"
                        | "numeric"
                )
            };
            let mut conj: Vec<String> = Vec::new();
            for c in a.split('&').chain(b.split('&')) {
                let cl = c.to_lowercase();
                if cl == "object" || cl == "mixed" {
                    continue;
                }
                if conj.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    continue;
                }
                conj.push(c.to_string());
            }
            if conj.is_empty() {
                return Some("object".to_string());
            }
            if conj.iter().any(|c| scalarish(c)) && conj.iter().any(|c| !scalarish(c)) {
                return None;
            }
            for i in 0..conj.len() {
                for j in (i + 1)..conj.len() {
                    let (x, y) = (conj[i].to_lowercase(), conj[j].to_lowercase());
                    if self.interfaces.contains_key(&x) || self.interfaces.contains_key(&y) {
                        continue;
                    }
                    if self.classes.contains_key(&x)
                        && self.classes.contains_key(&y)
                        && !self.ty_member_is_a(&x, &y)
                        && !self.ty_member_is_a(&y, &x)
                    {
                        return None;
                    }
                }
            }
            return Some(conj.join("&"));
        }
        const ATOMS: &[&str] = &[
            "int", "float", "string", "bool", "array", "null", "false", "true", "void", "never",
            "resource", "callable", "iterable", "object",
        ];
        let a_atom = ATOMS.contains(&al.as_str());
        let b_atom = ATOMS.contains(&bl.as_str());
        if !a_atom && !b_atom {
            // Both class-like: compatible when one is a subtype of the
            // other — the narrower type wins (A&B refs, 076).
            return if self.ty_member_is_a(&al, &bl) {
                Some(a.to_string())
            } else if self.ty_member_is_a(&bl, &al) {
                Some(b.to_string())
            } else {
                None
            };
        }
        if a_atom && b_atom {
            return match (al.as_str(), bl.as_str()) {
                ("iterable", "array") | ("array", "iterable") => Some("array".to_string()),
                ("iterable", "object") | ("object", "iterable") => Some("Traversable".to_string()),
                _ => None,
            };
        }
        // Exactly one side is a class-like name.
        let (cls, atom) = if a_atom { (b, a) } else { (a, b) };
        let cl = cls.to_lowercase();
        match atom.to_lowercase().as_str() {
            "object" => Some(cls.to_string()),
            "iterable" => {
                if self.ty_member_is_a(&cl, "Traversable") || self.ty_member_is_a(&cl, "iterable") {
                    Some(cls.to_string())
                } else {
                    None
                }
            }
            "callable" => {
                if self.ty_member_is_a(&cl, "Closure") {
                    Some(cls.to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Non-empty member-wise type intersection of two declared types —
    /// returns the intersected member list, or None when disjoint
    /// (typed_properties_068/076 `=&` conflict check).
    /// Ref-bind merge: the memberwise intersection of the two type
    /// sets (`int|string ∩ float|string = string`).
    pub(in crate::interp) fn ty_bind_merge(
        &mut self,
        a: &[String],
        b: &[String],
    ) -> Option<Vec<String>> {
        let mut inter: Vec<String> = Vec::new();
        for am in a {
            for bm in b {
                if let Some(m) = self.ty_member_intersect(am, bm) {
                    if !inter.iter().any(|x| x.eq_ignore_ascii_case(&m)) {
                        inter.push(m);
                    }
                }
            }
        }
        if inter.is_empty() {
            None
        } else {
            Some(inter)
        }
    }

    /// Binding `src` into a typed prop (`$p =& $src`): when src is
    /// another typed prop's slot, the two declared types must
    /// intersect ("Reference ... not compatible", 068/076); the
    /// current value is weak-checked like a normal write ("Cannot
    /// assign X to property"). Returns the merged member list.
    pub(in crate::interp) fn bind_typed_check(
        &mut self,
        pd: &PropDecl,
        dcls: &Rc<PhpClass>,
        src: &Cell,
    ) -> Result<Vec<String>, PhpError> {
        let tys = pd.ty.clone().unwrap_or_default();
        let mut merged = tys.clone();
        // The value must satisfy the NEW owner's type first — a plain
        // `Cannot assign X to property` error (typed_properties_034,
        // union_types/prop_ref_assign); the held-by incompatible
        // message only reports a type-SET conflict.
        let v = src.borrow().clone();
        let nv = self.prop_typed_write_check(pd, dcls, v)?;
        let sptr = Rc::as_ptr(src) as usize;
        // Drop owners whose prop stopped holding the cell (rebind/unset)
        // before reading the prior constraint.
        self.prune_typed_slot(sptr);
        // The cell's current constraint: the last merge result, else
        // the primary owner's declared type.
        let prior = self
            .slot_merged
            .get(&sptr)
            .cloned()
            .or_else(|| self.typed_slots.get(&sptr).map(|(_, t, _, _)| t.clone()));
        if let Some(cur) = prior {
            let ta = self.resolved_ty(&cur, dcls.as_ref());
            let tb = self.resolved_ty(&tys, dcls.as_ref());
            let inter = self.ty_bind_merge(&ta, &tb);
            // Bind fails when the sets are disjoint OR the current
            // value isn't already in the intersection (prop_ref_assign
            // B2-style held-by error naming the primary holder).
            let fits = match &inter {
                Some(m) => {
                    let vty = self.zval_type_name(&src.borrow());
                    m.iter().any(|mm| self.ty_member_is_a_strict(&vty, mm))
                }
                None => false,
            };
            if !fits {
                let (otys, ocn, opn) = self
                    .typed_slots
                    .get(&sptr)
                    .map(|(_, t, n, p)| (t.clone(), n.clone(), p.clone()))
                    .unwrap_or((cur.clone(), String::new(), String::new()));
                let sv = src.borrow().clone();
                let mut e = PhpError::uncaught(
                    "TypeError",
                    format!(
                        "Reference with value of type {} held by property {}::${} of type {} is not compatible with property {}::${} of type {}",
                        self.zval_type_name(&sv),
                        ocn,
                        opn,
                        ty_disp(&otys),
                        dcls.name(),
                        pd.name,
                        ty_disp(&tys)
                    ),
                    0,
                );
                e.thrown_line = Some(self.cur_line);
                return self.fail(e);
            }
            merged = inter.unwrap();
        }
        self.slot_merged.insert(sptr, merged.clone());
        *src.borrow_mut() = nv;
        Ok(merged)
    }

    /// Typed-property write check (plain and backing writes): the
    /// assigned value is coerced in weak mode, else a catchable
    /// `Cannot assign T to property C::$p of type U` TypeError.
    pub(in crate::interp) fn prop_typed_write_check(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(tys) = &p.ty else { return Ok(v) };
        // self/parent/static in a prop type resolve against the
        // DECLARING class (typed_properties_043); keep the literal
        // members for error display.
        let resolved = self.resolved_ty(tys, dcls.as_ref());
        // Object with __toString coerces into a `string` prop weakly
        // (typed_properties_051).
        if let Value::Object(o) = &v {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("string")) {
                let tcls = o.borrow().class.clone();
                let tostr = self.find_method_in(&tcls, "__tostring").is_some();
                if tostr {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    return Ok(sv);
                }
            }
        }
        let tys = &resolved;
        if self.ty_exact(tys, &v) {
            // int stored into a `float` prop widens to a float even in
            // strict mode (typed_properties_031) — but only when no
            // `int` member takes it exactly (`int|float` keeps int,
            // legal_default_values).
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float"))
                && !tys.iter().any(|t| t.eq_ignore_ascii_case("int"))
            {
                if let Value::Int(i) = v {
                    return Ok(Value::Float(i as f64));
                }
            }
            return Ok(v);
        }
        if !self.exec_file_strict() {
            if let Some(c) = weak_ty_coerce(tys, &v) {
                self.deprecate_lossy_int(tys, &v, &c);
                return Ok(c);
            }
        }
        let mut e = PhpError::uncaught(
            "TypeError",
            format!(
                "Cannot assign {} to property {}::${} of type {}",
                self.zval_type_name(&v),
                dcls.name(),
                p.name,
                ty_disp(p.ty.as_deref().unwrap_or(&[]))
            ),
            0,
        );
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// The value passed to a `set` hook is checked against the hook's
    /// `$value` parameter type — the declared prop type for the `set =>
    /// expr` shorthand — under weak coercion (gh17988's `string(2) "42"`).
    fn hook_set_arg_check(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        tys: Vec<String>,
        v: Value,
    ) -> Result<Value, PhpError> {
        if self.ty_exact(&tys, &v) {
            return Ok(v);
        }
        if let Some(c) = weak_ty_coerce(&tys, &v) {
            self.deprecate_lossy_int(&tys, &v, &c);
            return Ok(c);
        }
        let mut e = PhpError::uncaught(
            "TypeError",
            format!(
                "{}::${}::set(): Argument #1 ($value) must be of type {}, {} given, called in {} on line {}",
                dcls.name(),
                p.name,
                tys.join("|"),
                self.zval_type_name(&v),
                self.diag_file(),
                self.cur_line
            ),
            0,
        );
        e.thrown_line = Some(p.line);
        self.fail(e)
    }

    /// Write through a hooked prop: `set` hook, backing slot, or the
    /// read-only error. `private(set)` narrows the write side.
    pub(in crate::interp) fn hook_write(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
        mut v: Value,
    ) -> Result<(), PhpError> {
        let set = hs.iter().find(|(h, _)| !h.is_get && h.body.is_some());
        let dcls = &set
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| o.borrow().class.clone());
        if let Some(sv) = p.set_vis {
            if !self.hook_scope_allows(o, dcls, &p.name, sv) {
                return self.set_visibility_error(dcls, &p.name, sv);
            }
        }
        let vis = set.and_then(|(h, _)| h.visibility).unwrap_or(p.visibility);
        if !self.hook_scope_allows(o, dcls, &p.name, vis) {
            return self.hook_visibility_error(dcls, &p.name, vis);
        }
        if let Some((h, c)) = set {
            let arg_tys = h
                .params
                .first()
                .and_then(|pp| pp.ty.clone())
                .or_else(|| p.ty.clone());
            if let Some(tys) = arg_tys {
                v = self.hook_set_arg_check(p, dcls, tys, v)?;
            }
            self.run_hook(o, c, &p.name, h, Some(cell(v)))?;
            return Ok(());
        }
        if self.backed_for(o, &p.name, hs) {
            v = self.prop_typed_write_check(p, dcls, v)?;
            let mut ob = o.borrow_mut();
            if !ob.prop_order.contains(&p.name) {
                ob.prop_order.push(p.name.clone());
            }
            ob.props.insert(p.name.clone(), cell(v));
            return Ok(());
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Property {}::${} is read-only", dcls.name(), p.name),
            0,
        ))
    }

    /// unserialize(): writing into a *virtual* hooked prop aborts the
    /// whole unserialize with warnings (property_hooks/unserialize).
    pub fn unserial_prop_virtual(&mut self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> bool {
        match self.hooked_prop(o, pn) {
            Some((_, hs)) => !self.backed_for(o, pn, &hs),
            None => false,
        }
    }

    /// The canonical prop key a serialized prop name resolves to, or
    /// None when it stays a verbatim dynamic prop — zend's unserialize
    /// writes straight into the props hash for a mangled declared name,
    /// and `is_property_visibility_changed` resolves bare, `\0*\0`, and
    /// own-class-mangled names through `properties_info[plain]` (a
    /// subclass redeclaring the prop shadows the ancestor's decl).
    /// phpun's canonical key mirrors zend's: plain for
    /// public/protected, `\0DeclaringClass\0name` for private.
    pub fn unserial_resolve_key(&self, o: &Rc<RefCell<PhpObject>>, key: &[u8]) -> Option<String> {
        let (scope, name): (Option<&[u8]>, &[u8]) = if key.starts_with(&[0u8][..]) && key.len() > 2
        {
            let r = &key[1..];
            let z = r.iter().position(|b| *b == 0)?;
            (Some(&r[..z]), &r[z + 1..])
        } else {
            (None, key)
        };
        if name.is_empty() {
            return None;
        }
        let name_s = String::from_utf8_lossy(name).into_owned();
        if let Some(sc) = scope {
            if sc != b"*" {
                // An exact `\0Scope\0name` hits the props hash's
                // INDIRECT entry: a class `sc` in the object's
                // ancestry declaring `name` private.
                let mut cur = Some(o.borrow().class.clone());
                while let Some(c) = cur {
                    if c.name().as_bytes().eq_ignore_ascii_case(sc) {
                        if let Some(pd) = c
                            .decl
                            .props
                            .iter()
                            .find(|p| p.name == name_s && !p.is_static)
                        {
                            if pd.visibility == crate::ast::Visibility::Private {
                                return Some(format!("\0{}\0{}", c.name(), pd.name));
                            }
                        }
                        break;
                    }
                    cur = c
                        .decl
                        .parent
                        .as_ref()
                        .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                }
                // zend's visibility resolution only applies to the
                // object's own class (and `*`, handled above).
                if !sc.eq_ignore_ascii_case(o.borrow().class.name().as_bytes()) {
                    return None;
                }
            }
        }
        self.find_prop_decl(&o.borrow().class, &name_s)
            .map(|(pd, dcls)| match pd.visibility {
                crate::ast::Visibility::Private => format!("\0{}\0{}", dcls.name(), pd.name),
                _ => pd.name,
            })
    }

    /// `parent::$prop::get()/set()` inside a hook — runs the parent
    /// class's hook for the same prop+kind, or reads/writes the
    /// parent's plain prop (parent_property_hook tests).
    pub(in crate::interp) fn hook_parent_call(
        &mut self,
        pn: &str,
        is_get: bool,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        let kind = if is_get { "get" } else { "set" };
        // Borrow-free snapshot of the caller frame's hook context (the
        // outside/different-prop/different-kind rules are parse-time).
        let (f_this, f_dcls) = {
            let f = self.stack.last();
            (
                f.and_then(|f| f.this_obj.clone()),
                f.and_then(|f| f.decl_class.clone()),
            )
        };
        let Some(o) = f_this else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when no class scope is active",
                0,
            ));
        };
        let dcls = f_dcls.unwrap_or_else(|| o.borrow().class.clone());
        let Some(parent_name) = dcls.decl.parent.clone() else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when current class scope has no parent",
                0,
            ));
        };
        let Some(parent) = self.classes.get(&parent_name.to_lowercase()).cloned() else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when current class scope has no parent",
                0,
            ));
        };
        // Resolve the prop on the parent chain: per-kind merged hooks.
        let mut nearest: Option<(PropDecl, Rc<PhpClass>)> = None;
        let mut hooks: MergedHooks = Vec::new();
        let mut cur = Some(parent.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name != pn {
                    continue;
                }
                if nearest.is_none() {
                    nearest = Some((p.clone(), c.clone()));
                }
                if let Some(hs) = &p.hooks {
                    for h in hs {
                        if !hooks.iter().any(|(x, _)| x.is_get == h.is_get) {
                            hooks.push((h.clone(), c.clone()));
                        }
                    }
                }
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let Some((pd, pcls)) = nearest else {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined property {}::${}", parent.name(), pn),
                0,
            ));
        };
        // private parent prop is invisible to the child scope
        if pd.visibility == crate::ast::Visibility::Private && pcls.name() != dcls.name() {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot access private property {}::${}", pcls.name(), pn),
                0,
            ));
        }
        let hook = hooks
            .iter()
            .find(|(h, _)| h.is_get == is_get && h.body.is_some())
            .cloned();
        // A user hook tolerates extra args (user-function semantics);
        // the implicit hook of a *plain* parent prop is an internal
        // function with a strict arg count (parent_superfluous_args).
        if hook.is_none() {
            let want = if is_get { 0 } else { 1 };
            if args.len() != want {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "{}::${}::{}() expects exactly {} argument{}, {} given",
                        parent_name,
                        pn,
                        kind,
                        want,
                        if want == 1 { "" } else { "s" },
                        args.len()
                    ),
                    0,
                ));
            }
        }
        // Named args bind against the hook's params (or the implicit
        // set's `$value` for a plain parent prop); unknown names are a
        // catchable Error (gh20270).
        let pnames: Vec<String> = match &hook {
            Some((h, _)) => h.params.iter().map(|p| p.name.clone()).collect(),
            None => {
                if is_get {
                    Vec::new()
                } else {
                    vec!["value".to_string()]
                }
            }
        };
        let argvals = {
            let mut vs: Vec<Value> = Vec::new();
            for a in args {
                match a {
                    Expr::Binary {
                        op: "named", l, r, ..
                    } => {
                        let n = match self.eval(l)? {
                            Value::Str(s) => crate::value::lossy(&s).into_owned(),
                            v => v.to_php_string(),
                        };
                        match pnames.iter().position(|p| *p == n) {
                            Some(i) => {
                                while vs.len() <= i {
                                    vs.push(Value::Null);
                                }
                                vs[i] = self.eval(r)?;
                            }
                            None => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!("Unknown named parameter ${}", n),
                                    0,
                                ));
                            }
                        }
                    }
                    _ => vs.push(self.eval(a)?),
                }
            }
            vs
        };
        if let Some((h, c)) = hook {
            return self.run_hook(&o, &c, pn, &h, argvals.into_iter().next().map(cell));
        }
        // Plain parent prop: get reads the slot, set writes it (and —
        // like a plain assignment — the implicit set returns the value).
        if is_get {
            if !o.borrow().props.contains_key(pn) {
                // Implicit get reads the shared backing slot; an
                // uninitialized typed prop is a catchable Error naming
                // the decl visible from the object (parent_get_plain_…).
                if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                    if tpd.ty.is_some() && tpd.default.is_none() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed property {}::${} must not be accessed before initialization",
                                tdcls.name(),
                                pn
                            ),
                            0,
                        ));
                    }
                }
            }
            Ok(o.borrow()
                .props
                .get(pn)
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null))
        } else {
            let v = argvals.into_iter().next().unwrap_or(Value::Null);
            let mut ob = o.borrow_mut();
            if !ob.prop_order.iter().any(|k| k == pn) {
                ob.prop_order.push(pn.to_string());
            }
            ob.props.insert(pn.to_string(), cell(v.clone()));
            Ok(v)
        }
    }

    /// Whether the caller's scope violates set-visibility `sv` on a
    /// prop declared in `dcls` — objectless counterpart of
    /// hook_scope_allows for statics.
    pub(in crate::interp) fn set_vis_scope_denied(
        &mut self,
        dcls: &Rc<PhpClass>,
        sv: crate::ast::Visibility,
    ) -> bool {
        let scope = self.caller_scope_name();
        match (sv, scope.as_deref()) {
            (crate::ast::Visibility::Public, _) => false,
            (crate::ast::Visibility::Private, s) => s != Some(dcls.name()),
            (crate::ast::Visibility::Protected, Some(s)) => {
                !(self.is_a_str(s, dcls.name()) || self.is_a_str(dcls.name(), s))
            }
            (crate::ast::Visibility::Protected, None) => true,
        }
    }

    /// `private(set)`/`protected(set)` violation on INDIRECT writes
    /// (`[]`, `&`, compound, `++`, by-ref args) — zend names it
    /// 'Cannot indirectly modify ... (set)' instead of 'Cannot modify'.
    pub(in crate::interp) fn set_visibility_indirect_error<T>(
        &mut self,
        dcls: &Rc<PhpClass>,
        pname: &str,
        sv: crate::ast::Visibility,
    ) -> Result<T, PhpError> {
        let visname = match sv {
            crate::ast::Visibility::Private => "private",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Public => "public",
        };
        let from = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
            .map(|c| format!("scope {}", c.name()))
            .unwrap_or_else(|| "global scope".to_string());
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Cannot indirectly modify {}(set) property {}::${} from {}",
                visname,
                dcls.name(),
                pname,
                from
            ),
            0,
        ))
    }

    /// `private(set)`/`protected(set)` violation message — distinct from
    /// the read-side "Cannot access" (asymmetric_visibility).
    pub(in crate::interp) fn set_visibility_error<T>(
        &mut self,
        dcls: &Rc<PhpClass>,
        pname: &str,
        sv: crate::ast::Visibility,
    ) -> Result<T, PhpError> {
        let visname = match sv {
            crate::ast::Visibility::Private => "private",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Public => "public",
        };
        let from = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
            .map(|c| format!("scope {}", c.name()))
            .unwrap_or_else(|| "global scope".to_string());
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Cannot modify {}(set) property {}::${} from {}",
                visname,
                dcls.name(),
                pname,
                from
            ),
            0,
        ))
    }

    pub(in crate::interp) fn prop_read(
        &mut self,
        obj: &Expr,
        name: &PropName,
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        let ov = self.eval(obj)?;
        let pn = self.prop_name(name)?;
        self.prop_read_value(ov, &pn, nullsafe)
    }

    /// prop_read with a pre-bound name — zend binds the operand once,
    /// so a name mutation inside __isset/__get doesn't re-evaluate it
    /// (bug75420).
    pub(in crate::interp) fn prop_read_value(
        &mut self,
        ov: Value,
        pn: &str,
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let cls = o.borrow().class.clone();
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        return self.hook_read(&o, &pd, &hs);
                    }
                }
                if let Some(k) = self.obj_prop_key(&o, pn) {
                    // Declared but not visible from this scope →
                    // __get territory (bug37667).
                    if self.prop_visible(&cls, pn) {
                        return Ok(o.borrow().props.get(&k).unwrap().borrow().clone());
                    }
                }
                // Typed prop whose slot was never initialized → Error
                // (not __get, not a warning): parent_get_plain_typed_uninitialized.
                // One that was unset() routes to __get like an
                // undefined property (typed_properties_009).
                let was_unset = {
                    let ob = o.borrow();
                    ob.unset_props.contains(pn)
                        || ob
                            .unset_props
                            .iter()
                            .any(|k| k.ends_with(&format!("\0{}", pn)))
                };
                // ARRAY_AS_PROPS: undeclared props resolve against the
                // storage hash — spl read_property hashes it like an
                // array dimension (Undefined array key + NULL, no __get).
                if self.aap_active(&o) && (self.decl_prop(&o, pn).is_none() || was_unset) {
                    let arr = self.ao_state(&o).0;
                    let v = arr.borrow().get(&ArrKey::Str(Rc::from(pn)));
                    return match v {
                        Some(v) => Ok(v),
                        None => {
                            self.warn(&format!("Undefined array key \"{}\"", pn))?;
                            Ok(Value::Null)
                        }
                    };
                }
                // Without __get, an unset() declared prop still reads
                // as uninitialized; with __get it routes to magic
                // (typed_properties_047 vs _009).
                let has_get = self.find_method_in(&cls, "__get").is_some();
                if !was_unset || !has_get {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                        if tpd.ty.is_some() {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Typed property {}::${} must not be accessed before initialization",
                                    tdcls.name(),
                                    pn
                                ),
                                0,
                            ));
                        }
                    }
                }
                // __get magic — the (obj, prop) in-get guard keeps a
                // re-entrant `$this->$pn` inside __get on real storage
                // (bug63462/bug66609).
                if has_get {
                    let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.to_string());
                    if !self.magic_guards.insert(gkey.clone()) {
                        // Re-entrant access to a DECLARED prop the
                        // magic scope can't see is a hard Error, not
                        // an undefined-prop warning (bug48248).
                        if let Some(e) = self.hidden_decl_error(&o, pn) {
                            return self.fail(e);
                        }
                        self.check_prop_name(pn)?;
                        self.warn(&format!("Undefined property: {}::${}", cls.name(), pn))?;
                        return Ok(Value::Null);
                    }
                    let res = self.method_invoke(
                        o.clone(),
                        "__get",
                        CallArgs::positional(vec![cell(Value::str(pn))]),
                    );
                    self.magic_guards.remove(&gkey);
                    let rv = res?;
                    // A __get result for an unset() declared-typed prop
                    // must satisfy the declared type
                    // (typed_properties_030).
                    if was_unset {
                        if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                            if let Some(tys) = &tpd.ty {
                                if !self.ty_exact(tys, &rv) {
                                    if let Some(cv) = weak_ty_coerce(tys, &rv) {
                                        return Ok(cv);
                                    }
                                }
                                let ok =
                                    self.ty_exact(tys, &rv) || weak_ty_coerce(tys, &rv).is_some();
                                if !ok {
                                    let mut e = PhpError::uncaught(
                                        "TypeError",
                                        format!(
                                            "Value of type {} returned from {}::__get() must be compatible with unset property {}::${} of type {}",
                                            self.zval_type_name(&rv),
                                            tdcls.name(),
                                            tdcls.name(),
                                            pn,
                                            ty_disp(tys)
                                        ),
                                        0,
                                    );
                                    e.thrown_line = Some(self.cur_line);
                                    return self.fail(e);
                                }
                            }
                        }
                    }
                    return Ok(rv);
                }
                // A declared prop this scope can't see raises
                // `Cannot access private/protected property`, not the
                // undefined-property warning (closure_020).
                if let Some(e) = self.hidden_decl_error(&o, pn) {
                    return self.fail(e);
                }
                self.check_prop_name(pn)?;
                self.warn(&format!("Undefined property: {}::${}", cls.name(), pn))?;
                Ok(Value::Null)
            }
            Value::Callable(_) => {
                // Closure is a real class with no declared props —
                // reads warn "Undefined property: Closure::$a"
                // (closure_031).
                self.check_prop_name(pn)?;
                self.warn(&format!("Undefined property: Closure::${}", pn))?;
                Ok(Value::Null)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.warn(&format!("Attempt to read property \"{}\" on null", pn))?;
                Ok(Value::Null)
            }
            other => {
                if !self.is_quiet() {
                    // zend names scalar types by their zval name —
                    // 'on int', never 'on integer' (probe4j).
                    let t = self.zval_type_name(&other);
                    self.warn(&format!("Attempt to read property \"{}\" on {}", pn, t))?;
                }
                Ok(Value::Null)
            }
        }
    }

    pub(in crate::interp) fn prop_cell(
        &mut self,
        obj: &Expr,
        name: &PropName,
        _nullsafe: bool,
    ) -> Result<Cell, PhpError> {
        self.last_prop_ov = None;
        let pn = self.prop_name(name)?;
        // Write-context chains evaluate every link as a write fetch —
        // `$i->p->sub` dies with 'Attempt to modify property "p" on
        // int' instead of the read warning (probe4j). A paren just
        // wraps a link.
        let ov = self.eval_lvalue_obj(obj)?;
        self.last_prop_ov = Some(ov.clone());
        match ov {
            Value::Object(o) => {
                // Hooks intercept the cell path entirely — `[]`, `&`,
                // `++`/`--` are "indirect modification", unless `&get`
                // exists: the by-ref get's returned cell is used.
                if !self.in_own_hook(&o, &pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, &pn) {
                        let by_ref_get = hs
                            .iter()
                            .find(|(h, _)| h.is_get && h.by_ref && h.body.is_some());
                        let dcls = by_ref_get
                            .map(|(_, c)| c.clone())
                            .unwrap_or_else(|| o.borrow().class.clone());
                        if let Some((h, c)) = by_ref_get {
                            let vis = h.visibility.unwrap_or(pd.visibility);
                            if !self.hook_scope_allows(&o, c, &pd.name, vis) {
                                return self.hook_visibility_error(c, &pd.name, vis);
                            }
                            self.last_ret_cell = None;
                            let v = self.run_hook(&o, c, &pd.name, h, None)?;
                            return Ok(self.last_ret_cell.take().unwrap_or_else(|| cell(v)));
                        }
                        // `$obj->hooked[k] = v`: zend fetches the hooked
                        // prop once; when the result is an object the
                        // index op applies to the object itself (objects
                        // pass by handle), e.g. ArrayAccess offsetSet
                        // (object_in_hook.phpt). Arrays/scalars still
                        // need `&get` for the write to land.
                        if let Some((gh, gc)) = hs
                            .iter()
                            .find(|(h, _)| h.is_get && h.body.is_some())
                            .map(|(h, c)| (h.clone(), c.clone()))
                        {
                            let vis = gh.visibility.unwrap_or(pd.visibility);
                            if !self.hook_scope_allows(&o, &gc, &pd.name, vis) {
                                return self.hook_visibility_error(&gc, &pd.name, vis);
                            }
                            let gv = self.run_hook(&o, &gc, &pd.name, &gh, None)?;
                            if matches!(gv, Value::Object(_)) {
                                return Ok(cell(gv));
                            }
                        }
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Indirect modification of {}::${} is not allowed",
                                dcls.name(),
                                pd.name
                            ),
                            0,
                        ));
                    }
                }
                let key = self.obj_prop_key(&o, &pn);
                // A slot that exists but isn't visible from this scope
                // is *inaccessible*: cell ops route to __get like a
                // missing prop, and the write dies in the temp
                // (bug37667 — appends to a protected prop).
                let visible = self.prop_visible(&o.borrow().class.clone(), &pn);
                let key = match key {
                    Some(k) if visible => Some(k),
                    _ => None,
                };
                // zend's write-fetch on a readonly prop is only legal
                // when the slot already holds an object — the write
                // then targets the object, never the slot. The engine
                // hands out the object handle, so `&`-binds get a
                // detached temp whose writes can't reach the slot.
                // Any other content — missing, scalar, array — dies
                // with the indirect-modify Error, ahead of the
                // uninit-typed 'by reference' gate (R3 finding 3).
                // Invisible props keep falling to __get/hidden-error.
                if visible {
                    if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                        if pd.readonly {
                            let dk = if pd.visibility == crate::ast::Visibility::Private {
                                format!("\0{}\0{}", dcls.name(), pd.name)
                            } else {
                                pd.name.clone()
                            };
                            let held = o.borrow().props.get(&dk).cloned();
                            return match held {
                                Some(c) if matches!(&*c.borrow(), Value::Object(_)) => {
                                    Ok(cell(c.borrow().clone()))
                                }
                                _ => self.fail(PhpError::uncaught(
                                    "Error",
                                    format!(
                                        "Cannot indirectly modify readonly property {}::${}",
                                        dcls.name(),
                                        pd.name
                                    ),
                                    0,
                                )),
                            };
                        }
                        // private(set)/protected(set): a cell fetch is
                        // an indirect write — `[]`, `&`, `&arg`, `+=`,
                        // `++`, foreach-by-ref all name it (a
                        // whole-prop unset carries its own error).
                        // Like readonly, a slot already holding an
                        // OBJECT hands the object out instead — writes
                        // then target it (`$foo->bar->baz = 42`), and
                        // dim/compound ops on the slot itself hit the
                        // object's own errors, not 'indirectly modify'.
                        if !self.in_unset {
                            if let Some(sv) = pd.set_vis {
                                if !self.hook_scope_allows(&o, &dcls, &pn, sv) {
                                    let dk = if pd.visibility == crate::ast::Visibility::Private {
                                        format!("\0{}\0{}", dcls.name(), pd.name)
                                    } else {
                                        pd.name.clone()
                                    };
                                    let held = o.borrow().props.get(&dk).cloned();
                                    return match held {
                                        Some(c) if matches!(&*c.borrow(), Value::Object(_)) => {
                                            Ok(cell(c.borrow().clone()))
                                        }
                                        _ => {
                                            self.set_visibility_indirect_error(&dcls, &pd.name, sv)
                                        }
                                    };
                                }
                            }
                        }
                    }
                }
                if key.is_none() {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, &pn) {
                        if tpd.ty.is_some() && tpd.default.is_none() {
                            let nullable = tpd
                                .ty
                                .as_ref()
                                .map(|t| t.iter().any(|m| m.eq_ignore_ascii_case("null")))
                                .unwrap_or(false);
                            if !nullable && (self.dim_by_ref || self.foreach_by_ref) {
                                // `=&` on an uninit typed prop routes to
                                // `&__get` when it exists — the bound
                                // ref sees __get's cell (073).
                                let cls = o.borrow().class.clone();
                                let get_by_ref = self
                                    .find_method_in(&cls, "__get")
                                    .map(|(m, _)| m.decl.by_ref)
                                    .unwrap_or(false);
                                if get_by_ref {
                                    let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.clone());
                                    if !self.magic_guards.insert(gkey.clone()) {
                                        return self.fail(PhpError::uncaught(
                                            "Error",
                                            format!(
                                                "Cannot access uninitialized non-nullable property {}::${} by reference",
                                                tdcls.name(),
                                                pn
                                            ),
                                            0,
                                        ));
                                    }
                                    self.last_ret_cell = None;
                                    let res = self.method_invoke(
                                        o.clone(),
                                        "__get",
                                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                    );
                                    self.magic_guards.remove(&gkey);
                                    let rv = res?;
                                    let got = self.last_ret_cell.take().unwrap_or_else(|| cell(rv));
                                    // The bound ref IS __get's cell —
                                    // its value is cast to the declared
                                    // type in place and the prop itself
                                    // stays uninitialized (073).
                                    let cv = self.prop_typed_write_check(
                                        &tpd,
                                        &tdcls,
                                        got.borrow().clone(),
                                    )?;
                                    *got.borrow_mut() = cv;
                                    self.mark_ref(&got);
                                    return Ok(got);
                                }
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!(
                                        "Cannot access uninitialized non-nullable property {}::${} by reference",
                                        tdcls.name(),
                                        pn
                                    ),
                                    0,
                                ));
                            }
                            // Nullable uninit slots byref-init to null.
                            let nc = cell(Value::Null);
                            self.last_fresh_cell = Some(Rc::as_ptr(&nc) as usize);
                            o.borrow_mut().props.insert(pn.clone(), nc);
                        }
                    }
                }
                if key.is_none() {
                    // A missing prop on a class with __get is
                    // *overloaded*: cell ops (`[]`, `=&`, `++`) fetch
                    // through __get. `&__get` returns a real cell the
                    // write binds; a plain __get yields a temp — the
                    // write dies with an "Indirect modification"
                    // notice (bug32660, bug37667, bug43201).
                    let cls = o.borrow().class.clone();
                    if let Some(gm) = self.find_method_in(&cls, "__get").map(|(m, _)| m) {
                        let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.clone());
                        if self.magic_guards.insert(gkey.clone()) {
                            self.last_ret_cell = None;
                            let res = self.method_invoke(
                                o.clone(),
                                "__get",
                                CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                            );
                            self.magic_guards.remove(&gkey);
                            let rv = res?;
                            if gm.decl.by_ref {
                                return Ok(self.last_ret_cell.take().unwrap_or_else(|| cell(rv)));
                            }
                            // zend applies the dim op to an *object*
                            // result (objects pass by handle) — no
                            // notice; scalar/array results die in the
                            // temp with one (object_in_hook rule).
                            if !matches!(rv, Value::Object(_)) {
                                self.notice(&format!(
                                    "Indirect modification of overloaded property {}::${} has no effect",
                                    cls.name(),
                                    pn
                                ))?;
                            }
                            return Ok(cell(rv));
                        }
                        // Re-entrant `&`-fetch of a declared prop the
                        // magic scope can't see is a hard Error
                        // (bug48248 `&__get` returning `$this->priv`).
                        if let Some(e) = self.hidden_decl_error(&o, &pn) {
                            return self.fail(e);
                        }
                        return Ok(cell(Value::Null));
                    }
                }
                // Declared-but-invisible with no __get intercept dies
                // with zend's access Error — it must not fall through
                // to the dynamic-prop materialization below and shadow
                // the declaration (finding 11).
                if key.is_none() {
                    if let Some(e) = self.hidden_decl_error(&o, &pn) {
                        return self.fail(e);
                    }
                }
                let key = key.unwrap_or_else(|| pn.clone());
                // An RW fetch of an undeclared prop materializes a
                // dynamic one — E_DEPRECATED on non-exempt classes
                // (stdClass / #[AllowDynamicProperties]).
                if self.dyn_prop_deprecated(&o, &pn, &key) {
                    let cn = o.borrow().class.name().to_string();
                    self.deprecated(&format!(
                        "Creation of dynamic property {}::${} is deprecated",
                        cn, pn
                    ))?;
                }
                let undeclared = self.decl_prop(&o, &pn).is_none();
                let mut ob = o.borrow_mut();
                if !ob.props.contains_key(&key) {
                    if !ob.prop_order.contains(&key) {
                        ob.prop_order.push(key.clone());
                    }
                    let nc = cell(Value::Null);
                    self.last_fresh_cell = Some(Rc::as_ptr(&nc) as usize);
                    if undeclared {
                        // The compound read warns 'Undefined property'
                        // for a slot this write-fetch just created
                        // (finding 13) — only for genuinely dynamic
                        // props; declared-but-uninit slots stay silent.
                        self.fresh_dyn_props.push((
                            Rc::as_ptr(&nc) as usize,
                            ob.class.name().to_string(),
                            pn.clone(),
                        ));
                    }
                    ob.props.insert(key.clone(), nc);
                }
                let slot = ob.props.get(&key).unwrap().clone();
                drop(ob);
                if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                    if let Some(tys) = &pd.ty {
                        let p = Rc::as_ptr(&slot) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                slot.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                pn.clone(),
                            ),
                        );
                        self.slot_anchor
                            .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), key.clone()));
                    }
                }
                Ok(slot)
            }
            // Cell fetches (`=&`, `[]`, `++`) on a prop of a non-object
            // die with zend's modify-verb Error — catchable (probe4f).
            other => self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Attempt to modify property \"{}\" on {}",
                    pn,
                    self.zval_type_name(&other)
                ),
                self.cur_line,
            )),
        }
    }

    /// Resolve a property access to its storage key on `o`.
    /// Private slots are `\0DeclaringClass\0name`; a method sees the
    /// private slot of its own declaring class, then public/protected
    /// and dynamic props under the plain name.
    pub(in crate::interp) fn obj_prop_key(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<String> {
        let dc = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        });
        if let Some(dc) = dc {
            let k = format!("\0{}\0{}", dc, pn);
            if o.borrow().props.contains_key(&k) {
                return Some(k);
            }
        }
        if o.borrow().props.contains_key(pn) {
            return Some(pn.to_string());
        }
        None
    }

    pub(in crate::interp) fn unset_prop(&mut self, e: &Expr) -> Result<(), PhpError> {
        if let Expr::Prop { obj, name, .. } = e {
            let pn = self.prop_name(name)?;
            let ov = self.eval_lvalue_obj(obj)?;
            if let Value::Object(o) = ov {
                if !self.in_own_hook(&o, &pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, &pn) {
                        let _dcls = &hs[0].1;
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Cannot unset hooked property {}::${}",
                                o.borrow().class.name(),
                                pd.name
                            ),
                            0,
                        ));
                    }
                }
                let cls = o.borrow().class.clone();
                // A declared prop the scope can't see is zend's access
                // Error — `__unset` intercepts it like any overload
                // first (finding 11: unset($a->protected)). Computed
                // unconditionally: prop_visible() skips private decls,
                // which would make an invisible private prop unset()
                // silently no-op instead of 'Cannot access private
                // property' (R3 finding 2).
                let hidden = self.hidden_decl_error(&o, &pn);
                // zend's unset on a declared prop runs its write-scope
                // gates BEFORE the slot is touched (R3 finding 1): an
                // initialized readonly prop can never be unset; an
                // uninitialized one — or any asymmetric-visibility
                // prop — needs the set-visibility scope
                // (`public readonly` is implicitly protected(set)).
                if hidden.is_none() {
                    if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                        let dk = if pd.visibility == crate::ast::Visibility::Private {
                            format!("\0{}\0{}", dcls.name(), pd.name)
                        } else {
                            pd.name.clone()
                        };
                        if pd.readonly && o.borrow().props.contains_key(&dk) {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Cannot unset readonly property {}::${}",
                                    dcls.name(),
                                    pd.name
                                ),
                                0,
                            ));
                        }
                        let eff = pd.set_vis.unwrap_or(if pd.readonly {
                            crate::ast::Visibility::Protected
                        } else {
                            pd.visibility
                        });
                        if eff != crate::ast::Visibility::Public {
                            let scope = self.caller_scope_name();
                            let dn = dcls.name().to_string();
                            let ok = match eff {
                                crate::ast::Visibility::Private => {
                                    scope.as_deref() == Some(dn.as_str())
                                }
                                crate::ast::Visibility::Protected => scope
                                    .as_deref()
                                    .map(|s| self.is_a_str(s, &dn) || self.is_a_str(&dn, s))
                                    .unwrap_or(false),
                                crate::ast::Visibility::Public => true,
                            };
                            if !ok {
                                let word = if eff == crate::ast::Visibility::Private {
                                    "private(set)"
                                } else {
                                    "protected(set)"
                                };
                                // zend's wording tucks 'readonly' into
                                // the protected(set) form only — the
                                // private(set) message drops it.
                                let rw = if pd.readonly && eff == crate::ast::Visibility::Protected
                                {
                                    " readonly"
                                } else {
                                    ""
                                };
                                let from = scope
                                    .map(|s| format!("scope {}", s))
                                    .unwrap_or_else(|| "global scope".to_string());
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!(
                                        "Cannot unset {}{} property {}::${} from {}",
                                        word, rw, dn, pd.name, from
                                    ),
                                    0,
                                ));
                            }
                        }
                    }
                }
                if let Some(k) = self.obj_prop_key(&o, &pn).filter(|_| hidden.is_none()) {
                    let mut ob = o.borrow_mut();
                    let prune = if let Some(c) = ob.props.remove(&k) {
                        // unset() severs the typed slot: refs bound to it
                        // become plain variables again (typed_properties_090).
                        let ptr = Rc::as_ptr(&c) as usize;
                        if let Some(os) = self.slot_owners.get_mut(&ptr) {
                            os.retain(|(_, _, op, _)| op != &pn);
                            if os.is_empty() {
                                self.slot_owners.remove(&ptr);
                            }
                        }
                        if self
                            .typed_slots
                            .get(&ptr)
                            .map(|(_, _, _, sp)| sp == &pn)
                            .unwrap_or(false)
                        {
                            self.typed_slots.remove(&ptr);
                            self.slot_anchor.remove(&ptr);
                        }
                        Some(ptr)
                    } else {
                        None
                    };
                    ob.unset_props.insert(k);
                    drop(ob);
                    if let Some(ptr) = prune {
                        self.prune_typed_slot(ptr);
                    }
                } else if let Some(e) = hidden {
                    // __unset overloads the invisible decl like a
                    // missing prop; without it the access Error wins.
                    if self.find_method_in(&cls, "__unset").is_some()
                        && self
                            .magic_guards
                            .insert((Rc::as_ptr(&o) as usize, 3u8, pn.clone()))
                    {
                        let res = self.method_invoke(
                            o.clone(),
                            "__unset",
                            CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                        );
                        self.magic_guards
                            .remove(&(Rc::as_ptr(&o) as usize, 3u8, pn.clone()));
                        res?;
                    } else {
                        return self.fail(e);
                    }
                } else if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                    // unset() on an uninitialized declared prop still
                    // marks it unset — reads then route to __get
                    // (typed_properties_009/040).
                    let k = if pd.visibility == crate::ast::Visibility::Private {
                        format!("\0{}\0{}", dcls.name(), pd.name)
                    } else {
                        pd.name.clone()
                    };
                    o.borrow_mut().unset_props.insert(k);
                    // __unset only fires for UNDECLARED props — a
                    // declared one is simply marked uninitialized
                    // (typed_properties_magic_set).
                } else if self.aap_active(&o) {
                    // ARRAY_AS_PROPS: undeclared unsets delete from the
                    // storage hash — zend never reaches __unset.
                    let arr = self.ao_state(&o).0;
                    let evicted = arr.borrow_mut().unset(&ArrKey::Str(Rc::from(pn.as_str())));
                    if let Some(v) = evicted {
                        self.destruct_dying_value(&v)?;
                    }
                } else if self.find_method_in(&cls, "__unset").is_some()
                    && self
                        .magic_guards
                        .insert((Rc::as_ptr(&o) as usize, 3u8, pn.clone()))
                {
                    let res = self.method_invoke(
                        o.clone(),
                        "__unset",
                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                    );
                    self.magic_guards
                        .remove(&(Rc::as_ptr(&o) as usize, 3u8, pn.clone()));
                    res?;
                }
                // Real-storage tail: `\0` names error here — magic
                // already dispatched above when __unset existed
                // (bug52484).
                self.check_prop_name(&pn)?;
            }
        }
        Ok(())
    }

    /// Method lookup walking parent chain (uses registered classes).
    /// Visibility + declaring class for a property name (var_dump marks
    /// `["n":protected]` and `["n":"Cls":private]`).
    pub fn prop_visibility(
        &mut self,
        cls: &Rc<PhpClass>,
        name: &str,
    ) -> (crate::ast::Visibility, String) {
        // Private slots are stored mangled ("\0Cls\0name"); the declaring
        // class is encoded directly in the key.
        if let Some(rest) = name.strip_prefix('\0') {
            if let Some((dcls, _)) = rest.split_once('\0') {
                return (crate::ast::Visibility::Private, dcls.to_string());
            }
        }
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name == name && !p.is_static {
                    // A private decl's real slot is the mangled
                    // `\0C\0name` key — it can never own a PLAIN-name
                    // slot, which is then a dynamic prop instead
                    // (bug60536_001).
                    if p.visibility == crate::ast::Visibility::Private {
                        continue;
                    }
                    return (p.visibility, c.name().to_string());
                }
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        (crate::ast::Visibility::Public, String::new())
    }

    /// `Error` when `pn` resolves to a DECLARED prop the current scope
    /// can't see — zend throws `Cannot access private/protected
    /// property` rather than creating a dynamic prop or warning
    /// (bug38461/bug48248). None for undeclared or visible names.
    pub(in crate::interp) fn hidden_decl_error(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<PhpError> {
        // Zend checks the object's own class table for a same-named
        // declaration: an inherited public/protected decl hides the
        // name (Cannot-access when invisible), and a PRIVATE decl
        // hides it only when the object's own class declares it —
        // ancestor-private names still admit a dynamic prop
        // (bug38461/bug48248 vs bug60536_001).
        let (pd, dcls) = self.decl_prop(o, pn).or_else(|| {
            let ob = o.borrow();
            ob.class
                .decl
                .props
                .iter()
                .find(|p| {
                    p.name == pn && !p.is_static && p.visibility == crate::ast::Visibility::Private
                })
                .map(|p| (p.clone(), ob.class.clone()))
        })?;
        let word = match pd.visibility {
            crate::ast::Visibility::Private => "private",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Public => return None,
        };
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        let dn = dcls.name().to_string();
        let allows = match (pd.visibility, scope.as_deref()) {
            (crate::ast::Visibility::Private, Some(s)) => s == dn,
            (crate::ast::Visibility::Protected, Some(s)) => {
                self.is_a_str(s, &dn) || self.is_a_str(&dn, s)
            }
            _ => false,
        };
        if allows {
            return None;
        }
        // The message names the DECLARING class for private but the
        // object's RUNTIME class for protected ('Cannot access
        // protected property B::$x' on a B() even though A declares
        // the prop).
        let en = if pd.visibility == crate::ast::Visibility::Protected {
            o.borrow().class.name().to_string()
        } else {
            dn
        };
        Some(PhpError::uncaught(
            "Error",
            format!("Cannot access {} property {}::${}", word, en, pn),
            0,
        ))
    }

    /// Whether prop `name` on `cls` is readable from the current scope
    /// (foreach over objects iterates only visible props).
    pub fn prop_visible(&mut self, cls: &Rc<PhpClass>, name: &str) -> bool {
        let (vis, dcls) = self.prop_visibility(cls, name);
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        match (vis, scope) {
            (crate::ast::Visibility::Public, _) => true,
            (crate::ast::Visibility::Private, Some(s)) => s == dcls,
            (crate::ast::Visibility::Protected, Some(s)) => {
                self.is_a_str(&s, &dcls) || self.is_a_str(&dcls, &s)
            }
            _ => false,
        }
    }
}

/// `__METHOD__` scope name for a hook on `dcls`: a trait-origin hook
/// keeps its trait name via `decl_in`, else the declaring class.
fn decl_owner(dcls: &Rc<PhpClass>, pname: &str) -> String {
    dcls.decl
        .props
        .iter()
        .find(|p| p.name == pname)
        .and_then(|p| p.decl_in.clone())
        .unwrap_or_else(|| dcls.name().to_string())
}
