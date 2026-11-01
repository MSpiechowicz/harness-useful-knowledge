//! Work that must not outlast a deadline.

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
}
