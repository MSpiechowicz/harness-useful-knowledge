//! Work that must not outlast a deadline.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// The result of `work` run on a thread called `name`; `None` when it did not finish within
/// `deadline` (the thread is left to end by itself: a process that is about to exit takes it along,
/// a long-lived one loses a thread that ends when its connection does).
pub fn within<T: Send + 'static>(
    name: &str,
    deadline: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = mpsc::channel();

    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .ok()?;

    receiver.recv_timeout(deadline).ok()
}

/// How many threads of one kind may be left running past their deadline. A server that accepts a
/// connection and never speaks would otherwise leave one more thread (and socket) behind for every
/// attempt a long-lived process makes while it stays silent.
pub struct Slots {
    running: AtomicUsize,
    max: usize,
}

impl Slots {
    pub const fn new(max: usize) -> Self {
        Self {
            running: AtomicUsize::new(0),
            max,
        }
    }

    /// Takes a slot, `None` when all are in use.
    fn take(&'static self) -> Option<Slot> {
        let taken = self
            .running
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |running| {
                (running < self.max).then_some(running + 1)
            })
            .is_ok();

        taken.then(|| Slot(self))
    }
}

/// A slot of [`Slots`], given back when the thread that holds it ends.
struct Slot(&'static Slots);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why [`within_slots`] gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missed {
    /// All slots were held by threads that have not ended, so no thread was started: nothing is
    /// known about the work itself.
    Busy,
    /// The work did not finish within the deadline.
    Late,
}

/// [`within`] that starts no thread (and answers [`Missed::Busy`] at once) while `slots` are all
/// held by threads that have not ended, however long ago their deadline passed.
pub fn within_slots<T: Send + 'static>(
    slots: &'static Slots,
    name: &str,
    deadline: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Missed> {
    let slot = slots.take().ok_or(Missed::Busy)?;

    within(name, deadline, move || {
        let _held = slot;

        work()
    })
    .ok_or(Missed::Late)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_that_finishes_in_time_answers_and_work_that_does_not_is_left_behind() {
        assert_eq!(within("quick", Duration::from_secs(2), || 7), Some(7));

        let started = std::time::Instant::now();
        let slow = within("slow", Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
            7
        });

        assert_eq!(slow, None);
        assert!(started.elapsed() < Duration::from_millis(400));
    }

    #[test]
    fn work_left_behind_holds_its_slot_until_it_ends_and_no_more_starts_meanwhile() {
        static SLOTS: Slots = Slots::new(2);

        let (release, released) = mpsc::channel::<()>();
        let released = std::sync::Arc::new(std::sync::Mutex::new(released));

        for _ in 0..2 {
            let released = std::sync::Arc::clone(&released);
            let stuck = within_slots(&SLOTS, "stuck", Duration::from_millis(30), move || {
                let _ = released.lock().expect("lock").recv();
                1
            });

            assert_eq!(stuck, Err(Missed::Late), "it did not finish in time");
        }

        let started = std::time::Instant::now();
        assert_eq!(
            within_slots(&SLOTS, "refused", Duration::from_secs(5), || 7),
            Err(Missed::Busy),
            "both slots are held by threads that never ended"
        );
        assert!(started.elapsed() < Duration::from_millis(500), "no thread, no wait");

        drop(release);

        // The stuck threads end, the slots come back.
        let mut answered = Err(Missed::Busy);

        for _ in 0..100 {
            answered = within_slots(&SLOTS, "again", Duration::from_secs(5), || 7);

            if answered.is_ok() {
                break;
            }

            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(answered, Ok(7));
    }
}
