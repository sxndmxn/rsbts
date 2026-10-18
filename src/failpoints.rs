//! Deterministic safety-boundary failure injection used by crash/recovery tests.

#[cfg(test)]
use crate::Error;
use crate::Result;

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
#[derive(Debug, Default)]
struct State {
    fail_at: Option<usize>,
    raw_os_error: Option<i32>,
    hits: Vec<&'static str>,
}

#[cfg(test)]
thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Mark a mutation, sync, or database transition boundary.
#[cfg(test)]
pub fn hit(name: &'static str) -> Result<()> {
    let (should_fail, raw_os_error) = STATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return (false, None);
        };
        let index = state.hits.len();
        state.hits.push(name);
        (state.fail_at == Some(index), state.raw_os_error)
    });
    if !should_fail {
        return Ok(());
    }
    if let Some(raw_os_error) = raw_os_error {
        return Err(Error::Io(std::io::Error::from_raw_os_error(raw_os_error)));
    }
    Err(Error::Operation(format!(
        "injected failure after safety boundary {name}"
    )))
}

/// Production builds retain the same call sites without the test recorder.
#[cfg(not(test))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "keeps instrumented call sites uniform"
)]
pub const fn hit(name: &'static str) -> Result<()> {
    let _ = name;
    Ok(())
}

#[cfg(test)]
pub fn run_recording<T>(operation: impl FnOnce() -> Result<T>) -> (Result<T>, Vec<&'static str>) {
    STATE.with(|slot| *slot.borrow_mut() = Some(State::default()));
    let result = operation();
    let hits = STATE.with(|slot| {
        slot.borrow_mut()
            .take()
            .map_or_else(Vec::new, |state| state.hits)
    });
    (result, hits)
}

#[cfg(test)]
pub fn run_failing<T>(
    fail_at: usize,
    operation: impl FnOnce() -> Result<T>,
) -> (Result<T>, Vec<&'static str>) {
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(State {
            fail_at: Some(fail_at),
            raw_os_error: None,
            hits: Vec::new(),
        });
    });
    let result = operation();
    let hits = STATE.with(|slot| {
        slot.borrow_mut()
            .take()
            .map_or_else(Vec::new, |state| state.hits)
    });
    (result, hits)
}

/// Inject an operating-system I/O error at a recorded boundary.
#[cfg(test)]
pub fn run_failing_io<T>(
    fail_at: usize,
    raw_os_error: i32,
    operation: impl FnOnce() -> Result<T>,
) -> (Result<T>, Vec<&'static str>) {
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(State {
            fail_at: Some(fail_at),
            raw_os_error: Some(raw_os_error),
            hits: Vec::new(),
        });
    });
    let result = operation();
    let hits = STATE.with(|slot| {
        slot.borrow_mut()
            .take()
            .map_or_else(Vec::new, |state| state.hits)
    });
    (result, hits)
}
