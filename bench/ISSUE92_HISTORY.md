# Issue92 consolidated history

Exact issue-body snapshot before the current-state consolidation. Source updated_at: 2026-10-11T17:34:29Z. SHA256 of original UTF-8 body: e0d3e81b340a0b16a3e7ea62fffc240764b363b3067638cf847fd67fd0c7d5e4.

Historical measurements/configurations and implementation states below are preserved as history, not current source truth. Current issue92 BODY governs roadmap/status; current source/report records supersede older representation descriptions.

## Mục tiêu và cách dùng tracker

Đạt hiệu năng ngang PHP 8.5 trên workload thực tế, giữ semantics và công bố cấu hình đối chứng. Stack hiện tại chưa đạt đích.

**Body này là nguồn chính cho baseline, roadmap, dependencies và trạng thái. “Checks”=workspace tests/fmt/clippy; native/PHPT evidence ghi riêng.** Roadmap cập nhật ở body. Tracker tiếp tục mở, PR dùng `Refs #92`. Checklist[x]=implemented+verified trong stack;merged/open ghi riêng.

## Trạng thái mới nhất — 2026-10-11 17:34 UTC, main4e6b847; PR331→333→336→337→339→341→343→345→346→349→351→354→355→357→359; #358 trên359, #360 trên358

**#297/#299 đã vào main:** memory-limit cache,2P1root-global fixes,ctorbyref fallback295,packedfills. Closed-parent294 repaired297(ancestry/codechecked);#292 done via295,#187broader mixedstorage stillopen.

**Historical baseline main0ddef51→#299:**7rotating byte-gated/profiler-off pairs,geo4.2658→4.0376×PHP;configuration/fullratios/raw retained in report. Separate from current/user-host ratios.

**#293→#299 merged:** dense scalar array_fill implicit keys/lazy Cells;compact enum.7sieve−22.9%/long−13.5%;arrays+.8%,JSON−1.9%,objects−3.3%,strings−.8%,app−3.3%;31fib+3.7%,JSON+2.6% retained. Physicalsieve1032477→32477requests.3406PHPT2729pass601fail,knownrange swap only. Ref/foreach/sort/readonly gates pass;splice retains same pre-existing extraelement. Rejectedwide31arrays+4.3%,strings+2.1%,JSON+2.7% retained. [Raw](/and1truong/phpun/blob/923fe1e966e7/bench/PARITY92_FOLLOWUP.md).

**#296 rejected/NOT_PLANNED;report300 merged:** objects+1.4%/long−.6%,ctor+.7%,physicalunchanged;gatesPASS,ctorASTnotVMproof. Revert32d1195;raw/reviewerlabel73eb95f retained.

**#298→#302 merged525c4c9/published76932f5:** guardedscalarbuiltin kernels before rootbridge;refs/user/method/ctor/profiler/OOM/page cases canonical. Frozenf4313bd/d193a8f,allchecksPASS,3406onlyrange32swap,2729/601.7strings66.352→56.088ms(−15.5%)/long−16.7%;initialsieve+4%,slice+2.9%,replace+3.1%;31+.4%/+1.7%/+1%. Physicalstrings1002902882requests unchanged,bytes+16. Whole7geo4.03873→4.04012×PHPflat,strings−8.8%/−12.6%,objects+3.3%,JSON+1.1%,app+.7%. No parityclaim. Raw/mainreport,CIgreen,runtime unchanged afterintegration.

**#301 rejected/NOT_PLANNED;report304 closed302→main via308:** refscan checks/identical3406;31strings+1%/+2.8%,objects+.2%,fib+3.6%;physical−52000,no gain.7strings+5.7%/+2.4%;replace AST-root notVMproof. Revertbb1e460;raw042b495/mainreport.

**#303→#308 merged082b8f2:** GC scratchFxMap/FxSet,logic/owners unchanged;c44f9ea/33f740f. Checks/25+19/profile/full3406PASS,2730/601,onlyGCtimeout→pass.7cycle12.743→8.936s(−29.9%);objects−.8%/long+2.2%,strings+5.9%/+2.5%,JSON+1.1%;physicalunchanged. Pre30531strings+6.3%/cycle−30.1% retained;CIgreen,reports304main;raw/mainreport.

**#306/#312 mergedb4c005c:** GCborrowedpropsa0894ae/bb9f0bf;3406identical2730/601.7cycle−6.5%;31objects+.2%/long+.3%flat,physical−30048. Initialobjects+4.3%/arrays+3.2% retained;closed310 repaired312,runtime diff empty.

**#307 rejected/NOT_PLANNED;report313closed312→main314:** scalar207dd61/e2b40a0:objects+.2%,long+4.1%,fib+9.5%,only4fewerrequests. RawGCpass→timeout+2ranges,serializedPASS8340/8221. Reverts86f2878/98c0ab9 restore81.

**#309 rejected/NOT_PLANNED;report314 main:** weakprobeaa31f3a/0f77b51,7arrays+8.9%,31flat,objectsflat,physicalunchanged. RawPCRE72685pass→fail0.5s duringbuild+range32 retained;serializedboth314msPASS. Revert5103185 restoresruntime;reports307+309included,gates/CIgreen.

**#311/#317 merged999c517:** promoted ctor ValueABI;2e4dba8/13fac09,gates/full3406only2ranges,2730/601,CIgreen.31directnew−7.8%(64.924→59.891),physical−120005. Actualobjects+1.1%/long,requestsunchanged:appendnew remains AST. Initialapp+5.1%,31−.8%flat kept. Defaultwiden/hooks canonical.

**#315/#321 mergedcb0d61f:** promoted-store borrows effective decl(no clone/cache);f18b3b4/c1d6ab5 allchecksPASS,3406GCpass→timeout,serialized8804/8728PASS.31objects−2.9%/long−3.2%,physical−150000;strings+2.2%,Composer+2.3%. Pre31631objects−4.2%/−4.1%,strings+2.1%/+3.8% historical/raw separate.

**#318 rejected/NOT_PLANNED → #322,merged main:** framebuffer retention passeschecks,physical−49998requests;31objects+.4%/long+1.2%,longstrings+4%,sieve+6.3%,JSON+.8%,app+.4%. Reverts9905b95/89732ca restore315runtime exactly. FullGCtimeout→pass retained(parent serializedalreadyPASS). Raw/report in branch86;notimplemented.

**#319/#323 merged6c10715:** JSON live/dead reference repair. Native/pre316[0,7],main[0,1],fixed[0,7];dead/live/nonref snapshots[0,1]/[0,7]/[0,1]. Original69f2ace/857050a:31JSON−3.2%,objects+2.5%,requests4360112unchanged/bytes−16.4MB. P2fixed27f7c00/cae5751,threadresolved;gates/native/full3406onlyGCpass→timeout+2ranges;serialized8306/8593PASS.31reviewJSON−.5%,objects+.3%flat;rawf8ef107 inmain325.

**#320 → #325,MERGED main4e6b847:** guarded float methods, canonical misses;P2JSONfix included. Source77cd3da/hashd6dc53c,gates/native/full3406onlyGCtimeout→pass(serializedparentPASS).31objects−18.0%/long−19.6%,physical−124993;31sieve+1.7%,app−2.5%not attributed. Raw832a658,CIgreen;pre-P2whole retained as historical.

**#324 → #327,MERGED main4e6b847:** guarded fresh promoted-float ctor,canonical misses. af590b1/d261288,gates/native/full3406only2ranges,2730/601.31objects127.486→106.070(−16.8%),long475.879→439.881(−7.6%),physical−300034;31JSON+4.5%,app−6.2%not attributed,Composerflat. Whole7main6c107→stackgeo3.91818→3.79268×PHP(−.86%parent);objects12.486×/long23.848×PHP. [Raw/source](/and1truong/phpun/blob/ddd87a567a90/bench/PARITY92_FOLLOWUP.md).

**#326/PR329 report-only MERGED main4e6b847,NOT_PLANNED:**31objects+1.6%/long+.6%,strings+3%,physical−50000. Checks/full3406only2ranges;revert71c207e restores327;raw+excludedcontendedpartial retained. No runtime change.

**#328 rejected/NOT_PLANNED;#331 report-only OPEN main:** bounded keyed JSON decode +capacity8, allchecks/native/full3406only2ranges+GC77345pass→timeout;serialized10s parent/candidatePASS.31JSON527.877→532.771ms(+.9%),arrays+.3%,Composer−3%not attributed;physicalJSON−540000requests/−4.3MB. Revertscd9ea63/303428a restoremain4e6runtime exactly. Initial7JSON−1.7%,Composer+6.6% kept separately. [Raw](/and1truong/phpun/blob/7ae4bb81bfd7/bench/PARITY92_FOLLOWUP.md).

**#330 → #333,OPEN on331:** bounded compiled-pattern cache. Initiale398/706:31patterns−13.1%,whole7geo+2.66%negative;31JSON+1.7%,app+1% kept. Reviewerworkerreset/upgradepath fixed0217e12/hash2cc;weak-code release/recompile proved,checks/native/full3406only2ranges,GCserializedPASS.31old-cache/new-cache corpus−2.4%,patterns+.3%flat,Composer−2.3%;initialreview7corpus+6.6% kept. Threads resolved,CIgreen,report73fe094.

**#332 → #336,OPEN on333:** nativePCREJIT/PHPINI cachedtoggles/32–192KiBstack. Final0938/0b0445 usesPHP4096capacity,keys>64KiBuncached;checks/native/full3406:2731pass599fail,2JITfail→pass+2ranges only vs333. GCfulltimeout retained,serializedPASS;2JITcases isolatedoldFAIL/newPASS.31corpus41.400→19.262ms(−53.5%),300patterns−11.8%,4patternsflat,5000patterns+38.6%negative,objects+.8%. Whole7main4e6→stackgeo3.77709→3.50124×PHP(−7.3%),regex2.476×/objects13.686×/sieve8.888×PHP;wholeobjects+9.5%parent retained beside31flat. Initial256thrash300+31.3%/whole3.6305× historical. Report71dc4fb;no fullparity.

**#334→#337,OPEN on336:** borrowed booleanINI reads,db315report;checks/native/full3406only2ranges PASS.31four-pattern+.9%flat,300−2.4%,corpus−2.2%;20004fewer requests. Retain small stdsimplification,not proven primary timing win;initial7/raw negatives kept.

**#335→#339,OPEN on337:** sharedclass propertyorder/CoW;checks/native/full3406allstatuses identical.31objects−8.2%,long−12.6%,JSON+.7%flat;initial7objects+3.1% retained. Objects−74996requests,arrays/JSONunchangedrequests/+884bytes. Freshwhole7 main4e6/INI/candidate geo3.855→3.520×PHP,candidate/parent−1.2%;fib+4.8/arrays+1.9/DB+2.0/JSON+.5% negatives retained. Runtime61f9d69/frozen7281116;build466+archivedformatpatch,not clean466;report77bba2e,PR339published.

**#338→#341,OPEN on339:** guardedtypedinteger/packedboolean regions;privatebuffers+rollbackVM,512ops/256CVs/10msteps/1MiBcap. ONEactivation/CoW/overflow/ceilingcheck+checks/native25+19/profile PASS. Final7aa40eb/53a0e08 all3406statuses identicalparent. Final31sieve−69.8%,long−75.6%;objects+2.3%,arrays+1.1%,JSON+.5%,Composer−5.2% retained. Whole7 main3.760→candidate3.053×PHP,parent−11.8%;sieve3.091×/objects11.540×PHP. Physicalsieve10+380requests/+5.25MB;wholefib+4.6/startup+3.4/app+4.7% negatives. Initial3.069×cohort/31negatives/GCserialized retained;reportdb104e9. Portabletypedstack,not fullregisterVM/JIT. **#340→PR343,OPEN on341:** nativeemitter/W^X reuse+8MiBsharedbudget,privateprimitiveABI. 32319e5/0a1d298 checks/native25+19/profile PASS;3406onlyrange+GCtimeout→pass,serializedparent9258/cand9698ms bothPASS. ONEcheck provesnativeactivation/overflow/bounds/budget/mappingrelease.31sieve22.043→10.193ms(−53.8%),long152.079→33.115(−78.2%);objects+1.5%,arrays−3.2%,JSON−1.9%,Composer−1.8% notattributed. Whole7 main3.864→native2.777×PHP,parent−11.0%;sieve1.336×PHP/objects12.303×PHP. Negative31strings+4.0%,long+7.9%,DB+.8%;wholelongstrings+10.9% retained. BothOp128bytes;regressioncause notidentified;report26f7728,PRpublished. **#342→PR345,OPEN on343:** sharedMemProbe boxedclosures→stdWeakAny;onlyunsizedproducer wasPCREdummy token→Rcunit,sameaccounting. Retire/credit keepunsizedAPIs. 3911b6c/1042bda checks/native25+19/profile PASS;3406onlyGCpass→timeout,serializedparent8951/cand9348ms bothPASS. ONEweakcount/death/realloc/credit/retirecheck PASS.31JSON−22.1%,objects−4.9%/long−4.2%;strings+3.4%/long+1.6%,sieve+3.6% negatives retained;whole7 main3.816→candidate2.754×PHP,parent−3.4%;physicalstrings−804605requests,objects−50013,arrays−20013,JSON−15156. Whole app+7.3/Composer+11.4/sieve+8.1/regex+4.3% negatives retained;report8a77753. **#344 REJECTED/NOT_PLANNED→PR346,report-only on345:** e69221d/90bf414 experiment checks/native25+19/profile PASS;3406onlyGCtimeout→pass,serialized10s parent8937/cand8386ms bothPASS.31objects+2.1%/long+6.2%,JSON+.5%,arrays+.2%,strings100−7.0% not attributed;physicalobjects only4 fewer requests/104bytes. Ordinaryrevert47f5ac5 restores PR345 runtime exactly;reportfb82334 retains all raw/negativefacts. No freshwhole follow-up after rejection. **#347 REJECTED→PR349 report-only on346:** 4d10d60/b65f26e inlinefloat/stdOnceCell forASTfresh+compiledpromotions;ONEreceivercheck+workspace/fmt/clippy/native25+19/profile/typed/readonly/reflectionprobe PASS.3406onlyrange+GCtimeout→pass;serialized10s parent8543/cand9072ms bothPASS,599knownfails unchanged.31objects−3.6%/long−1.6%;confirm31objects−3.7%/long−.6%flat. Whole7 main3.941→candidate2.776×PHP,parent−1.6%;defaultobjectsflat/long+2.8%,regex+4.1/app+.7% negatives retained. Physicalobjects−50002requests/−1200004bytes;arrayssame requests/+44B. Historicallimitedgain;report16aca7b. P2review17:17: ordinaryslots8→24B penalizeineligibleclasses;revertsb217672+82c76dc restorecanonical8B slots,ONEactualsizeofcheck+freshchecksPASS,prod==PR345. Report83b75e7;threadresolved,mergedthrough351/354/355/357. **#348→PR351,OPEN on349:** a8aec12/19d80fe existing scalarargpool reused forASTfresh sends/ctor retirement;ONEpointerreuse/weak/sharedcheck+workspace/fmt/clippy/native25+19/profile/probes PASS. All3406statuses identicalparent,599knownfails.31objects−2.0%/longflat,strings100+2.3%/fib+2.2% negatives retained. Whole7 main3.856→candidate2.758×PHP,parent−1.9%;objects−4.5%/10.624×PHP,long−1.9%/20.283×PHP;app+10.6%notconfirmed by31−.6%,strings100follow31+1.2%,fib−4.7%. Physicalobjects−49997requests/−2.0MB;arrayssame requests/−36B. Report063cab0 retained allraw. **#350→PR354,OPEN on351:** 0f1245b/dd26c59 exacttyped scalarbuiltin spans fitpage/sharedrepayment withouttemporarytokens;ONEarena/type/pagecheck+workspace/fmt/clippy/native25+19/profile/correctedprobe PASS.3406onlygenerator sendtimeout→fail,same10s isolatedparent8844/cand9451ms bothFAIL;fullraw/normalizedmaps kept.31strings−13.0%/long−14.4%,objects+3.1%,JSON−.4%,sieve−1.7%;whole7 main3.832→candidate2.766×PHP,parent−1.0%,strings−16.0/long−15.0%,still3.124/3.976×PHP. Wholefib+9.6/startup+4.9/regex+6.5/JSON+4.6%;confirm31fib+1.5/regex+1.3/JSON+2.3/objects−1.0% retained. Physicalstrings−800000requests/−19199968B;report6f6dbce. Prior sqrt(null)warning/4MBsubstrOOMmiss parent=candidate,initialfailedprobe/raw kept;#352 sharedfix below. **#353 REJECTED/NOT_PLANNED→PR355 report-only on354:** fd9fc27/25b866d/1828385 experimentallchecks/native/full3406onlygenerator fail→timeout;serialized10s parent9933/cand9807ms bothFAIL.31objects+.4%/long−1.0%;wholegeo+3.3%,longobjects+9.8%,app+4.8/Composer+4.5%;confirm31long−2.2%/app+6.0%/Composer+2.9% retained. Physicalonly25000requests/125KBsaved. Ordinaryrevert9c17764 restores PR354 runtime;reportdc4965b keepsallraw. **#352→PR357 on355:** sharedsubstr livecharge+OOMframe99c4de5/integration9dbee92/ELFc845993. ONEarena/livecharge/OOMcheck+checks/native25+19/profilePASS.4MB exactexit255/stdout/stderr withlog_errors0;canonicalprofile stdout/exit match,profilerstderrseparate.3406GCpass→timeout/generatorfail→timeout;serializedGC7906/8232msbothPASS,generator9407/9258bothFAIL.31strings+9.8%/long+10.6%,objects−1.6%,JSON/arraysflat;wholegeo2.700×PHP,parent+.8%,strings3.740×/objects12.305×. Wholefib+8.9%,app+2.6%,Composer+7.9% notconfirmed/notattributed;physicalidentical. Report9fd872f/CIgreen before349review;newcombined7e87997/ELFeca4dd2 checks/native/full3406onlyGCtimeout→pass,OOMbytegatePASS;31objects+4.8%/long+6.1%,stringslongflat,app+.6%/Composer+.8%flat;wholegeo2.722×PHP,objects12.051×/strings3.594×. Wholeapp+6.2%/DB+1.4% retained;physical+50002objectrequests/+1200048B restorationcost. **#356 REJECTED→PR359 report-only on357:**87d5ff1/94e05cf/ELF1221735 allchecks/native/3406statusesidenticalparent.31objects+1.5%/longflat,strings100+2.2%,fib/Composer+.3%flat;physical~25000requests/~.8MBsaved. Revert80c2f23,report8577281;no freshwhole afterrejection. Incoming349review separatelyverified,not retroactivechange ofsamples. Reportd0e1948;production==updatedPR357. **#358 branch106 on359:**6e5a05e postGC survivingObject guard avoidsdestructortally;ONEalias/finalownercheck+workspace/fmt/clippy/native25+19/profile PASS;release6b6339d/full3406verificationcomplete,measurementrunning. **#360 branch107 on106:** exactscalarhelper readsvalue-stack slice beforeargument-vectorlease;existingguard/meter/callsite preserved,ONEcompiledactivation/poolexclusioncheck added;untimed.

**Chưa ngang PHP.** Snapshot người dùng sau #235 (không phải main hiện tại): geomean2.71× không đổi; objects7.88× vẫn là gap lớn nhất. Giữ nguồn riêng với native-agent datasets bên dưới, không trộn ratios khác host hoặc nhân speedup giữa steps.

| Bench | Snapshot sau #235 (theo người dùng) |
|---|---|
| geomean | 2.71× / 2.71× |
| objects | 8.64× / 7.88×;216ms,min208 |
| fib | 4.02× / 4.18× |
| strings | 3.67× / 3.90× |
| arrays | 4.16× / 3.82× |
| JSON | 4.20× / 3.93× |
| sieve | 5.83× / 5.78× |
| DB | 2.02× / 1.91× |
| regex | 1.31× / 1.50× |
| startup | 0.25× / 0.25× |

Objects cần giảm khoảng87.3% thời gian hiện tại để về1× nếu PHP đối chứng giữ nguyên; sieve82.7%,fib76.1%. Tối ưu vài phần trăm chưa đủ. Startup không đại diện execution throughput; không đặt thời hạn/parity claim trước khi đo.

**Những implementation mới trên main:** declared property layout/slots và ThisProp slot-index cache (025e97b/6a172bf); method normalization/resolution memo (#232), real compiled MethodCall + method frame context (#235). #187 body cũ “declared property slots chưa triển khai” được sửa; dense scalar array_fill packed storage đã vào main qua #299; broader mixed inline storage vẫn mở #187. #92 vẫn mở.

## Lịch sử stack — các cặp đo trước main hiện tại

**#236→#238 merged4a98df0:** live-frame call rollback giữ canonicaltoken/page/memory. Frozen e202891→9d1544f,7nativePHP/main/candidate:fib−12.6%,sieve−10%,arrays−11.3%,geo−5.6%(9.083→8.571×PHP,không số user2.71×). Fib26physical425169→32254requests,18562026→5979410bytes. Negativesobjects+1.5%,regex+2.2%,appinitial+5.2%;31app−4.6%,Composerflat,startup+2.7%overlap.3406onlyrange71132crash→timeout10s/standalone30sbothcrash;23VM/pagegrowth+19workload/workspace,fm,clippy pass;CPUfib34/1379samples,objects150×5000 zero lost. CIqueued lúcđo,reviewnofindings;no parityclaim. [Raw/report](/and1truong/phpun/blob/98d671bdd4ef/bench/PARITY92_FOLLOWUP.md).

**#237 → PR #241:** hai prototype trace shell+borrow và borrow-only đã bị loại, ordinary revert; runtime diff cuối cùng rỗng. Full observedcopybytes−97.2% nhưng fibflat, sieve+10.7%,objects+4.9%,regex+10.1%; borrow-onlyfib+4%,objects+2%. GitHub closed experiment ticket sau report-only merge; full lazy trace chưa triển khai. Raw/gates/hashes/prototype history và resource rechecks nằm trong PR body/report, không đổi negative cases thành speedup.

**#239/#242 merged d6c3ef7:** borrowed receiver teardown proof keeps canonical owner/destructor guards.7objects253.969→231.728ms(−8.8%),long−4.8%,app−15%,fibflat. Checks/23+19/profile/native+3406PHPT2727pass603fail;onlyrange71132swap. InitialSKIPIF-under-SUT raw retained;rerun usesPHP reference. No parityclaim;details in PARITY92_FOLLOWUP.md on codex/92-51-receiver-teardown.

**#240/#245 merged3ea4df5:** bounded x86_64Linux native pure globalinteger graph tier overexistingOps,notfibrecognizer. W^X/currentpage/overflow/depth/sideeffect guards replayVM;methods/arrays stayVM. ReviewerP1 READ4096fixedbe0e810/089bede;cleanASan originalreproduces/fixedpass,resolved. NativePHP8.5.11-n/OPcacheCLI0/JITdisable7fib10.901/parent132.499/fixed8.300ms (−93.7%step,.761×PHP);9geo8.183→5.946×PHP,not user2.71×host. Negativessieve+1.8%,objects+3.8%,JSON+1.4%,app+.7%,Composer+2.5% retained. Checks/25+19/profile/all3406statuses identicalparent2727pass603fail. [Raw/source/ASan](/and1truong/phpun/blob/089bedeae941/bench/PARITY92_FOLLOWUP.md). #237/#241report-only rejection,full lazytrace unimplemented.

**#243/#246 merged:** constructor parameter Rc metadata borrow,sharedbinder unchanged.31objects248.653→242.777ms(−2.4%),long988.904→983.079flat;exploratory−6.7%/−5.2% not currentclaims. Physical100000objects13463260→12763259requests(−700001),bytes−17.6%. Checks/25+19/profile/full3406only2knownrange swaps,no newfails. Raw/gates/source in codex/92-53-ctor-metadata/bench/PARITY92_FOLLOWUP.md.

**#244/#248 merged:** bounded context-free pure two-int callback comparator overexistingnative Ops;MIN/MAX-safe<=>,current-page/overflow/capture/type/owner guards replayVM.7arrays173.901→89.369ms(−48.6%),long−45%,usort−51.9%;app/Composerflat. Physicalarraysrequests−28.1%,bytes−28.7%. Checks/25+19/profile/full3406only2rangeswaps,no newfails;warm nativeMIN/MAX/capture/bool cases pass. [Raw](/and1truong/phpun/blob/39ccc562fe8d/bench/PARITY92_FOLLOWUP.md).

**#247 → PR #250:** đã merge; step đo trên parent #248. Guarded literal scalar existing integer-key write trong unique array; reuse DimCv/get_cell/cell_store; CoW/missing/nonint/globals/typed/owned/merged/readonly/container/handler cases canonical. Shared typed assignment-result bug sửa ở static/CV/dimension paths và PHP oracle chứng minh. Workspace8core/22runtime/19format,fm/clippy,25script+19workload+extended dimension byte gates,VM-profile,3406PHPT no new fail/pass/skip (hai knownrange crash/timeout swaps). Final7pairs sieve205.174→83.200ms(−59.4%),long−60.8%; arrays+1.6%,fib+4.9% giữ;31followuparrays+1.2%,fib−1.6%. Physicalsieve requests1,869,802→132,099(−92.9%),bytes−60.5%. Initial−63% superseded. Packed/mixed-storage #187 vẫn mở; không parity claim. [Report/raw](/and1truong/phpun/blob/c9f07c01c596/bench/PARITY92_FOLLOWUP.md).

**#249 → PR #252:** đã merge; step đo trên parent #250. Borrow property declaration pinned immutable Rc<Class>+index trong shared store; owned wrapper giữ cho callers khác. Workspace8core/22runtime/19format,fm/clippy,25script+19workload+warm comparator+dimension byte gates,VM-profile qua; **all3406 individualPHPT statuses giống parent**. Objects7pairs−4.1%,31followup238.160→222.645ms(−6.5%); initialComposer+3.6% giữ,31followup−5%. Physical100000objects requests12,763,259→10,963,259(−1,800,000/~14.1%),bytes−5.6%; events không tính thay physicalallocations. Chưa object/suite parity. [Report/raw](/and1truong/phpun/blob/edfd508dcb8a/bench/PARITY92_FOLLOWUP.md).

**#251 / PR #254:** worklist verifier cho pure integer backward/nested control flow, reuse emitter/jump patching. Existing ONE check chứng minh actual-loop execution/break/continue/overflow/uninit/deadline/cleanup. Generic scalar loops: nativePHP/parent/candidate100k5.013/33.878/7.404ms (còn1.477×PHP),1M15.730/275.901/11.312ms (0.719×PHP); không gán cho original9bench. Initialarrays+9.4%/Composer+1.9% giữ;31followuparrays−2.1%/Composer−2.2%. Workspace/fm/clippy,25+19+extendedbyte/VMprofile qua.3406PHPT chỉ2knownrange crash/timeout swaps. ExternalCI chưa khẳng định green. Chưa array-loop JIT/packed storage. [Report/raw](/and1truong/phpun/blob/ba5cc4f91473/bench/PARITY92_FOLLOWUP.md).

**#253 / PR #257:** shared bfind dùng existinglibc memchr cho bounded first-byte scan, giữ complete-needle/binary/from/conversion/count/diagnostic guards vàworst-caseO(n*m). Seven profiler-off alternating pairs: strings180.480→155.416ms (−13.9%),long826.149→714.644 (−13.5%); app+1.6% giữ. Allocation requests unchanged. Workspace/fm/clippy,25+19+extendedbyte/VMprofile qua;all3406individualPHPTstatuses identical parent. Existing string unit ASan qua,leakcheck disabled;không whole-runtime ASan claim. SeparateCPU3149/2704samples,zero lost; closure self includes semantic work,libc callee unresolved. [Report/raw](/and1truong/phpun/blob/d3aaf1a7f3f3/bench/PARITY92_FOLLOWUP.md). PR trên branch57; chưa suite parity.

**#255 / PR #259:** positional builtin display args clone trực tiếp Cells, bỏ fixed-slot scratch buffer/double clone; named binding/hole/default/ref/error frames giữ canonical. Stringsphysicalrequests2,225,333→2,144,433 (−80,900/~3.6%),bytes−3.8%. Sevenpairsstrings−3.6%/long−2.7%;31followupdefault−1.9%. App+1.9%initial/+2.2%followup giữ; object requestsunchanged nên không object attribution. Workspace/fm/clippy,25+19+VMprofile qua; all3406individualPHPTstatuses identicalparent. ExternalCI chưa khẳng địnhgreen. #237 experiment closed; fullLazyTrace chưa triển khai. [Report/raw](/and1truong/phpun/blob/2ec8cb5c91b2/bench/PARITY92_FOLLOWUP.md).

**#256 / PR #261:** private static signature view cho sharedZPP typechecks, publicownedstrict_sig/reflection API giữ nguyên. Sevenrotating PHP/main/parent/candidate: strings−7.3%,long−6.7% vsparent; physicalrequests2,144,443→1,578,563 (−26.4%),bytes−21.7%. Ninebench main6.0988×/stack4.8943×PHP, stack−19.75% vsmain. Đây là nativehost/config riêng, không gộp với user2.71×; chưa parity: objects28.07×,sieve11.35×,strings9.52×,fib0.747×PHP only. Giữ sieve+3.5%/json+1.5% negatives. Workspace/fm/clippy,25+19+VMprofile qua;3406PHPT chỉ2knownrangecrash/timeoutswaps. Release rebuild/hash verified, externalCI chưakhẳngđịnhgreen. [Report/raw/source/hashes](/and1truong/phpun/blob/bea987477363/bench/PARITY92_FOLLOWUP.md).

**#258 → PR #265:** đã merge; step đo trên parent #261, primitive single-value callback ABI; exact parameter/owner proof và canonical coercion/default/ref/context fallback. 31pairs arrays93.022→87.711ms(−5.7%);7pairs map−13%,filter−6.1%; sieve31+3.7%/app+1.6% negatives giữ. Physicalarrays requests−139919/~17.6%,bytes−10.9%. Workspace/fm/clippy,25+19+extended callback byte gates,VM-profile qua;all3406individualPHPT statuses identical parent. Không suite parity. [Report/raw](/and1truong/phpun/blob/0b205766639c/bench/PARITY92_FOLLOWUP.md).

**#260 → PR #268:** đã merge; step đo trên parent #265. Borrow fully supplied positional builtin Cells; named/default/missing/excess vẫn canonical owned binder; trace name Rc trực tiếp.7pairs strings127.854→119.961ms(−6.2%),long−8.7%;fib+2% giữ. Physicalstrings requests−241060/~15.3%,bytes−9.8%. Workspace/fm/clippy,25+19+VM-profile qua;all3406individualPHPT statuses identical parent. [Report/raw](/and1truong/phpun/blob/11170f22b42f/bench/PARITY92_FOLLOWUP.md).

**#262 → PR #269:** đã merge; step đo trên parent #268; concrete PHP methods trên root PHP class không parent/interface bỏ engine-family interception sau visibility/private resolution. Shared binder không đổi; native/inherited/interface/abstract/missing fallback.31pairs objects224.207→197.116ms(−12.1%);7pairs long−11.7%,ctor−11.2%,norm−17.6%. Physical100kobjects requests−2100000/~19.3%,bytes−5.7%. Initialarrays+2.9% giữ,31−2.6%. Workspace/fm/clippy,25+19+extended promoted/native ancestor byte gates,VM-profile qua;all3406individualPHPT statuses identical parent. SeparateCPU1965samples/zero lost, inclusive shares overlap. [Report/raw](/and1truong/phpun/blob/b3e9afa77445/bench/PARITY92_FOLLOWUP.md).

**#263 → PR #270:** đã merge; checked remainder0/-1 VM replay, bool roots immediate comparison Return, không jump vào Return/calls; integer graph reject bool callees.31pairs filter33.287→28.471ms(−14.5%);7pairs arrays−2.5%,usort−4.2%. Initialsieve+5.3%/fib+3.5% giữ,31+0.4%/−0.9%. Physicalfilter requests−19922/~7.2%,bytes−6.7%. Workspace/fm/clippy,25+19+extended modulo byte gates,VM-profile qua;3406PHPT chỉ2knownrange crash/timeout swaps. [Report/raw](/and1truong/phpun/blob/main/bench/PARITY92_FOLLOWUP.md).

**#264 → PR #273:** đã merge. SharedGC scanner bỏ cell maps chỉ khi scalar không có graph target, giữdepth-zero beforeborrow/cycle roots/edges/pins/threshold/destructor order. Objects−1.1%,longflat,JSON+0.5%,arrays+1.2% giữ: chưa target speedup đáng kể. Physical100kobjects bytes285087536→275058000(~−10MB/3.5%),requests−234;JSONrequestsunchanged. Workspace/fm/clippy,25+19+VM-profile qua;all3406individualPHPTstatuses identical parent. [Report/raw](/and1truong/phpun/blob/main/bench/PARITY92_FOLLOWUP.md).

**#266 → PR #277: đã merge.** Weak liveness giữ256tail scans/LIFO/ID reuse.31pairs objects−1.6%,long−0.2% (flat);initial+3.3%/+2.1% giữ; physicalrequests unchanged. Concurrent PHPT bug63635 timeout được giữ trong raw; serialized parent4104ms/candidate4130ms đều pass. Không broad speed claim.

**#267 → PR #279: đã merge.** Exact typed sqrt/substr dùng shared kernels, canonical arena reserve/free, fallback cho errors/profilers/types/named/override/page growth.7pairs strings−44.9%,long−46.3%,objects−6.1%/long−8.4%,fib+4.8% giữ. Physical strings−720000 allocations (−53.8%); objects100k−393663. Checks,25+19+VM-profile,3406PHPT qua so sánh từng status với known range crash/timeout và bug63635 isolation. Probe explicit-null phát hiện bug canonical có sẵn, sửa riêng #284; không ghi gate đó pass trước fix. [Report](/and1truong/phpun/blob/0439edc43772/bench/PARITY92_FOLLOWUP.md).

**#271/#272: closed/not_planned; prototype đã revert.** Borrow promoted declaration/key tác động canonical binder, trong khi VM ctor gọi store_prop.31pairs objects/long+0.7%, allocations không giảm. PR #281 chỉ lưu báo cáo (đã merge); runtime diff cuối rỗng. Private-key collision check được giữ trong #282.

**#276 → PR #282: đã merge.** Route bounded builtin names tới dispatcher gốc, giữ binder/ZPP/errors/fullscan fallback.7pairs strings−4.0%,long−4.1%,regex−11.2%;objects flat,json+0.7% giữ; allocations gần như không đổi. Checks,25+19+VM-profile,3406PHPT không thêm fail/pass/skip; chỉ known range swap.

**#278 → PR #284: đã merge, P2 resolved.** Canonical substr nullable length: omitted/null→suffix,0→empty; complete PHP byte probe qua. Reviewer P2 đã sửa bằng helper chung cho substr/mb_substr/mb_strcut/substr_replace/strspn/strcspn; sửa named $offset metadata. Publish3b668e9 và resolve thread sau checks,25+19+VM-profile/native core probe và3406individualPHPT giữ nguyên statuses. Oracle PHP8.5.11 ở host này không có mbstring: không tuyên bố byte gate native cho mb siblings.

**#280 → PR #287: đã merge.** Exact primitive property write bỏ type-name allocation, vẫn giữ int→float widening/unions/coercion/refs/errors.7pairs objects−2.1%,long−3.1%; physical100kobjects−800000 allocations (−9.6%). Checks,25+19+VM-profile và3406individual PHPT statuses khớp parent. Review fix #284 đã truyền xuống; exact reviewed head clean-built/gated với3406statuses khớp reviewed parent; timing pre-review giữ source/hash riêng.

**#283 → PR #290: đã merge.** Exact scalar replacement/ASCII kernels share VM và Cell-root entry. VM-only prototype allocation flat đã giữ raw và sửa đúng AST hot entry.31pairs strings−4.5%,long flat,JSON−1.3%,app−1.9%,Composer−1.5%;initial strings+5.9%,JSON+3.6%,app+4.3%,Composer+8.9% giữ. Target replace20000−4%,slice−9%;physicalstrings−11200requests/1.8%. Checks/clean release,25+19+VM-profile/binary/count/coercion/errors,3406PHPT only2knownrange swaps. Page-growth check caught bypass;guard fixed before final measurement.

**#285 → PR #291: mở, root hiện main.** Skip scalar decref empty tally và last-owner all-scalar dying maps; shared purple/mixed/nested/resource fullpath giữ.7pairs sieve10×100000−16.4%,default−3.6%,arrays−3.2%,JSON−2.7%,objects flat. Physicalsieve bytes105819177→78032005(−26.3%),requests−161. Initialfib+10.6%/app+2.2% giữ;31fib+3.9%,app flat. Checks/clean release/25+19+VM-profile/3406PHPT only2knownrange swaps. Ordinary merge latestmain026 đang kiểm chứng; original timings/hash pre-#289 riêng.

**Review/merge:** #246/#248/#250/#252/#254/#257/#259/#261/#265/#268/#269/#270/#273 đã merge, main3be72fe có toàn bộruntime tới65. Main sync ordinarymerge, không self-merge/force-push. CI snapshot06:14 cho246–270 rust/phpt success; các head mới kiểm riêng. Stack sau không chờ merge; body cập nhật chính, không comments. Native-host whole-suite4.894× ở #261 là snapshot với main3ea4df5, sẽ đo lại trực tiếp PHP/main mới/finalstack; không gán cho currentmain hay user2.71×.

Mục tiêu người dùng xác nhận: làm tiếp đến parity trên cùng suite/config, PR sau kế thừa branch PR trước và tự retarget main khi parent đã merge. Không tắt warning/trace/GC/memory_limit để lấy số. Các gaps chưa giải quyết: packed/mixed non-reference array storage #187, object/array physical allocation+GC, string builtin/value storage, scalar dispatch. Body này là nguồn tiến độ, không rải comments.

**PR #289 objects lane từ Devin đã merge:** compiled InitNew/NewCall, handle-drop epoch, class-name memo, method sync changes. Report140.1ms vs161ms,PHP27ms trên host khác; không gộp với native-agent timings. Globals rebind probe native/main/PR đều9. Phát hiện regression by-ref ctor riêng bên dưới.

**#292: P1 regression từ #289.** Compiled new mất caller aliases với ctor by-ref: PHP2,2,9; #2891,2,1. InitNew phải canonical fallback BEFORE evaluating argv; extend existing vm_canonical_lifetimes native check. Fix đã publish trong PR #294, stack trên #291. Native probe2,2,9; checks/release/25+19+VM-profile qua. Final integrated3406PHPT2729pass601fail, chỉ exception_005 và traits/error_007 nhập từ main fail→pass; không có new fail. #292 đóng khi merge, chưa tự đóng.

**#187 packed-storage prototype, branch76 trên75:** actual inline Vec<Value> cho zero-based scalar array_fill, keys implicit; refs/mixed/canonical Vec access materialize ordered Cells một lần. Existing memory preflight/charges giữ; fresh scalar get/store, CoW và GC phải kiểm chứng. Ticket #293; workspace tests qua, existing scalar-dimension check xác nhận inline writes/CoW vẫn packed và reference access chuyển canonical. Release/native gates/full PHPT đang chạy; chưa có số đo hay delivered packed layout claim. Sẽ chọn theo profile/Sieve và giữ negatives; không thêm algorithm recognizer.

**#294 review cập nhật:** hai P1 root NewCall/MethodCall đã fix runtimea25f253, publish18d70dc, threads resolved. Probe unset+value-write9 đã pass nhưng chưa đủ; reference rebind chứng minh PHP9/old2 cho cả constructor và direct method. Restore materialize/refresh như Op::Call, trước cả success/error propagation. Existing lifetime fixture mở rộng; reviewed checks/release/25+19+VM-profile và4extra probes pass.3406map2729pass601fail, chỉ knownrange71132crash→timeout. CI rust/phpt ở539a156 đã success; current18d70dc cần đọc riêng. Timings cache-only cũ không gồm sync restoration.

**#293 packed prototype:** pre-review-sync binary138cbf2,all3406statuses giống memory-limit-integrated;25+19/profile/native storage probe pass.7pairs Sieve73.306→63.133ms(default,−13.9%),689.098→558.643ms(10reps,−18.9%). Initialobjects+7.7%;31followup objects145.313→145.608(+0.2%),arrays+1.5%,JSON+1.7%,fib−1.1%, giữ negatives. Chưa PR: tích hợp #294 review sync rồi rebuild/retime theo parent đã sửa; không dùng số cũ thay cho reviewed source.

**#296, branch77 trên76:** empty user methods/ordinary promoted ctor hiện bị body.is_empty guard trước compiler/binder đã tồn tại. Prototype bỏ riêng guard thừa, line-zero internal/static/abstract/by-ref/generator/needs_bind giữ fallback; ONE existing receiver unit thêm inherited/default/promoted/type-error và direct-entry proof. Chưa build/gate/measure, chưa claim delivered.

## Lịch sử stack đã merge — nguồn đo riêng

#189–#204,#208/#210–#214/#216/#218 đã merge; #137/#183/#184/#185/#186/#188/#197/#206/#207/#215 closed theo bounded scope. Không coi trạng thái/ratio của main44eb376 hoặc các frozen runtime dưới đây là main hiện tại.

| PR / ticket | Implementation đã merge |
|---|---|
| #200/#201 | Stable frame shells, deferred trace context, promoted reference aliases; canonical pool/readonly fixes trong #202 |
| #202 / #207 | Shared JSON sink/borrowed keys/ASCII runs; trailing-backslash hang review c307a7c fixed/resolved |
| #203 / #188 | Direct CV/literal operands; default fib flat,sieve−7.7%; vm_exec CPU gồm semantic work |
| #204 / #185 | Capacity strings + unique-owner append; typed refs/aliases/weak-cache canonical; Value16B/Cell8B |
| #208 / #187 | Guarded current-table scalar integer reads; missing/non-scalar/globals fallback; chưa packed storage |
| #210/#212 / #206 | Writable inline args, defaults/extras/proven widening/variadic subset; snapshots/owner teardown |
| #211/#213 / #205 | CPU/heap/actual out-of-line copy lower bounds trên host riêng; requested bytes không phải bytes copy |
| #214/#216 / #187 | Hoãn metadata clone khi không có hook; rollup main31316ca;619property PHPT statuses unchanged |
| #218 / #215 | Guarded two-scalar user-sort value ABI; captured/bound/object/mixed/reference callbacks fallback |

#218 frozen92d0be0: arrays194.526→180.164ms (−7.4%),long−6.7%,usort−10.8%; physicalrequests−46.8%,bytes−36.7%,observed copy−21.9%. App31reps+2.4% với overlapping ranges; initial map+9.7% và app+12.7% giữ trong raw.583PHPT statuses identical,22VM+19workload PHP gates,checks; main integration và exact-head CI rust/phpt success, reviewer resolved. Canonical object-argument destructor/WeakReference mismatch có sẵn, object inputs không vào fast path. #214 objects−3.6%/long−2.8%,ranges overlap; startup+5.5% giữ; không broad app claim.

Full per-step configs/hashes/raw samples/negative prototypes/per-file maps: [PARITY92_FOLLOWUP](/and1truong/phpun/blob/4f1b2b3924ef/bench/PARITY92_FOLLOWUP.md), [data/92/parity](/and1truong/phpun/tree/4f1b2b3924ef/bench/data/92/parity), [PROFILE92_205](/and1truong/phpun/blob/main/bench/PROFILE92_205.md), [ROADMAP92_RESULTS](/and1truong/phpun/blob/main/bench/ROADMAP92_RESULTS.md), [FOLLOW_THROUGH92](/and1truong/phpun/blob/main/bench/FOLLOW_THROUGH92.md). Native geo9.398→8.155× của runtime15f5d86 và user baseline sau #235 là nguồn lịch sử riêng; không nhân gains/so ratio khác host. Body giữ roadmap/status; report giữ raw measurement provenance, không chuyển tiến độ sang comments.

## Đường tới ngang PHP — kế hoạch theo chi phí execution

Đích đối chứng đầu tiên: cùng workload/config với PHP 8.5.11 **không JIT**; cold CLI và warm execution báo riêng. Startup nhanh không bù cho gap execution. Mục tiêu parity là từng workload execution đạt tỷ lệ ~1× trong dispersion được công bố; geomean cả suite chỉ là chỉ số phụ. Đây là mục tiêu, chưa phải speedup được chứng minh.

Theo baseline người dùng main3a792db bên trên, parity đòi hỏi giảm khoảng **90.0%** thời gian objects (9.96×) và **80.4%** strings (5.09×) nếu thời gian PHP đối chứng giữ nguyên. Một tối ưu riêng chỉ chiếm 20% CPU, dù loại bỏ hoàn toàn, tối đa tăng tốc 1.25×. Cần xử lý nhiều lớp runtime/representation/dispatch; chưa có bằng chứng rằng chỉ thêm VM coverage hoặc JIT sẽ đủ.

### Thứ tự triển khai và tiêu chí quyết định

- [x] **R0 — khóa semantics và đo lại bottleneck trên main:** findings8–9 đã có #189, CPU/phase/paired timing đã có report#199; #205/#211/#213 đã có CPU, requested allocations và observed out-of-line copy bytes (lower bounds; inline/hidden copies chưa quan sát). Tiếp tục tách profile strings replace/concat và objects construction/norm/cleanup. Tách các đoạn thực sự trong workload; `scaled()` có khai báo nhưng không được gọi trong `40-objects.php`. Thu CPU + allocations + bytes copied; speed timings chạy profiler off, same-host paired. Báo thời gian tuyệt đối và ratios cho mỗi head.
- [x] **R1 — bounded shared string runtime:** replacement/search/borrowed concat đã merge #191/#193/#199; capacity/unique-owner append đã triển khai/gated #204. Baseline5b227c0: `str_replace` materialize subject bytes, `str_replace_one` copy subject rồi scan count và replace riêng. Ưu tiên borrow đầu vào, giữ shared subject khi không thay đổi, kết hợp count/output trong một pass cho subset hợp lệ, tránh copy trung gian. Baseline `PhpStr` dùng `Rc<[u8]>`; #204 chuyển Rc<Vec<u8>> và proved-owner append sau scaling attribution, còn một Rc shell/append. Không gọi memory-accounting “grow” là bằng chứng payload được append tại chỗ. Giữ binary bytes, aliases/CoW, warning/conversion order, count và memory_limit.
- [ ] **R2 — object construction/layout/calls:** #198 đã đưa fixed by-value/literal-default promoted constructors vào slot binder với canonical property stores; #194/#196 giảm shared helper costs, #195 cache resolved property keys; #201 giữ promoted reference aliases; #214/#216 hoãn clone property metadata tới khi có hook. Named/ref/variadic/default-expr vẫn canonical; stable offsets/broad method ICs chưa triển khai. Property storage hiện là `HashMap<String, Cell>`. Đo rồi triển khai stable declared-property slots và guarded class/method/property caches nếu lookup/allocation chiếm CPU; dynamic/magic/visibility/typed-ref/readonly cases giữ canonical fallback. Constructor fast path phải giữ promotion, coercion, errors và lifetime, không chỉ compile body rỗng.
- [ ] **R3 — arrays/JSON representation:** `PhpArray.entries` hiện là `Vec<(ArrKey, Cell)>`, `Cell = Rc<RefCell<Value>>`; flag `packed` dùng cho accounting không tự tạo packed inline storage. #202 đã merge JSON stream encode/ASCII decode scan và #208 guarded scalar dim reads từ CV slots; actual packed/mixed inline storage và slot-offset layout chưa triển khai. Đo per-element allocation và nested JSON conversion; nếu trội, triển khai actual packed/mixed storage với inline non-reference values và promotion khi cần alias. Không thêm reference cells cho mọi scalar mặc định; giữ foreach refs/cursors, key order, CoW, destructor/GC và memory accounting.
- [ ] **R4 — VM hot path và frame ABI:** #190 đã có readonly-scalar value-argument subset với lazy cell promotion/introspection snapshots; #210/#212 đã mở rộng writable direct-call/default/extras/proven coercion/untyped variadic gates; #218 bổ sung proven scalar user-sort callback pairs, canonical fallbacks tạo Cells trực tiếp; #200/#201 stable pooled frames, deferred trace context và promoted reference aliases; #203 literal CV operands; map/filter chưa dùng value ABI vì prototype map regressed. Canonical-bound hybrids/unresolved/general calls vẫn có thể dùng cells. Local `Slot::V` đã inline, không mở lại task chung “inline scalar Value”. Mục tiêu tiếp theo là giảm canonical AST re-entry, materialization arg cells và dispatch trong hot calls/loops theo profile. Dùng direct slot operations/call frames và guarded fast binding khi semantics cho phép; actual native CPU share và allocation cost phải giảm, entry counters không đủ.
- [ ] **R5 — JIT/native tier nếu dispatch còn trội:** sau R1–R4, nếu profile cho thấy scalar loops/calls còn bị opcode dispatch chi phối, thử native tier có guards/deoptimization về canonical path. Đo compile cost riêng và warm/cold; không cam kết JIT sẽ sửa string search, object allocation hay JSON copy. PHP không JIT là parity gate đầu; PHP OPcache/JIT và production app là đối chứng riêng.

Các bước trên là ứng viên kiến trúc cần kiểm chứng, không phải yêu cầu rewrite toàn runtime một lần. Mỗi PR sửa một nguồn chi phí chung, có PHP oracle gates, trước/sau trên workload tổng hợp và application-shaped cases; không chỉ tối ưu checksum hoặc pattern của bench. Sau mỗi bước, cập nhật body và chọn bước kế tiếp theo CPU còn lại. Không cam kết số PR hoặc thời hạn đạt parity khi chưa có profile main và prototype representation mới.

## Đối chiếu đề xuất 6 hướng — đề xuất tại main5b227c0, trạng thái cập nhật

| Hướng | Hiện trạng / phần còn lại |
|---|---|
| 1. Methods/ctors VM + IC | #134/#179 đã merged; eligible promoted constructors/value ABI và scoped method/property caches đã có. AST/canonical bridges vẫn tốn chi phí. #335/PR339 chia sẻ order; #347/PR349 giữ float inline. Đo constructor/dispatch riêng; magic/dynamic/visibility/ref/readonly/hook giữ fallback. |
| 2. Bỏ arg cells | Value ABI/native scalar tier đã triển khai; fib hiện dưới1×PHP trên host này. Residual AST sends còn tạo cells. #348 tái dùng pool hiện có. vm_run chuyển vector vào frame và clone handles cho slots, không clone nguyênVec mỗicall; pooling không loại Rc/RefCell/alias/arena/trace. |
| 3. Lazy TraceFrame | args là Vec<Cell>, không phải Vec<Value>; deferred args_frame và pooled context đã triển khai. Compact/lazy context phải giữ snapshot trước teardown. “92 readers” chưa kiểmđếm lại, không phải CPU evidence. |
| 4. Strings | #138 chia sẻ metadataRc<str>, không phải literal/key interning. #204 payloadRc<Vec<u8>>. Shared substring/replace/unique append đã có; string copy/search/call overhead còn lại cần đo. mb chỉ ưu tiên nếu workload dùng (20-strings không dùng). #342 giảm804605alloc nhưng31strings vẫn+3.4%; không suy allocation savings thành speed gain. |
| 5. Property/dim IC + storage | Stable declared-slot caches đã có; #187 mixed/packed storage vẫn mở. Offset phải validate key/storage generation sau unset/sort/splice/unshift/CoW, giữ iterator/ref semantics. #347 lazyfloat là bounded groundwork, longobject gain chưa chứng minh. |
| 6. Register/native tier | #338/PR341 portable typed region, #340/PR343 native private-buffer region với rollback/W^X/8MiBbudget đã có; sieve~1.3×PHP. Full register VM/JIT còn lại phải được quyết định bằng workload evidence. Native tier không tự loại allocation/search/copies. |

Ưu tiên hiện tại: objects (~12×PHP, long~20×) và strings (~4×); đo residual ASTconstructor/argument/lookup trước, tách từng thay đổi để reject regression. Historical#137 findings8–9 đã xử lý theo bounded scope. Không gộp rewrite trace vào call optimization khi chưa có attribution.

Semantic gates: không dùng func_get_args/by-ref chưa đủ; nested backtrace/errors quan sát ancestor args. Giữ param overwrite, extras/named/coercion, escaped references, destructor/error/unwind ordering, arena accounting và call-site context; lazy argument view hoặc canonical fallback phải snapshot trước khi state mất.

“1–5 về1–3×” và “vượtPHP bắt buộc6” là hypotheses, không phải trần đã chứng minh. Không dùng call_trace reader count/ticket count làm CPU attribution, không nhân các cohort speedup.

Historical source5b227c0: [core source](/and1truong/phpun/tree/5b227c0/crates/phpun-core/src);vm.rs2264/1208(direct ABI/movedargs),calls.rs2760(deferredargs),value.rs2339(TraceFrame).

## Tickets triển khai parity — 2026-10-10

- [x] #183 — direct value ABI for eligible scalar calls without arg cells
- [x] #184 — measure eager context and defer full trace rendering
- [x] #185 — remove replace copy/scan overhead and profile concat growth
- [x] #186 — promoted constructor fast path and guarded method dispatch
- [ ] #187 — guarded property/dimension caches and packed-value prototype; subset đã merge: resolved property-key cache #195 + guarded scalar dim reads #208; packed storage/layout chưa triển khai
- [x] #188 — profile dispatch and prototype register/native execution tier

Thứ tự thực thi: #137 findings 8–9 → #183 direct value ABI → đo #184 eager trace context (PR riêng nếu có benefit) → #185 strings → #186 objects → #187 storage/caches → #188 quyết định execution tier. #184/#187/#188 có gates continue/defer bằng evidence; không hứa rewrite/JIT khi chưa đo. Không chờ merge PR nền; các implementation PR tiếp theo stack theo branch cha.

## Trạng thái sau merge — lịch sử 2026-10-10 08:58 UTC

#177–#181 merged; #134/#135/#136/#138/#139/#140 closed/completed. #137 findings8–9 còn mở ở mốc này, về sau #189 đã merge và #137 closed. Main5b227c05c5521cbb0f8ecd800d38ed8ca0d7b952 (merge181) rust/phptCI success: [workflow](/and1truong/phpun/actions/runs/38039462257).

| PR | Trạng thái | Head cuối | Thay đổi / review fix | CI |
|---|---|---|---|---|
| #177 | merged | `79cc90c` | Lexical arrow imports; HashSet dedup giữ thứ tự | [success](/and1truong/phpun/actions/runs/38038706945) |
| #178 | merged | `b82127b` | Scalar closure VM calls; shared by-value cells copy trước trace/coercion | [success](/and1truong/phpun/actions/runs/38038770501) |
| #179 | merged | `ce31199` | Plain property reads/class context; sync parent, không có finding riêng | [success](/and1truong/phpun/actions/runs/38038802192) |
| #180 | merged | `c68e816` | Integer-key absence proof; unshift numeric renumber/cursor đúng | [success](/and1truong/phpun/actions/runs/38038859245) |
| #181 | merged | `328ff09` | Compiled foreach bodies; collect key/value/ref/destructuring/body assignments | [success](/and1truong/phpun/actions/runs/38039423075) |

Review4/4findings fixed/resolved, propagated to every child; #179 had no separate finding. Verified runtime8d06bf848f55e206f7b1ba30b073a1ccd0f77875 and rebased main328ff09 share tree7509b13bd2616a6423999c0657606e2d02839970 (source equivalence, no new timing).

fmt/workspace/clippy and all5PR/mainCI pass;21VM fixtures exit/stdout/stderr==PHP8.5.11-n,19workload+coverage/CPU-classifier probes pass. Generated4096-name imports check preserves order/uniqueness/exclusion. Native checks: by-value isolation/coercion array_walk/unpack/call_user_func_array closures/named funcs; negative/sparse unshift aliases/CoW; foreach delayed-CV key/value/ref/destructuring/body locals.2131individualPHPT vs0fe4140: no new regression.

| suite | total | pass before→after | existing fail | skip | unsupported | crash | timeout |
|---|---:|---:|---:|---:|---:|---:|---:|
| control/array |1232|858→862|349|7|5|7|2|
| binding |709|636→636|62|2|9|0|0|
| GC/exception/backtrace |190|116→116|68|1|5|0|0|

Only array_unshift_basic2/variation3/variation4/variation5 fail→pass. Existing crashes/timeouts not fixed. Baseline/after serialized, cutoff30s/lifetime60s,4workers.

Pre-review same-host dd99cac→0fe4140,7alternating release pairs,profiler off,all native byte gates:arrays1194.238→234.631ms(−80.4%);objects546.384→474.854(−13.1%);captured/scalar closures244.870→155.713 /222.940→121.002(−36.4/−45.7%);separate foreach100k63.107→50.102(−20.6%).Full9CLI+Doctrine/Symfony+focused probes oracle pass. Small cold-app ranges overlap; no generic-app win. Composer31pair near-flat; rejected/noisy data retained.

[Report/raw](/and1truong/phpun/blob/75f2a1c/bench/FOLLOW_THROUGH92.md),[RESULTS](/and1truong/phpun/blob/75f2a1c/bench/RESULTS.md).0fe4140 runtime differs from report-only75f2a1c; hashes/config/source retained. Frozen308cc05 below is separate history; no assigning old timings to new main/production. Native coverage/generators/storage/JIT/productionPHP-FPM/OPcache/worker isolation remain gated by the roadmap; CPU/top-three/stack/arrays attribution completed at this historical scope.

## Lịch sử đồng bộ stack — 2026-10-10

Các PR #144–#173 đã merged; conflicts/review fixes được truyền xuống stack bằng ordinary merges, không force-push. Exact-head CI và sync history nằm trong mỗi PR; #164 giữ main trace pooling + lazy args, #169 reset metadata/bound pool64; readonly/global/weak/lifetime fixes giữ chung. Runtime cuối cbe9434 có crate source cùng tip #173 b14bad5; benchmark308cc05 trước sync không được gán cho head này.

Đối chiếu3.841PHPT: core1.467 và coverage/string2.184 mọi status khớp; GC/exception190 có gc_045 vốn fail thành timeout30s. Recheck60s cả main30.148s/stack32.955s lại fail có sẵn; không coi timeout là pass. Các fail/crash/timeout lịch sử vẫn được giữ; chưa compatibility/parity claim.

## Stack đã triển khai và kiểm chứng — 2026-10-10

Không chờ merge để làm PR tiếp theo. Mỗi PR dựa trên branch của PR trước; không force-push lịch sử đã publish và không tự merge.

| PR | Base | Thay đổi | Trạng thái |
|---|---|---|---|
| #144 | main | Gate mọi rep; median/dispersion, hash/config/provenance | merged |
| #145 | branch #144 | Destructor overwrite local; compile-bail scope-sensitive builtins | merged |
| #146 | branch #145 | Resolve callee trước args; call-site resolution snapshot | merged |
| #147 | branch #146 | Canonical by-ref SEND/binder, variadic refs và reference-return | merged |
| #148 | branch #147 | Move argument vectors; gate profiler khi tắt | merged |
| #149 | branch #148 | Shared immutable declaration/frame/trace metadata Rc<str> | merged |
| #150 | branch #149 | Namespace user function trước global fallback, dùng chung resolver | merged |
| #151 | branch #150 | Fallback reasons và actual VM-body entry counters | merged |
| #152 | branch #151 | Uninitialized local khác null; import by-ref-created cells | merged |
| #154 | branch #152 | Method declaration identity ổn định cho dispatch/callable/reflection | merged |
| #156 | branch #154 | Hybrid canonical expressions; typed refs và return/destructor lifetime | merged |
| #157 | branch #156 | HTTP status/header/full-body gates; latency, worker policy, cleanup | merged |
| #158 | branch #157 | Borrow bytes trong strlen/substr/substr_count | merged |
| #159 | branch #158 | Canonical foreach/switch/try/finally và scope statements trong VM | merged |
| #160 | branch #159 | Shared statement guards; bỏ bookkeeping aliases khi AST chạy | merged |
| #163 | branch #160 | Top-level native loops giữ globals/include/declaration identity | merged |
| #164 | branch #163 | Lazy trace argument vectors, snapshot trước unwind | merged |
| #165 | branch #164 | Bounded scalar argument-cell pool khi không còn strong/weak aliases | merged |
| #166 | branch #165 | Phase/scaling/app probes và lifecycle/layout example | merged |
| #168 | branch #166 | Pool arena-site token sau khi charges được hoàn trả và destructor xong | merged |
| #169 | branch #168 | Reset deferred args metadata và bound trace pool 64; integration đã truyền từ #164 | merged |
| #170 | branch #169 | Bỏ receiver/capture/vars owners trước khi pool VM frame; GC oracle | merged |
| #171 | branch #170 | Refresh canonical globals sau direct calls, kể cả unset/rebind | merged |
| #173 | branch #171 | Report cuối, raw samples, PHP config và real-library oracle gates | merged |

PR #169 là điểm tích hợp `main @ ff93d67`, gồm mười PR đầu đã merged và trace pooling #143. Các branch được kiểm chứng trên exact commit; không suy trạng thái merged từ thứ tự stack.

- Snapshot bắt đầu: `7468797e0e80664fb661e8b2aa5aaef776ad57b7`.
- Main tích hợp dùng làm đối chứng: `ff93d67ee161e28c92e64963e8dc4b1c41a5f090`; đây là pin, không phải khẳng định main HEAD tương lai.
- Runtime cuối được đo: `308cc05345fb2a00991bebf5620769e3361eeebf` (#171).
- Helper/report commit: `1b163b870a8d004a3e422fe69cc818d4614b8015` (#173); helper thay đổi không phải runtime build provenance.
- Build sạch: Rust 1.99.0, release, LTO=true, codegen-units=1. Host Linux 6.18.44 x86_64/glibc 2.41, 5 logical CPUs.
- Oracle PHP 8.5.11, `-n`, không ini, OPcache CLI tắt, JIT disabled. Phase/real-app oracle có thêm mbstring 8.5.11; module hash/config được lưu, không dùng polyfill.

Report đầy đủ: [kết quả stack](/and1truong/phpun/blob/1b163b870a8d/bench/ROADMAP92_RESULTS.md), [CLI rolling baseline](/and1truong/phpun/blob/1b163b870a8d/bench/RESULTS.md), [HTTP](/and1truong/phpun/blob/1b163b870a8d/bench/HTTP_RESULTS.md), [raw samples/provenance](/and1truong/phpun/blob/1b163b870a8d/bench/data/92).

## Kết quả đo lịch sử — frozen runtime 308cc05

Bảng sau là **same-host paired before/after**, 7 reps, xen kẽ thứ tự runtimes, profiler tắt, mỗi rep phải exit 0 và stdout/stderr khớp PHP. Metric gồm cold startup + parse + execution. Dispersion và mọi raw sample nằm trong report.

| Workload | Initial median ms | Candidate median ms | Thay đổi wall time |
|---|---:|---:|---:|
| startup | 4.251 | 4.050 | −4.7% |
| fib | 275.772 | 209.192 | −24.1% |
| sieve | 274.448 | 232.456 | −15.3% |
| strings | 856.917 | 605.116 | −29.4% |
| arrays | 1073.667 | 1118.426 | **+4.2%** |
| objects | 832.142 | 510.838 | −38.6% |
| regex | 40.362 | 39.662 | −1.7% |
| JSON | 915.847 | 874.133 | −4.6% |
| DB | 131.923 | 131.486 | −0.3% |

Geometric mean candidate/initial wall time: **0.8604**; candidate/main tích hợp: **0.9282**. Không phải mọi workload đều nhanh hơn. Arrays tăng **5.3%** so với main tích hợp; phase probes riêng chưa tái hiện mức tăng của workload tổng hợp, nên chưa quy nguyên nhân cho callback hay một opcode cụ thể.

Phép đối chứng PHP riêng trong CLI report đạt **9/9 gates**, geometric mean phpun/PHP **13.16×**. PHP startup ở cấu hình này khoảng 3.7 ms, khác ~24 ms của report lịch sử; **không so ratio mới/cũ như speedup do code**.

- 28 phase/scaling/application cases, mỗi case 7 paired reps, đều qua oracle. Typed/untyped fib(29), hai iterations, giảm khoảng 18–20% so với main tích hợp.
- Composer autoload: 6.246 → 7.497 ms (**+20.0%, khoảng +1.25 ms**); Doctrine Inflector: 45.577 → 45.225 ms; Symfony Console: 83.491 → 88.662 ms (**+6.2%**). Đây là cold library demos, chưa đại diện production framework.
- Fib allocation requests: 4,341,657 → 413,992; strings cumulative requested bytes: 10,059,963,421 → 2,843,897,395. Đây là cumulative requests, **không phải peak live memory**.
- Final VM-body entries: fib 392,835; arrays 350,351; objects 50,000, trong đó hybrid 25,000. Sieve/strings/arrays/objects/regex/JSON/DB có top-level VM entry. Counts không phải CPU-time share; hybrid vẫn gọi canonical AST handlers cho construct phức tạp.

## Verification gates đã hoàn tất — frozen runtime 308cc05

- [x] `cargo fmt --all -- --check`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` trên exact runtime commit.
- [x] Tất cả VM oracle fixtures: exit/stdout/stderr; side effects, refs, destructors, call resolution và unwind.
- [x] Benchmark, HTTP và VM-diagnostic self-checks; 18 workload/application smoke oracle checks.
- [x] CI của #173 **success** trên report/helper head `1b163b8`: [workflow run](/and1truong/phpun/actions/runs/38016710182).
- [x] CLI 9/9, 7 reps; HTTP 4 configs, 10,000 requests/rep, 3 reps, concurrency 8; đầy đủ status/Content-Type/body gates.
- [x] **3.841 PHPT riêng biệt, mọi trạng thái cuối khớp baseline main đã pin**. Đây là regression gate, không phải tuyên bố mọi test pass.

| Suite | Total | Pass | Fail có sẵn | Skip | Unsupported | Crash có sẵn | Timeout có sẵn |
|---|---:|---:|---:|---:|---:|---:|---:|
| Core language/functions/types/closures/classes/traits/objects | 1467 | 1343 | 95 | 14 | 15 | 0 | 0 |
| Coverage bổ sung + standard array/string | 2184 | 1433 | 667 | 53 | 24 | 7 | 0 |
| GC/exception/backtrace | 190 | 115 | 68 | 1 | 5 | 0 | 1 |

Không có status change mới. Hai timeout GC phát sinh khi chạy đồng thời với build đã được kiểm tra lại riêng và **rerun cả 190 tests theo thứ tự ở cùng timeout 30s**; mọi trạng thái trở lại baseline. [Validation data](/and1truong/phpun/blob/1b163b870a8d/bench/data/92/validation.json) giữ summaries, fail/crash/timeout lists, raw-run hashes và ghi nhận lần chạy nhiễu. Bộ core dùng timeout 10s; coverage/lifetime 30s.

## Kiến trúc và thứ tự roadmap

Nền tảng:#125 VM/AST;#126 Int/direct-call/pools;#129 typed gates;#130 short-circuit/CV/ref/call;#133 scalar/dtor/profiler.

Thứ tự vẫn hợp lý: **measurement → semantic debt → measured call/copy costs → coverage dựa trên workload → redesign có bằng chứng**. Không lấy fallback hay checksum cuối làm chứng minh compiled path đúng. Coverage mới giữ canonical semantics hoặc thu hẹp subset khi chưa bảo đảm.

### Track B — Semantic debt (#137)

- [x] VM semantic gates: destructor/decref/uninitialized values;callee-before-args snapshots;canonical SEND/binder/ref-return/defaults/extras/named args;scope-introspection bail;typed stores/return/exception/receiver/capture teardown;array cursor/ref/body-binding;top-levelGLOBALSunset/rebind/aliases. #137findings8–9 fixed/verified in merged#189;broader namedZPP coverage not complete. Relevant oracle suites retained;see linked immutable follow-through raw/provenance.

### Track C — Chi phí call

- [x] Shared immutable `Rc<str>` metadata (#138); đây là shared ownership, không gọi là interning.
- [x] Move vectors, eager trace-frame reuse và lazy trace-arg snapshots (#139); snapshot trước unwind, không kéo dài object lifetime.
- [x] Bounded scalar argument-cell pool, chỉ reuse khi không còn strong/weak aliases; không intern mutable cells theo giá trị.
- [x] Reuse `vm_site` arena tokens sau khi charges được trả và destructor hoàn tất; tests cho return/throw/page extension/accounting.
- [x] Profiler phase counters tắt khi không bật; clock/counter runs tách khỏi speed measurements.
- [x] Before/after + allocation + call diagnostics được lưu. Fib profiler có 392,835 calls, tổng ns được normalize trong report; `exec` inclusive, không cộng phase như các chi phí độc lập.

### Track A — VM coverage

- [x] VM entry/fallback counters;canonical hybrid bridges for dimension/property/callable/interpolation/match/new/scope;foreach/switch/try/unwind;stable declarations;top-levelfor/while/do/include identity;eligible scalarclosures/thisproperty/foreach bytecode;arrow-import dedup. #203CV/#208dim subset merged. Historical sampledVM/AST/other uses weighted periods/raw stacks,includes runtime/builtins,not pureopcode CPU share.
- [ ] Expand native dimensions/properties/calls/iterator/unwind only with measured benefit and ref/CoW/visibility/destructor gates. Generators deferred,ASTfallback. Currenthost gprofng has no usable CPU samples;entry counts do not supply CPU attribution.

### Track D — Workload profiling (#140)

- [x] Arrays: append/map/**usort**/filter/string keys; không gán `array_reduce` cho benchmark không dùng reduce.
- [x] Strings: repeat/replace/slice/concat và copy/allocation; borrow read-only bytes đã giảm source copies.
- [x] Objects: ctor/norm (scaled khai báo nhưng không được gọi); layout và stable-declaration/callback diagnostics.
- [x] `11-sieve`: scaling 10k/20k/40k/80k; không dùng số lịch sử 389–535× làm bottleneck hiện tại.
- [x] Fib typed/untyped nhỏ/dài; regex/JSON/DB wrapper cases có input/argv rõ.
- [x] Composer autoload, Doctrine Inflector, Symfony Console, application-shaped CLI và HTTP request→response.
- [x] Verified sampled CPU top-three: perf 6.1.176 software cpu-clock:u, 199Hz/DWARF, arrays 1520 và objects 1093 samples, zero lost; raw symbols/stacks/provenance trong follow-through. Hardware cycles không hỗ trợ. Gprofng timer-warning data cũ vẫn bị loại, không công bố CPU% từ nó.
- [x] Arrays tổng hợp: lexical capture và gated closure binding, rồi CPU-sampled quadratic pos_of insertion; từng cặp before/after và aggregate current-main comparison trong #177–#181.
- [ ] Cold production app overhead: chỉ mở fix mới khi target workload + same-config profiling chứng minh chi phí; Composer/Doctrine/Symfony cold demos không đủ suy ra production-wide win.

### Storage redesign — conditional

- [x] Đo layout: frozen308cc05 `Value=24`, `Cell=8`, `TraceFrame=160`; merged #204 `Value=16`, `Cell=8`, `TraceFrame=160` bytes. Value đã inline Bool/Int/Float; không còn task “inline small scalars” chung chung.
- [x] Dense scalar fill/lazy promotion #299 merged;broader mixed/frame ownership#187 vẫn mở,oracle+measured-benefit required.
- [x] #245 x86_64 pure integer-function graph native tier merged.
- [ ] Broader native loops/arrays/objects/unwind chưa có;không parity claim.

## HTTP / lifecycle

Report mới dùng cùng load policy, xoay thứ tự, gate readiness/warmup/từng response, latency p50/p95/p99 và owned-process cleanup.

| Config | Median req/s [min,max] | p95 ms |
|---|---:|---:|
| PHP -S, 1 process | 4080.4 [4053.8,4390.5] | 3.409 |
| PHP -S, 8 total processes | 4019.4 [3860.9,4055.1] | 3.444 |
| phpun, fresh interpreter/request | 1120.6 [1100.4,1155.4] | 16.094 |
| phpun, 8 warm workers | 4253.6 [4005.1,4415.4] | 3.202 |

PHP WORKERS gồm serving parent + children; phpun warm WORKERS là handler threads. Các khoảng đo chồng lấn; load driver có thể giới hạn throughput. Đây là **dev-server comparison**, chưa chứng minh production PHP nhanh/chậm tương ứng.

- [x] Lifecycle release probe, 200 iterations, printed full body khớp PHP: `Interp::new≈1884.5µs`, parse≈108.8µs, bootstrap **gồm parse**≈99.0µs, reset≈0.041µs, handler≈29.94µs, end_request≈0.094µs. Các phép đo riêng không cộng dồn; persistent handler có lifecycle khác classic request.
- [x] Giữ default worker semantics hiện có; số liệu cho thấy init đáng kể, chưa đủ lý do đổi default.
- [ ] Trước khi đổi default/pool lifecycle: kiểm chứng riêng globals/statics/headers/output buffers/errors/request isolation và cleanup.
- [ ] PHP-FPM + OPcache: **conditional**, bắt buộc khi đánh giá production applications, chưa phải kết luận từ php -S.

## Measurement rules cho các PR tiếp theo

Exact binary commit/hash + dirty/build flags + host + PHP version/config/extensions/OPcache/JIT; argv và input sizes; profiler off cho timing. Giữ median/dispersion/reps và every-rep exit/stdout/stderr/full-response gates. Không trừ startup của file khác để gọi là execution time, không dùng error/mismatch runs tính speedup, không biến inclusive phase times thành additive savings. Mỗi fix có runnable check, PR coherent và cập nhật body này.

## Lịch sử được giữ riêng

Report 2026-10-09 22:19 dùng minimum/adaptive 2–5 reps, host/config khác và chưa có full provenance; [snapshot cũ](/and1truong/phpun/blob/395af109bb2f/bench/RESULTS.md). #133 báo typed fib(36) 35.6→30.5s, PHP khoảng 0.5s trên host noisy; không so với fib(26) cold CLI hoặc coi là full-suite refresh. Các phase averages/600–900ns-call từ lần đó là diagnostics theo cấu hình cụ thể, không phải trần kiến trúc hay speedup được đảm bảo.

**#286: PR #294 trên #291, đã publish.** Reuse raw-string-validated allocator cache tại statement_boundary và mutable resize. Pre-main0267pairs sieve/default và10reps đều−6.6%,JSON−8.7%,arrays flat,objects+3.4%,strings+1.6%,Composer+2.5% giữ. Checks/clean release/25+19+VM-profile/3406statuses khớp parent. Đã merge main026 và thêm fix #292; final integrated gates qua,2729pass601fail, chỉ2 imported fail→pass (exception_005/traits/error_007),không new fail. Số đo cũ giữ binary/source riêng.

## Lịch sử baseline và quyết định

Các ratio sau là **những lần đo lịch sử khác nhau**, không phải speedup so sánh trong môi trường đã kiểm soát:

Historical geomeans: v0.0.1(#90)7.6×PHP,main81915c4(#118)9.4×,report2026-10-09 22:19 6.0×. Full per-bench matrix remains in the linked immutable historical report;different configurations,not controlled speedups.

- #117 (`a82e75d`) sửa dim-write O(n²); #118 ghi sieve wall ~17.6s →14.0s trong các lần chạy được báo. Không suy tỷ lệ wall từ tỷ lệ phpun/PHP khi reference timings cũng thay đổi.
- #118 báo regression ở call-heavy workloads: fib ~0.67→1.21s, objects ~0.52→0.73s, arrays ~1.45→1.93s, json ~0.68→1.24s; manual re-runs được báo ổn định. Arg coercion/arena accounting (#116), dim operations và allocation/output-buffer metering là suspects, chưa phải causal attribution đã được profile/bisect.
- #39 là quyết định kiến trúc ban đầu; #125/#126/#129 đã đưa VM subset vào main. Giữ AST fallback làm đối chứng phụ; PHP 8.5.11 vẫn là oracle cho semantics.
- #125: untyped fib(29) ~2.26→1.17s; #126 ~0.95s, vẫn còn gap lớn so với PHP. #129 báo typed `10-fib` 18.8×→8.7×; mỗi kết quả cần giữ workload/config gốc.
- #133 bổ sung fast scalar gates và destruct early-outs; post-phase được báo ~375→150ns/call. Full-suite refresh sau merge đã được lưu trong report của stack bên trên.



## Performance follow-through — lịch sử đã merge

#144–#173,#177–#181,#189–#199 đã merge; #134–#140 và findings8–9 #137 closed. [Report/raw samples/provenance](/and1truong/phpun/blob/75f2a1c/bench/FOLLOW_THROUGH92.md) giữ các cặp 7 alternating release reps, exact PHP8.5.11 -n gates: arrays #1771142.623→580.128ms,#178579.791→463.100ms,#180457.571→225.909ms; objects #179499.817→463.964ms; foreach #18163.086→55.139ms. Không cộng speedups giữa các cặp/host. Software perf199Hz/DWARF đã chạy, không hỗ trợ hardware cycles; profile lịch sử trước #180 không phải profile main hiện tại. Generators/native iterators, storage/JIT, FPM/isolation vẫn theo roadmap riêng.

