use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

/// rustc-hash-style multiply-rotate hasher — SipHash's 5x+ speed on the
/// short keys that dominate our tables (var names, function names,
/// array keys). Deterministic across runs: all table iteration already
/// goes through `entries`, not map order.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
const FX_K: u64 = 0x517cc1b727220a95;

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = (self.hash.rotate_left(5) ^ b as u64).wrapping_mul(FX_K);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.hash = (self.hash.rotate_left(5) ^ i as u64).wrapping_mul(FX_K);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.hash = (self.hash.rotate_left(5) ^ i as u64).wrapping_mul(FX_K);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(FX_K);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.hash = (self.hash.rotate_left(5) ^ i as u64).wrapping_mul(FX_K);
    }
    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.hash = (self.hash.rotate_left(5) ^ i as u64).wrapping_mul(FX_K);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash ^ FX_SEED
    }
}

pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;
/// HashMap on FxHasher — drop-in for the hot lookup tables.
pub type FxMap<K, V> = std::collections::HashMap<K, V, FxBuild>;
pub type FxSet<K> = std::collections::HashSet<K, FxBuild>;

/// PHP arrays are insertion-ordered maps. Keys normalize per PHP rules:
/// `"8"` → 8, `"08"` stays string, `8.5` → 8, `true` → 1, `null` → "".
///
/// Stored as a Vec of entries (PHP tests exercise small arrays); existing
/// keys update in place so iteration order is insertion order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArrKey {
    Int(i64),
    Str(Rc<str>),
    /// Zend-style tombstone: `unset`/`array_shift` mark a bucket dead but
    /// keep its position so a live `foreach (&$v)` iteration over bucket
    /// positions still sees later elements (foreachLoop.013/.015).
    Tomb,
}

pub type Cell = Rc<RefCell<Value>>;

/// Below this bucket count a linear scan beats hashing the key.
const IDX_MIN: usize = 48;

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
    /// Slot cursors of in-flight by-ref foreach loops (zend's
    /// HashTableIterator list): each value is the raw `entries` index the
    /// loop examines next — zend's "one past the yielded element". Tombstoned
    /// unsets keep indices stable so the cursor survives `unset($a[$k])`;
    /// rebuild mutators (unshift/shift/splice) adjust cursors the way
    /// zend's iterators_update does. None = a finished loop's freed slot.
    pub foreach_pos: Vec<Option<usize>>,
    /// Slots charged to memory_limit — counts every slot ever appended
    /// (tombstones keep their bucket charge like zend's arData, which
    /// never shrinks on unset). Decremented wholesale at Drop.
    /// pub(crate) so literal constructions can init it; growth callers
    /// must go through mem_note_append/mem_note_seed, never this field.
    pub(crate) mem_elems: i64,
    /// zend's HT_IS_PACKED: stays true while every stored key is the
    /// next sequential int — mixed tables run a bigger stride (bucket
    /// 32 + hash-index 8 = 40/elem) than packed zval tables (16/elem).
    pub(crate) packed: bool,
    /// Live zend_string charge held by string keys (each costs a
    /// bin-rounded zend_string in the arena). Subtracted on unset and
    /// wholesale at Drop/mem_clear.
    pub(crate) key_bytes: i64,
    /// Lazily-built key → `entries` position index — zend's
    /// arHash/ht_hash analog. Never authoritative: every hit is
    /// verified (`entries[idx].0 == k`) and a miss heals by scanning
    /// once, because ~200 call sites mutate `entries` directly
    /// (sorts, splices, unshift) with no index maintenance.
    /// Populated only past IDX_MIN so small arrays stay scan-cheap.
    /// pub(crate) so literal constructions can init it (never read
    /// directly — all key lookups go through `pos_of`).
    pub(crate) idx: RefCell<FxMap<ArrKey, usize>>,
}

impl Default for PhpArray {
    fn default() -> Self {
        Self::new()
    }
}

impl PhpArray {
    pub fn new() -> Self {
        gc_root_note(1);
        Self {
            entries: Vec::new(),
            next: 0,
            is_ref: false,
            iter_pos: 0,
            foreach_pos: Vec::new(),
            mem_elems: 0,
            packed: true,
            key_bytes: 0,
            idx: RefCell::new(FxMap::default()),
        }
    }

    /// Charge one appended entry slot against ARR_LIVE, plus the base
    /// table on the first append (zend materializes the HashTable when
    /// the empty literal first gains an element — `[]` alone is free).
    /// Out-of-impl `entries.push` callers must go through here so the
    /// charge and Drop's decrement stay symmetric.
    pub fn mem_note_append(&mut self) {
        let old = arr_foot(self.mem_elems, self.packed);
        self.mem_elems += 1;
        // Zend trips inside the arData grow — a pow2-bucket realloc —
        // so the reported request is the grow alloc's size: packed
        // tables alloc nSize*16+8, mixed tables nSize*40 (oracle:
        // 1310720@nSize=32768, 5242880@131072 on 'k$i' loops).
        let n = (self.mem_elems.max(8) as u64).next_power_of_two() as i64;
        let request = if self.packed {
            n * ARR_ELEM_BYTES + 8
        } else {
            n * ARR_ELEM_HASH_BYTES
        };
        mem_charge(
            &ARR_LIVE,
            arr_foot(self.mem_elems, self.packed) - old,
            request,
        );
    }

    /// Bulk-charge `n` appended slots — the mass-prepend path in
    /// array_unshift grows the table once (zend_hash_extend), so the
    /// reported request is the single final pow2 grow, not per-element
    /// grows.
    pub fn mem_note_extend(&mut self, n: i64) {
        if n <= 0 {
            return;
        }
        let old = arr_foot(self.mem_elems, self.packed);
        self.mem_elems += n;
        let cap = (self.mem_elems.max(8) as u64).next_power_of_two() as i64;
        let request = if self.packed {
            cap * ARR_ELEM_BYTES + 8
        } else {
            cap * ARR_ELEM_HASH_BYTES
        };
        mem_charge(
            &ARR_LIVE,
            arr_foot(self.mem_elems, self.packed) - old,
            request,
        );
    }

    /// Key-side meter bookkeeping before a NEW entry is pushed: a
    /// non-sequential-int key flips the table to mixed (zend converts
    /// once, permanently) and a string key is a live zend_string (not
    /// a GC root — strings can't form cycles).
    /// Call with the key BEFORE the push so the packed check sees the
    /// pre-insert slot count.
    pub fn mem_note_key(&mut self, k: &ArrKey) {
        let flip = match k {
            ArrKey::Str(_) => self.packed,
            ArrKey::Int(i) => self.packed && *i != self.entries.len() as i64,
            ArrKey::Tomb => false,
        };
        if flip {
            // Packed→mixed conversion rebuilds arData as 40B buckets.
            self.packed = false;
            ARR_LIVE.fetch_add(
                arr_foot(self.mem_elems, false) - arr_foot(self.mem_elems, true),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        if let ArrKey::Str(s) = k {
            let c = key_charge(s.len());
            self.key_bytes += c;
            mem_charge(&STR_LIVE, c, c);
        }
    }

    /// Release the charged slots alongside an `entries.clear()`
    /// rebuild (zend's buckets are dropped, the table lives on —
    /// matching zend_hash_clean semantics under our element model).
    pub fn mem_clear(&mut self) {
        if self.mem_elems > 0 {
            ARR_LIVE.fetch_sub(
                arr_foot(self.mem_elems, self.packed),
                std::sync::atomic::Ordering::Relaxed,
            );
            self.mem_elems = 0;
        }
        if self.key_bytes > 0 {
            STR_LIVE.fetch_sub(self.key_bytes, std::sync::atomic::Ordering::Relaxed);
            self.key_bytes = 0;
        }
    }

    /// Charge a literal-built table's pre-populated entries at
    /// construction (dup_array copies, globals snapshots). No-op on an
    /// empty table — matches zend's free `[]`. Keys are noted first so
    /// `packed`/key_bytes reflect the literal's key mix.
    pub fn mem_note_seed(&mut self) {
        if self.mem_elems == 0 && !self.entries.is_empty() {
            let keys: Vec<ArrKey> = self.entries.iter().map(|(k, _)| k.clone()).collect();
            for (idx, k) in keys.iter().enumerate() {
                match k {
                    ArrKey::Int(i) => {
                        if *i != idx as i64 {
                            self.packed = false;
                        }
                    }
                    ArrKey::Str(s) => {
                        self.packed = false;
                        let c = key_charge(s.len());
                        self.key_bytes += c;
                        mem_charge(&STR_LIVE, c, c);
                    }
                    ArrKey::Tomb => {}
                }
            }
            self.mem_elems = self.entries.len() as i64;
            let n = (self.mem_elems.max(8) as u64).next_power_of_two() as i64;
            let request = if self.packed {
                n * ARR_ELEM_BYTES + 8
            } else {
                n * ARR_ELEM_HASH_BYTES
            };
            mem_charge(&ARR_LIVE, arr_foot(self.mem_elems, self.packed), request);
        }
    }

    /// Position of `k` in `entries`: O(1) for dense int keys (slot `i`
    /// holds `Int(i)` in a sequential-append table), hash-indexed past
    /// `IDX_MIN`, linear scan below it or when the hint is stale.
    fn pos_of(&self, k: &ArrKey) -> Option<usize> {
        if let ArrKey::Int(i) = k {
            if *i >= 0 && (*i as usize) < self.entries.len() {
                let p = *i as usize;
                if self.entries[p].0 == *k {
                    return Some(p);
                }
            }
        }
        if self.entries.len() <= IDX_MIN {
            return self.entries.iter().position(|(ek, _)| ek == k);
        }
        let mut idx = self.idx.borrow_mut();
        if let Some(&p) = idx.get(k) {
            if p < self.entries.len() && self.entries[p].0 == *k {
                return Some(p);
            }
            idx.remove(k);
        }
        let found = self.entries.iter().position(|(ek, _)| ek == k);
        if let Some(p) = found {
            idx.insert(k.clone(), p);
        }
        found
    }

    pub fn get(&self, k: &ArrKey) -> Option<Value> {
        self.pos_of(k).map(|p| self.entries[p].1.borrow().clone())
    }

    /// The cell holding an element (for by-ref binding).
    pub fn get_cell(&self, k: &ArrKey) -> Option<Cell> {
        self.pos_of(k).map(|p| self.entries[p].1.clone())
    }

    pub fn push(&mut self, v: Value) {
        let k = ArrKey::Int(self.next);
        self.mem_note_key(&k);
        self.entries.push((k, Rc::new(RefCell::new(v))));
        self.next += 1;
        self.mem_note_append();
    }

    /// Append an existing cell (by-ref variadics alias their args).
    pub fn push_cell(&mut self, c: Cell) {
        let k = ArrKey::Int(self.next);
        self.mem_note_key(&k);
        self.entries.push((k, c));
        self.next += 1;
        self.mem_note_append();
    }

    pub fn set(&mut self, k: ArrKey, v: Value) {
        self.set_cell(k, Rc::new(RefCell::new(v)));
    }

    /// Insert or update. An existing key's cell is replaced with the new
    /// value (so aliases bound to the cell see it); a missing key appends.
    pub fn set_cell(&mut self, k: ArrKey, c: Cell) {
        if let Some(p) = self.pos_of(&k) {
            let slot = &mut self.entries[p];
            // Same cell on both sides (a $GLOBALS sync can alias the slot to
            // its own global) — writing it would borrow_mut+borrow itself.
            if Rc::ptr_eq(&slot.1, &c) {
                return;
            }
            // A self-referential array ($a = [&$a]) can alias the very cell
            // an ancestor frame is borrowing — never panic on the reentrant
            // borrow: write through when possible, else rebind the entry.
            let new_v = c.try_borrow().map(|b| b.clone());
            let writable = slot.1.try_borrow_mut().is_ok();
            match (new_v, writable) {
                (Ok(v), true) => *slot.1.borrow_mut() = v,
                (Ok(v), false) => slot.1 = Rc::new(RefCell::new(v)),
                (Err(_), _) => slot.1 = c,
            }
            return;
        }
        self.mem_note_key(&k);
        if let ArrKey::Int(i) = k {
            if i >= self.next {
                self.next = i + 1;
            }
            self.entries.push((ArrKey::Int(i), c));
        } else {
            self.entries.push((k, c));
        }
        self.mem_note_append();
    }

    /// Bind an element slot to a specific cell (`$a[k] =& $x`).
    /// Returns the displaced slot cell — an object it held dies only
    /// after the new binding is visible (its __destruct writes land
    /// on the shared cell, gh10168).
    pub fn bind_cell(&mut self, k: ArrKey, c: Cell) -> Option<Cell> {
        if let ArrKey::Int(i) = k {
            if i >= self.next {
                self.next = i + 1;
            }
        }
        if let Some(p) = self.pos_of(&k) {
            Some(std::mem::replace(&mut self.entries[p].1, c))
        } else {
            self.mem_note_key(&k);
            self.entries.push((k, c));
            self.mem_note_append();
            None
        }
    }

    /// Remove a key (unset). The bucket is tombstoned — position kept
    /// (see ArrKey::Tomb) — and the table drops its hold on the cell
    /// entirely: an aliased (by-ref) slot keeps its value through the
    /// other owners, so a later `unset` of the last owner still sees
    /// the eager-destruct refcount. Returns the evicted payload when
    /// the table owned the cell outright.
    pub fn unset(&mut self, k: &ArrKey) -> Option<Value> {
        if let Some(p) = self.pos_of(k) {
            let slot = &mut self.entries[p];
            // Unset frees the key's zend_string (zend releases the
            // bucket's key ref even though the bucket stays tombstoned).
            if let ArrKey::Str(s) = &slot.0 {
                let c = key_charge(s.len());
                self.key_bytes -= c;
                STR_LIVE.fetch_sub(c, std::sync::atomic::Ordering::Relaxed);
            }
            slot.0 = ArrKey::Tomb;
            let old = std::mem::replace(&mut slot.1, Rc::new(RefCell::new(Value::Null)));
            if Rc::strong_count(&old) == 1 {
                return Some(std::mem::replace(&mut *old.borrow_mut(), Value::Null));
            }
        }
        None
    }

    /// First live (non-tombstone) index at or after `i`.
    fn live_at(&self, i: usize) -> Option<usize> {
        self.entries[i..]
            .iter()
            .position(|(k, _)| !matches!(k, ArrKey::Tomb))
            .map(|off| i + off)
    }

    /// Lowest registered foreach cursor at or after `start` (zend's
    /// zend_hash_iterators_lower_pos; `entries.len()` when none — zend uses
    /// nNumUsed as the "no iterator" sentinel).
    fn foreach_lower(&self, start: usize) -> usize {
        self.foreach_pos
            .iter()
            .flatten()
            .copied()
            .filter(|p| *p >= start)
            .min()
            .unwrap_or(self.entries.len())
    }

    /// Move every cursor sitting exactly on `from` to `to`
    /// (zend_hash_iterators_update).
    fn foreach_update(&mut self, from: usize, to: usize) {
        for c in self.foreach_pos.iter_mut().flatten() {
            if *c == from {
                *c = to;
            }
        }
    }

    /// array_unshift prepended `add` slots — cursors slide right to stay on
    /// the element they tracked (zend reindexes arData up by `add`).
    pub fn foreach_unshifted(&mut self, add: usize) {
        for c in self.foreach_pos.iter_mut().flatten() {
            *c += add;
        }
    }

    /// array_shift tombstoned the head bucket — the table conceptually
    /// slides down one slot (zend repacks), so cursors follow one left.
    pub fn foreach_shifted(&mut self) {
        for c in self.foreach_pos.iter_mut().flatten() {
            *c = c.saturating_sub(1);
        }
    }

    /// array_splice cursor maintenance, ported from zend's php_splice
    /// packed-array path: the rebuild drops `len` slots at `off` and inserts
    /// `ins` replacement elements. As each surviving input element is
    /// copied, a cursor sitting on its input index is moved to the
    /// element's output index — a cursor on a removed index is bumped to
    /// `off + len` in INPUT units. A moved cursor can coincide with a
    /// later input index and get bumped AGAIN (the zend cascade quirk that
    /// slides a past-offset cursor onto the tail instead of the inserted
    /// block). MUST be called while `entries` still holds the old layout.
    pub fn foreach_spliced(&mut self, off: usize, len: usize, ins: usize) {
        if self.foreach_pos.iter().all(Option::is_none) {
            return;
        }
        let n = self.entries.len();
        let off = off.min(n);
        let end = (off + len).min(n);
        let mut iter_pos = self.foreach_lower(0);
        // Output index of the element currently being copied (zend's `pos`
        // counts live elements, not raw slots).
        let mut pos = 0usize;
        for idx in 0..off {
            if matches!(self.entries[idx].0, ArrKey::Tomb) {
                continue;
            }
            if idx == iter_pos {
                if idx != pos {
                    self.foreach_update(idx, pos);
                }
                iter_pos = self.foreach_lower(iter_pos + 1);
            }
            pos += 1;
        }
        // Removed range: cursors on a removed index bump to `off + len` in
        // INPUT units (zend's "element after the removed block"). zend does
        // not advance `pos` here when the splice's return value is unused —
        // the common statement-form call — so `pos` enters the tail at
        // `off + ins`, i.e. the true output index.
        for idx in off..end {
            if matches!(self.entries[idx].0, ArrKey::Tomb) {
                continue;
            }
            if idx == iter_pos {
                self.foreach_update(idx, end);
                iter_pos = self.foreach_lower(iter_pos + 1);
            }
        }
        pos += ins;
        for idx in end..n {
            if matches!(self.entries[idx].0, ArrKey::Tomb) {
                continue;
            }
            if idx == iter_pos {
                if idx != pos {
                    self.foreach_update(idx, pos);
                }
                iter_pos = self.foreach_lower(iter_pos + 1);
            }
            pos += 1;
        }
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
        gc_root_note(1);
        let mut a = Self {
            entries: self
                .entries
                .iter()
                .map(|(k, c)| (k.clone(), Rc::new(RefCell::new(c.borrow().clone()))))
                .collect(),
            mem_elems: 0,
            next: self.next,
            is_ref: false,
            iter_pos: self.iter_pos,
            // A CoW copy does not inherit the source's live foreach loops.
            foreach_pos: Vec::new(),
            packed: self.packed,
            key_bytes: 0,
            idx: RefCell::new(FxMap::default()),
        };
        a.mem_note_seed();
        a
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
                fr.function.as_ref(),
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
    // Only the innermost surviving include frame renders bare
    // (`require()`); once a call frame sits above it, the executing
    // include renders like any call — `require('/path/trunc...')`
    // (probe9, d9).
    let bare_incl = frames
        .iter()
        .position(|f| !trace_frame_hidden(f))
        .filter(|&pos| include_frame(&frames[pos]));
    for (pos, fr) in frames.iter().enumerate() {
        if trace_frame_hidden(fr) {
            continue;
        }
        t.push_str(&format!(
            "#{} {}\n",
            i,
            trace_frame_str_at(fr, Some(pos) == bare_incl)
        ));
        i += 1;
    }
    t
}

pub fn trace_arg(v: &Value) -> String {
    match v {
        // A fatal can trace a frame while one of its args is still
        // mutably borrowed by an in-flight builtin — degrade to a
        // placeholder rather than panic on the double borrow.
        Value::Object(o) => match o.try_borrow() {
            Ok(b) => format!("Object({})", b.class.name()),
            Err(_) => "Object(?)".into(),
        },
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
        Value::Resource(r) => match r.try_borrow() {
            Ok(b) => format!("Resource id #{}", b.id()),
            Err(_) => "Resource id #?".into(),
        },
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
    let site = if fr.file.as_ref() == "[internal function]" {
        fr.file.to_string()
    } else {
        format!("{}({})", fr.file, fr.line)
    };
    let callee = match &fr.class {
        Some(c) => format!("{}{}{}", c, fr.ty, fr.function),
        None => fr.function.to_string(),
    };
    let mut arg_strs: Vec<String> = fr
        .args
        .iter()
        .enumerate()
        .map(|(i, c)| {
            // zend's #[SensitiveParameter] params render as an opaque
            // SensitiveParameterValue object in backtraces.
            let sensitive = match fr.function.as_ref() {
                "hash_pbkdf2" => i == 1,
                "password_hash" | "password_verify" | "password_needs_rehash" => i == 0,
                _ => false,
            };
            if sensitive {
                "Object(SensitiveParameterValue)".to_string()
            } else {
                trace_arg(&c.borrow())
            }
        })
        .collect();
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
            fr.function.as_ref(),
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

/// Frame body for an innermost-first live backtrace. The innermost
/// include/require pseudo-frame renders bare (`require()` — the
/// include op_array's own executing context carries no call args in
/// Zend) at WHATEVER depth it sits (bug28213); deeper include frames
/// keep their path argument (`require('/tmp/x/inc....')`).
pub fn trace_frame_str_at(fr: &TraceFrame, bare_incl: bool) -> String {
    if bare_incl && include_frame(fr) {
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
    Str(PhpStr),
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
        Value::Str(PhpStr::new(s.into().into_bytes()))
    }

    /// Build a string Value from raw bytes (binary literals, byte ops).
    pub fn bytes(b: impl Into<Vec<u8>>) -> Self {
        Value::Str(PhpStr::new(b.into()))
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

    /// zend's operand type word for 'Unsupported operand types' —
    /// objects report their CLASS name (anon-class names truncate at
    /// the \0 file:line$N suffix), scalars report zend_type_name.
    pub fn operand_type_name(&self) -> String {
        match self {
            Value::Object(o) => {
                let n = o.borrow().class.name().to_string();
                n.split('\0').next().unwrap_or(&n).to_string()
            }
            Value::Callable(_) => "Closure".into(),
            Value::Null => "null".into(),
            _ => self.type_name().into(),
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
        // Anonymous classes carry a `\0FILE:LINE$SEQ` mangled suffix
        // (or the older `$LINE` uniquifier) internally; the public
        // display name truncates at the marker.
        let n = self.decl.name.split('\0').next().unwrap_or(&self.decl.name);
        if let Some(pos) = n.find("@anonymous$") {
            &n[..pos + "@anonymous".len()]
        } else {
            n
        }
    }

    /// Method lookup walking the parent chain.
    pub fn find_method(&self, name: &str) -> Option<Rc<crate::ast::MethodDecl>> {
        let lname = name.to_lowercase();
        self.decl.find_method(&lname)
    }
}

/// Per-object-shell charge against memory_limit — zend's arena
/// counts LIVE allocations, so freeing the last Rc of an object
/// returns its charge (gc_* tests churn hundreds of thousands of
/// shells under 128M without exhausting). 72B ≈ a property-less
/// stdClass zend_object + handle slot (oracle-measured: 50k live
/// stdClass ≈ 3.6M against 8M).
pub const OBJ_SHELL_BYTES: i64 = 72;

/// zend-HashTable-faithful array charge: a live table's footprint is
/// its pow2 bucket/packed table plus a small header — usage grows in
/// doublings, not per element (oracle: packed int arrays at 65536
/// elements ≈ 1M = 65536*16 + ~200, at 100000 ≈ 2.1M = 131072*16).
/// Tombstoned slots stay charged — zend's arData never shrinks.
pub const ARR_BASE_BYTES: i64 = 200;
/// Packed (sequential-int) table: bare zval slots, 16/elem.
pub const ARR_ELEM_BYTES: i64 = 16;
/// Mixed table: bucket 32 + hash-index 8 = 40/elem (oracle's reported
/// arData grows run exactly nSize*40: 1310720@32768, 5242880@131072).
pub const ARR_ELEM_HASH_BYTES: i64 = 40;

/// Charged footprint of a live table holding `n` slots.
pub(crate) fn arr_foot(n: i64, packed: bool) -> i64 {
    if n <= 0 {
        0
    } else {
        ARR_BASE_BYTES
            + (n.max(8) as u64).next_power_of_two() as i64
                * if packed {
                    ARR_ELEM_BYTES
                } else {
                    ARR_ELEM_HASH_BYTES
                }
    }
}

/// zend_string cost of a string array key: 24B header + bytes + NUL,
/// rounded to zend_mm's small-bin ladder ('kNNNN' len≤6 lands on 32B).
fn key_charge(len: usize) -> i64 {
    let n = 24 + len as i64 + 1;
    match n {
        n if n <= 64 => (n + 7) / 8 * 8,
        n if n <= 256 => (n + 15) / 16 * 16,
        n if n <= 512 => (n + 31) / 32 * 32,
        n if n <= 1024 => (n + 63) / 64 * 64,
        n => (n + 127) / 128 * 128,
    }
}

/// zend_string charge: a 24B header + bytes + NUL through zend_mm's
/// small-bin ladder below 512B (oracle deltas: len7→32, len100→128,
/// len200→224), the fitted len + len/64 + 40 above it (str1M≈1003520B
/// oracle, where pow2 page-chunks dominate).
pub fn str_charge(len: usize) -> i64 {
    let n = 24 + len as i64 + 1;
    if n <= 512 {
        key_charge(len)
    } else {
        (len + len / 64 + 40) as i64
    }
}

/// PHP string payload: refcounted bytes carrying their memory_limit
/// charge — a clone shares the charge (zend's CoW) and the last
/// owner's drop returns it to the arena. `Deref` lands on `[u8]` so
/// `s.len()`/`s[..]`/iteration sites read unchanged; `.rc` reaches the
/// shared `Rc<[u8]>` for downgrade/ptr_eq/cache sites.
#[derive(Debug, Clone)]
pub struct PhpStr {
    pub rc: Rc<[u8]>,
}

impl PhpStr {
    /// Build and charge a new string. Every fresh byte string must go
    /// through here so STR_LIVE bookkeeping stays symmetric.
    pub fn new(bytes: Vec<u8>) -> Self {
        // zend's reported request is the zend_string alloc — len + 32
        // (24B header + 8B emalloc header; oracle: 8000032 for 8MB).
        let req = bytes.len() as i64 + 32;
        mem_charge(&STR_LIVE, str_charge(bytes.len()), req);
        PhpStr { rc: bytes.into() }
    }

    /// Re-attach a charge to shared bytes whose PhpStr owner died
    /// while a weak/raw clone kept the payload alive (DimPre
    /// resurrection). Bytes that stay live stay charged.
    pub fn adopt(rc: Rc<[u8]>) -> Self {
        let req = rc.len() as i64 + 32;
        mem_charge(&STR_LIVE, str_charge(rc.len()), req);
        PhpStr { rc }
    }
}

impl std::ops::Deref for PhpStr {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.rc
    }
}

impl Drop for PhpStr {
    fn drop(&mut self) {
        if Rc::strong_count(&self.rc) == 1 {
            STR_LIVE.fetch_sub(str_charge(self.len()), std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// Byte-payload adapters so `PhpStr` stays a drop-in for the old
// `Rc<[u8]>` payload at comparison/AsRef/From sites.
impl AsRef<[u8]> for PhpStr {
    fn as_ref(&self) -> &[u8] {
        &self.rc
    }
}

impl From<Vec<u8>> for PhpStr {
    fn from(v: Vec<u8>) -> Self {
        PhpStr::new(v)
    }
}

impl PartialEq for PhpStr {
    fn eq(&self, other: &Self) -> bool {
        self[..] == other[..]
    }
}
impl Eq for PhpStr {}

impl PartialEq<[u8]> for PhpStr {
    fn eq(&self, other: &[u8]) -> bool {
        &self[..] == other
    }
}

impl PartialEq<PhpStr> for [u8] {
    fn eq(&self, other: &PhpStr) -> bool {
        self == &other[..]
    }
}

impl std::hash::Hash for PhpStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self[..].hash(state)
    }
}

impl std::borrow::Borrow<[u8]> for PhpStr {
    fn borrow(&self) -> &[u8] {
        &self.rc
    }
}

static OBJ_LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static ARR_LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static STR_LIVE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// High-water of the three live counters' sum (memory_get_peak_usage).
static MEM_PEAK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// The most recent charge's request size — the OOM fatal's
/// 'tried to allocate N' arg.
static LAST_ALLOC: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// memory_limit as last synced by the stmt boundary, and the request
/// of the first charge whose post-total crossed it — zend dies
/// inside the crossing emalloc, so the fatal reports that request,
/// not whatever smaller charge happened to come last (the
/// last-charge garbage figure, e.g. 36).
static ARENA_LIM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
static ARENA_TRIP: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Add `bytes` to a live counter and keep the peak at the high-water
/// of the live total. `request` stamps the OOM fatal's
/// 'tried to allocate N' arg — zend reports the emalloc request size,
/// which for big allocs exceeds the charged footprint (a zend_string's
/// request is len+32 while its bin-rounded footprint runs ~len+len/4+40).
fn mem_charge(counter: &std::sync::atomic::AtomicI64, bytes: i64, request: i64) {
    counter.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    LAST_ALLOC.store(request, std::sync::atomic::Ordering::Relaxed);
    MEM_PEAK.fetch_max(mem_live_raw(), std::sync::atomic::Ordering::Relaxed);
    let lim = ARENA_LIM.load(std::sync::atomic::Ordering::Relaxed);
    if lim >= 0
        && MEM_BASE_BYTES + mem_live_raw() > lim
        && ARENA_TRIP.load(std::sync::atomic::Ordering::Relaxed) == 0
    {
        ARENA_TRIP.store(request, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Sync the boundary check's memory_limit into the arena trip
/// machinery; clears the crossing record when usage is under it
/// again (ini_set raised the ceiling or frees dropped usage).
pub fn mem_set_arena_lim(lim: i64) {
    ARENA_LIM.store(lim, std::sync::atomic::Ordering::Relaxed);
    if lim < 0 || MEM_BASE_BYTES + mem_live_raw() <= lim {
        ARENA_TRIP.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The crossing charge's request, or 0 when usage sits under the
/// synced limit (caller falls back to the last charge).
pub fn mem_arena_trip() -> i64 {
    ARENA_TRIP.load(std::sync::atomic::Ordering::Relaxed)
}

fn mem_live_raw() -> i64 {
    OBJ_LIVE.load(std::sync::atomic::Ordering::Relaxed)
        + ARR_LIVE.load(std::sync::atomic::Ordering::Relaxed)
        + STR_LIVE.load(std::sync::atomic::Ordering::Relaxed)
        + GC_PEAK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Live cycle-capable entities — zend's GC root buffer only tracks
/// HashTables/objects (zend_strings can't form cycles) and grows as
/// pow2(count)*16B. The buffer never shrinks without a GC run (none
/// under memory_limit probes), so the charge latches at its high-water
/// mark.
static GC_ROOTS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static GC_PEAK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// +1/-1 a live collectible entity; updates the GC-buffer high-water.
pub(crate) fn gc_root_note(delta: i64) {
    let n = GC_ROOTS.fetch_add(delta, std::sync::atomic::Ordering::Relaxed) + delta;
    let cap = (n.max(1024) as u64).next_power_of_two() as i64 * 16;
    GC_PEAK.fetch_max(cap, std::sync::atomic::Ordering::Relaxed);
}

/// Charge one live object shell (alloc_obj).
pub fn obj_charge() {
    mem_charge(&OBJ_LIVE, OBJ_SHELL_BYTES, OBJ_SHELL_BYTES);
    gc_root_note(1);
}

/// Live bytes held by object shells (clamped — objects wrapped before
/// or outside `alloc_obj` can dip the counter briefly negative).
pub fn obj_live_bytes() -> i64 {
    OBJ_LIVE.load(std::sync::atomic::Ordering::Relaxed).max(0)
}

/// Live bytes held by array tables (clamped like obj_live_bytes).
pub fn arr_live_bytes() -> i64 {
    ARR_LIVE.load(std::sync::atomic::Ordering::Relaxed).max(0)
}

/// Live bytes held by strings (clamped like obj_live_bytes).
pub fn str_live_bytes() -> i64 {
    STR_LIVE.load(std::sync::atomic::Ordering::Relaxed).max(0)
}

/// Runtime baseline under the metered bytes — zend reports ~465K at
/// an idle script start (oracle: memory_get_usage after unset = 465304
/// on this build; env-dependent, the metered deltas are what matter).
/// The 2M figure in -d/startup refusal messages is zend's emalloc
/// bootstrap RESERVE, a different number — don't reuse it here.
pub const MEM_BASE_BYTES: i64 = 465_304;

/// zend-arena live total: objects + array tables + strings.
/// Output-buffer contents are NOT in this arena — the zend_mm sim
/// owns them via ObLevel's token charge (`ob_meter_sync`).
pub fn mem_live_bytes() -> i64 {
    mem_live_raw().max(0)
}

/// High-water of the live total (memory_get_peak_usage).
pub fn mem_peak_bytes() -> i64 {
    MEM_PEAK.load(std::sync::atomic::Ordering::Relaxed).max(0)
}

/// memory_reset_peak_usage: zend re-baselines the peak at current usage.
pub fn mem_peak_reset() {
    MEM_PEAK.store(mem_live_raw(), std::sync::atomic::Ordering::Relaxed);
}

/// Last charge's request size — 'tried to allocate N' in the OOM fatal.
pub fn mem_last_alloc() -> i64 {
    LAST_ALLOC.load(std::sync::atomic::Ordering::Relaxed).max(0)
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

impl Drop for PhpObject {
    fn drop(&mut self) {
        OBJ_LIVE.fetch_sub(OBJ_SHELL_BYTES, std::sync::atomic::Ordering::Relaxed);
        gc_root_note(-1);
    }
}

impl Drop for PhpArray {
    fn drop(&mut self) {
        if self.mem_elems > 0 {
            ARR_LIVE.fetch_sub(
                arr_foot(self.mem_elems, self.packed),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        if self.key_bytes > 0 {
            STR_LIVE.fetch_sub(self.key_bytes, std::sync::atomic::Ordering::Relaxed);
            gc_root_note(
                -(self
                    .entries
                    .iter()
                    .filter(|(k, _)| matches!(k, ArrKey::Str(_)))
                    .count() as i64),
            );
        }
        gc_root_note(-1);
    }
}

/// One recorded call for exception backtraces (getTrace()).
#[derive(Debug, Clone)]
pub struct TraceFrame {
    /// Callee name (`fopen`, `Error2Exception`, `Cls::m`/`{closure}`-ish).
    pub function: Rc<str>,
    /// Class name for method calls (None for plain/builtin functions).
    pub class: Option<String>,
    /// `->` for object methods, `::` for static — empty for functions.
    pub ty: String,
    /// Call-site file and line; `"[internal function]"`/0 when the caller
    /// is a builtin (e.g. a userland callback invoked from ob_end_clean).
    pub file: Rc<str>,
    pub line: u32,
    /// Live VM frame index until a trace read snapshots its argument cells.
    pub args_frame: Option<usize>,
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
    /// Frame pushed by `Generator->{m}()` for the resume itself.
    /// A throwable constructed inside the running body snapshots it
    /// into its construction stack, but Zend's deferred raise renders
    /// the CURRENT resume — a stale resumer frame drops out of the
    /// rewritten trace (the live resume stack supplies the real one).
    pub gen_resume: bool,
    /// The generator BODY's own frame. A throwable constructed inside
    /// the body snapshots the drive stack that ran it below this
    /// frame — stale resume context the deferred rewrite drops.
    pub gen_body: bool,
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
    /// Storage-identity generation — bumped when `arr` is swapped or
    /// replaced in place (`exchangeArray`), so a slot fetched before a
    /// handler ran can tell its table from the dead one zend would
    /// have written into.
    pub gen: u64,
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
        /// Chained exception from the ctor's `previous` arg (or the
        /// engine's own chains — e.g. the incdec TypeError attached
        /// under a readonly-modify Error). Uncaught display renders
        /// the deepest first as `Uncaught`, each enclosing as `Next`.
        previous: Option<Value>,
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
    /// WeakReference::create($obj) payload — a weak handle; get()
    /// upgrades to the object or null once freed.
    WeakRef(std::rc::Weak<RefCell<PhpObject>>),
    /// DateTime, closures-as-objects, etc. — opaque marker.
    None,
}

/// One yielded pair — the value cell so `&function` generators can
/// yield by reference (typed_properties_033/034).
pub type GenItem = (Value, Cell);

/// Generator internal state (object internal behind the `Generator`
/// class, which implements `Iterator`).
/// A generator's suspended-finally journal — shared between the gen
/// state and the interpreter's `live_gens` GC registry, so a dead
/// weak can still replay it after the object is gone. Beyond the
/// buffered finally bytes it carries the destruction-time markers
/// the object can no longer answer once dropped: yields recorded
/// inside `finally` regions (a force-close unwinding into one dies
/// 'Cannot yield from finally in a force-closed generator'), the
/// body's error when it died inside `finally` (replayed as a raise),
/// a `$gen->throw()` parked at a finally-yield, and a mirror of the
/// consumer cursor plus the body's identity for the
/// destruction-site trace.
#[derive(Default, Clone)]
pub struct GenFinData {
    /// (yield-tag, bytes, is_err) — finally-region output buffered
    /// for destruction replay; entries drop as normal flushes cover
    /// them.
    pub bytes: Vec<(usize, Vec<u8>, bool)>,
    /// (item idx, line) of each `yield` emitted while a `finally`
    /// region ran.
    pub yields: Vec<(usize, usize)>,
    /// The body's terminal error when it died inside a `finally`
    /// region — (err, throwable), mirrors `GenState::deferred_err`'s
    /// finally component so a dead weak still surfaces it.
    pub fin_err: Option<(crate::error::PhpError, Option<Value>)>,
    /// Mirrored `GenState::pos` — the object is gone when a dead
    /// weak's entry replays.
    pub pos: usize,
    /// Consumer-visible cursor: `pos` counts the body's own resumes
    /// (a `yield from` drain drives them eagerly), while this mirrors
    /// what the outermost consumer has reached — the value ob
    /// windows, drains, and journal gates actually check.
    pub vis_pos: usize,
    /// The body closed or died — its deferred-output journal is
    /// complete, so every buffered byte it tagged is materialized
    /// for reads from then on. (Not mirrored from
    /// `GenState::finished`: eager collection marks that at run end,
    /// long before the consumer exhausts the items.)
    pub finished: bool,
    /// The gen was force-closed (throw() bounce, injected throwable
    /// uncaught, dead-weak/shutdown teardown) rather than consumed
    /// to exhaustion — its post-yield journaled tail never ran in
    /// Zend's frame, so journaled ob captures materialize only for
    /// tags a subsequent real resume confirmed (`pos > t + 1`).
    pub killed: bool,
    /// Total items collected by the eager run — the consumer-side
    /// "exhausted" condition is `pos >= total`, which is when the
    /// tail bytes (emitted after the last yield) may drain.
    pub total: usize,
    /// The body's function name and file — destruction-site frames
    /// attribute the raise (`FILE(n): g()` at an unset/overwrite
    /// point, `[internal function]: g()` at request shutdown).
    pub fn_name: Rc<str>,
    pub file: Rc<str>,
    /// Journals of `yield from` delegates that merged into this
    /// stream — a delegate replays only while the consumer's cursor
    /// sits inside its spliced range (`entry <= pos < entry + span`),
    /// since Zend force-closes just the actually-suspended
    /// delegation chain.
    pub delegates: Vec<FinDelegate>,
    /// (name, cell) pairs of the body's suspended frame in CV order —
    /// Zend keeps a suspended generator's CVs live in execute_data
    /// until the frame is freed (force-close, exhaustion,
    /// destruction), then decrefs them after the finally journal.
    /// Stashed on the journal because it outlives the dead-weak state.
    pub suspended: Vec<(String, Cell)>,
    /// The owner died as a re-run artifact — the resumed incarnation
    /// displaced this frame — so its destruction replay stays silent.
    pub suppressed: bool,
    /// Live journals of `yield from` delegates this gen collected —
    /// (parent item index where the delegate's stream begins, its
    /// journal). Killing this incarnation displaces them: their
    /// un-run tails are kill-dropped too.
    pub delegate_fins: Vec<(usize, FinQueue)>,
}

impl GenFinData {
    /// Mark this journal and every delegated journal beneath it
    /// force-closed — a re-run's displaced incarnations and a
    /// teardown's delegate chain alike leave eager tails un-run.
    pub fn kill_tree(&mut self) {
        self.killed = true;
        for (_, d) in &self.delegate_fins {
            d.borrow_mut().kill_tree();
        }
    }
}

/// A delegated `yield from` journal merged into the parent's — its
/// item-space tags are already retagged into the parent's space.
#[derive(Clone)]
pub struct FinDelegate {
    /// Parent item index where this delegate's stream begins, and
    /// how many items it spliced there.
    pub entry: usize,
    pub span: usize,
    /// The delegate's own destruction journal.
    pub fin: GenFinData,
}

impl GenFinData {
    /// Whether the consumer has consumed everything the body could
    /// emit — dead/closed, or the cursor passed the last item.
    pub fn consumed(&self) -> bool {
        self.finished || self.pos >= self.total
    }

    /// Shift every item-space index in this journal by `base` —
    /// applied when it merges into a `yield from` parent's item
    /// space.
    pub fn retag(&mut self, base: usize) {
        for (t, ..) in &mut self.bytes {
            *t += base;
        }
        for (i, _) in &mut self.yields {
            *i += base;
        }
        for d in &mut self.delegates {
            d.entry += base;
            d.fin.retag(base);
        }
    }

    /// Mirror the consumer cursor down the suspended delegation
    /// chain — a delegate's own journal positions (open/close tags on
    /// buffers the delegate opened) live in its own item space, offset
    /// from the parent's by `entry`.
    pub fn set_pos_tree(&mut self, pos: usize) {
        self.pos = pos;
        for d in &mut self.delegates {
            d.fin.set_pos_tree(pos.saturating_sub(d.entry));
        }
        self.set_vis_tree(pos);
    }

    /// Mirror only the consumer-visible cursor down the chain — a
    /// delegate's own `pos` (its production cursor) stays at
    /// whatever its eager drain left behind. Item `pos` of this
    /// gen's stream is item `pos - entry` inside a delegate's.
    pub fn set_vis_tree(&mut self, pos: usize) {
        self.vis_pos = pos;
        for d in &mut self.delegates {
            d.fin.set_vis_tree(pos.saturating_sub(d.entry));
        }
        for (entry, q) in &self.delegate_fins {
            q.borrow_mut().set_vis_tree(pos.saturating_sub(*entry));
        }
    }

    /// The delegates whose spliced range holds consumer cursor `pos`
    /// — the suspended delegation chain, innermost first.
    pub fn active_delegates_at(&self, pos: usize) -> Vec<&FinDelegate> {
        let mut out: Vec<&FinDelegate> = self
            .delegates
            .iter()
            .filter(|d| d.entry <= pos && pos < d.entry + d.span)
            .collect();
        // Innermost suspended level unwinds first.
        out.sort_by_key(|d| std::cmp::Reverse(d.entry));
        out
    }

    /// The first `yield`-inside-`finally` index ahead of `pos` along
    /// the suspended delegation chain — where a `throw()`-driven
    /// unwind parks next.
    pub fn next_fin_yield(&self, pos: usize) -> Option<usize> {
        let mut best = self
            .yields
            .iter()
            .filter(|(i, _)| *i > pos)
            .map(|(i, _)| *i)
            .min();
        for d in self.active_delegates_at(pos) {
            if let Some(i) = d.fin.next_fin_yield(pos) {
                best = Some(best.map_or(i, |b| b.min(i)));
            }
        }
        best
    }

    /// Whether `pos` itself is a `yield` inside `finally` on the
    /// suspended chain — a `throw()` injects at the suspended yield
    /// and surfaces immediately.
    pub fn at_fin_yield(&self, pos: usize) -> bool {
        self.yields.iter().any(|(i, _)| *i == pos)
            || self
                .active_delegates_at(pos)
                .iter()
                .any(|d| d.fin.at_fin_yield(pos))
    }
}

pub type FinQueue = Rc<RefCell<GenFinData>>;

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
    /// Every send() value ever passed, as (yield index, value) —
    /// send() delivers to the yield the gen is suspended at, so a
    /// re-run must not hand an early send to a preceding yield.
    pub sends: Vec<(usize, Value)>,
    /// Every `Generator->throw()` injection, as (yield index,
    /// throwable) — the body re-runs on each resume, so the queued
    /// throwable is raised as the result of that yield expression and
    /// the body's own try/catch/finally performs the real unwind
    /// (catch delivery, `return`-in-finally swallow, suspend at a
    /// yield inside `finally`).
    pub throws: Vec<(usize, Value)>,
    /// `yield from` splice windows into this gen's item stream:
    /// (first spliced index, item count). Consumer sends/throws
    /// landing inside a window route into the delegate when it is
    /// re-collected — injection arrives as the delegate's own
    /// suspended-yield index (`outer - base`).
    pub delegate_gens: Vec<(usize, usize)>,
    /// The throwable most recently queued by `Generator->throw()` —
    /// an uncaught injected throwable keeps its own trace (built at
    /// the `new` site) when it escapes, unlike a body-raised `throw`
    /// whose uncaught render is the resume stack.
    pub injected_throwable: Option<Value>,
    /// Output produced after a yield suspends mid-expression — Zend
    /// defers it to resume; buffered per yield index and emitted when
    /// the consumer advances `pos` past it (closure_call_leak). The
    /// first bool marks stderr-diag bytes so `PHP Fatal error:`/`PHP
    /// Warning:` lines defer in the same emission order as stdout's;
    /// the second marks bytes produced inside a `finally` region —
    /// Zend runs the finally chains enclosing the suspension point
    /// when a suspended generator is destroyed (Generator->throw(),
    /// unset()/GC, request shutdown), so they also accumulate in
    /// fin_q keyed the same way.
    pub pending_out: Vec<(usize, Vec<u8>, bool, bool)>,
    /// Buffered finally-region output of a suspended body — survives
    /// the GenState itself (shared with the interpreter's live_gens
    /// registry) so a GC'd generator's finally still replays.
    /// (yield-tag, bytes, is_err); entries are dropped as normal
    /// flushes cover them.
    pub fin_q: FinQueue,
    /// The body's terminal error, held until the consumer's next
    /// resume past the last collected item — Zend's lazy body dies
    /// inside `Generator->next()`/friends, after the bytes the
    /// consumer already echoed between yields. Carries the throwable
    /// itself for Throw deaths (the ambient pending_exception slot is
    /// transient — consumer calls between death and resume clobber
    /// it) and the call-trace frames suspended between the throw site
    /// and the gen body (eval()/include() pseudo-frames, userland
    /// calls) so the resume render can prepend them. The last flag
    /// marks a death that originated inside a `finally` region — a
    /// force-close then surfaces it at destruction instead.
    pub deferred_err: Option<(crate::error::PhpError, Option<Value>, Vec<TraceFrame>, bool)>,
    /// The body died by error — getReturn() reports 'hasn't returned'
    /// even after the deferred error was consumed.
    pub dead: bool,
    /// Killed by `Generator->throw()` — buffered items are dropped and
    /// consumer reads behave like an exhausted generator (`valid()`
    /// false, `current()`/`key()` null), like Zend's closed gen.
    pub closed: bool,
    /// The body's eager run is in flight right now — resuming ops
    /// (`next`/`send`/`throw`) on the object are guarded ('Cannot
    /// resume an already running generator') so a body that reaches
    /// its own handle cannot re-enter the cursor machinery mid-frame.
    pub running: bool,
    /// The sink filling up while `running` — consumer read ops
    /// (`valid`/`current`/`key`) consult it so a mid-run probe sees
    /// the yields already produced, like Zend's live execute_data.
    pub live: Option<Rc<RefCell<Vec<GenItem>>>>,
    /// This gen is being re-collected as a `yield from` delegate of a
    /// gen whose own send()/throw() replay is in flight: its
    /// pre-first-yield bytes were already echoed by the delegate run
    /// the consumer saw, so emit suppression covers its `done==0`
    /// prefix even though the active horizon targets the outer gen.
    pub suppress_prefix: bool,
    /// The suspended frame's charged span — retired when the body
    /// dies or the cursor proves it done, re-charged when a send()/
    /// throw() revive brings the dead frame back.
    pub vm_span: u64,
}

impl GenState {
    /// Advance the consumer cursor, mirroring it into the shared
    /// finally journal so a dead weak's destruction check can still
    /// gate on the suspension point.
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
        self.fin_q.borrow_mut().set_pos_tree(pos);
    }

    /// Whether `decl` is this generator's own body — the popped
    /// frame for that decl is a suspension (its CVs stay live in
    /// execute_data) rather than a call return.
    pub fn owns_frame(&self, decl: &crate::ast::FunctionDecl) -> bool {
        match &self.setup {
            GenSetup::Invoke { decl: d, .. } => std::ptr::eq(d.as_ref(), decl),
        }
    }
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
            ObjectInternal::WeakRef(_) => f.write_str("WeakRef"),
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
        /// Stream-level read buffer + capacity — zend buffers plain
        /// file streams, so rbuf.len() is zend's writepos-readpos
        /// (buffered-but-unread bytes) for cast resyncs and the
        /// stream_select emulate shortcut.
        rbuf: std::collections::VecDeque<u8>,
        rcap: usize,
        /// tmpfile() only — zend removes the temp file when the stream
        /// closes (Drop unlinks `path`).
        unlink_on_close: bool,
        /// Path and mode as given to fopen() — stream_get_meta_data().
        path: String,
        mode: String,
    },
    /// STDIN/STDOUT/STDERR — php:// and the CLI-SAPI constants.
    /// `which` > 2 is php://output: a write-only stream whose ftell
    /// counts bytes written (zend tracks them on the stream struct).
    Stdio { id: u64, which: u8, pos: u64 },
    /// php://input — the request body, readable like a file.
    Input {
        id: u64,
        body: std::rc::Rc<Vec<u8>>,
        pos: u64,
        eof: bool,
        /// zend's stream->position = -1 marker after a failed seek:
        /// ftell reports pos-1 (false) while IO resumes at pos=0.
        pos_broken: bool,
        /// The URI the stream was opened with ("php://input", "data:...")
        /// — reported verbatim in stream_get_meta_data()'s 'uri' key.
        uri: String,
        /// The fopen() mode, verbatim — stream_get_meta_data() 'mode'.
        mode: String,
        /// fd claimed by a PHP_STREAM_AS_FD_FOR_SELECT cast (zend's
        /// php_stream_temp_cast spills an RFC2397 buffer into a
        /// tmpfile() the stream then KEEPS — later casts reuse it and
        /// flock(2)/fstat(2) see it).
        spilled_fd: Option<std::os::unix::io::RawFd>,
        /// zend stream->readbuf — read(2) fills land here while
        /// spilled, and FILTERED bytes land here once a read filter is
        /// attached (zend's fill applies the chain before buffering).
        srbuf: std::collections::VecDeque<u8>,
        /// zend stream->readbuflen for srbuf — grows one chunk_size
        /// whenever a fill finds less than a chunk of free space.
        rcap: usize,
        /// The raw store cursor (zend's inner-stream fpos): filtered
        /// fills slice the body from here while pos stays the
        /// delivered count. Unfiltered reads bypass the buffer, so
        /// fraw tracks pos then.
        fraw: u64,
    },
    /// php://memory / php://temp — an in-memory byte buffer that is
    /// always read/write, seekable (Composer's BufferIO).
    Mem {
        id: u64,
        buf: Vec<u8>,
        pos: u64,
        eof: bool,
        /// zend's stream->position = -1 marker after a failed
        /// CUR/END-below-zero seek: ftell reports pos-1 (false) while
        /// IO resumes at pos=0; a successful seek clears it.
        pos_broken: bool,
        /// fwrite honors the fopen mode ('r' → false); fprintf does not
        /// (zend php_stream_printf bypasses the check).
        write: bool,
        /// zend's TEMP_STREAM_APPEND: an 'a'-mode buffer write lands at
        /// end-of-buffer regardless of position. Lost once the stream
        /// spills — the tmpfile is a plain r+b file (zend likewise).
        append: bool,
        /// The php:// URI the stream was opened with ("php://memory",
        /// "php://temp", "php://temp/maxmemory:N") — reported verbatim
        /// in stream_get_meta_data()'s 'uri' key.
        uri: String,
        /// zend's normalized open mode for meta ('rb', 'w+b', 'a+b').
        mode: String,
        /// php://temp* only: zend's ts->smax — the /maxmemory:N budget
        /// (default PHP_STREAM_MAX_MEM = 2MB). A write reaching
        /// stream->position+count >= smax spills the buffer to a
        /// tmpfile BEFORE the inner stream's readonly check runs.
        /// None on php://memory: never spills and not fd-castable.
        temp_smax: Option<u64>,
        /// fd claimed by a PHP_STREAM_AS_FD_FOR_SELECT cast or a write
        /// that crossed temp_smax — zend's php_stream_temp_cast /
        /// php_stream_temp_write spills a TEMP buffer into a tmpfile()
        /// the stream then KEEPS (later casts reuse it and
        /// flock(2)/fstat(2) see it). php://memory is not castable.
        spilled_fd: Option<std::os::unix::io::RawFd>,
        /// zend stream->readbuf — read(2) fills land here while
        /// spilled, and FILTERED bytes land here once a read filter is
        /// attached (zend's fill applies the chain before buffering).
        srbuf: std::collections::VecDeque<u8>,
        /// zend stream->readbuflen for srbuf — grows one chunk_size
        /// whenever a fill finds less than a chunk of free space.
        rcap: usize,
        /// The raw store cursor (zend ms->fpos): filtered fills slice
        /// the buffer from here while pos stays the delivered count.
        /// Unfiltered reads bypass the buffer, so fraw tracks pos.
        fraw: u64,
    },
    /// A resource closed via fclose()/fclose-aliased wrappers — Zend
    /// keeps the zval `resource (closed)` (gettype "resource (closed)",
    /// var_dump "of type (Unknown)", is_resource() false) and every
    /// stream function on it throws "must be an open stream resource".
    Closed { id: u64 },
    /// A proc_open() pipe end (or socketpair/pty end) as seen by the
    /// parent: a raw fd wrapped in File. `write` mirrors zend's
    /// mode; reads always hit the real fd so EBADF reports like zend.
    Pipe {
        id: u64,
        file: std::fs::File,
        write: bool,
        /// ["socket"] descriptor pair — bidirectional, different
        /// stream_type in stream_get_meta_data().
        socket: bool,
        /// ["pty"] descriptor — the parent's end is the pty master,
        /// opened 'r+' in zend's meta.
        pty: bool,
        /// stream_set_blocking($s, false) — reads return "" instead
        /// of waiting (fcntl O_NONBLOCK on the fd).
        nonblock: bool,
        pos: u64,
        eof: bool,
        /// read-buffered but unconsumed bytes (zend's readbuf/writepos/
        /// readpos): a php_stream_read() call drains this and performs
        /// at most ONE underlying fill of stream_set_chunk_size() bytes.
        rbuf: std::collections::VecDeque<u8>,
    },
    /// proc_open() process handle — type "process" in zend.
    Proc {
        id: u64,
        pid: i32,
        command: String,
        /// Raw waitpid status cached after a WIFEXITED reap
        /// (zend's waitpid_cached: only normal exits are cached).
        cached_status: Option<i32>,
        /// proc_close() already consumed this handle.
        closed: bool,
        /// The proc's pipe streams — zend's proc dtor zend_list_close()s
        /// them, so proc_close()/GC turns every $pipes entry "Unknown".
        pipes: Vec<std::rc::Rc<std::cell::RefCell<PhpResource>>>,
    },
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
            PhpResource::Pipe { id, .. } => *id,
            PhpResource::Proc { id, .. } => *id,
            PhpResource::Other { id, .. } => *id,
        }
    }

    /// Zend's `zend_rsrc_list_get_rsrc_type` name for var_dump's
    /// `of type (..)` and `get_resource_type()`.
    pub fn type_name(&self) -> &'static str {
        match self {
            PhpResource::Closed { .. } => "Unknown",
            PhpResource::Proc { .. } => "process",
            PhpResource::Other { kind, .. } => kind,
            _ => "stream",
        }
    }
}

/// A stream filter attached by stream_filter_append/prepend — zend's
/// php_stream_filter on a stream's read/write chains. `read`/`write`
/// say which chain it sits on (STREAM_FILTER_READ=1, WRITE=2, ALL=3 —
/// ALL creates TWO entries, one per chain, with separate state).
#[derive(Debug, Clone)]
pub struct StreamFilter {
    pub name: String,
    pub read: bool,
    pub write: bool,
    /// The filter resource's own id — stream_filter_remove() detaches
    /// the chain entry by this, not by name (two same-name filters
    /// stay distinct).
    pub fid: u64,
    /// zend's filter->abstract — per-instance state.
    pub state: FilterState,
}

/// Per-instance state for the stateful stream filters.
#[derive(Debug, Clone)]
pub enum FilterState {
    /// Stateless transforms (string.rot13/toupper/tolower) and
    /// recognized-but-unimplemented factories (zlib.*, bzip2.*).
    Plain,
    /// `consumed` — passes bytes through while counting them; on the
    /// closing flush zend seeks the stream back to offset+consumed
    /// (filters.c consumed_filter_filter).
    Consumed {
        count: u64,
        /// stream->position captured on the first filter call.
        offset: Option<u64>,
    },
    /// `dechunk` — the HTTP chunked-transfer decoder's state machine
    /// (filters.c php_dechunk): bytes between calls.
    Dechunk(Dechunk),
    /// convert.iconv.FROM/TO — normalized encoding pair plus the
    /// partial multibyte sequence carried between calls.
    Iconv {
        from: String,
        to: String,
        /// the original `from"=>"to` spec for the invalid-seq warn.
        disp: String,
        pending: Vec<u8>,
        /// UTF-16/32's BOM already emitted (iconv emits it once).
        bom_done: bool,
        /// to-charset carried //TRANSLIT — unrepresentable cps
        /// transliterate (é→e, €→EUR, unknown→'?') instead of
        /// erroring. zend's stream filter ignores //IGNORE
        /// (unrepresentable output still EILSEQ-fails), so only
        /// TRANSLIT is modeled.
        translit: bool,
    },
    /// convert.base64-encode / -decode — tail bytes carried between
    /// calls (3-in/4-out groupings).
    Base64 { decode: bool, tail: Vec<u8> },
    /// convert.quoted-printable-encode / -decode. `col` is the
    /// encoder's line-wrap column (75), `tail` the decoder's
    /// partial-escape carry.
    Qp {
        encode: bool,
        col: usize,
        tail: Vec<u8>,
    },
    /// zlib.inflate/deflate, bzip2.compress/decompress — the codec
    /// object lives in Interp::codec_states[fid] (compressors don't
    /// clone).
    Codec(CodecKind),
    /// A php_user_filter subclass instance created at attach time.
    User(std::rc::Rc<std::cell::RefCell<PhpObject>>),
}

/// Which streaming codec backs a FilterState::Codec entry — the
/// compressor/decompressor lives in Interp::codec_states[fid].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    /// zlib.deflate — raw RFC1951 deflate stream.
    ZlibDeflate,
    /// zlib.inflate — raw RFC1951 inflate; bad data → 'zlib: data error'.
    ZlibInflate,
    /// bzip2.compress.
    BzDeflate,
    /// bzip2.decompress — bad data → 'bzip2 decompression failed'.
    BzInflate,
}

/// The `dechunk` filter's persistent state machine — a byte-for-byte
/// port of zend's php_chunked_filter_data (filters.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DechunkState {
    SizeStart,
    Size,
    SizeExt,
    SizeCr,
    SizeLf,
    Body,
    BodyCr,
    BodyLf,
    Trailer,
    Error,
}

/// `dechunk` filter instance data (zend's php_chunked_filter_data).
#[derive(Debug, Clone)]
pub struct Dechunk {
    pub chunk_size: u64,
    pub state: DechunkState,
}

/// A dropped process handle closes its pipes and gets one non-blocking
/// reap so exited children don't stay zombies (zend's proc dtor does
/// the same — pipes first, then waitpid).
impl Drop for PhpResource {
    fn drop(&mut self) {
        if let PhpResource::Proc {
            pid,
            cached_status,
            closed,
            pipes,
            ..
        } = self
        {
            for p in pipes {
                let mut b = p.borrow_mut();
                if let PhpResource::Pipe { id, .. } = &*b {
                    *b = PhpResource::Closed { id: *id };
                }
            }
            if !*closed && cached_status.is_none() {
                unsafe {
                    let mut st = 0;
                    libc::waitpid(*pid, &mut st, libc::WNOHANG);
                }
            }
        }
        // tmpfile(): zend removes the temp file on stream close.
        if let PhpResource::File {
            unlink_on_close: true,
            path,
            ..
        } = self
        {
            let _ = std::fs::remove_file(&*path);
        }
        let fd = match self {
            PhpResource::Mem { spilled_fd, .. } | PhpResource::Input { spilled_fd, .. } => {
                spilled_fd.take()
            }
            _ => None,
        };
        if let Some(fd) = fd {
            unsafe {
                // A spilled temp stream maps a real filesystem entry
                // (zend php_stream_temp_cast unlinks it only when the
                // stream closes): remove it via the procfs link target
                // before dropping the last descriptor we own.
                if let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}")) {
                    let t = target.to_string_lossy();
                    if !t.ends_with(" (deleted)") {
                        let _ = std::fs::remove_file(&*t);
                    }
                }
                libc::close(fd);
            }
        }
    }
}

use std::collections::HashMap;
