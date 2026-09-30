//! No shard serves until every shard has restored its snapshot and log.
//!
//! State the command set keeps outside the keyspace (the server's index
//! catalog) is recorded in whichever shard's log it was changed on, so it
//! is known only once every log has been read; a command served before
//! that would act on part of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

#[derive(Debug)]
pub(crate) struct RestoreGate {
    left: Mutex<usize>,
    arrived: Vec<AtomicBool>,
    all: Condvar,
}

impl RestoreGate {
    pub(crate) fn new(nshards: usize) -> Self {
        Self {
            left: Mutex::new(nshards),
            arrived: (0..nshards).map(|_| AtomicBool::new(false)).collect(),
            all: Condvar::new(),
        }
    }

    /// Shard `id` has restored, or has stopped and never will: only the
    /// first call for a shard counts.
    pub(crate) fn arrive(&self, id: usize) {
        if self.arrived.get(id).is_some_and(|a| a.swap(true, Ordering::AcqRel)) {
            return;
        }
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        *left = left.saturating_sub(1);
        if *left == 0 {
            self.all.notify_all();
        }
    }

    /// Block until every shard has arrived.
    pub(crate) fn wait(&self) {
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        while *left > 0 {
            left = self.all.wait(left).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RestoreGate;
    use std::sync::Arc;

    #[test]
    fn it_opens_when_the_last_shard_arrives_and_counts_each_once() {
        let gate = Arc::new(RestoreGate::new(3));
        gate.arrive(0);
        gate.arrive(0);
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.wait())
        };
        gate.arrive(1);
        assert_eq!(*gate.left.lock().unwrap(), 1, "shard 0 counted once");
        gate.arrive(2);
        waiter.join().unwrap();
        assert_eq!(*gate.left.lock().unwrap(), 0);
    }
}
