#!/usr/bin/env python3
"""Build the paired opt-in examples; run from the repository root."""
import fcntl, hashlib, json, os, pathlib, shutil, subprocess, time
root=pathlib.Path.cwd()
(root/'.context').mkdir(exist_ok=True)
source=root/'tests/worker_liveness_service_overhead.rs'
original=source.read_text()
example='\n'.join(line.replace('//!','//',1) if line.startswith('//!') else line for line in original.splitlines() if line != '#[test]' and not line.startswith('#[ignore ='))
example=example.replace('include_str!("fixtures/', 'include_str!("../tests/fixtures/')+'\nfn main() { measure_worker_liveness_service(); }\n'
def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()
def git(path,*args): return subprocess.check_output(['git',*args],cwd=path,text=True).strip()
lock=pathlib.Path('/tmp/ww-fuel-coordination/build-benchmark.lock')
lock.parent.mkdir(parents=True,exist_ok=True)
receipt={}
command=['cargo','build','--release','--locked','--example','worker_liveness_service_measurement']
with lock.open('a') as held:
 fcntl.flock(held,fcntl.LOCK_EX)
 for name in ['service-performance-baseline','service-performance-candidate','service-performance-build-receipt.json']:
  if (root/'.context'/name).exists(): raise RuntimeError(f'refuse overwrite qualification artifact: {name}')
 (root/'.context/service-performance-example-final.rs').write_text(example)
 for label,path in [('baseline',root/'.context/liveness-baseline'),('candidate',root)]:
  target=path/'examples/worker_liveness_service_measurement.rs'
  if target.exists(): raise RuntimeError(f'refuse overwrite {target}')
  production=['src/services.rs','src/metrics.rs']
  if label=='candidate': production.append('src/services/worker_liveness.rs')
  production_hashes={p:sha(path/p) for p in production}
  head=git(path,'rev-parse','HEAD')
  assert not git(path,'diff','HEAD','--','src','crates','Cargo.toml','Cargo.lock','build.rs','capnp'), 'uncommitted production source'
  target.write_text(example)
  try:
   env=os.environ.copy(); env['CARGO_TARGET_DIR']=str(root/'target')
   log=root/f'.context/service-performance-final-build-{label}.log'
   with log.open('w') as out:
    result=subprocess.run(command,cwd=path,env=env,stdout=out,stderr=subprocess.STDOUT)
   result.check_returncode()
   assert sha(source)==hashlib.sha256(original.encode()).hexdigest(), 'harness changed during build'
   assert production_hashes=={p:sha(path/p) for p in production}, 'production changed during build'
   assert git(path,'rev-parse','HEAD')==head, 'head changed during build'
   binary=root/f'.context/service-performance-{label}'
   shutil.copy2(root/'target/release/examples/worker_liveness_service_measurement',binary)
   receipt[label]={
    'label':label,'harness_sha256':hashlib.sha256(original.encode()).hexdigest(),
    'example_sha256':hashlib.sha256(example.encode()).hexdigest(),
    'fixture_sha256':sha(path/'tests/fixtures/fuel-publication-component.wat'),
    'lockfile_sha256':sha(path/'Cargo.lock'),'production_sha256':production_hashes,
    'binary':str(binary),'binary_sha256':sha(binary),'head':head,
    'tree':git(path,'rev-parse','HEAD^{tree}'),
    'git_diff_sha256':hashlib.sha256(git(path,'diff','--binary','HEAD').encode()).hexdigest(),
    'completed_unix':time.time(),'log':str(log),'command':command,
   }
   (root/'.context/service-performance-build-receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
   print(f'{label} build complete: {binary}',flush=True)
  finally:
   target.unlink()
