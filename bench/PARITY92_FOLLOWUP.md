# Parity follow-up: direct scalar arguments

Runtime baseline `8688c2a` (dynamic builtin semantic fixes, PR #189) to
candidate `db51239`. Rust 1.99.0, release/LTO/codegen-units=1. Same host,
7 alternating pairs, profilers off, every exit/stdout/stderr matched PHP 8.5.11
`-n`. Binary hashes, samples, host, argv and build metadata: [raw data](data/92/parity/).

| Workload | Before → after median ms | After/before |
|---|---:|---:|
| fib default | 223.024 → 195.910 | 0.878 |
| typed fib(29), one iteration | 873.876 → 746.132 | 0.854 |
| untyped fib(29), one iteration | 884.529 → 784.123 | 0.886 |
| strings | 670.470 → 676.437 | 1.009 |
| arrays | 228.843 → 225.119 | 0.984 |
| objects | 453.198 → 437.419 | 0.965 |
| JSON | 971.871 → 941.488 | 0.969 |

The benefit is specific to direct calls with read-only scalar parameters and
already-proven type checks. Missing/default/named/extra/coercing/by-ref/hybrid
calls retain the cell binder. A cold by-reference operation materializes cells
before canonical SEND; introspection/backtrace can observe live argument values.
No general application or PHP-parity speedup is claimed.

The initial 7-pair application probe had +7.8% wall time with overlapping ranges.
Because that remained uncertain, 31 further alternating pairs were run: app CLI
42.480 → 42.320 ms (0.996), Composer 7.185 → 7.106 ms (0.989), ranges overlap.
Both datasets are retained. These cold demos do not establish production performance.

Allocation diagnostics are **requests/counters, not a count of host mallocs**.
Pooling already kept the cell count low: typed fib(29) cell counters 41 → 13;
total reported requests remain ~1.689M and bytes ~57.5M. The optimization removes
per-call Rc/RefCell manipulation; it does not remove arena accounting/trace shells.

Validation: workspace tests, fmt, clippy; new argument-observation/fallback test;
21 VM fixtures and 19 workload oracle checks. 709 binding PHPT: 636 pass,
62 existing fail, 2 skip, 9 unsupported, zero crashes/timeouts; no status changes.

## Trace and dispatch decision (#184 / #188)

Candidate fib(29), 10 iterations: perf6.1.176 `cpu-clock:u`199Hz, DWARF8192,
1,631 samples, zero lost. `call_site_frame` appears in about 9.6% of weighted
stacks, inclusive of helper/libc work. Its exclusive symbol samples are 2.51%;
`vm_exec` 37.22%, `vm_run` 10.79%. These are function samples, not proof of pure
opcode dispatch shares. Unresolved libc addresses and stack-depth limits remain.

Trace arguments are already deferred. The remaining context cost is measurable,
but these observations do not justify combining a full trace rewrite with the
argument ABI PR. Keep #184 as a measured, separate follow-up; prioritize shared
string/constructor costs. Register/native-tier selection (#188) still needs
instruction-level attribution and a separate prototype after shared-runtime fixes.

Strings baseline after ABI: 639 CPU samples, zero lost. Repeated libc comparisons
occur below `str_replace_one`/`breplace`; the old code scans for count and output
separately, materializes subject bytes and copies no-match results. #185 targets
that shared search/replace path first. Raw stacks/flat reports are preserved.

## Shared replacement pass (#185)

Frozen ABI runtime db51239 → replacement runtime 54da20e, seven alternating
release pairs, every rep PHP exit/stdout/stderr gate: strings 650.254→440.361 ms
(0.677×); replace phase 216.959→121.042 (0.558×). Concat 14.530→14.726, objects
454.669→449.984, JSON904.711→900.463, app39.185→39.071: small differences
with overlapping ranges; no gain claimed there. New replacement check covers
binary bytes, case folding, cascading searches, short replacement arrays, keys,
count/subject aliasing, stringable objects and throwing conversion. 59 relevant
string/search PHPT:26pass/33existingfail, all statuses unchanged, no crashes/timeouts.

Replacement now borrows input, returns shared string for no matches, and produces
output/count in one scan per search term. Case-insensitive search still has its
existing byte matcher. Full concat/capacity storage remains a separate decision.

Review arity fix49a3fff is inherited by this branch after frozen step timings;
reported runtime54da20e is not relabelled as the later branch head.

## Shared byte-search filter (#185)

Replacement runtime54da20e → search runtime9961ea5 (includes review arity fix49a3fff),
seven alternating PHP-gated release pairs: strings451.602→263.136ms (0.583×);
replace phase130.805→37.681 (0.288×). Objects435.964→446.988, JSON972.486→969.060,
app42.379→43.353 have overlapping ranges; no gain claimed. These are a separate
paired run, not cumulative percentages or a new PHP ratio measurement.

The shared byte search now filters candidate positions by the first byte before
checking the full needle. Empty needle, binary bytes and offset behavior are
unchanged. Worst-case O(n*m) remains; a two-way search requires evidence on
adversarial/repeated-prefix workloads. No new dependency or builtin-only shortcut.

Validation: exhaustive small binary haystack/needle/all-offset check, workspace
tests/fmt/clippy/release, 24 VM/standalone oracle probes, 19 workload checks;
59 relevant PHPT unchanged (26pass/33existingfail), zero crashes/timeouts.

## Class names and float positive proofs (#186)

Objects profile after shared replacement: cpu-clock:u199Hz, DWARF8192, 1,024
samples, zero lost. Exclusive StrSearcher::new6.84%, below PhpClass::name;
to_lowercase2.44%. Inclusive stack presence (overlapping): property plain-read98,
property-key51, hooks61, binder242, instantiate71, method_invoke320, new_instance218.
This does not isolate pure method lookup; find_method_in appears in only31stacks.

Runtime9961ea5 →6bbe9e0: avoid anonymous marker search for ordinary class names;
reuse existing compiled positive type gates for already-float values (int widening
stays canonical); construct return-diagnostic names only when needed in binder.
Seven PHP-gated alternating release pairs: objects497.716→428.740ms (0.861×),
ctor phase131.357→115.492 (0.879×), norm192.739→168.065 (0.872×). Fib/strings/app
ranges overlap; no benefit claimed. Full promoted binding remains canonical.

Validation: workspace tests/fmt/clippy/release, float widening/null/invalid-return
and anonymous-class regression; 26 VM/standalone oracle probes. 1,965 selected
class/hooks/type/lifetime PHPT:1,710pass/214existingfail/16skip/23unsupported/2existing
GC timeouts at30s, zero crashes; every status unchanged across before/after.
The two timeouts are retained in validation data, not counted as passes.

## Guarded plain this-property resolution cache (#187)

Runtime6bbe9e0 →728f405: per compiled `$this->name` site, cache only a
shared positive plain-read proof's resolved key. Guard exact class and scope;
read current backing cell every time, reprove on missing key/class/scope change.
Private-scope candidate presence guards a cached public fallback key. Hook-body
contexts bypass caching. Metadata pins no receiver or cell, verified by WeakReference.

This is a key-resolution cache, not an offset/shape cache or storage rewrite.
Hooks/magic/uninitialized/visibility failures retain canonical property machinery.

Seven PHP-gated alternating release pairs: objects412.351→377.808ms (0.916×),
norm176.631→149.209 (0.845×), ctor119.736→120.448 (1.006×). Fib/strings/app
changes have overlapping ranges; no benefit claimed there. Their sample noise
is retained in raw data. Cache benefit is specific to repeated method property reads.

Validation: workspace tests/fmt/clippy/release, class/scope rebinding/missing-key/
unset/magic/typed/hooks/weak-lifetime check; 26 oracle probes. Same1,965 class/hooks/
type/lifetime PHPT, all statuses unchanged (including two existing GC30s timeouts),
zero crashes. Offset/array-dim caches and storage-generation changes remain follow-ups.

## Scalar coercion member order without heap strings (#186)

Runtime728f405 →2077344: shared coerce_scalar uses borrowed static member names
and an iterator chain instead of constructing a Vec<String>/lowercase copies
on every coercion. Preserves numeric-string float preference then int/float/string/
bool family order. No separate constructor or builtin coercion policy.

Seven paired PHP-gated release reps: objects347.728→330.505ms (0.950×),
ctor109.308→107.362 (0.982×), scaled240.077→235.577 (0.981×), strings249.068→243.781
(0.979×), fib184.187→192.131 (1.043×), app43.143→41.992 (0.973×). Ranges overlap;
this run establishes no reliable broad speedup. Allocation removal is structural,
not a claim of measured malloc count. Raw samples retained.

Validation: workspace tests/fmt/clippy/release, union order/mixed-case/by-ref/
promoted args/introspection/object conversion check, 27 oracle probes; 709 binding
PHPT636pass/62existingfail/2skip/9unsupported, all statuses unchanged, zero
crashes/timeouts.

## Fixed by-value promoted slot binder (#186)

Runtime2077344 →f4a5c04: eligible fixed by-value constructors with literal defaults
reuse the existing VM argument binder. Publish bound args/locals before canonical
store_prop promotion so hooks can observe constructor context. Body, return/unwind
and destructor passes use the existing VM shell. Named/reference/variadic/expression
default calls retain canonical binding. Scalar raw-value ABI excludes promotion.

Seven alternating PHP-gated release pairs: objects357.569→347.709ms (0.972×),
ctor126.845→118.570 (0.935×), norm155.283→141.282 (0.910×), scaled276.082→248.459
(0.900×). Whole workload improvement is small and ranges overlap; phase medians
do not establish broad PHP parity. Fib/strings/app ranges also overlap.

Validation: workspace tests/fmt/clippy/release; coercion/default/named/inherited/
unpack/hooks/readonly/error regression; 28 oracle probes. 1,974 selected class/
hooks/binding/lifetime PHPT unchanged (1,716pass/217existingfail/16skip/23unsupported/
2existingGC30s timeouts), zero crashes. Existing by-ref promoted-property alias
difference #197 is present on main too; those calls remain canonical, not fixed here.

## Borrowed shared concat (#185)

After byte-search improvements, 504 CPU samples/zero lost on strings200: vm_binary
in125stacks, replacement61 (overlapping stack presence). Shared concat now borrows
string operands, converts other values in canonical left-to-right order, allocates
combined output once. AST binary/compound assignment and VM share the helper;
existing memory-accounting/growth checks stay at their call sites. Storage remains
Rc<[u8]>; final Vec→Rc copying and growing-prefix copies are not eliminated.

Runtimef4a5c04 →a5dc13f, seven PHP-gated release pairs: strings243.260→239.897ms
(0.986×), concat400015.460→15.074 (0.975×), concat1600066.681→55.506 (0.832×).
Small/default cases overlap; benefit on larger concat phase is not a claim of
whole-workload improvement. Whole strings improvement below primarily comes from
shared replacement/search. No capacity-storage rewrite or new key interning claimed.

Validation: workspace tests/fmt/clippy/release, binary/alias/LHS-RHS mutation/
conversion/throw-order check; 29 oracle probes and 19 workload checks. 1,557 relevant
core/string/concat/memory-limit PHPT unchanged (1,390pass/136existingfail/15skip/
15unsupported/1existing30s timeout), zero crashes.

## Fresh main → complete stack comparison

Frozen main5b227c0 →a5dc13f, same host, 7 rotating-order reps of native PHP/main/stack,
all exit/stdout/stderr byte-identical each rep, profiler off. PHP8.5.11 release
(non-debug, non-ZTS), -n, OPcache CLI0/JITdisabled; Rust1.99.0 release/LTO/
codegen-units1. Direct process timing via subprocess.run/perf_counter_ns; cold
startup+parse+execution. No wrapper/container-exec timing. Raw hashes/config/samples
and runnable comparison script are committed.

| Bench | PHP / main / stack median ms | Main/PHP | Stack/PHP | Stack/main |
|---|---:|---:|---:|---:|
| 00-startup | 5.243 / 5.906 / 6.180 | 1.13× | 1.18× | 1.046× |
| 10-fib | 12.891 / 216.288 / 199.412 | 16.78× | 15.47× | 0.922× |
| 11-sieve | 8.072 / 242.473 / 234.809 | 30.04× | 29.09× | 0.968× |
| 20-strings | 14.650 / 629.537 / 242.834 | 42.97× | 16.58× | 0.386× |
| 30-arrays | 19.470 / 232.041 / 219.825 | 11.92× | 11.29× | 0.947× |
| 40-objects | 7.922 / 451.854 / 348.824 | 57.04× | 44.03× | 0.772× |
| 50-regex | 7.553 / 40.285 / 40.392 | 5.33× | 5.35× | 1.003× |
| 60-json | 168.980 / 911.274 / 921.135 | 5.39× | 5.45× | 1.011× |
| 70-db | 28.682 / 120.852 / 115.973 | 4.21× | 4.04× | 0.960× |

Nine-bench geomean10.81→9.29×; stack/main0.860.
This is a separate baseline/configuration from the user-reported geomean3.62.
The earlier user result remains attributed to its source, not overwritten or
combined. Differences are not explained without the user's raw build/timing
provenance. PHP parity is not reached; objects/fib/sieve still have large gaps.
Startup/small-workload dispersion is substantial.

Seven-pair cold application medians were slightly higher with overlapping ranges,
so31 further isolated alternating main/stack pairs were run (no builds/profilers/
PHPT in parallel). App41.711→41.433ms (0.993×), Composer6.927→6.954 (1.004×),
startup4.981→4.756 (0.955×); ranges overlap. Both7/31 datasets retained. No app
speedup or reliable cold-start change claimed. Repeat/slice phase samples also
have overlapping ranges; no specific improvement claimed.

Final fib CPU profile:1,626samples/zero lost, call_site_frame10.947%weighted
inclusive (2.95%exclusive), vm_exec36.96%exclusive/vm_run11.01%. Context fields
are already mainly shared Rc strings; positional args are deferred, vectors pooled.
Full trace rendering rewrite (#184) is deferred for this stack: even hypothetical
removal of the entire ~10.95%bucket cannot close the measured fib gap. It remains
open for a compact live-call-record prototype with equivalent observation/unwind.

Register/native tier (#188) is not selected here. Function buckets include value
operations, bookkeeping and frame movement; unresolved libc addresses/depth limits
remain. Next evidence must separate instruction dispatch from frame/value copying.
Property/dim storage (#187) is partial: guarded key resolution is implemented,
actual inline packed storage/offset caches await JSON/array allocation attribution.
Promoted fixed by-value binding is implemented; broad method ICs remain conditional
and existing by-ref promotion compatibility is tracked separately in #197.

## Stable call frames and deferred VM class/type context (#184)

Merged main3a792db has the same runtime as frozen a5dc13f (only comments differ).
Runtime943eae5 keeps pooled call frames in Box ownership, so stack/pool/teardown
transfer pointers instead of copying the large frame shell. Proven VM calls defer
class/type strings to snapshot_trace_frame alongside their already-lazy arguments.
Callsite/function metadata remains shared Rc strings; named/coercing/failing binder
paths keep eager context. The SPL-stub check avoids scanning plain function files.

Seven alternating PHP-byte-gated samples: default fib197.219→163.765ms (0.830),
typed fib29 789.184→655.135 (0.830); independent untyped followup800.596→633.333
(0.791). Objects initial420.574→384.585 but followup333.736→336.126: no object
gain claim. Sieve medians worsen242.233→255.214 and230.820→244.833 (~6%, ranges
overlap); keep this visible and test subsequent dispatch work against this head.
31 small samples: startup4.805→4.816, app42.911→43.497, Composer7.340→7.546;
no startup/app improvement claim. All speed samples isolated from builds/profiling.

2,160 class/binding/hooks/lifetime/generator/exception PHPT:1824pass,290fail,
16skip,23unsupported,3 existing generator crashes,4 existing 30s timeouts. Every
individual status unchanged. Workspace tests/fmt/clippy pass; regression oracle
checks recursive live args, instance/static class context, retained exception after
frame reuse, nested handler trace, and receiver WeakReference lifetime. Existing
backtrace option semantics are unchanged; this does not claim to fix all trace
compatibility gaps. Raw timings/status maps and hashes in bench/data/92/parity/frame-*.

## Promoted reference aliases (#197, #186)

Runtimeb2c21b3 preserves the canonical binder's by-reference parameter cell when
promoting the property, using shared =& property binding. Shared binding now resolves
uninitialized private keys and rejects readonly references; hook backing reads raise
the existing typed-uninitialized error before reference installation. Named args,
type-owner constraints, owner removal on destruction, alias mutation both ways, two
promotions sharing one cell, private storage, readonly and hooked promotion match
PHP8.5.11 byte-for-byte in the retained runtime test. This is correctness work, not
a speedup claim; by-reference constructors still use canonical binding.

2,160 selected PHPT gain property_hooks/gh16615_002 (1824→1825pass), every other
status unchanged including3existing crashes/4timeouts. All18ctor_promotion PHPT gain
ctor_promotion_by_ref (6→7pass), no regressions. Workspace tests/fmt/clippy pass.
Raw per-test maps and summary in bench/data/92/parity/promoted-reference-gates.json.

## JSON allocation and ASCII decode runs (#187)

Main-equivalenta5dc13f CPU profile:1138 samples, zero lost; malloc7.82%,cfree7.21%,
Utf8Chunks6.50%,String::from_utf8_lossy4.13%exclusive. Runtime03dcdb3 streams
encoding into one sink instead of per-child strings/Vec<String>/join and borrows
string keys. Decoding scans ASCII runs instead of UTF8-validating/copying each byte.
Non-ASCII/escape paths and existing child-null/serialization scopes remain canonical.

Seven alternating samples beforeb2c21b3→03dcdb3: JSON30 941.365→720.657ms (0.766);
JSON150 2966.980→1937.965 (0.653). Strings234.257→234.408,objects336.632→330.512,
sieve246.755→242.513,app45.893→43.204: overlapping ranges, no broad gain claim.
All reps match PHP8.5.11 exit/stdout/stderr.300JSON/hooks PHPT every status unchanged
(218pass,73fail,3unsupported,6existing JSON recursion crashes). Workspace tests,
fmt/clippy pass; retained oracle covers flags/unicode/surrogates/NUL/scalars/key order,
JsonSerializable/hooked getters/nested object decode. Parent reviews00f9c7f/160643a
were merged after these frozen timings; final-stack measurements are separate.

This removes temporary string allocations, not inline packed array storage or
shape-generation caches. Raw hashes/samples/status maps/profile in bench/data/92/parity/json-*.

## Direct slot / literal operations (#188)

Runtime03dcdb3 →cc12b19: BinaryCvConst reads the CV and an immutable literal
directly, avoiding RHS push/pop and one opcode. Shared vm_binary retains all
coercion/overflow/error gates; side-effectful RHS retains its original path.
CPU attribution:1,289samples/zero lost, vm_exec46.63%exclusive, vm_run12.02%.
Annotated opcode PC/decode regions total ~6.5%of vm_exec (~3%overall); other
indirect jumps dispatch Value kinds. Sampling/skid and unresolved libc limits
apply. This is not evidence that pure dispatch dominates or that JIT is needed.

Seven alternating, profiler-off, PHP8.5.11-byte-gated release pairs:
sieve242.843→224.062ms (0.923), default fib152.553→151.499 (0.993),
typed fib29 626.315→618.164 (0.987), objects314.968→309.081 (0.981),
strings211.948→216.170 (1.020), app41.318→42.097 (1.019). Small changes
overlap; only the bounded sieve benefit is selected, not a broad speedup.

Separate seven-run function clocks: first fib10 call (includes compilation,
not pure compile cost)0.097→0.094ms; warm fib29 631.925→615.329ms (0.974).
PHP warm31.379ms; gap remains ~19.6× here. Cold CLI is recorded separately.
The timer is wall microtime, samples are variable, fixed result/exit/stderr gate.
Decision: continue bounded slot/shared-runtime improvements; defer register
rewrite/native tier without larger attributed dispatch cost. No parity claim.

Workspace tests/fmt/clippy and scalar/numeric-string/null/overflow/error/trace/
RHS-mutation oracle pass. Frozen d8cef25 includes parent review fixes;2521PHPT
gain two promoted-reference tests. One GC stress case crosses the30s threshold
under the parallel suite; standalone60s reruns pass on before/after. Nine
existing crashes and five pre-existing timeouts remain. Raw per-file maps,
original timeout and followup thresholds are retained; timing IDs are not
relabelled after parent review fixes. Parent JSON trailing-escape fix is included.

## Capacity-backed PHP byte strings (#185)

Runtime15f5d86 uses Rc<Vec<u8>>; constructing PhpStr moves the Vec instead of
copying its payload into Rc<[u8]>. A proven unique-owner string CV can append
using Vec capacity in AST and VM. Payload-sharing aliases and typed-reference
owners retain canonical concatenation/write gates. Weak handles detach before
mutation; tracked grow charges transfer by the old identity after a live sweep.
One Rc shell is still allocated per append for cache invalidation; there is no
stable-generation cache or inline packed-array storage claim. Existing memory
accounting heuristics remain, measured compatibility is reported below.

Correct frozen release verified by distinct hash and append symbols; shared Cargo
target initially reused the parent artifact. That copy was quarantined before
speed measurements, core cleaned/rebuilt, and never used for published timings.
Final6dbf71c includes parent trailing-JSON-escape rejection; valid-workload timings
retain15f5d86. Measured x86_64 layout: Value16B, Cell8B, TraceFrame160B.

Seven alternating slot-final d8cef25 →15f5d86 pairs, all exit/stdout/stderr
PHP-byte-identical, profiler off: concat4000 14.753→13.318ms (0.903),
16000 63.661→33.017 (0.519),64000 643.407→106.854 (0.166).
Strings251.316→191.829 (0.763), fib165.675→152.419 (0.920),
arrays226.493→213.427 (0.942), objects387.853→371.660 (0.958),
JSON738.307→637.630 (0.864). This removes increasing-prefix copying on the
proven path; small-case noise remains and individual steps are not multiplied.

Fresh native PHP/main3a-runtime-equivalent a5dc13f/15f5d86 comparison,7rotating
reps, profilers off, every exit/stdout/stderr PHP byte-identical, cold CLI:

| Bench | PHP / merged-main baseline / stack median ms | Main/PHP | Stack/PHP | Stack/main |
|---|---:|---:|---:|---:|
| bench/00-startup.php | 4.269 / 5.274 / 5.121 | 1.24× | 1.20× | 0.971× |
| bench/10-fib.php | 13.577 / 204.566 / 147.123 | 15.07× | 10.84× | 0.719× |
| bench/11-sieve.php | 8.427 / 244.067 / 246.210 | 28.96× | 29.22× | 1.009× |
| bench/20-strings.php | 14.546 / 247.204 / 182.590 | 16.99× | 12.55× | 0.739× |
| bench/30-arrays.php | 18.685 / 237.447 / 206.414 | 12.71× | 11.05× | 0.869× |
| bench/40-objects.php | 8.599 / 385.936 / 364.962 | 44.88× | 42.44× | 0.946× |
| bench/50-regex.php | 8.604 / 42.705 / 44.795 | 4.96× | 5.21× | 1.049× |
| bench/60-json.php | 187.067 / 1016.983 / 649.746 | 5.44× | 3.47× | 0.639× |
| bench/70-db.php | 31.481 / 127.692 / 124.250 | 4.06× | 3.95× | 0.973× |
| bench/app/cli.php | 6.110 / 44.789 / 43.648 | 7.33× | 7.14× | 0.975× |
| examples/composer/run.php | 5.324 / 9.096 / 8.114 | 1.71× | 1.52× | 0.892× |

Nine-bench geomean9.398→8.155×, stack/main0.868 (~13.2%less time).
Strings−26.1%, JSON−36.1%, fib−28.1%; objects−5.4%but still42.44×PHP here,
sieve1.009×main and regex1.049×main with substantial overlapping dispersion.
Cold app0.975×main, Composer0.892×main; seven small samples are insufficient
for a broad app gain claim. This is a fresh same-host native setup and is separate
from the existing user-reported main3a geomean3.05×; no cross-host ratio mixing.
Main moved after these samples: baseline explicitly3a/a5dc, not ef63392.

Workspace tests/fmt/clippy pass.26 VM/probe and19 workload byte gates pass,
including binary/NUL bytes, aliases, refs, typed-owner fallback, RHS mutation,
weak UTF8-cache invalidation after append and large tracked-string growth.

Final2521PHPT:2060→2063pass,399→397fail,9unchanged existing crashes,
5→4timeouts. Promoted-reference two fail→pass and concat_003 stress timeout→pass;
all other individual statuses unchanged, including memory-limit/string/PCRE/
class/hooks/lifetime selections. The GC30s threshold crossing on the earlier
slot suite passes here. Original maps and individual followups retained; do not
interpret changed timeout counts as full GC/JSON/generator compatibility.

## Guarded scalar CV array reads (#187)

Runtime6dbf71c →05804b7: DimCv reads an existing scalar at an integer key from
the current table, avoiding canonical frame materialization/refresh. Plain CVs
only; superglobals are excluded, and top-level global views, missing/unknown keys
non-array/non-scalar values retain the original AST bridge. Hybrid binding remains
canonical; reference-return functions still cannot compile. No offset cache,
storage generation/shape change or inline packed representation is claimed.
Current-table lookup naturally observes unset/sort/splice/unshift/CoW/ref writes.

First e45b99d prototype did not unwrap parser source-line markers, so the fast
opcode was not emitted. It was not published as an implementation. Its early
samples overlap0.18s oracle work and are retained only as prototype history.
05804b7 unwraps transparent source markers and has one retained runnable check
for compiler opcode eligibility plus PHP-matching mutation/reference/global-view/
missing-key/ArrayAccess/named-call results. Workspace tests/fmt/clippy pass;
27VM/probes and19workload byte gates pass. Correct release core cleaned/rebuilt,
source/runtime/hash explicit in array-read-provenance.json.

Seven rotating PHP/parent-stack/new-stack reps, no builds/PHPT/profilers in parallel:
sieve222.772→212.659ms (0.955), arrays196.298→186.877 (0.952).
Independent seven alternating larger sieve3×200k1482.571→1363.094 (0.919).
Other cases overlap; nine-bench geomean7.958→7.987×PHP (1.004×parent), no
whole-suite gain claimed. PHP/config/absolute medians/raw samples all retained.
Do not combine this dataset with the main3a→capacity dataset by multiplying steps.

Seven Composer samples initially7.204→7.777ms (1.079×parent);31fresh isolated
alternating followups7.030→7.168 (1.020). App40.718→40.927 (1.005),
startup5.253→5.205 (0.991), ranges overlap. Both datasets retained, no app win.

3390relevant PHPT:2671pass/639fail/29skip/34unsupported unchanged. Original
baseline11crashes/6timeouts becomes13crashes/4timeouts: two range stress cases
timeout→crash. The unchanged builtin range f64 loop cannot progress near
PHP_INT_MIN; these files exercise no new dimension opcode. Standalone same30s
before/after runs both crash on each case, confirming pre-existing failure mode,
not hiding the original statuses. Other individual statuses unchanged. Raw maps,
thresholds and range followups retained; no full PHP compatibility claim.

#187 remains partial: bounded property-key cache, JSON shared sink and scalar CV
array reads implemented. Actual packed inline storage/declared offset layouts
remain pending evidence and ownership/lifetime gates, explicitly tracked in #92.

After-read CPU profile946samples/zero lost: vm_refresh4.33%exclusive,
vm_materialize6.87%,global_var_cell12.58%,assign_inner5.39%,
assign_index_path3.59%; remaining writes still use canonical bridges.
Malloc2.22%/cfree3.07%do not by themselves justify inline packed ownership
rewrite on sieve. LLVM moved the main bucket into vm_exec_ops15.12%; this
is not pure dispatch attribution or directly the old vm_exec symbol bucket.
