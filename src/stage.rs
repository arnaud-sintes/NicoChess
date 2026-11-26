//! The source stage: what a picture is stamped with the moment it comes from a source, and how much memory the queues
//! behind the sources may hold.
//!
//! A picture is never dropped on its way to the recording or to the analysis because a queue is full: the queues grow, and
//! only a memory budget bounds them (a [`Budget`]). Going past it is a failure that is said loudly (the recorder warns
//! at half of it and writes a `gap` line for every stretch of pictures it had to leave out), not a way of keeping up.
//!
//! The analysis does not read its pictures from here yet (it still takes the latest of each camera, see `engine`);
//! the recorder does, and the stamp is what a queue per source will carry.

use image::RgbImage;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// A picture as it came from its source: which source, the number it got (counting every picture of that source since
/// the program started, so a hole in the numbers is a picture that was lost), and the moment the camera made it, as far
/// as the source knows (else the moment the program got it).
#[derive(Clone)]
pub struct Stamped {
    /// The number of the view: 0 and 1 for the cameras, 2 for the height picture of the depth camera.
    pub view: usize,
    pub seq: u64,
    pub captured: Instant,
    pub image: Arc<RgbImage>,
}

/// The memory that the queues of a recording may hold: pictures waiting for the encoder or the disk.
pub struct Budget {
    cap: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}

/// What a picture takes in a queue.
pub fn cost_of(image: &RgbImage) -> usize {
    image.as_raw().len()
}

impl Budget {
    pub fn new(cap: usize) -> Arc<Budget> {
        Arc::new(Budget { cap, used: AtomicUsize::new(0), peak: AtomicUsize::new(0) })
    }

    /// Takes `bytes` of the budget for a picture that is about to wait in a queue. False when that would pass the cap
    /// (the picture must then be left out and counted). A picture that is larger than the whole budget is still taken
    /// when nothing else waits: a budget too small must not mean that nothing is ever recorded.
    pub fn reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            if used > 0 && used.saturating_add(bytes) > self.cap {
                return false;
            }
            match self.used.compare_exchange_weak(used, used + bytes, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    self.peak.fetch_max(used + bytes, Ordering::Relaxed);
                    return true;
                }
                Err(now) => used = now,
            }
        }
    }

    /// Gives back what a picture took, once it is written (or lost).
    pub fn release(&self, bytes: usize) {
        let _ = self.used.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| Some(used.saturating_sub(bytes)));
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The most that ever waited at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// True when half of the budget or more is taken: the encoder or the disk is behind.
    pub fn half_taken(&self) -> bool {
        self.used().saturating_mul(2) >= self.cap
    }

    /// Gives `bytes` back when the returned value goes out of scope, whatever happened to the picture meanwhile (even a
    /// panic): put it on the line where a thread takes a picture from its queue.
    pub fn giving_back(&self, bytes: usize) -> GivingBack<'_> {
        GivingBack { budget: self, bytes }
    }
}

/// See [`Budget::giving_back`].
pub struct GivingBack<'a> {
    budget: &'a Budget,
    bytes: usize,
}

impl Drop for GivingBack<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_takes_pictures_up_to_its_cap_and_gives_the_room_back() {
        let budget = Budget::new(100);
        assert!(budget.reserve(60));
        assert!(!budget.half_taken() || budget.used() == 60);
        assert!(!budget.reserve(60), "60 and 60 pass 100");
        assert!(budget.reserve(40));
        assert_eq!((budget.used(), budget.peak()), (100, 100));
        budget.release(60);
        assert_eq!(budget.used(), 40);
        assert!(budget.reserve(60));
        budget.release(1000);
        assert_eq!(budget.used(), 0, "it never goes below nothing");
    }

    #[test]
    fn a_picture_larger_than_the_whole_budget_is_taken_when_nothing_waits() {
        let budget = Budget::new(10);
        assert!(budget.reserve(500));
        assert!(!budget.reserve(1));
        budget.release(500);
        assert!(budget.reserve(500));
    }

    #[test]
    fn half_taken_tells_that_the_encoder_is_behind() {
        let budget = Budget::new(100);
        assert!(!budget.half_taken());
        budget.reserve(49);
        assert!(!budget.half_taken());
        budget.reserve(1);
        assert!(budget.half_taken());
    }
}
