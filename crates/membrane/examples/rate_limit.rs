//! Example fixed-window policy built outside the membrane enforcement core.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use capnp::Error;
use membrane::{denied_error, Allowlist, Policy};

struct RateLimit {
    inner: Box<dyn Policy>,
    max_per_window: u32,
    window: Duration,
    state: RefCell<RateWindow>,
}

struct RateWindow {
    count: u32,
    started: Instant,
}

impl RateLimit {
    fn new(inner: Box<dyn Policy>, max_per_window: u32, window: Duration) -> Self {
        Self {
            inner,
            max_per_window,
            window,
            state: RefCell::new(RateWindow {
                count: 0,
                started: Instant::now(),
            }),
        }
    }
}

impl Policy for RateLimit {
    fn check(&self, interface_id: u64, method_id: u16) -> Result<(), Error> {
        self.inner.check(interface_id, method_id)?;

        let mut state = self.state.borrow_mut();
        if state.started.elapsed() >= self.window {
            state.count = 0;
            state.started = Instant::now();
        }
        if state.count >= self.max_per_window {
            return Err(denied_error(interface_id, method_id, "rate limit exceeded"));
        }
        state.count += 1;
        Ok(())
    }
}

fn main() {
    const INTERFACE_ID: u64 = 0xfeed_face_cafe_beef;
    let policy = RateLimit::new(
        Box::new(Allowlist::new().allow(INTERFACE_ID, 0)),
        3,
        Duration::from_secs(60),
    );

    for _ in 0..3 {
        assert!(policy.check(INTERFACE_ID, 0).is_ok());
    }
    assert!(policy.check(INTERFACE_ID, 0).is_err());
}
