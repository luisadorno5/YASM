//! Thread-safe bounded ring buffer for child stdout/stderr lines (`OutputLineBuffer`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Thread-safe ring buffer keeping the most recent `capacity` output lines.
#[derive(Clone, Debug)]
pub struct OutputLineBuffer {
    capacity: usize,
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl OutputLineBuffer {
    /// Create a new bounded ring buffer. If `capacity == 0`, no lines are retained.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(1024)))),
        }
    }

    /// Push a line into the ring buffer, evicting the oldest line if at capacity.
    pub fn push(&self, line: impl Into<String>) {
        if self.capacity == 0 {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() == self.capacity {
            guard.pop_front();
        }
        guard.push_back(line.into());
    }

    /// Return all currently buffered lines in chronological order.
    pub fn snapshot(&self) -> Vec<String> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.iter().cloned().collect()
    }

    /// Return up to the last `count` lines in chronological order.
    pub fn last_n(&self, count: usize) -> Vec<String> {
        if count == 0 {
            return Vec::new();
        }
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let len = guard.len();
        let skip = len.saturating_sub(count);
        guard.iter().skip(skip).cloned().collect()
    }

    /// Clear all buffered lines.
    pub fn clear(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.clear();
    }

    /// Return the configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn push_should_retain_only_most_recent_lines_up_to_capacity() {
        // Arrange
        let buffer = OutputLineBuffer::new(3);

        // Act
        buffer.push("line 1");
        buffer.push("line 2");
        buffer.push("line 3");
        buffer.push("line 4");

        // Assert
        assert_eq!(buffer.snapshot(), vec!["line 2", "line 3", "line 4"]);
        assert_eq!(buffer.last_n(2), vec!["line 3", "line 4"]);
    }

    #[test]
    fn push_should_store_nothing_when_capacity_is_zero() {
        // Arrange
        let buffer = OutputLineBuffer::new(0);

        // Act
        buffer.push("line 1");

        // Assert
        assert!(buffer.snapshot().is_empty());
    }

    #[test]
    fn push_should_be_thread_safe_under_concurrent_writes() {
        // Arrange
        let buffer = OutputLineBuffer::new(50);
        let mut handles = Vec::new();

        // Act
        for t in 0..8 {
            let buf = buffer.clone();
            handles.push(thread::spawn(move || {
                for i in 0..20 {
                    buf.push(format!("thread-{t}-line-{i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // Assert
        let snap = buffer.snapshot();
        assert_eq!(snap.len(), 50);
    }
}
