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
