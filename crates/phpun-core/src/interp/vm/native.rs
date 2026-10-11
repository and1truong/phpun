//! A bounded, W^X integer tier. Only proven pure graphs may restart in the VM.
use super::{CachedFn, Compiled, FunctionDecl, Interp, Op, Rc, Slot, Value};
use std::cell::Cell;

pub(in crate::interp) enum State {
    Cold(u32),
    Disabled,
    Ready(Rc<Program>),
}

#[repr(C)]
struct Context {
    used: *mut u64,
    size: u64,
    depth: u64,
    calls: u64,
}

pub(in crate::interp) struct Program {
    code: *mut libc::c_void,
    len: usize,
    // Keep declarations alive, not compiled bodies: recursive caches must not
    // gain another Rc<Compiled> cycle through this tier.
    dependencies: Vec<(Rc<str>, Rc<FunctionDecl>)>,
    pub(super) executions: Cell<u64>,
}

impl Drop for Program {
    fn drop(&mut self) {
        // SAFETY: this mapping is owned by this Program, never shared with a
        // running invocation (the invocation retains an Rc until it returns).
        unsafe { libc::munmap(self.code, self.len) };
    }
}

fn integer_type(ty: &Option<Vec<String>>) -> bool {
    ty.as_ref()
        .is_none_or(|t| t.len() == 1 && t[0].eq_ignore_ascii_case("int"))
}

fn boolean_type(ty: &Option<Vec<String>>) -> bool {
    ty.as_ref()
        .is_some_and(|t| t.len() == 1 && t[0].eq_ignore_ascii_case("bool"))
}

fn eligible(d: &FunctionDecl, c: &Compiled) -> bool {
    !c.hybrid
        && !c.needs_bind
        && !c.top_level
        && !c.loop_body
        && !d.by_ref
        && d.attrs.is_empty()
        && d.ns.is_empty()
        && d.decl_in.is_none()
        && d.params.len() <= 16
        && c.nslots >= d.params.len()
        && c.nslots <= 256
        && c.ops.len() <= 1024
        && (integer_type(&d.ret)
            || boolean_type(&d.ret) && !c.ops.iter().any(|op| matches!(op, Op::Call { .. })))
        && d.params.iter().all(|p| {
            !p.by_ref && !p.variadic && !p.promoted && p.default.is_none() && integer_type(&p.ty)
        })
}

struct Graph {
    functions: Vec<(Rc<FunctionDecl>, Rc<Compiled>)>,
    targets: Vec<Vec<Option<usize>>>,
    dependencies: Vec<(Rc<str>, Rc<FunctionDecl>)>,
}

impl Graph {
    fn build(it: &mut Interp<'_>, decl: &Rc<FunctionDecl>, closure: bool) -> Option<Self> {
        // An unregistered root is allowed only after the shared callback
        // target proves it is an uncaptured context-free scalar closure.
        let name = decl.name.to_lowercase();
        if !closure && !it.functions.get(&name).is_some_and(|d| Rc::ptr_eq(d, decl)) {
            return None;
        }
        let root = it.vm_compiled(decl)?;
        let mut g = Self {
            functions: vec![(decl.clone(), root)],
            targets: Vec::new(),
            dependencies: Vec::new(),
        };
        let mut index = 0;
        while index < g.functions.len() {
            let (d, c) = &g.functions[index];
            if !eligible(d, c) {
                return None;
            }
            let c = c.clone();
            let mut targets = vec![None; c.ops.len()];
            for (pc, op) in c.ops.iter().enumerate() {
                if let Op::Call {
                    lname, argc, cache, ..
                } = op
                {
                    let d = match &*cache.borrow() {
                        Some(CachedFn::Direct(d, _)) => d.clone(),
                        Some(_) => return None,
                        None => it.functions.get(lname.as_ref())?.clone(),
                    };
                    // Bool results never enter an integer graph value stack.
                    if !integer_type(&d.ret)
                        || d.params.len() != *argc as usize
                        || !it
                            .functions
                            .get(lname.as_ref())
                            .is_some_and(|current| Rc::ptr_eq(current, &d))
                    {
                        return None;
                    }
                    let target = if let Some(i) =
                        g.functions.iter().position(|(old, _)| Rc::ptr_eq(old, &d))
                    {
                        i
                    } else {
                        if g.functions.len() >= 16 {
                            return None;
                        }
                        let compiled = it.vm_compiled(&d)?;
                        g.functions.push((d.clone(), compiled));
                        g.functions.len() - 1
                    };
                    targets[pc] = Some(target);
                    g.dependencies.push((lname.clone(), d));
                }
            }
            g.targets.push(targets);
            index += 1;
        }
        if g.functions.iter().map(|(_, c)| c.ops.len()).sum::<usize>() > 1024 {
            return None;
        }
        Some(g)
    }
}

// The accepted value stack contains integers only. Comparisons may create a
// bool only immediately before its consuming conditional branch. All joins,
// including backward edges, must have one consistent stack depth.
fn depths(c: &Compiled, boolean: bool) -> Option<Vec<Option<usize>>> {
    let mut depths: Vec<Option<usize>> = vec![None; c.ops.len() + 1];
    depths[0] = Some(0);
    let mut work = vec![0];
    let set = |depths: &mut Vec<Option<usize>>,
               work: &mut Vec<usize>,
               pc: usize,
               depth: usize|
     -> Option<()> {
        let at = depths.get_mut(pc)?;
        if at.is_some_and(|old| old != depth) {
            return None;
        }
        if at.is_none() {
            *at = Some(depth);
            work.push(pc);
        }
        Some(())
    };
    while let Some(pc) = work.pop() {
        let Some(op) = c.ops.get(pc) else {
            continue;
        };
        let n = depths[pc]?;
        let (take, give) = match op {
            Op::Const(Value::Int(_))
            | Op::Load(_)
            | Op::IncDec { .. }
            | Op::BinaryCvConst(_, _, Value::Int(_)) => (0, 1),
            Op::Store(_) => (1, 1),
            Op::Pop => (1, 0),
            Op::Binary(_) => (2, 1),
            Op::BinaryCv(_, _) => (1, 1),
            Op::Call { argc, .. } => (*argc as usize, 1),
            Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => (1, 0),
            Op::Return => (1, 0),
            Op::Line(_) | Op::Sweep | Op::InitCall { .. } | Op::Jump(_) => (0, 0),
            _ => return None,
        };
        let next = n.checked_sub(take)? + give;
        if boolean && matches!(op, Op::Return) && !pc.checked_sub(1).is_some_and(|p| {
            matches!(&c.ops[p], Op::Binary(op) | Op::BinaryCv(op, _) | Op::BinaryCvConst(op, _, _)
                if matches!(*op, "==" | "===" | "!=" | "!==" | "<" | "<=" | ">" | ">="))
        }) {
            return None;
        }
        match op {
            Op::Binary(op) | Op::BinaryCv(op, _) | Op::BinaryCvConst(op, _, _)
                if !matches!(*op, "+" | "-" | "*" | "%" | "<=>")
                    && !(matches!(*op, "==" | "===" | "!=" | "!==" | "<" | "<=" | ">" | ">=")
                        && (matches!(
                            c.ops.get(pc + 1),
                            Some(Op::JumpIfFalse(_) | Op::JumpIfTrue(_))
                        ) || boolean && matches!(c.ops.get(pc + 1), Some(Op::Return)))) =>
            {
                return None
            }
            _ => {}
        }
        match op {
            Op::Return => {}
            Op::Jump(t) => {
                set(&mut depths, &mut work, *t, next)?;
            }
            Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                set(&mut depths, &mut work, *t, next)?;
                set(&mut depths, &mut work, pc + 1, next)?;
            }
            _ => set(&mut depths, &mut work, pc + 1, next)?,
        }
    }
    Some(depths)
}

// Tiny SysV x86_64 emitter over existing Ops. Stack locations are compile-time
// bounds checked; all branches/calls are patched within this single mapping.
struct Emitter {
    bytes: Vec<u8>,
    calls: Vec<(usize, usize)>,
}
impl Emitter {
    fn emit(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }
    fn word(&mut self, n: i32) {
        self.emit(&n.to_le_bytes());
    }
    fn branch(&mut self, condition: Option<u8>) -> usize {
        match condition {
            Some(c) => self.emit(&[0x0f, c]),
            None => self.emit(&[0xe9]),
        }
        let at = self.bytes.len();
        self.word(0);
        at
    }
    fn patch(&mut self, at: usize, target: usize) -> Option<()> {
        let delta = i32::try_from(target as isize - (at + 4) as isize).ok()?;
        self.bytes[at..at + 4].copy_from_slice(&delta.to_le_bytes());
        Some(())
    }
    fn load(&mut self, offset: i32, rdx: bool) {
        self.emit(&[0x48, 0x8b, if rdx { 0x94 } else { 0x84 }, 0x24]);
        self.word(offset);
    }
    fn store(&mut self, offset: i32) {
        self.emit(&[0x48, 0x89, 0x84, 0x24]);
        self.word(offset);
    }
    fn immediate(&mut self, n: i64) {
        self.emit(&[0x48, 0xb8]);
        self.emit(&n.to_le_bytes());
    }
    fn flag(&mut self, offset: i32, n: i32) {
        self.emit(&[0x48, 0xc7, 0x84, 0x24]);
        self.word(offset);
        self.word(n);
    }
    fn local(&mut self, slot: u16, fail: &mut Vec<usize>) {
        self.emit(&[0x48, 0x83, 0xbc, 0x24]);
        self.word(i32::from(slot) * 16 + 8);
        self.emit(&[0]);
        fail.push(self.branch(Some(0x84)));
        self.load(i32::from(slot) * 16, false);
    }
    fn binary(&mut self, op: &str, fail: &mut Vec<usize>) {
        match op {
            "+" => self.emit(&[0x48, 0x01, 0xd0]),
            "-" => self.emit(&[0x48, 0x29, 0xd0]),
            "*" => self.emit(&[0x48, 0x0f, 0xaf, 0xc2]),
            "%" => {
                // Zero and MIN/-1 retain the canonical VM error/edge path.
                // Refuse all -1 divisors rather than risking a hardware trap.
                self.emit(&[0x48, 0x85, 0xd2]);
                fail.push(self.branch(Some(0x84)));
                self.emit(&[0x48, 0x83, 0xfa, 0xff]);
                fail.push(self.branch(Some(0x84)));
                // RCX=divisor, CQO sign-extends RAX into RDX:RAX, IDIV RCX.
                self.emit(&[
                    0x48, 0x89, 0xd1, 0x48, 0x99, 0x48, 0xf7, 0xf9, 0x48, 0x89, 0xd0,
                ]);
                return;
            }
            "<=>" => {
                // Signed integer comparison, including MIN/MAX; SETcc leaves
                // flags intact until both booleans have been captured.
                self.emit(&[
                    0x48, 0x39, 0xd0, 0x0f, 0x9f, 0xc0, 0x0f, 0x9c, 0xc2, 0x48, 0x0f, 0xb6, 0xc0,
                    0x48, 0x0f, 0xb6, 0xd2, 0x48, 0x29, 0xd0,
                ]);
                return;
            }
            _ => {
                self.emit(&[0x48, 0x39, 0xd0]);
                let cc = match op {
                    "==" | "===" => 0x94,
                    "!=" | "!==" => 0x95,
                    "<" => 0x9c,
                    "<=" => 0x9e,
                    ">" => 0x9f,
                    ">=" => 0x9d,
                    _ => unreachable!(),
                };
                self.emit(&[0x0f, cc, 0xc0, 0x48, 0x0f, 0xb6, 0xc0]);
                return;
            }
        }
        fail.push(self.branch(Some(0x80))); // JO: PHP overflow promotes to float.
    }
    fn function(
        &mut self,
        d: &FunctionDecl,
        c: &Compiled,
        targets: &[Option<usize>],
    ) -> Option<usize> {
        let depths = depths(c, boolean_type(&d.ret))?;
        let start = self.bytes.len();
        let locals = i32::try_from(c.nslots * 16).ok()?;
        let max = depths.iter().flatten().copied().max().unwrap_or(0);
        let out = locals + i32::try_from((max + 1) * 8).ok()?;
        let frame = (out + 8 + 15) & !15;
        let span =
            i32::try_from((super::super::VM_FRAME_SLOTS + d.params.len() as u64) * 16).ok()?;
        let mut fail = Vec::new();
        let mut no_entry = Vec::new();
        let mut returns = Vec::new();
        // Five saved registers align RSP to16 before CALL; ctx/args/out survive.
        self.emit(&[
            0x55, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x49, 0x89, 0xfc, 0x49, 0x89,
            0xf5, 0x49, 0x89, 0xd6, 0x48, 0x81, 0xec,
        ]);
        self.word(frame);
        self.emit(&[0x49, 0x83, 0x7c, 0x24, 0x10, 64]);
        no_entry.push(self.branch(Some(0x83)));
        self.emit(&[0x49, 0x83, 0x7c, 0x24, 0x10, 0]);
        let root = self.branch(Some(0x84));
        self.emit(&[0x49, 0x8b, 0x04, 0x24, 0x48, 0x8b, 0x10, 0x48, 0x81, 0xc2]);
        self.word(span);
        self.emit(&[0x49, 0x3b, 0x54, 0x24, 0x08]);
        no_entry.push(self.branch(Some(0x87)));
        self.emit(&[0x48, 0x89, 0x10]);
        self.patch(root, self.bytes.len())?;
        self.emit(&[0x49, 0xff, 0x44, 0x24, 0x10, 0x49, 0xff, 0x44, 0x24, 0x18]);
        for slot in 0..c.nslots {
            self.flag(slot as i32 * 16 + 8, 0);
        }
        for slot in 0..d.params.len() {
            self.emit(&[0x49, 0x8b, 0x85]);
            self.word(slot as i32 * 8);
            self.store(slot as i32 * 16);
            self.flag(slot as i32 * 16 + 8, 1);
        }
        let mut labels = vec![0; c.ops.len() + 1];
        let mut jumps = Vec::new();
        for (pc, op) in c.ops.iter().enumerate() {
            labels[pc] = self.bytes.len();
            let Some(n) = depths[pc] else {
                continue;
            };
            let value = |n: usize| locals + n as i32 * 8;
            match op {
                Op::Const(Value::Int(i)) => {
                    self.immediate(*i);
                    self.store(value(n));
                }
                Op::Load(slot) => {
                    if *slot as usize >= c.nslots {
                        return None;
                    }
                    self.local(*slot, &mut fail);
                    self.store(value(n));
                }
                Op::Store(slot) => {
                    if *slot as usize >= c.nslots {
                        return None;
                    }
                    self.load(value(n - 1), false);
                    self.store(i32::from(*slot) * 16);
                    self.flag(i32::from(*slot) * 16 + 8, 1);
                }
                Op::Binary(op) => {
                    self.load(value(n - 2), false);
                    self.load(value(n - 1), true);
                    self.binary(op, &mut fail);
                    self.store(value(n - 2));
                }
                Op::BinaryCv(op, slot) => {
                    if *slot as usize >= c.nslots {
                        return None;
                    }
                    self.local(*slot, &mut fail);
                    self.load(value(n - 1), true);
                    self.binary(op, &mut fail);
                    self.store(value(n - 1));
                }
                Op::BinaryCvConst(op, slot, Value::Int(rhs)) => {
                    if *slot as usize >= c.nslots {
                        return None;
                    }
                    self.local(*slot, &mut fail);
                    self.emit(&[0x48, 0xba]);
                    self.emit(&rhs.to_le_bytes());
                    self.binary(op, &mut fail);
                    self.store(value(n));
                }
                Op::IncDec { slot, delta, post } => {
                    if *slot as usize >= c.nslots {
                        return None;
                    }
                    self.local(*slot, &mut fail);
                    if *post {
                        self.store(value(n));
                    }
                    self.emit(&[0x48, 0xba]);
                    self.emit(&delta.to_le_bytes());
                    self.binary("+", &mut fail);
                    self.store(i32::from(*slot) * 16);
                    if !*post {
                        self.store(value(n));
                    }
                }
                Op::Jump(t) => jumps.push((self.branch(None), *t)),
                Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                    self.load(value(n - 1), false);
                    self.emit(&[0x48, 0x85, 0xc0]);
                    jumps.push((
                        self.branch(Some(if matches!(op, Op::JumpIfFalse(_)) {
                            0x84
                        } else {
                            0x85
                        })),
                        *t,
                    ));
                }
                Op::Call { argc, .. } => {
                    self.emit(&[0x4c, 0x89, 0xe7, 0x48, 0x8d, 0xb4, 0x24]);
                    self.word(value(n - *argc as usize));
                    self.emit(&[0x48, 0x8d, 0x94, 0x24]);
                    self.word(out);
                    self.emit(&[0xe8]);
                    let at = self.bytes.len();
                    self.word(0);
                    self.calls.push((at, targets[pc]?));
                    self.emit(&[0x84, 0xc0]);
                    fail.push(self.branch(Some(0x84)));
                    self.load(out, false);
                    self.store(value(n - *argc as usize));
                }
                Op::Return => {
                    self.load(value(n - 1), false);
                    self.emit(&[0x49, 0x89, 0x06, 0xb0, 1]);
                    returns.push(self.branch(None));
                }
                Op::Line(_) | Op::InitCall { .. } | Op::Sweep | Op::Pop => {}
                _ => return None,
            }
        }
        labels[c.ops.len()] = self.bytes.len();
        let failure = self.bytes.len();
        self.emit(&[0xb0, 0]);
        let teardown = self.bytes.len();
        // Root arena was reserved by the shared binder. Nested spans are
        // repaid even on overflow/uninit/depth/capacity exits before VM replay.
        self.emit(&[
            0x49, 0xff, 0x4c, 0x24, 0x10, 0x49, 0x83, 0x7c, 0x24, 0x10, 0,
        ]);
        let root_done = self.branch(Some(0x84));
        self.emit(&[0x49, 0x8b, 0x14, 0x24, 0x48, 0x81, 0x2a]);
        self.word(span);
        let final_return = self.bytes.len();
        self.patch(root_done, final_return)?;
        self.emit(&[0x48, 0x81, 0xc4]);
        self.word(frame);
        self.emit(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5d, 0xc3]);
        let early = self.bytes.len();
        self.emit(&[0xb0, 0]);
        let j = self.branch(None);
        self.patch(j, final_return)?;
        for at in fail {
            self.patch(at, failure)?;
        }
        for at in returns {
            self.patch(at, teardown)?;
        }
        for at in no_entry {
            self.patch(at, early)?;
        }
        for (at, t) in jumps {
            self.patch(at, *labels.get(t)?)?;
        }
        Some(start)
    }
}

impl Program {
    fn build(it: &mut Interp<'_>, decl: &Rc<FunctionDecl>, closure: bool) -> Option<Self> {
        let graph = Graph::build(it, decl, closure)?;
        let mut e = Emitter {
            bytes: Vec::new(),
            calls: Vec::new(),
        };
        let mut starts = Vec::new();
        for (i, (d, c)) in graph.functions.iter().enumerate() {
            starts.push(e.function(d, c, &graph.targets[i])?);
        }
        for (at, index) in std::mem::take(&mut e.calls) {
            e.patch(at, *starts.get(index)?)?;
        }
        if e.bytes.len() > 131072 {
            return None;
        }
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
        if page == 0 {
            return None;
        }
        let len = e.bytes.len().checked_add(page - 1)? / page * page;
        let used: usize = it
            .compiled_fns
            .values()
            .filter_map(|entry| match &entry.3 {
                State::Ready(p) => Some(p.len),
                _ => None,
            })
            .sum();
        if len > (8 * 1024 * 1024usize).saturating_sub(used) {
            return None;
        }
        // SAFETY: anonymous private RW mapping, initialized completely before
        // publishing RX. No writable executable page or external code bytes.
        let code = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if code == libc::MAP_FAILED {
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(e.bytes.as_ptr(), code.cast::<u8>(), e.bytes.len());
            if libc::mprotect(code, len, libc::PROT_READ | libc::PROT_EXEC) != 0 {
                libc::munmap(code, len);
                return None;
            }
        }
        Some(Self {
            code,
            len,
            dependencies: graph.dependencies,
            executions: Cell::new(0),
        })
    }
}

fn cached(it: &mut Interp<'_>, decl: &Rc<FunctionDecl>, closure: bool) -> Option<Rc<Program>> {
    if it.deadline.is_some()
        || super::Interp::callprof_on()
        || super::coverage_on()
        || it.compiled_fns.len() > 4096
    {
        return None;
    }
    let key = Rc::as_ptr(decl) as usize;
    let program = {
        let state = &mut it.compiled_fns.get_mut(&key)?.3;
        match state {
            State::Disabled => return None,
            State::Ready(p) => Some(p.clone()),
            State::Cold(n) => {
                *n = n.saturating_add(1);
                if *n < 64 {
                    return None;
                }
                None
            }
        }
    };
    let program = match program {
        Some(p) => p,
        None => match Program::build(it, decl, closure) {
            Some(p) => {
                let p = Rc::new(p);
                it.compiled_fns.get_mut(&key)?.3 = State::Ready(p.clone());
                p
            }
            None => {
                it.compiled_fns.get_mut(&key)?.3 = State::Disabled;
                return None;
            }
        },
    };
    if !program.dependencies.iter().all(|(name, d)| {
        it.functions
            .get(name.as_ref())
            .is_some_and(|current| Rc::ptr_eq(current, d))
    }) {
        it.compiled_fns.get_mut(&key)?.3 = State::Disabled;
        return None;
    }
    Some(program)
}

fn run(
    it: &mut Interp<'_>,
    decl: &Rc<FunctionDecl>,
    program: &Program,
    arguments: &[i64],
) -> Option<Value> {
    if decl.params.len() > arguments.len() {
        return None;
    }
    let top = it.vm_stack.last_mut()?;
    let used = top.used;
    let mut context = Context {
        used: &mut top.used,
        size: top.size,
        depth: 0,
        calls: 0,
    };
    let mut out = 0;
    // SAFETY: emitter implements this SysV ABI, checks every slot/stack target,
    // and accepts only integer ops. The mapping and current arena segment live
    // through the call. Native code cannot call Rust/PHP or grow this segment.
    let entry: unsafe extern "C" fn(*mut Context, *const i64, *mut i64) -> u8 =
        unsafe { std::mem::transmute(program.code) };
    let success = unsafe { entry(&mut context, arguments.as_ptr(), &mut out) };
    assert_eq!(context.depth, 0, "native activation leak");
    assert_eq!(top.used, used, "native arena span leak");
    if success == 1 {
        program.executions.set(program.executions.get() + 1);
        Some(if boolean_type(&decl.ret) {
            Value::Bool(out != 0)
        } else {
            Value::Int(out)
        })
    } else {
        it.compiled_fns.get_mut(&(Rc::as_ptr(decl) as usize))?.3 = State::Disabled;
        None
    }
}

#[inline(never)]
pub(super) fn execute(
    it: &mut Interp<'_>,
    decl: &Rc<FunctionDecl>,
    slots: &[Slot],
) -> Option<Value> {
    if it.cur().this_obj.is_some() || it.cur().closure_rc.is_some() {
        return None;
    }
    let mut arguments = [0i64; 16];
    if decl.params.len() > arguments.len() {
        return None;
    }
    for (i, slot) in slots.iter().take(decl.params.len()).enumerate() {
        let value = match slot {
            Slot::Arg(i) => it.cur().value_args[*i as usize].clone(),
            Slot::V(v) => v.clone(),
            Slot::C(c) => c.borrow().clone(),
            Slot::Uninit => return None,
        };
        let Value::Int(n) = value else {
            return None;
        };
        arguments[i] = n;
    }
    let program = cached(it, decl, false)?;
    run(it, decl, &program, &arguments)
}

/// Caller reuses vm_callback_values_target's complete context/argument proof.
/// The graph adds purity and int-only guards, so no PHP effect precedes replay.
pub(super) fn callback(
    it: &mut Interp<'_>,
    decl: &Rc<FunctionDecl>,
    values: &[Value],
) -> Option<Value> {
    let mut arguments = [0i64; 16];
    if values.len() > arguments.len() {
        return None;
    }
    for (i, value) in values.iter().enumerate() {
        let Value::Int(n) = value else {
            return None;
        };
        arguments[i] = *n;
    }
    let program = cached(it, decl, true)?;
    let span = (super::super::VM_FRAME_SLOTS + values.len() as u64) * 16;
    let top = it.vm_stack.last_mut()?;
    if top.size.saturating_sub(top.used) < span {
        return None;
    }
    let used = top.used;
    top.used += span;
    let result = run(it, decl, &program, &arguments[..values.len()]);
    let top = it.vm_stack.last_mut().unwrap();
    assert_eq!(top.used, used + span, "native callback root span leak");
    top.used = used;
    if result.is_some() {
        // Same scalar return side channels as the shared VM teardown. Pure
        // integer code owns no callback objects/temps needing a destructor pass.
        it.last_ret_cell = None;
        it.last_popped_frame = None;
        it.last_call_by_ref = false;
        it.send_line = Some(it.cur_line);
    }
    result
}
