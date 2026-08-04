//! Review-only production-shaped collector benchmark.

use cc_spike::cc::{collect_cycles, maybe_collect, Cc};
use cc_spike::model::{closure, define, defs, Drops, MAtom, MCapInner, MFnBody, MVal};
use std::cell::RefCell;
use std::hint::black_box;
use std::time::{Duration, Instant};

const MODULES: usize = 500;
const OPS: usize = 20_000;

fn workload() -> Duration {
    let drops = Drops::default();
    let durable = std::rc::Rc::new(vec![0x5a; 4096]);
    let mut modules = Vec::with_capacity(MODULES);

    for i in 0..MODULES {
        let owner = defs(&drops);
        let atom = Cc::new(MAtom {
            value: RefCell::new(MVal::Int(i as i64)),
            drops: drops.clone(),
        });
        let callable = closure(
            &owner,
            vec![
                ("state".into(), MVal::Atom(atom.clone())),
                ("blob".into(), MVal::Durable(durable.clone())),
            ],
            MFnBody::Raw(vec![MVal::Atom(atom.clone())]),
            &drops,
        );
        let cap = Cc::new(MCapInner {
            methods: RefCell::new(vec![("call".into(), MVal::Fn(callable.clone()))]),
            base: RefCell::new(Some(MVal::Atom(atom.clone()))),
            handler: RefCell::new(Some(MVal::Fn(callable.clone()))),
            drops: drops.clone(),
        });
        define(&owner, "run", MVal::Fn(callable));
        define(&owner, "state", MVal::Atom(atom));
        define(&owner, "cap", MVal::Cap(cap));
        modules.push(owner);
    }

    let start = Instant::now();
    for i in 0..OPS {
        let module = &modules[i % MODULES];
        let value = module.bindings.borrow()["run"].clone();
        black_box(value);
        let pulse = module.clone();
        drop(pulse);
        if i % 128 == 0 {
            black_box(maybe_collect(64));
        }
    }
    drop(modules);
    black_box(collect_cycles());
    let elapsed = start.elapsed();
    assert_eq!(drops.count(), MODULES * 4);
    elapsed
}

fn main() {
    for _ in 0..2 {
        black_box(workload());
    }
    let mut samples: Vec<Duration> = (0..11).map(|_| workload()).collect();
    samples.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
    println!(
        "mixed workload: modules={MODULES} ops={OPS} n=11 median={:.3}ms p95={:.3}ms min={:.3}ms max={:.3}ms",
        ms(samples[5]),
        ms(samples[10]),
        ms(samples[0]),
        ms(samples[10]),
    );
}
