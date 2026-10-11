import os,json,pathlib,platform,hashlib,subprocess,statistics,math,sys
sys.path.insert(0,'/workspace/phpun/bench')
from measure import capture
assert not any(os.environ.get(k) is not None for k in ['PHPUN_ALLOC','PHPUN_CALLPROF','PHPUN_VMPROF'])
names=['php','main','parent','candidate'];bins=['/workspace/php85','/workspace/parity92/oct11/native-fixed','/workspace/parity92/oct11/positional-builtin-traces','/workspace/parity92/oct11/static-builtin-signatures'];cmds=[[bins[0],'-n']]+[[b] for b in bins[1:]]
report={'metric':'cold CLI parse+execution; 7 rotating-order reps; profilers off','host':platform.platform(),'source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'runtime':'556d7e6898f99e79880aa460eb046ed23adf41c7','main_tip':'3ea4df564be132ffda049875bb72a36be8647e3c','main_frozen_runtime':'be0e8103d9eeac7dcfab65f82148a567e0737723','main_note':'no tracked crates/Cargo difference from native-fixed runtime; main merges metadata/report only','tracked_clean':'only untracked vendor symlink','release_rebuild':'cargo clean --offline --release -p phpun-core; cargo build --offline --release','build':'rust1.99.0 release LTO=true codegen-units=1','php_config':{'args':['-n'],'settings':json.loads(subprocess.check_output([bins[0],'-n','-r','echo json_encode(["version"=>PHP_VERSION,"opcache_loaded"=>extension_loaded("Zend OPcache"),"enable_cli"=>ini_get("opcache.enable_cli"),"jit"=>ini_get("opcache.jit"),"extensions"=>get_loaded_extensions()]);']))},'sha256':{b:hashlib.sha256(pathlib.Path(b).read_bytes()).hexdigest() for b in bins},'cases':[]}
save=pathlib.Path('/workspace/parity92/oct11/static-builtin-signatures-four-timings.json')
cases=[[str(p)] for p in sorted(pathlib.Path('bench').glob('[0-9]*.php'))]+[['bench/20-strings.php','100'],['bench/40-objects.php','20','5000'],['bench/app/cli.php','100','20'],['examples/composer/run.php']]
for case in cases:
 samples=[[] for _ in bins];oracle=capture(cmds[0]+case,300);assert oracle[1]==0,(case,oracle[1:])
 for rep in range(7):
  for i in [(rep+j)%len(bins) for j in range(len(bins))]:
   r=capture(cmds[i]+case,300);assert r[1:]==oracle[1:],(case,i,r[1:],oracle[1:]);samples[i].append(r[0])
 med=[statistics.median(x) for x in samples];row={'argv':case,'samples_ms':dict(zip(names,samples)),'medians_ms':dict(zip(names,med)),'main_php_ratio':med[1]/med[0],'candidate_php_ratio':med[3]/med[0],'candidate_main_ratio':med[3]/med[1],'candidate_parent_ratio':med[3]/med[2],'gate':'every exit/stdout/stderr == PHP8.5.11 -n'};report['cases'].append(row)
 if len(report['cases'])==9:report['nine_bench_geomean']={key:math.exp(statistics.mean(math.log(x[key]) for x in report['cases'])) for key in ['main_php_ratio','candidate_php_ratio','candidate_main_ratio','candidate_parent_ratio']}
 save.write_text(json.dumps(report,indent=2)+'\n');print(case,med,'candidate/PHP',row['candidate_php_ratio'],'candidate/parent',row['candidate_parent_ratio'],flush=True)
