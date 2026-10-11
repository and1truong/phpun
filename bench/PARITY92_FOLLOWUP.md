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

## CPU, requested allocations and observed copy bytes (#205 supplement)

Complements merged `PROFILE92_205.md` (#211). Frozen **main at collection
start ef633928471a36c7f180b30fa1ff52fc0a4dcb97**, force-clean release core,
SHA2561f10ddd3487e93b74a91812e49b3d2b51c01021eae5253c4817028f5e98ab205.
This is not the later main e847541 (which also merged #202–#212); do not
relabel profiles or claim its performance from these measurements.

Linux6.18.44 x86_64/glibc2.41; Rust1.99.0 release/LTO/codegen-units1;
native PHP8.5.11 -n SHA25632fa863f672cfe0048b95d08fc1593f78bf97971823207a338f3f82d730e564f.
CPU sampling `cpu-clock:u199Hz --call-graph dwarf,8192`, five byte-gated runs,
all **zero lost samples**. gprofng heap collection uses **CPU sampling OFF**,
no timer warning; copy interposer is a separate run, no speed claims from either.
Every target exit/stdout/stderr compared to PHP; only known collector records
removed. Original arrays20 heap run timed out without RESULT: excluded entirely.

| Workload (heap/copy argv) | Heap requests | Requested heap bytes | Observed copy calls | Observed copy bytes |
|---|---:|---:|---:|---:|
| objects 1 5000 | 1,089,919 | 36,838,813 | 842,470 | 86,049,106 |
| arrays 1 | 135,429 | 11,806,556 | 159,820 | 28,915,368 |
| json 1 | 792,613 | 34,951,969 | 955,353 | 19,833,928 |
| fib 20 | 46,273 | 4,857,550 | 194,240 | 29,066,596 |
| sieve 1 5000 | 102,022 | 5,432,527 | 98,535 | 10,519,481 |

CPU uses longer argv (manifest), while heap/copy includes cold startup/parse
at bounded sizes. No extrapolation or claim that composition is invariant.
Heap function rows are **inclusive, overlap and have truncated unwind**:
objects store_prop3,160,000B, scope_private_prop1,400,140B and
PropDecl::clone1,201,491B are useful attribution, not additive percentages.
Global Rust alloc counter and gprofng totals differ by collector/bootstrap scope.
`cell` site counts in ef63392 omit direct element Rc allocations; #211 separately
wired those counters. No claim that ef's site counter counts every element Cell.

| Workload | Exclusive CPU symbols (not pure decode) | Additive observed-copy callsite shares |
|---|---|---|
| objects (1752samples) | malloc5.48%,cfree6.62%,to_lowercase2.51%,is_a_str2.17%,PropDecl clone0.86% | scope_private_prop17.71%,store_prop13.12%,PropDecl clone10.87%,invoke_fn_run10.13%,Param to_vec8.20% |
| arrays (724) | vm_run15.61%,vm_exec12.85%,sort comparator10.22%,call_value4.28%,malloc/cfree3.87%each | bounded1rep:call_value36.30%,trace_pop13.33%,call_site_frame13.32%,vm_run6.66% |
| JSON (642) | json_value9.66%,json_enc7.48%,malloc8.26%,cfree9.03%,UTF8 chunks5.45% | Vec grow15.99%,eval10.46%,json_value9.49%,write_str9.11%,join_generic5.13% |
| fib (931) | vm_exec44.68%,vm_run14.29%,call_site_frame9.13%,vm_binary3.22% | trace_pop24.10%,call_site_frame24.09%,vm_exec12.68%,vm_run12.65% |
| sieve (1007) | global_var_cell15.79%,vm_exec12.02%,materialize6.55%,refresh6.36%,assign_inner5.16% | bounded5000:eval11.70%,dim_op_outlet11.69%; parser/AST startup substantial |

Separately, arrays20 observed611,105,245B/3,265,348copies:
call_value49.99%, trace_pop18.35%, call_site_frame18.35%, vm_run9.17%.
Small arrays1 percentages differ, demonstrating why size-invariance is not assumed.
Objects first three callsite buckets sum41.70% of **observed copy bytes**.
These are **out-of-line libc memcpy/memmove lower bounds**, not all copies:
LLVM inline copies, hidden libc implementations and realloc movement are unobserved.
Byte shares are not CPU shares or allocation shares. LLVM inlining also mixes
semantic operations into vm_exec; prior slot ASM work bounds decode instead.

Decision: **R4 first** (callback/trace-shell movement and remaining canonical
array-write bridges), **R2 next** (method-name normalization and avoid needless
property/parameter metadata clones). R3 packed storage is conditional on a
current-tip prototype plus accounting/ownership gates; this data does not prove
rewriting storage or JIT is required. JSON shared sink/capacity improvements
already merged after this baseline. #211's different-host malloc/CPU attribution
supports the same ranking, but its counts and speed ratios stay separate.
No runtime optimization in this supplement.

Reproduce (Linux/gcc/nm, single-thread CLI only, no new dependency):

```sh
gcc -std=c11 -Wall -Wextra -Werror -O2 -fPIC -shared -fno-builtin \
  -o /tmp/copy-profile.so bench/data/92/parity/copy-profile.c -ldl
gcc -std=c11 -Wall -Wextra -Werror -O2 -fno-builtin \
  -o /tmp/copy-check bench/data/92/parity/copy-check.c
python3 bench/data/92/parity/profile-copies.py --check \
  --php /tmp/copy-check --binary /tmp/copy-check --library /tmp/copy-profile.so \
  --save /tmp/copy-check.json --runtime self-check
# Check asserts collector2calls384bytes, memcpy256 + overlapping memmove128.
python3 bench/data/92/parity/profile-copies.py --php /path/php85 \
  --binary /path/frozen-phpun --library /tmp/copy-profile.so \
  --save /tmp/objects-copy.json --runtime EXACT_BUILD_COMMIT bench/40-objects.php 1 5000
gprofng collect app -p off -S off -H on -O /tmp/objects.er \
  /path/frozen-phpun bench/40-objects.php 1 5000
gprofng display text -metrics i+heapallocbytes:i+heapalloccnt:name \
  -sort i.heapallocbytes -functions /tmp/objects.er
perf record -e cpu-clock:u -F 199 --call-graph dwarf,8192 -o /tmp/objects.data \
  /path/frozen-phpun bench/40-objects.php 100 5000
perf report --stdio --no-children --call-graph none -i /tmp/objects.data
```

Raw callsites/flat stacks/heap rows/counters compressed under
`data/92/parity/attribution-*`; manifest contains exact argv/hash/limits.
Interposer fixed8192table avoids allocator recursion, rejects overflow.
Lazy resolver pointers are intentionally only safe for the single-thread CLI;
server/parallel use requires synchronized initialization before reuse.

## Fresh ef63392 → ca76c30 profiler-off comparison

Separate from profiling and previous main3a→15f datasets. Seven rotating
PHP/main/stack reps, all byte gates pass. ca76c30 is #210's pre-merge runtime;
main e847541 additionally includes #212 changes, so **not current-tip timing**.
Dirty-source flags are preserved: the runner/report files were untracked; binaries
were frozen from clean core builds and their hashes identify exact runtime code.
No multiplication of step gains or mixing with the different-host3.05× report.

| Bench | Main / candidate median ms | Candidate/main | Candidate/PHP |
|---|---:|---:|---:|
| 00-startup | 5.243 / 6.102 | 1.164× | 1.44× |
| 10-fib | 169.065 / 159.360 | 0.943× | 13.83× |
| 11-sieve | 254.046 / 215.830 | 0.850× | 26.05× |
| 20-strings | 223.109 / 168.477 | 0.755× | 12.24× |
| 30-arrays | 203.661 / 201.654 | 0.990× | 11.26× |
| 40-objects | 354.827 / 333.854 | 0.941× | 41.59× |
| 50-regex | 41.114 / 43.254 | 1.052× | 5.93× |
| 60-json | 980.606 / 585.310 | 0.597× | 3.54× |
| 70-db | 122.683 / 124.735 | 1.017× | 4.17× |
| cli | 40.108 / 40.298 | 1.005× | 7.78× |
| run | 6.737 / 7.146 | 1.061× | 1.67× |

Nine-bench geomean9.481→8.607×PHP; candidate/main0.908 (~9.2% less time).
Objects still41.59×PHP on this host. Initial seven startup samples +16.4%;
31fresh isolated alternating followup5.584→5.796ms (+3.8%),
Composer7.625→8.021 (+5.2%), app44.622→42.154 (−5.5%).
Ranges overlap but the cold startup/Composer cost remains visible; no universal
application win or claimed explanation. Both sample sets retained verbatim.

## Avoid property metadata cloning for absent hooks (#187 bounded R2 followup)

Frozen main e847541→runtime6afc6bd (built sourcec5e7eca).
`scope_private_prop` returns immutable declaration index + owner class metadata;
its two callers borrow the declaration and clone only when returning an actual
hook or owned declaration. `hooked_prop` similarly retains the nearest declaration
owner/index until a hook exists. Caller scope name is borrowed rather than copied.
No new cache, layout, mutable property offset or receiver/Cell pin. Class metadata
is immutable during this read-only lookup; handles are local and dropped on return.
Private priority, inherited get/set composition and canonical type/visibility/
reference stores remain unchanged. This does not complete packed storage #187.

Validation: one retained runnable runtime check covers plain/private parent-child
shadowing, inherited hooks through plain redeclaration, get override inheriting
set, typed-reference rejection, outside-private errors and WeakReference cleanup.
Workspace5core/22runtime/19format checks pass; fmt/clippy pass. Twenty-one VM
fixture scripts + metadata source probe and19 workload checks exact byte/exit/
stderr gate against PHP8.5.11 -n. All619 relevant PHPT individual statuses
unchanged vs frozen e847541:551pass/63existing fail/5unsupported, no crash/timeout.
Original per-file maps, probes and binary provenance retained, no full PHP claim.

Separate same-argv objects1×5000 instrumented runs:

| Measure | Before | After | Ratio |
|---|---:|---:|---:|
| Rust allocation requests | 1,079,560 | 1,009,544 | 0.935 |
| Requested allocation bytes | 32,794,742 | 32,294,632 | 0.985 |
| Observed copy calls | 832,308 | 742,284 | 0.892 |
| Observed out-of-line copy bytes | 82,444,379 | 72,340,424 | 0.877 |

No speed claims from instrumented times. Observed copy lower bounds omit LLVM
inline/hidden/realloc movement; LLVM also moves callsites between symbols, so
scope_private_prop disappearing from the symbol table is not complete logical
copy elimination. Root structural change removes unnecessary declaration copies;
physical counters show less allocation/copy traffic on the same whole workload.

Seven alternating profiler-off PHP-byte-gated release pairs:

| Whole workload | Before / after median ms | After/before |
|---|---:|---:|
| bench/40-objects.php | 331.029 / 319.096 | 0.964 |
| bench/40-objects.php 20 5000 | 1422.245 / 1381.901 | 0.972 |
| bench/30-arrays.php | 192.393 / 189.202 | 0.983 |
| bench/60-json.php | 574.502 / 565.021 | 0.983 |
| bench/10-fib.php | 141.143 / 138.366 | 0.980 |
| bench/00-startup.php | 4.977 / 5.253 | 1.055 |
| bench/app/cli.php 100 20 | 42.067 / 40.689 | 0.967 |
| examples/composer/run.php | 7.313 / 7.191 | 0.983 |

Default objects−3.6%,long objects−2.8%; ranges overlap, modest median changes,
not a parity/broad-app claim. Other medians/ranges retained; cold startup+5.5%
visible, seven small samples not enough to attribute. No further repeat justified
for this bounded change. First timing runner stopped at a nonexistent app path
AFTER five byte-gated cases; no error/mismatch case used for timings, apps collected
separately with correct existing path. Both raw datasets retained.

Hashes: main76fd8bb184d7853ddfe972b311feb691a9390d8a1c949b05441d49ddb6db9400;
after5f3de127ff71092ae4a1e776f72dd282ae5302b5bc997b6696c8a1461edae89b.
Both release cores force-cleaned/rebuilt (Rust1.99.0 LTO/codegen-units1);
raw `data/92/parity/metadata-*`, same native host/PHP as preceding supplement.

## Proven user-sort argument pairs (#215)

Final runtime92d0be0421f5eb845a7f25db3d74330fa762e5ad on reviewed property
metadata sourcec5e7eca; parents merged into main31316ca via #213/#214/#216.
Only **usort/uasort/uksort** fresh pairs enter the shared value ABI. A context-free
capture-free fixed-arity primitive closure must pass compiler value-ABI and exact
parameter-type gates, without reference/variadic/promotion requirements. Caller
pending aliases/class/generator/hook context retain canonical binding. Metadata
owner stays on the frame through the existing closure_rc field, cleared before
pooling. The compile cache owns declarations, not the callable instance.

Canonical callbacks create their original fresh cells directly, preserving sort's
nonref flags. They never traverse an extra value vector. map/filter unchanged.
This is neither a comparator arithmetic special case nor a universal dynamic-call
ABI. Bound/captured/method/string/object/coercing/default/unresolved cases stay
canonical. No new cache or packed layout; #187 remains open.

**Prototype history retained:** all-phase6ea4a3a removed49.9% requests, but focused
map100k145.315→159.396ms (+9.7%). Therefore map/filter implementation was removed.
Sort-only074da0a still transferred values through a vector on fallback;
app41.512→46.781 (+12.7%) in that sample. Final92d0be0 gates first and sends
fallbacks directly to cells. These datasets keep original runtime hashes, not
relabelled as final or silently dropped. Do not multiply gains across them.

One retained runnable core check: live func_get_args/ancestor backtrace argument
writes during sort, map/filter/captured/bound/default/coercion fallback, integer
key sorting, by-ref warning flags, boolean retry/deprecation, exception unwind
and continued calls. RustWeak verifies pooled frames do not retain callable
owners, and a value-vector capacity witness verifies the actual ABI was reached.
PHP WeakReference::create(Closure) is unsupported on parent, so no claim of that
PHP gate. Separately a preexisting canonical object-argument dtor/WeakReference
mismatch is retained with source + PHP/parent/prototype/final results: every
candidate matches the parent, but this is **not an oracle-passing case**. No broad
PHP lifetime compatibility claim or silent baseline deletion.

Final workspace6core/22runtime/19format checks, fmt/clippy pass;
21VM fixtures + new probe exact exit/stdout/stderr == PHP8.5.11 -n;
19 existing workload oracles pass. All583relevant array/sort/closure/argument/
exception/backtrace PHPT individual statuses unchanged:453pass/120existing fail/
3skip/7unsupported/zero crash or timeout. Original per-file maps retained.

Separate byte-gated arrays20 instrumented counts:

| Measure | Parent | Final | Ratio |
|---|---:|---:|---:|
| Rust allocation requests | 2,649,899 | 1,408,498 | 0.532 |
| Requested bytes | 81,127,379 | 51,333,823 | 0.633 |
| Observed copy calls | 2,894,832 | 2,894,832 | 1.000 |
| Observed out-of-line copy bytes | 487,078,120 | 380,317,386 | 0.781 |

No speed from instrumented times. Copy lower bounds omit LLVM inline/hidden/
realloc movement. The `cell` site counter remains40125 because fresh builtin
callback cells bypass that helper; it is not an all-Cell counter. `vm_site`
requests6→310357 reflects the shared direct binder's arena protocol, not that
many new physical heap allocations: sites are reused. Physical request totals
above are measured separately, not inferred from site hits.

Seven final alternating profiler-off PHP-byte-gated release pairs:

| Workload | Parent / final median ms | Final/parent |
|---|---:|---:|
| bench/30-arrays.php | 194.526 / 180.164 | 0.926 |
| bench/30-arrays.php 100 | 1119.369 / 1044.455 | 0.933 |
| bench/profile/arrays.php usort 100000 | 918.011 / 818.829 | 0.892 |
| bench/40-objects.php | 414.289 / 442.686 | 1.069 |
| bench/60-json.php | 656.371 / 663.564 | 1.011 |
| bench/10-fib.php | 153.354 / 156.922 | 1.023 |
| bench/11-sieve.php | 222.492 / 230.378 | 1.035 |
| bench/app/cli.php 100 20 | 42.258 / 41.617 | 0.985 |
| examples/composer/run.php | 7.199 / 7.489 | 1.040 |
| bench/00-startup.php | 5.387 / 5.427 | 1.007 |

Arrays−7.4%,larger100−6.7%,usort phase−10.8%; ranges overlap and no parity
claim. Objects+6.9%,JSON+1.1%,fib+2.3%,sieve+3.5%,Composer+4.0% remain visible;
no attribution to the callback path for workloads that do not execute it.
Seven app42.258→41.617 (0.985), then31fresh isolated alternating followup
40.243→41.219ms (1.024), ranges overlap. No broad app win, no asserted removal
of every performance regression. The canonical fallback extra transfer is
structurally removed; these timings do not isolate its causal contribution.

Each release core force-cleaned/rebuilt; parent SHA256
5f3de127ff71092ae4a1e776f72dd282ae5302b5bc997b6696c8a1461edae89b,
finaldf19afc5bf14758f5021e028ac1ee7b97ea209be35d8e1d9c90f642d5d1ad63b.
Same native host/Rust/PHP config as preceding steps, frozen identities/config/
argv/maps/counters/callsite lower bounds/samples under `data/92/parity/callback-*`.

Integration: PR #218 targets main. After main advanced to0179ed3 (#217),
ordinary merge81ca6ef had no conflict. Workspace tests/fmt/clippy and rebuilt
release22VM/callback byte gates plus19workload gates pass. This integration
binary is recorded separately in `callback-main-integration.json`; the timings
and583PHPT maps above remain frozen92d0be0 evidence, not timing/status claims
for a different binary.

## Current-main direct arena rollback (#236, main #235)

Frozen main e202891 → runtime9d1544f, dedicated release target with unchanged
Rust/PHP configuration. User-reported main geomean2.71× and objects7.88× live
in #92 as a separate host/config source. Do not substitute the ratios below
for that dataset or multiply step gains.

Direct binders already have a live frame: current-segment span rollback now
sits on that frame, without Rc<VmSite>/MemCharge lookup/token-vector transfer.
Cold page growth retains the shared canonical token/accounting path; canonical
argument evaluation tokens unchanged. Frame pop repays the span on all returns,
unwind and canonical rebinds, and pooled frames retain no span. Accounting is
kept, not disabled.

ONE existing arena runnable check extended with actual inline-reservation
witness (no token/charge added) and clean rollback/pool invariants. Workspace
6core/22runtime/19format checks,fmt/clippy pass.23VM/callback/160-arg page-growth
probes and19workloads exit/stdout/stderr byte-identical to PHP8.5.11 -n.
3406PHPT: both2727pass/603existingfail/25skip/38unsupported; main6crash7timeout,
candidate5crash8timeout. Only individual status change is known
range_bug71132 crash→timeout at10s. Standalone30s both crash, unchanged
f64 range unable to progress near PHP_INT_MIN. Original maps retained.

Main CPU fib34:1379samples,zero lost; vm_exec_ops27.56%,vm_run15.23%,
vm_run_direct8.05%,call_site_frame5.08%,vm_site3.63%,vm_frame_free1.74%
exclusive symbols. Ops symbols include semantics, not pure opcode decode.
Objects150×5000 has zero lost; malloc/free~10.5%,destruct_cells2.9%,
GC collector3.0%. Diagnostic phase times are not speed timings.

Separate PHP-gated physical allocation requests: fib26 425169→32254(−92.4%),
requested bytes18562026→5979410(−67.8%). vm_site392835→1 is an EVENT counter,
not the physical total. Objects20×5000 requests14563261→14463259,
bytes468286008→465086016: small effect on that path. Observed out-of-line
fib26 copies394512846→391370846B; call_site_frame/trace_pop still each~32%
of observed bytes. Lower bounds omit inline/hidden/realloc movement and are
not CPU percentages. This selects stable trace-shell/call-state movement #237
next; does not prove native/JIT would resolve object allocation.

Seven rotating PHP/main/candidate profiler-off pairs, every byte gate passes:

| Workload | PHP/main/candidate median ms | Candidate/main |
|---|---:|---:|
| bench/00-startup.php | 3.297/5.605/5.317 | 0.949 |
| bench/10-fib.php | 10.765/160.221/140.032 | 0.874 |
| bench/11-sieve.php | 7.309/240.324/216.303 | 0.900 |
| bench/20-strings.php | 12.833/174.494/160.350 | 0.919 |
| bench/30-arrays.php | 15.886/187.400/166.196 | 0.887 |
| bench/40-objects.php | 7.324/255.108/258.814 | 1.015 |
| bench/50-regex.php | 6.272/38.407/39.250 | 1.022 |
| bench/60-json.php | 145.695/519.124/510.340 | 0.983 |
| bench/70-db.php | 27.996/116.149/111.178 | 0.957 |
| bench/app/cli.php 100 20 | 4.655/39.894/41.961 | 1.052 |
| examples/composer/run.php | 4.257/7.919/8.230 | 1.039 |

Native nine-bench geomean9.083→8.571×PHP,
candidate/main0.944. Fib−12.6%,sieve−10.0%,arrays−11.3%;
objects+1.5%,regex+2.2%,app+5.2% retained; no object/parity/broad-app claim.

31fresh isolated follow-up (PHP gated):
- bench/app/cli.php 100 20: 41.967→40.052ms (0.954×), ranges38.296–52.686/38.022–46.801.
- examples/composer/run.php: 7.627→7.626ms (1.000×), ranges7.037–8.290/7.145–9.940.
- bench/00-startup.php: 5.484→5.631ms (1.027×), ranges5.117–6.667/5.060–6.359.

Initial app regression does not reproduce in31samples; ranges overlap and
startup+2.7% retained. No broad app claim. Hashes/config/argv/samples/status
maps/CPU source rows/copy counts are in `data/92/parity/oct11/arena-*` and
`*-main-*`; large raw perf.data remains in workspace. No perf.data from old
main relabelled as current.

## Rejected trace/call-state experiments (#237)

Parent runtime9d1544f (#238). Full stable Box<TraceFrame> pool plus borrowed
CallArgs prototype fa968a7 passed workspace checks,23 PHP script gates,19
workloads. Observed out-of-line copies fell391370846→11102261B (−97.2%),
physical allocations32254→32282. This does **not** establish a CPU win.
Seven isolated, byte-gated alternating pairs gave fib135.467→134.103ms
(0.990×),sieve203.342→225.155(1.107×),objects253.126→265.411(1.049×),
regex39.918→43.941(1.101×). Arrays0.967×,JSON1.002×,DB1.053×,
app100,20 1.024×,Composer0.957×; all raw medians retained. Rejected.

3406PHPT full prototype2726pass603fail25skip38unsupported6crash8timeout.
Two recorded transitions: gc63635 pass→10s timeout and range_bug71197
timeout→crash. Standalone30s gc63635 passes main3939ms,parent3999ms,
prototype3756ms. Original batch status maps retained; no all-status-identical
claim. Known pathological range behavior unchanged, not fixed here.

Borrow-only prototype06d6005 removes stable trace shells to isolate the
CallArgs effect. Workspace tests/fmt/clippy and all captured PHP gates pass.
Fib147.550→153.502ms(1.040×),sieve0.989×,strings1.023×,arrays1.023×,
objects1.020×,regex1.000×,JSON0.983×,DB0.967×. Separate small app10,2
0.941×/Composer0.964× retained; this app input differs from full prototype.
The eight-bench run stopped on an invalid app filename before candidate
execution; partial rows preserved, valid apps measured separately. Rejected.

Both runtime changes were reverted in ordinary commits. Final runtime tree
is identical to #238 parent; the prototype history/patch, exact hashes,
raw timings, physical requests/copies, PHPT maps and standalone resource
rechecks are in data/92/parity/oct11/trace-* and borrow-*. #237 remains open.
This rules out copy-byte counts as sufficient evidence for the next tier;
method teardown and removing repeated scalar call/dispatch work remain next.

## Receiver teardown guard (#239)

Runtime75e78c5, parent#238 runtime9d1544f (trace prototypes reverted). Skip
frame object-cell vector/hashmap bookkeeping only when the receiver has an
owner beyond this frame's raw handle+$this cell and no other frame object
cell/arg/alias must be considered. Other frames use the shared destructor
pass. ONE runnable WeakReference/object-arg/TypeError/named-call ownership
check covers normal returns, fallback and owner release.

Workspace7core/22runtime/19format checks,fm/clippy,23 VM/arena/receiver
script+19workload PHP byte gates,measure/http-load/VM-profile checks pass.
3406PHPT comparable2727pass603fail25skip38unsupported6crash7timeout.
Only range_bug71132 timeout→crash relative parent, known pathological
range near INT_MIN. Initial misconfigured SKIPIF-under-SUT map retained
separately; all3406 rerun with --sut candidate/--php native reference.
No gain claim from altered skip conditions.

Seven isolated alternating profiler-off pairs, PHP exit/stdout/stderr gate:

| Workload | Parent/candidate median ms | Candidate/parent |
|---|---:|---:|
| objects5×5000 | 253.969/231.728 | 0.912 |
| objects20×5000 | 1008.835/960.085 | 0.952 |
| profile objects norm10 | 6.335/6.319 | 0.997 |
| profile objects ctor10 | 6.492/6.370 | 0.981 |
| fib26 | 135.418/134.564 | 0.994 |
| app100,20 | 41.583/35.356 | 0.850 |
| Composer | 7.892/7.738 | 0.980 |

Separate physical objects20×5000 requests14463259→13463260(−6.9%),
bytes465086016→388949018(−16.4%). Object allocations still100000,
remaining storage/GC work not solved. No broad parity claim or multiplying
step ratios; raw timings/hashes/maps/correction source in oct11/receiver-*.

## Guarded pure integer native experiments (#240, superseded by review fix below)

Final runtime d089dbb, parent runtime75e78c5 (#242). x86_64 Linux only; other
platforms retain VM. Reuse existing Ops/declaration cache/libc. No fib pattern
recognizer, memoization or benchmark-specific program. Pure global fixed-arity
integer call graphs only; no refs/defaults/variadics/attrs/namespaces, canonical
AST bridges, globals, builtins/introspection, objects/arrays/closures/methods.
Root binding/trace/return/destruction stay shared. Check integer overflow,
initialized locals, dependency identity, depth64 and current arena capacity.
Unwind all nested spans and replay pure root in VM on failure; page growth,
errors and deadlines retain VM behavior. No observable prefix is replayed.
Code pages are RW then RX, owned until calls end; code graph bounds and8MiB
rounded mapped-page budget enforced. Failed mapping/code generation falls back.

ONE runnable actual-native execution witness plus overflow/TypeError/depth/
uninit/page-capacity replay and clean arena check. Depth100 VM replay needs
32MiB debug test-thread stack; initial2MiB thread overflow retained as a
resource limitation, no production stack change.25 PHP script probes include
mutually recursive graphs, conditional joins, increment/decrement, factorial
overflow, receiver ownership and160-arg arena growth;19 workloads all byte/
exit/error gated. Workspace8core/22runtime/19format,fm/clippy and VM-profile
checks pass.3406PHPT2727pass603fail25skip38unsupported6crash7timeout; two
known range crash/timeout changes relative parent, no new fail/pass change.

Seven rotating native PHP/parent/candidate isolated profiler-off pairs:

| Workload | PHP/parent/native median ms | Native/parent | Native/PHP |
|---|---:|---:|---:|
| startup | 3.747/6.167/6.328 | 1.026 | 1.689 |
| fib26 | 11.461/136.707/8.512 | 0.062 | **0.743** |
| sieve | 7.265/204.239/216.819 | 1.062 | 29.846 |
| strings | 13.302/159.997/162.384 | 1.015 | 12.208 |
| arrays | 16.999/172.540/175.252 | 1.016 | 10.310 |
| objects | 7.860/251.919/248.384 | 0.986 | 31.600 |
| regex | 7.570/38.865/39.701 | 1.022 | 5.245 |
| JSON | 177.883/547.290/561.897 | 1.027 | 3.159 |
| DB | 28.939/121.728/113.546 | 0.933 | 3.924 |
| app100,20 | 4.903/34.930/34.251 | 0.981 | 6.986 |
| Composer | 4.470/7.812/8.517 | 1.090 | 1.905 |

Nine-bench native-host geomean8.060→5.973×PHP;time step0.741 (−25.9%).
Fib−93.8% and beats native PHP on this host; **full suite is not at parity**,
not a replacement/extrapolation for the user's2.71× dataset. Negative sieve/
Composer/JSON medians retained. Isolated31 follow-up: sieve209.919→215.262
(+2.5%),arrays173.038→172.421(flat),Composer8.109→8.280(+2.1%).
No other workloads claimed faster from this global-integer tier.

Initial83e0b38 prototype also beats PHP on fib but regresses sieve12.9%,
arrays5.3%,objects2.6%. Cache moved off Compiled then off Interp to existing
declaration cache; method/callback VM shells avoid outlined native entry.
All four artifact hashes, raw negatives and cache-layout trials retained.
Cache-map startup overlapped a short oracle batch and is excluded from speed
claims; raw row preserved. Its other rows and final/follow-up timings isolated.

Physical fib26 requests32254→32269,bytes5979410→5983254: no allocation win.
trace_frame392835→85 is an EVENT count; native inner calls do not create
observable PHP frames unless VM replay is needed. Mapped executable pages are
separate from malloc requests. Old OPcache-module-not-loaded description in
arena manifest corrected: native PHP module is loaded, enable_cli=0,JIT=disable;
raw PHP hash/config/numeric timings unchanged. Native raw script's stale
83e0b38 label is corrected by hash→runtime manifest (not rewriting samples).

Next: pinned constructor metadata (#243), pure integer comparator native
entry before callback frame/binding, then real array/object storage and string
builtin costs. Parent merges do not block child implementation; no self-merge.

## Native P1 review fix and fresh release validation (#245)

Reviewer4240135045 correctly found that page-rounding `len` introduced an
out-of-bounds source read in copy_nonoverlapping. Fixed be0e810: copy exactly
e.bytes.len(); mapping/protection/unmapping retain rounded length. Fix is
merged into constructor and comparator child branches. Earlier native trial
measurements above are pre-fix exploratory evidence, superseded by this run.

AddressSanitizer on fresh, isolated source/core builds: original e513822 fails
with READ4096 heap-buffer-overflow; fixed source1b6c124 (tree identical to
be0e810) passes the existing native execution/guard-replay unit. First cross-
worktree attempt reused a fixed artifact and is retained as stale-target,
excluded from evidence; forced package clean before actual before/after runs.
Leak detection disabled: this checks the invalid read, not all project leaks
or uninstrumented JIT instructions. Raw logs/exits/clean commands retained.

Fresh be0e810 release workspace8core22runtime19format,fm/clippy pass;25script
plus19workload byte/exit/error PHP-n gates,VM-profile and3406PHPT rerun retain
2727pass603fail25skip38unsupported6crash7timeout. All3406 individual statuses match the parent receiver map; earlier known
range resource crash/timeout transitions remain in historical raw maps.

Seven rotating PHP/parent/fixed medians, profiler-off isolated:

| Workload | PHP/parent/fixed ms | Fixed/parent | Fixed/PHP |
|---|---:|---:|---:|
| startup | 3.660/6.176/6.082 | 0.985 | 1.662 |
| fib26 | 10.901/132.499/8.300 | 0.063 | **0.761** |
| sieve | 7.251/210.098/213.868 | 1.018 | 29.496 |
| strings | 15.507/187.282/174.966 | 0.934 | 11.283 |
| arrays | 17.694/174.846/169.521 | 0.970 | 9.581 |
| objects | 8.559/241.990/251.167 | 1.038 | 29.344 |
| regex | 6.624/42.179/39.944 | 0.947 | 6.030 |
| JSON | 158.365/519.994/527.113 | 1.014 | 3.328 |
| DB | 28.562/112.089/111.586 | 0.996 | 3.907 |
| app100,20 | 4.850/35.502/35.766 | 1.007 | 7.374 |
| Composer | 5.309/8.159/8.364 | 1.025 | 1.576 |

Nine-bench native-host geo8.183→5.946×PHP,time step0.727(−27.3%).
Fib−93.7% vsparent and~24% faster than native PHP. Sieve+1.8%,objects+3.8%,
JSON+1.4%,app+0.7%,Composer+2.5% retained; other non-target changes are not
broad wins. Full suite still far from parity, user's2.71× remains separate.
Exact fixed hash/config/raw samples and P1 before/after sources are in
native-fixed-*; no pre-fix dataset relabelled as fixed.

## #243: borrow pinned constructor parameter metadata (2026-10-11)

The resolved Rc<Method> keeps its immutable declaration alive during shared
argument binding. Borrow its parameter slice instead of cloning the complete
Vec<Param> for every construction; inherited-owner diagnostics and the same
named/by-reference/default/promoted/type binder remain in place. Runtime ac3d7dd
(6 additions/4 deletions in expr.rs); fresh release inherits P1 fix be0e810.
Existing promoted-slot binding test covers the shared constructor path.

Seven isolated profiler-off alternating fixed-parent/candidate PHP byte gates:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/40-objects.php | 249.202/245.316 | 0.984 |
| bench/40-objects.php 20 5000 | 980.168/989.296 | 1.009 |
| bench/10-fib.php | 8.809/8.520 | 0.967 |
| bench/30-arrays.php | 174.942/173.068 | 0.989 |
| bench/app/cli.php 100 20 | 36.882/36.158 | 0.980 |
| examples/composer/run.php | 8.719/8.232 | 0.944 |

Objects31 follow-up248.653→242.777ms (−2.4%); long20×5000
988.904→983.079ms (−0.6%, effectively flat). The preliminary pre-copy-fix
−6.7%/−5.2% object medians are retained as exploratory evidence, **not current
speed claims**. This change reduces allocation cost; it does not close the
large object gap or establish broad application/PHP parity.

Separate instrumented100000-construction probe: physical requests13,463,260
→12,763,259 (−700,001/~5.2%), requested bytes388,949,018→320,686,734
(−17.6%); obj_new100000 and cell994898 events unchanged. These are allocation
requests/bytes, not CPU time; neither timings nor native mmap bytes are counted
as malloc measurements. Both probe outputs match PHP. Fresh gates below.

Workspace/fmt/clippy and fresh25script+19workload byte/exit/error gates,
VM-profile pass. Full3406PHPT2727pass603fail25skip38unsupported6crash7timeout;
individual status changes vs fixed native parent: {"/workspace/phpun/vendor/php-tests/ext/standard/tests/array/range/range_bug71132.phpt": ["crash", "timeout"], "/workspace/phpun/vendor/php-tests/ext/standard/tests/array/range/range_bug71197.phpt": ["timeout", "crash"]}.
Existing failures/resource limits retained. External CI not yet green.

## #244: native pure integer sort callbacks (2026-10-11)

Reuse the complete context-free callback value/type/owner proof, and run
validated pure integer graphs before creating the callback frame. Extend the
existing integer emitter with signed `<=>` (MIN/MAX-safe). This still rejects
captures, bound class/receiver context, bool returns, references, defaults,
effects, introspection, deadlines and profiler runs. Root callback arena span
reserves only existing-page capacity; native failure restores it and replays
the original callback VM. Same scalar return side channels; no callable-owner
pins or new dependency. P1 initialized-byte copy fix is inherited.

Seven alternating profiler-off fixed-constructor-parent/candidate PHP byte gates:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/30-arrays.php | 173.901/89.369 | 0.514 |
| bench/30-arrays.php 10 5000 | 88.093/48.486 | 0.550 |
| bench/profile/arrays.php usort 5000 | 32.082/15.425 | 0.481 |
| bench/10-fib.php | 8.427/8.316 | 0.987 |
| bench/11-sieve.php | 212.243/201.741 | 0.951 |
| bench/40-objects.php | 224.158/223.222 | 0.996 |
| bench/app/cli.php 100 20 | 32.372/32.406 | 1.001 |
| examples/composer/run.php | 7.648/7.652 | 1.000 |

Arrays−48.6%, long−45.0%, usort phase−51.9%; app and Composer flat.
Other row movements are not attributed as wins from this callback-only change.
Separate PHP-byte-gated instrumented default arrays: physical requests
1,106,113→795,841 (−28.1%), requested bytes43,268,089→30,857,804 (−28.7%).
trace_frame350351→40063 are EVENT counts, not physical allocations; executable
mappings are outside malloc counters. Full suite remains far from PHP parity.

Workspace8core/22runtime/19format,fmt/clippy pass. Existing callback runnable
check proves actual native entry, guard replay for typed overflow, no trace/
arena leak and callable Weak owner release.25VM/script+19workload byte/exit/
stderr gates, warm comparator MIN/MAX/equal/overflow/capture/bool PHP gate and
VM-profile pass.3406PHPT2727pass603fail25skip38unsupported6crash7timeout; only
two known pathological range crash/timeout swaps vs parent, no new fail/pass.
External CI is not yet green. Raw/hash/source manifests preserve all samples.

## #247: guarded scalar dimension writes (2026-10-11)

Reuse DimCv for literal null/bool/int assignment to an existing scalar
integer-key element in a unique array. Fresh current-table get_cell and shared
cell_store preserve cell aliases, displaced scalar decrefs and memory charges.
Shared arrays, missing/nonint/uninitialized keys, globals views, typed/owned/
merged/readonly cells, containers and pending dimension errors/handlers retain
full canonical assignment. No offset cache, layout/storage layer or dependency.

The enlarged existing dimension check found a preexisting fallback bug: typed
reference assignment stored coerced int0 but returned raw false. Shared static/
CV/dimension assignment paths now return the value committed to cell_store;
parent mismatch and fixed result verified against PHP. New path rejects these
typed/readonly cells, so the canonical fallback is tested too.

Final51a8401 release; seven isolated profiler-off PHP-byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/11-sieve.php | 205.174/83.200 | 0.406 |
| bench/11-sieve.php 5 100000 | 1087.643/426.338 | 0.392 |
| bench/30-arrays.php | 90.816/92.249 | 1.016 |
| bench/40-objects.php | 251.942/246.727 | 0.979 |
| bench/20-strings.php | 166.045/165.885 | 0.999 |
| bench/10-fib.php | 8.059/8.451 | 1.049 |
| bench/app/cli.php 100 20 | 35.385/35.346 | 0.999 |
| examples/composer/run.php | 8.869/8.180 | 0.922 |

Sieve−59.4%, long−60.8%. Arrays+1.6%,fib+4.9% initial medians retained;
31 follow-up arrays89.678→90.788(+1.2%),fib8.034→7.909(−1.6%).
No speed claim for nontarget movements; preliminary−63% sieve row retained
and superseded by final ownership-guard artifact. Still no suite parity.

Separate instrumented sieve physical requests/bytes (not speed timing):
native-comparator: allocs: 1869802 bytes: 33488028
scalar-dim-writes-fixed: allocs: 132099 bytes: 13214714

Workspace8core/22runtime/19format,fmt/clippy pass; existing opcode/CoW/alias/
typed/readonly/ArrayAccess check,25script+19workload byte/exit/stderr gates,
extended dimension PHP probe andVM-profile pass.3406PHPT2727pass603fail25skip
38unsupported6crash7timeout; only two known range crash/timeout swaps, no
new fail/pass/skip. External CI not yet green. Packed/mixed storage #187 is
still unfinished. Next #249 borrows property declarations during shared stores.

## #249: borrow pinned property declarations during stores (2026-10-11)

Move the existing private/public/inherited declaration resolver into
decl_prop_site, returning immutable Rc<Class>+declaration index. Existing
decl_prop keeps its owned wrapper for other callers. Shared store_prop borrows
the declaration while inspecting set visibility, readonly and type gates;
declaration-existence checks no longer clone metadata. All hooks, owner/ref
checks, conversion, clone/init diagnostics and actual property writes remain
shared. No object slot cache, offset pin or new dependency. Runtime e62b948,
23add/9delete. Existing promoted-slot binding check covers the shared path.

Seven isolated alternating profiler-off PHP byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/40-objects.php | 237.885/228.219 | 0.959 |
| bench/40-objects.php 20 5000 | 990.289/926.580 | 0.936 |
| bench/profile/objects.php ctor 5000 | 48.244/45.097 | 0.935 |
| bench/30-arrays.php | 93.880/91.334 | 0.973 |
| bench/11-sieve.php | 75.530/73.760 | 0.977 |
| bench/10-fib.php | 8.535/7.920 | 0.928 |
| bench/app/cli.php 100 20 | 39.274/35.183 | 0.896 |
| examples/composer/run.php | 8.625/8.937 | 1.036 |

Objects31 follow-up238.160→222.645ms (−6.5%); Composer8.415→7.992
(−5.0%), initial+3.6% negative row retained. No causal speed claim for other
nontarget movements. Full object/suite gap remains far from PHP parity.
Separate100000-object PHP-byte-gated allocation probe: requests12,763,259
→10,963,259 (−1,800,000/~14.1%), bytes320,687,560→302,687,528 (−5.6%);
obj_new100000 and cell994898 events unchanged. Allocation bytes are not CPU
time; event counters do not count all physical allocations.

Workspace8core/22runtime/19format,fm/clippy pass;25script+19workload and warm
comparator/extended dimension byte/exit/error gates,VM-profile pass. All3406
individual PHPT statuses identical to scalar-write parent:2727pass603fail25skip
38unsupported6crash7timeout. External CI not asserted green. Next #251 extends
pure integer native control-flow validation to backward/nested loops.

## #251: validate pure integer backward/nested control flow (2026-10-11)

Replace the existing forward-only depth scan with a worklist over reachable
Ops. Every merge/backedge must agree on stack depth and every destination is
bounds-checked. Reuse the existing integer emitter/jump patching and all purity/
local-init/overflow/depth/current-page/dependency guards. No loop recognizer,
new dependency or bypassed deadline; deadline/profiler runs retain the VM.
Guard failure restores spans and replays the complete pure root. Scalar loops
own no PHP objects/effects; this does not compile arrays/objects/builtins.

Existing native runnable check now proves actual nested-loop execution,
break/continue, overflow/uninit replay exactly once, deadline exclusion and
trace/arena cleanup. Runtime2919eda,36add/19delete.

Seven isolated alternating profiler-off PHP-byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| /workspace/parity92/oct11/native-loops.php 100000 | 35.410/7.060 | 0.199 |
| /workspace/parity92/oct11/native-loops.php 1000000 | 270.325/11.569 | 0.043 |
| bench/10-fib.php | 8.808/8.679 | 0.985 |
| bench/11-sieve.php | 83.384/78.517 | 0.942 |
| bench/30-arrays.php | 91.898/100.500 | 1.094 |
| bench/40-objects.php | 210.830/210.557 | 0.999 |
| bench/20-strings.php | 160.912/157.591 | 0.979 |
| bench/app/cli.php 100 20 | 33.395/32.852 | 0.984 |
| examples/composer/run.php | 7.821/7.966 | 1.019 |

Separate seven rotating native PHP/parent/candidate loop workload:100k
5.013/33.878/7.404ms (candidate1.477×PHP);1M15.730/275.901/11.312ms
(candidate0.719×PHP). Includes parsing/startup/warmup, not isolated kernel
execution; fixture contains generic sum/nested/while loops, no cached result
or algorithm recognizer. These are additional scalar workloads, **not an
original-ninebench geomean or suite parity claim**. Initial arrays+9.4% and
Composer+1.9% negatives retained;31followuparrays95.917→93.887(−2.1%),
Composer7.820→7.650(−2.2%). No nontarget speed attribution.

Workspace8core/22runtime/19format,fm/clippy;25script+19workload and extended
loop/warm-comparator/dimension nativePHP8.5.11 -n byte/exit/stderr gates,
VM-profile pass.3406PHPT2727pass603fail25skip38unsupported6crash7timeout;
individual changes vs parent: {"/workspace/phpun/vendor/php-tests/ext/standard/tests/array/range/range_bug71132.phpt": ["timeout", "crash"], "/workspace/phpun/vendor/php-tests/ext/standard/tests/array/range/range_bug71197.phpt": ["crash", "timeout"]}. External CI not asserted green.
Next #253 replaces scalar first-byte scanning in shared bfind with libc memchr.

## #253: shared byte search through libc memchr (2026-10-11)

Replace scalar first-byte scanning in shared bfind with existing libc memchr,
then verify the complete needle at the candidate offset. C only reads the
bounded nonempty live slice; returned pointers belong to that slice. Preserve
NUL/non-UTF8 bytes, nonoverlap, offsets, conversion/order/error behavior and
the existing O(n*m) worst-case ceiling. No dependency or algorithm recognizer.
Runtime1f48ee6,8add/1delete; one existing string check extended for boundary bytes.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/20-strings.php | 180.480/155.416 | 0.861 |
| bench/20-strings.php 100 | 826.149/714.644 | 0.865 |
| bench/30-arrays.php | 90.280/91.129 | 1.009 |
| bench/11-sieve.php | 78.567/79.386 | 1.010 |
| bench/40-objects.php | 226.341/223.486 | 0.987 |
| bench/10-fib.php | 8.501/8.322 | 0.979 |
| bench/app/cli.php 100 20 | 34.512/35.062 | 1.016 |
| examples/composer/run.php | 8.259/8.069 | 0.977 |

Strings default -13.9%, longer -13.5%; initial app+1.6% negative retained.
No nontarget attribution or whole-suite PHP parity claim. Physical string
allocation requests unchanged2,225,342; bytes67,747,514/67,747,510, counters
unchanged. Separate CPU profiles3149/2704 samples, zero lost; str_replace
closure self13.24%/0.59% includes more than search. New libc callee unresolved
in report: do not equate closure share with total replacement CPU cost.
vm_exec_ops10.21% includes semantic work, not pure dispatch. Raw perf.data
kept locally; exact symbol reports and raw hashes published. Both sampled
executions stdout byte-identical to PHP.

Workspace8core/22runtime/19format,fm/clippy;25script+19workload and extended
string/loop/comparator/dimension gates,VM-profile pass. All3406 PHPT statuses
identical to parent:2727pass603fail25skip38unsupported6crash7timeout. Existing
shared-string unit passes AddressSanitizer (leak check disabled); not whole
interpreter sanitizer coverage. External CI not asserted green. Next #255
removes temporary display-binding buffers for positional builtin calls;
#256 keeps strict builtin signature metadata borrowed during type checks.

## #255: positional builtin display args without named-slot scratch (2026-10-11)

With no named arguments, original positional Cells are the exact bound display
args, including extras/variadics. Clone that vector once and keep an empty named
tail. Named slots/holes/defaults/errors/by-ref/coercion/trace/ZPP/frame accounting
remain canonical. Runtimec5bc8d9,3add; existing checks reused. #237 full lazy
trace remains open.

Seven isolated alternating profiler-off PHP byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/20-strings.php | 142.871/137.697 | 0.964 |
| bench/20-strings.php 100 | 665.441/647.624 | 0.973 |
| bench/40-objects.php | 239.285/242.496 | 1.013 |
| bench/40-objects.php 20 5000 | 970.942/887.612 | 0.914 |
| bench/30-arrays.php | 100.225/95.795 | 0.956 |
| bench/11-sieve.php | 80.176/82.989 | 1.035 |
| bench/10-fib.php | 9.195/8.615 | 0.937 |
| bench/app/cli.php 100 20 | 37.173/37.870 | 1.019 |
| examples/composer/run.php | 8.388/8.144 | 0.971 |

31-pair follow-up to inspect target noise and regressions:

- bench/20-strings.php: 144.583/141.860ms (0.981).
- bench/40-objects.php: 215.256/215.925ms (1.003).
- bench/app/cli.php 100 20: 34.058/34.813ms (1.022).

Strings physical requests2,225,333→2,144,433 (−80,900/~3.6%);
bytes67,752,290→65,156,826 (−3.8%). Object requests unchanged
10,963,249/10,963,250: sqrt has no known signature table entry, so this
change does not remove its binding buffer. No targeted object attribution.
App+1.9% initial/+2.2% follow-up negative retained, strings follow-up−1.9%;
not suite parity or a large CPU gain. Next #256 removes per-call signature
String/Vec allocations from shared type checks, then fresh whole-suite comparison.

Workspace8core/22runtime/19format,fm/clippy;25script+19workload+VM-profile pass.
All3406individualPHPTstatuses identical to byte-search parent:2727pass603fail
25skip38unsupported6crash7timeout. External CI not asserted green.

## #256: borrow static builtin type metadata (2026-10-11)

Private strict_sig_view borrows existing literal name/type pairs in hot shared
ZPP checks. Keep public strict_sig returning owned String pairs for existing
reflection/API consumers. Same names/types/order/coercion/errors, no cache or
dependency. Runtime556d7e6,15add/9delete. Existing type/reflection checks reused.

Seven rotating profiler-off native PHP/main/parent/candidate coldCLI runs;
every exit/stdout/stderr byte-gated. NativePHP8.5.11 -n,OPcache module loaded
but CLI disabled,JIT disabled. Main3ea4df5 runtime matches frozenbe0e810
(no crates/Cargo delta). Candidate release explicitly cleaned/rebuilt; an
initial stale-parent artifact was detected before any measurement. Rawsource/
hashes and command published, tracked runtime clean except vendor symlink.

| Workload | PHP/main/parent/candidate ms | Candidate/PHP | Candidate/parent |
|---|---:|---:|---:|
| bench/00-startup.php | 3.495/5.844/5.936/5.788 | 1.656 | 0.975 |
| bench/10-fib.php | 10.394/7.931/8.272/7.768 | 0.747 | 0.939 |
| bench/11-sieve.php | 6.807/210.651/74.693/77.275 | 11.352 | 1.035 |
| bench/20-strings.php | 13.288/158.219/136.570/126.540 | 9.523 | 0.927 |
| bench/30-arrays.php | 16.129/169.037/92.813/89.025 | 5.520 | 0.959 |
| bench/40-objects.php | 7.581/233.069/218.588/212.823 | 28.073 | 0.974 |
| bench/50-regex.php | 7.217/39.974/40.992/39.479 | 5.470 | 0.963 |
| bench/60-json.php | 146.872/510.426/507.993/515.654 | 3.511 | 1.015 |
| bench/70-db.php | 28.018/112.178/115.116/113.392 | 4.047 | 0.985 |
| bench/20-strings.php 100 | 48.321/780.858/656.777/612.489 | 12.675 | 0.933 |
| bench/40-objects.php 20 5000 | 19.230/919.650/884.422/858.588 | 44.649 | 0.971 |
| bench/app/cli.php 100 20 | 4.983/34.323/34.631/33.879 | 6.799 | 0.978 |
| examples/composer/run.php | 4.075/8.131/7.943/7.838 | 1.923 | 0.987 |

Original-ninebench main/candidate geomean **6.0988×/4.8943×PHP**,
candidate/main0.8025,candidate/parent0.9741.
This host/config differs from user's2.71×host; no cross-host ratio aggregation
or product of earlier pair gains. **Still far from suite parity**: objects28.07×,
sieve11.35×,strings9.52×;fib0.747×PHP only. Retain sieve+3.5%/json+1.5% vs
parent and other negatives; no causal nontarget attribution. Stringsdefault
−7.3%,long−6.7% vs immediate parent. Strings physicalrequests2,144,443
→1,578,563 (−565,880/~26.4%),bytes65,157,144→50,991,224 (−21.7%). Object
requests unchanged10,963,259/10,963,260; no targeted object allocation claim.

Workspace8core/22runtime/19format,fm/clippy;25script+19workload+VM-profile pass.
3406PHPT2727pass603fail25skip38unsupported6crash7timeout; only2knownrange
crash/timeout swaps,no newfail/pass/skip. ExternalCI not assertedgreen.
Next #258 reuses existing primitive callback valueABI for one-value arraymap/
filter; #260 borrows complete positional builtin binding Cells and avoids
temporary trace-name Strings. FullLazyTrace #237 and packedstorage #187 open.

## #258: reuse primitive value callbacks for one array element (2026-10-11)

Generalize the existing fresh-value callback helper to fixed-size argument
arrays, using the same exact context-free primitive closure proof and shared
value ABI. Single-array map/value-filter join sort pairs; no original/reference
array Cell is treated as fresh. Captures/receiver/scope/default/coercion/by-ref/
nonprimitive cases keep original canonical Cell fallback. Native integer
callback supports bounded argument slices/current-page spans; Boolfilter still
VM here. Runtimecbcbe7c,69add/21delete including ONE existing runnable check
extension proving actual one-value native entry/typed overflow/owner release.

Seven alternating isolated profiler-off PHP byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/30-arrays.php | 92.276/89.827 | 0.973 |
| bench/30-arrays.php 10 | 47.739/44.447 | 0.931 |
| bench/profile/arrays.php map 5000 | 13.412/11.666 | 0.870 |
| bench/profile/arrays.php filter 5000 | 13.304/12.489 | 0.939 |
| bench/40-objects.php | 238.606/225.487 | 0.945 |
| bench/20-strings.php | 127.409/125.766 | 0.987 |
| bench/11-sieve.php | 76.609/81.515 | 1.064 |
| bench/10-fib.php | 8.504/8.107 | 0.953 |
| bench/app/cli.php 100 20 | 35.131/35.768 | 1.018 |
| examples/composer/run.php | 8.465/8.082 | 0.955 |

31pairs for target noise and negative workloads:

- bench/30-arrays.php: 93.022/87.711ms (0.943).
- bench/11-sieve.php: 78.283/81.190ms (1.037).
- bench/app/cli.php 100 20: 33.989/34.517ms (1.016).

Defaultarraysinitial−2.7%/follow-up−5.7%;map5000−13.0%,filter5000−6.1%,
CLI includes input preparation. Physicalarrayrequests795,809→655,890
(−139,919/~17.6%),bytes30,856,089→27,496,036 (−10.9%); maprequests−19,923,
filterrequests−15,001. Retain sieve+6.4%initial/+3.7%followup,app+1.8%/
+1.6% negative; no causal nontarget attribution or suite parity claim.
Workspace8core/22runtime/19format,fm/clippy;25script+19workload+extended
callback/comparator/dimension/string/loop nativePHP8.5.11 -n byte gates and
VM-profile pass. All3406individualPHPTstatuses identical to parent2727pass
603fail25skip38unsupported6crash7timeout. Explicit release clean/rebuild.
ExternalCI not assertedgreen. Next #260 borrows fully positional builtin
binding; #262 bypasses engine-family interception for proven ordinary PHP
methods; #263 emits checked remainder and proven bool native roots.

## #260: borrow complete positional builtin binding Cells (2026-10-11)

Existing resolver returns Cow<Cell slice>. Borrow only when all fixed params
are supplied positionally, plus an allowed variadic tail. No missing/default/
hole/name/excess-error remap is needed; preserve Cell identity/coercion/by-ref.
Other packs keep the original owned binder/errors. Builtin trace function name
uses Rc::from(&str), avoiding a temporary String. No new cache/dependency.
Runtimefd92682,58add/5delete including ONE existing runnable check for borrowed/
owned selection, pointer identity, variadic acceptance and missing/excess errors.

Seven alternating isolated profiler-off nativePHP8.5.11 -n byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/20-strings.php | 127.854/119.961 | 0.938 |
| bench/20-strings.php 100 | 626.463/572.071 | 0.913 |
| bench/40-objects.php | 238.148/235.921 | 0.991 |
| bench/30-arrays.php | 88.235/85.992 | 0.975 |
| bench/11-sieve.php | 79.697/75.469 | 0.947 |
| bench/10-fib.php | 8.118/8.281 | 1.020 |
| bench/app/cli.php 100 20 | 35.973/35.983 | 1.000 |
| examples/composer/run.php | 8.261/8.089 | 0.979 |

Stringsdefault−6.2%,long−8.7%. Physicalrequests1,578,563→1,337,503
(−241,060/~15.3%),bytes50,991,188→46,018,264 (−9.8%);objects100krequests
10,963,260→10,863,260 (−100,000/~0.9%),bytes−0.2%. Preserve fib+2.0%
negative, no nontarget speed attribution or new suite parity claim.
Workspace8core/22runtime/19format,fm/clippy;25script+19workload+VM-profile pass;
all3406individualPHPTstatuses identical to one-value callback parent2727pass
603fail25skip38unsupported6crash7timeout. Explicit release clean/rebuild.
ExternalCI not assertedgreen. Next #262 ordinary user-method dispatcher.

## #262: skip engine-family interception for ordinary PHP methods (2026-10-11)

After canonical visibility/private target resolution, a concrete PHP method
on a declared ordinary root PHP class with no parent/interfaces cannot be a
native-family method. Invoke the same shared user method/binder directly.
Internal/inherited/interface/abstract/missing cases keep the old dispatcher.
No shape cache, changed promotion/write/type/frame/trace/GC protocol.
Runtime3a206b5,25add/1delete including ONE existing promoted check extended
for ArrayIterator and Throwable ancestor/override fallback.
Separate objectCPUprofile1965samples/zero lost: method_invoke_pre34.66%
inclusive/invoke_method25.29%inclusive; these overlap, never add them.
Raw perf.data kept locally with published hash/symbol report.

Seven alternating isolated profiler-off nativePHP8.5.11 -n byte-gated pairs:

| Workload | Parent/candidate ms | Candidate/parent |
|---|---:|---:|
| bench/40-objects.php | 216.377/204.803 | 0.947 |
| bench/40-objects.php 20 5000 | 878.917/776.039 | 0.883 |
| bench/profile/objects.php ctor 5000 | 46.372/41.161 | 0.888 |
| bench/profile/objects.php norm 5000 | 64.179/52.910 | 0.824 |
| bench/30-arrays.php | 85.430/87.929 | 1.029 |
| bench/20-strings.php | 122.554/119.472 | 0.975 |
| bench/11-sieve.php | 76.768/74.809 | 0.974 |
| bench/10-fib.php | 8.066/8.038 | 0.997 |
| bench/app/cli.php 100 20 | 34.175/34.130 | 0.999 |
| examples/composer/run.php | 8.458/8.291 | 0.980 |

31pairs for target noise and initial arrays+2.9% negative:

- bench/40-objects.php: 224.207/197.116ms (0.879).
- bench/30-arrays.php: 88.177/85.875ms (0.974).

Objectsdefault31−12.1%,long7−11.7%,ctorphase−11.2%,normphase−17.6%.
Physical100kobjectrequests10,863,260→8,763,260 (−2,100,000/~19.3%),
bytes302,289,539→285,089,519 (−5.7%). Initial arrays+2.9% retained,
31followup−2.6%; no nontarget attribution or object/suite PHP parity claim.
Workspace8core/22runtime/19format,fm/clippy;25script+19workload+extended
promoted/native ancestor PHP byte gates andVM-profile pass. All3406individual
PHPTstatuses identical to parent2727pass603fail25skip38unsupported6crash
7timeout. Explicit release clean/rebuild;externalCI not assertedgreen.
Next #263 checked remainder/proven bool native roots; #264 no-target scalar
GC cells; #266 Weak liveness; #267 shared scalar builtin kernels.

## #263: checked remainder and proven native boolean predicates (2026-10-11)

Stack branch64 after ordinary-method dispatch #269. Extend shared pure Ops
emitter with signed remainder, guarding denominator0/-1 before idiv and
replaying canonical VM. Explicit bool roots require comparison immediately
before Return, reject jumps to Return and every child call; integer graphs
reject bool callees. Existing purity/type/span/init/overflow/deadline/profiler
proofs remain. ONE existing callback check proves actual native bool entry,
MIN/-1 and zero fallback, invalid bool returns and bool-callee rejection.

Seven isolated alternating profiler-off PHP8.5.11 -n byte-gated pairs:
filter20k34.388→27.494ms (−20.0%), arrays−2.5%, usort−4.2%.
Initial sieve+5.3% and fib+3.5% retained;31 followup sieve74.767→75.086
(+0.4%),fib8.137→8.064(−0.9%), filter33.287→28.471(−14.5%).
Physical filter requests275254→255332 (−19922/~7.2%),bytes11968967→11172646
(−6.7%). Arrays requests655877→635955 (−19922/~3.0%). These separate
allocation runs are not timings; trace events do not substitute allocations.

Workspace8core/22runtime/19format,fm/clippy,25script+19workload+extended
modulo byte gate and VM-profile pass. All3406individual PHPT statuses have
only known range71132/71197 crash↔timeout swaps:2727pass603fail25skip
38unsupported6crash7timeout; no new fail/pass/skip. Explicit release core
clean/rebuild and binary hash recorded. External CI not asserted green.
Raw/source/hashes in bench/data/92/parity/oct11/native-predicates-*.
No object/suite parity claim. Next #264 scalar GC cells, #266 Weak liveness,
#267 shared exact scalar builtin calls, keeping GC/memory/error fallback.

## #264: perf(gc): omit scalar cells from cycle graph bookkeeping (2026-10-11)

The shared GC cell scanner now proves gc_val_ptr==0 before inserting cell-slot/count/pin metadata. Scalar cells cannot point to a cycle graph node; arrays/objects/closures retain the same accounting. Depth-zero short circuit stays before RefCell borrow.

Collection thresholds, candidates, roots, edges, external holders, typed pins, destructor order and every cycle-target cell remain canonical. Existing ONE frame-owner check extended proves no-target scalar exclusion and repeated array-cell accounting.

Runtime dce4f08f3a5821c31aa7f632fd0805c47553950f; measured source 1eb1af2a5f6fe0df006bce0ba676a9d1f1f15ac4.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 194.929 | 192.833 | 0.9892 |
| bench/40-objects.php 20 5000 | 781.967 | 782.040 | 1.0001 |
| bench/60-json.php | 506.135 | 508.687 | 1.0050 |
| bench/30-arrays.php | 80.167 | 81.113 | 1.0118 |
| bench/20-strings.php | 117.684 | 115.289 | 0.9797 |
| bench/11-sieve.php | 76.322 | 75.508 | 0.9893 |
| bench/10-fib.php | 7.923 | 7.935 | 1.0015 |
| bench/app/cli.php 100 20 | 34.152 | 33.136 | 0.9703 |
| examples/composer/run.php | 7.867 | 7.418 | 0.9429 |

Objects−1.1%/long flat and JSON+0.5% do not establish a speed gain. Retain arrays+1.2% and all other negative medians. The measured benefit is reduced GC scratch allocation bytes (~10MB/~3.5% on100kobjects), not suite performance.

Separate physical allocator requests/bytes (not speed runs):

- objects: requests 8763259→8763025; bytes 285087536→275058000.
- json: requests 4540348→4540348; bytes 315892020→315892012.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing runnable checks reused; no redundant test fixtures.
All3406individualPHPT compared; 0 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/gc-scalar-cells-*.

## #266: perf(handles): test weak liveness without temporary strong owners (2026-10-11)

ObjHandle::alive uses Weak::strong_count()!=0 instead of upgrade().is_some() on the existing single-threaded Rc handles. Avoid creating/dropping a temporary strong owner for each liveness test.

Preserve the256-slot dead tail scan, dead-slot LIFO, silent-death handling, PHP-visible handle reuse and cleanup. No new cache/alive bits or deferred scan. Existing owner/reuse/weak lifetime runnable checks retained.

Runtime 889f89faa05dc253f72ee7e23e4914805094e24f; measured source a7da86f9e2b359f933c0cd1801de5830b7e9be0f.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 201.174 | 207.766 | 1.0328 |
| bench/40-objects.php 20 5000 | 776.502 | 792.623 | 1.0208 |
| bench/profile/objects.php ctor 5000 | 39.258 | 39.150 | 0.9972 |
| bench/profile/arrays.php map 20000 | 32.044 | 30.580 | 0.9543 |
| bench/30-arrays.php | 91.447 | 83.726 | 0.9156 |
| bench/20-strings.php | 127.948 | 120.110 | 0.9387 |
| bench/11-sieve.php | 79.284 | 77.075 | 0.9721 |
| bench/10-fib.php | 8.689 | 8.479 | 0.9759 |
| bench/app/cli.php 100 20 | 33.986 | 33.195 | 0.9767 |
| examples/composer/run.php | 8.580 | 7.666 | 0.8934 |

Seven target objects+3.3%/long+2.1% retained;31 followup objects202.887→199.631ms(−1.6%),long796.588→794.683ms(−0.2%). Small/flat target change, not a broad speed claim. Nontarget arrays/strings differences are not attributed to this two-line handle change. Physical requests unchanged.

Separate physical allocator requests/bytes (not speed runs):

- objects: requests 8763025→8763025; bytes 275057282→275057274.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Full concurrent PHPT has bug63635 pass→timeout10s; both binaries rechecked serialized at10s: parent pass4104ms,candidate pass4130ms. Original raw map and two isolated reports retained separately; release build competition removed for rechecks.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/weak-liveness-*.

## #267: perf(vm): call exact scalar sqrt and substr kernels from values (2026-10-11)

For already resolved builtin targets with exact Float/Int sqrt or Str/Int substr arguments, reuse the original shared math/byte kernels directly from the VM value stack. Preserve canonical arena reservation/free; avoid argument Cells/transient builtin trace/ZPP/family scans for these effect-free kernels only.

Page growth, memory-exceeded, profilers, pending conversion/probe/compare diagnostics, named/ref/coercion/type/arity/user override retain canonical path. Missing/null length stays canonical; pre-existing explicit null length bug is recorded with parent/candidate matching and scheduled shared parser fix #278.

Runtime 85144a9071432a89c80feae86b3baad964dadc36; measured source 569248f2e26ea0972498cd19e9955731b6a1d981.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 194.512 | 182.742 | 0.9395 |
| bench/40-objects.php 20 5000 | 793.817 | 727.115 | 0.9160 |
| bench/profile/objects.php norm 5000 | 49.601 | 46.014 | 0.9277 |
| bench/20-strings.php | 119.200 | 65.649 | 0.5508 |
| bench/20-strings.php 100 | 574.443 | 308.738 | 0.5375 |
| bench/30-arrays.php | 81.697 | 80.020 | 0.9795 |
| bench/11-sieve.php | 79.467 | 77.921 | 0.9805 |
| bench/10-fib.php | 8.658 | 9.070 | 1.0475 |
| bench/app/cli.php 100 20 | 36.217 | 34.345 | 0.9483 |
| examples/composer/run.php | 7.596 | 7.590 | 0.9993 |

Seven pairs strings−44.9%,long−46.3%,objects−6.1%/long−8.4%,norm−7.2%. Retain nontarget fib+4.8%, not attributed or erased. Native-host parent comparisons, not user2.71× dataset or current suite parity.

Separate physical allocator requests/bytes (not speed runs):

- objects: requests 8763026→8369363; bytes 275059963→264885143.
- strings: requests 1337503→617503; bytes 46016453→29056481.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing VM runnable check proves actual kernel entry using empty trace-pool and balanced arena span; array-argument error and page-growth create canonical trace frames. Extra boundary byte gate passes for binary/Int-extremes/NaN/-0/named/namespace/type/arity/coercion. Full NULL-length probe matches parent but mismatches PHP (pre-existing parser bug); kept separately and queued #278.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/scalar-builtin-calls-*.

## #271: bench(objects): reject promotion prototypes outside the VM hot path (2026-10-11)

Prototype branch68 replaced effective promoted PropDecl cloning with the existing borrowed declaration resolver. Branch69 tried borrowed declared promotion storage keys. Both change canonical promoted_prop_write, while VM constructor promotion calls shared store_prop instead.

Reject both runtime prototypes via ordinary reverts. Final branch68 crates/Cargo runtime diff versus #279 is empty. Branch69 retains only a useful existing private-collision check; its storage-key prototype was rejected before speed testing. No bypass of canonical type/hook/readonly/ref/destructor protocols.

Runtime df90ccf7a77ab9e25f598991c128da8e388bf388; measured source eb879fadd6570026ce1a4f6be7606652e57dd1ef.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 211.082 | 213.162 | 1.0099 |
| bench/40-objects.php 20 5000 | 786.589 | 820.296 | 1.0429 |
| bench/profile/objects.php ctor 5000 | 46.036 | 41.336 | 0.8979 |
| bench/profile/objects.php norm 5000 | 49.005 | 47.278 | 0.9648 |
| bench/20-strings.php | 64.193 | 66.737 | 1.0396 |
| bench/30-arrays.php | 82.073 | 81.113 | 0.9883 |
| bench/11-sieve.php | 82.373 | 81.736 | 0.9923 |
| bench/10-fib.php | 8.384 | 8.401 | 1.0020 |
| bench/app/cli.php 100 20 | 35.009 | 35.395 | 1.0110 |
| examples/composer/run.php | 8.281 | 7.815 | 0.9437 |

These are prototype timings, not final-head performance. Physical VM objects requests unchanged8369362→8369363 (startup1 request variation), bytes increased3.18MB in separate run; no measured allocation benefit.31pairs objects+0.7%,long+0.7%,strings+4.6% retained. Initial ctor−10.2% cannot be attributed to code outside the actual VM lane. Runtime revert restored exact #279 source. Do not multiply or advertise these medians as gains.

Separate physical allocator requests/bytes (not speed runs):

- objects: requests 8369362→8369363; bytes 261705889→264884393.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Prototype workspace/fmt/clippy,25script+19workload+existing promoted/native ancestor byte gate andVM-profile pass;3406only range71197crash→timeout. Final runtime source is exactly restored to #279; no post-revert speed claim.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/promotion-metadata-*.

## #276: perf(builtins): route known names to the shared extension dispatcher (2026-10-11)

For a bounded known list of string/math/JSON/regex names, select the existing original extension dispatcher directly. All other names retain the FAMILIES scan. Same kernel implementation and shared deferred cast-error handling, no duplicate algorithms.

Canonical builtin binder/frame/ZPP/by-ref/diagnostics remain; namespaced/user resolution happens before this builtin root. No argument ABI or memory/GC changes. Existing shared string and private collision runnable checks reused.

Runtime 33876df32ef544eb799b126d0e567793e0a1de3e; measured source 651422f61514b552a6198428d5ff51a03f988464.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/20-strings.php | 68.206 | 65.511 | 0.9605 |
| bench/20-strings.php 100 | 317.212 | 304.176 | 0.9589 |
| bench/40-objects.php | 189.252 | 189.041 | 0.9989 |
| bench/60-json.php | 515.382 | 519.004 | 1.0070 |
| bench/50-regex.php | 46.252 | 41.071 | 0.8880 |
| bench/30-arrays.php | 80.475 | 78.783 | 0.9790 |
| bench/11-sieve.php | 75.284 | 75.780 | 1.0066 |
| bench/10-fib.php | 8.053 | 8.090 | 1.0046 |
| bench/app/cli.php 100 20 | 33.475 | 33.513 | 1.0011 |
| examples/composer/run.php | 8.580 | 8.070 | 0.9406 |

Seven profiler-off pairs strings−4.0%,long−4.1%,regex−11.2%,objects flat. JSON+0.7%,sieve+0.7%,fib+0.5%,app+0.1% retained. Regex7pairs is a bounded median, not stable broad parity. Physicalrequests/bytes unchanged aside from2 startup requests/18B variation.

Separate physical allocator requests/bytes (not speed runs):

- strings: requests 617503→617501; bytes 29057534→29057516.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing runnable checks reused; no redundant test fixtures.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/builtin-family-dispatch-*.

## #278: fix(strings): treat explicit null substring length as omitted (2026-10-11)

The shared canonical substr parser accepted ?int but converted explicit null to0. Preserve None for omitted/null length; explicit0 remains Some(0). Integer fast kernel stays unchanged and null still takes canonical builtin binding/frame.

Keep named/weak coercion/type/arity/namespace/ref behavior and the shared byte substring kernel. ONE existing shared string runnable check extended with binary omitted/null/zero distinction.

Runtime 2bc9b379eb8d0fde6b76a873dc791cd666df0bea; measured source 372b930e62565ebebd3cb22183d0461a5f483eb9.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/20-strings.php | 65.704 | 64.450 | 0.9809 |
| bench/40-objects.php | 186.094 | 185.428 | 0.9964 |
| bench/app/cli.php 100 20 | 33.616 | 33.303 | 0.9907 |

Correctness fix, not an optimization claim. Three seven-pair smoke medians retained without attributing speed: strings−1.9%,objects−0.4%,app−0.9%. Full prior failing null-length probe now matches PHP byte-for-byte, including NUL/nonUTF8,Int MIN/MAX offsets/lengths,named/coercion/error/namespace cases.

Separate physical allocator requests/bytes (not speed runs):


Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Full scalar-builtin-calls-null-length-baseline probe now passes PHP byte gate, including nullable length. Binary omitted/null/zero distinction in ONE existing string check. No new fail/pass/skip in3406PHPT;onlyknownrange71132timeout→crash.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/null-substr-length-*.

### #284 review: nullable length root and named replacement offset

Runtime a2dfbd2cf3292acdc39c5de03c230014efc25e19. Shared nullable_int_arg handles omitted/null versus explicit0 for substr,substr_replace,mb_substr,mb_strcut,strspn,strcspn. Correct substr_replace named parameter from start to PHP offset. Existing shared string runnable check covers all siblings, multibyte/binary omitted/null/0 and named replacement. NativePHP8.5.11 -n byte probe passes for core substr/replacement/span cases; native oracle lacks mbstring, so no native mb oracle claim. Workspace/fmt/clippy and explicit release clean/rebuild pass;25+19+VM-profile gates pass. All3406individualPHPT statuses identical original #284, existing2727pass603fail retained. Correctness-only inherited review fix, not a new timing claim. Binary/source/raw verification under nullable-length-review-*; original pre-review timing artifacts remain separate.

#280 inherited #284 review verification: exact reviewed runtime e6ab84b7e4c0c88b0a9219f67e2ebed12e59482e; clean workspace/fmt/clippy/release and25+19+VM-profile/core nullable/property byte probes pass. All3406individualPHPT statuses identical reviewed parent. New binary/hash and raw under primitive-property-reviewed-*. Original primitive-property-checks-* timings/alloc/profile were taken before sibling-null review fixes and remain labeled separately; no invented repeat speedup.

## #283: perf(builtins): share guarded scalar string kernels across VM and Cell entry (2026-10-11)

Reuse the original replacement and ASCII case kernels for exact primitive Value-stack calls and canonical Cell-root calls. str_replace_one accepts existing owned vectors and borrowed byte slices through AsRef; unchanged subjects retain their Rc. Scalar Cell-root entry avoids transient trace/binding work; VM entry also avoids arg Cells.

Named/count-ref/array/coercion/type/arity/override/pending diagnostic/error/OOM/profiler/coverage cases retain canonical binding. Arena reserve/free remains canonical; any VmSite owning a grown page rejects the root fast path. ONE existing callback/builtin check proves both entries and grown-page fallback. No duplicated string algorithm.

Runtime e8fea20ada7f7df4c5469942329a829602525335; timing source e8fea20ada7f7df4c5469942329a829602525335; report/integration source 154ac34268e9669625e63a37d2356579ac846b7a.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/20-strings.php | 64.293 | 68.081 | 1.0589 |
| bench/20-strings.php 100 | 317.747 | 299.465 | 0.9425 |
| bench/40-objects.php | 184.189 | 184.638 | 1.0024 |
| bench/11-sieve.php | 79.490 | 78.709 | 0.9902 |
| bench/10-fib.php | 8.916 | 8.631 | 0.9681 |
| bench/30-arrays.php | 85.537 | 83.469 | 0.9758 |
| bench/60-json.php | 539.376 | 558.722 | 1.0359 |
| bench/app/cli.php 100 20 | 36.161 | 37.721 | 1.0432 |
| examples/composer/run.php | 7.872 | 8.571 | 1.0888 |

Initial VM-only experiment left original string allocations unchanged and produced mixed medians; retained separately. Final shared-root7pairs strings+5.9%,long−5.8%,JSON+3.6%,app+4.3%,Composer+8.9% retained.31pairs addressing those negatives: strings−4.5%,long flat(+0.1%),JSON−1.3%,app−1.9%,Composer−1.5%. Separate7pairs replacement20000−4.0%,slice20000−9.0%. Physicalstrings−11200 requests(−1.8%),bytes−0.6%. Bounded gains/no suite parity claim.

Separate physical allocator requests/bytes (not speed runs):

- strings: requests 617501→606301; bytes 29056128→28890096.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Binary/NUL/nonUTF8,ASCII/CI,unchanged Rc identity,count-ref,arrays/coercion/errors/native probes pass. Existing check caught grown-page root bypass; guard corrected before final measurement.3406individualPHPT only2known range crash/timeout swaps. Ordinary main38 merge has no runtime delta.
All3406individualPHPT compared; 2 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/scalar-string-calls-*.

## #285: perf(gc): skip no-target scalar decrefs and scalar-array dying maps (2026-10-11)

At shared destruct_dying_value, scalar decrefs return before empty object tally setup. At gc_note_dying, LAST-real-owner arrays whose current elements are all Null/Bool/Int/Float/Str return before dying-cell map/walk/release. Shared-array purple buffering stays first.

Mixed/nested/object/closure/resource tables keep the full snapshot and callback order. No individual filtering inside mixed tables, no cache/threshold/GC disable/deferred drop. ONE existing GC runnable fixture extends scalar aliases and mixed-cycle collection; native/parent outputs identical.

Runtime b5ac32f926801d690aa396ebb5d982829133edac; timing source 8bcecaa3a38fd7fcb99338a65c3cb398f1331b93; report/integration source 8bcecaa3a38fd7fcb99338a65c3cb398f1331b93.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/11-sieve.php | 77.516 | 74.690 | 0.9635 |
| bench/11-sieve.php 10 100000 | 858.516 | 717.455 | 0.8357 |
| bench/30-arrays.php | 82.896 | 80.213 | 0.9676 |
| bench/60-json.php | 536.583 | 521.845 | 0.9725 |
| bench/40-objects.php | 175.735 | 175.437 | 0.9983 |
| bench/20-strings.php | 68.214 | 65.354 | 0.9581 |
| bench/10-fib.php | 8.703 | 9.622 | 1.1056 |
| bench/app/cli.php 100 20 | 33.276 | 34.022 | 1.0224 |
| examples/composer/run.php | 8.927 | 8.010 | 0.8973 |

Seven pairs Sieve10×100000−16.4%,default−3.6%,arrays−3.2%,JSON−2.7%,objects flat. Initialfib+10.6%,app+2.2% retained;31follow-upfib+3.9%,app flat. PhysicalSieve10×1000001032554→1032393 requests;bytes105819177→78032005 (−26.3%). Target GC CPU/maps, not a broad whole-suite or fib claim.

Separate physical allocator requests/bytes (not speed runs):

- sieve: requests 1032554→1032393; bytes 105819177→78032005.
- arrays: requests 635955→635955; bytes 26701670→26701678.
- json: requests 4540348→4540348; bytes 315894275→315894283.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing GC fixture scalar aliases/mixed-cycle probe matches native PHP and parent. Workspace/fmt/clippy,25+19+VM-profile/core nullable probe pass;3406individualPHPT only2knownrange swaps. Full compatibility remains81.57%, not asserted green.
All3406individualPHPT compared; 2 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/scalar-array-teardown-*.

## #286: perf(memory): reuse parsed limits and retain reference constructor binding (2026-10-11)

Reuse the existing raw-string-validated allocator memory-limit cache at statement boundaries and mutable resize checks. Keep every raw lookup/check/charge and INI invalidation. Fresh main integration also fixes #292: InitNew routes any by-ref ctor to canonical Expr::New before evaluating argv, retaining caller Cells.

No skipped limit check, delayed INI change or changed OOM site/arena accounting. Missing raw limit remains -1. Existing arena check enforces a changed limit at the next boundary. Constructor fallback preserves original alias/type/error/autoload/visibility binding; existing vm_canonical_lifetimes fixture extends compiled-loop promoted refs.

Runtime 81c0d5f7724960515402c66bb62e3a911000f472; timing source 8b1c8210830f08f7c4160b7f6a7da0c2463f5aed; report/integration source df67afdc76a19f0983c15956b37ebded3a7f6a37.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/11-sieve.php | 76.372 | 71.357 | 0.9343 |
| bench/11-sieve.php 10 100000 | 702.871 | 656.769 | 0.9344 |
| bench/30-arrays.php | 84.428 | 84.129 | 0.9965 |
| bench/60-json.php | 563.181 | 514.408 | 0.9134 |
| bench/40-objects.php | 195.002 | 201.634 | 1.0340 |
| bench/20-strings.php | 66.474 | 67.562 | 1.0164 |
| bench/10-fib.php | 9.249 | 8.704 | 0.9411 |
| bench/app/cli.php 100 20 | 35.953 | 35.258 | 0.9807 |
| examples/composer/run.php | 8.409 | 8.622 | 1.0253 |

Pre-main026 seven pairs sieve/default and10reps−6.6%,JSON−8.7%,arrays flat,objects+3.4%,strings+1.6%,Composer+2.5% retained. Original timing binary precedes main026 object lane and #292 correctness fix; final integrated head is separately rebuilt/gated, no old timing attributed to it. No whole-suite parity claim.

Separate physical allocator requests/bytes (not speed runs):


Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Original cache-only3406PHPT statuses identical parent. Final main026/#292 integration verification is appended separately with exact binary/source and intentional imported fail→pass cases; no claim that pre-integration raw belongs to final head.
All3406individualPHPT compared; 0 known range crash/timeout swaps,
no new fail/pass/skip. Existing2727pass603fail25skip38unsupported retained;
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/memory-limit-boundaries-*.

### #286/#292 integration on main026f3e2

Integrated runtime df67afdc76a19f0983c15956b37ebded3a7f6a37. Explicit release clean/rebuild; workspace/fmt/clippy,25script+19workload native byte gates,VM-profile pass. Reference constructor probe: native/fixed2,2,9; original merged #2891,2,1. Fix BEFORE argument evaluation retains canonical caller Cells for by-ref ctor params. Existing vm_canonical_lifetimes check extended, no new fixture.3406PHPT vs pre-main026 candidate:2729pass601fail25skip38unsupported6crash7timeout; only exception_005 and traits/error_007 improve fail→pass from imported main integration, no new failures. Do not attribute these improvements or old timings to the limit cache. Integrated source/hash/gates/maps under memory-limit-integrated-*; original timings stay memory-limit-boundaries-* with their own binary.

## #294 review: restore root global lifetime/bindings (2026-10-11)

Root NewCall and direct MethodCall release caller slot handles before dispatch and refresh canonical globals afterward, including errors, just as Op::Call does. The simple unset/value-write probe was already byte-correct; the reference-rebind probe reproduced native9 versus old2 for both dispatches. ONE existing vm_canonical_lifetimes fixture extends both paths; canonical local-frame calls remain unchanged. Reviewer threads PRRT_kwDOU4oR3M6rLzlh / PRRT_kwDOU4oR3M6rLzln.

Runtime a25f25377261affa86d850b67e7929f39543f86c; binarySHA256 faac63b35b157fc6f26ed69bb7f080cd3967fcdc1a393a784d9b48373ed00d03. Workspace/fmt/clippy/clean release/25script+19workload+VM-profile and four extra probes pass. All3406PHPT compared:2729pass601fail, only range_bug71132 crash→timeout (known resource result), no new fail/pass/skip. Prior CI rust/phpt succeeded at539a156; this new head requires its own CI. Prior cache-only/per-stage timings do not time this global synchronization restoration. Fresh paired/whole-suite measurements will use this exact reviewed binary.

#295 independently duplicates the by-ref ctor correction and adds handle backlog stamping; its separate epoch fix is not included here. No external-host figures mixed into this report. Raw reviewed artifacts in bench/data/92/parity/oct11/memory-limit-review-*.

## #294 closed-parent integration onto main0ddef51 (2026-10-11)

GitHub merged #294 into branch74 after #291 was closed; adc3090/18d70dc were not main ancestors. Ordinary main0ddef51 merge32e8353 retains #295 handle-epoch fix and resolves overlapping ctor-reference guard through the shared existing canonical fallback. Remaining main delta is parsed-limit reuse, root globals synchronization, and existing runnable checks. No external branch rewrite/force or self-merge.

Exact runtime 32e8353c2c22905cff3dad49ecce4211ae0c20c4 / binarySHA256 8361bdb46bf6701ea7fb9e114a749f410a6902813ad20d3d877ac083e2adbae3. Fresh workspace/fmt/clippy/core release clean/rebuild/25script+19workload+VM-profile pass.3406individualPHPT retains2729pass601fail; only knownrange71132 timeout→crash versus reviewed parent. No new fail/pass/skip. Prior per-step figures are historical binary/source pairs, not timings of this integrated source; current-main whole comparison is pending. Raw main integration stored under memory-limit-main-*.

## #293: perf(storage): keep dense scalar fills inline with lazy Cell promotion (2026-10-11)

Store zero-based scalar array_fill values in actual Vec<Value> with implicit integer keys. Scalar reads/writes and CoW avoid per-element Rc/RefCell. Memory count/type/OOM preflight, Zend table charges and mixed/reference/cursor semantics remain unchanged. Compact enum keeps boxed Vec directly; only packed rows allocate lazy promotion state.

Canonical Vec access or exporting an element Cell materializes the complete ordered Cell table once. Nonzero-key or object/array/resource fills remain boxed; sparse/mixed/reference operations normalize. Globals/typed-owner/readonly/handler guards and canonical GC paths remain. Shared purple-array buffering precedes the scalar last-owner shortcut. Deliberate ceiling: producer limited to scalar zero-based fill; iteration generally promotes, mixed inline storage remains open in #187.

Runtime ee63f18aee2425669d55653fa0bbfcf6b7283ab4; timing source ee63f18aee2425669d55653fa0bbfcf6b7283ab4; report/integration source c8966dfff804a01ae0ec7181bd05aaebdc322de6.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/11-sieve.php | 84.583 | 65.177 | 0.7706 |
| bench/11-sieve.php 10 100000 | 652.839 | 564.471 | 0.8646 |
| bench/30-arrays.php | 76.922 | 77.519 | 1.0078 |
| bench/60-json.php | 537.869 | 527.491 | 0.9807 |
| bench/40-objects.php | 161.546 | 156.235 | 0.9671 |
| bench/20-strings.php | 64.448 | 63.903 | 0.9915 |
| bench/10-fib.php | 8.739 | 9.277 | 1.0616 |
| bench/app/cli.php 100 20 | 33.854 | 32.734 | 0.9669 |
| examples/composer/run.php | 8.713 | 8.058 | 0.9249 |

7pairs defaultSieve84.583→65.177ms(−22.9%),10reps652.839→564.471(−13.5%);arraysflat+.8%,JSON−1.9%,objects−3.3%,stringsflat−.8%,app−3.3%. Initialfib+6.2%;31followupfib+3.7%,JSON+2.6% retained. Separate7rotating PHP/main0ddef/parent/final:9bench geomean4.2658→4.0376×PHP (−5.35% vs main,−3.65% vs parent),still not parity;objects18.99×PHP/long33.23×,app6.89×. Wide-state prototype had31arrays+4.3%,JSON+2.7%,strings+2.1%; compact enum replaces that state. All raw/source/hashes retained, no mixing user/Devin host ratios.

Separate physical allocator requests/bytes (not speed runs):

- sieve10: requests 1032477→32477; bytes 78034527→22034515.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing scalar-dimension unit asserts packed writes/CoW and reference normalization; existing lifetime/readonly/canonical fixtures reused. Additional native packed mutator/view probe recorded separately.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 5, 'timed_out': 8, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/compact-packed-*.

### Packed storage versions and current-main whole comparison

Wide-state prototype main31058ed:31pairs arrays+4.3%,strings+2.1%,JSON+2.7%,objects-long+0.1%,app+0.8%. Compact enum ee63f18 keeps boxed Vec directly; promotion state only on packed rows. Three earlier binary/source/maps and all negative rows remain separate. No performance claim transferred across review sync/epoch integrations.

Final compactSHA256 0feb20789ce1b0b97028950ac31e2612431884ebc03c996aa53d1923e8d88786; extra readonly/reference/foreach/shift/unshift/sort/ArrayIterator supported probe byte-pass. Full shift/unshift/splice probe has a PRE-EXISTING extra element on real main, integrated parent, wide and compact alike; preserve source/native/main/candidate outputs in packed-views-comparison.json, do not claim its PHP gate passes.

Fresh clean real main0ddef51 binarya8d427b:3406map equal integrated reference, native ctor-ref pass but two globals-reference probes fail on main and pass in #297. PHP8.5.11-n hash32fa863; OPcache loaded CLI0/JITdisable, no mbstring extension.

Seven rotating PHP/main0ddef/parent/final byte-gated profiler-off runs:9bench main4.265840→final4.037589×PHP, final/main0.946493,final/parent0.963475. FinalPHP ratios startup1.556,fib0.799,sieve8.212,strings4.712,arrays4.476,objects18.991,regex5.607,JSON3.494,DB3.558. Longstrings6.034,longobjects33.234,app6.889,Composer1.757.31followupfib8.502→8.820(+3.7%);JSON516.901→530.245(+2.6%) retained. This host/main0ddef/candidate snapshot is not user2.71× or a later moving main. Full raw compact-packed-four-timings.json; no multiplication of stage gains/parity claim.

## #296: bench(vm): retain the rejected empty-method dispatch experiment (2026-10-11)

Report-only: tested removing body.is_empty from existing vm_method_target, then ordinary-reverted the complete prototype and added unit. Final crates/Cargo runtime diff versus #299 is EMPTY; final executable/behavior is the exact frozen compact-packed parent, not the rejected empty-method-dispatch binary.

Rejected prototype remains in commit6691796 and frozen4b15cda/f3afeac; no retained method/ctor runtime optimization, value ABI extension or claim that #296 is delivered. Existing native/global/arena/readonly/error/reference guard suite remains on parent.

Runtime 6691796308cccc558a22cf0f39314945dd7e8740; timing source 4b15cdaf6fa35026f3a3992a7e8b9202b71caa2f; report/integration source 32d11959721b838ffa87729c513384bfce7c6c6f.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 143.845 | 145.914 | 1.0144 |
| bench/40-objects.php 20 5000 | 603.776 | 599.932 | 0.9936 |
| bench/profile/objects.php ctor 5000 | 33.825 | 34.054 | 1.0068 |
| bench/profile/objects.php norm 5000 | 43.007 | 40.860 | 0.9501 |
| bench/11-sieve.php | 61.880 | 62.753 | 1.0141 |
| bench/30-arrays.php | 79.061 | 76.559 | 0.9684 |
| bench/20-strings.php | 62.776 | 62.807 | 1.0005 |
| bench/60-json.php | 519.746 | 517.052 | 0.9948 |
| bench/10-fib.php | 8.166 | 8.178 | 1.0014 |
| bench/app/cli.php 100 20 | 33.351 | 32.466 | 0.9734 |
| examples/composer/run.php | 7.850 | 7.922 | 1.0092 |

7pairs objects+1.4%,longobjects−.6%,ctor+.7%,norm−5%,sieve+1.4%,arrays−3.2%,stringsflat,JSON−.5%,fibflat,app−2.7%,Composer+.9%. No demonstrated target ctor/object benefit. Physical ctor5000 and actual objects1 requests identical before/after; tiny byte delta is argv path length. ctor diagnostic has switch and uses AST root (pre/cells phase timers both0), so its unchanging event count is not proof of the compiled path; actual objects1 physical is unchanged too. Profiling totals are inclusive/not speed evidence. Reject rather than attribute unrelated norm/array variation to the guard.

Separate physical allocator requests/bytes (not speed runs):

- ctor5000: requests 382646→382646; bytes 18582830→18582858.
- objects1: requests 352870→352870; bytes 21023574→21023602.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Rejected ONE existing receiver unit covered inherited/default/promoted/error/no-op behavior and eligibility; revert restores parent unit. No added framework/fixtures. Existing native main/parent globals probes remain in report.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/empty-method-dispatch-*.

Final #296 decision: revert6691796 through ordinary commit; runtime crates/Cargo diff with #299 is empty (checked). The empty-method-dispatch ELF is the REJECTED prototype, not final PR runtime. Physical/core/native comparisons and inclusive profiles remain separate. Final uses exact compact-packed source/binary0feb207 and verified parent map, including its existing failing/unsupported/resource statuses; no new test/rebuild required for a metadata-only identical runtime. Perf goal #92 continues in #298.

## #298: perf(vm): avoid root bridges for guarded scalar builtin kernels (2026-10-11)

Reuse the existing exact scalar builtin kernel/reservation helper before the top-level slot/global materialize-refresh bridge, only for resolved builtin targets with no exposed $GLOBALS array. Successful kernels retain the original VM frame reserve/free and memory charge. This removes repeated root table synchronization for pure builtin operations; no benchmark-specific behavior.

Existing profilers, diagnostics, OOM/current-page capacity, type/arity/domain guards remain. Misses, reference/callback/user calls and exposed globals use the original canonical path. Direct constructors/methods retain #297 P1 synchronization. ONE existing callback unit covers root/global rebind, fallback and unchanged VM stack occupancy; no new dependencies/frameworks.

Runtime f4313bd93f4cff704ee2707640d4fb83cdcfd38e; timing source f4313bd93f4cff704ee2707640d4fb83cdcfd38e; report/integration source 013641533789e37be78953062592f3006a923109.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/20-strings.php | 66.352 | 56.088 | 0.8453 |
| bench/20-strings.php 100 | 284.057 | 236.721 | 0.8334 |
| bench/profile/strings.php slice 20000 | 68.857 | 70.828 | 1.0286 |
| bench/profile/strings.php replace 20000 | 678.788 | 700.100 | 1.0314 |
| bench/40-objects.php | 157.443 | 155.977 | 0.9907 |
| bench/11-sieve.php | 63.080 | 65.628 | 1.0404 |
| bench/30-arrays.php | 79.745 | 79.806 | 1.0008 |
| bench/60-json.php | 542.739 | 534.280 | 0.9844 |
| bench/10-fib.php | 8.472 | 8.545 | 1.0086 |
| bench/app/cli.php 100 20 | 33.659 | 33.085 | 0.9830 |
| examples/composer/run.php | 8.562 | 7.950 | 0.9285 |

7pairs strings66.352→56.088ms (−15.5%),long284.057→236.721 (−16.7%). Initial sieve+4.0%,slice+2.9%,replace+3.1%;31followup sieve+.4%,slice+1.7%,replace+1.0% retained. Separate7rotating PHP/currentmain3f52972/parent/candidate: ninebench geomean4.03873→4.04012×PHP, effectively flat (+.034%main); duplicate parent ELF produces −1.32%, an order/noise difference, not a second baseline gain. Longstrings−12.6%vs main/−14.0%parent; objectsdefault+3.3%main/long+.15%,JSON+1.1%,DB−.4%,app+.7%. No suite parity or overall speedup claim. Exactmain and parent crates/Cargo are identical to compact-packed binary0feb207, reused after empty runtime diff; candidate explicitly rebuilt from f4313bd. Physical strings100 requests identical2902882; +16bytes from argv path length, no allocator reduction claim.

Separate physical allocator requests/bytes (not speed runs):

- strings100: requests 2902882→2902882; bytes 120174740→120174756.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing callback unit extended once; additional native binary strings/type errors/provided-ref count/global-rebind probe passes. Current-main whole measurement uses verified code-identical compact ELF, not an obsolete main snapshot.
All3406individualPHPT compared; 1 known range crash/timeout swaps,
no new fail/pass/skip. Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/pure-root-builtins-*.

## #301: perf(vm): report rejected optional builtin reference-SEND experiment (2026-10-11)

Experiment limited InitCall builtin reference flags to supplied positional args, retaining supplied references/user declarations/named/unpack. This unlocked existing guarded 3-arg replacement kernels. The prototype was tested and measured, then REJECTED; final source uses an ordinary revert and has no runtime delta from #302.

Do not treat the rejected frozen candidate ELF c02f8b6 as final runtime. Original reference SEND is restored. Full raw negatives, physical counters and native alias/error/named/unpack probes retained. No allocation gain implies a speed gain.

Runtime bc148d8df07176022991a40fb2f5697d2e24b735; timing source ebf62d63a4d59e110fb08ea2d0eae5c69660c379; report/integration source ebf62d63a4d59e110fb08ea2d0eae5c69660c379.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/20-strings.php | 54.599 | 57.701 | 1.0568 |
| bench/20-strings.php 100 | 241.244 | 246.946 | 1.0236 |
| bench/profile/strings.php replace 20000 | 689.913 | 626.080 | 0.9075 |
| bench/profile/strings.php slice 20000 | 68.005 | 71.072 | 1.0451 |
| bench/40-objects.php | 155.317 | 159.929 | 1.0297 |
| bench/11-sieve.php | 65.607 | 64.277 | 0.9797 |
| bench/30-arrays.php | 78.601 | 78.016 | 0.9925 |
| bench/60-json.php | 519.294 | 516.556 | 0.9947 |
| bench/10-fib.php | 8.557 | 9.267 | 1.0830 |
| bench/app/cli.php 100 20 | 34.145 | 33.152 | 0.9709 |
| examples/composer/run.php | 8.832 | 8.541 | 0.9670 |

7pairs strings+5.7%default/+2.4%long,objects+3.0%,fib+8.3%,slice+4.5%,sieve−2.0%,arrays−.7%,JSON−.5%,app−2.9%,Composer−3.3%.31negativefollowupstrings+1.0%default/+2.8%long,objects+.2%,fib+3.6%,slice−2.9%. Target strings still no speed benefit. Physicalstrings100 requests2902882→2850882(−52000),bytes120177245→114093273; tiny argv path delta included. Replace selector has switch/AST root, physicalrequests992181 unchanged; its timing−9.3% is not proof of InitCall optimization. Revert rather than ship a measured slowdown.

Separate physical allocator requests/bytes (not speed runs):

- strings100: requests 2902882→2850882; bytes 120177245→114093273.
- replace20000: requests 992181→992181; bytes 5452238184→5452238212.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing callback unit extended in prototype; native omitted/provided aliases,named/unpack,arity/type errors,regex optional outputs,user default/variadic refs byte-identical. Original unit restored by ordinary revert.
All3406individualPHPT compared; 0 known range crash/timeout swaps,
no new fail/pass/skip. Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/optional-builtin-ref-args-*.

Final #301 decision: ordinary revert restores exact #302 runtime (crates/Cargo diff empty, checked). Frozen optional-builtin-ref-args ELF is rejected; final uses verified parent pure-root-builtins ELF d193a8f and parent status map. No source rebuild needed for unchanged runtime.

## #303: perf(gc): use existing Fx tables for pointer scratch identities (2026-10-11)

Historical pre-#305 candidate: use existing FxMap/FxSet for GcScan and temporary pointer-only visited/reach/dead sets. Persistent purple/dying state, traversal, root counts, callbacks and destructor order unchanged; no new abstraction/dependency. Frozen sourceb0a02f2/binaryfccfcdc and timingdb6adbe. Main#305 was merged afterward; final integration is rebuilt/gated/measured separately, never attribute these old pairs to final current main.

Canonical graph discovery, depth/refcount accounting, typed pins, duplicate Cell identities, generator/closure/internal storage and buffer-overflow paths all retained. ONE existing registered/plain-cell fixture extended for live typed root,40deadtyped pairs and callable cycle; existing scanner check adopts visited-set type.

Runtime b0a02f2105ef3591a33db1cfc00bb61bb5ae5eab; timing source db6adbea7cafa41ae6cd6cd4c5312245b2172349; report/integration source c44f9ea9e26b13bd2888cf57147c0eaa7fc6e69e.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 149.908 | 148.603 | 0.9913 |
| bench/40-objects.php 20 5000 | 606.164 | 608.406 | 1.0037 |
| bench/20-strings.php | 56.942 | 59.400 | 1.0432 |
| bench/11-sieve.php | 66.147 | 65.318 | 0.9875 |
| bench/30-arrays.php | 79.073 | 85.124 | 1.0765 |
| bench/60-json.php | 536.640 | 537.399 | 1.0014 |
| bench/10-fib.php | 8.528 | 8.603 | 1.0088 |
| bench/50-regex.php | 41.480 | 41.478 | 0.9999 |
| bench/70-db.php | 107.104 | 109.686 | 1.0241 |
| bench/app/cli.php 100 20 | 35.225 | 34.439 | 0.9777 |
| examples/composer/run.php | 9.178 | 8.475 | 0.9234 |

7pairs objects149.908→148.603ms(−.9%),long606.164→608.406(+.4%),strings+4.3%,arrays+7.7%,JSON+.1%,fib+.9%,regexflat,DB+2.4%,app−2.2%,Composer−7.7%.31followup arrays−1.3%,strings+6.3%,longobjects−1.2%,DB−.9%; strings regression retained, no normal-objects or overall speedup claim. Independent7pairs extracted200000-node cyclic linked-list PHPT workload13274.766→9281.903ms(−30.1%), everyexit/stdout/stderr==nativePHP8.5.11-n. Fullsuite GC testparenttimeout10s/candidatepass9085ms;serializedparenttimeout10007ms/candidatepass9508ms confirms timeout boundary. This is performance admission at10s,not a repaired semantic failure. Physicalobjects5 requests1663202 identical;gcgraph35122 identical. Hashing-work gain,not allocator-elision.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1663202→1663202; bytes 69796944→69796940.
- gcgraph: requests 35122→35122; bytes 6196215→6191987.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing fixture nativePHP parent-output verified;typed/shared/callable cycles pass. Serialized GC threshold rechecks and whole3406raw status maps retained.
All3406PHPT:bug77345_gc_1 timeout→pass (serialized10s parenttimeout/candidatepass confirmed),plus2knownrangecrash/timeout swaps;no new failures.2730pass/601fail;existing compatibility gaps remain.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/gc-pointer-tables-*.

## #303: perf(gc): use existing Fx tables for pointer scratch identities (2026-10-11)

Use existing FxMap/FxSet for GcScan and temporary pointer-only visited/reach/dead sets. Keep persistent purple/dying state and all graph,root,refcount,typed/generator/destructor logic. Ordinary-merge current main7b74c2e (#305); explicit fresh frozen runtimec44f9ea/binary33f740f, not pre305 prototype. Also roll up rejected #301 report data from closed-parent #304; that experiment has no runtime delta.

Canonical graph discovery, depth/refcount accounting, typed pins, duplicate Cell identities, generator/closure/internal storage and buffer-overflow paths all retained. ONE existing registered/plain-cell fixture extended for live typed root,40deadtyped pairs and callable cycle; existing scanner check adopts visited-set type.

Runtime c44f9ea9e26b13bd2888cf57147c0eaa7fc6e69e; timing source e4df1d9e5fbf13f6de59afb3e00bfb123ad3b7ba; report/integration source e4df1d9e5fbf13f6de59afb3e00bfb123ad3b7ba.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 153.195 | 152.015 | 0.9923 |
| bench/40-objects.php 20 5000 | 592.722 | 605.682 | 1.0219 |
| bench/20-strings.php | 54.891 | 58.148 | 1.0593 |
| bench/20-strings.php 100 | 230.609 | 236.439 | 1.0253 |
| bench/11-sieve.php | 62.739 | 63.871 | 1.0180 |
| bench/30-arrays.php | 80.963 | 77.623 | 0.9587 |
| bench/60-json.php | 530.877 | 536.562 | 1.0107 |
| bench/10-fib.php | 9.374 | 8.233 | 0.8783 |
| bench/50-regex.php | 39.409 | 38.975 | 0.9890 |
| bench/70-db.php | 107.090 | 105.510 | 0.9852 |
| bench/app/cli.php 100 20 | 32.793 | 32.674 | 0.9964 |
| examples/composer/run.php | 7.885 | 7.796 | 0.9887 |

Currentmain7b74 paired7 profiler-off,byte-gated:objects153.195→152.015ms(−.8%),long592.722→605.682(+2.2%),strings54.891→58.148(+5.9%),long230.609→236.439(+2.5%),sieve+1.8%,arrays−4.1%,JSON+1.1%,fib−12.2%,regex−1.1%,DB−1.5%,app−.4%,Composer−1.1%. These include negatives/no broad speedup claim; fib is not attributed to GC hashing. Separate7pairs 200000-node cycle workload12742.765→8936.068ms(−29.9%),everyrep nativePHP-byte-gated. Full3406parent2729pass/candidate2730pass601fail,onlyGC77345timeout→pass; range maps identical in integration. Physicalobjects5 requests1663202 identical;gcgraph35122 identical; byte differences recorded not allocator-elision. Pre305 full raw31strings+6.3%,arrays−1.3%,longobjects−1.2% retained above; pre305serializedGC parenttimeout10007ms/candidatepass9508ms retained separately. PHP parity remains unmet; no multiplying step gains or mixing host ratios.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1663202→1663202; bytes 69796932→69796960.
- gcgraph: requests 35122→35122; bytes 6196203→6192007.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing fixture nativePHP parent-output verified;typed/shared/callable cycles pass. Serialized GC threshold rechecks and whole3406raw status maps retained.
All3406PHPT:onlybug77345_gc_1 timeout→pass (200000-node cycle performance,not a semantic repair);no new failures.2730pass/601fail;existing parity gaps remain.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/gc-pointer-tables-main-*.

## #306: perf(gc): traverse live property Cells without owned snapshots (2026-10-11)

Use existing prop_order/prop_get iterators in shared GC object graph and dying walks. Avoid temporary owned Vec<(String,Cell)> and cloned keys. Each dying step re-reads current live order, preserving callback mutation/tombstones; selected Cell/payload clone and object borrow are released before callbacks.

Graph/root/duplicate-Cell accounting,typed pins,purple buffering,generator/internal fields/destructor order unchanged. Keep owned live_props snapshots for other consumers requiring them. No layout/unsafe/dependencies. Extend ONE existing registered/plain-cell fixture for aliased duplicate props/live roots and unset tombstones; native outputs checked vs PHP/parent.

Runtime a0894ae601329f0f1d93ce207abd65a2d8027543; timing source a0894ae601329f0f1d93ce207abd65a2d8027543; report/integration source c8b23dee0e41ce0535e64aa741d68fa0ca0f13f4.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 145.785 | 151.997 | 1.0426 |
| bench/40-objects.php 20 5000 | 625.761 | 613.187 | 0.9799 |
| bench/20-strings.php | 60.922 | 57.810 | 0.9489 |
| bench/20-strings.php 100 | 272.575 | 245.233 | 0.8997 |
| bench/11-sieve.php | 61.643 | 60.961 | 0.9889 |
| bench/30-arrays.php | 75.717 | 78.112 | 1.0316 |
| bench/60-json.php | 504.470 | 504.531 | 1.0001 |
| bench/10-fib.php | 8.405 | 8.423 | 1.0021 |
| bench/50-regex.php | 40.695 | 39.967 | 0.9821 |
| bench/70-db.php | 107.589 | 107.957 | 1.0034 |
| bench/app/cli.php 100 20 | 33.091 | 31.684 | 0.9575 |
| examples/composer/run.php | 7.951 | 7.325 | 0.9212 |

7pairs ordinaryobjects+4.3%,long−2.0%,strings−5.1%/long−10%,sieve−1.1%,arrays+3.2%,JSONflat,fibflat,regex−1.8%,DB+.3%,app−4.3%,Composer−7.9%.31followup ordinaryobjects+.2%,long+.3%,arrays−.7%: normal objects effectivelyflat,not a demonstrated broad object speedup. Independent7pairs200000-nodecycle8798.866→8230.058ms(−6.5%),everyexit/stdout/stderr nativePHP-byte-gated. Physicalobjects5 requests1663202→1633154(−30048/~1.8%),bytes69798420→68356112;gcgraph32245→32236. Eventcell225018 unchanged; physical allocator counts are separate. Not attribute unrelated string/fib/app changes solely to GC. No PHP parity or overall claim.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1663202→1633154; bytes 69798420→68356112.
- gcgraph: requests 32245→32236; bytes 5997998→5997583.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
Existing GC fixture extended once;native duplicate-property Cell/keep-root/unset-tombstone probe passes. Existing lifetime/readonly/globals/native-graph/160-arg page-growth checks retained.
All3406individualPHPT compared; 0 known range crash/timeout swaps; no new fail/pass/skip.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/gc-property-traversal-*.

## #307: docs(perf): reject scalar-object GC snapshot guard after measurements (2026-10-11)

Report-only: reject and ordinarily revert #307 scalar-only last-owner object snapshot guard. Actual objects allocator requests fell by only four; no useful ordinary object speed improvement. Preserve candidate source, frozen binary hash, negative measurements and gates.

Final runtime restores the existing GC dying/property traversal, including resource/destructor/callback paths. No production scalar guard retained.

Runtime 207dd613572a5c5fde1dafb7720bd84539c46542; timing source 207dd613572a5c5fde1dafb7720bd84539c46542; report/integration source 207dd613572a5c5fde1dafb7720bd84539c46542.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 148.367 | 148.627 | 1.0017 |
| bench/40-objects.php 20 5000 | 583.609 | 607.759 | 1.0414 |
| bench/20-strings.php | 57.973 | 57.119 | 0.9853 |
| bench/20-strings.php 100 | 238.079 | 242.524 | 1.0187 |
| bench/11-sieve.php | 63.524 | 66.246 | 1.0428 |
| bench/30-arrays.php | 81.606 | 80.617 | 0.9879 |
| bench/60-json.php | 535.174 | 533.550 | 0.9970 |
| bench/10-fib.php | 8.357 | 9.147 | 1.0946 |
| bench/app/cli.php 100 20 | 33.197 | 32.306 | 0.9732 |
| examples/composer/run.php | 8.131 | 8.220 | 1.0109 |

Seven pairs: default objects +0.2% (flat), long objects +4.1%, fib +9.5%. Only four allocator requests saved in default objects. Reject rather than retain a guard without demonstrated target benefit. Serialized bug77345 parent 8340ms and candidate 8221ms both pass; concurrent candidate timeout remains in full raw results.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1633154→1633150; bytes 68355753→68355621.
- scalarleaf: requests 32479→32477; bytes 6010970→6010902.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing GC fixture extended during candidate verification; removed with rejected guard. Native output checked. Serialized GC rechecks retained separately.
All3406 raw results retain bug77345 pass→timeout and two known range crash/timeout swaps. Serialized10s bug77345 parent/candidate BOTH PASS (8340/8221ms); no new semantic fail. Candidate full summary2729pass601fail is not parity.
Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/gc-scalar-properties-*.

## #309: docs(perf): reject redundant weak-probe removal without target speed benefit (2026-10-11)

Report-only: reject #309 live same-capacity Rc weak-probe removal. Restore existing mem_realloc guard by ordinary revert; retain actual candidate/native gates and negative timings.

No production memory-accounting change retained. Growth/stale/huge/OOM/reference-lifetime paths remain parent runtime. Candidate ONE existing arena unit extension reverts too.

Runtime aa31f3a475018aad739e75711780e884d55a8aa7; timing source 1b5cf6fd9cc28f5a16a63bbbcd83043bbb26774a; report/integration source 1b5cf6fd9cc28f5a16a63bbbcd83043bbb26774a.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 154.302 | 152.581 | 0.9888 |
| bench/40-objects.php 20 5000 | 634.569 | 635.699 | 1.0018 |
| bench/20-strings.php | 53.999 | 52.328 | 0.9691 |
| bench/20-strings.php 100 | 247.866 | 238.328 | 0.9615 |
| bench/11-sieve.php | 64.179 | 63.716 | 0.9928 |
| bench/30-arrays.php | 76.978 | 83.803 | 1.0887 |
| bench/60-json.php | 526.189 | 519.366 | 0.9870 |
| bench/10-fib.php | 8.394 | 8.480 | 1.0103 |
| bench/50-regex.php | 38.891 | 38.740 | 0.9961 |
| bench/70-db.php | 107.713 | 112.159 | 1.0413 |
| bench/app/cli.php 100 20 | 33.910 | 33.318 | 0.9826 |
| examples/composer/run.php | 7.880 | 8.378 | 1.0632 |

Seven pairs arrays +8.9%,long objects +0.2%,default objects −1.1%,JSON −1.3%,Composer +6.3%.31 arrays 76.080→76.058ms (flat),DB−2.1%,Composer+2.7%. No useful target win, reject. Full suite PCRE bug72685 pass→fail was its0.5s wall threshold under concurrent build; serialized parent/candidate BOTH PASS314ms. Raw failure retained; not hidden as full map parity.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1633144→1633144; bytes 68354693→68354681.
- arrays: requests 636039→636039; bytes 26703119→26703107.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing arena unit extended for live/drop/sweep/same-capacity accounting during candidate verification; reverts with rejected guard. Serialized PCRE rechecks published.
Full3406 raw map2729pass602fail retains PCRE bug72685 pass→fail wall-time threshold and range32crash→timeout. Serialized parent/candidate PCRE BOTH PASS314ms. Other individual statuses identical; no semantic repair/parity claim.
Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 602, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 5, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/live-owner-realloc-*.

## #311: perf(vm): use value arguments for proven promoted constructors (2026-10-11)

Allow existing promoted_slots constructors in the value ABI and remove empty-body method-target veto. Reuse existing Slot::Arg promoted stores/default float widening and early materialization before property hooks. No duplicate binder/storage/backend.

Fixed by-value/constant defaults only; retain reference/named/unpacked/type/visibility/private/abstract/native/hook/OOM/page/trace/lifetime canonical guards. Provided-byref InitNew fallback unchanged.

Runtime 2e4dba83dc47d190c57ee118b41d52d23a42860b; timing source 2e4dba83dc47d190c57ee118b41d52d23a42860b; report/integration source 2e4dba83dc47d190c57ee118b41d52d23a42860b.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 147.867 | 149.524 | 1.0112 |
| bench/40-objects.php 20 5000 | 589.765 | 606.161 | 1.0278 |
| bench/20-strings.php | 56.203 | 55.039 | 0.9793 |
| bench/20-strings.php 100 | 254.167 | 248.064 | 0.9760 |
| bench/11-sieve.php | 63.718 | 64.165 | 1.0070 |
| bench/30-arrays.php | 77.686 | 79.267 | 1.0203 |
| bench/60-json.php | 521.534 | 524.661 | 1.0060 |
| bench/10-fib.php | 8.287 | 8.475 | 1.0226 |
| bench/50-regex.php | 39.237 | 39.404 | 1.0043 |
| bench/70-db.php | 106.134 | 107.083 | 1.0089 |
| bench/app/cli.php 100 20 | 32.688 | 34.355 | 1.0510 |
| examples/composer/run.php | 8.176 | 7.771 | 0.9505 |

Seven actual objects pairs +1.1%default/+2.8%long,31 +1.1%both: no ordinary object win. Physical actualobjects unchanged1633154: $pts[]=new uses Canonical assignment, so compiled constructor is not reached. Separate general direct $p=new promoted ctor20000 probe: allocations852095→732090(−120005),cell events120015→80015;7pairs63.043→61.382ms(−2.6%),31pairs64.924→59.891(−7.8%). This demonstrates bounded compiled constructor benefit, not benchmark40/parity benefit. Initialapp+5.1%,31app−.8%flat; other initial negatives retained.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1633154→1633154; bytes 68356830→68356838.
- ctor-direct: requests 852095→732090; bytes 36353259→30929407.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing receiver unit extends default/inherited/func_get_args/ref/type-error/weak ctor cases and asserts cached compiled value_abi. Extra native complete type message/default/extra/ref probes pass.
All3406individualPHPT compared; 2 known range crash/timeout swaps; no new fail/pass/skip.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/promoted-ctor-value-abi-*.

## #315: perf(objects): borrow effective promoted property metadata (pre316 dataset) (2026-10-11)

Pre316 dataset only: reuse existing decl_prop_site in shared promoted_prop_write instead of cloning PropDecl solely to read effective visibility/readonly/class. Applies AST and compiled constructors; no new cache/layout/helper/unsafe/dependency. Frozen b7281ad/3001e4c parent2e4dba8/13fac09 excludes newly merged JSON #316. Current main999 integration adds JSON and requires fresh measurement/gates below.

Same pinned scope/private/inheritance resolution and effective hook/readonly/private owner/slot fallback guards. ONE existing promoted-store runtime unit adds child effective set hook, native checked.

Runtime b7281ad7345305d31060b8dea6e2d787e44d9cc8; timing source b7281ad7345305d31060b8dea6e2d787e44d9cc8; report/integration source c3b77583def04ab6ed75a706b820691a67b29191.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 149.430 | 142.633 | 0.9545 |
| bench/40-objects.php 20 5000 | 644.096 | 601.716 | 0.9342 |
| bench/20-strings.php | 57.249 | 61.719 | 1.0781 |
| bench/20-strings.php 100 | 257.849 | 266.267 | 1.0326 |
| bench/11-sieve.php | 66.020 | 66.184 | 1.0025 |
| bench/30-arrays.php | 82.160 | 79.829 | 0.9716 |
| bench/60-json.php | 524.940 | 546.132 | 1.0404 |
| bench/10-fib.php | 8.132 | 8.832 | 1.0861 |
| bench/50-regex.php | 41.671 | 40.396 | 0.9694 |
| bench/70-db.php | 110.915 | 111.101 | 1.0017 |
| bench/app/cli.php 100 20 | 34.268 | 34.436 | 1.0049 |
| examples/composer/run.php | 8.405 | 8.426 | 1.0025 |

Pre316 seven objects149.430→142.633ms(−4.5%),long644.096→601.716(−6.6%);31objects158.930→152.275(−4.2%),long606.457→581.340(−4.1%). Physicalobjects1633154→1483154(−150000). Initialstrings+7.8%/long+3.3%,JSON+4.0%,fib+8.6% retained;31strings+2.1%/long+3.8%,JSON+.3%,fib−3.0% not attributed. This is historical pair, not current-main/parity claim.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1633154→1483154; bytes 68355761→66855749.
- arrays: requests 636049→636049; bytes 26704253→26704241.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing promoted-store runtime unit effective child set hook/private collision;native extra complete probe pass.
All3406individualPHPT compared; 2 known range crash/timeout swaps; no new fail/pass/skip.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/promoted-decl-borrow-*.

## #315: perf(objects): borrow effective promoted property metadata on current main (2026-10-11)

Use existing decl_prop_site in shared promoted_prop_write to borrow effective PropDecl without cloning metadata. Identical scope/private/inheritance lookup and guards; no new cache/layout/helper/unsafe/dependency. Both AST and VM constructor paths. Fresh integration includes currentmain999c517 JSON316 and ctor317; historical pre316 dataset retained separately above.

Effective hook/readonly/visibility/private declaring-class/slot fallback unchanged. ONE existing promoted-store unit extends inherited effective set hook/private collision, complete native output checked. Known main JSON live-reference regression is unchanged here and tracked/fixed next in #319.

Runtime f18b3b4c53551d4f9ecc0abf82feadab64fa025c; timing source f18b3b4c53551d4f9ecc0abf82feadab64fa025c; report/integration source f18b3b4c53551d4f9ecc0abf82feadab64fa025c.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 157.701 | 155.052 | 0.9832 |
| bench/40-objects.php 20 5000 | 640.850 | 607.994 | 0.9487 |
| bench/20-strings.php | 57.781 | 60.565 | 1.0482 |
| bench/20-strings.php 100 | 269.991 | 269.016 | 0.9964 |
| bench/11-sieve.php | 67.411 | 68.204 | 1.0118 |
| bench/30-arrays.php | 88.789 | 86.030 | 0.9689 |
| bench/60-json.php | 589.872 | 561.979 | 0.9527 |
| bench/10-fib.php | 8.312 | 8.412 | 1.0120 |
| bench/50-regex.php | 42.879 | 42.708 | 0.9960 |
| bench/70-db.php | 115.156 | 115.250 | 1.0008 |
| bench/app/cli.php 100 20 | 35.889 | 36.222 | 1.0093 |
| examples/composer/run.php | 9.953 | 10.519 | 1.0569 |

Currentmain999 fresh7objects157.701→155.052ms(−1.7%),long640.850→607.994(−5.1%);31objects146.877→142.595(−2.9%),long624.569→604.752(−3.2%). Physicalobjects1633154→1483154(−150000).7strings+4.8%,long−.4%,JSON−4.7%(not attributed),fib+1.2%,Composer+5.7% retained;31strings+2.2%,Composer+2.3%. No whole/parity claim. FullrawGC77345pass→timeout retained;serializedparent/candidate PASS8804/8728ms.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1633154→1483154; bytes 68355725→66855769.
- arrays: requests 636049→636049; bytes 26704217→26704261.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing promoted-store runtime unit plus complete native extra child-hook/private probe. Serialized GC rechecks retained.
All3406 raw status maps:onlyGC77345pass→timeout (candidate2729pass601fail). Serialized10s parent8804/candidate8728ms BOTH PASS; full raw timeout retained. No other status change. Existing main JSON reference regression tracked#319, not covered by PHPT; standalone native reproducer published.
Candidate summary retained: {'total': 3406, 'passed': 2729, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 7, 'applicable': 3343, 'compatibility_pct': 81.63};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/promoted-decl-borrow-main-*.

## #318: docs(perf): reject canonical frame buffer retention after negative timings (2026-10-11)

Report-only: reject #318 frame_new buffer preservation after actual-target speed measurement. Ordinary revert restores existing canonical frame reset; no production capacity-retention change retained. Keep pre316 unvalidated ELF provenance separate from fully verified current integration ade0f37/8ce5b19.

Full Frame::new context reset, owner/destructor/call/namespace semantics restored exactly to parent315. Candidate ONE existing canonical unit extended capacity/no-owner/context assertions and passed; reverts with prototype. Known inherited main JSON live-reference regression unchanged, fixed next#319.

Runtime ade0f37ba5fe3682a9d6ad914e23575319b590c8; timing source ade0f37ba5fe3682a9d6ad914e23575319b590c8; report/integration source cd81522a80b000a653d52999b4979764cd7a5784.

Seven isolated alternating profiler-off nativePHP8.5.11 -n byte-gated pairs;
cold CLI including startup/execution. Parent/candidate binary hashes and raw samples
are in the timing JSON. OPcache loaded, enable_cli=0, JIT disabled.

| argv | parent ms | candidate ms | candidate/parent |
|---|---:|---:|---:|
| bench/40-objects.php | 146.265 | 153.807 | 1.0516 |
| bench/40-objects.php 20 5000 | 589.460 | 608.933 | 1.0330 |
| bench/20-strings.php | 55.359 | 55.723 | 1.0066 |
| bench/20-strings.php 100 | 238.843 | 248.126 | 1.0389 |
| bench/11-sieve.php | 65.413 | 67.890 | 1.0379 |
| bench/30-arrays.php | 77.949 | 78.259 | 1.0040 |
| bench/60-json.php | 504.180 | 518.898 | 1.0292 |
| bench/10-fib.php | 9.254 | 8.793 | 0.9501 |
| bench/50-regex.php | 39.267 | 39.667 | 1.0102 |
| bench/70-db.php | 112.757 | 106.967 | 0.9486 |
| bench/app/cli.php 100 20 | 33.128 | 34.421 | 1.0390 |
| examples/composer/run.php | 9.642 | 9.865 | 1.0231 |

Physicalobjects1483154→1433156(−49998/~3.4%),bytes−4.5MB.7objects+5.2%,long+3.3%,longstrings+3.9%,sieve+3.8%,JSON+2.9%,app+3.9% retained.31objects145.867→146.430(+.4%flat),long592.073→599.324(+1.2%),longstrings244.852→254.577(+4.0%),sieve64.292→68.366(+6.3%),JSON+.8%,app+.4%. No target win; reject. FullGC77345timeout→pass is wall threshold,not semantic repair: parent serialized8728msPASS already published;candidatefullPASS.

Separate physical allocator requests/bytes (not speed runs):

- objects5: requests 1483154→1433156; bytes 66855410→62355566.
- arrays: requests 636049→636049; bytes 26703845→26703821.

Workspace/fmt/clippy pass;25 script+19 workload byte gates and VM-profile pass.
ONE existing canonical receiver unit extended capacity/empty PHP owners/context defaults;removed with rejected prototype.
All3406raw maps:onlyGC77345timeout→pass;candidate2730pass601fail. Parent frozen serialized8728ms alreadyPASS,so not semantic repair;raw maps retained. Other statuses identical. Known main JSON reference bug unchanged,tracked#319.
Candidate summary retained: {'total': 3406, 'passed': 2730, 'failed': 601, 'xfailed': 0, 'xpassed': 0, 'skipped': 25, 'unsupported': 38, 'borked': 0, 'crashed': 6, 'timed_out': 6, 'applicable': 3343, 'compatibility_pct': 81.66};
this is not full compatibility. Explicit release core clean/rebuild/hash recorded.
External CI not asserted green. No suite parity claim or multiplication of step gains.
Raw/source/hashes in bench/data/92/parity/oct11/frame-reset-buffers-*.
