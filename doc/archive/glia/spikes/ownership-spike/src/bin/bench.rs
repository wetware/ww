//! Microbenchmarks for the amended-Graph-4 barrier (release mode).
//!
//! PROVISIONAL ACCEPTANCE THRESHOLDS — SET BEFORE FIRST RUN:
//!  T1  data-only fast path (has_resting=false) must avoid the escape walk:
//!      lookup of a 10k-entry data map <= 2x plain clone of the same map.
//!  T2  no Rust-stack growth with depth: 100k-deep define+lookup completes
//!      (crash = fail).
//!  T3  no superlinear behavior: doubling nodes must not more-than-triple
//!      wall time on the callable-under-N-nodes case (allowing noise).
//!  T4  one callable under 10,000 data nodes: lookup <= 5 ms/iter
//!      (classify: <=1ms acceptable; 1-5ms optimization-required;
//!       >5ms design-blocking).
//!  T5  1,000 duplicate/aliased callables: transform node count linear in
//!      occurrences, wall <= 5 ms/iter.
//!  T6  local_bindings over 1,000 defs (10% callables) <= 10 ms/iter.
//!  T7  repeated rest+escape on a small callable value <= 0.005 ms/iter.
use ownership_spike::*;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn nodes_reset() { NODES_VISITED.store(0, Ordering::Relaxed); }
fn nodes() -> usize { NODES_VISITED.load(Ordering::Relaxed) }

fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) -> f64 {
    let t = Instant::now();
    for _ in 0..iters { f(); }
    let total = t.elapsed().as_secs_f64() * 1e3;
    println!("{label}: {:.3} ms / {iters} iters ({:.5} ms/iter)", total, total / iters as f64);
    total / iters as f64
}

fn big_data_map(n: usize) -> ToyVal {
    ToyVal::MapV((0..n).map(|i| (ToyVal::Int(i as i64), ToyVal::Int((i * 2) as i64))).collect())
}

fn main() {
    let own = Defs::new(None);

    own.define("s", ToyVal::Int(1)).unwrap();
    time("1  scalar lookup", 100_000, || { let _ = own.lookup("s").unwrap(); });

    own.define("bigdata", big_data_map(10_000)).unwrap();
    let t_fast = time("2  data-only 10k-map lookup (fast path)", 200, || { let _ = own.lookup("bigdata").unwrap(); });
    let raw = big_data_map(10_000);
    let t_clone = time("2b plain clone baseline", 200, || { let _ = raw.clone(); });
    println!("   T1 ratio fast-path/clone: {:.2} (<=2.0 passes)", t_fast / t_clone);

    let mut v = ToyVal::Fn(make_fn(&own, vec![]));
    for _ in 0..5 { v = ToyVal::List(vec![v]); }
    own.define("d5", v).unwrap();
    time("3  callable@depth5 lookup", 100_000, || { let _ = own.lookup("d5").unwrap(); });

    let mut items: Vec<ToyVal> = (0..9_999).map(ToyVal::Int).collect();
    items.push(ToyVal::Fn(make_fn(&own, vec![])));
    nodes_reset();
    let t = Instant::now();
    own.define("wide", ToyVal::List(items)).unwrap();
    println!("4  define callable-under-10k-nodes: {:.3} ms, {} nodes", t.elapsed().as_secs_f64()*1e3, nodes());
    nodes_reset();
    let t4 = time("4  lookup callable-under-10k-nodes", 200, || { let _ = own.lookup("wide").unwrap(); });
    println!("   lookup nodes/iter: {}", nodes() / 200);
    println!("   T4 verdict: {}", if t4 <= 1.0 {"ACCEPTABLE (<=1ms)"} else if t4 <= 5.0 {"OPTIMIZATION-REQUIRED (1-5ms)"} else {"DESIGN-BLOCKING (>5ms)"});

    let mut items2: Vec<ToyVal> = (0..19_999).map(ToyVal::Int).collect();
    items2.push(ToyVal::Fn(make_fn(&own, vec![])));
    own.define("wide2", ToyVal::List(items2)).unwrap();
    let t4b = time("4b lookup callable-under-20k-nodes", 200, || { let _ = own.lookup("wide2").unwrap(); });
    println!("   T3 scaling 20k/10k: {:.2} (<=3.0 passes)", t4b / t4);

    let f = make_fn(&own, vec![]);
    let repeated = ToyVal::List((0..1_000).map(|_| ToyVal::Fn(f.clone())).collect());
    nodes_reset();
    own.define("rep", repeated).unwrap();
    println!("5  define nodes for 1000 duplicates: {} (linear passes)", nodes());
    let t5 = time("5  lookup 1000-duplicate list", 1_000, || { let _ = own.lookup("rep").unwrap(); });
    println!("   T5 verdict: {}", if t5 <= 5.0 {"PASS"} else {"FAIL"});

    let t = Instant::now();
    for i in 0..1_000 { own.define(&format!("alias{i}"), ToyVal::Fn(f.clone())).unwrap(); }
    println!("6  1000 alias defines: {:.3} ms", t.elapsed().as_secs_f64()*1e3);

    own.define("keyed", ToyVal::MapV(vec![(ToyVal::Fn(make_fn(&own, vec![])), ToyVal::Int(1))])).unwrap();
    time("7  callable-map-key lookup", 100_000, || { let _ = own.lookup("keyed").unwrap(); });

    let foreign = Defs::new(None);
    let fgraph = ToyVal::List((0..1_000).map(|_| ToyVal::Fn(make_fn(&foreign, vec![]))).collect());
    own.define("fgraph", fgraph).unwrap();
    time("8  foreign-owner 1000-callable lookup", 1_000, || { let _ = own.lookup("fgraph").unwrap(); });

    let m = Defs::new(None);
    for i in 0..900 { m.define(&format!("d{i}"), ToyVal::Int(i)).unwrap(); }
    for i in 0..100 { m.define(&format!("f{i}"), ToyVal::Fn(make_fn(&m, vec![]))).unwrap(); }
    let t6 = time("9  local_bindings 1000 defs (10% fns)", 100, || { let _ = m.local_bindings().unwrap(); });
    println!("   T6 verdict: {}", if t6 <= 10.0 {"PASS (<=10ms)"} else {"FAIL"});

    let mut deep = ToyVal::Fn(make_fn(&m, vec![]));
    for _ in 0..100_000 { deep = ToyVal::List(vec![deep]); }
    let t = Instant::now();
    m.define_ref("deep", &deep).unwrap();
    let _ = m.lookup("deep").unwrap();
    println!("10 100k-deep define+lookup: {:.3} ms (T2: completing = pass)", t.elapsed().as_secs_f64()*1e3);
    let stored = m.bindings.borrow_mut().remove("deep").unwrap().value;
    for val in [deep, stored] {
        let mut stack = vec![val];
        while let Some(x) = stack.pop() { if let ToyVal::List(xs) = x { stack.extend(xs); } }
    }

    let cap = make_owned_cap(&m, (0..100).map(|i| (format!("m{i}"), ToyVal::Fn(make_fn(&m, vec![])))).collect());
    m.define("svc", ToyVal::Cap(cap)).unwrap();
    time("11 cap lookup (100 sealed methods)", 100_000, || { let _ = m.lookup("svc").unwrap(); });
    let svc = m.lookup("svc").unwrap().unwrap();
    let ToyVal::Cap(c) = &svc else { panic!() };
    time("11b cap_dispatch one method", 100_000, || { let _ = cap_dispatch(c, "m50").unwrap(); });

    let small = ToyVal::List(vec![ToyVal::Fn(make_fn(&m, vec![])), ToyVal::Int(1)]);
    let t7 = time("12 rest+escape (small value)", 10_000, || {
        let (r, _) = rest_for(&m, &small);
        let _ = escape_with(&m, &r).unwrap();
    });
    println!("   T7 verdict: {}", if t7 <= 0.005 {"PASS"} else {"FAIL"});

    println!("\nDEFS_DROPS={}", DEFS_DROPS.load(Ordering::Relaxed));
}
