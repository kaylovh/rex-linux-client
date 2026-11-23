//! The transfer list: what is queued, running and finished, with progress and speed; at most
//! two run at once, and never two into the same place.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub type TransferId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
    Done,
    Failed(String),
    Canceled,
}

#[derive(Debug, Clone)]
pub struct NewTransfer {
    pub direction: Direction,
    pub src_machine: String,
    pub src_path: String,
    pub dst_machine: String,
    /// The folder the copy goes into.
    pub dst_dir: String,
    /// The name it gets there.
    pub dst_name: String,
    pub overwrite: bool,
}

#[derive(Debug, Clone)]
pub struct Transfer {
    pub id: TransferId,
    pub job: NewTransfer,
    pub state: State,
    pub done: u64,
    pub total: u64,
    /// Bytes per second over the last few seconds.
    pub speed: f64,
    /// Continue from the parts already there (after a failure).
    pub resume: bool,
    pub cancel_requested: bool,
    samples: VecDeque<(Instant, u64)>,
}

/// How far back the speed looks.
pub const SPEED_WINDOW: Duration = Duration::from_secs(3);
pub const MAX_RUNNING: usize = 2;

#[derive(Default)]
pub struct Queue {
    items: Vec<Transfer>,
    next_id: TransferId,
}

impl Queue {
    pub fn items(&self) -> &[Transfer] {
        &self.items
    }
    pub fn get(&self, id: TransferId) -> Option<&Transfer> {
        self.items.iter().find(|t| t.id == id)
    }
    fn get_mut(&mut self, id: TransferId) -> Option<&mut Transfer> {
        self.items.iter_mut().find(|t| t.id == id)
    }

    pub fn add(&mut self, job: NewTransfer) -> TransferId {
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(Transfer {
            id,
            job,
            state: State::Queued,
            done: 0,
            total: 0,
            speed: 0.0,
            resume: false,
            cancel_requested: false,
            samples: VecDeque::new(),
        });
        id
    }

    /// The next queued transfer that may start now (marked running), if any: fewer than
    /// MAX_RUNNING run and none of them goes into the same place.
    pub fn next_to_start(&mut self) -> Option<TransferId> {
        let running: Vec<(String, String, String)> = self
            .items
            .iter()
            .filter(|t| t.state == State::Running)
            .map(|t| (t.job.dst_machine.clone(), t.job.dst_dir.clone(), t.job.dst_name.clone()))
            .collect();
        if running.len() >= MAX_RUNNING {
            return None;
        }
        let t = self.items.iter_mut().find(|t| {
            t.state == State::Queued && !running.iter().any(|(m, d, n)| *m == t.job.dst_machine && *d == t.job.dst_dir && *n == t.job.dst_name)
        })?;
        t.state = State::Running;
        t.samples.clear();
        t.speed = 0.0;
        Some(t.id)
    }

    pub fn progress(&mut self, id: TransferId, done: u64, total: u64, now: Instant) {
        let Some(t) = self.get_mut(id) else { return };
        t.done = done;
        t.total = total;
        t.samples.push_back((now, done));
        while t.samples.front().is_some_and(|(at, _)| now.duration_since(*at) > SPEED_WINDOW) {
            t.samples.pop_front();
        }
        t.speed = match (t.samples.front(), t.samples.back()) {
            (Some((a, da)), Some((b, db))) if b > a => (db.saturating_sub(*da)) as f64 / b.duration_since(*a).as_secs_f64(),
            _ => 0.0,
        };
    }

    /// A run ended: done, failed (why), or canceled when that was asked for.
    pub fn finished(&mut self, id: TransferId, result: Result<(), String>) {
        let Some(t) = self.get_mut(id) else { return };
        t.state = match result {
            Ok(()) => State::Done,
            Err(_) if t.cancel_requested => State::Canceled,
            Err(why) => State::Failed(why),
        };
        t.speed = 0.0;
    }

    /// Cancels a transfer; true when it is running and its runner has to stop.
    pub fn cancel(&mut self, id: TransferId) -> bool {
        let Some(t) = self.get_mut(id) else { return false };
        match t.state {
            State::Queued => {
                t.state = State::Canceled;
                false
            }
            State::Running => {
                t.cancel_requested = true;
                true
            }
            _ => false,
        }
    }

    /// Queues a failed or canceled transfer again: a failed one continues where it stopped.
    pub fn retry(&mut self, id: TransferId) {
        let Some(t) = self.get_mut(id) else { return };
        if matches!(t.state, State::Failed(_) | State::Canceled) {
            t.resume = matches!(t.state, State::Failed(_));
            t.state = State::Queued;
            t.cancel_requested = false;
        }
    }

    pub fn clear_finished(&mut self) {
        self.items.retain(|t| matches!(t.state, State::Queued | State::Running));
    }

    /// Queued or running.
    pub fn active(&self) -> usize {
        self.items.iter().filter(|t| matches!(t.state, State::Queued | State::Running)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dst: &str) -> NewTransfer {
        NewTransfer {
            direction: Direction::Upload,
            src_machine: "local".into(),
            src_path: format!("/home/me/{dst}"),
            dst_machine: "m:dev".into(),
            dst_dir: "/srv".into(),
            dst_name: dst.into(),
            overwrite: false,
        }
    }

    #[test]
    fn at_most_two_run_and_never_two_into_one_place() {
        let mut q = Queue::default();
        let a = q.add(job("a"));
        let a2 = q.add(job("a"));
        let b = q.add(job("b"));
        let c = q.add(job("c"));
        assert_eq!(q.next_to_start(), Some(a));
        // a2 goes where a goes: b first.
        assert_eq!(q.next_to_start(), Some(b));
        assert_eq!(q.next_to_start(), None);
        q.finished(a, Ok(()));
        assert_eq!(q.next_to_start(), Some(a2));
        assert_eq!(q.next_to_start(), None);
        q.finished(b, Err("lost".into()));
        assert_eq!(q.next_to_start(), Some(c));
        assert_eq!(q.active(), 2);
        assert_eq!(q.get(b).unwrap().state, State::Failed("lost".into()));
    }

    #[test]
    fn cancel_and_retry() {
        let mut q = Queue::default();
        let a = q.add(job("a"));
        let b = q.add(job("b"));
        assert_eq!(q.next_to_start(), Some(a));
        // Queued: canceled at once; running: asked to stop.
        assert!(!q.cancel(b));
        assert_eq!(q.get(b).unwrap().state, State::Canceled);
        assert!(q.cancel(a));
        assert!(q.get(a).unwrap().cancel_requested);
        q.finished(a, Err("canceled".into()));
        assert_eq!(q.get(a).unwrap().state, State::Canceled);
        // Retry after a failure resumes; after a cancel it starts over.
        let c = q.add(job("c"));
        q.next_to_start();
        q.finished(c, Err("lost".into()));
        q.retry(c);
        assert_eq!(q.get(c).unwrap().state, State::Queued);
        assert!(q.get(c).unwrap().resume);
        q.retry(a);
        assert!(!q.get(a).unwrap().resume);
        assert!(!q.get(a).unwrap().cancel_requested);
        q.clear_finished();
        assert!(q.items().iter().all(|t| matches!(t.state, State::Queued | State::Running)));
    }

    #[test]
    fn speed_over_the_window() {
        let mut q = Queue::default();
        let a = q.add(job("a"));
        q.next_to_start();
        let t0 = Instant::now();
        q.progress(a, 0, 10_000_000, t0);
        q.progress(a, 1_000_000, 10_000_000, t0 + Duration::from_secs(1));
        q.progress(a, 3_000_000, 10_000_000, t0 + Duration::from_secs(2));
        let s = q.get(a).unwrap().speed;
        assert!((s - 1_500_000.0).abs() < 1.0, "{s}");
        // Old samples drop out of the window.
        q.progress(a, 9_000_000, 10_000_000, t0 + Duration::from_secs(6));
        assert!((q.get(a).unwrap().speed - 1_500_000.0).abs() > 1.0);
        assert_eq!(q.get(a).unwrap().done, 9_000_000);
    }
}
