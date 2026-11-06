//! The transport contract, the loop that carries out sync actions over it,
//! and `MemoryNetwork`, an in-process transport for tests.
//!
//! A transport moves two things: frames, in order per peer, and objects,
//! which it pulls from a holder's `objects_root()` joined with
//! `object_relpath(digest)` and lands on this drive's volume. It verifies
//! nothing — the drive does (DR-L3) — and it never retries a pull: the drive
//! picks the next holder.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::digest::Blake3;
use crate::drive::Drive;
use crate::error::{DriveError, DriveErrorCode};
use crate::sync::SyncAction;

/// A transport's failure, in its own words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

pub trait Transport {
    /// Send a frame to `to`. Frames to one peer arrive in the order sent.
    fn send(&self, to: &str, frame: Vec<u8>) -> Result<(), TransportError>;

    /// Pull the object `digest` from `from`, landing it at `into` or anywhere
    /// else on the drive's volume. Returns where it landed.
    fn pull(&self, from: &str, digest: &Blake3, into: &Path) -> Result<PathBuf, TransportError>;
}

/// What `apply_actions` did.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub sent: usize,
    pub pulled: usize,
    /// Failures along the way: refused copies (`EMISMATCH`), transport
    /// errors. None of them stopped the loop.
    pub errors: Vec<DriveError>,
}

fn transport_error(err: &TransportError) -> DriveError {
    DriveError::new(DriveErrorCode::Transport, err.0.clone())
}

/// Carry out `actions`, and every action they lead to, until none is left.
/// Synchronous: each pull runs to completion before the next action.
pub fn apply_actions(
    drive: &Drive,
    transport: &impl Transport,
    actions: Vec<SyncAction>,
) -> ApplyReport {
    let mut queue: VecDeque<SyncAction> = actions.into();
    let mut report = ApplyReport::default();
    while let Some(action) = queue.pop_front() {
        match action {
            SyncAction::Send { to, frame } => match transport.send(&to, frame) {
                Ok(()) => report.sent += 1,
                Err(err) => report.errors.push(transport_error(&err)),
            },
            SyncAction::Pull { from, digest, into } => {
                let landed = transport.pull(&from, &digest, &into);
                let outcome = match landed {
                    Ok(path) => drive.sync_pulled(&from, &digest, &path),
                    Err(err) => Err(transport_error(&err)),
                };
                let next = match outcome {
                    Ok(next) => {
                        report.pulled += 1;
                        Ok(next)
                    }
                    Err(err) => {
                        report.errors.push(err);
                        drive.sync_pull_failed(&from, &digest)
                    }
                };
                match next {
                    Ok(next) => queue.extend(next),
                    Err(err) => report.errors.push(err),
                }
            }
        }
    }
    report
}

/// The most frames `MemoryNetwork::run_until_quiet` delivers before it calls
/// the exchange a livelock.
const MAX_DELIVERIES: usize = 100_000;

#[derive(Default)]
struct Net {
    drives: BTreeMap<String, Arc<Drive>>,
    /// (from, to, frame), in send order.
    queue: VecDeque<(String, String, Vec<u8>)>,
    /// Pairs that cannot reach each other, smaller name first.
    cut: BTreeSet<(String, String)>,
    corrupt_next: BTreeSet<Blake3>,
    errors: Vec<(String, DriveError)>,
}

fn pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

/// Every registered drive connected to every other, in one process: frames
/// queue until `run_until_quiet` delivers them, and a pull copies straight
/// from the holder's object. Fault hooks cut a pair off, or damage the next
/// copy of an object.
#[derive(Clone, Default)]
pub struct MemoryNetwork {
    net: Arc<Mutex<Net>>,
}

/// One drive's view of a `MemoryNetwork`.
pub struct MemoryTransport {
    net: Arc<Mutex<Net>>,
    me: String,
}

impl Transport for MemoryTransport {
    fn send(&self, to: &str, frame: Vec<u8>) -> Result<(), TransportError> {
        let mut net = self.net.lock();
        if !net.drives.contains_key(to) {
            return Err(TransportError(format!("no peer {to}")));
        }
        // A cut link drops frames silently, as a lost connection would.
        if !net.cut.contains(&pair(&self.me, to)) {
            net.queue.push_back((self.me.clone(), to.to_owned(), frame));
        }
        Ok(())
    }

    fn pull(&self, from: &str, digest: &Blake3, into: &Path) -> Result<PathBuf, TransportError> {
        let (source, corrupt) = {
            let mut net = self.net.lock();
            if net.cut.contains(&pair(&self.me, from)) {
                return Err(TransportError(format!("{from} is unreachable")));
            }
            let Some(holder) = net.drives.get(from) else {
                return Err(TransportError(format!("no peer {from}")));
            };
            let Some(source) = holder.object_path(digest) else {
                return Err(TransportError(format!("{from} does not hold {digest}")));
            };
            let corrupt = net.corrupt_next.remove(digest);
            (source, corrupt)
        };
        let failed = |err: std::io::Error| TransportError(err.to_string());
        let mut bytes = fs::read(source).map_err(failed)?;
        if corrupt {
            match bytes.first_mut() {
                Some(first) => *first ^= 0xff,
                None => bytes.push(0),
            }
        }
        if let Some(parent) = into.parent() {
            fs::create_dir_all(parent).map_err(failed)?;
        }
        fs::write(into, bytes).map_err(failed)?;
        Ok(into.to_path_buf())
    }
}

impl MemoryNetwork {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a drive under the peer name `peer`.
    pub fn register(&self, peer: &str, drive: Arc<Drive>) {
        self.net.lock().drives.insert(peer.to_owned(), drive);
    }

    pub fn transport(&self, peer: &str) -> MemoryTransport {
        MemoryTransport {
            net: Arc::clone(&self.net),
            me: peer.to_owned(),
        }
    }

    fn drive(&self, peer: &str) -> Arc<Drive> {
        let drive = self.net.lock().drives.get(peer).cloned();
        drive.unwrap_or_else(|| panic!("no drive registered as {peer}"))
    }

    fn record(&self, peer: &str, errors: Vec<DriveError>) {
        let mut net = self.net.lock();
        for err in errors {
            net.errors.push((peer.to_owned(), err));
        }
    }

    /// Run `peer`'s actions over its transport.
    pub fn apply(&self, peer: &str, actions: Vec<SyncAction>) {
        let drive = self.drive(peer);
        let report = apply_actions(&drive, &self.transport(peer), actions);
        self.record(peer, report.errors);
    }

    fn step(&self, peer: &str, actions: crate::error::Result<Vec<SyncAction>>) {
        match actions {
            Ok(actions) => self.apply(peer, actions),
            Err(err) => self.record(peer, vec![err]),
        }
    }

    /// Open `a` and `b` to each other: each greets the other.
    pub fn connect(&self, a: &str, b: &str) {
        let (one, two) = (self.drive(a), self.drive(b));
        self.step(a, one.sync_open(b));
        self.step(b, two.sync_open(a));
    }

    /// Close `a` and `b` to each other.
    pub fn disconnect(&self, a: &str, b: &str) {
        self.drive(a).sync_close(b);
        self.drive(b).sync_close(a);
    }

    /// Tell `peer`'s drive it changed locally.
    pub fn local_changed(&self, peer: &str) {
        let drive = self.drive(peer);
        self.step(peer, drive.sync_local_changed());
    }

    /// Run `peer`'s periodic tick.
    pub fn tick(&self, peer: &str) {
        let drive = self.drive(peer);
        self.step(peer, drive.sync_tick());
    }

    /// Cut (or restore) the link between `a` and `b`. A cut drops frames
    /// already queued between them and fails pulls.
    pub fn partition(&self, a: &str, b: &str, cut: bool) {
        let mut net = self.net.lock();
        let key = pair(a, b);
        if cut {
            net.queue.retain(|(from, to, _)| pair(from, to) != key);
            net.cut.insert(key);
        } else {
            net.cut.remove(&key);
        }
    }

    /// Damage the next copy of `digest` any pull makes.
    pub fn corrupt_next_pull(&self, digest: Blake3) {
        self.net.lock().corrupt_next.insert(digest);
    }

    /// Every error recorded since the last call, with the peer it happened on.
    pub fn take_errors(&self) -> Vec<(String, DriveError)> {
        std::mem::take(&mut self.net.lock().errors)
    }

    /// Deliver queued frames, and everything they lead to, until the network
    /// is quiet. Returns how many frames were delivered. Panics on a livelock.
    pub fn run_until_quiet(&self) -> usize {
        for delivered in 0..MAX_DELIVERIES {
            let (from, to, frame, drive) = {
                let mut net = self.net.lock();
                let Some((from, to, frame)) = net.queue.pop_front() else {
                    return delivered;
                };
                let drive = net.drives.get(&to).cloned();
                (from, to, frame, drive)
            };
            if let Some(drive) = drive {
                self.step(&to, drive.sync_handle(&from, &frame));
            }
        }
        panic!("the memory network did not go quiet after {MAX_DELIVERIES} frames");
    }
}
