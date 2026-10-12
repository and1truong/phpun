//! Native execution of the same transactional buffers as the portable region.
use super::super::{FeItem, Slot};
use super::{Compiled, Emitter, Interp, Op, Program, Value};
use crate::value::ArrKey;

/// Local/stack pair kinds shared by the emitted code and the Rust helpers.
pub(in crate::interp) const K_INT: i64 = 1;
pub(in crate::interp) const K_BOOL: i64 = 2;
pub(in crate::interp) const K_FLOAT: i64 = 3;
pub(in crate::interp) const K_PTR: i64 = 4;
pub(in crate::interp) const K_NULL: i64 = 5;

/// Context shared between generated code and Rust helper fns. Generated code
/// touches only `raw` and `stack`; the other fields
/// exist so helpers can reach canonical interpreter state. `raw`/`stack` stay
/// first so generated code reaches them at small fixed offsets.
#[repr(C)]
pub(in crate::interp) struct ObjCtx<'a> {
    /// Mirror of the used slots: [value, kind] — K_PTR means the canonical
    /// slot is authoritative; scalar kinds mean the pair is authoritative and
    /// the canonical slot only converges on `flush`.
    pub raw: [[i64; 2]; 256],
    /// Scalar value-stack pairs. Heap operands resume in canonical dispatch.
    pub stack: [[i64; 2]; 64],
    // Everything below is read only by Rust helpers, not by generated code.
    pub it: *mut Interp<'a>,
    pub comp: *const Compiled,
    pub slots: *mut Slot,
    pub nslots: usize,
    pub fe_states: *mut Vec<Option<std::vec::IntoIter<(ArrKey, FeItem)>>>,
    pub err: Option<crate::error::PhpError>,
    pub flow: Option<super::super::Flow>,
    pub paused: bool,
    pub tmark: usize,
    pub written: [u64; 4],
}

fn depths(
    ops: &[Op],
    nslots: usize,
    start: usize,
    end: usize,
    obj: bool,
) -> Option<Vec<Option<usize>>> {
    if end > ops.len() || start >= end || end - start > 512 || nslots > 256 {
        return None;
    }
    let mut depths: Vec<Option<usize>> = vec![None; end + 1];
    depths[start] = Some(0);
    let mut work = vec![start];
    while let Some(pc) = work.pop() {
        let n = depths[pc]?;
        if pc == end {
            if n != 0 {
                return None;
            }
            continue;
        }
        let op = &ops[pc];
        let (take, give) = match op {
            Op::Const(Value::Int(_) | Value::Bool(_)) | Op::DimCv { .. } => (0, 1),
            Op::Const(_) if obj => (0, 1),
            Op::Load(i) | Op::IncDec { slot: i, .. } | Op::BinaryCvConst(_, i, Value::Int(_))
                if (*i as usize) < nslots =>
            {
                (0, 1)
            }
            Op::BinaryCvConst(_, i, _) if obj && (*i as usize) < nslots => (0, 1),
            Op::MethodCallSlot { obj: i, .. } if obj && (*i as usize) < nslots => (0, 1),
            Op::Store(i) if (*i as usize) < nslots => (1, 1),
            Op::BinaryCv(_, i) | Op::BinaryCvStore(_, i) if (*i as usize) < nslots => (1, 1),
            Op::Binary(_) => (2, 1),
            Op::Not | Op::Boolify => (1, 1),
            Op::Pop | Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => (1, 0),
            Op::JumpCmp { a, b, .. } if (*a as usize) < nslots && (*b as usize) < nslots => (0, 0),
            Op::IncJump { slot: i, .. } if (*i as usize) < nslots => (0, 0),
            Op::FeNext { vslot, kslot, .. }
                if obj
                    && (*vslot as usize) < nslots
                    && (*kslot == u16::MAX || (*kslot as usize) < nslots) =>
            {
                (0, 0)
            }
            Op::Jump(_) | Op::Line(_) | Op::Sweep | Op::BoolLoop { .. } => (0, 0),
            _ => return None,
        };
        match op {
            Op::Binary(op)
            | Op::BinaryCv(op, _)
            | Op::BinaryCvConst(op, _, _)
            | Op::BinaryCvStore(op, _)
                if !matches!(
                    *op,
                    "+" | "-" | "*" | "%" | "==" | "===" | "!=" | "!==" | "<" | "<=" | ">" | ">="
                ) && !obj =>
            {
                return None
            }
            Op::JumpCmp { op, .. }
                if !matches!(*op, "<" | "<=" | ">" | ">=" | "==" | "!=" | "===" | "!==") && obj =>
            {
                return None
            }
            Op::JumpCmp { op, .. } if !matches!(*op, "<" | "<=" | ">" | ">=") => return None,
            Op::DimCv { .. } if obj => return None,
            Op::DimCv {
                key_slot: Some(i), ..
            } if (*i as usize) >= nslots => return None,
            Op::DimCv { write: Some(v), .. } if !matches!(v, Value::Bool(_)) => return None,
            _ => {}
        }
        let next = n.checked_sub(take)? + give;
        if next > 64 {
            return None;
        }
        let mut set = |target: usize| -> Option<()> {
            if target < start || target > end {
                return None;
            }
            match depths[target] {
                Some(previous) if previous != next => return None,
                Some(_) => {}
                None => {
                    depths[target] = Some(next);
                    work.push(target);
                }
            }
            Some(())
        };
        match op {
            Op::Jump(target) => set(*target)?,
            Op::JumpIfFalse(target) | Op::JumpIfTrue(target) | Op::JumpCmp { target, .. } => {
                set(*target)?;
                set(pc + 1)?;
            }
            Op::IncJump { target, .. } => set(*target)?,
            Op::FeNext { end: t, .. } => {
                set(*t)?;
                set(pc + 1)?;
            }
            _ => set(pc + 1)?,
        }
    }
    Some(depths)
}

impl Emitter {
    fn region_local(&mut self, slot: u16, integer: bool, fail: &mut Vec<usize>) {
        // R12 points to bounded [value, kind] pairs, kind0=uninit,1=int,2=bool.
        self.emit(&[0x49, 0x83, 0xbc, 0x24]);
        self.word(i32::from(slot) * 16 + 8);
        self.emit(&[if integer { 1 } else { 0 }]);
        fail.push(self.branch(Some(if integer { 0x85 } else { 0x84 })));
        self.emit(&[0x49, 0x8b, 0x84, 0x24]);
        self.word(i32::from(slot) * 16);
    }
    fn region_kind(&mut self, offset: i32, fail: &mut Vec<usize>) {
        self.emit(&[0x48, 0x83, 0xbc, 0x24]);
        self.word(offset + 8);
        self.emit(&[1]);
        fail.push(self.branch(Some(0x85)));
    }
    fn region_body(&mut self, c: &Compiled, start: usize, end: usize) -> Option<()> {
        let depths = depths(&c.ops, c.nslots, start, end, false)?;
        let max = depths.iter().flatten().copied().max().unwrap_or(0);
        let frame = i32::try_from((max + 1) * 16).ok()?;
        let value = |n: usize| i32::try_from(n * 16).ok();
        let mut fail = Vec::new();
        let mut jumps = Vec::new();
        let mut labels = vec![0; end + 1];
        // Preserve all callee-saved registers used; no CALL or VM frame exists.
        self.emit(&[
            0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x49, 0x89, 0xfc, 0x49, 0x89,
            0xf5, 0x49, 0x89, 0xd6, 0x49, 0x89, 0xcf, 0x48, 0x81, 0xec,
        ]);
        self.word(frame);
        self.emit(&[0xbb]);
        self.word(10_000_001);
        for pc in start..end {
            labels[pc] = self.bytes.len();
            let Some(n) = depths[pc] else {
                continue;
            };
            let op = &c.ops[pc];
            // Same finite per-op budget as portable speculation.
            self.emit(&[0x48, 0xff, 0xcb]);
            fail.push(self.branch(Some(0x84)));
            match op {
                Op::Const(v) => {
                    let (v, kind) = match v {
                        Value::Int(v) => (*v, 1),
                        Value::Bool(v) => (i64::from(*v), 2),
                        _ => return None,
                    };
                    self.immediate(v);
                    self.store(value(n)?);
                    self.flag(value(n)? + 8, kind);
                }
                Op::Load(i) => {
                    self.region_local(*i, false, &mut fail);
                    self.store(value(n)?);
                    self.emit(&[0x49, 0x8b, 0x84, 0x24]);
                    self.word(i32::from(*i) * 16 + 8);
                    self.store(value(n)? + 8);
                }
                Op::Store(i) => {
                    self.load(value(n - 1)?, false);
                    self.emit(&[0x49, 0x89, 0x84, 0x24]);
                    self.word(i32::from(*i) * 16);
                    self.load(value(n - 1)? + 8, false);
                    self.emit(&[0x49, 0x89, 0x84, 0x24]);
                    self.word(i32::from(*i) * 16 + 8);
                }
                Op::Binary(op) | Op::BinaryCv(op, _) | Op::BinaryCvConst(op, _, _) => {
                    let out = match *op {
                        "+" | "-" | "*" | "%" => 1,
                        _ => 2,
                    };
                    let dest = match &c.ops[pc] {
                        Op::Binary(_) => {
                            self.region_kind(value(n - 2)?, &mut fail);
                            self.region_kind(value(n - 1)?, &mut fail);
                            self.load(value(n - 2)?, false);
                            self.load(value(n - 1)?, true);
                            value(n - 2)?
                        }
                        Op::BinaryCv(_, i) => {
                            self.region_kind(value(n - 1)?, &mut fail);
                            self.region_local(*i, true, &mut fail);
                            self.load(value(n - 1)?, true);
                            value(n - 1)?
                        }
                        Op::BinaryCvConst(_, i, Value::Int(rhs)) => {
                            self.region_local(*i, true, &mut fail);
                            self.emit(&[0x48, 0xba]);
                            self.emit(&rhs.to_le_bytes());
                            value(n)?
                        }
                        _ => return None,
                    };
                    self.binary(op, &mut fail);
                    self.store(dest);
                    self.flag(dest + 8, out);
                }
                Op::IncDec { slot, delta, post } => {
                    self.region_local(*slot, true, &mut fail);
                    if *post {
                        self.store(value(n)?);
                    }
                    self.emit(&[0x48, 0xba]);
                    self.emit(&delta.to_le_bytes());
                    self.binary("+", &mut fail);
                    self.emit(&[0x49, 0x89, 0x84, 0x24]);
                    self.word(i32::from(*slot) * 16);
                    if !*post {
                        self.store(value(n)?);
                    }
                    self.flag(value(n)? + 8, 1);
                }
                Op::BinaryCvStore(op, i) => {
                    let out = match *op {
                        "+" | "-" | "*" | "%" => 1,
                        _ => 2,
                    };
                    self.region_local(*i, true, &mut fail);
                    self.region_kind(value(n - 1)?, &mut fail);
                    self.load(value(n - 1)?, true);
                    self.binary(op, &mut fail);
                    self.emit(&[0x49, 0x89, 0x84, 0x24]);
                    self.word(i32::from(*i) * 16);
                    self.emit(&[0x49, 0xc7, 0x84, 0x24]);
                    self.word(i32::from(*i) * 16 + 8);
                    self.word(out);
                    self.store(value(n - 1)?);
                    self.flag(value(n - 1)? + 8, out);
                }
                Op::DimCv {
                    key_slot,
                    key_int,
                    write,
                    ..
                } => {
                    match key_slot {
                        Some(i) => self.region_local(*i, true, &mut fail),
                        None => self.immediate(*key_int),
                    }
                    // Unsigned bounds reject negative keys too; R13 owns byte0/1 values.
                    self.emit(&[0x4c, 0x39, 0xf0]);
                    fail.push(self.branch(Some(0x83)));
                    if let Some(Value::Bool(v)) = write {
                        self.emit(&[0x41, 0xc6, 0x44, 0x05, 0x00, u8::from(*v)]);
                    }
                    self.emit(&[0x41, 0x0f, 0xb6, 0x44, 0x05, 0x00]);
                    self.store(value(n)?);
                    self.flag(value(n)? + 8, 2);
                }
                Op::Not | Op::Boolify => {
                    self.load(value(n - 1)?, false);
                    self.emit(&[
                        0x48,
                        0x85,
                        0xc0,
                        0x0f,
                        if matches!(op, Op::Not) { 0x94 } else { 0x95 },
                        0xc0,
                        0x48,
                        0x0f,
                        0xb6,
                        0xc0,
                    ]);
                    self.store(value(n - 1)?);
                    self.flag(value(n - 1)? + 8, 2);
                }
                Op::Jump(t) => jumps.push((self.branch(None), *t)),
                Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                    self.load(value(n - 1)?, false);
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
                Op::JumpCmp { a, b, op, target } => {
                    // Both operands are slot ints: kind-check + load a
                    // into RAX, then b into RDX, cmp, branch on zero.
                    self.region_local(*a, true, &mut fail);
                    self.emit(&[0x49, 0x83, 0xbc, 0x24]);
                    self.word(i32::from(*b) * 16 + 8);
                    self.emit(&[1]);
                    fail.push(self.branch(Some(0x85)));
                    self.emit(&[0x49, 0x8b, 0x94, 0x24]);
                    self.word(i32::from(*b) * 16);
                    self.binary(op, &mut fail);
                    self.emit(&[0x48, 0x85, 0xc0]);
                    jumps.push((self.branch(Some(0x84)), *target));
                }
                Op::IncJump {
                    slot,
                    delta,
                    target,
                } => {
                    self.region_local(*slot, true, &mut fail);
                    self.emit(&[0x48, 0xba]);
                    self.emit(&delta.to_le_bytes());
                    self.binary("+", &mut fail);
                    self.emit(&[0x49, 0x89, 0x84, 0x24]);
                    self.word(i32::from(*slot) * 16);
                    jumps.push((self.branch(None), *target));
                }
                Op::Line(line) => {
                    self.immediate(i64::try_from(*line).ok()?);
                    self.emit(&[0x49, 0x89, 0x07]);
                }
                Op::Pop | Op::Sweep | Op::BoolLoop { .. } => {}
                _ => return None,
            }
        }
        labels[end] = self.bytes.len();
        self.emit(&[0xb0, 1]);
        let done = self.branch(None);
        let failure = self.bytes.len();
        self.emit(&[0xb0, 0]);
        let teardown = self.bytes.len();
        self.emit(&[0x48, 0x81, 0xc4]);
        self.word(frame);
        self.emit(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5b, 0xc3]);
        self.patch(done, teardown)?;
        for at in fail {
            self.patch(at, failure)?;
        }
        for (at, target) in jumps {
            self.patch(at, *labels.get(target)?)?;
        }
        Some(())
    }
}

impl Program {
    pub(in crate::interp) fn region(
        it: &Interp<'_>,
        c: &Compiled,
        start: usize,
        end: usize,
    ) -> Option<Self> {
        let mut emitter = Emitter {
            bytes: Vec::new(),
            calls: Vec::new(),
        };
        emitter.region_body(c, start, end)?;
        // ponytail: reuse the existing8MiB/interpreter W^X budget and cap each
        // region at64KiB. A register emitter needs measured dispatch/copy costs
        // before lifting this same-op stack ceiling.
        if emitter.bytes.len() > 65536 {
            return None;
        }
        Self::map(
            &emitter.bytes,
            Vec::new(),
            Self::mapped_bytes(it),
            Some(it.native_regions_bytes.clone()),
        )
    }
    pub(in crate::interp) fn run_region(
        &self,
        locals: &mut [[i64; 2]],
        bools: &mut [bool],
        line: &mut usize,
    ) -> bool {
        // SAFETY: generated code has verified CV/stack/branch bounds, byte-array
        // bounds checks and a finite budget. It receives private owned buffers,
        // writes only bool bytes0/1, preserves the ABI and never calls PHP/Rust.
        // Program owns an RX mapping for this entire synchronous invocation.
        let f: unsafe extern "C" fn(*mut [i64; 2], *mut u8, usize, *mut usize) -> u8 =
            unsafe { std::mem::transmute(self.code) };
        let okay = unsafe {
            f(
                locals.as_mut_ptr(),
                bools.as_mut_ptr().cast(),
                bools.len(),
                line,
            )
        } != 0;
        if okay {
            self.executions.set(self.executions.get().saturating_add(1));
        }
        okay
    }
}

// ====================== object-capable regions ======================
//
// Same depth/branch validation, scalar arithmetic/copies inline, and SysV
// helpers for snapshot fetches and proven float leaves. All potentially
// observable PHP callbacks resume at the exact PC, never replay the loop.
// ponytail: allocation/append and general calls stay canonical; helper-only
// construction regressed instructions, revisit with measured inline kernels.

const OBJ_BUDGET: i32 = 10_000_001;
const O_STK: i32 = core::mem::offset_of!(ObjCtx, stack) as i32;

type ObjFail = Vec<(usize, usize)>;

/// Ops an object region can emit — a native fast arm or a helper call.
fn obj_supported(op: &Op) -> bool {
    matches!(
        op,
        Op::Const(Value::Int(_) | Value::Bool(_) | Value::Float(_) | Value::Null)
            | Op::Load(_)
            | Op::Store(_)
            | Op::Pop
            | Op::Binary(_)
            | Op::BinaryCv(..)
            | Op::BinaryCvConst(_, _, Value::Int(_))
            | Op::BinaryCvStore(..)
            | Op::Not
            | Op::Boolify
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::JumpIfTrue(_)
            | Op::JumpCmp { .. }
            | Op::IncJump { .. }
            | Op::IncDec { .. }
            | Op::Line(_)
            | Op::Sweep
            | Op::BoolLoop { .. }
            | Op::MethodCallSlot { .. }
            | Op::FeNext { .. }
    )
}

impl Emitter {
    fn o_ld(&mut self, off: i32) {
        self.emit(&[0x49, 0x8b, 0x84, 0x24]);
        self.word(off);
    }
    fn o_st(&mut self, off: i32) {
        self.emit(&[0x49, 0x89, 0x84, 0x24]);
        self.word(off);
    }
    fn o_ld2(&mut self, off: i32) {
        self.emit(&[0x49, 0x8b, 0x94, 0x24]);
        self.word(off);
    }
    fn o_flag(&mut self, off: i32, v: i32) {
        self.emit(&[0x49, 0xc7, 0x84, 0x24]);
        self.word(off);
        self.word(v);
    }
    /// movsd xmmN, [r12+off] — N ∈ {0,1}.
    fn o_movsd(&mut self, xmm: u8, off: i32) {
        self.emit(&[0xf2, 0x41, 0x0f, 0x10, 0x84 + (xmm << 3), 0x24]);
        self.word(off);
    }
    fn o_movsd_st(&mut self, xmm: u8, off: i32) {
        self.emit(&[0xf2, 0x41, 0x0f, 0x11, 0x84 + (xmm << 3), 0x24]);
        self.word(off);
    }
    /// cvtsi2sd xmmN, qword [r12+off].
    fn o_cvtsi(&mut self, xmm: u8, off: i32) {
        self.emit(&[0xf2, 0x49, 0x0f, 0x2a, 0x84 + (xmm << 3), 0x24]);
        self.word(off);
    }
    /// movq xmmN, rdx.
    fn o_movq(&mut self, xmm: u8) {
        self.emit(&[0x66, 0x48, 0x0f, 0x6e, 0xc2 + (xmm << 3)]);
    }
    /// SysV helper: rdi=ctx, esi=pc, edx=n (stack depth), call rax,
    /// test al, jz fail(pc).
    fn o_call(&mut self, helper: usize, pc: usize, n: usize, fail: &mut ObjFail) {
        self.emit(&[0x4c, 0x89, 0xe7, 0xbe]);
        self.word(i32::try_from(pc).unwrap_or(0));
        self.emit(&[0xba]);
        self.word(i32::try_from(n).unwrap_or(0));
        self.emit(&[0x48, 0xb8]);
        let b = helper.to_le_bytes();
        self.emit(&b);
        self.emit(&[0xff, 0xd0, 0x84, 0xc0]);
        fail.push((self.branch(Some(0x84)), pc));
    }
    fn o_fail(&mut self, pc: usize, fail: &mut ObjFail) {
        fail.push((self.branch(None), pc));
    }
    /// Emits `je → patch-site` when the pair-kind at `off` equals `kind`.
    fn o_jkind(&mut self, off: i32, kind: i64) -> usize {
        self.o_jcc_kind(off, kind, 0x84)
    }
    /// Emits `jne → patch-site` when the pair-kind at `off` differs.
    fn o_jkind_ne(&mut self, off: i32, kind: i64) -> usize {
        self.o_jcc_kind(off, kind, 0x85)
    }
    fn o_jcc_kind(&mut self, off: i32, kind: i64, cc: u8) -> usize {
        self.emit(&[0x49, 0x83, 0xbc, 0x24]);
        self.word(off + 8);
        self.emit(&[kind as u8]);
        self.branch(Some(cc))
    }
    /// xmmN = f64 of pair at `val` (kind in `reg`: 3 → movsd, else cvtsi).
    fn o_tof64(&mut self, xmm: u8, reg: u8, val: i32) {
        self.emit(&[0x48, 0x83, 0xf8 + reg, 3]);
        let jsse = self.branch(Some(0x84));
        self.o_cvtsi(xmm, val);
        let jdone = self.branch(None);
        let at = self.bytes.len();
        self.o_movsd(xmm, val);
        let done = self.bytes.len();
        let _ = self.patch(jsse, at);
        let _ = self.patch(jdone, done);
    }
    /// ucomisd xmm0,xmm1 → rax = bool per `op`, NaN-correct.
    fn o_fcmp(&mut self, op: &str) {
        self.emit(&[0x66, 0x0f, 0x2e, 0xc1]);
        match op {
            "<" | "<=" | "==" | "===" => {
                self.emit(&[0x0f, 0x9b, 0xc1]); // setnp cl
                self.emit(&[
                    0x0f,
                    match op {
                        "<" => 0x92,
                        "<=" => 0x96,
                        _ => 0x94,
                    },
                    0xc0,
                ]);
                self.emit(&[0x20, 0xc8]); // and al,cl
            }
            "!=" | "!==" => {
                self.emit(&[0x0f, 0x9a, 0xc1, 0x0f, 0x95, 0xc0, 0x08, 0xc8]);
            }
            ">" => self.emit(&[0x0f, 0x97, 0xc0]),
            ">=" => self.emit(&[0x0f, 0x93, 0xc0]),
            _ => {}
        }
        self.emit(&[0x48, 0x0f, 0xb6, 0xc0]); // movzx rax,al
    }
}

impl Emitter {
    /// Binary-family fast arms: int×int inline, numeric pairs via SSE, the
    /// rest resumes in canonical dispatch.
    /// Returns the list of `jmp done` sites to patch after the failure branch.
    #[allow(clippy::too_many_arguments)]
    fn o_arith(
        &mut self,
        pc: usize,
        op: &str,
        a: i32,
        b: i32,
        b_const: Option<i64>,
        dest: i32,
        dest2: Option<i32>,
        fail: &mut ObjFail,
    ) -> Option<Vec<usize>> {
        let int_ok = matches!(
            op,
            "+" | "-" | "*" | "%" | "==" | "===" | "!=" | "!==" | "<" | "<=" | ">" | ">="
        );
        let flt_ok = matches!(op, "+" | "-" | "*" | "==" | "!=" | "<" | "<=" | ">" | ">=")
            && !matches!(op, "===" | "!==");
        let mut pend_next = Vec::new();
        let mut pend_done = Vec::new();
        if int_ok {
            pend_next.push(self.o_jkind_ne(a, K_INT));
            if b_const.is_none() {
                pend_next.push(self.o_jkind_ne(b, K_INT));
            }
            self.o_ld(a);
            if let Some(v) = b_const {
                self.emit(&[0x48, 0xba]);
                let b8 = v.to_le_bytes();
                self.emit(&b8);
            } else {
                self.o_ld2(b);
            }
            let mut tf = Vec::new();
            self.binary(op, &mut tf);
            fail.extend(tf.into_iter().map(|at| (at, pc)));
            self.o_st(dest);
            if let Some(d2) = dest2 {
                self.o_st(d2);
            }
            let k = if matches!(op, "+" | "-" | "*" | "%") {
                K_INT
            } else {
                K_BOOL
            } as i32;
            self.o_flag(dest + 8, k);
            if let Some(d2) = dest2 {
                self.o_flag(d2 + 8, k);
            }
            pend_done.push(self.branch(None));
        }
        let next = self.bytes.len();
        for at in pend_next.drain(..) {
            self.patch(at, next)?;
        }
        if flt_ok {
            // numeric gate: kind ∈ {1,3} ⇔ (kind | 2) == 3
            self.o_ld(a + 8);
            self.emit(&[0x48, 0x83, 0xc8, 2, 0x48, 0x83, 0xf8, 3]);
            pend_next.push(self.branch(Some(0x85)));
            if b_const.is_none() {
                self.o_ld(b + 8);
                self.emit(&[0x48, 0x83, 0xc8, 2, 0x48, 0x83, 0xf8, 3]);
                pend_next.push(self.branch(Some(0x85)));
            }
            self.o_ld(a + 8);
            self.o_tof64(0, 0, a);
            if let Some(v) = b_const {
                self.emit(&[0x48, 0xba]);
                let b8 = (v as f64).to_bits().to_le_bytes();
                self.emit(&b8);
                self.o_movq(1);
            } else {
                self.o_ld2(b + 8);
                self.o_tof64(1, 2, b);
            }
            match op {
                "+" => self.emit(&[0xf2, 0x0f, 0x58, 0xc1]),
                "-" => self.emit(&[0xf2, 0x0f, 0x5c, 0xc1]),
                "*" => self.emit(&[0xf2, 0x0f, 0x59, 0xc1]),
                _ => {
                    self.o_fcmp(op);
                    self.o_st(dest);
                    if let Some(d2) = dest2 {
                        self.o_st(d2);
                    }
                    self.o_flag(dest + 8, K_BOOL as i32);
                    if let Some(d2) = dest2 {
                        self.o_flag(d2 + 8, K_BOOL as i32);
                    }
                    pend_done.push(self.branch(None));
                }
            }
            if matches!(op, "+" | "-" | "*") {
                self.o_movsd_st(0, dest);
                if let Some(d2) = dest2 {
                    self.o_movsd_st(0, d2);
                }
                self.o_flag(dest + 8, K_FLOAT as i32);
                if let Some(d2) = dest2 {
                    self.o_flag(d2 + 8, K_FLOAT as i32);
                }
                pend_done.push(self.branch(None));
            }
        }
        let help = self.bytes.len();
        for at in pend_next.drain(..) {
            self.patch(at, help)?;
        }
        Some(pend_done)
    }

    /// Whole-region emitter for object-capable ranges. R12 = ctx pointer.
    fn obj_region_body(&mut self, c: &Compiled, start: usize, end: usize) -> Option<()> {
        let depths = depths(&c.ops, c.nslots, start, end, true)?;
        if !c.ops[start..end].iter().all(obj_supported) {
            return None;
        }
        let mut fail: ObjFail = Vec::new();
        let mut jumps: Vec<(usize, usize)> = Vec::new();
        let mut labels = vec![0usize; end + 1];
        let stk = |n: usize| O_STK + n as i32 * 16;
        self.emit(&[
            0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x49, 0x89, 0xfc, 0xbb,
        ]);
        self.word(OBJ_BUDGET);
        for pc in start..end {
            labels[pc] = self.bytes.len();
            let Some(n) = depths[pc] else {
                continue;
            };
            self.emit(&[0x48, 0xff, 0xcb]);
            fail.push((self.branch(Some(0x84)), pc));
            match &c.ops[pc] {
                Op::Const(v) => {
                    let (val, kind) = match v {
                        Value::Int(v) => (*v, K_INT),
                        Value::Bool(v) => (i64::from(*v), K_BOOL),
                        Value::Float(v) => (v.to_bits() as i64, K_FLOAT),
                        Value::Null => (0, K_NULL),
                        _ => {
                            return None;
                        }
                    };
                    self.immediate(val);
                    self.o_st(stk(n));
                    self.o_flag(stk(n) + 8, kind as i32);
                }
                Op::Load(i) => {
                    let loc = i32::from(*i) * 16;
                    let j1 = self.o_jkind(loc, K_PTR);
                    self.emit(&[0x49, 0x83, 0xbc, 0x24]);
                    self.word(loc + 8);
                    self.emit(&[0]);
                    let j2 = self.branch(Some(0x84));
                    self.o_ld(loc);
                    self.o_st(stk(n));
                    self.o_ld(loc + 8);
                    self.o_st(stk(n) + 8);
                    let jdone = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(j1, help)?;
                    self.patch(j2, help)?;
                    self.o_fail(pc, &mut fail);
                    self.patch(jdone, self.bytes.len())?;
                }
                Op::Store(i) => {
                    let loc = i32::from(*i) * 16;
                    let j1 = self.o_jkind(stk(n - 1), K_PTR);
                    let j2 = self.o_jkind(loc, K_PTR);
                    self.o_ld(stk(n - 1));
                    self.o_st(loc);
                    self.o_ld(stk(n - 1) + 8);
                    self.o_st(loc + 8);
                    let jdone = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(j1, help)?;
                    self.patch(j2, help)?;
                    self.o_fail(pc, &mut fail);
                    self.patch(jdone, self.bytes.len())?;
                }
                Op::Binary(op) => {
                    let done = self.o_arith(
                        pc,
                        op,
                        stk(n - 2),
                        stk(n - 1),
                        None,
                        stk(n - 2),
                        None,
                        &mut fail,
                    )?;
                    self.o_fail(pc, &mut fail);
                    for at in done {
                        self.patch(at, self.bytes.len())?;
                    }
                }
                Op::BinaryCv(op, i) => {
                    let done = self.o_arith(
                        pc,
                        op,
                        i32::from(*i) * 16,
                        stk(n - 1),
                        None,
                        stk(n - 1),
                        None,
                        &mut fail,
                    )?;
                    self.o_fail(pc, &mut fail);
                    for at in done {
                        self.patch(at, self.bytes.len())?;
                    }
                }
                Op::BinaryCvConst(op, i, v) => {
                    let b = match v {
                        Value::Int(v) => *v,
                        _ => {
                            self.o_fail(pc, &mut fail);
                            continue;
                        }
                    };
                    let done = self.o_arith(
                        pc,
                        op,
                        i32::from(*i) * 16,
                        0,
                        Some(b),
                        stk(n),
                        None,
                        &mut fail,
                    )?;
                    self.o_fail(pc, &mut fail);
                    for at in done {
                        self.patch(at, self.bytes.len())?;
                    }
                }
                Op::BinaryCvStore(op, i) => {
                    let done = self.o_arith(
                        pc,
                        op,
                        i32::from(*i) * 16,
                        stk(n - 1),
                        None,
                        stk(n - 1),
                        Some(i32::from(*i) * 16),
                        &mut fail,
                    )?;
                    self.o_fail(pc, &mut fail);
                    for at in done {
                        self.patch(at, self.bytes.len())?;
                    }
                }
                Op::Not | Op::Boolify => {
                    // kind4 → helper; kind3 → float test; else int test.
                    let jptr = self.o_jkind(stk(n - 1), K_PTR);
                    let jflt = self.o_jkind(stk(n - 1), K_FLOAT);
                    self.o_ld(stk(n - 1));
                    self.emit(&[0x48, 0x85, 0xc0]); // test rax,rax
                    self.emit(&[0x0f, 0x95, 0xc0, 0x48, 0x0f, 0xb6, 0xc0]); // setne al;movzx
                    let jdone1 = self.branch(None);
                    let fat = self.bytes.len();
                    self.patch(jflt, fat)?;
                    self.o_movsd(0, stk(n - 1));
                    self.emit(&[0x66, 0x0f, 0x57, 0xc9, 0x66, 0x0f, 0x2e, 0xc1]);
                    self.emit(&[0x0f, 0x9a, 0xc1, 0x0f, 0x95, 0xc0, 0x08, 0xc8]);
                    self.emit(&[0x48, 0x0f, 0xb6, 0xc0]);
                    let jdone2 = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(jptr, help)?;
                    self.o_fail(pc, &mut fail);
                    let done = self.bytes.len();
                    self.patch(jdone1, done)?;
                    self.patch(jdone2, done)?;
                    if matches!(&c.ops[pc], Op::Not) {
                        self.emit(&[0x48, 0x83, 0xf0, 1]); // xor rax,1
                    }
                    self.o_st(stk(n - 1));
                    self.o_flag(stk(n - 1) + 8, K_BOOL as i32);
                }
                Op::Pop => {
                    fail.push((self.o_jkind(stk(n - 1), K_PTR), pc));
                    // Entry refuses last_ret_cell, and no emitted op sets it.
                }
                Op::Jump(t) => jumps.push((self.branch(None), *t)),
                Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
                    let jptr = self.o_jkind(stk(n - 1), K_PTR);
                    let jflt = self.o_jkind(stk(n - 1), K_FLOAT);
                    self.o_ld(stk(n - 1));
                    self.emit(&[0x48, 0x85, 0xc0]);
                    let jdone1 = self.branch(None);
                    let fat = self.bytes.len();
                    self.patch(jflt, fat)?;
                    self.o_movsd(0, stk(n - 1));
                    self.emit(&[0x66, 0x0f, 0x57, 0xc9, 0x66, 0x0f, 0x2e, 0xc1]);
                    self.emit(&[0x0f, 0x9a, 0xc1, 0x0f, 0x95, 0xc0, 0x08, 0xc8]);
                    self.emit(&[0x48, 0x0f, 0xb6, 0xc0]);
                    let jdone2 = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(jptr, help)?;
                    self.o_fail(pc, &mut fail);
                    let done = self.bytes.len();
                    self.patch(jdone1, done)?;
                    self.patch(jdone2, done)?;
                    self.emit(&[0x48, 0x85, 0xc0]);
                    jumps.push((
                        self.branch(Some(if matches!(&c.ops[pc], Op::JumpIfFalse(_)) {
                            0x84
                        } else {
                            0x85
                        })),
                        *t,
                    ));
                }
                Op::JumpCmp { a, b, op, target } => {
                    let al = i32::from(*a) * 16;
                    let bl = i32::from(*b) * 16;
                    let mut pend = Vec::new();
                    if matches!(*op, "<" | "<=" | ">" | ">=" | "==" | "!=" | "===" | "!==") {
                        pend.push((self.o_jkind_ne(al, K_INT), false));
                        pend.push((self.o_jkind_ne(bl, K_INT), false));
                        self.o_ld(al);
                        self.o_ld2(bl);
                        let mut tf = Vec::new();
                        self.binary(op, &mut tf);
                        fail.extend(tf.into_iter().map(|at| (at, pc)));
                        self.emit(&[0x48, 0x85, 0xc0]);
                        let jt = self.branch(Some(0x84));
                        let jdone = self.branch(None);
                        let next = self.bytes.len();
                        for (at, _) in pend.drain(..) {
                            self.patch(at, next)?;
                        }
                        if !matches!(*op, "===" | "!==") {
                            // numeric gate on both locals.
                            self.o_ld(al + 8);
                            self.emit(&[0x48, 0x83, 0xc8, 2, 0x48, 0x83, 0xf8, 3]);
                            let jh1 = self.branch(Some(0x85));
                            self.o_ld(bl + 8);
                            self.emit(&[0x48, 0x83, 0xc8, 2, 0x48, 0x83, 0xf8, 3]);
                            let jh2 = self.branch(Some(0x85));
                            self.o_ld(al + 8);
                            self.o_tof64(0, 0, al);
                            self.o_ld2(bl + 8);
                            self.o_tof64(1, 2, bl);
                            self.o_fcmp(op);
                            self.emit(&[0x48, 0x85, 0xc0]);
                            let jt2 = self.branch(Some(0x84));
                            let jdone2 = self.branch(None);
                            let help = self.bytes.len();
                            self.patch(jh1, help)?;
                            self.patch(jh2, help)?;
                            self.o_fail(pc, &mut fail);
                            let done = self.bytes.len();
                            jumps.push((jt, *target));
                            jumps.push((jt2, *target));
                            self.patch(jdone, done)?;
                            self.patch(jdone2, done)?;
                        } else {
                            // strict compare: int-int only, else helper.
                            let help = self.bytes.len();
                            self.o_fail(pc, &mut fail);
                            let done = self.bytes.len();
                            let _ = help;
                            jumps.push((jt, *target));
                            self.patch(jdone, done)?;
                        }
                    } else {
                        self.o_fail(pc, &mut fail);
                    }
                }
                Op::IncJump {
                    slot,
                    delta,
                    target,
                } => {
                    let loc = i32::from(*slot) * 16;
                    let jint = self.o_jkind_ne(loc, K_INT);
                    self.o_ld(loc);
                    self.emit(&[0x48, 0xba]);
                    let b = delta.to_le_bytes();
                    self.emit(&b);
                    let mut tf = Vec::new();
                    self.binary("+", &mut tf);
                    fail.extend(tf.into_iter().map(|at| (at, pc)));
                    self.o_st(loc);
                    let jdone = self.branch(None);
                    let fpath = self.bytes.len();
                    self.patch(jint, fpath)?;
                    let jhelp = self.o_jkind_ne(loc, K_FLOAT);
                    self.o_movsd(0, loc);
                    self.emit(&[0x48, 0xba]);
                    let bits = (*delta as f64).to_bits().to_le_bytes();
                    self.emit(&bits);
                    self.o_movq(1);
                    self.emit(&[0xf2, 0x0f, 0x58, 0xc1]);
                    self.o_movsd_st(0, loc);
                    let jdone2 = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(jhelp, help)?;
                    self.o_fail(pc, &mut fail);
                    let done = self.bytes.len();
                    self.patch(jdone, done)?;
                    self.patch(jdone2, done)?;
                    jumps.push((self.branch(None), *target));
                }
                Op::IncDec { slot, delta, post } => {
                    let loc = i32::from(*slot) * 16;
                    let jint = self.o_jkind_ne(loc, K_INT);
                    self.o_ld(loc);
                    if *post {
                        self.o_st(stk(n));
                        self.o_flag(stk(n) + 8, K_INT as i32);
                    }
                    self.emit(&[0x48, 0xba]);
                    let b = delta.to_le_bytes();
                    self.emit(&b);
                    let mut tf = Vec::new();
                    self.binary("+", &mut tf);
                    fail.extend(tf.into_iter().map(|at| (at, pc)));
                    self.o_st(loc);
                    if !*post {
                        self.o_st(stk(n));
                        self.o_flag(stk(n) + 8, K_INT as i32);
                    }
                    let jdone = self.branch(None);
                    let fpath = self.bytes.len();
                    self.patch(jint, fpath)?;
                    let jhelp = self.o_jkind_ne(loc, K_FLOAT);
                    self.o_movsd(0, loc);
                    if *post {
                        self.o_movsd_st(0, stk(n));
                        self.o_flag(stk(n) + 8, K_FLOAT as i32);
                    }
                    self.emit(&[0x48, 0xba]);
                    let bits = (*delta as f64).to_bits().to_le_bytes();
                    self.emit(&bits);
                    self.o_movq(1);
                    self.emit(&[0xf2, 0x0f, 0x58, 0xc1]);
                    self.o_movsd_st(0, loc);
                    if !*post {
                        self.o_movsd_st(0, stk(n));
                        self.o_flag(stk(n) + 8, K_FLOAT as i32);
                    }
                    let jdone2 = self.branch(None);
                    let help = self.bytes.len();
                    self.patch(jhelp, help)?;
                    self.o_fail(pc, &mut fail);
                    let done = self.bytes.len();
                    self.patch(jdone, done)?;
                    self.patch(jdone2, done)?;
                }
                Op::Line(_) => self.o_call(Interp::rh_line as *const () as usize, pc, n, &mut fail),
                Op::Sweep => self.o_call(Interp::rh_sweep as *const () as usize, pc, n, &mut fail),
                Op::MethodCallSlot { .. } => {
                    self.o_call(Interp::rh_mcallslot as *const () as usize, pc, n, &mut fail)
                }
                Op::FeNext { end: t, .. } => {
                    self.o_call(Interp::rh_fenext as *const () as usize, pc, n, &mut fail);
                    self.emit(&[0x3c, 2]);
                    jumps.push((self.branch(Some(0x84)), *t));
                }
                Op::BoolLoop { .. } => {}
                _ => return None,
            }
        }
        labels[end] = self.bytes.len();
        self.emit(&[0xb8]);
        self.word(i32::try_from(end).ok()?);
        let exit = self.branch(None);
        // Per-pc fail stubs: mov eax, pc; jmp teardown.
        let mut stub_jmps = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut stub_at = std::collections::BTreeMap::new();
        for (_, pc) in &fail {
            if seen.insert(*pc) {
                stub_at.insert(*pc, self.bytes.len());
                self.emit(&[0xb8]);
                self.word(*pc as i32);
                stub_jmps.push(self.branch(None));
            }
        }
        let teardown = self.bytes.len();
        self.emit(&[0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5b, 0xc3]);
        self.patch(exit, teardown)?;
        for at in stub_jmps {
            self.patch(at, teardown)?;
        }
        for (at, pc) in fail {
            self.patch(at, stub_at[&pc])?;
        }
        for (at, target) in jumps {
            self.patch(at, *labels.get(target)?)?;
        }
        Some(())
    }
}

impl ObjCtx<'_> {
    /// Stack pair `n` as a canonical scalar Value.
    pub(in crate::interp) fn stack_value(&mut self, n: usize) -> Value {
        let v = self.stack[n][0];
        match self.stack[n][1] {
            K_INT => Value::Int(v),
            K_BOOL => Value::Bool(v != 0),
            K_FLOAT => Value::Float(f64::from_bits(v as u64)),
            K_NULL => Value::Null,
            _ => unreachable!("native stack contains only scalar pairs"),
        }
    }
    /// Publishes a float-leaf result onto the scalar stack.
    pub(in crate::interp) fn stack_put(&mut self, n: usize, v: Value) {
        match v {
            Value::Int(v) => {
                self.stack[n] = [v, K_INT];
            }
            Value::Bool(v) => {
                self.stack[n] = [i64::from(v), K_BOOL];
            }
            Value::Float(v) => {
                self.stack[n] = [v.to_bits() as i64, K_FLOAT];
            }
            Value::Null => {
                self.stack[n] = [0, K_NULL];
            }
            _ => unreachable!("float leaf returned a heap value"),
        }
    }
    /// Writes a scalar local pair back into the canonical slot (ptr-kind
    /// pairs are already authoritative — they are written through).
    pub(in crate::interp) unsafe fn flush(&mut self) -> Result<(), crate::error::PhpError> {
        for i in 0..self.nslots {
            if (self.written[i / 64] >> (i % 64)) & 1 == 0 {
                continue;
            }
            let kind = self.raw[i][1];
            if kind == K_PTR || kind == 0 {
                continue;
            }
            let v = match kind {
                K_INT => Value::Int(self.raw[i][0]),
                K_BOOL => Value::Bool(self.raw[i][0] != 0),
                K_FLOAT => Value::Float(f64::from_bits(self.raw[i][0] as u64)),
                _ => Value::Null,
            };
            let it = &mut *self.it;
            let slots = std::slice::from_raw_parts_mut(self.slots, self.nslots);
            match &mut slots[i] {
                Slot::C(c) => {
                    let c = c.clone();
                    it.cell_store(&c, v)?;
                }
                Slot::V(sv) => *sv = v,
                Slot::Uninit => slots[i] = Slot::V(v),
                Slot::Arg(j) => {
                    it.cur().value_args[*j as usize] = v;
                }
            }
            it.vm_publish_global(&*self.comp, slots, i as u16);
        }
        Ok(())
    }
    /// Re-mirrors one slot after a canonical write (helper-side sync).
    ///
    /// SAFETY: `slots`/`it`/`nslots` remain valid during native execution.
    pub(in crate::interp) unsafe fn resync(&mut self, i: usize) {
        let slots = std::slice::from_raw_parts_mut(self.slots, self.nslots);
        self.raw[i] = match &slots[i] {
            Slot::Uninit => [0, 0],
            Slot::V(v) => pair_of(v),
            Slot::C(c) => pair_of(&c.borrow()),
            Slot::Arg(j) => pair_of(&(*self.it).cur().value_args[*j as usize]),
        };
    }
}

pub(in crate::interp) fn pair_of(v: &Value) -> [i64; 2] {
    match v {
        Value::Int(v) => [*v, K_INT],
        Value::Bool(v) => [i64::from(*v), K_BOOL],
        Value::Float(v) => [v.to_bits() as i64, K_FLOAT],
        Value::Null => [0, K_NULL],
        _ => [0, K_PTR],
    }
}

impl Program {
    /// Compile-time entry: validates and emits an object-capable region.
    pub(in crate::interp) fn obj_region(
        it: &mut Interp<'_>,
        c: &Compiled,
        start: usize,
        end: usize,
    ) -> Option<(Self, Vec<Option<usize>>)> {
        let depths = depths(&c.ops, c.nslots, start, end, true)?;
        if !c.ops[start..end].iter().all(obj_supported) {
            return None;
        }
        let mut e = Emitter {
            bytes: Vec::new(),
            calls: Vec::new(),
        };
        e.obj_region_body(c, start, end)?;
        if e.bytes.len() > 65536 {
            return None;
        }
        let p = Self::map(
            &e.bytes,
            Vec::new(),
            Self::mapped_bytes(it),
            Some(it.native_regions_bytes.clone()),
        )?;
        Some((p, depths))
    }

    /// Runs an object region; returns the pc where it stopped (end = done).
    /// SAFETY: emitted code writes only inside ObjCtx fields, budget-bounds
    /// every op, and calls helpers that preserve canonical semantics.
    pub(in crate::interp) fn run_obj_region(&self, ctx: &mut ObjCtx) -> usize {
        let f: unsafe extern "C" fn(*mut ObjCtx) -> usize =
            unsafe { std::mem::transmute(self.code) };
        unsafe { f(ctx) }
    }
}
