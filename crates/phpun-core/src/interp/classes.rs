//! Class linking: `register_class` and its decl-time checks
//! (variance, traits, abstract/final, const types, enums), name
//! resolution, object handles and `new` instantiation.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- classes -----

    /// Class-ish name already registered — (prev-kind word, file,
    /// line) for 'Cannot redeclare' diagnostics. Zend's message names
    /// the previously declared kind — an enum entry names 'enum'
    /// (ev_enum_dup: `enum W {} enum W {}` → 'Cannot redeclare enum W').
    pub(in crate::interp) fn existing_class_site(
        &self,
        key: &str,
    ) -> Option<(&'static str, String, usize)> {
        if let Some(c) = self.classes.get(key) {
            let kind = if c.decl.kind == ClassKind::Enum {
                "enum"
            } else {
                "class"
            };
            return Some((kind, c.decl.file.clone(), c.decl.line));
        }
        if let Some(c) = self.interfaces.get(key) {
            return Some(("interface", c.file.clone(), c.line));
        }
        if let Some(c) = self.traits.get(key) {
            return Some(("trait", c.file.clone(), c.line));
        }
        None
    }

    /// 'Cannot redeclare K N' — internal classes carry no
    /// (previously declared in FILE:LINE) suffix.
    pub(in crate::interp) fn redeclare_class_msg(
        kind: &str,
        name: &str,
        file: &str,
        line: usize,
    ) -> String {
        if file.is_empty() {
            format!("Cannot redeclare {} {}", kind, name)
        } else {
            format!(
                "Cannot redeclare {} {} (previously declared in {}:{})",
                kind, name, file, line
            )
        }
    }

    pub(in crate::interp) fn register_class(
        &mut self,
        decl: Rc<ClassDecl>,
    ) -> Result<(), PhpError> {
        // The name is "in progress" from the moment registration is
        // entered: type probes treat it as resolvable so a check can
        // defer on it instead of autoloading recursively
        // (infinite_recursion: `class C extends Z implements C`).
        self.declaring.push(decl.clone());
        let res = self.register_class_inner(decl);
        self.declaring.pop();
        match res {
            // Class-linking errors are compile-class fatals. A
            // registration from early binding (hoist) reports the
            // compile context's trace — the innermost include/eval
            // pseudo-frame dropped — while an exec-phase registration
            // (conditional decl, a class with interfaces/traits, a
            // redeclare) keeps the live call chain.
            Err(e) if self.in_hoist && matches!(e.kind, crate::error::ErrorKind::Fatal) => {
                Err(PhpError {
                    trace: Some(self.compile_err_frames()),
                    ..e
                })
            }
            Err(e) => Err(self.decl_fatal_ctx(e)),
            r => r,
        }
    }

    fn register_class_inner(&mut self, decl: Rc<ClassDecl>) -> Result<(), PhpError> {
        // Reserved scalar names can't name a class/interface/trait/enum
        // (scalar_reserved*): `class int {}` is a compile fatal.
        let short = decl.name.rsplit('\\').next().unwrap_or(&decl.name);
        const RESERVED_DECL: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        if RESERVED_DECL.contains(&short.to_lowercase().as_str()) {
            let kind = match decl.kind {
                ClassKind::Interface => "an interface",
                ClassKind::Trait => "a trait",
                ClassKind::Enum => "an enum",
                ClassKind::Class => "a class",
            };
            return Err(PhpError::compile_fatal(
                format!(
                    "Cannot use \"{}\" as {} name as it is reserved",
                    short, kind
                ),
                self.cur_line,
            ));
        }
        // PHP links a declared class eagerly: the parent class, every
        // implemented interface, and every used trait must resolve at
        // declaration time, autoloading them when unregistered
        // (composer PSR-4 trees depend on this — MarkBased links
        // RegexBasedAbstract and the DataGenerator interface here).
        if let Some(p) = &decl.parent {
            let pl = p.to_lowercase();
            // Kind-mismatched parents (a trait/interface under
            // `extends`) count as "found" so the dedicated fatals
            // below report them (error_009). Names still on the
            // linking stack are mid-registration CEs — resolvable
            // (traits/abstract_method_9).
            let mut found = if decl.kind == ClassKind::Interface {
                self.interfaces.contains_key(&pl)
            } else {
                self.classes.contains_key(&pl)
                    || self.traits.contains_key(&pl)
                    || self.interfaces.contains_key(&pl)
                    || self
                        .linking
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&pl))
            };
            if !found {
                self.run_autoload(p.trim_start_matches('\\'))?;
                found = if decl.kind == ClassKind::Interface {
                    self.interfaces.contains_key(&pl)
                } else {
                    self.classes.contains_key(&pl)
                        || self.traits.contains_key(&pl)
                        || self.interfaces.contains_key(&pl)
                        || self
                            .linking
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&pl))
                };
            }
            // Still unlinked after autoload — catchable Error
            // (variance/unlinked_parent_1).
            if !found {
                let v = self.exception("Error", &format!("Class \"{}\" not found", p));
                return Err(self.throw(v));
            }
        }
        for i in &decl.implements {
            if !self.interfaces.contains_key(&i.to_lowercase())
                && !self.classes.contains_key(&i.to_lowercase())
            {
                self.run_autoload(i.trim_start_matches('\\'))?;
            }
        }
        for t in &decl.traits {
            if !self.traits.contains_key(&t.to_lowercase()) {
                self.run_autoload(t.trim_start_matches('\\'))?;
            }
        }
        let mut d = (*decl).clone();
        // Declaring file — prop/const default exprs bind __FILE__/__DIR__
        // to it (composer's generated `__DIR__ . '/../..' . ...` paths).
        // A class decl inside a function attributes to the FUNCTION's
        // file, not whatever file happens to be executing at call time
        // (oracle m8c: 'previously declared in m8c.php:3').
        if d.file.is_empty() {
            d.file = self.diag_file();
        }

        // Synthesize PropDecls from promoted constructor params
        // (`__construct(public readonly int $x)`) — they behave as
        // declared props for visibility/type/readonly and hooks.
        if d.kind != ClassKind::Interface {
            if let Some(ctor) = d
                .methods
                .iter()
                .find(|m| m.decl.name.eq_ignore_ascii_case("__construct"))
            {
                for p in &ctor.decl.params {
                    if p.promoted && !d.props.iter().any(|x| x.name == p.name) {
                        d.props.push(PropDecl {
                            name: p.name.clone(),
                            default: None,
                            is_static: false,
                            visibility: p.vis.unwrap_or(crate::ast::Visibility::Public),
                            readonly: p.readonly,
                            ty: p.ty.clone(),
                            is_abstract: false,
                            is_final: p.is_final,
                            set_vis: p.set_vis,
                            decl_in: None,
                            hooks: p.hooks.clone(),
                            attrs: vec![],
                            line: 0,
                            dline: 0,
                        });
                    }
                }
            }
        }
        self.check_hooked_props(&d)?;
        let lname = decl.name.to_lowercase();
        match decl.kind {
            ClassKind::Interface => {
                if let Some(t0) = d.traits.first() {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot use traits inside of interfaces. {} is used in {}",
                            t0, d.name
                        ),
                        self.cur_line,
                    ));
                }
                Self::resolve_scope_tys(&mut d);
                // `interface B extends A` — B's methods must stay
                // compatible with A's (invalid_covariance_*).
                self.linking.push(Rc::new(d.clone()));
                let checks_res = self.check_interface_sigs(&d);
                self.linking.pop();
                checks_res?;
                self.magic_method_checks(&d)?;
                // An interface declaring __toString implicitly extends
                // Stringable (interface_with_tostring).
                let mut d = d;
                if d.methods
                    .iter()
                    .any(|m| m.decl.name.eq_ignore_ascii_case("__tostring"))
                    && !d
                        .implements
                        .iter()
                        .any(|i| i.eq_ignore_ascii_case("stringable"))
                {
                    d.implements.push("Stringable".to_string());
                }
                self.interfaces.insert(lname.clone(), Rc::new(d));
                self.decl_order.push(lname);
            }
            ClassKind::Trait => {
                // `trait _ {}` — deprecated since 8.4.
                if d.name.rsplit('\\').next() == Some("_") {
                    self.deprecated("Using \"_\" as a trait name is deprecated since 8.4")?;
                }
                // Traits may `use` traits — flatten recursively
                // (flattening003). Origin chains keep the DEFINING
                // trait's name via decl_in.
                if !d.traits.is_empty() {
                    for t in &d.traits {
                        if !self.traits.contains_key(&t.to_lowercase()) {
                            let msg = if self.lookup_class(t).is_some()
                                || self.interfaces.contains_key(&t.to_lowercase())
                            {
                                format!("{} cannot use {} - it is not a trait", d.name, t)
                            } else {
                                format!("Trait \"{}\" not found", t)
                            };
                            // Catchable Error, not a fatal (gh17959).
                            let v = self.exception("Error", &msg);
                            return Err(self.throw(v));
                        }
                    }
                    self.linking.push(Rc::new(d.clone()));
                    let merge_res = self.merge_trait_adaptations(&mut d);
                    self.linking.pop();
                    merge_res?;
                }
                self.magic_method_checks(&d)?;
                self.traits.insert(lname.clone(), Rc::new(d));
                self.decl_order.push(lname);
            }
            _ => {
                // `use static|self|parent` — reserved, uncatchable
                // (class_uses_static).
                for t in &d.traits {
                    if ["static", "self", "parent"]
                        .iter()
                        .any(|r| t.eq_ignore_ascii_case(r))
                    {
                        return Err(PhpError::fatal(
                            format!("Cannot use \"{}\" as trait name, as it is reserved", t),
                            self.cur_line,
                        ));
                    }
                }
                // Apply traits: merge methods/props into the decl.
                if !d.traits.is_empty() {
                    // Missing trait → catchable Error (gh17959).
                    for t in &d.traits {
                        if !self.traits.contains_key(&t.to_lowercase()) {
                            let msg = if self.lookup_class(t).is_some()
                                || self.interfaces.contains_key(&t.to_lowercase())
                            {
                                format!("{} cannot use {} - it is not a trait", d.name, t)
                            } else {
                                format!("Trait \"{}\" not found", t)
                            };
                            let v = self.exception("Error", &msg);
                            return Err(self.throw(v));
                        }
                    }
                    self.linking.push(Rc::new(d.clone()));
                    let merge_res = self.merge_trait_adaptations(&mut d);
                    self.linking.pop();
                    merge_res?;
                }
                // `self`/`static`/`parent` in member types bind to the
                // declaring class at registration — a `self` member
                // otherwise wildcard-matches everything
                // (union_types/anonymous_class). This runs AFTER trait
                // merge so trait methods' `self` resolves to the
                // consuming class (traits/abstract_method_8). `static`
                // loses late-static nuance, which type checks don't
                // distinguish anyway.
                Self::resolve_scope_tys(&mut d);
                self.check_rtwc_attr(&d.attrs, "class")?;
                for p in &d.props {
                    self.check_rtwc_attr(&p.attrs, "property")?;
                }
                // `extends <trait>` / `extends <interface>` → fatal
                // (error_009/error_010).
                if let Some(pn) = &d.parent {
                    let pl = pn.to_lowercase();
                    if self.traits.contains_key(&pl) {
                        return Err(PhpError::fatal(
                            format!("Class {} cannot extend trait {}", d.name, pn),
                            self.cur_line,
                        ));
                    }
                    if self.interfaces.contains_key(&pl) {
                        return Err(PhpError::fatal(
                            format!("Class {} cannot extend interface {}", d.name, pn),
                            self.cur_line,
                        ));
                    }
                }
                // `implements <non-interface>` → fatal (error_008).
                for i in &d.implements {
                    if !self.interfaces.contains_key(&i.to_lowercase()) {
                        let missing = !(self.lookup_class(i).is_some()
                            || self.traits.contains_key(&i.to_lowercase()));
                        if missing {
                            // Catchable Error like a missing trait
                            // (variance/unlinked_parent_2).
                            let v =
                                self.exception("Error", &format!("Interface \"{}\" not found", i));
                            return Err(self.throw(v));
                        }
                        return Err(PhpError::fatal(
                            format!("{} cannot implement {} - it is not an interface", d.name, i),
                            self.cur_line,
                        ));
                    }
                }
                // Implementing Serializable is deprecated (8.1+) — the
                // __serialize/__unserialize pair is the replacement.
                if !d.name.eq_ignore_ascii_case("serializable")
                    && self.implements_iface(&d, "serializable")
                {
                    self.deprecated(&format!(
                        "{} implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary)",
                        d.name
                    ))?;
                }
                // Magic-method declaration diagnostics are compile-time
                // in zend — they precede every link-time inheritance
                // fatal (magic_methods_008).
                // A private+final method (declared outright or produced
                // by `m as final` / `m as private` adaptations) warns
                // once per class (gh12854).
                if d.methods.iter().any(|m| {
                    m.is_final
                        && m.visibility == crate::ast::Visibility::Private
                        && m.trait_alias_of.is_none()
                        // PHP exempts __construct: a private final
                        // constructor can't collide with a child's own
                        // ctor, so no warning (every other magic method
                        // still warns — marc-mabe/php-enum relies on it).
                        && !m.decl.name.eq_ignore_ascii_case("__construct")
                }) {
                    self.warn("Private methods cannot be final as they are never overridden by other classes")?;
                }
                self.magic_method_checks(&d)?;
                self.linking.push(Rc::new(d.clone()));
                let checks_res = self
                    .check_interface_sigs(&d)
                    .and_then(|_| self.check_abstract_hooks(&d))
                    .and_then(|_| self.check_abstract_methods(&d))
                    .and_then(|_| self.check_final_override(&d))
                    .and_then(|_| self.check_override_sigs(&d))
                    .and_then(|_| self.check_const_types(&d));
                self.linking.pop();
                checks_res?;
                // Declaring __toString (incl. via a trait) implicitly
                // implements Stringable — added post-checks like zend,
                // so no sig-compat check runs against it
                // (stringable_automatic_implementation).
                if d.methods
                    .iter()
                    .any(|m| m.decl.name.eq_ignore_ascii_case("__tostring"))
                    && !d
                        .implements
                        .iter()
                        .any(|i| i.eq_ignore_ascii_case("stringable"))
                {
                    d.implements.push("Stringable".to_string());
                }
                self.classes.insert(
                    lname.clone(),
                    Rc::new(PhpClass {
                        decl: Rc::new(d),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    }),
                );
                self.decl_order.push(lname);
            }
        }
        // Delayed variance obligations re-verify once the types they
        // waited on link — a registration re-checks only its own
        // ancestors' obligations, and never nests a pass inside a
        // running pass (class_order_autoload*).
        if !self.in_variance_pass && !self.variance_obligations.is_empty() {
            self.in_variance_pass = true;
            let res = self.process_variance_obligations(&decl);
            self.in_variance_pass = false;
            res?;
        }
        Ok(())
    }

    /// Zend's magic-method signature validation at class registration
    /// (zend_compile_magic_method): non-public visibility is a warning
    /// (ctor/dtor/clone exempt — private __clone is the
    /// clone-prevention idiom); wrong static-ness, arity or by-ref
    /// params are fatals. Checked after trait merge so merged methods
    /// validate too; interfaces and traits get the same rules.
    fn magic_method_checks(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        for m in &d.methods {
            let n = m.decl.name.to_lowercase();
            let (arity, want_static, vis_exempt): (Option<usize>, bool, bool) = match n.as_str() {
                "__call" | "__callstatic" => (Some(2), n == "__callstatic", false),
                "__get" | "__isset" | "__unset" | "__unserialize" => (Some(1), false, false),
                "__set" => (Some(2), false, false),
                "__set_state" => (Some(1), true, false),
                "__sleep" | "__wakeup" | "__tostring" | "__serialize" | "__debuginfo" => {
                    (Some(0), false, false)
                }
                "__invoke" => (None, false, false),
                "__construct" => (None, false, true),
                "__destruct" => (Some(0), false, true),
                "__clone" => (Some(0), false, true),
                _ => continue,
            };
            let cn = d.name.rsplit('\\').next().unwrap_or(&d.name);
            // Zend echoes the declared spelling in all diagnostics.
            let mn = &m.decl.name;
            // Zend order: arity first, then static-ness, by-ref and
            // type checks; the public-visibility warning only fires
            // when the signature is otherwise valid
            // (magic_methods_007/010).
            let nargs = m.decl.params.len();
            if let Some(need) = arity {
                if nargs != need {
                    return self.fail(PhpError::fatal(
                        if need == 0 {
                            format!("Method {}::{}() cannot take arguments", cn, mn)
                        } else {
                            format!(
                                "Method {}::{}() must take exactly {} argument{}",
                                cn,
                                mn,
                                need,
                                if need == 1 { "" } else { "s" }
                            )
                        },
                        m.decl.line,
                    ));
                }
            }
            if m.is_static != want_static {
                return self.fail(PhpError::fatal(
                    format!(
                        "Method {}::{}() {}",
                        cn,
                        mn,
                        if want_static {
                            "must be static"
                        } else {
                            "cannot be static"
                        }
                    ),
                    m.decl.line,
                ));
            }
            // `__invoke` and `__construct` are exempt — `function
            // &__invoke(&$a)` and `__construct(&$x)` are legal
            // signatures (closure_014; PHPUnit's Stub/ReturnReference).
            if n != "__invoke" && n != "__construct" && m.decl.params.iter().any(|p| p.by_ref) {
                return self.fail(PhpError::fatal(
                    format!("Method {}::{}() cannot take arguments by reference", cn, mn),
                    m.decl.line,
                ));
            }
            // Declared param types must still accept the values zend
            // passes (`?string` and `iterable` are fine where `string`
            // /`array` are required).
            let req_params: &[&str] = match n.as_str() {
                "__call" | "__callstatic" => &["string", "array"],
                "__get" | "__set" | "__isset" | "__unset" => &["string"],
                "__unserialize" | "__set_state" => &["array"],
                _ => &[],
            };
            for (i, req) in req_params.iter().enumerate() {
                if let Some(p) = m.decl.params.get(i) {
                    if let Some(ty) = &p.ty {
                        if !ty.iter().any(|t| {
                            t == req || t == "mixed" || (*req == "array" && t == "iterable")
                        }) {
                            return self.fail(PhpError::fatal(
                                format!(
                                    "{}::{}(): Parameter #{} (${}) must be of type {} when declared",
                                    cn,
                                    mn,
                                    i + 1,
                                    p.name,
                                    req
                                ),
                                m.decl.line,
                            ));
                        }
                    }
                }
            }
            // Declared return type must be a subtype of zend's
            // requirement (nullable allowed only when required).
            let req_ret: Option<&str> = match n.as_str() {
                "__isset" => Some("bool"),
                "__tostring" => Some("string"),
                "__sleep" | "__serialize" => Some("array"),
                "__debuginfo" => Some("?array"),
                "__set" | "__unset" | "__unserialize" | "__wakeup" | "__clone" => Some("void"),
                "__set_state" => Some("object"),
                _ => None,
            };
            if let Some(req) = req_ret {
                if let Some(ty) = &m.decl.ret {
                    let req_nullable = req.starts_with('?');
                    let req_base = req.trim_start_matches('?');
                    let declared_nullable = ty.iter().any(|t| t == "null");
                    let fits = (!declared_nullable || req_nullable)
                        && ty.iter().filter(|t| *t != "null").all(|t| {
                            t == req_base
                                || (req_base == "bool" && (t == "true" || t == "false"))
                                || (req_base == "object"
                                    && !matches!(
                                        t.as_str(),
                                        "int"
                                            | "float"
                                            | "string"
                                            | "bool"
                                            | "array"
                                            | "void"
                                            | "iterable"
                                            | "callable"
                                            | "mixed"
                                            | "never"
                                            | "false"
                                            | "true"
                                    ))
                        });
                    if !fits {
                        return self.fail(PhpError::fatal(
                            format!(
                                "{}::{}(): Return type must be {} when declared",
                                cn, mn, req
                            ),
                            m.decl.line,
                        ));
                    }
                }
            }
            if !vis_exempt && m.visibility != crate::ast::Visibility::Public {
                self.warn(&format!(
                    "The magic method {}::{}() must have public visibility",
                    cn, mn
                ))?;
            }
        }
        Ok(())
    }

    /// The class currently linking owes a signature re-check whenever
    /// one of its probes autoloads a type — record it before the
    /// autoloader runs so nested registrations can re-verify it.
    pub(in crate::interp) fn note_variance_obligation(&mut self) {
        if let Some(d) = self.linking.last() {
            let l = d.name.to_lowercase();
            if !self.variance_obligations.contains(&l) {
                self.variance_obligations.push(l);
            }
        }
    }

    /// Re-run the deferred signature checks for obligated ancestors
    /// of the just-linked class — a subclass can't link against a
    /// parent whose own variance is still unverified, so linking it
    /// forces the parent's obligations first (class_order_autoload*).
    /// Obligations on unrelated classes stay pending (error8).
    fn process_variance_obligations(&mut self, decl: &Rc<ClassDecl>) -> Result<(), PhpError> {
        let mut anc: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut work: Vec<String> = decl
            .parent
            .iter()
            .chain(decl.implements.iter())
            .map(|n| n.trim_start_matches('\\').to_lowercase())
            .collect();
        while let Some(n) = work.pop() {
            if !anc.insert(n.clone()) {
                continue;
            }
            let d = self
                .classes
                .get(&n)
                .map(|c| c.decl.clone())
                .or_else(|| self.interfaces.get(&n).cloned())
                .or_else(|| self.traits.get(&n).cloned())
                .or_else(|| {
                    self.linking
                        .iter()
                        .find(|c| c.name.to_lowercase() == n)
                        .cloned()
                });
            if let Some(d) = d {
                work.extend(
                    d.parent
                        .iter()
                        .chain(d.implements.iter())
                        .map(|x| x.trim_start_matches('\\').to_lowercase()),
                );
            }
        }
        let obls = std::mem::take(&mut self.variance_obligations);
        for lname in obls {
            if !anc.contains(&lname) {
                // Not an ancestor of what just linked — stays pending.
                self.variance_obligations.push(lname);
                continue;
            }
            let decl = self
                .classes
                .get(&lname)
                .map(|c| c.decl.clone())
                .or_else(|| self.interfaces.get(&lname).cloned())
                .or_else(|| {
                    self.linking
                        .iter()
                        .find(|c| c.name.to_lowercase() == lname)
                        .cloned()
                });
            let Some(d) = decl else { continue };
            self.check_interface_sigs(&d)?;
            self.check_override_sigs(&d)?;
        }
        Ok(())
    }

    /// `self`/`static`/`parent` members in method/prop types bind to
    /// the declaring class name at registration (anonymous_class).
    fn resolve_scope_tys(d: &mut ClassDecl) {
        let (dn, dp) = (d.name.clone(), d.parent.clone());
        let resolve = |ms: &mut Vec<String>| {
            for m in ms.iter_mut() {
                let l = m.to_lowercase();
                // `static` stays literal — late-static binds to the
                // called class, resolved at check time.
                if l == "self" {
                    *m = dn.clone();
                } else if l == "parent" {
                    if let Some(p) = &dp {
                        *m = p.clone();
                    }
                }
            }
        };
        for m in d.methods.iter_mut() {
            let mut mm = (**m).clone();
            for p in mm.decl.params.iter_mut() {
                if let Some(ty) = &mut p.ty {
                    resolve(ty);
                }
            }
            if let Some(ty) = &mut mm.decl.ret {
                resolve(ty);
            }
            *m = Rc::new(mm);
        }
        for p in d.props.iter_mut() {
            if let Some(ty) = &mut p.ty {
                resolve(ty);
            }
        }
    }

    /// Merge used traits' methods into `d`, applying `insteadof`
    /// exclusions, `as` aliases/visibility changes, and collision
    /// detection. Trait origin is preserved on each merged method as
    /// `decl.decl_in` (drives `__METHOD__`/`__TRAIT__`).
    fn merge_trait_adaptations(&mut self, d: &mut ClassDecl) -> Result<(), PhpError> {
        let used: Vec<(String, Rc<ClassDecl>)> = d
            .traits
            .iter()
            .filter_map(|t| {
                self.traits
                    .get(&t.to_lowercase())
                    .map(|td| (t.clone(), td.clone()))
            })
            .collect();
        let cur_line = self.cur_line;
        let dname = d.name.clone();
        let is_used = |n: &str| d.traits.iter().any(|u| u.eq_ignore_ascii_case(n));
        // `insteadof`/`as` on a trait that doesn't exist: distinct
        // message from exists-but-not-used (precedence_unknown_class).
        // Free-standing fn (not a closure) so later `&mut self` calls
        // don't conflict with a captured borrow of `self.traits`.
        fn not_found(
            traits: &HashMap<String, Rc<ClassDecl>>,
            n: &str,
            dname: &str,
            line: usize,
        ) -> PhpError {
            if traits.contains_key(&n.to_lowercase()) {
                PhpError::fatal(
                    format!("Required Trait {} wasn't added to {}", n, dname),
                    line,
                )
            } else {
                PhpError::fatal(format!("Could not find trait {}", n), line)
            }
        }
        // `static`/`self`/`parent` are reserved — never valid trait
        // names (static_in_trait_*).
        let reserved = |n: &str| {
            ["static", "self", "parent"]
                .iter()
                .any(|r| n.eq_ignore_ascii_case(r))
        };
        for t in &d.traits {
            if reserved(t) {
                return Err(PhpError::fatal(
                    format!("Cannot use \"{}\" as trait name, as it is reserved", t),
                    cur_line,
                ));
            }
        }
        // Validate adaptation trait references.
        for ad in &d.adaptations {
            match ad {
                crate::ast::TraitAdaptation::Insteadof {
                    trait_name,
                    method,
                    excludes,
                } => {
                    for n in std::iter::once(trait_name).chain(excludes.iter()) {
                        if reserved(n) {
                            return Err(PhpError::fatal(
                                format!("Cannot use \"{}\" as trait name, as it is reserved", n),
                                cur_line,
                            ));
                        }
                        // A class name in `as`/`insteadof` is its own
                        // error (bug64235).
                        if !is_used(n) && self.classes.contains_key(&n.to_lowercase()) {
                            return Err(PhpError::fatal(
                                format!(
                                    "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                    n
                                ),
                                cur_line,
                            ));
                        }
                    }
                    if !is_used(trait_name) {
                        return Err(not_found(&self.traits, trait_name, &dname, cur_line));
                    }
                    // `T::m insteadof ...` — the method must exist in T
                    // (bug60165d).
                    if let Some(td) = self.traits.get(&trait_name.to_lowercase()) {
                        if !td
                            .methods
                            .iter()
                            .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                        {
                            return Err(PhpError::fatal(
                                format!(
                                    "A precedence rule was defined for {}::{} but this method does not exist",
                                    trait_name, method
                                ),
                                cur_line,
                            ));
                        }
                    }
                    for e in excludes {
                        if !is_used(e) {
                            return Err(not_found(&self.traits, e, &dname, cur_line));
                        }
                    }
                    if excludes.iter().any(|e| e.eq_ignore_ascii_case(trait_name)) {
                        return Err(PhpError::fatal(
                            format!(
                                "Inconsistent insteadof definition. The method {} is to be used from {}, but {} is also on the exclude list",
                                method, trait_name, trait_name
                            ),
                            cur_line,
                        ));
                    }
                }
                crate::ast::TraitAdaptation::Alias {
                    trait_name: Some(tn),
                    ..
                } => {
                    if !is_used(tn) {
                        if self.classes.contains_key(&tn.to_lowercase()) {
                            return Err(PhpError::fatal(
                                format!(
                                    "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                    tn
                                ),
                                cur_line,
                            ));
                        }
                        return Err(not_found(&self.traits, tn, &dname, cur_line));
                    }
                }
                crate::ast::TraitAdaptation::Alias {
                    trait_name: None,
                    method,
                    alias,
                    ..
                } => {
                    // `method as alias` with no qualifier — method must
                    // exist somewhere among the used traits.
                    if alias.is_some()
                        && !used.iter().any(|(_, td)| {
                            td.methods
                                .iter()
                                .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                        })
                    {
                        return Err(PhpError::fatal(
                            format!(
                                "An alias ({}) was defined for method {}(), but this method does not exist",
                                alias.as_deref().unwrap_or_default(),
                                method
                            ),
                            cur_line,
                        ));
                    }
                }
            }
        }
        // insteadof exclusions: (method_lname, suppressed trait_lname)
        let mut exclusions: Vec<(String, String)> = Vec::new();
        for ad in &d.adaptations {
            if let crate::ast::TraitAdaptation::Insteadof {
                method, excludes, ..
            } = ad
            {
                for e in excludes {
                    // Each trait's method may be excluded only once
                    // (error_010).
                    if exclusions
                        .iter()
                        .any(|(mn, et)| *mn == method.to_lowercase() && et.eq_ignore_ascii_case(e))
                    {
                        return Err(PhpError::fatal(
                            format!(
                                "Failed to evaluate a trait precedence ({}). Method of trait {} was defined to be excluded multiple times",
                                method, e
                            ),
                            cur_line,
                        ));
                    }
                    exclusions.push((method.to_lowercase(), e.to_lowercase()));
                }
            }
        }
        // Merge methods in `use` order. Class-own methods always win
        // silently; a trait's ABSTRACT requirement is still checked
        // against the class's implementation (abstract_method_*).
        // `taken` = name -> (display trait, origin trait, own|abstract)
        let mut taken: HashMap<String, (String, String)> = HashMap::new();
        for m in &d.methods {
            taken.insert(m.decl.name.to_lowercase(), (dname.clone(), String::new()));
        }
        for (t, td) in &used {
            for m in &td.methods {
                let lname = m.decl.name.to_lowercase();
                if exclusions
                    .iter()
                    .any(|(mn, et)| *mn == lname && et.eq_ignore_ascii_case(t))
                {
                    continue;
                }
                let origin = m.decl.decl_in.clone().unwrap_or_else(|| t.clone());
                // A trait abstract requirement already implemented by
                // an ancestor stays inherited, not merged (bug55424) —
                // but the inherited impl must still be signature-
                // compatible with the abstract (gh14009_002).
                if m.is_abstract {
                    if let Some((aon, am)) = self.ancestor_concrete(d, &lname) {
                        if let Some(e) = self.trait_sig_error(&am, m, &aon, &origin, false) {
                            return Err(e);
                        }
                        continue;
                    }
                }
                if let Some((pt, porig)) = taken.get(&lname).cloned() {
                    // Diamond reuse of the same origin trait is fine
                    // (bug63911).
                    if porig.eq_ignore_ascii_case(&origin) {
                        continue;
                    }
                    let class_own = porig.is_empty();
                    let existing = d
                        .methods
                        .iter()
                        .find(|x| x.decl.name.eq_ignore_ascii_case(&m.decl.name))
                        .cloned();
                    // Abstract requirements interplay with what holds
                    // the name already.
                    if m.is_abstract || existing.as_ref().is_some_and(|x| x.is_abstract) {
                        if class_own {
                            // Class impl must satisfy the abstract
                            // (abstract_method_1/3/4/5).
                            if let Some(ex) = existing {
                                if let Some(e) =
                                    self.trait_sig_error(&ex, m, &dname, &origin, false)
                                {
                                    return Err(e);
                                }
                            }
                            continue;
                        }
                        if m.is_abstract && existing.as_ref().is_some_and(|x| x.is_abstract) {
                            // Two abstract requirements: signatures must
                            // agree in both directions (bug60217).
                            let ex = existing.unwrap();
                            if let Some(e) = self.trait_sig_error(&ex, m, &pt, &origin, true) {
                                return Err(e);
                            }
                            continue;
                        }
                        // concrete-vs-abstract: concrete impl checked
                        // against the abstract requirement; on success
                        // the concrete replaces (or keeps) the slot.
                        let (impl_m, abs_m, abs_owner) = if m.is_abstract {
                            (existing.clone().unwrap(), m.clone(), &origin)
                        } else {
                            (m.clone(), existing.clone().unwrap(), &porig)
                        };
                        if let Some(e) =
                            self.trait_sig_error(&impl_m, &abs_m, &dname, abs_owner, false)
                        {
                            return Err(e);
                        }
                        if m.is_abstract {
                            continue;
                        }
                        // Concrete replaces the abstract slot.
                        if let Some(slot) = d
                            .methods
                            .iter_mut()
                            .find(|x| x.decl.name.eq_ignore_ascii_case(&m.decl.name))
                        {
                            let mut m2 = (**m).clone();
                            m2.decl.decl_in = Some(origin.clone());
                            *slot = Rc::new(m2);
                            taken.insert(lname, (t.clone(), origin));
                        }
                        continue;
                    }
                    if class_own {
                        continue;
                    }
                    return Err(PhpError::fatal(
                        format!(
                            "Trait method {}::{} has not been applied as {}::{}, because of collision with {}::{}",
                            t, m.decl.name, dname, m.decl.name, pt, m.decl.name
                        ),
                        cur_line,
                    ));
                }
                let mut m2 = (**m).clone();
                m2.decl.decl_in = Some(origin.clone());
                taken.insert(lname, (t.clone(), origin));
                d.methods.push(Rc::new(m2));
            }
        }
        // Aliases: clone the source method under a new name and/or apply
        // a visibility override to the merged original.
        for ad in &d.adaptations {
            let crate::ast::TraitAdaptation::Alias {
                trait_name,
                method,
                alias,
                vis,
                is_final,
            } = ad
            else {
                continue;
            };
            if let Some(tn) = trait_name {
                if reserved(tn) {
                    return Err(PhpError::fatal(
                        format!("Cannot use \"{}\" as trait name, as it is reserved", tn),
                        cur_line,
                    ));
                }
                if !is_used(tn) {
                    if self.classes.contains_key(&tn.to_lowercase()) {
                        return Err(PhpError::fatal(
                            format!(
                                "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                tn
                            ),
                            cur_line,
                        ));
                    }
                    return Err(not_found(&self.traits, tn, &dname, cur_line));
                }
            }
            let src: Option<Rc<MethodDecl>> = match trait_name {
                Some(tn) => self.traits.get(&tn.to_lowercase()).and_then(|td| {
                    td.methods
                        .iter()
                        .find(|m| m.decl.name.eq_ignore_ascii_case(method))
                        .cloned()
                }),
                None => {
                    // Unqualified `m as x` is ambiguous when >1 used
                    // trait provides `m` (bug62069).
                    let mut holders = used.iter().filter(|(_, td)| {
                        td.methods
                            .iter()
                            .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                    });
                    match (holders.next(), holders.next()) {
                        (Some((n1, _)), Some((n2, _))) => {
                            return Err(PhpError::fatal(
                                format!(
                                    "An alias was defined for method {}(), which exists in both {} and {}. Use {}::{} or {}::{} to resolve the ambiguity",
                                    method, n1, n2, n1, method, n2, method
                                ),
                                cur_line,
                            ));
                        }
                        (Some((_, td1)), None) => td1
                            .methods
                            .iter()
                            .find(|m| m.decl.name.eq_ignore_ascii_case(method))
                            .cloned(),
                        _ => None,
                    }
                }
            };
            let Some(m) = src else {
                // `T::m as x` cites the qualified name; `m as x` cites
                // the alias (bug60165b); a bare `m as vis` modifier is
                // the "modifiers changed" wording (bug54441).
                if trait_name.is_some() {
                    return Err(PhpError::fatal(
                        format!(
                            "An alias was defined for {}::{} but this method does not exist",
                            trait_name.as_deref().unwrap_or_default(),
                            method
                        ),
                        cur_line,
                    ));
                }
                if let Some(a) = alias {
                    return Err(PhpError::fatal(
                        format!(
                            "An alias ({}) was defined for method {}(), but this method does not exist",
                            a, method
                        ),
                        cur_line,
                    ));
                }
                return Err(PhpError::fatal(
                    format!(
                        "The modifiers of the trait method {}() are changed, but this method does not exist. Error in",
                        method
                    ),
                    cur_line,
                ));
            };
            // `as abstract|final|static` are not valid alias modifiers
            // (language018/019) — PHP rejects them as alias names too.
            for bad in ["abstract", "static"] {
                if alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(bad))
                {
                    return Err(PhpError::fatal(
                        format!("Cannot use \"{}\" as method modifier in trait alias", bad),
                        cur_line,
                    ));
                }
            }
            let origin = m
                .decl
                .decl_in
                .clone()
                .or_else(|| trait_name.clone())
                .unwrap_or_default();
            if let Some(a) = alias {
                let alname = a.to_lowercase();
                // An aliased name collides like a real method
                // (language010/014): a merged method or an earlier
                // alias holding the name is fatal.
                let src_trait = trait_name.clone().unwrap_or_else(|| {
                    used.iter()
                        .find(|(_, td)| {
                            td.methods
                                .iter()
                                .any(|mm| mm.decl.name.eq_ignore_ascii_case(method))
                        })
                        .map(|(t, _)| t.clone())
                        .unwrap_or_default()
                });
                if let Some((pt, porg)) = taken.get(&alname) {
                    if !porg.is_empty() {
                        // Trait order decides the loser: a slot held by
                        // a trait merged LATER loses to the earlier
                        // trait's alias (language010 vs language014).
                        let ord =
                            |n: &str| used.iter().position(|(u, _)| u.eq_ignore_ascii_case(n));
                        let (lt, lm, wt) = match (ord(&src_trait), ord(pt)) {
                            (Some(si), Some(pi)) if pi > si => {
                                (pt.clone(), a.clone(), src_trait.clone())
                            }
                            _ => (src_trait.clone(), method.clone(), pt.clone()),
                        };
                        return Err(PhpError::fatal(
                            format!(
                                "Trait method {}::{} has not been applied as {}::{}, because of collision with {}::{}",
                                lt, lm, dname, a, wt, a
                            ),
                            cur_line,
                        ));
                    }
                    // The class's own method wins silently (bug61998).
                    continue;
                }
                let mut m2 = (*m).clone();
                m2.decl.name = a.clone().into();
                if let Some(v) = vis {
                    m2.visibility = *v;
                }
                if *is_final {
                    m2.is_final = true;
                }
                m2.decl.decl_in = Some(origin.clone());
                m2.trait_alias_of = Some(m.decl.name.to_string());
                taken.insert(alname, (src_trait, origin));
                d.methods.push(Rc::new(m2));
            } else if vis.is_some() || *is_final {
                // `m as private` / `m as final` — modifier change on the
                // merged original itself.
                for slot in d.methods.iter_mut() {
                    if slot.decl.name.eq_ignore_ascii_case(method) {
                        let mut m2 = (**slot).clone();
                        if let Some(v) = vis {
                            m2.visibility = *v;
                        }
                        if *is_final {
                            m2.is_final = true;
                        }
                        *slot = Rc::new(m2);
                        break;
                    }
                }
            }
        }
        // Trait props merge with the identical-definition rule
        // (property001/002, bug74922): differing decls fatal; hooked
        // props can't be resolved at all.
        for (t, td) in &used {
            for p in &td.props {
                if let Some(ex) = d.props.iter().find(|x| x.name == p.name) {
                    if ex.hooks.is_some() || p.hooks.is_some() {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same hooked property (${}) in the composition of {}. Conflict resolution between hooked properties is currently not supported. Class was composed",
                                ex_src, t, p.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    let compat = ex.visibility == p.visibility
                        && ex.is_static == p.is_static
                        && ex.readonly == p.readonly
                        && ex.ty == p.ty
                        && self.const_exprs_eq(
                            &ex.default,
                            &ex.decl_in.clone().unwrap_or_else(|| dname.clone()),
                            &p.default,
                            t,
                        );
                    if !compat {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same property (${}) in the composition of {}. However, the definition differs and is considered incompatible. Class was composed",
                                ex_src, t, p.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    continue;
                }
                let mut np = p.clone();
                np.decl_in = Some(t.clone());
                d.props.push(np);
            }
        }
        // Trait constants merge like props: same name+identical
        // definition is fine, differing definitions are fatal
        // (constant_*). `T::CONST` direct access is rejected at the
        // lookup site instead.
        for (t, td) in &used {
            for cd in &td.consts {
                if let Some(ex) = d.consts.iter().find(|x| x.name == cd.name) {
                    let compat = ex.visibility == cd.visibility
                        && ex.is_final == cd.is_final
                        && ty_list_eq(&ex.ty, &cd.ty)
                        && self.const_exprs_eq(
                            &Some(ex.value.clone()),
                            &ex.decl_in.clone().unwrap_or_else(|| dname.clone()),
                            &Some(cd.value.clone()),
                            t,
                        );
                    if !compat {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same constant ({}) in the composition of {}. However, the definition differs and is considered incompatible. Class was composed",
                                ex_src, t, cd.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    continue;
                }
                let mut nc = cd.clone();
                nc.decl_in = Some(t.clone());
                d.consts.push(nc);
            }
        }
        Ok(())
    }

    /// Two default-value exprs are compatible when their evaluated
    /// values match loosely (bug74922, constant_016); falls back to a
    /// textual compare when either side won't eval (both-None is fine).
    /// Each side evals in its declaring trait's namespace so an
    /// unqualified `FOO` in `Bug74922\T1` means `Bug74922\FOO`.
    fn const_exprs_eq(
        &mut self,
        a: &Option<Expr>,
        a_owner: &str,
        b: &Option<Expr>,
        b_owner: &str,
    ) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                match (self.eval_in_ns(x, a_owner), self.eval_in_ns(y, b_owner)) {
                    (Ok(va), Ok(vb)) => identical(&va, &vb),
                    _ => format!("{:?}", x) == format!("{:?}", y),
                }
            }
            _ => false,
        }
    }

    /// Const-eval an expr as if inside `owner`'s namespace (trait prop/
    /// const defaults resolve unqualified names against their declaring
    /// namespace — bug74922b).
    fn eval_in_ns(&mut self, e: &Expr, owner: &str) -> Result<Value, PhpError> {
        let ns = owner
            .rsplit_once('\\')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        let old = std::mem::replace(&mut self.globals.ns, ns);
        let f = self.cur_file.clone();
        let r = self.eval_decl_const(e, &f, 0);
        self.globals.ns = old;
        r
    }

    /// Param-list render for "Declaration of X::m(...) must be
    /// compatible" diagnostics (Zend prints the declared signature).
    /// Render one type member for signature messages: `self` resolves
    /// against the composing class (abstract_method_10), other members
    /// stay verbatim.
    fn sig_ty(ty: &[String], ctx: &str) -> String {
        // `X|null` renders as `?X` in Zend signatures (internal_parent).
        if ty.len() == 2 {
            if let Some(other) = ty.iter().find(|t| !t.eq_ignore_ascii_case("null")) {
                if ty.iter().any(|t| t.eq_ignore_ascii_case("null")) && !other.contains('&') {
                    return format!(
                        "?{}",
                        if other.eq_ignore_ascii_case("self") {
                            ctx.to_string()
                        } else {
                            other.clone()
                        }
                    );
                }
            }
        }
        ty.iter()
            .map(|t| {
                if t.eq_ignore_ascii_case("self") {
                    ctx.to_string()
                } else if t.eq_ignore_ascii_case("iterable") {
                    // Compatibility messages render the normalized
                    // form (invalid5).
                    "Traversable|array".to_string()
                } else if t.contains('&') && ty.len() > 1 {
                    format!("({t})")
                } else {
                    t.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn sig_str(f: &crate::ast::FunctionDecl, ctx: &str) -> String {
        f.params
            .iter()
            .map(|p| {
                let ty =
                    p.ty.as_ref()
                        .map(|t| Self::sig_ty(t, ctx) + " ")
                        .unwrap_or_default();
                let br = if p.by_ref { "&" } else { "" };
                let var = if p.variadic {
                    format!("...${}", p.name)
                } else {
                    format!("${}", p.name)
                };
                let def = match &p.default {
                    Some(Expr::Int(i)) => format!(" = {}", i),
                    Some(Expr::Float(f)) => format!(" = {}", f),
                    Some(Expr::Str(b)) => format!(" = '{}'", b),
                    Some(Expr::Null) => " = null".to_string(),
                    Some(Expr::Bool(b)) => format!(" = {}", b),
                    Some(Expr::Const(c)) => format!(" = {}", c),
                    Some(Expr::ClassConst { class, name }) => match class.as_ref() {
                        Expr::Var(n) | Expr::Const(n) => format!(" = {}::{}", n, name),
                        _ => format!(" = {}", name),
                    },
                    Some(Expr::ArrayLit(_)) => " = []".to_string(),
                    _ => String::new(),
                };
                format!("{}{}{}{}", ty, br, var, def)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `sig_str` variant that appends `: ret` OUTSIDE the paren — used
    /// inside `({})` placeholders, so the signature is `(params): ret`.
    fn sig_str_full(f: &crate::ast::FunctionDecl, ctx: &str) -> String {
        let ps = Self::sig_str(f, ctx);
        match &f.ret {
            Some(r) => format!("({}): {}", ps, Self::sig_ty(r, ctx)),
            None => format!("({})", ps),
        }
    }

    /// The (declaring-class name, method) pair for `lname` provided by
    /// a concrete method in `d`'s ancestor chain — nearest wins.
    fn ancestor_concrete(&self, d: &ClassDecl, lname: &str) -> Option<(String, Rc<MethodDecl>)> {
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if let Some(m) = pc
                .decl
                .methods
                .iter()
                .find(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            {
                return Some((pc.decl.name.clone(), m.clone()));
            }
            pn = pc.decl.parent.clone();
        }
        None
    }

    /// Signature-compat check of an implementation against a trait's
    /// abstract requirement (abstract_method_*). `both_abs` marks the
    /// two-traits-both-abstract case where Zend cites trait names for
    /// both sides.
    fn trait_sig_error(
        &mut self,
        impl_m: &Rc<MethodDecl>,
        abs_m: &Rc<MethodDecl>,
        impl_disp: &str,
        abs_disp: &str,
        both_abs: bool,
    ) -> Option<PhpError> {
        let m = &impl_m.decl.name;
        if impl_m.is_static != abs_m.is_static {
            return Some(PhpError::fatal(
                format!(
                    "Cannot make {} method {}::{}() {} in class {}",
                    if abs_m.is_static {
                        "static"
                    } else {
                        "non static"
                    },
                    abs_disp,
                    m,
                    if impl_m.is_static {
                        "static"
                    } else {
                        "non static"
                    },
                    // impl_disp is the using class for concrete impls;
                    // for two abstract traits Zend still prints the
                    // class being composed... using impl_disp for both.
                    impl_disp
                ),
                self.cur_line,
            ));
        }
        let req = |ms: &MethodDecl| {
            // Optional-before-required counts as required (zend's
            // "implicitly required"), matching call-site arity.
            ms.decl
                .params
                .iter()
                .rposition(|p| p.default.is_none() && !p.variadic)
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        let (ir, ar) = (req(impl_m), req(abs_m));
        let count_ok = ir <= ar
            && (impl_m.decl.params.iter().any(|p| p.variadic)
                || (!abs_m.decl.params.iter().any(|p| p.variadic)
                    && impl_m.decl.params.len() >= abs_m.decl.params.len()));
        let mut ok = count_ok;
        // An unresolvable class member makes the check impossible
        // rather than incompatible — Zend reports which class it
        // couldn't load (variance/trait_error, abstract_constructor).
        let mut miss: Option<String> = None;
        if ok {
            for (i, ap) in abs_m.decl.params.iter().enumerate() {
                if ap.variadic {
                    break;
                }
                let Some(ip) = impl_m.decl.params.get(i) else {
                    ok = false;
                    break;
                };
                if ip.variadic {
                    break;
                }
                if ip.by_ref != ap.by_ref {
                    ok = false;
                    break;
                }
                let it = ip.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                let at = ap.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                if !self.ty_sup(&it, &at) {
                    miss = self.first_unres(&it).or_else(|| self.first_unres(&at));
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            // Return-type covariance: an untyped impl fails a typed
            // abstract; a typed impl must be a subtype of the abstract's
            // (`never` bottoms out any requirement) (bug81192).
            ok = match (&impl_m.decl.ret, &abs_m.decl.ret) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(ir), Some(ar)) => {
                    let resolve = |ms: &[String]| -> Vec<String> {
                        ms.iter()
                            .map(|m| {
                                if m.eq_ignore_ascii_case("self") {
                                    impl_disp.to_string()
                                } else {
                                    m.clone()
                                }
                            })
                            .collect()
                    };
                    let (ir2, ar2) = (resolve(ir), resolve(ar));
                    // `static` in the abstract keeps its late-static
                    // meaning: the impl's own class satisfies it only
                    // when the class is final (self == static then);
                    // a real subclass member always narrows it
                    // (override_static_with_self/*).
                    let impl_final = self.linking.last().map(|c| c.is_final).unwrap_or(false);
                    let covers = ir2.iter().all(|t| {
                        ar2.iter().any(|s| {
                            if s.eq_ignore_ascii_case("static") {
                                t.eq_ignore_ascii_case("static")
                                    || (t.eq_ignore_ascii_case(impl_disp) && impl_final)
                                    || self.ty_member_is_a(t, impl_disp)
                                        && !t.eq_ignore_ascii_case(impl_disp)
                            } else if t.eq_ignore_ascii_case("static") {
                                // impl-side `static` ⊆ s when the impl
                                // class is-a s (any late-static callee
                                // is still an s) (static_variance_success).
                                self.ty_member_is_a(impl_disp, s)
                            } else {
                                self.ty_member_is_a(t, s)
                            }
                        })
                    });
                    let pass = ir2.iter().any(|m| m.eq_ignore_ascii_case("never")) || covers;
                    if !pass {
                        miss = self.first_unres(&ir2).or_else(|| self.first_unres(&ar2));
                    }
                    pass
                }
            };
        }
        // A fatal raised inside an autoload the probes triggered
        // aborts the whole check — the original error wins over any
        // synthesized compatibility message (error3 cascade).
        if let Some(e) = self.sig_fatal.take() {
            return Some(e);
        }
        if ok && both_abs {
            // Requirements must agree in BOTH directions (bug60217c).
            return self.trait_sig_error(abs_m, impl_m, abs_disp, impl_disp, false);
        }
        if ok {
            return None;
        }
        // An unresolvable class member makes the check impossible
        // rather than incompatible — Zend reports which class it
        // couldn't load (variance/trait_error, abstract_constructor).
        if let Some(cn) = miss {
            if self.autoloading.contains(&cn.to_lowercase()) {
                // The compared type's own autoload is still in flight —
                // Zend defers the verdict; the obligation re-runs when
                // the type links (class_order_autoload1).
                return None;
            }
            let mut e = PhpError::fatal(
                format!(
                    "Could not check compatibility between {}::{}{} and {}::{}{}, because class {} is not available",
                    impl_disp,
                    m,
                    Self::sig_str_full(&impl_m.decl, impl_disp),
                    abs_disp,
                    m,
                    Self::sig_str_full(&abs_m.decl, impl_disp),
                    cn
                ),
                impl_m.decl.line,
            );
            e.line = impl_m.decl.line;
            if impl_m.decl.file != self.diag_file_shared() {
                self.last_err_file = impl_m.decl.file.to_string();
            }
            return Some(e);
        }
        // Zend cites the implementing method's declaration — for merged
        // trait methods that's the trait's own file/line (bug81192).
        let mut e = PhpError::fatal(
            format!(
                "Declaration of {}::{}{} must be compatible with {}::{}{}",
                impl_disp,
                m,
                Self::sig_str_full(&impl_m.decl, impl_disp),
                abs_disp,
                m,
                Self::sig_str_full(&abs_m.decl, impl_disp)
            ),
            impl_m.decl.line,
        );
        e.line = impl_m.decl.line;
        if impl_m.decl.file != self.diag_file_shared() {
            self.last_err_file = impl_m.decl.file.to_string();
        }
        Some(e)
    }

    /// Interface method signatures must be compatible with the class's
    /// implementation (bug60153): same rules as trait abstracts.
    fn check_interface_sigs(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        // (iface, display-for-errors): own `implements` cites the
        // interface; an ancestor's requirement cites the ancestor
        // (bug62358).
        let mut ifaces: Vec<(Rc<ClassDecl>, String)> = Vec::new();
        for iname in &d.implements {
            if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                ifaces.push((f, String::new()));
            }
        }
        let mut chain: Vec<Rc<ClassDecl>> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.clone());
            pn = pc.decl.parent.clone();
        }
        for c in &chain {
            for iname in &c.implements {
                if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                    ifaces.push((f, c.name.clone()));
                }
            }
        }
        let mut seen = 0;
        while seen < ifaces.len() {
            let (f, disp) = ifaces[seen].clone();
            seen += 1;
            let cite = if disp.is_empty() {
                f.name.clone()
            } else {
                disp
            };
            for im in &f.methods {
                if let Some(impl_m) = d
                    .methods
                    .iter()
                    .find(|m| m.decl.name.eq_ignore_ascii_case(&im.decl.name))
                    .cloned()
                {
                    // Interface methods must stay public in the
                    // implementation (bug69467).
                    if !matches!(impl_m.visibility, crate::ast::Visibility::Public) {
                        return Err(PhpError::fatal(
                            format!(
                                "Access level to {}::{}() must be public (as in class {})",
                                d.name, im.decl.name, cite
                            ),
                            impl_m.decl.line,
                        ));
                    }
                    if !self.builtin_ifaces.contains(&f.name.to_lowercase()) {
                        if let Some(e) = self.trait_sig_error(&impl_m, im, &d.name, &cite, false) {
                            return Err(e);
                        }
                    }
                }
            }
            for p2 in &f.implements {
                if let Some(pp) = self.interfaces.get(&p2.to_lowercase()).cloned() {
                    ifaces.push((pp, String::new()));
                }
            }
        }
        Ok(())
    }

    /// Non-abstract classes must implement every abstract method: own/
    /// trait-merged abstracts (labelled `C::m`), plus abstracts from
    /// ancestor classes and interfaces (labelled `Src::m`).
    fn check_abstract_methods(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        if d.kind != crate::ast::ClassKind::Class {
            return Ok(());
        }
        // A private abstract requirement can't be delegated to a
        // subclass — the composing class itself must implement it,
        // even when abstract (abstract_method_6).
        let priv_missing: Vec<String> = d
            .methods
            .iter()
            .filter(|m| {
                m.is_abstract
                    && m.visibility == crate::ast::Visibility::Private
                    && m.decl.decl_in.is_some()
            })
            .map(|m| format!("{}::{}", d.name, m.decl.name))
            .collect();
        if !priv_missing.is_empty() {
            let n = priv_missing.len();
            return Err(PhpError::fatal(
                format!(
                    "Class {} must implement {} abstract method{} ({})",
                    d.name,
                    n,
                    if n == 1 { "" } else { "s" },
                    priv_missing.join(", ")
                ),
                self.cur_line,
            ));
        }
        // A class declaring abstract methods itself must be marked
        // abstract — Zend checks this at the class's own compile with
        // a different message than the unimplemented-inherited one
        // ('declares abstract method m()', first own-declared wins).
        if !d.is_abstract {
            if let Some(m) = d
                .methods
                .iter()
                .find(|m| m.is_abstract && m.decl.decl_in.is_none())
            {
                return Err(PhpError::fatal(
                    format!(
                        "Class {} declares abstract method {}() and must therefore be declared abstract",
                        d.name, m.decl.name
                    ),
                    self.cur_line,
                ));
            }
        }
        if d.is_abstract {
            return Ok(());
        }
        // Concrete impls visible to this class: own methods (incl.
        // trait-merged) plus ancestor classes' methods.
        let mut chain: Vec<Rc<ClassDecl>> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.clone());
            pn = pc.decl.parent.clone();
        }
        // `end` = ancestors chain[0..end] that may satisfy the abstract
        // (the declaring link itself plus everything below it).
        let concrete = |lname: &str, end: usize| -> bool {
            if d.methods
                .iter()
                .any(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            {
                return true;
            }
            chain.iter().take(end).any(|c| {
                c.methods
                    .iter()
                    .any(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            })
        };
        let mut missing: Vec<String> = Vec::new();
        // Own + trait-merged abstracts first (label: this class).
        for m in &d.methods {
            if m.is_abstract {
                let lname = m.decl.name.to_lowercase();
                if concrete(&lname, chain.len()) {
                    continue;
                }
                let label = format!("{}::{}", d.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
        // Ancestor abstracts: an impl must also be signature-compat
        // (bug62358) — find it in this class or a descendant link.
        for (i, c) in chain.iter().enumerate() {
            for m in &c.methods {
                if !m.is_abstract {
                    continue;
                }
                let impl_at =
                    |m: &Rc<MethodDecl>| -> Option<(Rc<MethodDecl>, String)> {
                        if let Some(x) = d.methods.iter().find(|x| {
                            x.decl.name.eq_ignore_ascii_case(&m.decl.name) && !x.is_abstract
                        }) {
                            return Some((x.clone(), d.name.clone()));
                        }
                        chain.iter().take(i + 1).find_map(|c2| {
                            c2.methods
                                .iter()
                                .find(|x| {
                                    x.decl.name.eq_ignore_ascii_case(&m.decl.name) && !x.is_abstract
                                })
                                .map(|x| (x.clone(), c2.name.clone()))
                        })
                    };
                if let Some((im, iname)) = impl_at(m) {
                    if let Some(e) = self.trait_sig_error(&im, m, &iname, &c.name, false) {
                        let mut e = e;
                        e.line = im.decl.line;
                        return Err(e);
                    }
                    continue;
                }
                let label = format!("{}::{}", c.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
        // Interfaces implemented anywhere in the chain (including
        // this class's own `implements`).
        let mut ifaces: Vec<Rc<ClassDecl>> = Vec::new();
        for iname in d
            .implements
            .iter()
            .chain(chain.iter().flat_map(|c| c.implements.iter()))
        {
            if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                ifaces.push(f);
            }
        }
        let mut seen = 0;
        while seen < ifaces.len() {
            let f = ifaces[seen].clone();
            seen += 1;
            for m in &f.methods {
                // Interface methods are implicitly abstract.
                let lname = m.decl.name.to_lowercase();
                if concrete(&lname, chain.len()) {
                    continue;
                }
                let label = format!("{}::{}", f.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
            for p2 in &f.implements {
                if let Some(pp) = self.interfaces.get(&p2.to_lowercase()).cloned() {
                    ifaces.push(pp);
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let (n, list) = (missing.len(), missing.join(", "));
        Err(PhpError::fatal(
            format!(
                "Class {} contains {} abstract method{} and must therefore be declared abstract or implement the remaining method{} ({})",
                d.name,
                n,
                if n == 1 { "" } else { "s" },
                if n == 1 { "" } else { "s" },
                list
            ),
            self.cur_line,
        ))
    }

    /// Resolve `self`/`static`/`parent` members against a declaring
    /// class given only its name + parent name (variance checks run
    /// while the child class is still mid-registration).
    fn ty_scope_resolve(&self, ty: &[String], name: &str, parent: &Option<String>) -> Vec<String> {
        ty.iter()
            .map(|m| match m.to_lowercase().as_str() {
                "self" | "static" => name.to_string(),
                "parent" => parent.clone().unwrap_or_else(|| "\\0parent".to_string()),
                _ => m.clone(),
            })
            .collect()
    }

    /// `iterable` ≡ `Traversable|array` for type-set comparisons.
    fn ty_expand_iterable(ty: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(ty.len() + 1);
        for m in ty {
            if m.eq_ignore_ascii_case("iterable") {
                out.push("Traversable".into());
                out.push("array".into());
            } else {
                out.push(m.clone());
            }
        }
        out
    }

    /// Member coverage: `covers(big, small)` — every value matching
    /// `small` also matches `big`. Drives semantic type equality for
    /// prop variance (union_types/variance/valid).
    fn ty_covers(&mut self, big: &str, small: &str) -> bool {
        let bl = big.to_lowercase();
        let sl = small.to_lowercase();
        if bl == sl {
            return true;
        }
        let b_inner = big.trim_start_matches('(').trim_end_matches(')');
        let s_inner = small.trim_start_matches('(').trim_end_matches(')');
        if b_inner.contains('&') {
            // `small ⊆ B1&B2` iff every conjunct of big is covered by
            // some conjunct of small (`B&A` ⊆ `A&B` — commutative).
            let sparts: Vec<&str> = s_inner.split('&').collect();
            return b_inner
                .split('&')
                .all(|b| sparts.iter().any(|p| self.ty_covers(b, p)));
        }
        if s_inner.contains('&') {
            // `A&B` ⊆ anything covering one of its parts.
            return s_inner.split('&').any(|p| self.ty_covers(big, p));
        }
        const SCALARS: &[&str] = &[
            "int", "float", "string", "bool", "array", "callable", "object", "mixed", "void",
            "never", "null", "false", "true", "iterable", "resource", "numeric",
        ];
        match bl.as_str() {
            "mixed" => true,
            "bool" => sl == "false" || sl == "true",
            "float" => sl == "int",
            "object" => !SCALARS.contains(&sl.as_str()),
            "callable" => sl == "closure" || sl == "callable",
            _ => {
                if SCALARS.contains(&sl.as_str()) || SCALARS.contains(&bl.as_str()) {
                    false
                } else {
                    self.is_a_str(&sl, &bl)
                }
            }
        }
    }

    /// `final` props/hooks may not be overridden by a subclass.
    fn check_final_override(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let mut an = d.parent.clone();
        while let Some(pname) = an {
            let Some(pc) = self.classes.get(&pname.to_lowercase()).cloned() else {
                break;
            };
            for cm in &d.methods {
                let Some(am) = pc
                    .decl
                    .methods
                    .iter()
                    .find(|x| x.decl.name.eq_ignore_ascii_case(&cm.decl.name))
                else {
                    continue;
                };
                if am.is_final && !cm.is_abstract {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot override final method {}::{}()",
                            pc.decl.name, cm.decl.name
                        ),
                        self.cur_line,
                    ));
                }
            }
            for cc in &d.consts {
                let Some(ac) = pc.decl.consts.iter().find(|x| x.name == cc.name) else {
                    continue;
                };
                if ac.is_final {
                    return Err(PhpError::fatal(
                        format!(
                            "{}::{} cannot override final constant {}::{}",
                            d.name, cc.name, pc.decl.name, cc.name
                        ),
                        self.cur_line,
                    ));
                }
            }
            for cp in &d.props {
                let Some(ap) =
                    pc.decl.props.iter().find(|x| {
                        x.name == cp.name && x.visibility != crate::ast::Visibility::Private
                    })
                else {
                    continue;
                };
                if ap.is_final {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot override final property {}::${}",
                            pc.decl.name, cp.name
                        ),
                        self.cur_line,
                    ));
                }
                // zend's prop-redeclare order after final: static-ness,
                // readonly, then visibility; type invariance last.
                if ap.is_static != cp.is_static {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot redeclare {} {}::${} as {} {}::${}",
                            if ap.is_static { "static" } else { "non static" },
                            pc.decl.name,
                            cp.name,
                            if cp.is_static { "static" } else { "non static" },
                            d.name,
                            cp.name
                        ),
                        self.cur_line,
                    ));
                }
                if ap.readonly != cp.readonly {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot redeclare {} property {}::${} as {} {}::${}",
                            if ap.readonly {
                                "readonly"
                            } else {
                                "non-readonly"
                            },
                            pc.decl.name,
                            cp.name,
                            if cp.readonly {
                                "readonly"
                            } else {
                                "non-readonly"
                            },
                            d.name,
                            cp.name
                        ),
                        self.cur_line,
                    ));
                }
                let vis_rank = |v: &crate::ast::Visibility| match v {
                    crate::ast::Visibility::Private => 0,
                    crate::ast::Visibility::Protected => 1,
                    crate::ast::Visibility::Public => 2,
                };
                if vis_rank(&cp.visibility) < vis_rank(&ap.visibility) {
                    // ap is never private (filtered above): public →
                    // 'must be public'; protected → 'or weaker'.
                    let msg = if ap.visibility == crate::ast::Visibility::Protected {
                        format!(
                            "Access level to {}::${} must be protected (as in class {}) or weaker",
                            d.name, cp.name, pc.decl.name
                        )
                    } else {
                        format!(
                            "Access level to {}::${} must be public (as in class {})",
                            d.name, cp.name, pc.decl.name
                        )
                    };
                    return Err(PhpError::fatal(msg, self.cur_line));
                }
                // Property types are invariant across inheritance
                // only for *backed* props — a virtual hook pair
                // follows per-kind signature variance instead
                // (backed_invariant vs override_add_get_contravariant).
                let backed = ap.hooks.is_none()
                    || Self::prop_is_backed(ap)
                    || cp.hooks.is_none()
                    || Self::prop_is_backed(cp);
                // Prop types are invariant but compared SEMANTICALLY:
                // `X|Y` ≡ `X` when Y extends X (dropping a member
                // subsumed by another is identity), and `iterable`
                // expands to `Traversable|array` (union variance
                // valid.phpt). Untyped props stay exact `==`.
                let mut ty_equiv =
                    |ct: &Option<Vec<String>>, at: &Option<Vec<String>>| match (ct, at) {
                        (None, None) => true,
                        (Some(c), Some(a)) => {
                            let ce = self.ty_scope_resolve(c, &d.name, &d.parent);
                            let ae = self.ty_scope_resolve(a, &pc.decl.name, &pc.decl.parent);
                            let ce = Self::ty_expand_iterable(&ce);
                            let ae = Self::ty_expand_iterable(&ae);
                            ce.iter().all(|c| ae.iter().any(|a| self.ty_covers(a, c)))
                                && ae.iter().all(|a| ce.iter().any(|c| self.ty_covers(c, a)))
                        }
                        _ => false,
                    };
                if backed && !ty_equiv(&cp.ty, &ap.ty) {
                    // Child re-types an untyped parent prop → "must be
                    // omitted"; typed-vs-typed mismatch → "must be T".
                    if ap.ty.is_none() && cp.ty.is_some() {
                        return Err(PhpError::fatal(
                            format!(
                                "Type of {}::${} must be omitted to match the parent definition in class {}",
                                d.name, cp.name, pc.decl.name
                            ),
                            self.cur_line,
                        ));
                    }
                    let aty = ap
                        .ty
                        .as_ref()
                        .map(|m| {
                            m.iter()
                                .map(|t| {
                                    if t.contains('&') && m.len() > 1 {
                                        format!("({})", t)
                                    } else {
                                        t.clone()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .unwrap_or_else(|| "mixed".into());
                    return Err(PhpError::fatal(
                        format!(
                            "Type of {}::${} must be {} (as in class {})",
                            d.name, cp.name, aty, pc.decl.name
                        ),
                        self.cur_line,
                    ));
                }
                if let (Some(chs), Some(ahs)) = (&cp.hooks, &ap.hooks) {
                    for ch in chs {
                        if ahs.iter().any(|ah| ah.is_get == ch.is_get && ah.is_final) {
                            let kind = if ch.is_get { "get" } else { "set" };
                            return Err(PhpError::fatal(
                                format!(
                                    "Cannot override final property hook {}::${}::{}()",
                                    pc.decl.name, cp.name, kind
                                ),
                                self.cur_line,
                            ));
                        }
                        let Some(ah) = ahs.iter().find(|ah| ah.is_get == ch.is_get) else {
                            continue;
                        };
                        // Hook signature variance: a get's return type (the
                        // prop type) is covariant; a set's $value parameter
                        // is contravariant (type_compatibility*).
                        let fmt = |t: &Option<Vec<String>>| {
                            t.as_ref()
                                .map(|m| m.join("|"))
                                .unwrap_or_else(|| "mixed".into())
                        };
                        if ch.is_get {
                            // A parent `&get` requires the child's get to
                            // return by reference too; a child `&get`
                            // under a plain parent get is fine
                            // (interface_get_value_as_ref).
                            if ah.by_ref && !ch.by_ref {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::get() must be compatible with & {}::${}::get()",
                                        d.name, cp.name, pc.decl.name, cp.name
                                    ),
                                    ch.line,
                                ));
                            }
                            let (cty, aty) = (fmt(&cp.ty), fmt(&ap.ty));
                            let cm = cp.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                            let am = ap.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                            if !self.ty_sup(&am, &cm) {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::get(): {} must be compatible with {}::${}::get(): {}",
                                        d.name, cp.name, cty, pc.decl.name, cp.name, aty
                                    ),
                                    ch.line,
                                ));
                            }
                        } else {
                            let eff = |pd: &crate::ast::PropDecl, h: &crate::ast::PropHook| {
                                h.params
                                    .first()
                                    .and_then(|sp| sp.ty.clone())
                                    .or_else(|| pd.ty.clone())
                                    .unwrap_or_else(|| vec!["mixed".into()])
                            };
                            let cm = eff(cp, ch);
                            let am = eff(ap, ah);
                            let cn = ch
                                .params
                                .first()
                                .map(|sp| sp.name.clone())
                                .unwrap_or_else(|| "value".into());
                            let an = ah
                                .params
                                .first()
                                .map(|sp| sp.name.clone())
                                .unwrap_or_else(|| "value".into());
                            if !self.ty_sup(&cm, &am) {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::set({} ${}): void must be compatible with {}::${}::set({} ${}): void",
                                        d.name, cp.name, cm.join("|"), cn,
                                        pc.decl.name, cp.name, am.join("|"), an
                                    ),
                                    ch.line,
                                ));
                            }
                        }
                    }
                }
            }
            an = pc.decl.parent.clone();
        }
        // Interface prop hooks also constrain the implementation's
        // signatures (get_by_ref_implemented_by_val: `&get;` in the
        // interface requires `&get` in the class).
        for iname in &d.implements {
            let Some(iface) = self.interfaces.get(&iname.to_lowercase()).cloned() else {
                continue;
            };
            for cp in &d.props {
                let Some(ap) = iface.props.iter().find(|x| x.name == cp.name) else {
                    continue;
                };
                let (Some(chs), Some(ahs)) = (&cp.hooks, &ap.hooks) else {
                    continue;
                };
                for ch in chs {
                    let Some(ah) = ahs.iter().find(|ah| ah.is_get == ch.is_get) else {
                        continue;
                    };
                    if ch.is_get && ah.by_ref && !ch.by_ref {
                        return Err(PhpError::fatal(
                            format!(
                                "Declaration of {}::${}::get() must be compatible with & {}::${}::get()",
                                d.name, cp.name, iface.name, cp.name
                            ),
                            cp.line,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// PHP 8.4 hooked-property decl checks (property_hooks tests): hooks
    /// are forbidden on static/readonly props; a default requires a
    /// *backed* prop; `set(T)` must be type-compatible; `final`/`abstract`
    /// and interface restrictions produce link-time fatals.
    fn check_hooked_props(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let in_iface = d.kind == ClassKind::Interface;
        for p in &d.props {
            if in_iface && p.is_abstract {
                return Err(PhpError::fatal(
                    "Property in interface cannot be explicitly abstract. All interface members are implicitly abstract",
                    self.cur_line,
                ));
            }
            if in_iface && p.visibility != crate::ast::Visibility::Public {
                return Err(PhpError::fatal(
                    "Property in interface cannot be protected or private",
                    self.cur_line,
                ));
            }
            if in_iface && p.is_final {
                return Err(PhpError::fatal(
                    "Property in interface cannot be final",
                    self.cur_line,
                ));
            }
            if p.is_abstract && p.hooks.is_none() {
                return Err(PhpError::fatal(
                    "Only hooked properties may be declared abstract",
                    self.cur_line,
                ));
            }
            if p.is_abstract && p.is_final {
                return Err(PhpError::fatal(
                    "Cannot use the final modifier on an abstract property",
                    self.cur_line,
                ));
            }
            if p.is_final && p.visibility == crate::ast::Visibility::Private {
                return Err(PhpError::fatal(
                    "Property cannot be both final and private",
                    self.cur_line,
                ));
            }
            // Untyped readonly faults before the hooked-property rules
            // (the same message a plain readonly prop gets).
            if p.readonly && p.ty.is_none() {
                return Err(PhpError::fatal(
                    format!("Readonly property {}::${} must have type", d.name, p.name),
                    self.cur_line,
                ));
            }
            // `readonly $p = v` — 'cannot have default value' outranks
            // the static-readonly and hook rules below (m11b/m11j).
            // zend attributes it to the prop's decl line, not the
            // class's.
            if p.readonly && p.default.is_some() {
                return Err(PhpError::fatal(
                    format!(
                        "Readonly property {}::${} cannot have default value",
                        d.name, p.name
                    ),
                    p.line.max(1),
                ));
            }
            // `static readonly` — zend's own decl fatal; it outranks
            // the hook rules but loses to 'must have type' above.
            if p.readonly && p.is_static {
                return Err(PhpError::fatal(
                    format!("Static property {}::${} cannot be readonly", d.name, p.name),
                    self.cur_line,
                ));
            }
            let Some(hs) = &p.hooks else { continue };
            // readonly classes forbid hooked props entirely, whether
            // declared or ctor-promoted (gh15419_1, gh15419_2).
            if d.readonly {
                return Err(PhpError::fatal(
                    "Hooked properties cannot be readonly",
                    self.cur_line,
                ));
            }
            if p.is_abstract && hs.iter().all(|h| h.body.is_some()) {
                return Err(PhpError::fatal(
                    format!(
                        "Abstract property {}::${} must specify at least one abstract hook",
                        d.name, p.name
                    ),
                    self.cur_line,
                ));
            }
            for h in hs {
                if let Some(v) = h.visibility {
                    let vn = match v {
                        crate::ast::Visibility::Public => "public",
                        crate::ast::Visibility::Protected => "protected",
                        crate::ast::Visibility::Private => "private",
                    };
                    return Err(PhpError::fatal(
                        format!("Cannot use the {} modifier on a property hook", vn),
                        self.cur_line,
                    ));
                }
                if h.is_final && p.visibility == crate::ast::Visibility::Private {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both final and private",
                        self.cur_line,
                    ));
                }
                if h.is_final && (in_iface || h.body.is_none()) {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both abstract and final",
                        self.cur_line,
                    ));
                }
                if h.body.is_none() && p.visibility == crate::ast::Visibility::Private {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both abstract and private",
                        self.cur_line,
                    ));
                }
                if h.is_get && (h.has_plist || !h.params.is_empty()) {
                    return Err(PhpError::fatal(
                        format!(
                            "get hook of property {}::${} must not have a parameter list",
                            d.name, p.name
                        ),
                        self.cur_line,
                    ));
                }
                if !h.is_get {
                    if let Some(sp) = h.params.first() {
                        if sp.default.is_some() {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not have a default value",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                        if sp.by_ref {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not be pass-by-reference",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                        if sp.variadic {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not be variadic",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                    }
                }
            }
            if p.is_static {
                return Err(PhpError::fatal(
                    "Cannot declare hooks for static property",
                    self.cur_line,
                ));
            }
            if p.readonly {
                return Err(PhpError::fatal(
                    "Hooked properties cannot be readonly",
                    self.cur_line,
                ));
            }
            if p.default.is_some()
                && !Self::prop_is_backed(p)
                && !self.chain_prop_backed(d, &p.name)
            {
                return Err(PhpError::fatal(
                    format!(
                        "Cannot specify default value for virtual hooked property {}::${}",
                        d.name, p.name
                    ),
                    self.cur_line,
                ));
            }
            // `&get` alongside `set` is legal only on a *virtual* prop —
            // when the hooks (or a plain ancestor decl) back the property
            // the engine can't reconcile the returned reference with set
            // writes (get_by_ref_virtual vs get_by_ref_backed).
            let backed = Self::prop_is_backed(p) || {
                let mut par = d.parent.clone();
                let mut found = false;
                while let Some(pname) = par {
                    let Some(pc) = self.classes.get(&pname.to_lowercase()) else {
                        break;
                    };
                    if pc.decl.props.iter().any(|p2| {
                        p2.name == p.name
                            && p2.hooks.is_none()
                            && p2.visibility != crate::ast::Visibility::Private
                    }) {
                        found = true;
                        break;
                    }
                    par = pc.decl.parent.clone();
                }
                found
            };
            if backed && hs.iter().any(|h| !h.is_get) && hs.iter().any(|h| h.is_get && h.by_ref) {
                return Err(PhpError::fatal(
                    format!(
                        "Get hook of backed property {}::{} with set hook may not return by reference",
                        d.name, p.name
                    ),
                    p.line,
                ));
            }
            // A hook without a body is only legal in an interface or on
            // a prop declared `abstract`.
            let abs_ok = d.kind == ClassKind::Interface || p.is_abstract;
            if !abs_ok && hs.iter().any(|h| h.body.is_none()) {
                return Err(PhpError::fatal(
                    "Non-abstract property hook must have a body",
                    self.cur_line,
                ));
            }
            if let Some(set) = hs.iter().find(|h| !h.is_get) {
                if let Some(sp) = set.params.first() {
                    // The set $value parameter must accept every value the
                    // property type admits (param type ⊇ prop type); an
                    // untyped prop is mixed, and an untyped parameter is
                    // only legal on an untyped prop
                    // (set_value_parameter_type_variance_005).
                    let compat = match (&p.ty, &sp.ty) {
                        (None, None) => true,
                        (pty, Some(sty)) => {
                            let pty = pty.clone().unwrap_or_else(|| vec!["mixed".to_string()]);
                            self.ty_sup(sty, &pty)
                        }
                        (Some(_), None) => false,
                    };
                    if !compat {
                        // Zend reports this on the hook's own line, except
                        // when a type name wasn't resolvable at check
                        // time (later-declared class-likes): then the
                        // verdict lands on the class-decl line
                        // (set_value_parameter_type_variance_003).
                        let mut involved =
                            p.ty.iter()
                                .flatten()
                                .chain(sp.ty.iter().flatten())
                                .flat_map(|t| t.split(['|', '&']).map(str::trim))
                                .filter(|t| !t.is_empty());
                        let line = if involved.any(|t| !self.ty_member_registered(t)) {
                            d.line
                        } else {
                            set.line
                        };
                        return Err(PhpError::fatal(
                            format!(
                                "Type of parameter ${} of hook {}::${}::set must be compatible with property type",
                                sp.name, d.name, p.name
                            ),
                            line,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// First type member naming a class that can't be resolved even
    /// after an autoload attempt — builtins, `self`/`parent`/`static`,
    /// registered classes/interfaces, and names on the linking stack
    /// all count as resolvable (variance/mixed_return_type: members
    /// covered without resolution never reach here).
    fn first_unres(&mut self, tys: &[String]) -> Option<String> {
        const BUILTIN_T: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null", "resource", "self", "parent", "static",
        ];
        tys.iter()
            .flat_map(|m| m.split('&').map(str::to_string).collect::<Vec<_>>())
            .find(|m| {
                let ml = m.trim_start_matches('\\').to_lowercase();
                if BUILTIN_T.contains(&ml.as_str())
                    || self.classes.contains_key(&ml)
                    || self.interfaces.contains_key(&ml)
                    || self
                        .declaring
                        .iter()
                        .any(|d| d.name.eq_ignore_ascii_case(&ml))
                    || self
                        .linking
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&ml))
                {
                    return false;
                }
                self.note_variance_obligation();
                if let Err(e) = self.run_autoload(m.trim_start_matches('\\')) {
                    self.sig_fatal.get_or_insert(e);
                    self.pending_exception = None;
                }
                !self.classes.contains_key(&ml) && !self.interfaces.contains_key(&ml)
            })
            .map(|m| m.trim_start_matches('\\').to_string())
    }

    /// `sup` is a supertype of `sub` when every `sub` member is admitted
    /// by some `sup` member — equal names, `mixed`, or a class/interface
    /// the member is-a (set_value_parameter_type_variance_006).
    fn ty_sup(&mut self, sup: &[String], sub: &[String]) -> bool {
        sub.iter()
            .all(|t| sup.iter().any(|s| self.ty_member_is_a(t, s)))
    }

    /// Whether every type name a decl's members mention is already
    /// resolvable — zend prevents early binding when prop/method/const
    /// signature types can't be checked at compile time, deferring the
    /// link (and its variance verdicts) to exec
    /// (property_types_early_bind, enum_forward_compat).
    pub(in crate::interp) fn decl_types_resolvable(&self, d: &ClassDecl) -> bool {
        let mut tys = Vec::new();
        for p in &d.props {
            if let Some(t) = &p.ty {
                tys.extend(t.iter().cloned());
            }
            if let Some(hs) = &p.hooks {
                for h in hs {
                    for sp in &h.params {
                        if let Some(t) = &sp.ty {
                            tys.extend(t.iter().cloned());
                        }
                    }
                }
            }
        }
        for m in &d.methods {
            for sp in &m.decl.params {
                if let Some(t) = &sp.ty {
                    tys.extend(t.iter().cloned());
                }
            }
            if let Some(t) = &m.decl.ret {
                tys.extend(t.iter().cloned());
            }
        }
        for cd in &d.consts {
            if let Some(t) = &cd.ty {
                tys.extend(t.iter().cloned());
            }
        }
        tys.iter()
            .flat_map(|t| t.split('&'))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .all(|t| {
                t.trim_start_matches('\\').eq_ignore_ascii_case(&d.name)
                    || self.ty_member_registered(t)
            })
    }

    /// Whether a type member already names a registered class-like —
    /// the compile-vs-link distinction Zend's prop-type hook check
    /// uses to pick its error line (no autoload, just a lookup).
    fn ty_member_registered(&self, t: &str) -> bool {
        let tl = t.trim_start_matches('\\').to_lowercase();
        const BUILTIN: &[&str] = &[
            "int",
            "float",
            "string",
            "bool",
            "array",
            "object",
            "callable",
            "iterable",
            "mixed",
            "void",
            "never",
            "false",
            "true",
            "null",
            "numeric",
            "resource",
            "self",
            "static",
            "parent",
            "closure",
            "traversable",
            "iterator",
            "generator",
        ];
        BUILTIN.contains(&tl.as_str())
            || self.classes.contains_key(&tl)
            || self.interfaces.contains_key(&tl)
            || self.traits.contains_key(&tl)
            || self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&tl))
            || self
                .linking
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(&tl))
    }

    /// Whether a single type conjunct resolves to a registered (or
    /// mid-linking / autoloadable) class-like name or builtin. Used
    /// to gate `&`-member coverage of `object`/`iterable`/`callable`.
    fn ty_conj_resolvable(&mut self, c: &str) -> bool {
        let cl = c.to_lowercase();
        const BUILTIN: &[&str] = &[
            "int",
            "float",
            "string",
            "bool",
            "array",
            "object",
            "callable",
            "iterable",
            "mixed",
            "void",
            "never",
            "false",
            "true",
            "null",
            "numeric",
            "resource",
            "self",
            "static",
            "parent",
            "closure",
            "traversable",
            "iterator",
            "generator",
        ];
        if BUILTIN.contains(&cl.as_str()) {
            return true;
        }
        if self.classes.contains_key(&cl)
            || self.interfaces.contains_key(&cl)
            || self.traits.contains_key(&cl)
            || self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&cl))
            || self.linking.iter().any(|d| d.name.to_lowercase() == cl)
        {
            return true;
        }
        // Autoload errors must not surface here — resolvability is a
        // yes/no probe (invalid4 "could not check" is raised by the
        // caller). Preserve any pre-existing pending exception; a fatal
        // still propagates through sig_fatal.
        self.note_variance_obligation();
        let prior = self.pending_exception.take();
        if let Err(e) = self.run_autoload(c) {
            self.sig_fatal.get_or_insert(e);
        }
        self.pending_exception = prior;
        self.classes.contains_key(&cl)
            || self.interfaces.contains_key(&cl)
            || self.traits.contains_key(&cl)
    }

    /// Type-member acceptance: `t` is admitted by `s` when they match by
    /// name, `s` is `mixed`, or `t`'s class/interface ancestry includes
    /// `s` (interfaces live in `self.interfaces`, not `self.classes`).
    pub(in crate::interp) fn ty_member_is_a(&mut self, t: &str, s: &str) -> bool {
        self.ty_member_is_a_impl(t, s, false)
    }

    /// Strict membership for ref-bind merges — `int ⊄ float` (scalar
    /// coercions don't apply to declared-type sets, prop_ref_assign).
    pub(in crate::interp) fn ty_member_is_a_strict(&mut self, t: &str, s: &str) -> bool {
        self.ty_member_is_a_impl(t, s, true)
    }

    fn ty_member_is_a_impl(&mut self, t: &str, s: &str, strict: bool) -> bool {
        let tl0 = t.to_lowercase();
        let sl0 = s.to_lowercase();
        // `never` is the bottom type (subtype of everything). `void`
        // and `never` match only themselves — `void` is NOT a subtype
        // of `mixed` (mixed_return_inheritance_error1), and nothing
        // but `never` is a subtype of `never`.
        if tl0 == "never" {
            return true;
        }
        if tl0 == "void" || sl0 == "void" || sl0 == "never" {
            return tl0 == sl0;
        }
        if s.eq_ignore_ascii_case(t) || s.eq_ignore_ascii_case("mixed") {
            return true;
        }
        if s.contains('&') {
            // `t ⊆ S1&S2&…` iff every conjunct of s is covered by some
            // conjunct of t (`A&B&C` is a subtype of `A&B`).
            let tparts: Vec<&str> = t.split('&').collect();
            return s.split('&').all(|sc| {
                tparts
                    .iter()
                    .any(|tc| self.ty_member_is_a_impl(tc, sc, strict))
            });
        }
        if t.contains('&') {
            // `C1&C2 ⊆ s` when some conjunct already is-a `s` — but a
            // conjunct covering `object`/`iterable`/`callable` must be
            // a resolvable class-like name; an unloadable conjunct
            // can't prove the member is object-like (invalid4).
            return t.split('&').any(|sm| {
                let atomish =
                    ["object", "iterable", "callable"].contains(&s.to_lowercase().as_str());
                if atomish && !self.ty_conj_resolvable(sm) {
                    return false;
                }
                self.ty_member_is_a_impl(sm, s, strict)
            });
        }
        if t.eq_ignore_ascii_case("null") || t.eq_ignore_ascii_case("mixed") {
            return false;
        }
        let tl = t.to_lowercase();
        let sl = s.to_lowercase();
        if sl == "iterable"
            && (["array", "traversable", "iterator", "generator"].contains(&tl.as_str())
                || self.is_a_str(&tl, "traversable"))
        {
            return true;
        }
        if sl == "callable" && tl == "closure" {
            return true;
        }
        if sl == "object" && tl == "closure" {
            return true;
        }
        if !strict && sl == "float" && tl == "int" {
            return true;
        }
        if sl == "object" {
            const SCALARS: &[&str] = &[
                "int", "float", "string", "bool", "array", "callable", "iterable", "void", "never",
                "null", "false", "true", "resource", "numeric",
            ];
            if !SCALARS.contains(&tl.as_str()) {
                // A non-scalar member covers `object` only when it
                // actually resolves to a class-like — Zend autoloads
                // it to verify (enum_forward_compat).
                return self.ty_conj_resolvable(t);
            }
        }
        if sl == "bool" && (tl == "true" || tl == "false") {
            return true;
        }
        // A class declaring __toString implicitly implements
        // Stringable for variance (variance/stringable).
        if sl == "stringable"
            && (self
                .lookup_class(t)
                .map(|c| self.find_method_in(&c, "__tostring").is_some())
                .unwrap_or(false)
                || self.linking.iter().any(|c| {
                    c.name.eq_ignore_ascii_case(t)
                        && c.methods
                            .iter()
                            .any(|mm| mm.decl.name.eq_ignore_ascii_case("__tostring"))
                }))
        {
            return true;
        }
        if let Some(iface) = self.interfaces.get(&tl).cloned() {
            let mut stack = vec![iface];
            while let Some(f) = stack.pop() {
                for p in &f.implements {
                    if p.eq_ignore_ascii_case(s) {
                        return true;
                    }
                    if let Some(ff) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(ff);
                    }
                }
            }
            return false;
        }
        self.is_a_str(t, s)
    }

    /// Typed class constants (PHP 8.3): forbidden members, the
    /// declared-value check (strict — no coercion), and the
    /// inheritance variance rule (child ⊆ parent when the parent side
    /// declares a type; private consts exempt).
    fn check_const_types(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        for cd in &d.consts {
            let Some(ty) = &cd.ty else { continue };
            for m in ty {
                let l = m.to_lowercase();
                if ["callable", "void", "never"].contains(&l.as_str()) {
                    return Err(PhpError::fatal(
                        format!(
                            "Class constant {}::{} cannot have type {}",
                            d.name, cd.name, m
                        ),
                        self.cur_line,
                    ));
                }
            }
            // Compile-time values: fatal now. Runtime values (define'd
            // consts, `new`) defer to the access-time TypeError below.
            if is_compile_const(&cd.value) {
                if let Ok(v) = self.eval_decl_const(&cd.value, &d.file, cd.line) {
                    if !self.const_ty_accepts(ty, &v, &d.name) {
                        let tn = self.zval_type_name(&v);
                        return Err(PhpError::fatal(
                            format!(
                                "Cannot use {} as value for class constant {}::{} of type {}",
                                tn,
                                d.name,
                                cd.name,
                                ty_disp(ty)
                            ),
                            self.cur_line,
                        ));
                    }
                }
            }
        }
        for pd in &d.props {
            if let Some(ty) = &pd.ty {
                for m in ty {
                    let l = m.to_lowercase();
                    if ["callable", "void", "never"].contains(&l.as_str()) {
                        return Err(PhpError::fatal(
                            format!(
                                "Property {}::${} cannot have type {}",
                                d.name,
                                pd.name,
                                ty_disp(ty)
                            ),
                            pd.line,
                        ));
                    }
                }
                let ctx = Some((d.name.as_str(), d.parent.clone()));
                self.check_ty_redundant(ty, &ctx)?;
                if let Some(def) = &pd.default {
                    if is_compile_const(def) {
                        if let Ok(v) = self.eval_decl_const(
                            def,
                            &d.file,
                            if pd.dline > 0 { pd.dline } else { pd.line },
                        ) {
                            // `= null` on a non-nullable prop needs `?T`
                            // (typed_properties_015).
                            if matches!(v, Value::Null)
                                && !ty.iter().any(|m| {
                                    m.eq_ignore_ascii_case("null")
                                        || m.eq_ignore_ascii_case("mixed")
                                })
                            {
                                // Implicit nullable is only hinted for
                                // single non-`&` types — unions and
                                // intersections report the plain
                                // "Cannot use null" (bug81268).
                                if ty.len() > 1 || ty.iter().any(|m| m.contains('&')) {
                                    return Err(PhpError::fatal(
                                        format!(
                                            "Cannot use null as default value for property {}::${} of type {}",
                                            d.name,
                                            pd.name,
                                            ty_disp(ty)
                                        ),
                                        pd.line,
                                    ));
                                }
                                let hint = if ty.len() == 1 {
                                    format!("?{}", ty_disp(ty))
                                } else {
                                    format!("{}|null", ty_disp(ty))
                                };
                                return Err(PhpError::fatal(
                                    format!(
                                        "Default value for property of type {} may not be null. Use the nullable type {} to allow null default value",
                                        ty_disp(ty),
                                        hint
                                    ),
                                    pd.line,
                                ));
                            }
                            // int→float is the only allowed widening.
                            if !(self.const_ty_accepts(ty, &v, &d.name)
                                || (matches!(v, Value::Int(_))
                                    && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))))
                            {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Cannot use {} as default value for property {}::${} of type {}",
                                        self.zval_type_name(&v),
                                        d.name,
                                        pd.name,
                                        ty_disp(ty)
                                    ),
                                    pd.line,
                                ));
                            }
                        }
                    }
                }
            }
        }
        // Inheritance variance — parent classes and implemented
        // interfaces' same-name consts constrain this class's.
        let mut supers: Vec<(String, crate::ast::ConstDecl)> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            for cd in &pc.decl.consts {
                if cd.visibility != crate::ast::Visibility::Private {
                    supers.push((pc.decl.name.clone(), cd.clone()));
                }
            }
            pn = pc.decl.parent.clone();
        }
        for i in &d.implements {
            if let Some(id) = self.interfaces.get(&i.to_lowercase()).cloned() {
                for cd in &id.consts {
                    supers.push((id.name.clone(), cd.clone()));
                }
            }
        }
        for cd in &d.consts {
            if cd.visibility == crate::ast::Visibility::Private {
                continue;
            }
            for (sn, scd) in &supers {
                if scd.name != cd.name {
                    continue;
                }
                let Some(pty) = &scd.ty else { continue };
                let ok = match &cd.ty {
                    Some(cty) => self.ty_sup(pty, cty),
                    None => false,
                };
                if !ok {
                    return Err(PhpError::fatal(
                        format!(
                            "Type of {}::{} must be compatible with {}::{} of type {}",
                            d.name,
                            cd.name,
                            sn,
                            cd.name,
                            ty_disp(pty)
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Strict const-value acceptance: any union member (intersections
    /// are flattened to union members by take_type — DNF types in const
    /// positions behave the same for the tests at hand). `self`/`static`
    ////`parent` resolve against the declaring class.
    fn const_ty_accepts(&mut self, ty: &[String], v: &Value, dname: &str) -> bool {
        ty.iter().any(|m| {
            if m.contains('&') {
                return m
                    .split('&')
                    .all(|sm| self.const_ty_accepts(&[sm.to_string()], v, dname));
            }
            let l = m.to_lowercase();
            match l.as_str() {
                "null" => matches!(v, Value::Null),
                "bool" | "true" | "false" => {
                    matches!(v, Value::Bool(b) if l == "bool" || (*b) == (l == "true"))
                }
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_) | Value::Int(_)),
                "string" => matches!(v, Value::Str(_)),
                "array" => matches!(v, Value::Array(_)),
                "object" => matches!(v, Value::Object(_)),
                "iterable" => {
                    matches!(v, Value::Array(_))
                        || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
                }
                "mixed" => true,
                "self" | "static" => match v {
                    Value::Object(o) => self.obj_is_a(o, dname),
                    _ => false,
                },
                "parent" => match v {
                    Value::Object(o) => {
                        let p = self
                            .classes
                            .get(&dname.to_lowercase())
                            .and_then(|c| c.decl.parent.clone());
                        match p {
                            Some(pn) => self.obj_is_a(o, &pn),
                            None => false,
                        }
                    }
                    _ => false,
                },
                _ => match v {
                    Value::Object(o) => self.obj_is_a(o, m),
                    Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                    _ => false,
                },
            }
        })
    }

    /// Zend link semantics: at a class's first use, every const
    /// initializer is evaluated — eval errors (undefined constant)
    /// propagate as Errors, then typed checks raise TypeErrors.
    fn link_const_inits(&mut self, cls: &Rc<PhpClass>) -> Result<(), PhpError> {
        let lname = cls.decl.name.to_lowercase();
        if !self.consts_linked.insert(lname) {
            return Ok(());
        }
        let mut chain = vec![cls.clone()];
        let mut pn = cls.decl.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if !self.consts_linked.insert(pc.decl.name.to_lowercase()) {
                break;
            }
            pn = pc.decl.parent.clone();
            chain.push(pc);
        }
        for c in chain {
            for cd in &c.decl.consts {
                if cd.enum_case {
                    continue;
                }
                let file = c.decl.file.clone();
                // Bind the declaring class while evaluating so `self::X`
                // inside the decl (Enum::MAPPING) resolves correctly.
                let old = self.const_self.replace(c.clone());
                self.class_const_ctx += 1;
                let r = self.eval_decl_const(&cd.value, &file, cd.line);
                self.class_const_ctx -= 1;
                self.const_self = old;
                let v = r?;
                self.const_apply_ty(cd, &c.decl.name, v)?;
            }
        }
        Ok(())
    }

    /// Unit/backed enum case singleton: `E::Foo` is an object of class
    /// E with `name` (+ `value` for backed enums) props; one instance
    /// per case so `===` holds.
    pub(in crate::interp) fn enum_case_value(
        &mut self,
        cls: &str,
        case: &str,
        cd: &crate::ast::ConstDecl,
    ) -> Result<Value, PhpError> {
        let key = format!("{}\0{}", cls.to_lowercase(), case);
        if let Some(v) = self.enum_cases.get(&key) {
            return Ok(v.clone());
        }
        let mut props = std::collections::HashMap::new();
        props.insert("name".to_string(), Value::str(case));
        let is_unit = matches!(cd.value, Expr::Null);
        if !is_unit {
            let ecls = self.classes.get(&cls.to_lowercase()).cloned();
            let file = ecls
                .as_ref()
                .map(|c| c.decl.file.clone())
                .unwrap_or_default();
            // Case values are const slots scoped to the enum —
            // `self::K` resolves to it and `parent::` gets the
            // 'no parent' catchable Error like any class scope.
            let old = ecls.map(|c| self.const_self.replace(c));
            self.class_const_ctx += 1;
            let r = self.eval_decl_const(&cd.value, &file, cd.line);
            self.class_const_ctx -= 1;
            if let Some(o) = old {
                self.const_self = o;
            }
            let v = r?;
            props.insert("value".to_string(), v);
        }
        let o = self.instantiate(cls, &[])?;
        if let Value::Object(h) = &o {
            for (k, v) in props {
                h.borrow_mut().props.insert(k, cell(v));
            }
        }
        self.enum_cases.insert(key, o.clone());
        Ok(o)
    }

    /// Access-time typed-const enforcement: int→float widening, else
    /// a catchable TypeError ("Cannot assign ...").
    pub(in crate::interp) fn const_apply_ty(
        &mut self,
        cd: &crate::ast::ConstDecl,
        owner: &str,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(ty) = &cd.ty else { return Ok(v) };
        let v = if ty.iter().any(|m| m.eq_ignore_ascii_case("float")) {
            match v {
                Value::Int(i) => Value::Float(i as f64),
                _ => v,
            }
        } else {
            v
        };
        if !self.const_ty_accepts(ty, &v, owner) {
            return self.fail(PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to class constant {}::{} of type {}",
                    self.zval_type_name(&v),
                    owner,
                    cd.name,
                    ty_disp(ty)
                ),
                0,
            ));
        }
        Ok(v)
    }

    /// Every visible override must be signature-compatible with the
    /// nearest ancestor method of the same name — not just abstracts;
    /// this also covers trait-merged methods vs concrete ancestors
    /// (bug81192). `__construct` is exempt from LSP rules in PHP.
    fn check_override_sigs(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let mut chain: Vec<(String, Rc<ClassDecl>)> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push((pc.decl.name.clone(), pc.decl.clone()));
            pn = pc.decl.parent.clone();
        }
        if chain.is_empty() {
            return Ok(());
        }
        let rank = |v: &crate::ast::Visibility| match v {
            crate::ast::Visibility::Public => 2,
            crate::ast::Visibility::Protected => 1,
            crate::ast::Visibility::Private => 0,
        };
        for m in &d.methods {
            let lname = m.decl.name.to_lowercase();
            let Some((aname, am)) = chain.iter().find_map(|(pn, pc)| {
                pc.methods
                    .iter()
                    .find(|x| x.decl.name.to_lowercase() == lname)
                    .filter(|x| !matches!(x.visibility, crate::ast::Visibility::Private))
                    .map(|x| (pn.clone(), x.clone()))
            }) else {
                continue;
            };
            // An abstract declaration's contract propagates through
            // intermediate concrete impls — the fatal cites the
            // abstract declarer, not the nearest impl (bug61970_2).
            let (aname, am) = chain
                .iter()
                .find_map(|(pn, pc)| {
                    pc.methods
                        .iter()
                        .find(|x| {
                            x.decl.name.to_lowercase() == lname
                                && x.is_abstract
                                && !matches!(x.visibility, crate::ast::Visibility::Private)
                        })
                        .map(|x| (pn.clone(), x.clone()))
                })
                .unwrap_or((aname, am));
            // `__construct` and private impls escape LSP visibility —
            // except when an ancestor declares the contract abstractly,
            // which the impl must satisfy (bug61970, magic_methods_008).
            if (m.decl.name.eq_ignore_ascii_case("__construct")
                || matches!(m.visibility, crate::ast::Visibility::Private))
                && !am.is_abstract
            {
                continue;
            }
            if rank(&m.visibility) < rank(&am.visibility) {
                let want = match am.visibility {
                    crate::ast::Visibility::Public => {
                        format!("public (as in class {})", aname)
                    }
                    crate::ast::Visibility::Protected => {
                        format!("protected (as in class {}) or weaker", aname)
                    }
                    crate::ast::Visibility::Private => unreachable!(),
                };
                return Err(PhpError::fatal(
                    format!(
                        "Access level to {}::{}() must be {}",
                        d.name, m.decl.name, want
                    ),
                    m.decl.line,
                ));
            }
            if let Some(e) = self.trait_sig_error(m, &am, &d.name, &aname, false) {
                // An internal method's tentative return type warns
                // instead of erroring unless the override carries
                // #[ReturnTypeWillChange] (internal_parent/*).
                if !e.message.starts_with("Could not check")
                    && self
                        .tentative
                        .contains(&(aname.to_lowercase(), lname.clone()))
                    && !m.decl.attrs.iter().any(|a| {
                        a.name
                            .rsplit('\\')
                            .next()
                            .unwrap_or(&a.name)
                            .eq_ignore_ascii_case("ReturnTypeWillChange")
                    })
                {
                    self.deprecated(&format!(
                        "Return type of {}::{}{} should either be compatible with {}::{}{}, or the #[\\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice",
                        d.name,
                        m.decl.name,
                        Self::sig_str_full(&m.decl, &d.name),
                        aname,
                        am.decl.name,
                        Self::sig_str_full(&am.decl, &d.name)
                    ))?;
                    continue;
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// An abstract hook (`get;`/`set;` in an interface or `abstract`
    /// prop) the class must implement — reported as abstract methods
    /// (`A::$p::get`) at class-decl link time. Inherited but
    /// unimplemented hooks still fault subclasses (v3-style:
    /// `abstract A implements I` leaves `I::$p::get` for `B extends A`).
    fn check_abstract_hooks(&self, d: &ClassDecl) -> Result<(), PhpError> {
        if d.is_abstract {
            return Ok(());
        }
        // Concrete hook impls available to this class: its own decl plus
        // every ancestor.
        let mut chain: Vec<&ClassDecl> = vec![d];
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.as_ref());
            pn = pc.decl.parent.clone();
        }
        let implemented = |pname: &str, is_get: bool| {
            chain.iter().any(|c| {
                c.props.iter().any(|cp| {
                    if cp.name != pname {
                        return false;
                    }
                    if let Some(ch) = &cp.hooks {
                        return ch.iter().any(|x| x.is_get == is_get && x.body.is_some());
                    }
                    // A plain prop satisfies `get` always; `set` only
                    // when writable (not readonly / private(set)).
                    if is_get {
                        true
                    } else {
                        !cp.readonly && cp.set_vis.is_none()
                    }
                })
            })
        };
        // Abstract hook decls owed by this class: its interfaces + every
        // class/interface in the ancestor chain.
        let mut sources: Vec<&ClassDecl> = Vec::new();
        for c in &chain {
            sources.push(*c);
            for i in &c.implements {
                if let Some(f) = self.interfaces.get(&i.to_lowercase()) {
                    sources.push(f.as_ref());
                }
            }
        }
        let mut missing: Vec<String> = Vec::new();
        for src in sources {
            for p in &src.props {
                let Some(hs) = &p.hooks else { continue };
                for h in hs {
                    if h.body.is_some() {
                        continue;
                    }
                    let kind = if h.is_get { "get" } else { "set" };
                    let label = format!("{}::${}::{}", src.name, p.name, kind);
                    if implemented(&p.name, h.is_get) {
                        continue;
                    }
                    // A readonly prop (implicit private(set)) can not
                    // satisfy an interface `set` — a dedicated message,
                    // not the abstract-method one.
                    if !h.is_get && src.kind == ClassKind::Interface {
                        let ro = chain.iter().any(|c| {
                            c.props.iter().any(|cp| {
                                cp.name == p.name
                                    && cp.hooks.is_none()
                                    && (cp.readonly || cp.set_vis.is_some())
                            })
                        });
                        if ro {
                            return Err(PhpError::fatal(
                                format!(
                                    "Set access level of {}::${} must be omitted (as in class {})",
                                    d.name, p.name, src.name
                                ),
                                self.cur_line,
                            ));
                        }
                    }
                    if !missing.contains(&label) {
                        missing.push(label);
                    }
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let (n, list) = (missing.len(), missing.join(", "));
        Err(PhpError::fatal(
            format!(
                "Class {} contains {} abstract method{} and must therefore be declared abstract or implement the remaining method{} ({})",
                d.name,
                n,
                if n == 1 { "" } else { "s" },
                if n == 1 { "" } else { "s" },
                list
            ),
            self.cur_line,
        ))
    }

    /// Resolve a class expression to a class name. Scope keywords
    /// (`self`/`static`/`parent`) resolve ONLY from the literal
    /// keyword node — a runtime string naming one looks up literally
    /// and misses ('Class "self" not found', m6 oracle).
    pub(in crate::interp) fn class_name_of(&mut self, e: &Expr) -> Result<String, PhpError> {
        match e {
            Expr::Const(n) => Ok(self.resolve_class_name(n)),
            Expr::Str(s) => Ok(s.trim_start_matches('\\').to_string()),
            Expr::Paren(inner) => self.class_name_of(inner),
            Expr::AnonClass(d) => Ok(self.anon_class_name(d)?),
            _ => {
                let v = self.eval(e)?;
                match v {
                    // The object's INTERNAL class name — `name()` is
                    // the display truncation (`class@anonymous`), which
                    // doesn't key the class table (anon-class `::`
                    // postfixes like `(new class)::K` need the mangled
                    // `class@anonymous\0FILE:LINE$SEQ`).
                    Value::Object(o) => Ok(o.borrow().class.decl.name.clone()),
                    Value::Str(s) => Ok(String::from_utf8_lossy(&s)
                        .trim_start_matches('\\')
                        .to_string()),
                    _ => {
                        // A variable-origin operand (`new $x`, `new
                        // $x[i]`) throws the catchable Error; a folded
                        // expr (`new (5)`, `(5)::f()`) is zend's plain
                        // `Illegal class name` fatal.
                        let mut ce = e;
                        while let Expr::Binary {
                            op: "argline", r, ..
                        } = ce
                        {
                            ce = r;
                        }
                        if matches!(
                            ce,
                            Expr::Var(_)
                                | Expr::VarVar(..)
                                | Expr::Index { .. }
                                | Expr::Prop { .. }
                                | Expr::StaticProp { .. }
                        ) {
                            Err(PhpError::uncaught(
                                "Error",
                                "Class name must be a valid object or a string".to_string(),
                                self.cur_line,
                            ))
                        } else {
                            Err(PhpError::compile_fatal("Illegal class name", self.cur_line))
                        }
                    }
                }
            }
        }
    }

    /// zend's anonymous-class name: `{base}@anonymous\0FILE:LINE$SEQ`
    /// — stable per decl site (a `new class` in a loop reuses the
    /// registered name) and counted process-wide.
    pub(in crate::interp) fn anon_class_name(
        &mut self,
        decl: &Rc<ClassDecl>,
    ) -> Result<String, PhpError> {
        let key = Rc::as_ptr(decl) as usize;
        if let Some(n) = self.anon_class_names.get(&key) {
            return Ok(n.clone());
        }
        let base = decl
            .name
            .rsplit_once('$')
            .map(|(b, _)| b)
            .unwrap_or(&decl.name);
        let file = self
            .decl_file_ctx
            .clone()
            .or_else(|| {
                self.stack
                    .last()
                    .map(|f| f.file.to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.cur_file.to_string());
        let n = format!("{}\0{}:{}${}", base, file, decl.line, self.anon_class_seq);
        self.anon_class_seq += 1;
        let mut d = (**decl).clone();
        d.name = n.clone();
        self.register_class(Rc::new(d))?;
        self.anon_class_names.insert(key, n.clone());
        Ok(n)
    }

    /// `self`/`static`/`parent`/leading-\ name resolution → concrete name.
    pub fn resolve_class_name(&mut self, n: &str) -> String {
        let lname = n.trim_start_matches('\\');
        match lname.to_lowercase().as_str() {
            "static" => {
                if self.in_const_expr > 0 {
                    if let Some(c) = &self.const_self {
                        return c.name().to_string();
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| {
                        f.called_class
                            .as_ref()
                            .map(|c| c.name().to_string())
                            .or_else(|| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    })
                    .unwrap_or_else(|| lname.to_string())
            }
            "self" => {
                if self.in_const_expr > 0 {
                    if let Some(c) = &self.const_self {
                        return c.name().to_string();
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .unwrap_or_else(|| lname.to_string())
            }
            "parent" => {
                if self.in_const_expr > 0 {
                    if let Some(cself) = &self.const_self {
                        return cself
                            .decl
                            .parent
                            .clone()
                            .unwrap_or_else(|| lname.to_string());
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.clone())
                    .and_then(|c| c.decl.parent.clone())
                    .unwrap_or_else(|| lname.to_string())
            }
            _ => lname.to_string(),
        }
    }

    /// name → registered class name; a miss runs the autoloaders
    /// once (FCC string args, class_exists).
    pub(in crate::interp) fn resolve_class(&mut self, name: &str) -> Option<String> {
        let n = name.trim_start_matches('\\');
        if !self.classes.contains_key(&n.to_lowercase())
            && !self.interfaces.contains_key(&n.to_lowercase())
        {
            // Option-typed: a throwing autoloader can't surface here —
            // PHP propagates it, but callers of resolve_class (e.g.
            // class_exists) mostly can't throw either; keep the swallow.
            let _ = self.run_autoload(n);
        }
        if self.classes.contains_key(&n.to_lowercase())
            || self.interfaces.contains_key(&n.to_lowercase())
        {
            Some(n.to_string())
        } else {
            None
        }
    }

    /// Allocate a PHP object/closure handle id: reuse the lowest dead
    /// slot, like Zend's object store recycling freed handles (closures
    /// share the store — `object(Closure)#N` interleaves with objects).
    fn next_obj_id(&mut self, rc: &Rc<RefCell<PhpObject>>) -> u64 {
        let w = ObjHandle::Obj(Rc::downgrade(rc));
        self.push_handle(w)
    }

    pub(in crate::interp) fn next_callable_id(&mut self, c: &Rc<PhpCallable>) -> u64 {
        // Only closures own per-instance statics tables — the decl name
        // lets push_handle GC a dead closure's table on slot reuse.
        let name = match &c.kind {
            CallableKind::Closure(d) => Some(d.name.to_string()),
            _ => None,
        };
        self.push_handle(ObjHandle::Callable(Rc::downgrade(c), name))
    }

    fn push_handle(&mut self, w: ObjHandle) -> u64 {
        // Zend reuses the most recently freed handle first (its free
        // list is a LIFO stack): `dead_slots` is that stack — deaths
        // push their slot in `mark_obj_died`, allocation pops it.
        // Silently-dead slots (no dtor ran) aren't stamped, so fall
        // back to a bounded scan from the tail when the stack empties
        // (namespace_004, gh10168).
        self.spawn_seq += 1;
        let mut best: Option<usize> = None;
        while let Some(i) = self.dead_slots.pop() {
            if i < self.obj_handles.len()
                && !self.obj_handles[i].alive()
                && self.obj_died.get(i).copied().unwrap_or(0) != 0
            {
                best = Some(i);
                break;
            }
        }
        if best.is_none() {
            let n = self.obj_handles.len();
            for i in (0..n).rev().take(256) {
                if !self.obj_handles[i].alive() {
                    best = Some(i);
                    break;
                }
            }
        }
        if let Some(i) = best {
            if let ObjHandle::Callable(_, Some(name)) = &self.obj_handles[i] {
                // The dead closure's per-instance statics table dies
                // with it — the recycled id must not leak stale
                // entries to an unrelated decl of the same name.
                self.statics.remove(&format!("{}\u{0}c{}", name, i + 1));
            }
            self.obj_handles[i] = w;
            self.obj_born[i] = self.spawn_seq;
            self.obj_died[i] = 0;
            return (i + 1) as u64;
        }
        self.obj_handles.push(w);
        self.obj_born.push(self.spawn_seq);
        self.obj_died.push(0);
        self.obj_handles.len() as u64
    }

    /// A handle slot's zval hit refcount 0 — stamp its death order so
    /// the next allocation reuses the most recently freed slot.
    pub(in crate::interp) fn mark_obj_died(&mut self, o: &Rc<RefCell<PhpObject>>) {
        let id = o.borrow().id as usize;
        if id >= 1 && id <= self.obj_died.len() && self.obj_died[id - 1] == 0 {
            self.spawn_seq += 1;
            self.obj_died[id - 1] = self.spawn_seq;
            self.dead_slots.push(id - 1);
        }
    }

    /// Wrap a PhpCallable assigning its object-store id.
    pub(in crate::interp) fn new_callable(&mut self, inner: PhpCallable) -> Rc<PhpCallable> {
        let c = Rc::new(inner);
        let id = self.next_callable_id(&c);
        c.id.set(id);
        c
    }

    /// Wrap a PhpObject in Rc and assign its handle id.
    pub fn alloc_obj(&mut self, o: PhpObject) -> Rc<RefCell<PhpObject>> {
        // zend emalloc: the object store handle + zval + its
        // default_properties_table (~56B struct + 16B/prop slot) — the
        // bulk of `while(true) { $a[] = new X }` growth. Tracked: the
        // charge releases when the object dies (efree).
        let bytes = 72 + 16 * o.props.len() as u64;
        // The arena counter counts every shell too — a flat cost per
        // allocation (new_oom), decremented at Drop like zend's arena.
        crate::value::obj_charge();
        let rc = Rc::new(RefCell::new(o));
        self.mem_track(&rc, bytes);
        let id = self.next_obj_id(&rc);
        rc.borrow_mut().id = id;
        rc
    }

    /// `WeakReference::create($obj)` — object holding a weak handle to
    /// $obj (the WeakRef internal; get() upgrades it). Zend keeps a
    /// per-handle weakref list: repeated create() on the same live
    /// target returns the identical wrapper (`===` true).
    pub(in crate::interp) fn new_weakref(
        &mut self,
        target: Rc<RefCell<PhpObject>>,
    ) -> Result<Value, PhpError> {
        let target_id = target.borrow().id;
        // The dedup hit must prove the TARGET still lives: the wrapper
        // object itself stays alive in userland while its handle id is
        // recycled to a new object, so upgrading the wrapper's own weak
        // is not enough — a recycled id would hand back a stale wrapper
        // whose get() reads null on a live target.
        if let Some(existing) = self.weakrefs.get(&target_id).and_then(|w| w.upgrade()) {
            let live = matches!(
                &existing.borrow().internal,
                Some(ObjectInternal::WeakRef(tw)) if tw.upgrade().is_some()
            );
            if live {
                return Ok(Value::Object(existing));
            }
        }
        let cls = match self.classes.get("weakreference").cloned() {
            Some(c) => c,
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Class \"WeakReference\" not found",
                    0,
                ))
            }
        };
        let rc = self.alloc_obj(PhpObject {
            class: cls,
            props: HashMap::new(),
            prop_order: vec![],
            id: 0,
            internal: Some(ObjectInternal::WeakRef(Rc::downgrade(&target))),
            unset_props: std::collections::HashSet::new(),
        });
        self.weakrefs.insert(target_id, Rc::downgrade(&rc));
        Ok(Value::Object(rc))
    }

    /// `new X(args)` — instantiate + call __construct.
    pub(in crate::interp) fn new_instance(
        &mut self,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let lname = name.to_lowercase();
        if !self.classes.contains_key(&lname) {
            self.run_autoload(name.trim_start_matches('\\'))?;
        }
        let cls = match self.classes.get(&lname) {
            Some(c) => c.clone(),
            None => {
                let t = name.trim_start_matches('\\');
                if self.traits.contains_key(&t.to_lowercase()) {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot instantiate trait {}", t),
                        0,
                    ));
                }
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", name),
                    0,
                ));
            }
        };
        if cls.name().eq_ignore_ascii_case("closure") {
            return self.fail(PhpError::uncaught(
                "Error",
                "Instantiation of class Closure is not allowed",
                0,
            ));
        }
        if cls.name().eq_ignore_ascii_case("weakreference") {
            return self.fail(PhpError::uncaught(
                "Error",
                "Direct instantiation of WeakReference is not allowed, use WeakReference::create instead",
                0,
            ));
        }
        if cls.decl.kind == ClassKind::Trait {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate trait {}", cls.name()),
                0,
            ));
        }
        if cls.decl.kind == ClassKind::Interface {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate interface {}", cls.name()),
                0,
            ));
        }
        if cls.decl.is_abstract {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate abstract class {}", cls.name()),
                0,
            ));
        }
        if cls.decl.kind == ClassKind::Enum {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate enum {}", cls.name()),
                0,
            ));
        }
        self.link_const_inits(&cls)?;
        let has_ctor = self.find_method_in(&cls, "__construct").is_some();
        if !has_ctor {
            if let Some((n, ..)) = args.named.first() {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Unknown named parameter ${}", n),
                    0,
                ));
            }
        }
        let obj = self.instantiate(&lname, &[])?;
        // __construct (native for builtins via method_invoke's
        // interception); the ctor may be inherited (property_hooks/foreach).
        if has_ctor {
            if let Value::Object(o) = &obj {
                if let Err(e) = self.method_invoke_vis(o.clone(), "__construct", args) {
                    // A ctor that throws leaves a half-built object;
                    // zend never runs its __destruct (bug29368_1/_3).
                    self.mark_destructed(o);
                    return Err(e);
                }
            }
        }
        if let Value::Object(o) = &obj {
            self.expr_temps.push(o.clone());
        }
        Ok(obj)
    }

    /// Build the object shell: init props along the whole parent chain.
    pub fn instantiate(&mut self, lname: &str, _args: &[Value]) -> Result<Value, PhpError> {
        let cls = self.classes.get(&lname.to_lowercase()).cloned();
        let cls = match cls {
            Some(c) => c,
            None => {
                return Ok(Value::Object(self.alloc_obj(PhpObject {
                    class: Rc::new(PhpClass {
                        decl: Rc::new(ClassDecl {
                            name: lname.into(),
                            kind: ClassKind::Class,
                            is_abstract: false,
                            is_final: false,
                            readonly: false,
                            parent: None,
                            implements: vec![],
                            attrs: vec![],
                            traits: vec![],
                            adaptations: vec![],
                            methods: vec![],
                            props: vec![],
                            consts: vec![],
                            file: String::new(),
                            line: 0,
                        }),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(true),
                    }),
                    props: HashMap::new(),
                    prop_order: vec![],
                    id: 0,
                    internal: None,
                    unset_props: std::collections::HashSet::new(),
                })))
            }
        };
        // Collect decl chain (self + parents, parent-first for prop order).
        let mut chain = vec![cls.clone()];
        let mut cur = cls.clone();
        while let Some(p) = cur.decl.parent.clone() {
            if let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() {
                chain.push(pc.clone());
                cur = pc;
            } else {
                break;
            }
        }
        chain.reverse();
        let mut props = HashMap::new();
        let mut prop_order = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (ci, c) in chain.iter().enumerate() {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                let is_priv = p.visibility == crate::ast::Visibility::Private;
                if !is_priv {
                    // The NEAREST redecl is authoritative — defaults are
                    // never inherited (default_value_inheritance): a later
                    // decl replaces any slot a grandparent already made,
                    // but keeps the first declaration's slot position
                    // (foreachLoopObjects.002).
                    if !seen.insert(p.name.clone()) {
                        props.remove(&p.name);
                    }
                }
                let backed = if p.hooks.is_some() {
                    Self::prop_is_backed(p)
                        || chain[..=ci].iter().any(|c2| {
                            c2.decl.props.iter().any(|p2| {
                                p2.name == p.name
                                    && p2.hooks.is_none()
                                    && p2.visibility != crate::ast::Visibility::Private
                            })
                        })
                } else {
                    true
                };
                // A virtual hooked prop has no backing slot at all.
                if !backed {
                    continue;
                }
                // A typed prop without a default starts *uninitialized* —
                // no cell, but Zend still reserves its table position, so
                // a later write lands in declaration order
                // (property_hooks/foreach's backedUninitialized).
                if p.ty.is_some() && p.default.is_none() {
                    let key = if is_priv {
                        format!("\0{}\0{}", c.decl.name, p.name)
                    } else {
                        p.name.clone()
                    };
                    if !prop_order.contains(&key) {
                        prop_order.push(key);
                    }
                    continue;
                }
                let mut default = match &p.default {
                    Some(d) => {
                        let old = self.const_self.replace(c.clone());
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(
                            d,
                            &c.decl.file,
                            if p.dline > 0 { p.dline } else { p.line },
                        );
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        r?
                    }
                    None => Value::Null,
                };
                // Runtime defaults (define()'d consts etc.) go through
                // the same write check as assignments — strict files
                // TypeError here (typed_properties_058).
                if p.ty.is_some() {
                    default = self.prop_typed_write_check(p, c, default)?;
                }
                // Private props live in a per-declaring-class slot
                // ("\0Cls\0name"), so C::$e and E::$e are distinct.
                let key = if is_priv {
                    format!("\0{}\0{}", c.decl.name, p.name)
                } else {
                    p.name.clone()
                };
                if !prop_order.contains(&key) {
                    prop_order.push(key.clone());
                }
                props.insert(key, cell(default));
            }
        }
        let internal = if self.is_throwable_name(&cls.decl.name) {
            // A throwable's file/line attribute to the executing code
            // unit — inside a call frame that's the frame's own file
            // (an error handler declared in the caller's file reports
            // there even when invoked for an eval'd unit's diag); only
            // outside frames does the ambient diag file apply.
            let exec_file = self
                .stack
                .last()
                .map(|f| f.file.to_string())
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| self.diag_file());
            Some(ObjectInternal::Exception {
                file: exec_file,
                line: self.send_line.unwrap_or(self.cur_line) as u32,
                trace: String::new(),
                thrown: self.send_line.unwrap_or(self.cur_line) as u32,
                full_msg: String::new(),
                eval_ctx: 0,
                previous: None,
                frames: Rc::new(self.call_trace.clone()),
            })
        } else {
            None
        };
        let v = Value::Object(self.alloc_obj(PhpObject {
            class: cls,
            props,
            prop_order,
            id: 0,
            internal,
            unset_props: std::collections::HashSet::new(),
        }));
        // zend's throwable dump view needs engine state materialized
        // into props (file/line/string/trace) before any ctor runs.
        if let Value::Object(o) = &v {
            if matches!(o.borrow().internal, Some(ObjectInternal::Exception { .. })) {
                self.exception_prop_defaults(o);
            }
        }
        Ok(v)
    }

    pub(in crate::interp) fn is_throwable_name(&mut self, name: &str) -> bool {
        let ln = name.to_lowercase();
        let mut cur = self.classes.get(&ln).cloned();
        while let Some(c) = cur {
            if c.decl
                .implements
                .iter()
                .any(|i| i.eq_ignore_ascii_case("throwable"))
            {
                return true;
            }
            if c.name().eq_ignore_ascii_case("throwable") {
                return true;
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }

    pub(in crate::interp) fn prop_name(&mut self, n: &PropName) -> Result<String, PhpError> {
        let s = match n {
            PropName::Name(s) => return Ok(s.clone()),
            PropName::Var(v) => {
                let val = self.var_get(v)?;
                self.conv_str(&val)?
            }
            PropName::Expr(e) => {
                let v = self.eval(e)?;
                self.conv_str(&v)?
            }
        };
        // Property names keep NUL bytes — the private-name-mangle
        // check fires downstream (bug52484). METHOD names truncate
        // at their call sites (bug46238).
        Ok(s)
    }

    /// Zend method names are C strings — a NUL byte truncates the
    /// name (`"\0"` invokes `""`; bug46238).
    pub(in crate::interp) fn nul_trunc(s: &str) -> String {
        s.split('\0').next().unwrap_or_default().to_string()
    }
}

/// Typed-const compat in trait composition: same member list
/// (case-insensitive; both `None` is compatible).
fn ty_list_eq(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> bool {
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
