//! Ring buffer that mirrors recently-played samples for the waveform
//! visualizer. The audio iterator pushes each consumed sample into the ring;
//! the visualizer reads via [`AmplitudeTap::recent_samples`] / `rms`.
//!
//! Locking is `std::sync::RwLock` with brief, bounded critical sections — the
//! audio iterator holds the write lock just long enough to push one sample.
//! If profiling later reveals stalls under contention, switch to a lock-free
//! ring (`ringbuf` crate or similar); the simple version keeps the test surface
//! small.

use std::sync::{Arc, RwLock};

/// 1 second of audio at 24 kHz — enough headroom for any reasonable
/// visualizer window without unbounded memory.
pub const RING_CAPACITY: usize = 24_000;

#[derive(Debug)]
pub struct RingBuffer {
    samples: Vec<f32>,
    head: usize,
    filled: bool,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity],
            head: 0,
            filled: false,
        }
    }

    pub fn push(&mut self, s: f32) {
        let cap = self.samples.len();
        if cap == 0 {
            return;
        }
        self.samples[self.head] = s;
        self.head = (self.head + 1) % cap;
        if self.head == 0 {
            self.filled = true;
        }
    }

    /// Reset the ring to empty so [`recent`](Self::recent) returns nothing
    /// until fresh samples are pushed. Called on a playback stop so the
    /// visualizer / avatar lip-sync doesn't momentarily replay the tail of
    /// the previous utterance at the start of the next read. No need to zero
    /// the backing storage — `recent` is bounded by `head`/`filled`.
    pub fn clear(&mut self) {
        self.head = 0;
        self.filled = false;
    }

    pub fn recent(&self, count: usize) -> Vec<f32> {
        let cap = self.samples.len();
        let available = if self.filled { cap } else { self.head };
        let count = count.min(available);
        if count == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count);
        let start = (self.head + cap - count) % cap;
        for i in 0..count {
            out.push(self.samples[(start + i) % cap]);
        }
        out
    }
}

#[derive(Clone)]
pub struct AmplitudeTap {
    inner: Arc<RwLock<RingBuffer>>,
}

impl AmplitudeTap {
    pub(crate) fn from_arc(inner: Arc<RwLock<RingBuffer>>) -> Self {
        Self { inner }
    }

    pub fn recent_samples(&self, count: usize) -> Vec<f32> {
        self.inner
            .read()
            .map(|b| b.recent(count))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_recent_after_partial_fill() {
        let mut r = RingBuffer::new(8);
        for i in 0..3 {
            r.push(i as f32);
        }
        assert_eq!(r.recent(5), vec![0.0, 1.0, 2.0]);
        assert_eq!(r.recent(2), vec![1.0, 2.0]);
    }

    #[test]
    fn ring_clear_empties_recent_and_resets() {
        let mut r = RingBuffer::new(4);
        for i in 0..6 {
            r.push(i as f32);
        }
        assert_eq!(r.recent(4).len(), 4);
        r.clear();
        assert!(r.recent(4).is_empty(), "cleared ring should report nothing");
        // New samples start a fresh fill.
        r.push(9.0);
        assert_eq!(r.recent(4), vec![9.0]);
    }

    #[test]
    fn ring_recent_after_wrap() {
        let mut r = RingBuffer::new(4);
        for i in 0..6 {
            r.push(i as f32);
        }
        // Last 4: 2, 3, 4, 5
        assert_eq!(r.recent(4), vec![2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r.recent(2), vec![4.0, 5.0]);
    }
}
