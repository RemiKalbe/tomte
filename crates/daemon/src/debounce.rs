//! Rolling debounce for filesystem event paths (spec §3.1, ~500ms window).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

pub struct Debouncer {
    rx: Receiver<PathBuf>,
    window: Duration,
}

impl Debouncer {
    pub fn new(window: Duration) -> (Self, Sender<PathBuf>) {
        let (tx, rx) = channel();
        (Self { rx, window }, tx)
    }

    pub fn recv_batch(&self) -> Option<Vec<PathBuf>> {
        // block for the first path of a burst
        let first = self.rx.recv().ok()?;
        let mut batch = BTreeSet::new();
        batch.insert(first);
        let start = Instant::now();
        let cap = self.window * 10;
        loop {
            match self.rx.recv_timeout(self.window) {
                Ok(p) => {
                    batch.insert(p);
                    if start.elapsed() >= cap {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        Some(batch.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn coalesces_bursts_and_dedups() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(50));
        for _ in 0..3 {
            tx.send(std::path::PathBuf::from("/a")).unwrap();
        }
        tx.send(std::path::PathBuf::from("/b")).unwrap();
        let batch = deb.recv_batch().unwrap();
        assert_eq!(batch.len(), 2, "{batch:?}");
        assert!(batch.contains(&std::path::PathBuf::from("/a")));
        assert!(batch.contains(&std::path::PathBuf::from("/b")));
    }

    #[test]
    fn separate_bursts_are_separate_batches() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(30));
        tx.send(std::path::PathBuf::from("/one")).unwrap();
        let b1 = deb.recv_batch().unwrap();
        tx.send(std::path::PathBuf::from("/two")).unwrap();
        let b2 = deb.recv_batch().unwrap();
        assert_eq!((b1.len(), b2.len()), (1, 1));
    }

    #[test]
    fn returns_none_when_senders_gone() {
        let (deb, tx) = Debouncer::new(Duration::from_millis(10));
        drop(tx);
        assert!(deb.recv_batch().is_none());
    }
}
