import os, json, pathlib, platform, hashlib, subprocess, statistics, math, sys, argparse
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[3]))
from measure import capture
if any(os.environ.get(k) is not None for k in ['PHPUN_ALLOC','PHPUN_CALLPROF','PHPUN_VMPROF']):
    raise SystemExit('Disable profilers for speed measurements')
parser = argparse.ArgumentParser(description='Native PHP/main/stack cold CLI comparison; run from repository root.')
for option in ['php', 'main', 'stack', 'main-runtime', 'stack-runtime', 'save']:
    parser.add_argument('--' + option, required=True)
args = parser.parse_args()
bins=[str(pathlib.Path(p).resolve()) for p in [args.php, args.main, args.stack]]
cmds=[[bins[0],'-n'],[bins[1]],[bins[2]]]
report={'metric':'cold CLI parse+execution; 7 rotating-order reps; profilers off','host':platform.platform(),'main_runtime':args.main_runtime,'stack_runtime':args.stack_runtime,'stack_source':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),'dirty':bool(subprocess.check_output(['git','status','--porcelain'])),'build':'rust1.99.0 release LTO=true codegen-units=1','php_config':{'args':['-n'],'settings':json.loads(subprocess.check_output([bins[0],'-n','-r','echo json_encode(["version"=>PHP_VERSION,"debug"=>PHP_DEBUG,"zts"=>PHP_ZTS,"ini"=>php_ini_loaded_file(),"enable_cli"=>ini_get("opcache.enable_cli"),"jit_buffer_size"=>ini_get("opcache.jit_buffer_size"),"opcache_loaded"=>extension_loaded("Zend OPcache"),"jit"=>ini_get("opcache.jit"),"extensions"=>get_loaded_extensions()]);']))},'sha256':{b:hashlib.sha256(pathlib.Path(b).read_bytes()).hexdigest() for b in bins},'cases':[]}
save=pathlib.Path(args.save)
for case in [[str(p)] for p in sorted(pathlib.Path('bench').glob('[0-9]*.php'))]+[['bench/app/cli.php','100','20'],['examples/composer/run.php']]:
 samples=[[],[],[]];oracle=capture(cmds[0]+case,300)
 if oracle[1] != 0:
  raise SystemExit(f'oracle failed: {case}, exit={oracle[1]}')
 for rep in range(7):
  for i in [(rep+j)%3 for j in range(3)]:
   result=capture(cmds[i]+case,300)
   if result[1] != 0 or result[2:] != oracle[2:]:
    raise SystemExit(f'gate failed: {case}, runtime={i}, exit={result[1]} or stdout/stderr mismatch')
   samples[i].append(result[0])
 medians=[statistics.median(x) for x in samples];row={'argv':case,'php_ms':samples[0],'main_ms':samples[1],'stack_ms':samples[2],'medians_ms':dict(zip(['php','main','stack'],medians)),'main_php_ratio':medians[1]/medians[0],'stack_php_ratio':medians[2]/medians[0],'stack_main_ratio':medians[2]/medians[1],'gate':'every exit/stdout/stderr == PHP8.5.11 -n'};report['cases'].append(row)
 if len(report['cases'])==9:
  report['nine_bench_geomean']={key:math.exp(statistics.mean(math.log(x[key]) for x in report['cases'])) for key in ['main_php_ratio','stack_php_ratio','stack_main_ratio']}
 save.write_text(json.dumps(report,indent=2)+'\n');print(case,medians,'PHP ratios',row['main_php_ratio'],row['stack_php_ratio'],'stack/main',row['stack_main_ratio'],flush=True)
