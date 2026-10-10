# Lane R0 profile for issue 205 (tracker 92)

Measurement only — no optimization code. CPU attribution via `perf record`
(cpu-clock:u @ 999Hz, dwarf call graph), allocation/bytes attribution via
`PHPUN_ALLOC=1` site counters plus libc malloc/calloc/realloc uprobes
(`perf record -e 'myprobes:*' --call-graph dwarf,1024`, first `phpun` frame as
the site). All counters describe requested allocations, not peak live memory
and not a count of host mallocs.

- checkout: `ef63392` (main tip at measurement time, 2026-10-10); branch
  rebased onto `4e34900` (post-#208) — gates and byte-identity re-verified
  there; counters/attribution unchanged
- phpun: release build, `phpun 0.0.1` (PHP 8.5 target); sha256
  `41a6be6d…572a4`
- oracle: PHP 8.5.11 NTS via brew, `-n`, opcache.enable_cli=0, jit=disable;
  sha256 `db75babc…39152`
- host: Linux 6.8.0-1061-aws x86_64, glibc 2.35, 8 logical CPUs
- timing: `bench/run.sh --reps 7`, profilers OFF, alternating order, cold CLI
  (startup + parse + execution), byte-identical stdout/stderr/exit vs oracle
  on every rep
- attribution: profilers ON, same source checkouts, reduced argv where noted

## Timing (7 reps, profilers off)

| bench | PHP median [min,max] ms | phpun median [min,max] ms | ratio |
|---|---:|---:|---:|
| 40-objects | 27.973 [25.773,48.104] | 293.950 [265.391,480.076] | 10.51× |
| 60-json | 196.392 [138.952,233.544] | 1499.977 [859.384,1658.708] | 7.64× |
| 11-sieve | 41.701 [26.384,60.626] | 282.845 [176.761,323.804] | 6.78× |
| 30-arrays | 57.388 [36.314,73.307] | 187.487 [176.124,341.742] | 3.27× |
| 10-fib | 58.846 [28.607,72.813] | 183.193 [129.896,250.495] | 3.11× |

## CPU attribution

Exclusive samples rolled up by function group; top symbols verbatim in
`data/92/profile-205/*-flat.report`, the rollup in `buckets.txt`.

### 10-fib (3.11×)

| group | % CPU |
|---|---:|
| VM dispatch (`vm_exec`+`vm_run`+`vm_slot_value`+`vm_binary`+`vm_site`+`vm_frame_free`) | 63.6 |
| call/frame machinery (`call_site_frame`, `trace_pop`) | 10.8 |
| memory accounting | 0.3 |
| libc | 2.0 |
| other/unsymbolized | 23.3 |

`PHPUN_CALLPROF`: `pre`+`cells`+`site`+`bind`+`post` = 108.66 ms over 392,835
calls ≈ 277 ns of measured per-call overhead; `exec` (inclusive body) is the
rest. Allocation profile (n=24 uprobe): 86.9% of mallocs are
`RawVec::finish_grow` at ~40 B each ≈ one tiny Vec grow per frame (arg/vars
cells); the remaining hits are parser/startup. New `array_new`/`to_string`
counters: 79/92 hits — negligible. The gap is per-call frame + dispatch cost.

### 11-sieve (6.78×)

Top-level code runs the AST path — 0 compiled VM entries — so `vm_*` below is
the dim-write interpreter machinery, not function dispatch.

| group | % CPU |
|---|---:|
| global var resolution (`global_var_cell`) | 13.7 |
| dim-write machinery (`assign_inner` 5.9 + `assign_index_path` 2.6 + `index_into_key` 2.4 + `dim_*`/`index_read*` ~3.9) | ~14.8 |
| `vm_*` ops (materialize/refresh/slot/exec of the index-write path) | 28.8 |
| array ops | 3.6 |
| memory accounting | 1.3 |
| libc | 3.9 |
| other/unsymbolized | 30.7 |

Allocations (lim=20000 uprobe, 320k events): `assign_inner` 29.2% +
`assign_index_path` 19.5% + `dim_self_root` 9.7% + `dim_var_key` 9.7% +
`String::clone` 9.9% ≈ 78.1% of all mallocs — tiny (1–15 B) key/scratch churn
per index write. `builtins::cell` (array_fill cells) 5.9%. Element cells are
NOT the problem: new `array_new` counter reads 79 for the whole run; the
table is already sequential-int keyed and writes update slots in place.
Packed storage (R3) would not move this bench.

### 40-objects (10.51×)

| group | % CPU |
|---|---:|
| method dispatch / name normalization (`to_lowercase` 4.5 + `PhpClass::name` 3.4 + `CharSearcher` 3.3 + `is_a_str` 2.4 + prop-key helpers) | 17.6 |
| `vm_*` ops | 9.3 |
| cleanup/GC (`destruct_cells`, drop glue, `cfree`) | 8.6 |
| libc malloc/free | 6.0 |
| construction (`push_handle`, instantiate path) | 2.9 |
| property store/lookup | 2.4 |
| call/frame machinery | 2.3 |
| memory accounting | 2.1 |
| long tail (SipHash/RandomState ~3.5, `PropDecl::clone` 1.0, evals) | 48.5 |

Allocations (1×1000 objects uprobe, 234k events ≈ 234 mallocs per
object+`norm()` pair): `to_lowercase` 32.0% (7 B avg — a fresh lowercase
String per name compare in the method-lookup chain), `Vec<String>` clone
8.6%, `RawVec::finish_grow` 9.3%, `RawTableInner::fallible_with_capacity`
4.7% (a fresh HashMap per property/props op), `PropDecl::clone` 3.4%,
`scope_private_prop` 3.4%, `is_a_str` 2.5%, `invoke_fn_run` 2.5%,
`destruct_frame_objs` 1.7%, `store_prop`/`resolved_ty` ~3.4%. New `obj_new`
counter reads exactly 25,000 (the object shells); `cell` 300,018 = 12
cells/object. Construction itself is cheap — the cost is per-dispatch name
normalization + HashMap/Vec churn around property access.

### 30-arrays (3.27×)

| group | % CPU |
|---|---:|
| `vm_*` ops (callback bodies) | 30.5 |
| call/frame machinery (`call_site_frame` 3.5 + `call_value` 3.0 + `trace_pop` 1.7 + `expr_yield_kind` 1.6 + `Frame::new` 1.1 + `call_site` 0.95) | 12.6 |
| `zend_sort_user` comparator closure | 5.9 |
| assign/dim (`assign_inner` 1.7 + `assign_index_path` + `isset_val_mode` ~1.6) | 5.3 |
| other array ops (`PhpArray`, `to_key`, push) | ~3.9 |
| libc | 6.5 |
| memory accounting | 1.5 |
| other/unsymbolized | 33.4 |

Allocations (scale=2000 uprobe, 251k events): `zend_sort_user` closure 33.4%,
`call_value` 9.9%, `Frame::new` 9.9%, `call_site` 9.9% — ≈63% of all mallocs
sit in per-CALL callback machinery (usort/map/filter invoke the user closure
per element). `assign_inner`+`assign_index_path` ~7%, `to_key` 3.2%.
`array_new` counter: 22,936 element cells of 2,999,933 host mallocs — element
cells are <1% of allocation traffic; packed storage is not the bottleneck.
`trace_frame` 350,351 ≈ the callback count.

### 60-json (7.64×)

| group | % CPU |
|---|---:|
| memory_limit meter (`mem_realloc<PhpArray>` closure 21.6 + `mem_track<u8>` 12.0 + `mem_sweep` 4.3 + `mem_note_append`/`mem_note_key` ~4.4) | 42.4 |
| JSON encode/decode (`json_value` 5.2 + `json_enc` 4.0 + `json_str` 2.6) | 11.8 |
| string materialization (`PhpStr::new` 2.5 + `lossy` 2.4 + `spec_to_string` + clones) | 9.8 |
| `PhpArray` ops (drop glue 2.7 + `set`/`set_cell`/`push`) | 4.0 |
| libc | 6.9 |
| other/unsymbolized | 24.8 |

The dominant cost is not JSON code — it is the zend memory-limit emulation:
every `PhpArray` append/realloc walks the meter, and the `mem_realloc`
closure alone is 21.6% of CPU. Allocations (reps=1 uprobe, 783k events,
34.5 MB): `finish_grow` Vec growth 37.7% (element Vec reallocation),
`PhpStr::new` 11.4%, `PhpArray::set`+`push` 8.8%, `json_*` funcs ~18.2%,
`to_key` 6.8%. New counters: `to_string` 1,308,179 + `array_new` 1,113,079 ≈
2.4M of 15.1M host mallocs — the rest is Vec growth inside decode/encode.

## Lane decision

| bench | next lane | why |
|---|---|---|
| 10-fib | R4 (VM ABI) | 63.6% dispatch + ~11% call machinery; ~1 frame Vec-grow per call. Nothing else registers. |
| 11-sieve | R4 (VM ABI / hot-path) | AST-path dim-write machinery + global name resolution ≈ 45% CPU and ~78% of mallocs; element cells ≈ 0 — packed storage cannot help. Needs dim-assign fast path (top-level coverage is the `executed-body=0` gap). |
| 40-objects | R2 (objects) | Dispatch/name normalization ≈ 17.6% CPU + ~45% of mallocs (`to_lowercase` 32%); property HashMap/Vec churn. R2's method/property caches + declared-slot props target exactly this. Largest single gap (10.51×). |
| 30-arrays | R4 (VM ABI) | ~63% of mallocs + ~13% CPU in per-call callback machinery; element cells <1%. `usort`/`map`/`filter` pay per call, not per element. |
| 60-json | R3 (arrays-json), gated | Element cells/Vec growth ≈ 46% of mallocs (real R3 target), BUT the meter (`mem_realloc`/`mem_track`/`mem_sweep`) is 42% CPU — a cross-cutting accounting cost that also taxes arrays/objects. Any lane that grows `PhpArray` must fix the meter path first or it keeps eating the win. |

Ranking across benches: **R4 first** — it owns fib, sieve, and arrays (3 of 5
benches, and the fib/sieve/array costs are the same frame/dispatch machinery).
**R2 second** — objects has the largest gap and a clear, concentrated cause.
**R3 third** — real for json but gated on the memory-meter cost, which likely
deserves its own sub-lane under R3 or shared infra. **R1 last** — strings only
burn via R2's name normalization and json `PhpStr::new` (~11% of json mallocs,
~2.5% CPU); no bench is string-bound.

## Instrumentation added

`PHPUN_ALLOC` site counters previously ended at index 6; sites 7–11 were
declared but never wired, leaving the hottest allocation sources unattributed.
Minimal wiring (lazy diff, no new deps):

- `to_string` (7) ← `PhpStr::new` — every fresh string payload.
- `array_new` (9) ← the five `Rc::new(RefCell::new(..))` element-cell sites in
  `PhpArray::push`/`set`/`set_cell`/`unset`/`clone` — per-element Cell
  allocation, the number issue 205 asked for. These sites cannot call the
  private `cell()` helper, so the counter is taken inline.
- `obj_new` (10) ← object instantiation in `classes.rs` (reads exactly 25,000
  for objects.php).

`expr_temp` (8) and `other` (11) stay unwired — no hot site needed them.
All counters sit behind `ALLOC_ON`; profilers-off path is a single relaxed
atomic load, and the gate below shows output is unchanged.

## Caveats

- Uprobe runs used reduced argv (sieve lim=20000, objects 1×1000, arrays
  scale=2000, fib n=24, json reps=1) to bound perf.data size; per-element /
  per-call composition is argv-invariant.
- Sieve uprobe lost ~11.9% of events to perf buffer pressure at
  dwarf,1024; rankings are unaffected (top site leads by 10 pts).
- Dwarf stacks through glibc are shallow; malloc-site attribution uses the
  first `phpun` frame — a small number of events attribute to libc-internal
  wrappers and land in `(other)`.
- Site counters are requests, not host mallocs; totals legitimately exceed the
  global allocator count (two counters can fire on one logical op).

## Verification

- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` — clean.
- Byte-identical probe: `PHPUN_ALLOC=1 ./target/release/phpun bench/*.php` vs
  `php -n bench/*.php` — stdout byte-identical on all five benches.

Raw data: `bench/data/92/profile-205/` — `timings.txt` (measure.py output),
`buckets.txt` (CPU rollups), `*-flat.report` (perf flat tables),
`*-malloc-sites.txt` (uprobe site tables), `*.alloc.log` / `*.vm.log` /
`*.call.log` (PHPUN_ALLOC / PHPUN_VMPROF / PHPUN_CALLPROF dumps).
