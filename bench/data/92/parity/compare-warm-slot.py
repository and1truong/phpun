import hashlib,json,pathlib,statistics,subprocess,sys,os,tempfile
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[3]))
from measure import capture
bins=sys.argv[1:-1];save=pathlib.Path(sys.argv[-1]);rows={b:{'first_call_including_compile_ms':[],'warm_fib29_ms':[],'cold_cli_ms':[],'hash':hashlib.sha256(pathlib.Path(b).read_bytes()).hexdigest()} for b in bins}
if any(os.environ.get(k) for k in ('PHPUN_ALLOC','PHPUN_CALLPROF','PHPUN_VMPROF')):raise SystemExit('profiler enabled')
for rep in range(7):
 for b in (bins if rep%2==0 else bins[::-1]):
  handle,path=tempfile.mkstemp(prefix='phpun-warm-');os.close(handle);metric=pathlib.Path(path)
  cmd=[b]+(['-n'] if b==bins[0] else [])+[str(pathlib.Path(__file__).with_name('warm-slot.php')),str(metric)]
  elapsed,exit,out,err=capture(cmd,120)
  if (exit,out,err)!=(0,b'RESULT 55 1542687\n',b''):raise SystemExit((b,exit,out,err))
  first,warm=json.loads(metric.read_text());metric.unlink()
  if not 0<first<10000 or not 0<warm<30000:raise SystemExit('invalid clock sample')
  rows[b]['first_call_including_compile_ms'].append(first);rows[b]['warm_fib29_ms'].append(warm);rows[b]['cold_cli_ms'].append(elapsed)
for b,row in rows.items():
 row['medians']={k:statistics.median(v) for k,v in row.items() if isinstance(v,list)};print(b,row['medians'],flush=True)
save.write_text(json.dumps({'metric':'first call (includes VM compile, not pure compile cost); warm execution function-only; cold CLI separate','gate':'exact exit/stdout/stderr; metric file intentionally variable','profiler':'off','source_script':pathlib.Path(__file__).with_name('warm-slot.php').read_text(),'binaries':rows},indent=2)+'\n')
