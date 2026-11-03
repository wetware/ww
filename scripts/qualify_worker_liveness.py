#!/usr/bin/env python3
"""One bounded campaign. Criteria are fixed before any process executes."""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import statistics
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / '.context/service-performance'
PLAN = ROOT / 'doc/measurements/worker-liveness-service-plan.md'
ORDERS = [('baseline', 'candidate', 'candidate', 'baseline'),
          ('candidate', 'baseline', 'baseline', 'candidate'),
          ('baseline', 'candidate', 'candidate', 'baseline')]

def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def command(args):
    return subprocess.check_output(args, text=True, timeout=10).strip()

def percentile(values, quantile):
    ordered = sorted(values)
    assert ordered
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]

def describe(rows):
    """Process summaries; no pooling across independent processes."""
    summary = {}
    for workers in (1, 4):
        selected = [r for r in rows if r.get('workers') == workers]
        assert [r['compute_grants'] for r in selected if r['case'] == 'preflight'][0] > workers
        def add(name, value):
            assert math.isfinite(value) and value >= 0, (name, value)
            summary[f'{workers}/{name}'] = value
        for case in ('cell_startup', 'short_batch', 'cooperative'):
            group = [r for r in selected if r['case'] == case]
            assert len(group) == (1 if case == 'cooperative' else 7)
            cells = [c for r in group for c in r['cells']]
            assert sum(r['operations'] for r in group) == len(cells)
            if case == 'short_batch':
                assert all(r['operations'] == 256 for r in group)
            for field in ('built_ns', 'completed_ns'):
                values = [c[field] for c in cells]
                add(f'{case}/{field}/median', statistics.median(values))
                add(f'{case}/{field}/p95', percentile(values, .95))
            add(f'{case}/ns_per_cell', sum(r['elapsed_ns'] for r in group) / len(cells))
            add(f'{case}/cpu_seconds_per_cell', sum(r['cpu_seconds'] for r in group) / len(cells))
        for case in ('pool_creation', 'idle_shutdown', 'pending_shutdown', 'workload_shutdown', 'io_shutdown'):
            group = [r['elapsed_ns'] for r in selected if r['case'] == case]
            assert len(group) == (1 if case in ('workload_shutdown', 'io_shutdown') else 7)
            add(f'{case}/median', statistics.median(group))
            add(f'{case}/p95', percentile(group, .95))
        io = [r for r in selected if r['case'] == 'pending_io']
        assert len(io) == 1 and len(io[0]['cells']) == workers
        io = io[0]
        assert io['elapsed_ns'] >= 5e9
        add('pending_io/cpu_fraction', io['cpu_seconds'] / (io['elapsed_ns'] / 1e9))
        add('pending_io/lateness_ns/median', statistics.median(io['lateness_ns']))
        add('pending_io/lateness_ns/p95', percentile(io['lateness_ns'], .95))
    return summary

def allowance(key, baseline):
    case, *parts = key.split('/')[1:]
    metric = '/'.join(parts)
    if case in ('cell_startup', 'short_batch', 'cooperative'):
        if metric.endswith('/median'):
            return max(.05 * baseline, 10_000.)
        if metric.endswith('/p95'):
            return max(.10 * baseline, 50_000.)
        if metric == 'ns_per_cell':
            return .05 * baseline if case == 'cooperative' else max(.05 * baseline, 10_000.)
        if metric == 'cpu_seconds_per_cell' and case == 'cooperative':
            return .05 * baseline
        return None  # short/startup CPU descriptive only
    if case == 'pool_creation':
        return max(.05 * baseline, 100_000.) if metric == 'median' else None
    if 'shutdown' in case:
        return max(.1 * baseline, 1e6 if metric == 'median' else 5e6)
    if metric == 'cpu_fraction':
        return .005
    if metric == 'lateness_ns/median':
        return 1e6
    if metric == 'lateness_ns/p95':
        return 5e6
    raise AssertionError(key)

def geomean(a, b):
    # The frozen plan uses geometric means, including zero-valued summaries.
    return math.sqrt(a*b)

def evaluate(processes, valid):
    if len(processes) != 12:
        return {'overall': 'INCONCLUSIVE', 'reason': 'Incomplete fixed campaign', 'metrics': {}}
    keys = sorted(processes[0]['summary'])
    metrics = {}
    for key in keys:
        blocks = []
        for block in range(3):
            groups = {label: [p['summary'][key] for p in processes if p['block'] == block and p['label'] == label]
                      for label in ('baseline', 'candidate')}
            assert all(len(v) == 2 for v in groups.values())
            baseline, candidate = [geomean(*groups[label]) for label in ('baseline', 'candidate')]
            budget = allowance(key, baseline)
            blocks.append({'baseline': baseline, 'candidate': candidate,
                           'delta': candidate-baseline,
                           'ratio': candidate/baseline if baseline else None,
                           'allowance': budget,
                           'baseline_control_difference': abs(groups['baseline'][0]-groups['baseline'][1]),
                           'candidate_control_difference': abs(groups['candidate'][0]-groups['candidate'][1])})
        if blocks[0]['allowance'] is None:
            verdict = 'DESCRIPTIVE'
            stable = None
        else:
            half = [max(b['baseline_control_difference'], b['candidate_control_difference']) <= b['allowance']/2 for b in blocks]
            full = [max(b['baseline_control_difference'], b['candidate_control_difference']) <= b['allowance'] for b in blocks]
            stable = sum(half) >= 2 and all(full)
            exceeded = [b['delta'] > b['allowance'] for b in blocks]
            verdict = ('PASS' if not any(exceeded) else 'CONFIRMED REGRESSION' if all(exceeded) else 'INCONCLUSIVE') if valid and stable else 'INCONCLUSIVE'
        metrics[key] = {'verdict': verdict, 'stable_controls': stable, 'blocks': blocks}
    verdicts = {v['verdict'] for v in metrics.values()}
    overall = 'CONFIRMED REGRESSION' if 'CONFIRMED REGRESSION' in verdicts else 'INCONCLUSIVE' if 'INCONCLUSIVE' in verdicts else 'PASS'
    return {'overall': overall, 'metrics': metrics}

def run(args):
    OUT.mkdir(exist_ok=True)
    output = OUT / 'results.json'
    paths = {label: Path(getattr(args, label)).resolve() for label in ('baseline','candidate')}
    lock_path = Path('/tmp/ww-fuel-coordination/build-benchmark.lock')
    lock_path.parent.mkdir(exist_ok=True)
    with lock_path.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if output.exists() or (OUT/'environment.jsonl').exists():
            raise SystemExit('Campaign already recorded; refusing to overwrite or repeat timing.')
        before = {label: sha(path) for label,path in paths.items()}
        receipts = json.loads(Path(args.receipt).read_text())
        for label in paths:
            receipt = receipts[label]
            assert receipt['binary_sha256'] == before[label], 'binary build receipt mismatch'
            assert receipt['harness_sha256'] == sha(ROOT/'tests/worker_liveness_service_overhead.rs'), 'compiled harness differs'
            assert receipt['fixture_sha256'] == sha(ROOT/'tests/fixtures/fuel-publication-component.wat'), 'compiled fixture differs'
            assert receipt['lockfile_sha256'] == sha(ROOT/'Cargo.lock'), 'compiled dependency lock differs'
            source_root = ROOT if label == 'candidate' else ROOT/'.context/liveness-baseline'
            assert all(sha(source_root/path) == digest for path,digest in receipt['production_sha256'].items()), 'compiled production source differs'
            assert receipt['head'] == command(['git','-C',str(source_root),'rev-parse','HEAD']), 'build commit changed'
        assert before['baseline'] != before['candidate']
        source_paths = ['tests/worker_liveness_service_overhead.rs','tests/fixtures/fuel-publication-component.wat',
                        'src/services.rs','src/services/worker_liveness.rs','src/metrics.rs','Cargo.lock']
        evidence = {'plan_sha256': sha(PLAN), 'driver_sha256': sha(__file__), 'binary_sha256': before,
                    'source_sha256': {p:sha(ROOT/p) for p in source_paths}, 'build_receipts': receipts,
                    'candidate_head': command(['git','rev-parse','HEAD']),
                    'baseline_head': command(['git','-C',str(ROOT/'.context/liveness-baseline'),'rev-parse','HEAD']),
                    'machine': command(['uname','-m']), 'rustc': command(['rustc','--version']),
                    'orders': ORDERS, 'processes': [], 'environment_valid': True,
                    'start_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())}
        power = []
        stop = threading.Event()
        def monitor():
            with (OUT/'environment.jsonl').open('x') as log:
                while not stop.is_set():
                    sample = {'elapsed': time.monotonic()-started}
                    try:
                        sample['power'] = command(['pmset','-g','batt'])
                        sample['ac'] = "Now drawing from 'AC Power'" in sample['power']
                        sample['thermal'] = command(['pmset','-g','therm'])
                        processes = command(['ps','-axo','pid,pcpu,comm'])
                        sample['processes'] = processes
                    except Exception as error:
                        sample['ac'] = False
                        sample['error'] = str(error)
                    power.append(sample)
                    log.write(json.dumps(sample)+'\n'); log.flush()
                    stop.wait(1)
        started = time.monotonic()
        watcher = threading.Thread(target=monitor,daemon=True); watcher.start()
        try:
            while not power:
                assert watcher.is_alive() and time.monotonic()-started < 35, 'power monitor startup failed'
                time.sleep(.05)
            assert power[0]['ac'], 'AC power required before measurement'
            for block, order in enumerate(ORDERS):
                for position, label in enumerate(order):
                    assert time.monotonic()-started < 25*60, 'campaign deadline'
                    assert all(p['ac'] for p in power), 'AC monitoring failed'
                    assert sha(paths[label]) == before[label], 'binary identity changed'
                    env = os.environ.copy()
                    env['WW_WORKER_SERVICE_BENCH']='1'
                    env.pop('WW_CWASM_DIR',None); env.pop('WW_CWASM_CACHE_MAX_BYTES',None)
                    cmd = [str(paths[label])]
                    if args.entry == 'test':
                        cmd += ['--exact','measure_worker_liveness_service','--ignored','--nocapture','--test-threads=1']
                    log_path = OUT/f'block-{block}-{position}-{label}.log'
                    with log_path.open('x') as log:
                        proc = subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT,env=env,start_new_session=True)
                        try:
                            code = proc.wait(timeout=min(120, 25*60-(time.monotonic()-started)))
                        except subprocess.TimeoutExpired:
                            os.killpg(proc.pid,signal.SIGKILL); proc.wait()
                            raise RuntimeError('Independent process watchdog expired')
                    rows = [json.loads(line) for line in log_path.read_text().splitlines() if line.startswith('{')]
                    assert code == 0, f'{label} exited {code}; retained {log_path}'
                    evidence['processes'].append({'block':block,'position':position,'label':label,
                                                  'rows':rows,'summary':describe(rows),'log':log_path.name})
                    output.write_text(json.dumps(evidence,indent=2)+'\n')
                    print(f'completed block={block} position={position} label={label}',flush=True)
        except Exception as error:
            evidence['environment_valid']=False
            evidence['failure']=str(error)
            print(f'campaign stopped: {error}',flush=True)
        finally:
            stop.set(); watcher.join(timeout=15)
            try:
                final_power=command(['pmset','-g','batt'])
            except Exception as error:
                final_power=''
                evidence['final_power_error']=str(error)
            evidence['power_samples']=len(power)
            evidence['all_samples_ac']=all(p['ac'] for p in power) and "Now drawing from 'AC Power'" in final_power
            evidence['binary_hashes_unchanged']=all(sha(paths[label]) == before[label] for label in paths)
            evidence['environment_valid'] &= evidence['all_samples_ac'] and evidence['binary_hashes_unchanged']
            evidence['elapsed_seconds']=time.monotonic()-started
            evidence['evaluation']=evaluate(evidence['processes'],evidence['environment_valid'])
            output.write_text(json.dumps(evidence,indent=2)+'\n')
            print('Overall:',evidence['evaluation']['overall'],flush=True)

if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--receipt',required=True)
    parser.add_argument('--baseline',required=True); parser.add_argument('--candidate',required=True)
    parser.add_argument('--entry',choices=['test','example'],default='test')
    run(parser.parse_args())
