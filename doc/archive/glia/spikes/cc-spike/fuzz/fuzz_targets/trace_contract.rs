#![no_main]
use cc_spike::cc::{collect_cycles, Cc, Trace, TraceAbort, Tracer};
use libfuzzer_sys::fuzz_target;
use std::cell::RefCell;

struct Leaf;
unsafe impl Trace for Leaf {
    fn trace(&self, _: &mut Tracer) -> Result<(), TraceAbort> { Ok(()) }
}

struct Contract {
    edges: RefCell<Vec<Cc<Leaf>>>,
    mode: u8,
}
unsafe impl Trace for Contract {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let edges = self.edges.try_borrow().map_err(|_| TraceAbort)?;
        match self.mode % 4 {
            0 => for edge in edges.iter() { tracer.edge(edge); },
            1 => {} // deliberate opaque omission
            2 => if let Some(edge) = edges.first() { tracer.edge(edge); tracer.edge(edge); },
            _ => for edge in edges.iter() { tracer.edge(edge); }, // distinct clones are valid
        }
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    // cargo-fuzz's default hook aborts before catch_unwind can observe the
    // collector's deliberate contract panic. Replace it so this target can
    // distinguish the expected duplicate-handle rejection from crashes.
    std::panic::set_hook(Box::new(|_| {}));
    let mode = data.first().copied().unwrap_or(0) % 4;
    let leaf = Cc::new(Leaf);
    let edge_count = if mode == 3 { 2 } else { 1 };
    let source = Cc::new(Contract {
        edges: RefCell::new((0..edge_count).map(|_| leaf.clone()).collect()),
        mode,
    });
    let pulse = source.clone();
    drop(pulse);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
    if cfg!(debug_assertions) && mode == 2 {
        assert!(outcome.is_err(), "same physical handle was not rejected");
    } else {
        assert!(outcome.is_ok(), "valid trace contract was rejected");
    }
    drop(source);
    drop(leaf);
    collect_cycles();
});
