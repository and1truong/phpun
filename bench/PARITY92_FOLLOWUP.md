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
