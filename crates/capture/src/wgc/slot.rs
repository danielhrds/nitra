//! Single-slot handoff where the newest value wins.
//!
//! The producer never blocks: putting a value replaces any value the
//! consumer has not taken yet (the old one is dropped, which for frames
//! returns the texture to its pool). An end marker can be set once and is
//! reported after the last pending value.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub enum Taken<T, E> {
    Value(T),
    Ended(E),
    Timeout,
}

#[derive(Debug)]
pub struct LatestSlot<T, E> {
    state: Mutex<State<T, E>>,
    changed: Condvar,
}

#[derive(Debug)]
struct State<T, E> {
    value: Option<T>,
    end: Option<E>,
}

impl<T, E: Clone> LatestSlot<T, E> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                value: None,
                end: None,
            }),
            changed: Condvar::new(),
        }
    }

    /// Stores `value`; returns `true` if it replaced one not yet taken.
    /// Values put after the end marker are dropped.
    pub fn put(&self, value: T) -> bool {
        let mut state = self.state.lock().expect("slot lock");
        if state.end.is_some() {
            return false;
        }
        let replaced = state.value.replace(value).is_some();
        drop(state);
        self.changed.notify_one();
        replaced
    }

    /// Marks the end; only the first call has an effect.
    pub fn end(&self, reason: E) {
        let mut state = self.state.lock().expect("slot lock");
        if state.end.is_none() {
            state.end = Some(reason);
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Waits up to `timeout` for a value or the end marker.
    pub fn take(&self, timeout: Duration) -> Taken<T, E> {
        let state = self.state.lock().expect("slot lock");
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.value.is_none() && state.end.is_none()
            })
            .expect("slot lock");
        if let Some(value) = state.value.take() {
            Taken::Value(value)
        } else if let Some(end) = &state.end {
            Taken::Ended(end.clone())
        } else {
            Taken::Timeout
        }
    }
}

impl<T, E: Clone> Default for LatestSlot<T, E> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Slot = LatestSlot<u32, &'static str>;

    #[test]
    fn newest_value_wins() {
        let slot = Slot::new();
        assert!(!slot.put(1));
        assert!(slot.put(2));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Value(2)));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Timeout));
    }

    #[test]
    fn end_is_reported_after_pending_value() {
        let slot = Slot::new();
        slot.put(5);
        slot.end("closed");
        slot.end("ignored");
        assert!(!slot.put(6));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Value(5)));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Ended("closed")));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Ended("closed")));
    }

    #[test]
    fn take_wakes_up_on_put_from_another_thread() {
        let slot = Arc::new(Slot::new());
        let producer = Arc::clone(&slot);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.put(9);
        });
        assert!(matches!(slot.take(Duration::from_secs(5)), Taken::Value(9)));
        handle.join().unwrap();
    }

    #[test]
    fn replaced_values_are_dropped() {
        let counter = Arc::new(());
        let slot: LatestSlot<Arc<()>, ()> = LatestSlot::new();
        slot.put(Arc::clone(&counter));
        slot.put(Arc::clone(&counter));
        assert_eq!(Arc::strong_count(&counter), 2);
    }
}
