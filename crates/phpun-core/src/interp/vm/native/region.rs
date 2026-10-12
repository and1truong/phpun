//! Native execution of the same transactional buffers as the portable region.
use super::{Compiled, Emitter, Interp, Op, Program, Value};

fn depths(c: &Compiled, start: usize, end: usize) -> Option<Vec<Option<usize>>> {
    if end > c.ops.len() || start >= end || end - start > 512 || c.nslots > 256 {
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
        let op = &c.ops[pc];
        let (take, give) = match op {
            Op::Const(Value::Int(_) | Value::Bool(_)) | Op::DimCv { .. } => (0, 1),
            Op::Load(i) | Op::IncDec { slot: i, .. } | Op::BinaryCvConst(_, i, Value::Int(_))
                if (*i as usize) < c.nslots =>
            {
                (0, 1)
            }
            Op::Store(i) if (*i as usize) < c.nslots => (1, 1),
            Op::BinaryCv(_, i) if (*i as usize) < c.nslots => (1, 1),
            Op::Binary(_) => (2, 1),
            Op::Not | Op::Boolify => (1, 1),
            Op::Pop | Op::JumpIfFalse(_) | Op::JumpIfTrue(_) => (1, 0),
            Op::JumpCmp { a, b, .. } if (*a as usize) < c.nslots && (*b as usize) < c.nslots => {
                (0, 0)
            }
            Op::IncJump { slot: i, .. } if (*i as usize) < c.nslots => (0, 0),
            Op::Jump(_) | Op::Line(_) | Op::Sweep | Op::BoolLoop { .. } => (0, 0),
            _ => return None,
        };
        match op {
            Op::Binary(op) | Op::BinaryCv(op, _) | Op::BinaryCvConst(op, _, _)
                if !matches!(
                    *op,
                    "+" | "-" | "*" | "%" | "==" | "===" | "!=" | "!==" | "<" | "<=" | ">" | ">="
                ) =>
            {
                return None
            }
            Op::JumpCmp { op, .. } if !matches!(*op, "<" | "<=" | ">" | ">=") => return None,
            Op::DimCv {
                key_slot: Some(i), ..
            } if (*i as usize) >= c.nslots => return None,
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
        let depths = depths(c, start, end)?;
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
