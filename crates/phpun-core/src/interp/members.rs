//! Member access: property hooks, prop read/write cells, method
//! dispatch and magic methods, static props/calls, class consts,
//! visibility rules, `is_a` and backtrace/reflection helpers.

use super::util::*;
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

    /// Name of the class whose scope the current frame runs in —
    /// private props are only visible to their own declaring class.
    /// Namespace of the currently executing code — the running
    /// function's declaring namespace, or the file-level `namespace`
    /// for top-level statements (Zend/tests/namespaces).
    pub fn caller_ns(&self) -> String {
        self.stack
            .last()
            .map(|f| f.ns.clone())
            .unwrap_or_else(|| self.globals.ns.clone())
    }

    pub fn caller_scope_name(&self) -> Option<String> {
        self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        })
    }

    /// A private prop of the caller's scope class: it is a *distinct*
    /// property from same-name decls elsewhere in the chain and wins
    /// outright when the caller's scope declares it (private_override).
    pub(in crate::interp) fn scope_private_prop(
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

    /// Foreach iteration spec for a plain object: `(emitted key, slot
    /// key, decl name)` entries for declared props in first-declaration
    /// order — parent props first, a child redecl keeps the first decl's
    /// slot position, private props keep per-class mangled keys, virtual
    /// hooked props appear (no slot) at their decl position. Dynamic
    /// props are NOT included — the loop scans prop_order live so props
    /// added mid-iteration still appear (foreach_002). Returns the spec
    /// plus the declared-name set used to hide shadowed dynamics.
    pub(in crate::interp) fn object_foreach_spec(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
    ) -> (
        Vec<(String, String, String)>,
        std::collections::HashSet<String>,
    ) {
        let mut spec: Vec<(String, String, String)> = Vec::new();
        let mut decl_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        });
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                decl_names.insert(p.name.clone());
            }
        }
        // One entry per prop NAME: each name resolves to a single decl
        // through scope-private-first — a private decl wins only for its
        // own declaring scope, else the first non-private decl. The
        // entry emits at the RESOLVED decl's position (a child's private
        // redecl iterates at the end, after inherited protecteds), and
        // only when that resolved decl is visible — an invisible
        // resolution suppresses the name entirely (C::e skipped under
        // an E scope, but emitted under C).
        let mut emitted: Vec<(usize, usize, String, String)> = Vec::new();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static || done.contains(&p.name) {
                    continue;
                }
                done.insert(p.name.clone());
                // Resolve: first private decl matching the caller scope,
                // else the first non-private decl.
                let mut pick: Option<(usize, usize)> = None;
                for (ci, cc) in chain.iter().enumerate() {
                    for (pi, p2) in cc.decl.props.iter().enumerate() {
                        if p2.name != p.name || p2.is_static {
                            continue;
                        }
                        if p2.visibility == crate::ast::Visibility::Private {
                            if scope.as_ref().is_some_and(|sc| sc == &cc.decl.name) {
                                pick = Some((ci, pi));
                                break;
                            }
                        } else if pick.is_none() {
                            pick = Some((ci, pi));
                        }
                    }
                    if pick.as_ref().is_some_and(|&(ci, pi)| {
                        chain[ci].decl.props[pi].visibility == crate::ast::Visibility::Private
                    }) {
                        break;
                    }
                }
                let Some((ci, pi)) = pick else { continue };
                let decl = &chain[ci].decl.props[pi];
                let dcls = &chain[ci];
                let visible = match decl.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &dcls.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &dcls.decl.name) || self.is_a_str(&dcls.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                // Position: a private decl emits at its own decl position;
                // a non-private prop shares the first declaration's slot.
                let (pci, ppi) = if decl.visibility == crate::ast::Visibility::Private {
                    (ci, pi)
                } else {
                    chain
                        .iter()
                        .enumerate()
                        .find_map(|(i, cc)| {
                            cc.decl
                                .props
                                .iter()
                                .enumerate()
                                .find(|(_, p3)| p3.name == decl.name && !p3.is_static)
                                .map(|(j, _)| (i, j))
                        })
                        .unwrap_or((ci, pi))
                };
                let slot_key = if decl.visibility == crate::ast::Visibility::Private {
                    format!("\0{}\0{}", dcls.decl.name, decl.name)
                } else {
                    decl.name.clone()
                };
                emitted.push((pci, ppi, decl.name.clone(), slot_key));
            }
        }
        emitted.sort_by_key(|(a, b, _, _)| (*a, *b));
        for (_, _, n, k) in emitted {
            spec.push((n.clone(), k, n));
        }
        (spec, decl_names)
    }

    /// Serialization view for get_object_vars/json_encode/var_export:
    /// per-DECL entries in parent-first order — non-private decls
    /// dedupe by name at their first position while private decls emit
    /// one entry per declaring class (both `changed`s in dump.phpt).
    /// Returns `(emitted key, slot key, decl+decl class)` entries;
    /// dynamic props appended live in insertion order carry `None` —
    /// the caller filters by visibility and resolves values.
    pub fn object_serial_entries(&self, o: &Rc<RefCell<PhpObject>>) -> Vec<SerialEntry> {
        let mut entries = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                if p.visibility == crate::ast::Visibility::Private {
                    entries.push((
                        p.name.clone(),
                        format!("\0{}\0{}", c.decl.name, p.name),
                        Some((p.clone(), c.clone())),
                    ));
                } else if seen.insert(p.name.clone()) {
                    entries.push((p.name.clone(), p.name.clone(), Some((p.clone(), c.clone()))));
                }
            }
        }
        // Dynamic props follow the declared entries in insertion order
        // (gh20479's g/h, oss-fuzz-382922236's b); mangled keys are
        // declared-private slots emitted by their own entries already.
        let emitted: std::collections::HashSet<String> =
            entries.iter().map(|(_, s, _)| s.clone()).collect();
        for k in &o.borrow().prop_order {
            if emitted.contains(k) || k.starts_with('\0') {
                continue;
            }
            entries.push((k.clone(), k.clone(), None));
        }
        entries
    }

    /// Resolve one serial entry's value: a hooked prop runs its `get`
    /// (write-only props are skipped); a plain prop yields the live
    /// slot (uninitialized slots are skipped).
    pub fn serial_entry_value(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        slot: &str,
    ) -> Option<Value> {
        let hs: MergedHooks = if p.visibility == crate::ast::Visibility::Private {
            p.hooks
                .as_ref()
                .map(|hs| hs.iter().cloned().map(|h| (h, dcls.clone())).collect())
                .unwrap_or_default()
        } else {
            match self.hooked_prop(o, &p.name) {
                Some((_, hs)) => hs,
                None => Vec::new(),
            }
        };
        if !hs.is_empty() {
            if let Some((h, c)) = hs.iter().find(|(h, _)| h.is_get && h.body.is_some()) {
                // Serialization bypasses the caller's visibility — a
                // private hook runs in its own declaring scope (dump).
                let v = self.run_hook(o, c, &p.name, h, None).ok()?;
                return self.hook_get_typecheck(p, c, v).ok();
            }
            // A set-only hooked prop still backs a slot — serialization
            // reads it raw, like a plain prop (gh17988).
        }
        o.borrow().props.get(slot).map(|c| c.borrow().clone())
    }

    /// get_class_vars(): declared prop defaults in parent-first decl
    /// order, filtered by caller visibility — hooked props keep their
    /// raw default (no `get` run), virtual props and private props
    /// outside scope are omitted (gh15456).
    pub fn class_default_props(&mut self, cls: &Rc<PhpClass>) -> Vec<(String, Value)> {
        let mut chain = Vec::new();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.caller_scope_name();
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (ci, c) in chain.iter().enumerate() {
            for p in &c.decl.props {
                let visible = match p.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &c.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &c.decl.name) || self.is_a_str(&c.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                if p.visibility != crate::ast::Visibility::Private && !seen.insert(p.name.clone()) {
                    continue;
                }
                // Virtual hooked props have no storage — not in vars.
                if p.hooks.is_some()
                    && !Self::prop_is_backed(p)
                    && !chain[..=ci].iter().any(|c2| {
                        c2.decl.props.iter().any(|p2| {
                            p2.name == p.name
                                && p2.hooks.is_none()
                                && p2.visibility != crate::ast::Visibility::Private
                        })
                    })
                {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => {
                        let old = self.const_self.replace(c.clone());
                        self.class_const_ctx += 1;
                        let v = self.eval_const(d).unwrap_or(Value::Null);
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        v
                    }
                    None => Value::Null,
                };
                out.push((p.name.clone(), v));
            }
        }
        out
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

    /// The nearest PropDecl for `pn` along the chain, honoring the same
    /// private-scope rules as `hooked_prop` (hooked or plain).
    /// Declared *static* prop lookup across the class chain
    /// (`Foo::$i = v` write checks).
    pub(in crate::interp) fn find_static_prop_decl(
        &self,
        cls: &Rc<PhpClass>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let priv_ok = std::rc::Rc::ptr_eq(&c, cls);
            if let Some(pd) = c.decl.props.iter().find(|p| {
                p.name == pn
                    && p.is_static
                    && (priv_ok || p.visibility != crate::ast::Visibility::Private)
            }) {
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

    pub(in crate::interp) fn stmts_use_this_prop(v: &[Stmt], pn: &str) -> bool {
        v.iter().any(|s| Self::stmt_uses_this_prop(s, pn))
    }

    pub(in crate::interp) fn stmt_uses_this_prop(s: &Stmt, pn: &str) -> bool {
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
                .filter_map(|(_, d)| d.as_ref())
                .any(|e| Self::expr_uses_this_prop(e, pn)),
            Stmt::Declare { value, .. } => Self::expr_uses_this_prop(value, pn),
            _ => false,
        }
    }

    pub(in crate::interp) fn expr_uses_this_prop(e: &Expr, pn: &str) -> bool {
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
            Expr::List(items) => items
                .iter()
                .flatten()
                .any(|e| Self::expr_uses_this_prop(e, pn)),
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
    pub(in crate::interp) fn prop_scope_class(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<Rc<PhpClass>> {
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

    pub(in crate::interp) fn hook_visibility_error<T>(
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
    pub(in crate::interp) fn resolved_ty(&self, tys: &[String], dcls: &PhpClass) -> Vec<String> {
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
    pub(in crate::interp) fn ty_member_intersect(&mut self, a: &str, b: &str) -> Option<String> {
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
    pub(in crate::interp) fn hook_set_arg_check(
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

    /// Native bodies for the ArrayIterator stub. Iteration state lives in
    /// the `ArrayIter` object internal; unknown methods return None so
    /// the generic dispatch can report `Call to undefined method`.
    pub(in crate::interp) fn array_iter_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        let mk_arr = |ob: &mut PhpObject, a: Rc<RefCell<PhpArray>>, flags: i64| {
            ob.internal = Some(ObjectInternal::ArrayIter {
                arr: a,
                pos: 0,
                flags,
            });
            Value::Null
        };
        match lname.as_str() {
            "__construct" => {
                let flags = args.cells.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                let mut ob = obj.borrow_mut();
                let a = match args.cells.first().map(|c| c.borrow().clone()) {
                    Some(Value::Array(a)) => {
                        let mut copy = PhpArray::new();
                        for (k, c) in &a.borrow().entries {
                            copy.set(k.clone(), c.borrow().clone());
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    // Objects iterate their prop cells BY REFERENCE —
                    // writes through $v update the prop (typed gate
                    // still applies); deprecated since 8.5
                    // (typed_properties_113/114/115).
                    Some(Value::Object(o)) => {
                        drop(ob);
                        self.deprecated(
                            "ArrayIterator::__construct(): Using an object as a backing array for ArrayIterator is deprecated, as it allows violating class constraints and invariants",
                        )?;
                        ob = obj.borrow_mut();
                        let mut copy = PhpArray::new();
                        copy.is_ref = true;
                        let pairs: Vec<(String, Cell)> = o
                            .borrow()
                            .props
                            .iter()
                            .map(|(k, c)| (k.clone(), c.clone()))
                            .collect();
                        for (k, c) in pairs {
                            let pn = k.rsplit('\0').next().unwrap_or(&k).to_string();
                            if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                                let p = Rc::as_ptr(&c) as usize;
                                if let Some(tys) = &pd.ty {
                                    // Writes through the shared cell
                                    // still hit the typed gate.
                                    self.typed_slots.insert(
                                        p,
                                        (
                                            c.clone(),
                                            tys.clone(),
                                            dcls.name().to_string(),
                                            pn.clone(),
                                        ),
                                    );
                                    self.slot_anchor
                                        .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), k.clone()));
                                    self.slot_owners.entry(p).or_default().push((
                                        tys.clone(),
                                        dcls.name().to_string(),
                                        pn.clone(),
                                        SlotAnchor::Obj(Rc::downgrade(&o), k.clone()),
                                    ));
                                }
                                // Remember readonly cells — by-ref
                                // acquisition must fail (115).
                                if pd.readonly {
                                    self.readonly_cells
                                        .insert(p, (dcls.name().to_string(), pn.clone()));
                                }
                            }
                            copy.bind_cell(ArrKey::Str(Rc::from(k.as_str())), c);
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    _ => Rc::new(RefCell::new(PhpArray::new())),
                };
                Ok(Some(mk_arr(&mut ob, a, flags)))
            }
            _ => {
                // All remaining methods need initialized state.
                let (arr, pos) = {
                    let ob = obj.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { arr, pos, .. }) => (arr.clone(), *pos),
                        _ => return Ok(None),
                    }
                };
                let v = match lname.as_str() {
                    "rewind" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = 0;
                        }
                        Value::Null
                    }
                    "valid" => Value::Bool(pos < arr.borrow().entries.len()),
                    "current" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(_, c)| c.borrow().clone())
                        .unwrap_or(Value::Bool(false)),
                    "key" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(k, _)| key_value(k))
                        .unwrap_or(Value::Null),
                    "next" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p += 1;
                        }
                        Value::Null
                    }
                    "seek" => {
                        let i = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        let len = arr.borrow().entries.len() as i64;
                        if i < 0 || i >= len.max(1) && !(i == 0 && len == 0) {
                            return self.fail(PhpError::uncaught(
                                "OutOfBoundsException",
                                format!("Seek position {} is out of range", i),
                                0,
                            ));
                        }
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = i as usize;
                        }
                        Value::Null
                    }
                    "count" => Value::Int(arr.borrow().entries.len() as i64),
                    "getarraycopy" => Value::Array(arr.clone()),
                    "offsetget" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        match arr.borrow().get(&k) {
                            Some(v) => v,
                            None => {
                                let kn = key_value(&k).to_php_string();
                                let _ = self.warn(&format!("Undefined array key {}", kn));
                                Value::Null
                            }
                        }
                    }
                    "offsetexists" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        Value::Bool(arr.borrow().get(&k).is_some())
                    }
                    "offsetset" => {
                        let v = args
                            .cells
                            .get(1)
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        match args.cells.first().map(|c| c.borrow().clone()) {
                            Some(Value::Null) | None => arr.borrow_mut().push(v),
                            Some(kv) => arr.borrow_mut().set(to_key(&kv), v),
                        }
                        Value::Null
                    }
                    "offsetunset" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        arr.borrow_mut().unset(&k);
                        Value::Null
                    }
                    "getflags" => Value::Int({
                        let ob = obj.borrow();
                        match &ob.internal {
                            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags,
                            _ => 0,
                        }
                    }),
                    "setflags" => {
                        let f = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        if let Some(ObjectInternal::ArrayIter { flags, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *flags = f;
                        }
                        Value::Null
                    }
                    "asort" | "ksort" => {
                        let mut a = arr.borrow_mut();
                        if lname == "asort" {
                            a.entries
                                .sort_by(|(_, x), (_, y)| compare(&x.borrow(), &y.borrow()));
                        } else {
                            a.entries.sort_by(|(x, _), (y, _)| match (x, y) {
                                (ArrKey::Int(a), ArrKey::Int(b)) => a.cmp(b),
                                _ => compare(&key_value(x), &key_value(y)),
                            });
                        }
                        Value::Bool(true)
                    }
                    "natsort" | "natcasesort" => {
                        let ci = lname == "natcasesort";
                        arr.borrow_mut().entries.sort_by(|(_, x), (_, y)| {
                            let mut a = x.borrow().to_php_string();
                            let mut b = y.borrow().to_php_string();
                            if ci {
                                a = a.to_lowercase();
                                b = b.to_lowercase();
                            }
                            compare(&Value::str(a), &Value::str(b))
                        });
                        Value::Bool(true)
                    }
                    _ => return Ok(None),
                };
                Ok(Some(v))
            }
        }
    }

    /// Call-arg list for `invokeArgs`/`newInstanceArgs`: array entries
    /// with string keys become named args (named_params/call_user_func).
    pub(in crate::interp) fn args_from_array(&mut self, v: &Value) -> CallArgs {
        let mut ca = CallArgs::empty();
        if let Value::Array(a) = v {
            for (k, c) in a.borrow().iter() {
                match k {
                    ArrKey::Str(s) => ca.named.push((s.to_string(), c.clone(), true, false)),
                    _ => ca.cells.push(c.clone()),
                }
            }
        }
        ca
    }

    /// Native bodies for the Reflection* stubs. The reflected
    /// class/function/prop names live under `\0rc\0` prop keys.
    pub(in crate::interp) fn reflection_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        match lname.as_str() {
            "__construct" => {
                let mut ob = obj.borrow_mut();
                let cls = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // Zend reflectors keep the class NAME, not the object —
                // `new ReflectionClass(new T)` drops the arg temp so
                // its __destruct runs at statement end (bug29368_2).
                let cls = match &cls {
                    Value::Object(o) => Value::str(o.borrow().class.name()),
                    _ => cls,
                };
                let prop = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                ob.props.insert("\0rc\0class".into(), cell(cls.clone()));
                ob.props.insert("\0rc\0prop".into(), cell(prop.clone()));
                // Public metadata props the real reflectors expose:
                // ReflectionProperty::{class,name}, ReflectionMethod::
                // {class,name}, ReflectionClass/Function::name. `class`
                // keeps the canonical (declared-case) class name.
                let cname = match &cls {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Str(s) => {
                        let raw = String::from_utf8_lossy(s).to_string();
                        let resolved = self.resolve_class(&raw).unwrap_or_else(|| raw.clone());
                        self.classes
                            .get(&resolved.to_lowercase())
                            .map(|c| c.decl.name.clone())
                            .unwrap_or(resolved)
                    }
                    // A closure first arg is a Closure object to Zend
                    // (bug69802_2).
                    Value::Callable(_) => "Closure".into(),
                    _ => String::new(),
                };
                match ob.class.name().to_lowercase().as_str() {
                    "reflectionproperty" | "reflectionmethod" | "reflectionclassconstant" => {
                        ob.props.insert("name".into(), cell(prop));
                        ob.props.insert("class".into(), cell(Value::str(&cname)));
                        // Public metadata props render in var_dump in
                        // declaration order: name, then class.
                        for k in ["name", "class"] {
                            if !ob.prop_order.contains(&k.into()) {
                                ob.prop_order.push(k.into());
                            }
                        }
                    }
                    "reflectionclass" | "reflectionfunction" => {
                        // A closure reflector's `name` is its Zend name
                        // `{closure:enclosing():L}` (closure_065).
                        let nm = match &cls {
                            Value::Callable(c) => match &c.kind {
                                CallableKind::Closure(d) => Value::str(&d.name),
                                CallableKind::Named(n) => Value::str(n),
                                CallableKind::Method { name, .. } => Value::str(name),
                            },
                            _ => cls,
                        };
                        ob.props.insert("name".into(), cell(nm));
                        if !ob.prop_order.contains(&"name".into()) {
                            ob.prop_order.push("name".into());
                        }
                    }
                    _ => {}
                }
                Ok(Some(Value::Null))
            }
            // ReflectionFunction::invoke(...$args) and
            // ReflectionMethod::invoke($object, ...$args) forward named
            // args to the target (named_params/call_user_func).
            "invoke" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = CallArgs {
                        cells: args.cells[1.min(args.cells.len())..].to_vec(),
                        named: args.named.clone(),
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        Value::Null => {
                            // Static context: Class::method or null $this.
                            let cn = obj
                                .borrow()
                                .props
                                .get("\0rc\0class")
                                .map(|c| c.borrow().clone())
                                .unwrap_or(Value::Null);
                            let cn = self.conv_str(&cn)?.to_string();
                            Ok(Some(self.call_named(&format!("{}::{}", cn, mn), &[])?))
                        }
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    };
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            "invokeargs" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let arr = args
                        .get(1)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = self.args_from_array(&arr);
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = self.args_from_array(&arr);
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            // Name introspection shared by function/class reflectors
            // (closure_067/068): closures report their zend name.
            "getshortname" | "getnamespacename" | "innamespace" | "isanonymous" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let (fname, anon) = match &stored {
                    Value::Callable(c) => (
                        self.callable_ctx_name(&stored),
                        matches!(c.kind, CallableKind::Closure(_)),
                    ),
                    _ => {
                        let n = self.conv_str(&stored)?.to_string();
                        // A class reflector is anonymous when the CLASS
                        // is — anon classes carry `class@anonymous` in
                        // their generated name (the closure check below
                        // only covers function reflectors).
                        let anon_cls = self
                            .classes
                            .get(&n.to_lowercase())
                            .map(|c| c.decl.name.contains("class@anonymous"))
                            .unwrap_or(false);
                        (n, anon_cls)
                    }
                };
                // A closure's "short name" is its whole zend name —
                // the `\` inside `{closure:Foo\Bar::baz():N}` is part
                // of the literal (closure_067).
                let (short, ns) = if anon {
                    (fname.clone(), String::new())
                } else {
                    (
                        fname.rsplit('\\').next().unwrap_or(&fname).to_string(),
                        match fname.rfind('\\') {
                            Some(i) => fname[..i].to_string(),
                            None => String::new(),
                        },
                    )
                };
                Ok(Some(match lname.as_str() {
                    "getshortname" => Value::str(short),
                    "getnamespacename" => Value::str(ns),
                    "innamespace" => Value::Bool(!anon && fname.contains('\\')),
                    _ => Value::Bool(anon),
                }))
            }
            // ReflectionFunctionAbstract closure accessors
            // (closure_031/042). A function reflector keeps the
            // callable under \0rc\0class; a method reflector keeps
            // {class-name, method-name} — resolve it for scope.
            "isclosure"
            | "getclosure"
            | "getclosurescopeclass"
            | "getclosurecalledclass"
            | "getclosurethis" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let cb = match &stored {
                    Value::Callable(c) => Some(c.clone()),
                    _ => None,
                };
                if lname == "isclosure" {
                    return Ok(Some(Value::Bool(matches!(
                        cb.as_ref().map(|c| &c.kind),
                        Some(CallableKind::Closure(_))
                    ))));
                }
                if lname == "getclosure" {
                    if cb.is_some() {
                        return Ok(Some(stored));
                    }
                    if is_method {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mn = obj
                            .borrow()
                            .props
                            .get("\0rc\0prop")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let mn = self.conv_str(&mn)?.to_string();
                        let target = args
                            .first()
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let (mc, tc) = match target {
                            Value::Object(o) => (o.borrow().class.clone(), Some(o.clone())),
                            _ => (
                                self.classes
                                    .get(&cn.to_lowercase())
                                    .cloned()
                                    .ok_or_else(|| {
                                        PhpError::uncaught(
                                            "ReflectionException",
                                            format!("Class {} does not exist", cn),
                                            0,
                                        )
                                    })?,
                                None,
                            ),
                        };
                        let (mdecl, decl_cls) = self
                            .find_method_in(&mc, &mn)
                            .map(|(m, dc)| (Some(m), dc))
                            .unwrap_or_else(|| (None, mc.clone()));
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Method {
                                obj: tc,
                                class: Some(mc.clone()),
                                name: mn,
                            },
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: Some(decl_cls.clone()),
                            called_class: Some(mc),
                            // Static methods produce static closures —
                            // rebinding an instance warns (closure_061).
                            is_static: mdecl.map(|m| m.is_static).unwrap_or(false),
                        }))));
                    }
                    // A function reflector's getClosure is a named
                    // callable — builtins included (bug70630).
                    let fname = self.conv_str(&stored)?.to_string();
                    if !fname.is_empty() {
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Named(fname),
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: None,
                            called_class: None,
                            is_static: false,
                        }))));
                    }
                    return Ok(Some(Value::Null));
                }
                // Scope/this accessors.
                let (scope, called, this) = match &cb {
                    Some(c) => (
                        c.scope_class.clone(),
                        c.called_class.clone(),
                        c.this_obj.clone(),
                    ),
                    None if is_method => {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mc = self.classes.get(&cn.to_lowercase()).cloned();
                        (mc.clone(), mc, None)
                    }
                    None => (None, None, None),
                };
                match lname.as_str() {
                    "getclosurethis" => Ok(Some(match this {
                        Some(o) => Value::Object(o),
                        None => Value::Null,
                    })),
                    _ => {
                        let rc = if lname == "getclosurescopeclass" {
                            scope
                        } else {
                            called
                        };
                        // Dummy scope: a closure bound to $this with
                        // no real scope reflects class Closure
                        // (closure_042).
                        let nm = rc.map(|c| c.name().to_string()).or_else(|| {
                            if lname == "getclosurescopeclass" && this.is_some() {
                                Some("Closure".to_string())
                            } else {
                                None
                            }
                        });
                        match nm {
                            Some(n) => {
                                let r = self.instantiate("reflectionclass", &[])?;
                                if let Value::Object(o) = &r {
                                    let mut ob = o.borrow_mut();
                                    ob.props.insert("name".into(), cell(Value::str(&n)));
                                    ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                                }
                                Ok(Some(r))
                            }
                            None => Ok(Some(Value::Null)),
                        }
                    }
                }
            }
            // ReflectionClass::getProperty($name) -> ReflectionProperty
            // carrying {\0rc\0class, \0rc\0prop} (typed_properties_018).
            "getproperty" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let cls = self.classes.get(&cn.to_lowercase()).cloned();
                let found = cls.as_ref().and_then(|c| self.find_prop_decl(c, &pn));
                match found {
                    Some((_, dcls)) => {
                        let rp = self.instantiate("reflectionproperty", &[])?;
                        if let Value::Object(o) = &rp {
                            let mut ob = o.borrow_mut();
                            ob.props
                                .insert("\0rc\0class".into(), cell(Value::str(dcls.name())));
                            ob.props.insert("\0rc\0prop".into(), cell(Value::str(&pn)));
                            ob.props.insert("name".into(), cell(Value::str(&pn)));
                            ob.props
                                .insert("class".into(), cell(Value::str(dcls.name())));
                        }
                        Ok(Some(rp))
                    }
                    None => self.fail(PhpError::uncaught(
                        "ReflectionException",
                        format!("Property {}::${} does not exist", cn, pn),
                        0,
                    )),
                }
            }
            "hasproperty" => {
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // ReflectionObject wraps the object itself; check its
                // live + declared props (bug50146). Closures never have
                // props.
                match &target {
                    Value::Object(t) => {
                        let has =
                            t.borrow().props.contains_key(&pn) || self.decl_prop(t, &pn).is_some();
                        return Ok(Some(Value::Bool(has)));
                    }
                    Value::Callable(_) | Value::Null => {
                        return Ok(Some(Value::Bool(false)));
                    }
                    _ => {}
                }
                let cn = self.conv_str(&target)?.to_string();
                let has = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_prop_decl(&c, &pn))
                    .is_some();
                Ok(Some(Value::Bool(has)))
            }
            // ReflectionProperty::getType() -> ReflectionNamedType with
            // the declared members under \0rp\0ty (same convention as
            // ReflectionParameter).
            "gettype" => {
                let is_prop = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionproperty");
                if is_prop {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let ty = self
                        .classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_prop_decl(&c, &pn))
                        .and_then(|(pd, _)| pd.ty);
                    match ty {
                        Some(tys) => {
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ta = PhpArray::default();
                                for m in &tys {
                                    ta.push(Value::str(m));
                                }
                                let mut ob = o.borrow_mut();
                                ob.props.insert(
                                    "\0rp\0ty".into(),
                                    cell(Value::Array(Rc::new(RefCell::new(ta)))),
                                );
                                ob.props
                                    .insert("name".into(), cell(Value::str(tys.first().unwrap())));
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                } else {
                    // ReflectionParameter::getType() — members stored
                    // under \0rp\0ty by getParameters()
                    // (trampoline_closure_named_arguments).
                    let is_param = obj
                        .borrow()
                        .class
                        .name()
                        .eq_ignore_ascii_case("reflectionparameter");
                    let tys = if is_param {
                        obj.borrow()
                            .props
                            .get("\0rp\0ty")
                            .map(|c| c.borrow().clone())
                            .and_then(|v| match v {
                                Value::Array(a) => Some(a),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    match tys {
                        Some(ta) => {
                            let members: Vec<String> = ta
                                .borrow()
                                .entries
                                .iter()
                                .map(|(_, c)| c.borrow().to_php_string())
                                .collect();
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ob = o.borrow_mut();
                                ob.props
                                    .insert("\0rp\0ty".into(), cell(Value::Array(ta.clone())));
                                if let Some(first) = members.first() {
                                    ob.props
                                        .insert("name".into(), cell(Value::str(first.clone())));
                                }
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                }
            }
            // ReflectionClass::getDefaultProperties(): prop defaults
            // keyed by prop name; typed props without defaults are
            // absent (bug #77673 — typed_properties_105).
            "getdefaultproperties" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(cls) = self.classes.get(&cn.to_lowercase()).cloned() {
                    let mut chain: Vec<Rc<PhpClass>> = Vec::new();
                    let mut cur = Some(cls);
                    while let Some(c) = cur {
                        chain.push(c.clone());
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    for c in chain.iter().rev() {
                        for p in &c.decl.props {
                            if p.ty.is_some() && p.default.is_none() {
                                continue;
                            }
                            let dv = match &p.default {
                                Some(d) => {
                                    let old = self.const_self.replace(c.clone());
                                    self.class_const_ctx += 1;
                                    let r = self.eval_decl_const(d, &c.decl.file);
                                    self.class_const_ctx -= 1;
                                    self.const_self = old;
                                    match r {
                                        Ok(v) => v,
                                        Err(_) => continue,
                                    }
                                }
                                None => Value::Null,
                            };
                            arr.set(ArrKey::Str(p.name.clone().into()), dv);
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getparameters" => {
                // Each param becomes a ReflectionParameter carrying its
                // declared type members under \0rp\0ty (callable_002).
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let decl: Option<Rc<crate::ast::FunctionDecl>> = if is_method {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    if matches!(&cn, Value::Callable(_)) {
                        // new ReflectionMethod($closure, '__invoke') —
                        // Closure::__invoke carries the wrapped
                        // function's signature (bug69802_2).
                        if mn.eq_ignore_ascii_case("__invoke") {
                            self.callable_decl(&cn)
                        } else {
                            None
                        }
                    } else {
                        let cn = self.conv_str(&cn)?.to_string();
                        let c = self.classes.get(&cn.to_lowercase()).cloned();
                        match c {
                            Some(c) => self
                                .find_method_in(&c, &mn)
                                .map(|(m, _)| Rc::new(m.decl.clone())),
                            None => None,
                        }
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.callable_decl(&cb)
                };
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for p in &d.params {
                        let rp = self.instantiate("reflectionparameter", &[])?;
                        if let Value::Object(o) = &rp {
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0name".into(), cell(Value::str(&p.name)));
                            // Zend's ReflectionParameter exposes the name
                            // as a public prop rendered by var_dump.
                            let mut ob = o.borrow_mut();
                            ob.props.insert("name".into(), cell(Value::str(&p.name)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                            drop(ob);
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0variadic".into(), cell(Value::Bool(p.variadic)));
                            let mut ta = PhpArray::default();
                            if let Some(ty) = &p.ty {
                                for m in ty {
                                    ta.push(Value::str(m));
                                }
                            }
                            o.borrow_mut().props.insert(
                                "\0rp\0ty".into(),
                                cell(Value::Array(Rc::new(RefCell::new(ta)))),
                            );
                        }
                        arr.push(rp);
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "isvariadic" => Ok(Some(Value::Bool(
                obj.borrow()
                    .props
                    .get("\0rp\0variadic")
                    .is_some_and(|c| c.borrow().is_truthy()),
            ))),
            "hastype" => {
                // ReflectionParameter::hasType() — \0rp\0ty members
                // populated by getParameters().
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(!a.borrow().entries.is_empty()),
                        _ => None,
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getclass" => {
                // Deprecated since 8.0 — returns a ReflectionClass for
                // the first class/interface member of the declared
                // type (a union picks the class part — bug69802_2).
                self.deprecated(
                    "Method ReflectionParameter::getClass() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let ty = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    });
                let class_ty = ty.and_then(|ta| {
                    ta.borrow().entries.iter().find_map(|(_, c)| {
                        let n = c.borrow().to_php_string();
                        self.classes
                            .get(&n.to_lowercase())
                            .map(|cl| cl.decl.name.clone())
                            .or_else(|| {
                                self.interfaces
                                    .get(&n.to_lowercase())
                                    .map(|d| d.name.clone())
                            })
                    })
                });
                match class_ty {
                    Some(n) => {
                        let rc = self.instantiate("reflectionclass", &[])?;
                        if let Value::Object(o) = &rc {
                            let mut ob = o.borrow_mut();
                            ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                            ob.props.insert("name".into(), cell(Value::str(&n)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                        }
                        Ok(Some(rc))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "iscallable" => {
                self.deprecated(
                    "Method ReflectionParameter::isCallable() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .map(|a| {
                        a.borrow().entries.iter().any(|(_, c)| {
                            matches!(&*c.borrow(), Value::Str(s) if s.eq_ignore_ascii_case(b"callable"))
                        })
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getattributes" => {
                let is_fn = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionfunction");
                let tn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let tn = self.conv_str(&tn)?.to_string();
                let is_cc = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionclassconstant");
                if is_cc {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let decls = self
                        .find_const_decl(&cn, &pn)
                        .map(|(cd, _)| cd.attrs.clone())
                        .unwrap_or_default();
                    let fname = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                        .transpose()?
                        .unwrap_or_default();
                    let mut arr = PhpArray::default();
                    for a in decls {
                        if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                            continue;
                        }
                        let v = self.instantiate("reflectionattribute", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                                name: a.name.clone(),
                                args: Rc::new(a.args.clone()),
                                target: 16,
                            });
                        }
                        arr.push(v);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let is_m = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_m {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let decls = self
                        .classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_method_in(&c, &mn))
                        .map(|(m, _)| m.decl.attrs.clone())
                        .unwrap_or_default();
                    let fname = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                        .transpose()?
                        .unwrap_or_default();
                    let mut arr = PhpArray::default();
                    for a in decls {
                        if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                            continue;
                        }
                        let v = self.instantiate("reflectionattribute", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                                name: a.name.clone(),
                                args: Rc::new(a.args.clone()),
                                target: 4,
                            });
                        }
                        arr.push(v);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let (decls, target): (Vec<crate::ast::AttrDecl>, i64) = if is_fn {
                    (
                        self.functions
                            .get(&tn.to_lowercase())
                            .map(|d| d.attrs.clone())
                            .unwrap_or_default(),
                        2,
                    )
                } else {
                    (
                        self.classes
                            .get(&tn.to_lowercase())
                            .map(|c| c.decl.attrs.clone())
                            .unwrap_or_default(),
                        1,
                    )
                };
                // Optional class-name filter arg.
                let fname = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                    .transpose()?
                    .unwrap_or_default();
                let mut arr = PhpArray::default();
                for a in decls {
                    if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                        continue;
                    }
                    let v = self.instantiate("reflectionattribute", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                            name: a.name.clone(),
                            args: Rc::new(a.args.clone()),
                            target,
                        });
                    }
                    arr.push(v);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getarguments" => {
                let exprs = match &obj.borrow().internal {
                    Some(ObjectInternal::ReflectionAttribute { args, .. }) => args.clone(),
                    _ => return Ok(Some(Value::Null)),
                };
                let mut arr = PhpArray::default();
                for e in exprs.iter() {
                    if let Expr::Binary { op: "named", l, r } = e {
                        if let Expr::Str(n) = l.as_ref() {
                            let v = self.eval_const(r)?;
                            arr.set(ArrKey::Str(n.as_str().into()), v);
                            continue;
                        }
                    }
                    arr.push(self.eval_const(e)?);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "newinstance" | "newinstanceargs" => {
                if let Some(ObjectInternal::ReflectionAttribute {
                    name,
                    args: aexprs,
                    target,
                }) = &obj.borrow().internal
                {
                    let (name, aexprs, target) = (name.clone(), aexprs.clone(), *target);
                    let lname = name.trim_start_matches('\\').to_lowercase();
                    let cls = match self.classes.get(&lname).cloned() {
                        Some(c) => c,
                        None => {
                            // `new ReflectionClass` may not have loaded
                            // the attribute class yet — trigger autoload.
                            let resolved = self
                                .resolve_class(name.trim_start_matches('\\'))
                                .unwrap_or_else(|| name.clone());
                            match self.classes.get(&resolved.to_lowercase()).cloned() {
                                Some(c) => c,
                                None => {
                                    return self
                                        .fail::<Option<Value>>(PhpError::uncaught(
                                            "Error",
                                            format!("Class \"{}\" not found", name),
                                            0,
                                        ))
                                        .map(|_| None);
                                }
                            }
                        }
                    };
                    let short = |n: &str| n.rsplit('\\').next().unwrap_or(n).to_string();
                    let marker = cls
                        .decl
                        .attrs
                        .iter()
                        .find(|a| short(&a.name).eq_ignore_ascii_case("attribute"));
                    let Some(marker) = marker else {
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attempting to use non-attribute class \"{}\" as attribute",
                                    name
                                ),
                                0,
                            ))
                            .map(|_| None);
                    };
                    // `#[Attribute(flags: N)]` (or first positional) gates
                    // which declarations the attribute may target.
                    let mut mask = 63i64;
                    let flags_e = marker
                        .args
                        .iter()
                        .find_map(|a| {
                            if let Expr::Binary { op: "named", l, r } = a {
                                if matches!(l.as_ref(), Expr::Str(n) if n == "flags") {
                                    return Some(r.as_ref());
                                }
                                None
                            } else {
                                None
                            }
                        })
                        .or_else(|| marker.args.first());
                    if let Some(e) = flags_e {
                        mask = self.eval_const(e)?.to_int();
                    }
                    if mask & target == 0 {
                        let tn = match target {
                            1 => "class",
                            2 => "function",
                            4 => "method",
                            8 => "property",
                            16 => "class constant",
                            32 => "parameter",
                            _ => "unknown",
                        };
                        let allowed: Vec<&str> = [
                            (1i64, "class"),
                            (2, "function"),
                            (4, "method"),
                            (8, "property"),
                            (16, "class constant"),
                            (32, "parameter"),
                        ]
                        .iter()
                        .filter(|(b, _)| mask & b != 0)
                        .map(|(_, n)| *n)
                        .collect();
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attribute \"{}\" cannot target {} (allowed targets: {})",
                                    name,
                                    tn,
                                    allowed.join(", ")
                                ),
                                0,
                            ))
                            .map(|_| None);
                    }
                    let mut cells = Vec::new();
                    let mut named = Vec::new();
                    for e in aexprs.iter() {
                        if let Expr::Binary { op: "named", l, r } = e {
                            if let Expr::Str(n) = l.as_ref() {
                                named.push((n.clone(), cell(self.eval_const(r)?), true, false));
                                continue;
                            }
                        }
                        cells.push(cell(self.eval_const(e)?));
                    }
                    let ca = CallArgs {
                        cells,
                        named,
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    return self.new_instance(&name, ca).map(Some);
                }
                let ca = if lname == "newinstanceargs" {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.args_from_array(&arr)
                } else {
                    CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    }
                };
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                Ok(Some(self.new_instance(&cn, ca)?))
            }
            "getname" => {
                if let Some(ObjectInternal::ReflectionAttribute { name, .. }) =
                    &obj.borrow().internal
                {
                    return Ok(Some(Value::str(name.clone())));
                }
                let clsname = obj.borrow().class.name().to_lowercase();
                // ReflectionNamedType::getName() -> the stored type name.
                if clsname == "reflectionnamedtype" {
                    return Ok(Some(
                        obj.borrow()
                            .props
                            .get("name")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null),
                    ));
                }
                // ReflectionClassConstant::getName() is the const name;
                // ReflectionProperty::getName() the prop name; every
                // other reflector reports its class/subject.
                let key = match clsname.as_str() {
                    "reflectionclassconstant"
                    | "reflectionclass"
                    | "reflectionfunction"
                    | "reflectionmethod" => "name",
                    "reflectionproperty" => "\0rc\0prop",
                    "reflectionparameter" => "\0rp\0name",
                    _ => "\0rc\0class",
                };
                Ok(Some(
                    obj.borrow()
                        .props
                        .get(key)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            // ReflectionClass::getParentClass() -> a ReflectionClass of
            // the parent, or false when there is none (php-enum walks
            // ancestors this way).
            "getparentclass" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let parent = self
                    .classes
                    .get(&cn.to_lowercase())
                    .and_then(|c| c.decl.parent.clone());
                match parent {
                    Some(p) => {
                        let pcn = self
                            .classes
                            .get(&p.to_lowercase())
                            .map(|c| c.decl.name.clone())
                            .unwrap_or(p);
                        let rc = self.instantiate("reflectionclass", &[])?;
                        if let Value::Object(o) = &rc {
                            let mut ob = o.borrow_mut();
                            ob.props
                                .insert("\0rc\0class".into(), cell(Value::str(&pcn)));
                            ob.props.insert("name".into(), cell(Value::str(&pcn)));
                        }
                        Ok(Some(rc))
                    }
                    None => Ok(Some(Value::Bool(false))),
                }
            }
            // getshortname/getnamespacename are handled by the earlier
            // combined name-introspection arm (a second arm here would be
            // unreachable — clippy failure surfaced by the #50 merge).
            "newinstancewithoutconstructor" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?;
                Ok(Some(self.instantiate(&cn.to_lowercase(), &[])?))
            }
            "isfinal" | "isabstract" | "isstatic" | "ispublic" | "isprotected" | "isprivate"
            | "isenumcase" | "isdeprecated" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let refl_kind = obj.borrow().class.name().to_lowercase();
                let b = match refl_kind.as_str() {
                    // ReflectionClassConstant: visibility/final/enum_case
                    // come off the ConstDecl (marc-mabe Enum::getConstants).
                    "reflectionclassconstant" => {
                        let cd = self.find_const_decl(&cn, &mn);
                        match cd {
                            Some(cd) => match lname.as_str() {
                                "isfinal" => cd.0.is_final,
                                "ispublic" => cd.0.visibility == crate::ast::Visibility::Public,
                                "isprotected" => {
                                    cd.0.visibility == crate::ast::Visibility::Protected
                                }
                                "isprivate" => cd.0.visibility == crate::ast::Visibility::Private,
                                "isenumcase" => cd.0.enum_case,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    // ReflectionProperty: visibility/static/readonly off
                    // the PropDecl.
                    "reflectionclass" | "reflectionobject" | "reflectionenum" => {
                        match self.classes.get(&cn.to_lowercase()) {
                            Some(c) => match lname.as_str() {
                                "isfinal" => c.decl.is_final,
                                "isabstract" => c.decl.is_abstract,
                                "isinterface" => c.decl.kind == crate::ast::ClassKind::Interface,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    "reflectionproperty" => {
                        let pd = self
                            .classes
                            .get(&cn.to_lowercase())
                            .cloned()
                            .and_then(|c| self.find_prop_decl(&c, &mn))
                            .map(|(pd, _)| pd);
                        match pd {
                            Some(pd) => match lname.as_str() {
                                "isstatic" => pd.is_static,
                                "ispublic" => pd.visibility == crate::ast::Visibility::Public,
                                "isprotected" => pd.visibility == crate::ast::Visibility::Protected,
                                "isprivate" => pd.visibility == crate::ast::Visibility::Private,
                                "isfinal" => pd.is_final,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    _ => {
                        let m = self
                            .classes
                            .get(&cn.to_lowercase())
                            .cloned()
                            .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m));
                        match m {
                            Some(m) => match lname.as_str() {
                                "isfinal" => m.is_final,
                                "isabstract" => m.is_abstract,
                                "isstatic" => m.is_static,
                                "ispublic" => m.visibility == crate::ast::Visibility::Public,
                                "isprotected" => m.visibility == crate::ast::Visibility::Protected,
                                _ => m.visibility == crate::ast::Visibility::Private,
                            },
                            None => false,
                        }
                    }
                };
                Ok(Some(Value::Bool(b)))
            }
            // ReflectionClass file location — user classes carry
            // decl.file; internal classes report false like Zend.
            "getfilename" | "isinternal" | "isuserdefined" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let file = if !mn.is_empty() {
                    self.classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                        .map(|m| m.decl.file.clone())
                        .unwrap_or_default()
                } else {
                    self.classes
                        .get(&cn.to_lowercase())
                        .map(|c| c.decl.file.clone())
                        .unwrap_or_default()
                };
                let internal = file.is_empty() || file == "builtin";
                Ok(Some(match lname.as_str() {
                    "getfilename" => {
                        if internal {
                            Value::Bool(false)
                        } else {
                            Value::str(file)
                        }
                    }
                    "isinternal" => Value::Bool(internal),
                    _ => Value::Bool(!internal),
                }))
            }
            // Class-level kind predicates (the member-flag arm above
            // only resolves Reflection{Method,Property,ClassConstant};
            // isAnonymous lives in the name-introspection arm which
            // covers both closure and class subjects).
            "isinterface" | "istrait" | "isenum" | "isinstantiable" | "iscloneable"
            | "isreadonly" | "isiterable" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let decl = self.classes.get(&cn.to_lowercase()).map(|c| c.decl.clone());
                let b = decl
                    .map(|d| match lname.as_str() {
                        "isinterface" => d.kind == crate::ast::ClassKind::Interface,
                        "istrait" => d.kind == crate::ast::ClassKind::Trait,
                        "isenum" => d.kind == crate::ast::ClassKind::Enum,
                        "isreadonly" => d.readonly,
                        "isiterable" => d.implements.iter().any(|i| {
                            i.eq_ignore_ascii_case("traversable")
                                || i.eq_ignore_ascii_case("iterator")
                                || i.eq_ignore_ascii_case("iteratoraggregate")
                        }),
                        _ => d.kind == crate::ast::ClassKind::Class && !d.is_abstract,
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(b)))
            }
            // isSubclassOf/implementsInterface — walk the parent chain /
            // transitive interface set. Arg: class-string or reflector.
            "issubclassof" | "implementsinterface" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let target = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let tname = match &target {
                    Value::Object(o) => o
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null),
                    v => v.clone(),
                };
                let tname = self.conv_str(&tname)?.to_string().to_lowercase();
                let mut hit = false;
                let mut cur = self.classes.get(&cn.to_lowercase()).cloned();
                let mut seen = std::collections::HashSet::new();
                while let Some(c) = cur {
                    if lname == "issubclassof" {
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                        if let Some(n) = cur.as_ref().map(|c| c.decl.name.to_lowercase()) {
                            if n == tname {
                                hit = true;
                            }
                        }
                    } else {
                        // transitive interface set incl. interface-extends
                        let mut q: Vec<String> = c.decl.implements.clone();
                        if c.decl.kind == crate::ast::ClassKind::Interface {
                            q.extend(c.decl.parent.iter().cloned());
                        }
                        for i in q {
                            if i.to_lowercase() == tname {
                                hit = true;
                            }
                            if let Some(ic) = self.classes.get(&i.to_lowercase()).cloned() {
                                for pp in ic.decl.parent.iter().chain(ic.decl.implements.iter()) {
                                    if pp.to_lowercase() == tname {
                                        hit = true;
                                    }
                                }
                            }
                        }
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    let k = cur.as_ref().map(|c| c.decl.name.to_lowercase());
                    if k.is_none() || !seen.insert(k.unwrap()) {
                        break;
                    }
                }
                Ok(Some(Value::Bool(hit)))
            }
            // getMethods(filter) — own + inherited methods as
            // ReflectionMethod objects; declaring class per PHP.
            "getmethods" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let filter = args.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                let mut arr = PhpArray::default();
                let mut seen = std::collections::HashSet::new();
                let mut cur = self.classes.get(&cn.to_lowercase()).cloned();
                while let Some(c) = cur {
                    for m in &c.decl.methods {
                        let key = m.decl.name.to_lowercase();
                        if !seen.insert(key) {
                            continue;
                        }
                        let ok = filter == 0
                            || (filter & 1 != 0 && m.visibility == crate::ast::Visibility::Public)
                            || (filter & 2 != 0
                                && m.visibility == crate::ast::Visibility::Protected)
                            || (filter & 4 != 0 && m.visibility == crate::ast::Visibility::Private)
                            || (filter & 16 != 0 && m.is_static)
                            || (filter & 32 != 0 && m.is_final)
                            || (filter & 64 != 0 && m.is_abstract);
                        if !ok {
                            continue;
                        }
                        let rm = self.instantiate("reflectionmethod", &[])?;
                        if let Value::Object(o) = &rm {
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rc\0class".into(),
                                cell(Value::str(c.decl.name.clone())),
                            );
                            ob.props
                                .insert("\0rc\0prop".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("name".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("class".into(), cell(Value::str(c.decl.name.clone())));
                        }
                        arr.push(rm);
                    }
                    cur = c
                        .decl
                        .parent
                        .as_ref()
                        .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getmethod" | "hasmethod" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let cls = self.classes.get(&cn.to_lowercase()).cloned();
                let found = cls.as_ref().and_then(|c| self.find_method_in(c, &mn));
                match (lname.as_str(), found) {
                    ("hasmethod", f) => Ok(Some(Value::Bool(f.is_some()))),
                    (_, Some((m, dcls))) => {
                        let rm = self.instantiate("reflectionmethod", &[])?;
                        if let Value::Object(o) = &rm {
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rc\0class".into(),
                                cell(Value::str(dcls.decl.name.clone())),
                            );
                            ob.props
                                .insert("\0rc\0prop".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("name".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("class".into(), cell(Value::str(dcls.decl.name.clone())));
                        }
                        Ok(Some(rm))
                    }
                    _ => self.fail(PhpError::uncaught(
                        "ReflectionException",
                        format!("Method {}::{}() does not exist", cn, mn),
                        0,
                    )),
                }
            }
            // ReflectionMethod identity/lines/return-type plumbing.
            "isconstructor" | "isdestructor" => {
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                Ok(Some(Value::Bool(mn.eq_ignore_ascii_case(
                    if lname == "isconstructor" {
                        "__construct"
                    } else {
                        "__destruct"
                    },
                ))))
            }
            "getstartline" | "getendline" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let ln = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                    .map(|m| {
                        if lname == "getstartline" {
                            m.decl.line
                        } else {
                            m.decl.end_line
                        }
                    })
                    .unwrap_or(0);
                Ok(Some(Value::Int(ln as i64)))
            }
            "getdeclaringnamespace" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let ns = cn
                    .rsplit_once('\\')
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_default();
                Ok(Some(Value::str(ns)))
            }
            "hasreturntype" | "getreturntype" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let tys = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                    .and_then(|m| m.decl.ret.clone());
                match (lname.as_str(), tys) {
                    ("hasreturntype", t) => Ok(Some(Value::Bool(t.is_some()))),
                    (_, Some(tys)) => {
                        let nt = self.instantiate("reflectionnamedtype", &[])?;
                        if let Value::Object(o) = &nt {
                            let mut ta = PhpArray::default();
                            for m in &tys {
                                ta.push(Value::str(m));
                            }
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rp\0ty".into(),
                                cell(Value::Array(Rc::new(RefCell::new(ta)))),
                            );
                            if let Some(first) = tys.first() {
                                ob.props.insert("name".into(), cell(Value::str(first)));
                            }
                        }
                        Ok(Some(nt))
                    }
                    _ => Ok(Some(Value::Null)),
                }
            }
            "getdoccomment" => Ok(Some(Value::Bool(false))),
            "gettraitaliases" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() {
                    for m in &c.decl.methods {
                        if let Some(orig) = &m.trait_alias_of {
                            let v =
                                format!("{}::{}", m.decl.decl_in.clone().unwrap_or_default(), orig);
                            arr.set(ArrKey::Str(m.decl.name.as_str().into()), Value::str(v));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            // ReflectionClass::getInterfaceNames() — declared-case
            // interface names; getInterfaces() returns the reflectors.
            "getinterfacenames" | "getinterfaces" => {
                let cv = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = match &cv {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    _ => self.conv_str(&cv)?.to_string(),
                };
                let l = self
                    .resolve_class(&cn)
                    .unwrap_or_else(|| cn.clone())
                    .to_lowercase();
                let decl = self
                    .classes
                    .get(&l)
                    .map(|c| c.decl.clone())
                    .or_else(|| self.interfaces.get(&l).cloned())
                    .or_else(|| self.traits.get(&l).cloned());
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for i in &d.implements {
                        if lname == "getinterfaces" {
                            let r = self.instantiate("reflectionclass", &[Value::str(i)])?;
                            arr.set(ArrKey::Str(i.as_str().into()), r);
                        } else {
                            arr.push(Value::str(i.clone()));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getconstant" | "getconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                if lname == "getconstants" {
                    let mut arr = PhpArray::default();
                    for (n, _cd) in self.all_const_decls(&cn) {
                        // Go through class_const_named: it binds the
                        // declaring class so `self::X` inside const decls
                        // (e.g. Enum::MAPPING) resolves correctly.
                        if let Ok(v) = self.class_const_named(&cn, &n) {
                            arr.set(ArrKey::Str(n.into()), v);
                        }
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.class_const_named(&cn, &pn) {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => Ok(Some(Value::Bool(false))),
                }
            }
            "getreflectionconstant" | "getreflectionconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mk = |it: &mut Self, cname: &str, n: &str| -> Result<Value, PhpError> {
                    let v = it.instantiate("reflectionclassconstant", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0class".into(), cell(Value::str(cname)));
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0prop".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("name".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("class".into(), cell(Value::str(cname)));
                    }
                    Ok(v)
                };
                if lname == "getreflectionconstants" {
                    let mut arr = PhpArray::default();
                    for (n, _) in self.all_const_decls(&cn) {
                        arr.push(mk(self, &cn, &n)?);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                if self.find_const_decl(&cn, &pn).is_none() {
                    return Ok(Some(Value::Bool(false)));
                }
                Ok(Some(mk(self, &cn, &pn)?))
            }
            "getvalue" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.find_const_decl(&cn, &pn) {
                    Some((cd, f)) => {
                        // Const decls can reference self::X — bind the
                        // declaring scope during eval (Enum::MAPPING).
                        let old = self
                            .classes
                            .get(&cn.to_lowercase())
                            .map(|c| self.const_self.replace(c.clone()));
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(&cd.value, &f);
                        self.class_const_ctx -= 1;
                        if let Some(o) = old {
                            self.const_self = o;
                        }
                        Ok(Some(r?))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "setvalue" => {
                // ReflectionProperty::setValue($object, $value) —
                // bypasses prop visibility without __set (bug72177).
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = args.first().map(|c| c.borrow().clone());
                let val = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Some(Value::Object(t)) = target {
                    let mut tb = t.borrow_mut();
                    if !tb.props.contains_key(&pn) && !tb.prop_order.iter().any(|k| k == &pn) {
                        tb.prop_order.push(pn.clone());
                    }
                    tb.props.insert(pn.clone(), cell(val));
                    tb.unset_props.remove(&pn);
                }
                Ok(Some(Value::Null))
            }
            "getdeclaringclass" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let v = self.instantiate("reflectionclass", &[])?;
                if let Value::Object(o) = &v {
                    o.borrow_mut()
                        .props
                        .insert("\0rc\0class".into(), cell(Value::str(cn.clone())));
                    o.borrow_mut()
                        .props
                        .insert("name".into(), cell(Value::str(cn)));
                }
                Ok(Some(v))
            }
            "isinitialized" => {
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?;
                let target = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(o) = target {
                    Ok(Some(Value::Bool(self.obj_prop_key(&o, &pn).is_some())))
                } else {
                    Ok(Some(Value::Bool(false)))
                }
            }
            _ => Ok(None),
        }
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
                if self.silence == 0 {
                    self.warn(&format!(
                        "Attempt to read property \"{}\" on {}",
                        pn,
                        other.gettype()
                    ))?;
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
        let pn = self.prop_name(name)?;
        let ov = self.eval(obj)?;
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
                let key = match key {
                    Some(k) if self.prop_visible(&o.borrow().class.clone(), &pn) => Some(k),
                    _ => None,
                };
                if key.is_none() {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, &pn) {
                        if tpd.ty.is_some() && tpd.default.is_none() {
                            let nullable = tpd
                                .ty
                                .as_ref()
                                .map(|t| t.iter().any(|m| m.eq_ignore_ascii_case("null")))
                                .unwrap_or(false);
                            if !nullable {
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
                                    self.ref_cells.insert(Rc::as_ptr(&got) as usize);
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
                            self.notice(&format!(
                                "Indirect modification of overloaded property {}::${} has no effect",
                                cls.name(),
                                pn
                            ))?;
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
                let key = key.unwrap_or_else(|| pn.clone());
                let mut ob = o.borrow_mut();
                if !ob.props.contains_key(&key) {
                    if !ob.prop_order.contains(&key) {
                        ob.prop_order.push(key.clone());
                    }
                    let nc = cell(Value::Null);
                    self.last_fresh_cell = Some(Rc::as_ptr(&nc) as usize);
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
            _ => self.fail(PhpError::fatal(
                format!("Attempt to assign property \"{}\" on non-object", pn),
                0,
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
            let ov = self.eval(obj)?;
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
                if let Some(k) = self.obj_prop_key(&o, &pn) {
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

    pub(in crate::interp) fn method_call(
        &mut self,
        obj: &Expr,
        name: &PropName,
        args: &[Expr],
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        let mn = Self::nul_trunc(&self.prop_name(name)?);
        let ov = self.eval(obj)?;
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let params = self
                    .find_method_in(&o.borrow().class.clone(), &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // method_invoke handles builtin (Throwable), __call, undefined.
                self.method_invoke_vis(o.clone(), &mn, argvals)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to a member function {}() on null", mn),
                    0,
                ))
            }
            // Closures expose Closure::__invoke/call/bindTo
            // (named_params/call_user_func's `$closure->__invoke(...)`).
            Value::Callable(c) if mn.eq_ignore_ascii_case("__invoke") => {
                let params = match &c.kind {
                    CallableKind::Closure(d) => d.params.clone(),
                    _ => vec![],
                };
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // `$f->__invoke()` runs the internal Closure::__invoke —
                // diagnostics name `Closure::__invoke` and drop the
                // ", called in" suffix (closure_059).
                self.pending_call_alias = Some("Closure::__invoke".into());
                let r = self.call_value(&Value::Callable(c), argvals);
                self.pending_call_alias = None;
                r
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("call") => {
                // `$fn->call($newThis, ...$args)`: bind with an omitted
                // scope then invoke — previous scope preserved when the
                // new instance is compatible (closure_036/038).
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let mut ca = argvals;
                let newthis = ca
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(t) = newthis {
                    // call() scopes to the new instance's class
                    // (unlike bindTo's default 'static' scope).
                    match self.rebind_closure(&c, Some(t.clone()), Some(Value::Object(t)))? {
                        Some(nc) => {
                            ca.cells.remove(0);
                            return self.call_value(&Value::Callable(Rc::new(nc)), ca);
                        }
                        // A failed bind (warned) skips the invocation
                        // (closure_from_callable_rebinding).
                        None => return Ok(Value::Null),
                    }
                }
                self.call_value(&Value::Callable(c), ca)
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("bindto") => {
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let this = argvals.cells.first().map(|c| c.borrow().clone());
                let scope = argvals.cells.get(1).map(|c| c.borrow().clone());
                let new_this = match &this {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(o)) => Some(o.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #1 ($newThis) must be of type ?object, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                let scope_arg = match &scope {
                    None => None,
                    Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => Some(v.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #2 ($newScope) must be of type object|string|null, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                match self.rebind_closure(&c, new_this, scope_arg)? {
                    Some(nc) => Ok(Value::Callable(Rc::new(nc))),
                    None => Ok(Value::Null),
                }
            }
            other => self.fail(PhpError::uncaught(
                "Error",
                format!("Call to a member function {}() on {}", mn, other.gettype()),
                0,
            )),
        }
    }

    /// `$obj->method()` dispatch to a resolved decl.
    /// `dc` is the declaring class — used as the private-prop scope.
    pub(in crate::interp) fn invoke_method(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        args: CallArgs,
        dc: Rc<PhpClass>,
    ) -> Result<Value, PhpError> {
        // Native SPL stubs (empty body, line 0) dispatch through
        // spl_method however the call resolved — direct, parent::,
        // or late-bound — so subclass PHP methods stay authoritative
        // while inherited engine behavior still fires.
        if m.decl.body.is_empty()
            && m.decl.line == 0
            && self.is_a_str(obj.borrow().class.name(), "splfileinfo")
        {
            if let Some(v) = self.spl_method(&obj, &m.decl.name, &args)? {
                return Ok(v);
            }
        }
        let called = obj.borrow().class.clone();
        self.pending_decl_class = Some(dc.clone());
        self.pending_called_class = Some(called);
        let r = if m.is_static {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(dc))
        } else {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(obj), Some(dc))
        };
        self.pending_decl_class = None;
        self.pending_called_class = None;
        r
    }

    /// Dispatch `$obj->name($args)` through `__call(name, args)`.
    /// Zend's $args array for __call/__callStatic: elements that were
    /// references in the caller's send array stay shared (bug50394);
    /// plain zvals are copied so var_dump shows no `&`
    /// (trampoline_closure_named_arguments).
    pub(in crate::interp) fn magic_args_array(&self, args: &CallArgs) -> PhpArray {
        let mut arr = PhpArray::new();
        let share = |a: &Cell| {
            if self.ref_cells.contains(&(Rc::as_ptr(a) as usize)) {
                a.clone()
            } else {
                cell(a.borrow().clone())
            }
        };
        for a in &args.cells {
            arr.push_cell(share(a));
        }
        for (n, a, ..) in &args.named {
            arr.set_cell(ArrKey::Str(Rc::from(n.as_str())), share(a));
        }
        arr
    }

    pub(in crate::interp) fn call_via_magic(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        dc: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let arr = self.magic_args_array(&args);
        self.invoke_method(
            obj,
            m,
            CallArgs::positional(vec![
                cell(Value::str(name)),
                cell(Value::Array(Rc::new(RefCell::new(arr)))),
            ]),
            dc,
        )
    }

    /// A private method owned by the calling scope binds statically:
    /// `$this->m()` inside `S::x` always resolves to `S::m`, bypassing
    /// the object's override (zend private methods are not virtual).
    pub(in crate::interp) fn scope_private_method(
        &mut self,
        name: &str,
    ) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone())?;
        let m = scope.decl.find_method(&name.to_lowercase())?;
        (m.visibility == crate::ast::Visibility::Private).then_some((m, scope))
    }

    /// Userland `Cls::name()` dispatch: gate visibility at the call
    /// site, routing inaccessible methods through __callStatic.
    pub(in crate::interp) fn static_invoke_vis(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        if let Some((m, sc)) = self.scope_private_method(name) {
            let this_obj = if m.is_static {
                None
            } else {
                self.stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    })
            };
            if !m.is_static && this_obj.is_none() {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Non-static method {}::{}() cannot be called statically",
                        sc.name(),
                        m.decl.name
                    ),
                    0,
                ));
            }
            self.pending_decl_class = Some(sc.clone());
            self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
            let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(sc));
            self.pending_decl_class = None;
            self.pending_called_class = None;
            return r;
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible found-method: same magic preference as
                // the missing path — __call first in object context,
                // else __callStatic (bug53826, bug48533).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                        return self.call_via_magic(o, &cm, cdc, name, args);
                    }
                }
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__callstatic") {
                    let arr = self.magic_args_array(&args);
                    self.pending_decl_class = Some(cdc.clone());
                    self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
                    let r = self.invoke_fn(
                        &Rc::new(cm.decl.clone()),
                        CallArgs::positional(vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ]),
                        None,
                        Some(cdc),
                    );
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    return r;
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.static_invoke(cls, name, args, called_class, fwd)
    }

    /// Userland `$obj->name()` dispatch: gate visibility at the call
    /// site. Internal invocations (FCC bound scope, engine magic calls)
    /// go through `method_invoke` unchecked.
    pub(in crate::interp) fn method_invoke_vis(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let cls = obj.borrow().class.clone();
        // Scope-private binding takes precedence over the object's own
        // method table (private methods are not virtual) — but only
        // when the callee is an instance of the scope class. For
        // unrelated objects the scope's same-named private method is
        // invisible and normal dispatch applies (`$io->output->m()`
        // must not find IO's private m()).
        if let Some((m, sc)) = self.scope_private_method(name) {
            let cname = cls.name().to_string();
            if self.is_a_str(&cname, sc.name()) {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", sc.name(), name),
                        0,
                    ));
                }
                return self.invoke_method(obj, &m, args, sc);
            }
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible method routes through __call when
                // defined (zend_std_get_method fallback).
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &cm, cdc, name, args);
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.method_invoke(obj, name, args)
    }

    /// Method-call visibility against the current calling scope.
    pub(in crate::interp) fn method_access_ok(
        &mut self,
        m: &MethodDecl,
        dc: &Rc<PhpClass>,
    ) -> bool {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        match m.visibility {
            crate::ast::Visibility::Public => true,
            crate::ast::Visibility::Private => scope
                .as_ref()
                .map(|s| s.name() == dc.name())
                .unwrap_or(false),
            crate::ast::Visibility::Protected => scope
                .as_ref()
                .map(|s| {
                    let proto = self.method_prototype(dc, &m.decl.name.to_lowercase());
                    self.is_a_str(s.name(), dc.name())
                        || self.is_a_str(dc.name(), s.name())
                        // Sibling access: protected `B::m()` callable from
                        // scope A when A is a descendant of the method's
                        // PROTOTYPE owner (the ancestor that first
                        // declared it — gh14009: A and B both extend P,
                        // P first declared `common`).
                        || self.is_a_str(s.name(), &proto)
                })
                .unwrap_or(false),
        }
    }

    /// The ancestor class that first declared `lname` (non-private) —
    /// the prototype owner for protected-member access rules.
    pub(in crate::interp) fn method_prototype(
        &mut self,
        cls: &Rc<PhpClass>,
        lname: &str,
    ) -> String {
        let mut owner = cls.name().to_string();
        let mut cur = cls.decl.parent.clone();
        while let Some(p) = cur {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if pc.decl.methods.iter().any(|m| {
                m.decl.name.to_lowercase() == lname
                    && !matches!(m.visibility, crate::ast::Visibility::Private)
            }) {
                owner = pc.decl.name.clone();
            }
            cur = pc.decl.parent.clone();
        }
        owner
    }

    /// `Call to private/protected method X::m() from scope Y` — catchable.
    pub(in crate::interp) fn method_vis_error(
        &mut self,
        m: &MethodDecl,
        dc: &Rc<PhpClass>,
    ) -> PhpError {
        let vis = match m.visibility {
            crate::ast::Visibility::Public => "public",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Private => "private",
        };
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        let from = match &scope {
            Some(s) => format!("scope {}", s.name()),
            None => "global scope".to_string(),
        };
        PhpError::uncaught(
            "Error",
            format!(
                "Call to {} method {}::{}() from {}",
                vis,
                dc.name(),
                m.decl.name,
                from
            ),
            0,
        )
    }

    /// Calls a method by name through an object cell (magic methods, __call).
    pub fn method_invoke(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        // Builtin exception methods implemented natively.
        if let Value::Object(_) = Value::Object(obj.clone()) {}
        let is_throwable = {
            let ob = obj.borrow();
            matches!(ob.internal, Some(ObjectInternal::Exception { .. }))
                || self.is_throwable_name(&ob.class.decl.name)
        };
        let cls = obj.borrow().class.clone();
        if is_throwable {
            // Native method only when the resolved method is a builtin
            // registration (line 0 — userland always runs, even an empty
            // body: `__construct(public $x) {}` still promotes).
            // find_method_in walks the parent chain so inherited stubs
            // (Exception::getTrace on a userland subclass) resolve
            // (tests/lang/038, error_2_exception_001).
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.throwable_method(&obj, name, &args) {
                    return Ok(v);
                }
            }
        }
        // Reflection stubs are native: constructor stores the target
        // name, methods act on it (gh15438_2).
        if cls.name().starts_with("Reflection") || cls.name().starts_with("reflection") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                // Native Reflection calls leave a `Cls->m()` frame in
                // uncaught traces (named_params/attributes_named_flags).
                self.call_trace.push(TraceFrame {
                    file: self.diag_file(),
                    line: self.cur_line as u32,
                    function: name.to_string(),
                    class: Some(cls.name().to_string()),
                    ty: "->".into(),
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: false,
                });
                let r = self.reflection_method(&obj, name, &args);
                self.call_trace.pop();
                if let Some(v) = r? {
                    return Ok(v);
                }
            }
        }
        // SplDoublyLinkedList / SplStack — list state on \0dll\0items.
        if matches!(
            cls.name().to_lowercase().as_str(),
            "spldoublylinkedlist" | "splstack" | "splqueue"
        ) {
            let dll_method = |o: &Rc<RefCell<PhpObject>>| -> Vec<Value> {
                match o.borrow().props.get("\0dll\0items") {
                    Some(c) => match &*c.borrow() {
                        Value::Array(a) => {
                            a.borrow().iter().map(|(_, c)| c.borrow().clone()).collect()
                        }
                        _ => Vec::new(),
                    },
                    None => Vec::new(),
                }
            };
            match name.to_lowercase().as_str() {
                "count" => return Ok(Value::Int(dll_method(&obj).len() as i64)),
                "isempty" => return Ok(Value::Bool(dll_method(&obj).is_empty())),
                "top" => return Ok(dll_method(&obj).first().cloned().unwrap_or(Value::Null)),
                "bottom" => return Ok(dll_method(&obj).last().cloned().unwrap_or(Value::Null)),
                "push" => {
                    let v = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mut items = dll_method(&obj);
                    items.push(v);
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(Value::Null);
                }
                "pop" | "shift" => {
                    let mut items = dll_method(&obj);
                    let r = if name.eq_ignore_ascii_case("pop") {
                        items.pop()
                    } else {
                        if items.is_empty() {
                            None
                        } else {
                            Some(items.remove(0))
                        }
                    };
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(r.unwrap_or(Value::Null));
                }
                _ => {}
            }
        }
        // DateTime: minimal native clock — the ctor stores the parsed
        // timestamp so getTimestamp/diff can read it back
        // (closure_call_internal).
        if matches!(
            cls.name().to_lowercase().as_str(),
            "datetime" | "datetimeimmutable"
        ) {
            match name.to_lowercase().as_str() {
                "__construct" => {
                    let ts = match args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null)
                    {
                        Value::Str(s) => {
                            let s = crate::value::lossy(&s);
                            match s.strip_prefix('@') {
                                Some(num) => num.trim().parse::<i64>().unwrap_or(0),
                                // Relative formats beyond '@N' are
                                // stubs — treat as epoch for now.
                                None => 0,
                            }
                        }
                        _ => 0,
                    };
                    obj.borrow_mut()
                        .props
                        .insert("\0dt\0ts".into(), cell(Value::Int(ts)));
                    return Ok(Value::Null);
                }
                "gettimestamp" => {
                    return Ok(obj
                        .borrow()
                        .props
                        .get("\0dt\0ts")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Int(0)));
                }
                _ => {}
            }
        }
        // ArrayIterator: native iteration state on the object internal.
        if cls.name().eq_ignore_ascii_case("arrayiterator") {
            if let Some(v) = self.array_iter_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // Generator: same pattern — native iteration state.
        if cls.name().eq_ignore_ascii_case("generator") {
            if let Some(v) = self.generator_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // SplFileInfo / DirectoryIterator family: SPL filesystem
        // objects — is_a covers FilesystemIterator,
        // RecursiveDirectoryIterator and userland subclasses. Only the
        // native stubs dispatch here — a userland override (e.g.
        // symfony/finder's current()) still wins.
        if self.is_a_str(cls.name(), "splfileinfo") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.spl_method(&obj, name, &args)? {
                    return Ok(v);
                }
            }
        }
        // PDO / PDOStatement: sqlite-backed storage surface (#15 spike).
        if cls.name().eq_ignore_ascii_case("pdo") {
            if let Some(v) = crate::pdo::pdo_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        if cls.name().eq_ignore_ascii_case("pdostatement") {
            if let Some(v) = crate::pdo::pdostmt_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                self.invoke_method(obj, &m, args, dc)
            }
            None => {
                if let Some((m, dc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &m, dc, name, args);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
    }

    /// Native implementations of Throwable methods.
    pub(in crate::interp) fn throwable_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        _args: &[Cell],
    ) -> Option<Value> {
        let ob = obj.borrow();
        let lname = name.to_lowercase();
        match lname.as_str() {
            "getmessage" => Some(
                ob.props
                    .get("message")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null),
            ),
            "getcode" => Some(
                ob.props
                    .get("code")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Int(0)),
            ),
            "getfile" => match &ob.internal {
                Some(ObjectInternal::Exception { file, .. }) => Some(Value::str(file.clone())),
                _ => Some(Value::str(self.diag_file())),
            },
            "getline" => match &ob.internal {
                Some(ObjectInternal::Exception { line, .. }) => Some(Value::Int(*line as i64)),
                _ => Some(Value::Int(self.cur_line as i64)),
            },
            "gettrace" => {
                let mut arr = PhpArray::new();
                let frames: Vec<TraceFrame> = match &ob.internal {
                    Some(ObjectInternal::Exception { frames, .. }) => (**frames).clone(),
                    _ => Vec::new(),
                };
                // PHP orders innermost call first (tests/lang/038);
                // internal-function call sites carry no file/line.
                for fr in frames.iter().rev() {
                    let mut f = PhpArray::new();
                    if fr.file != "[internal function]" {
                        f.set(ArrKey::Str("file".into()), Value::str(fr.file.clone()));
                        f.set(ArrKey::Str("line".into()), Value::Int(fr.line as i64));
                    }
                    f.set(
                        ArrKey::Str("function".into()),
                        Value::str(fr.function.clone()),
                    );
                    if let Some(c) = &fr.class {
                        f.set(ArrKey::Str("class".into()), Value::str(c.clone()));
                        f.set(ArrKey::Str("type".into()), Value::str(fr.ty.clone()));
                    }
                    let mut a = PhpArray::new();
                    for av in &fr.args {
                        a.push(av.borrow().clone());
                    }
                    for (n, av) in &fr.named_args {
                        a.set(ArrKey::Str(n.clone().into()), av.borrow().clone());
                    }
                    f.set(
                        ArrKey::Str("args".into()),
                        Value::Array(Rc::new(RefCell::new(a))),
                    );
                    arr.push(Value::Array(Rc::new(RefCell::new(f))));
                }
                Some(Value::Array(Rc::new(RefCell::new(arr))))
            }
            "gettraceasstring" => match &ob.internal {
                Some(ObjectInternal::Exception { trace, .. }) if !trace.is_empty() => {
                    Some(Value::str(trace.clone()))
                }
                Some(ObjectInternal::Exception { frames, .. }) if !frames.is_empty() => {
                    Some(Value::str(format_trace(frames)))
                }
                _ => Some(Value::str("#0 {main}")),
            },
            "getprevious" => Some(Value::Null),
            "__tostring" => {
                let msg = ob
                    .props
                    .get("message")
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let (file, line, trace, full) = match &ob.internal {
                    Some(ObjectInternal::Exception {
                        file,
                        line,
                        trace,
                        frames,
                        full_msg,
                        ..
                    }) => {
                        let t = if !trace.is_empty() {
                            trace.clone()
                        } else if !frames.is_empty() {
                            format_trace(frames)
                        } else {
                            "#0 {main}".into()
                        };
                        (file.clone(), *line, t, full_msg.clone())
                    }
                    _ => (
                        self.diag_file(),
                        self.cur_line as u32,
                        "#0 {main}".into(),
                        String::new(),
                    ),
                };
                let msg = if full.is_empty() { msg } else { full };
                Some(Value::str(format!(
                    "{}: {} in {}:{}\nStack trace:\n{}",
                    ob.class.name(),
                    msg,
                    file,
                    line,
                    trace
                )))
            }
            "__construct" => {
                // Builtin ctor: props from args message/code.
                drop(ob);
                let mut ob = obj.borrow_mut();
                let msg = _args
                    .first()
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let code = _args.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                ob.props.insert("message".into(), cell(Value::str(msg)));
                ob.props.insert("code".into(), cell(Value::Int(code)));
                if !ob.prop_order.contains(&"message".into()) {
                    ob.prop_order.push("message".into());
                    ob.prop_order.push("code".into());
                }
                Some(Value::Null)
            }
            _ => None,
        }
    }

    /// `X::` member access where X may be a trait: traits resolve to a
    /// synthesized class holding their statics; direct trait member
    /// access is deprecated (direct_static_member_access). Returns the
    /// resolved class plus the trait's display name when it is one.
    pub(in crate::interp) fn member_class_of(
        &mut self,
        class: &Expr,
    ) -> Result<(Rc<PhpClass>, Option<String>), PhpError> {
        let name = self.class_name_of(class)?;
        if let Some(td) = self.traits.get(&name.to_lowercase()).cloned() {
            let key = td.name.to_lowercase();
            let cls = self
                .trait_statics
                .entry(key)
                .or_insert_with(|| {
                    Rc::new(PhpClass {
                        decl: td.clone(),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    })
                })
                .clone();
            return Ok((cls, Some(td.name.clone())));
        }
        Ok((self.class_of(class)?, None))
    }

    pub(in crate::interp) fn static_prop_read(
        &mut self,
        class: &Expr,
        name: &PropName,
    ) -> Result<Value, PhpError> {
        let name = self.prop_name(name)?;
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let v = cls.statics.borrow().get(&name).map(|c| c.borrow().clone());
        match v {
            Some(v) => Ok(v),
            None => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &name) {
                    if pd.ty.is_some() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed static property {}::${} must not be accessed before initialization",
                                dcls.name(),
                                name
                            ),
                            0,
                        ));
                    }
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    pub(in crate::interp) fn static_prop_cell(
        &mut self,
        class: &Expr,
        name: &PropName,
    ) -> Result<Cell, PhpError> {
        let name = self.prop_name(name)?;
        self.static_prop_named(class, &name)
    }

    pub(in crate::interp) fn static_prop_named(
        &mut self,
        class: &Expr,
        name: &str,
    ) -> Result<Cell, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let found = cls.statics.borrow().get(name).cloned();
        match found {
            Some(c) => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    if let Some(tys) = &pd.ty {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                }
                Ok(c)
            }
            None => {
                // Write/cell path materializes a declared static; an
                // undeclared one is an Error.
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    let c = cell(Value::Null);
                    cls.statics.borrow_mut().insert(name.to_string(), c.clone());
                    if let Some(tys) = &pd.ty {
                        self.last_fresh_cell = Some(Rc::as_ptr(&c) as usize);
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                    return Ok(c);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    /// Lazily initialize static prop defaults.
    pub(in crate::interp) fn statics_init(&mut self, cls: &Rc<PhpClass>) {
        if *cls.statics_init.borrow() {
            return;
        }
        *cls.statics_init.borrow_mut() = true;
        // Inherited statics: PHP snapshots the parent's static-prop
        // values into the child's table at link time, so `static::$p`
        // on the child resolves parent defaults.
        if let Some(pname) = &cls.decl.parent {
            if let Some(p) = self.classes.get(&pname.to_lowercase()).cloned() {
                self.statics_init(&p);
                for (k, v) in p.statics.borrow().iter() {
                    cls.statics
                        .borrow_mut()
                        .entry(k.clone())
                        .or_insert_with(|| cell(v.borrow().clone()));
                }
            }
        }
        for p in &cls.decl.props {
            if !p.is_static {
                continue;
            }
            let mut default = match &p.default {
                Some(d) => {
                    let old = self.const_self.replace(cls.clone());
                    self.class_const_ctx += 1;
                    let v = self
                        .eval_decl_const(d, &cls.decl.file)
                        .unwrap_or(Value::Null);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    v
                }
                None => Value::Null,
            };
            if p.ty.is_some() {
                if let Ok(d) = self.prop_typed_write_check(p, cls, default.clone()) {
                    default = d;
                }
            }
            // A typed static without a default stays *uninitialized*:
            // reads raise the uninit Error until first assignment.
            if p.ty.is_some() && p.default.is_none() {
                continue;
            }
            cls.statics
                .borrow_mut()
                .insert(p.name.clone(), cell(default));
        }
    }

    pub(in crate::interp) fn static_call(
        &mut self,
        class: &Expr,
        name: &str,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Calling static trait method {}::{} is deprecated, it should only be called on a class using the trait",
                t, name
            ))?;
        }
        let params = self
            .find_method_in(&cls, name)
            .map(|m| m.0.decl.params.clone())
            .unwrap_or_default();
        let argvals = self.arg_cells(args, &params, &format!("{}()", name), false)?;
        // Only a syntactic class ref (self/parent/static/Foo) is a
        // forwarding call; `$x::m()` is not (bug48533).
        let fwd = matches!(class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
        // Forwarding calls (self::/parent::/static::) preserve the
        // current late-static-binding class instead of resetting it to
        // the resolved target: `parent::__construct()` on a subclass
        // still sees the subclass via `static::` inside the parent ctor.
        let called = match class {
            Expr::Const(n) | Expr::Str(n)
                if matches!(
                    n.trim_start_matches('\\').to_lowercase().as_str(),
                    "self" | "parent" | "static"
                ) =>
            {
                self.stack.last().and_then(|f| {
                    f.called_class
                        .clone()
                        .or_else(|| f.this_obj.as_ref().map(|o| o.borrow().class.clone()))
                })
            }
            _ => None,
        };
        self.static_invoke_vis(cls, name, argvals, called, fwd)
    }

    pub(in crate::interp) fn static_invoke(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        // Closure::{bind,fromCallable}: native callable rebinding.
        if cls.name().eq_ignore_ascii_case("closure") {
            let lname = name.to_lowercase();
            match lname.as_str() {
                "getcurrent" => {
                    // Current frame must itself be executing a closure
                    // body (closure_get_current).
                    return match self.stack.last().and_then(|f| f.closure_rc.clone()) {
                        Some(rc) => Ok(Value::Callable(rc)),
                        None => self.fail(PhpError::uncaught(
                            "Error",
                            "Current function is not a closure",
                            0,
                        )),
                    };
                }
                "bind" | "bindto" => {
                    // `Closure::bind($closure, $newThis, $newScope = ?)`.
                    let c = args.cells.first().map(|c| c.borrow().clone());
                    let this = args.cells.get(1).map(|c| c.borrow().clone());
                    let scope = args.cells.get(2).map(|c| c.borrow().clone());
                    let Value::Callable(cb) = c.unwrap_or(Value::Null) else {
                        return Ok(Value::Null);
                    };
                    let new_this = match &this {
                        None | Some(Value::Null) => None,
                        Some(Value::Object(o)) => Some(o.clone()),
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #2 ($newThis) must be of type ?object, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    let scope_arg = match &scope {
                        None => None,
                        Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => {
                            Some(v.clone())
                        }
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #3 ($newScope) must be of type object|string|null, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    match self.rebind_closure(&cb, new_this, scope_arg)? {
                        Some(nc) => return Ok(Value::Callable(Rc::new(nc))),
                        None => return Ok(Value::Null),
                    }
                }
                "fromcallable" => {
                    let v = args
                        .cells
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    match self.callable_to_closure(&v) {
                        Ok(c) => return Ok(c),
                        Err(fail) => {
                            // Zend appends the reason:
                            // "Failed to create closure from callable:
                            // non-static method A::m() cannot be called
                            // statically" (from_callable_non_static).
                            let reason = if !fail.message.is_empty() {
                                fail.message
                                    .replacen("Non-static method", "non-static method", 1)
                            } else {
                                self.pending_exception
                                    .as_ref()
                                    .and_then(|e| match e {
                                        Value::Object(o) => o
                                            .borrow()
                                            .props
                                            .get("message")
                                            .map(|c| c.borrow().to_php_string()),
                                        _ => None,
                                    })
                                    .unwrap_or_default()
                            };
                            let e = self.exception(
                                "TypeError",
                                &format!("Failed to create closure from callable: {}", reason),
                            );
                            self.pending_exception = Some(e);
                            return Err(PhpError {
                                trace: None,
                                thrown_line: None,
                                display_msg: None,
                                kind: ErrorKind::Throw,
                                message: "fromCallable".into(),
                                line: 0,
                            });
                        }
                    }
                }
                _ => {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method Closure::{}()", name),
                        0,
                    ));
                }
            }
        }
        // Throwable methods are instance-only; look up incl. parents.
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                // Forwarding call: a non-static method invoked
                // statically still receives $this when the caller's
                // $this is an instance of the callee's class
                // (bug21961) — but only via a syntactic class ref;
                // `$obj::m()` is not a forwarding call (bug48533).
                let this_obj = if m.is_static || !fwd {
                    None
                } else {
                    self.stack
                        .last()
                        .and_then(|f| f.this_obj.clone())
                        .filter(|o| {
                            let cname = o.borrow().class.name().to_string();
                            self.is_a_str(&cname, cls.name())
                        })
                };
                // `parent::__construct()` on a throwable resolves to a
                // builtin stub — run the native impl (message/code
                // props, exception internals) exactly like the object
                // dispatch in method_invoke (PHPUnit's Exception chain
                // ctor-chains here).
                if m.decl.body.is_empty() && m.decl.line == 0 {
                    if let Some(o) = &this_obj {
                        let is_throwable = {
                            let ob = o.borrow();
                            matches!(ob.internal, Some(ObjectInternal::Exception { .. }))
                                || self.is_throwable_name(&ob.class.decl.name)
                        };
                        if is_throwable {
                            if let Some(v) = self.throwable_method(o, name, &args.cells) {
                                return Ok(v);
                            }
                        }
                    }
                }
                if !m.is_static && this_obj.is_none() {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Non-static method {}::{}() cannot be called statically",
                            dc.name(),
                            m.decl.name
                        ),
                        0,
                    ));
                }
                self.pending_decl_class = Some(dc.clone());
                self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(dc));
                self.pending_decl_class = None;
                self.pending_called_class = None;
                r
            }
            None => {
                // A missing __construct never reaches magic —
                // `Foo::__construct()` is "Cannot call constructor"
                // (call_static_006). __destruct etc. dispatch normally.
                if name.eq_ignore_ascii_case("__construct") {
                    return self.fail(PhpError::uncaught("Error", "Cannot call constructor", 0));
                }
                let mut arr = PhpArray::new();
                for a in &args.cells {
                    arr.push(a.borrow().clone());
                }
                for (n, a, ..) in &args.named {
                    arr.set(ArrKey::Str(Rc::from(n.as_str())), a.borrow().clone());
                }
                let magic_args = CallArgs::positional(vec![
                    cell(Value::str(name)),
                    cell(Value::Array(Rc::new(RefCell::new(arr)))),
                ]);
                // Object context prefers __call over __callStatic when
                // the caller's $this is an instance of the callee —
                // `self::x()` inside a method acts as an instance call
                // (call_static_003/007, bug45186).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if self.find_method_in(&cls, "__call").is_some() {
                        return self.method_invoke(o, "__call", magic_args);
                    }
                }
                if let Some((m, dc)) = self.find_method_in(&cls, "__callstatic") {
                    self.pending_decl_class = Some(dc.clone());
                    self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                    let r = self.invoke_fn(&Rc::new(m.decl.clone()), magic_args, None, Some(dc));
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    return r;
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
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
        Some(PhpError::uncaught(
            "Error",
            format!("Cannot access {} property {}::${}", word, dn, pn),
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

    /// Declared class/interface/trait names for get_declared_*().
    pub fn declared_names(&self, kind: crate::ast::ClassKind) -> Vec<String> {
        let mut out = Vec::new();
        for n in &self.decl_order {
            let name = match kind {
                crate::ast::ClassKind::Trait => self.traits.get(n).map(|d| d.name.clone()),
                crate::ast::ClassKind::Interface => self.interfaces.get(n).map(|d| d.name.clone()),
                _ => self
                    .classes
                    .get(n)
                    .filter(|c| c.decl.kind == kind)
                    .map(|c| c.name().to_string()),
            };
            if let Some(nm) = name {
                out.push(nm);
            }
        }
        for (k, a) in &self.decl_aliases {
            if *k == kind {
                out.push(a.clone());
            }
        }
        out
    }

    /// class_alias($name, $alias): alias entries resolve like the
    /// original (classes, interfaces and traits alike).
    pub fn class_alias(&mut self, name: &str, alias: &str) -> Result<bool, PhpError> {
        // `class_alias($cls, 'int')` — the alias may not be a reserved
        // scalar type name (scalar_reserved*_class_alias).
        let short = alias
            .trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(alias)
            .to_lowercase();
        const RESERVED_ALS: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        if RESERVED_ALS.contains(&short.as_str()) {
            return Err(PhpError::fatal(
                format!(
                    "Cannot use \"{}\" as a class alias as it is reserved",
                    short
                ),
                self.cur_line,
            ));
        }
        let alias_l = alias.trim_start_matches('\\').to_lowercase();
        let key = name.trim_start_matches('\\').to_lowercase();
        if let Some(c) = self.classes.get(&key).cloned() {
            self.classes.insert(alias_l.clone(), c);
            self.decl_aliases
                .push((crate::ast::ClassKind::Class, alias_l));
            return Ok(true);
        }
        if let Some(i) = self.interfaces.get(&key).cloned() {
            self.interfaces.insert(alias_l.clone(), i);
            self.decl_aliases
                .push((crate::ast::ClassKind::Interface, alias_l));
            return Ok(true);
        }
        if let Some(t) = self.traits.get(&key).cloned() {
            self.traits.insert(alias_l.clone(), t);
            self.decl_aliases
                .push((crate::ast::ClassKind::Trait, alias_l));
            return Ok(true);
        }
        self.warn(&format!("Class \"{}\" not found", name))?;
        Ok(false)
    }

    /// Does this object's class implement `iname` (transitively)?
    /// Used by serialize() for the Serializable C:-format branch.
    pub fn obj_implements(&mut self, o: &Rc<RefCell<PhpObject>>, iname: &str) -> bool {
        self.is_a(&o.borrow().class, iname)
    }

    /// Does `d` (a class decl) implement interface `iname`, directly or
    /// through the implements chain of interfaces it names?
    pub(in crate::interp) fn implements_iface(&self, d: &ClassDecl, iname: &str) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<String> = d.implements.clone();
        while let Some(i) = stack.pop() {
            let l = i.trim_start_matches('\\').to_lowercase();
            if l == iname {
                return true;
            }
            if !seen.insert(l.clone()) {
                continue;
            }
            if let Some(id) = self.interfaces.get(&l) {
                stack.extend(id.implements.iter().cloned());
            }
        }
        false
    }

    /// Locate a class/interface/trait const decl by name — walks the
    /// class chain, used traits' merged consts and implemented
    /// interfaces (reflection APIs see trait consts on both sides).
    pub(in crate::interp) fn find_const_decl(
        &mut self,
        cname: &str,
        name: &str,
    ) -> Option<(crate::ast::ConstDecl, String)> {
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key) {
            for cd in &td.consts {
                if cd.name == name {
                    return Some((cd.clone(), td.file.clone()));
                }
            }
            return None;
        }
        if let Some(id) = self.interfaces.get(&key) {
            for cd in &id.consts {
                if cd.name == name {
                    return Some((cd.clone(), id.file.clone()));
                }
            }
            return None;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    return Some((cd.clone(), c.decl.file.clone()));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()) {
                for cd in &id.consts {
                    if cd.name == name {
                        return Some((cd.clone(), id.file.clone()));
                    }
                }
            }
        }
        None
    }

    /// All (name, (decl, file)) consts visible on `cname` — class chain
    /// + interfaces; trait members appear via the class's merged decl.
    pub(in crate::interp) fn all_const_decls(
        &mut self,
        cname: &str,
    ) -> Vec<(String, (crate::ast::ConstDecl, String))> {
        let mut out: Vec<(String, (crate::ast::ConstDecl, String))> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key).cloned() {
            for cd in &td.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), td.file.clone())));
                }
            }
            return out;
        }
        if let Some(id) = self.interfaces.get(&key).cloned() {
            for cd in &id.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                }
            }
            return out;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), c.decl.file.clone())));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &id.consts {
                    if seen.insert(cd.name.clone()) {
                        out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                    }
                }
            }
        }
        out
    }

    /// Zend backtrace text for debug_print_backtrace(): innermost-first
    /// frames, no `{main}` line (bug28213).
    pub fn format_backtrace(&self) -> String {
        let frames: Vec<TraceFrame> = self
            .call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect();
        format_backtrace_frames(&frames)
    }

    /// debug_backtrace() array — same frames as format_backtrace().
    pub fn backtrace(&self) -> Vec<TraceFrame> {
        self.call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect()
    }

    /// is-a check between two class-name strings.
    pub(in crate::interp) fn is_a_str(&mut self, a: &str, b: &str) -> bool {
        // Type-member checks during signature verification autoload the
        // compared classes — Zend verifies covariance with the real
        // hierarchy, so `C::m(): D` inside an autoloaded class sees `D`
        // even when it is declared later (abstract_method_9). A name
        // mid-link counts as resolvable without loading
        // (infinite_recursion — `class C extends Z implements C`).
        if !self.classes.contains_key(&a.to_lowercase())
            && !self.linking.iter().any(|c| c.name.eq_ignore_ascii_case(a))
        {
            // The check creates a delayed variance dependency either
            // way; a name mid-registration is still unlinked — the
            // decl answers it by name, but the obligation is recorded
            // so the check re-verifies after it links
            // (variance/loading_exception*).
            self.note_variance_obligation();
            if !self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(a))
            {
                if let Err(e) = self.run_autoload(a) {
                    // An autoload failure is a compile-time fatal everywhere
                    // else; inside a signature check a missing class just
                    // means "not a subtype" — but the fatal itself must
                    // still reach the checking context (error3 cascade).
                    self.sig_fatal.get_or_insert(e);
                    self.pending_exception = None;
                }
            }
        }
        // Aliases canonicalize through the class table: `Bar` (an
        // alias of Foo) compares as `Foo` (typed_properties_084).
        let canon_b = self
            .classes
            .get(&b.trim_start_matches('\\').to_lowercase())
            .map(|c| c.name().to_string());
        let b = canon_b.as_deref().unwrap_or(b);
        match self.classes.get(&a.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, b),
            None => self.is_a_unresolved(a, b, 0),
        }
    }

    /// Ancestry check by NAME for a class not (yet) in `self.classes`:
    /// walks parent/implements names through `classes` and `linking`.
    pub(in crate::interp) fn is_a_unresolved(&mut self, a: &str, b: &str, depth: u8) -> bool {
        if a.trim_start_matches('\\').eq_ignore_ascii_case(b) {
            return true;
        }
        if depth > 16 {
            return false;
        }
        let d = self
            .classes
            .get(&a.to_lowercase())
            .map(|c| c.decl.clone())
            .or_else(|| {
                self.linking
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.declaring
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.interfaces
                    .get(&a.to_lowercase())
                    .cloned()
                    .or_else(|| self.traits.get(&a.to_lowercase()).cloned())
            });
        let Some(d) = d else {
            return false;
        };
        if let Some(p) = &d.parent {
            if self.is_a_unresolved(p, b, depth + 1) {
                return true;
            }
        }
        for i in &d.implements {
            if self.is_a_unresolved(i, b, depth + 1) {
                return true;
            }
        }
        false
    }

    pub fn find_method_in(
        &mut self,
        cls: &Rc<PhpClass>,
        name: &str,
    ) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let lname = name.to_lowercase();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(m) = c.decl.find_method(&lname) {
                return Some((m, c));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    pub(in crate::interp) fn class_of(&mut self, e: &Expr) -> Result<Rc<PhpClass>, PhpError> {
        let name = self.class_name_of(e)?;
        if !self.classes.contains_key(&name.to_lowercase()) {
            self.run_autoload(&name)?;
        }
        match self.classes.get(&name.to_lowercase()) {
            Some(c) => Ok(c.clone()),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Class \"{}\" not found", name),
                0,
            )),
        }
    }

    pub(in crate::interp) fn class_const(
        &mut self,
        class: &Expr,
        name: &str,
    ) -> Result<Value, PhpError> {
        let cname = self.class_name_of(class)?;
        self.class_const_named(&cname, name)
    }

    /// is_callable(['Cls'|$obj, 'm']): 'parent'/'self'/'static' names
    /// resolve against the caller's scope; "parent"/"self" string
    /// callables are deprecated once they resolve (bug76773-deprecated).
    pub fn is_callable_arr(&mut self, first: &Value, mname: &str) -> bool {
        let cls = match first {
            Value::Callable(_) => return mname.eq_ignore_ascii_case("__invoke"),
            Value::Object(o) => Some(o.borrow().class.clone()),
            Value::Str(n) => {
                let n = crate::value::lossy(n);
                let ln = n.trim_start_matches('\\').to_lowercase();
                match ln.as_str() {
                    "parent" | "self" | "static" => {
                        let Some(scope) = self.caller_scope_name() else {
                            return false;
                        };
                        let Some(sc) = self.classes.get(&scope.to_lowercase()).cloned() else {
                            return false;
                        };
                        let target = match ln.as_str() {
                            "parent" => sc
                                .decl
                                .parent
                                .as_ref()
                                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned()),
                            "static" => self
                                .stack
                                .last()
                                .and_then(|f| f.called_class.clone())
                                .or(Some(sc)),
                            _ => Some(sc),
                        };
                        if let Some(c) = &target {
                            if self.find_method_in(c, mname).is_some() {
                                self.deprecated(&format!(
                                    "Use of \"{}\" in callables is deprecated",
                                    ln
                                ))
                                .ok();
                                return true;
                            }
                        }
                        return false;
                    }
                    _ => {
                        if !self.classes.contains_key(&ln) {
                            let _ = self.run_autoload(&n);
                            self.pending_exception = None;
                        }
                        self.classes.get(&ln).cloned()
                    }
                }
            }
            _ => None,
        };
        match cls {
            Some(c) => self.find_method_in(&c, mname).is_some(),
            None => false,
        }
    }

    /// `Cls::CONST` lookup by plain class-name string (shared with the
    /// constant() builtin so `constant('T::X')` honours the trait-const
    /// rule — constant_018).
    pub fn class_const_named(&mut self, cname: &str, name: &str) -> Result<Value, PhpError> {
        let cname = cname.to_string();
        if name == "class" {
            return Ok(Value::str(cname));
        }
        // `X::CONST` on an unloaded class runs the autoloaders (real
        // psr-4 code hits this constantly — e.g. `Language::ENGLISH`).
        let resolved = self.resolve_class(&cname).unwrap_or_else(|| cname.clone());
        let ckey = resolved.to_lowercase();
        if !self.classes.contains_key(&ckey)
            && !self.traits.contains_key(&ckey)
            && !self.interfaces.contains_key(&ckey)
        {
            // An autoloader's throwable propagates through the `::`
            // lookup (PHP fatals the same way); on a clean miss it
            // leaves no exception behind.
            self.run_autoload(&resolved)?;
        }
        if let Some(td) = self.traits.get(&ckey).cloned() {
            return self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Cannot access trait constant {}::{} directly",
                    td.name, name
                ),
                0,
            ));
        }
        if let Some(iface) = self.interfaces.get(&ckey).cloned() {
            // Const on an interface (e.g. `FastRoute\Dispatcher::FOUND`):
            // walk it and its extended interfaces.
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![iface];
            while let Some(c) = stack.pop() {
                if !seen.insert(c.name.to_lowercase()) {
                    continue;
                }
                for cd in &c.consts {
                    if cd.name == name {
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(&cd.value, &c.file);
                        self.class_const_ctx -= 1;
                        return match r {
                            Ok(v) => self.const_apply_ty(cd, &c.name, v),
                            Err(e) => Err(e),
                        };
                    }
                }
                for i in &c.implements {
                    if let Some(f) = self.interfaces.get(&i.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
                if let Some(p) = &c.parent {
                    if let Some(f) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
            }
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined constant {}", name),
                0,
            ));
        }
        let cls = match self.classes.get(&ckey) {
            Some(c) => c.clone(),
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", cname),
                    0,
                ))
            }
        };
        // Walk chain for the const (class first, then implemented
        // interfaces transitively — interface consts are inherited).
        let mut cur = Some(cls.clone());
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    if cd.enum_case {
                        return self.enum_case_value(&c.decl.name, &cd.name, cd);
                    }
                    let old = self.const_self.replace(c.clone());
                    self.class_const_ctx += 1;
                    let r = self.eval_decl_const(&cd.value, &c.decl.file);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    return match r {
                        Ok(v) => self.const_apply_ty(cd, &c.decl.name, v),
                        Err(e) => Err(e),
                    };
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let mut seen = std::collections::HashSet::new();
        let mut queue: Vec<String> = ifaces;
        while let Some(iname) = queue.pop() {
            if !seen.insert(iname.to_lowercase()) {
                continue;
            }
            if let Some(c) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &c.consts {
                    if cd.name == name {
                        let old = self.const_self.replace(cls.clone());
                        self.class_const_ctx += 1;
                        let r = self
                            .eval_decl_const(&cd.value, &c.file)
                            .and_then(|v| self.const_apply_ty(cd, &c.name, v));
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        return r;
                    }
                }
                queue.extend(c.implements.iter().cloned());
                if let Some(p) = &c.parent {
                    queue.push(p.clone());
                }
            }
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Undefined constant {}", name),
            0,
        ))
    }

    /// `defined('Cls::CONST')` — declaration check only (no value
    /// eval), mirroring class_const_named's walk. The class is
    /// resolved (and autoloaded) first.
    pub fn class_const_defined(&mut self, cname: &str, name: &str) -> bool {
        if name == "class" {
            return self.resolve_class(cname).is_some()
                || self
                    .classes
                    .contains_key(&cname.trim_start_matches('\\').to_lowercase());
        }
        let ckey = self
            .resolve_class(cname)
            .unwrap_or_else(|| cname.to_string())
            .to_lowercase();
        if let Some(iface) = self.interfaces.get(&ckey).cloned() {
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![iface];
            while let Some(c) = stack.pop() {
                if !seen.insert(c.name.to_lowercase()) {
                    continue;
                }
                if c.consts.iter().any(|cd| cd.name == name) {
                    return true;
                }
                for i in &c.implements {
                    if let Some(f) = self.interfaces.get(&i.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
                if let Some(p) = &c.parent {
                    if let Some(f) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
            }
            return false;
        }
        let Some(cls) = self.classes.get(&ckey).cloned() else {
            return false;
        };
        let mut cur = Some(cls);
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            if c.decl.consts.iter().any(|cd| cd.name == name) {
                return true;
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let mut seen = std::collections::HashSet::new();
        while let Some(iname) = ifaces.pop() {
            if !seen.insert(iname.to_lowercase()) {
                continue;
            }
            if let Some(c) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                if c.consts.iter().any(|cd| cd.name == name) {
                    return true;
                }
                ifaces.extend(c.implements.iter().cloned());
                if let Some(p) = &c.parent {
                    ifaces.push(p.clone());
                }
            }
        }
        false
    }
}
