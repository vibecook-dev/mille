//! The per-worktree status queue (SPEC mille-git §12).
//!
//! **Per worktree, not global.** git's lock is `index.lock`, and there is
//! one index per worktree: a linked worktree's status can never contend with
//! its primary's. A process-global queue would serialize eight agents'
//! worktrees behind one another for no reason, so every session owns its own
//! queue and statuses of different worktrees run concurrently (AC-008).
//!
//! **At most one status per worktree is computing at a time, and requests
//! that arrive meanwhile coalesce instead of piling up.** An agent that
//! writes 40 files in a second produces 40 hints; they union into one
//! pending set, which runs once — immediately when the in-flight run ends,
//! because those hints already waited out a run (§6.2, §12.2).
//!
//! **Drop-and-replace is about in-flight *compute*, not the cache.** A run
//! whose result has been invalidated (a blocking mutation started while it
//! was computing — PR 4) is discarded without publishing, and the next run
//! takes over. A run that completes normally is always published, and
//! publishing *merges* a path-limited result into the previous snapshot
//! (`status.rs`, §10.6); "replace" never means "replace the list with
//! whatever one path-limited run saw".
//!
//! Debounce: hints arm one timer per session — `debounce` after the latest
//! hint, but never later than `max_wait` after the first, so a continuous
//! stream of writes still refreshes. A path-limited publish arms an idle
//! timer; if nothing else ran when it fires, a full rescan picks up
//! untracked files no hint asked about (§21.1).
//!
//! `hint` is synchronous and only touches a mutex and the runtime's spawn
//! queue: safe on the explorer's notify thread while it holds `policy_gate`
//! (§11.2 rule 5). The status itself always runs on the engine runtime.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::session::Session;
use crate::status::MAX_HINT_PATHS;
use crate::Inner;

/// What a status run should look at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Hints {
    /// The whole repository.
    Full,
    /// Worktree-relative POSIX paths.
    Paths(BTreeSet<String>),
}

impl Hints {
    pub(crate) fn paths(paths: impl IntoIterator<Item = String>) -> Self {
        Hints::Paths(BTreeSet::new()).absorb(Hints::Paths(paths.into_iter().collect()))
    }

    /// Union two hint sets. A full request wins; so does a set too large for
    /// a path-limited run to be cheaper (§10.3).
    fn absorb(self, other: Hints) -> Hints {
        match (self, other) {
            (Hints::Paths(mut a), Hints::Paths(b)) => {
                a.extend(b);
                if a.is_empty() || a.len() > MAX_HINT_PATHS {
                    Hints::Full
                } else {
                    Hints::Paths(a)
                }
            }
            _ => Hints::Full,
        }
    }

    pub(crate) fn as_pathspecs(&self) -> Option<Vec<String>> {
        match self {
            Hints::Full => None,
            Hints::Paths(paths) => Some(paths.iter().cloned().collect()),
        }
    }
}

/// When the queued work should start.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Urgency {
    /// After the debounce window (explorer and gitdir events).
    Debounced,
    /// As soon as the queue is free (open, an explicit refresh, after a
    /// blocking mutation).
    Now,
}

#[derive(Default)]
struct State {
    pending: Option<Hints>,
    first_hint: Option<Instant>,
    last_hint: Option<Instant>,
    urgent: bool,
    /// A driver task exists (sleeping on the debounce, or about to run).
    driver: bool,
    /// A status is computing.
    running: bool,
    /// The running status's gix interrupt flag.
    interrupt: Option<Arc<AtomicBool>>,
    /// Runs started so far; a run's sequence number is the value after its
    /// start. Waiters compare against it.
    started: u64,
    /// Bumped to invalidate the in-flight run (drop-and-replace).
    epoch: u64,
}

/// One session's status scheduler.
#[derive(Default)]
pub(crate) struct StatusQueue {
    state: Mutex<State>,
}

impl StatusQueue {
    /// `None` when idle with a published snapshot available; otherwise the
    /// run sequence whose publish brings the snapshot up to date.
    pub(crate) fn settle_target(&self) -> Option<u64> {
        let state = self.state.lock();
        if state.pending.is_some() || state.driver {
            Some(state.started + 1)
        } else if state.running {
            Some(state.started)
        } else {
            None
        }
    }
}

impl Session {
    /// Queue a status run. Synchronous and non-blocking. Returns the
    /// sequence number of the run that will carry these hints — the next run
    /// to start, read under the same lock that queued them.
    pub(crate) fn hint(
        self: &Arc<Self>,
        inner: &Arc<Inner>,
        hints: Hints,
        urgency: Urgency,
    ) -> u64 {
        let mut state = self.queue.state.lock();
        let target = state.started + 1;
        if self.cancel.is_cancelled() {
            return target;
        }
        state.pending = Some(match state.pending.take() {
            Some(pending) => pending.absorb(hints),
            None => hints,
        });
        let now = Instant::now();
        state.first_hint.get_or_insert(now);
        state.last_hint = Some(now);
        state.urgent |= urgency == Urgency::Now;
        // A running status picks the pending set up when it ends; an armed
        // driver will see it when its timer fires.
        if state.driver || state.running {
            return target;
        }
        state.driver = true;
        drop(state);
        let (session, engine) = (self.clone(), inner.clone());
        inner
            .handle
            .spawn(async move { session.drive(engine).await });
        target
    }

    /// The driver: wait out the debounce, run, publish, and keep going while
    /// hints arrived during the run.
    async fn drive(self: Arc<Self>, inner: Arc<Inner>) {
        loop {
            // 1. Debounce.
            loop {
                let deadline = {
                    let state = self.queue.state.lock();
                    if state.urgent {
                        break;
                    }
                    let (Some(first), Some(last)) = (state.first_hint, state.last_hint) else {
                        break;
                    };
                    (last + inner.options.debounce).min(first + inner.options.max_wait)
                };
                if deadline <= Instant::now() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline.into()) => {}
                    _ = self.cancel.cancelled() => {
                        self.queue.state.lock().driver = false;
                        return;
                    }
                }
            }

            // 2. Take the pending set.
            let (hints, interrupt, epoch, sequence) = {
                let mut state = self.queue.state.lock();
                let Some(hints) = state.pending.take() else {
                    state.driver = false;
                    return;
                };
                state.first_hint = None;
                state.last_hint = None;
                state.urgent = false;
                state.driver = false;
                state.running = true;
                state.started += 1;
                let interrupt = Arc::new(AtomicBool::new(false));
                state.interrupt = Some(interrupt.clone());
                (hints, interrupt, state.epoch, state.started)
            };

            // 3. Compute, outside every lock.
            let in_flight = inner.metrics.run_started();
            let delay = inner.metrics.status_delay();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let pathspecs = hints
                .as_pathspecs()
                .and_then(|paths| self.widen_with_renames(paths));
            let run = tokio::select! {
                run = self.compute_status(&inner, pathspecs.clone(), interrupt) => Some(run),
                _ = self.cancel.cancelled() => None,
            };
            drop(in_flight);

            // 4. Publish unless the run was invalidated meanwhile.
            let mut published = None;
            let rerun = {
                let mut state = self.queue.state.lock();
                state.running = false;
                state.interrupt = None;
                if let (Some(run), true) = (run, state.epoch == epoch) {
                    if !self.cancel.is_cancelled() {
                        published = Some(self.publish(
                            run,
                            pathspecs.as_deref(),
                            sequence,
                            inner.options.status_row_cap,
                        ));
                    }
                }
                let rerun = state.pending.is_some() && !self.cancel.is_cancelled();
                if rerun {
                    // Drop-and-replace: the hints that arrived during the
                    // run have already waited; start them now.
                    state.urgent = true;
                    state.driver = true;
                }
                rerun
            };
            if let Some(snapshot) = published {
                self.notify_status(&snapshot);
                if snapshot.path_limited && !rerun {
                    self.arm_idle_rescan(&inner, sequence);
                }
            }
            if !rerun {
                return;
            }
        }
    }

    /// After a path-limited publish, run a full status if the session stays
    /// idle for `idle_rescan`: path-limited runs never discover untracked
    /// files outside their hints (§10.6, §21.1).
    fn arm_idle_rescan(self: &Arc<Self>, inner: &Arc<Inner>, sequence: u64) {
        let (session, engine) = (self.clone(), inner.clone());
        let idle = inner.options.idle_rescan;
        inner.handle.spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(idle) => {}
                _ = session.cancel.cancelled() => return,
            }
            let quiet = {
                let state = session.queue.state.lock();
                state.started == sequence && !state.running && state.pending.is_none()
            };
            if quiet {
                session.hint(&engine, Hints::Full, Urgency::Now);
            }
        });
    }

    /// Stop the in-flight gix walk (session close).
    pub(crate) fn interrupt_status(&self) {
        let mut state = self.queue.state.lock();
        state.epoch += 1;
        if let Some(flag) = &state.interrupt {
            flag.store(true, Ordering::Release);
        }
    }
}

/// Engine-wide counters the tests read (doc-hidden on `GitEngine`).
#[derive(Default)]
pub(crate) struct Metrics {
    started: std::sync::atomic::AtomicU64,
    in_flight: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    delay_ms: std::sync::atomic::AtomicU64,
}

pub(crate) struct InFlight<'a>(&'a Metrics);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Metrics {
    fn run_started(&self) -> InFlight<'_> {
        self.started.fetch_add(1, Ordering::AcqRel);
        let now = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(now, Ordering::AcqRel);
        InFlight(self)
    }

    fn status_delay(&self) -> Duration {
        Duration::from_millis(self.delay_ms.load(Ordering::Acquire))
    }

    pub(crate) fn set_status_delay(&self, delay: Duration) {
        self.delay_ms
            .store(delay.as_millis() as u64, Ordering::Release);
    }

    pub(crate) fn runs_started(&self) -> u64 {
        self.started.load(Ordering::Acquire)
    }

    pub(crate) fn peak_concurrency(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_union_and_escalate_to_full() {
        let a = Hints::paths(["a.ts".to_owned()]);
        let ab = a.clone().absorb(Hints::paths(["b.ts".to_owned()]));
        assert_eq!(
            ab.as_pathspecs(),
            Some(vec!["a.ts".to_owned(), "b.ts".to_owned()])
        );
        assert_eq!(ab.clone().absorb(Hints::Full), Hints::Full);
        assert_eq!(Hints::Full.absorb(a), Hints::Full);
        let many = Hints::paths((0..=MAX_HINT_PATHS).map(|i| format!("f{i}")));
        assert_eq!(many, Hints::Full);
        assert_eq!(
            Hints::paths(Vec::<String>::new()),
            Hints::Full,
            "an empty hint set is a full run"
        );
    }
}
