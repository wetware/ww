#![no_main]
use cc_spike::cc::{collect_cycles, Cc, Trace, TraceAbort, Tracer};
use libfuzzer_sys::fuzz_target;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct PanicTrace {
    next: RefCell<Option<Cc<PanicTrace>>>,
    panic_trace: Rc<Cell<bool>>,
}
unsafe impl Trace for PanicTrace {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        if self.panic_trace.get() { panic!("injected trace panic"); }
        let next = self.next.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(next) = next.as_ref() { tracer.edge(next); }
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() { return; }
    // Let the target catch the injected Trace panic; libFuzzer otherwise
    // converts every panic into an immediate abort from its installed hook.
    std::panic::set_hook(Box::new(|_| {}));
    let panic_trace = Rc::new(Cell::new(true));
    let a = Cc::new(PanicTrace { next: RefCell::new(None), panic_trace: panic_trace.clone() });
    let b = Cc::new(PanicTrace { next: RefCell::new(None), panic_trace: panic_trace.clone() });
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone());
    drop(a);
    drop(b);
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
    assert!(first.is_err());
    panic_trace.set(false);
    assert_eq!(collect_cycles().collected, 2, "Trace panic lost the candidate set");
});
