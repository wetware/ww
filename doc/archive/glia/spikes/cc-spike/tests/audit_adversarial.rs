//! Review-only adversarial harnesses. These tests are ignored by default
//! because several intentionally demonstrate a failing safety/panic contract.

use cc_spike::cc::{collect_cycles, Cc, Trace, TraceAbort, Tracer};
use cc_spike::model::{Drops, MAtom, MVal};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct NeighborReader {
    next: RefCell<Option<Cc<NeighborReader>>>,
    payload: String,
}

unsafe impl Trace for NeighborReader {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let next = self.next.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(next) = next.as_ref() {
            tracer.edge(next);
        }
        Ok(())
    }
}

impl Drop for NeighborReader {
    fn drop(&mut self) {
        if let Some(next) = self.next.get_mut().as_ref() {
            // Whichever node is destroyed second reads the first node after
            // its String field has been dropped. A safe Cc::deref must never
            // hand out a reference to that destroyed value.
            std::hint::black_box(next.payload.as_bytes()[0]);
        }
    }
}

#[test]
#[ignore = "Miri-only reproducer: current collector exposes a dropped neighbor"]
fn destructor_cannot_deref_an_already_dropped_white_neighbor() {
    let a = Cc::new(NeighborReader {
        next: RefCell::new(None),
        payload: String::from("a"),
    });
    let b = Cc::new(NeighborReader {
        next: RefCell::new(None),
        payload: String::from("b"),
    });
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone());
    drop(a);
    drop(b);
    collect_cycles();
}

struct PanicDropNode {
    next: RefCell<Option<Cc<PanicDropNode>>>,
    panic_once: Rc<Cell<bool>>,
}

unsafe impl Trace for PanicDropNode {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let next = self.next.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(next) = next.as_ref() {
            tracer.edge(next);
        }
        Ok(())
    }
}

impl Drop for PanicDropNode {
    fn drop(&mut self) {
        if self.panic_once.replace(false) {
            panic!("injected destructor panic");
        }
    }
}

#[test]
#[ignore = "intentional panic/leak reproducer"]
fn destructor_panic_must_not_permanently_poison_the_release_pump() {
    let panic_once = Rc::new(Cell::new(true));
    let a = Cc::new(PanicDropNode {
        next: RefCell::new(None),
        panic_once: panic_once.clone(),
    });
    let b = Cc::new(PanicDropNode {
        next: RefCell::new(None),
        panic_once,
    });
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone());
    drop(a);
    drop(b);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
    assert!(outcome.is_err(), "the injected destructor must panic");

    let drops = Drops::default();
    let probe = Cc::new(MAtom {
        value: RefCell::new(MVal::Int(1)),
        drops: drops.clone(),
    });
    drop(probe);
    assert_eq!(
        drops.count(),
        1,
        "a caught destructor panic must not leave PUMPING=true forever"
    );
}

struct TracePanicNode {
    next: RefCell<Option<Cc<TracePanicNode>>>,
    panic_trace: Rc<Cell<bool>>,
    drops: Drops,
}

unsafe impl Trace for TracePanicNode {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        if self.panic_trace.get() {
            panic!("injected Trace panic");
        }
        let next = self.next.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(next) = next.as_ref() {
            tracer.edge(next);
        }
        Ok(())
    }
}

impl Drop for TracePanicNode {
    fn drop(&mut self) {
        self.drops.0.set(self.drops.count() + 1);
    }
}

#[test]
#[ignore = "intentional panic/leak reproducer"]
fn trace_panic_must_preserve_candidate_buffering() {
    let panic_trace = Rc::new(Cell::new(true));
    let drops = Drops::default();
    let a = Cc::new(TracePanicNode {
        next: RefCell::new(None),
        panic_trace: panic_trace.clone(),
        drops: drops.clone(),
    });
    let b = Cc::new(TracePanicNode {
        next: RefCell::new(None),
        panic_trace: panic_trace.clone(),
        drops: drops.clone(),
    });
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone());
    drop(a);
    drop(b);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
    assert!(outcome.is_err(), "the injected Trace implementation must panic");
    panic_trace.set(false);
    let recovered = collect_cycles();
    assert_eq!(recovered.collected, 2, "the same candidates must remain collectable");
    assert_eq!(drops.count(), 2);
}

struct ReenterNode {
    next: RefCell<Option<Cc<ReenterNode>>>,
    reenter_once: Rc<Cell<bool>>,
}

unsafe impl Trace for ReenterNode {
    fn trace(&self, tracer: &mut Tracer) -> Result<(), TraceAbort> {
        let next = self.next.try_borrow().map_err(|_| TraceAbort)?;
        if let Some(next) = next.as_ref() {
            tracer.edge(next);
        }
        Ok(())
    }
}

impl Drop for ReenterNode {
    fn drop(&mut self) {
        if self.reenter_once.replace(false) {
            // The public collector currently has no COLLECTING guard.
            let _ = collect_cycles();
        }
    }
}

#[test]
#[ignore = "expected to fail until collector re-entry is rejected"]
fn collection_reentry_from_drop_must_be_rejected() {
    let reenter_once = Rc::new(Cell::new(true));
    let a = Cc::new(ReenterNode {
        next: RefCell::new(None),
        reenter_once: reenter_once.clone(),
    });
    let b = Cc::new(ReenterNode {
        next: RefCell::new(None),
        reenter_once,
    });
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone());
    drop(a);
    drop(b);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(collect_cycles));
    assert!(outcome.is_err(), "collector re-entry from Drop was silently allowed");
}

