//! The sync core (DR-D7 (a)): frames in, actions out, no I/O of its own.
//!
//! A host owns the transport — VibeField over field-native's one truffle
//! node, mille-truffle over its own — and runs a loop: hand the drive each
//! frame and peer event, and carry out the actions it returns. Two kinds:
//! send a frame to a peer, and pull an object from a peer. The drive decides
//! everything else: what each peer lacks, which holder to pull from, what to
//! retry.
//!
//! **Frames.** One version byte, then a bincode body:
//!
//! - `Hello {vv}`: the sender's version vector. The receiver answers with the
//!   changes the sender lacks, and registers the sender if it was not open.
//! - `Updates {bytes}`: a Loro update. One that would not fit in
//!   `max_frame_bytes` travels as `UpdatesChunk {id, seq, total, bytes}`,
//!   reassembled in order before it is imported.
//! - `Want {digests}`: objects the sender wants, at most 1024 per frame. The
//!   receiver answers `Have` for those it holds, and remembers the rest so it
//!   can answer when it gets them.
//! - `Have {digests}`: objects the sender holds and the receiver wanted.
//!
//! **Pulls.** A `Pull {from, digest, into}` asks the host to fetch the object
//! from `from`'s `objects_root()` joined with `object_relpath(digest)`, land
//! it anywhere on this drive's volume (`into` is a free name in staging), and
//! report back: `sync_pulled` with the landed path, which verifies it (DR-L3),
//! or `sync_pull_failed`. Either way the drive picks the next holder itself;
//! a host never retries on its own.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};

use loro::VersionVector;
use serde::{Deserialize, Serialize};

use crate::digest::Blake3;
use crate::drive::{catalog, ChangeKind, Drive};
use crate::error::{DriveError, Result};

/// The first byte of every frame.
pub const FRAME_VERSION: u8 = 1;
/// The most digests one `Want` or `Have` carries.
pub const MAX_DIGESTS_PER_FRAME: usize = 1024;
/// Room a frame needs beyond its payload: the version byte, the variant, the
/// chunk header and a length prefix.
const FRAME_OVERHEAD: usize = 64;
/// The most chunks one update may be cut into.
const MAX_CHUNKS: u32 = 1 << 20;
/// The most outstanding wants this drive remembers per peer.
const MAX_PEER_WANTED: usize = 65_536;
/// bincode's allocation ceiling while decoding one frame.
const FRAME_DECODE_LIMIT: usize = 1 << 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Frame {
    Hello { vv: Vec<u8> },
    Updates { bytes: Vec<u8> },
    UpdatesChunk {
        id: u64,
        seq: u32,
        total: u32,
        bytes: Vec<u8>,
    },
    Want { digests: Vec<[u8; 32]> },
    Have { digests: Vec<[u8; 32]> },
}

/// What the host does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Send `frame` to the peer `to`.
    Send { to: String, frame: Vec<u8> },
    /// Pull the object `digest` from the peer `from`, land it at `into` (or
    /// anywhere on this drive's volume), then report with `sync_pulled` or
    /// `sync_pull_failed`.
    Pull {
        from: String,
        digest: Blake3,
        into: PathBuf,
    },
}

/// Where sync stands, for an honest status line (DR-L8).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub peers: Vec<String>,
    /// Pulls in flight.
    pub pulling: usize,
    /// Objects wanted and not held.
    pub wants: usize,
    /// Wants no open peer has offered: waiting for a holder to appear.
    pub unsourced: usize,
}

#[derive(Default)]
pub(crate) struct SyncState {
    peers: BTreeMap<String, PeerState>,
    /// Peers that said `Have`, per wanted object, in the order they said it.
    holders: BTreeMap<Blake3, Vec<String>>,
    /// Peers a pull of the object failed from.
    failed: BTreeMap<Blake3, BTreeSet<String>>,
    /// The peer each object is being pulled from.
    inflight: BTreeMap<Blake3, String>,
    /// Wants already asked of every open peer.
    asked: BTreeSet<Blake3>,
    next_chunk_id: u64,
}

#[derive(Default)]
struct PeerState {
    /// What this drive knows the peer has: its `Hello`, advanced by what was
    /// sent to it since.
    vv: Option<VersionVector>,
    /// Objects the peer asked for that this drive did not hold.
    wanted: BTreeSet<Blake3>,
    assembly: Option<Assembly>,
}

struct Assembly {
    id: u64,
    total: u32,
    next: u32,
    bytes: Vec<u8>,
}

enum Trigger<'a> {
    Open(&'a str),
    Hello(&'a str, VersionVector),
    Imported {
        from: &'a str,
        before: VersionVector,
        after: VersionVector,
    },
    Local,
    Want(&'a str, Vec<Blake3>),
    Have(&'a str, Vec<Blake3>),
    Pulled(Blake3),
    PullFailed(&'a str, Blake3),
    Tick,
}

pub(crate) fn encode_frame(frame: &Frame) -> Vec<u8> {
    let config = bincode::config::standard();
    let encoded = bincode::serde::encode_to_vec(frame, config);
    let body = encoded.expect("a drive frame always encodes");
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(FRAME_VERSION);
    out.extend_from_slice(&body);
    out
}

pub(crate) fn decode_frame(bytes: &[u8]) -> Result<Frame> {
    let Some((&version, body)) = bytes.split_first() else {
        return Err(DriveError::proto("an empty frame"));
    };
    if version != FRAME_VERSION {
        let message = format!("frame version {version}; this drive speaks {FRAME_VERSION}");
        return Err(DriveError::proto(message));
    }
    let config = bincode::config::standard().with_limit::<FRAME_DECODE_LIMIT>();
    let decoded = bincode::serde::decode_from_slice::<Frame, _>(body, config);
    let (frame, used) = match decoded {
        Ok(done) => done,
        Err(err) => return Err(DriveError::format(format!("a frame did not decode: {err}"))),
    };
    if used != body.len() {
        return Err(DriveError::format("a frame has bytes after its end"));
    }
    Ok(frame)
}

fn decode_vv(bytes: &[u8]) -> Result<VersionVector> {
    match catch_unwind(|| VersionVector::decode(bytes)) {
        Ok(Ok(vv)) => Ok(vv),
        _ => Err(DriveError::format("a version vector did not decode")),
    }
}

fn digests_of(raw: Vec<[u8; 32]>) -> Result<Vec<Blake3>> {
    let count = raw.len();
    if count > MAX_DIGESTS_PER_FRAME {
        let message = format!("a frame names {count} digests, over {MAX_DIGESTS_PER_FRAME}");
        return Err(DriveError::proto(message));
    }
    Ok(raw.into_iter().map(Blake3::from_bytes).collect())
}

fn send(to: &str, frame: &Frame) -> SyncAction {
    SyncAction::Send {
        to: to.to_owned(),
        frame: encode_frame(frame),
    }
}

fn hello(to: &str, ours: &VersionVector) -> SyncAction {
    send(to, &Frame::Hello { vv: ours.encode() })
}

fn digests_per_frame(max_frame: usize) -> usize {
    let room = max_frame.saturating_sub(FRAME_OVERHEAD) / 32;
    room.clamp(1, MAX_DIGESTS_PER_FRAME)
}

/// `Want` (or, with `have`, `Have`) frames for `digests`, split to fit.
fn digest_frames(out: &mut Vec<SyncAction>, to: &str, digests: &[Blake3], have: bool, per: usize) {
    for chunk in digests.chunks(per) {
        let digests: Vec<[u8; 32]> = chunk.iter().map(|d| *d.as_bytes()).collect();
        let frame = if have {
            Frame::Have { digests }
        } else {
            Frame::Want { digests }
        };
        out.push(send(to, &frame));
    }
}

/// An update as one `Updates` frame, or as chunks when it would not fit.
fn update_frames(
    sync: &mut SyncState,
    to: &str,
    bytes: Vec<u8>,
    max_frame: usize,
) -> Vec<SyncAction> {
    if bytes.len() + FRAME_OVERHEAD <= max_frame {
        return vec![send(to, &Frame::Updates { bytes })];
    }
    let chunk = max_frame - FRAME_OVERHEAD;
    let total = bytes.len().div_ceil(chunk) as u32;
    sync.next_chunk_id += 1;
    let id = sync.next_chunk_id;
    let mut out = Vec::new();
    for (seq, part) in bytes.chunks(chunk).enumerate() {
        let frame = Frame::UpdatesChunk {
            id,
            seq: seq as u32,
            total,
            bytes: part.to_vec(),
        };
        out.push(send(to, &frame));
    }
    out
}

impl Drive {
    /// This drive's version vector, encoded.
    pub fn version_vector(&self) -> Vec<u8> {
        self.doc.lock().oplog_vv().encode()
    }

    /// Every change this drive has that the encoded version vector `vv` does
    /// not.
    pub fn export_updates_since(&self, vv: &[u8]) -> Result<Vec<u8>> {
        let since = decode_vv(vv)?;
        self.doc.lock().export_updates_since(&since)
    }

    /// A peer is reachable: greet it, and ask it for what this drive wants.
    pub fn sync_open(&self, peer: &str) -> Result<Vec<SyncAction>> {
        self.react(Trigger::Open(peer))
    }

    /// A frame from `peer`. `EPROTO` or `EFORMAT` for a frame this drive
    /// cannot accept; the peer stays open.
    pub fn sync_handle(&self, peer: &str, frame: &[u8]) -> Result<Vec<SyncAction>> {
        match decode_frame(frame)? {
            Frame::Hello { vv } => {
                let theirs = decode_vv(&vv)?;
                self.react(Trigger::Hello(peer, theirs))
            }
            Frame::Updates { bytes } => self.on_updates(peer, &bytes),
            Frame::UpdatesChunk {
                id,
                seq,
                total,
                bytes,
            } => match self.assemble(peer, id, seq, total, bytes)? {
                Some(bytes) => self.on_updates(peer, &bytes),
                None => Ok(Vec::new()),
            },
            Frame::Want { digests } => self.react(Trigger::Want(peer, digests_of(digests)?)),
            Frame::Have { digests } => self.react(Trigger::Have(peer, digests_of(digests)?)),
        }
    }

    /// Something changed locally: send each open peer what it lacks, and ask
    /// for any object the change made wanted.
    pub fn sync_local_changed(&self) -> Result<Vec<SyncAction>> {
        self.react(Trigger::Local)
    }

    /// A pull landed at `landed`. The file is consumed: verified and admitted
    /// as the object, or refused (`EMISMATCH`) and removed. On a refusal the
    /// host calls `sync_pull_failed`, which picks the next holder.
    pub fn sync_pulled(
        &self,
        peer: &str,
        digest: &Blake3,
        landed: &Path,
    ) -> Result<Vec<SyncAction>> {
        if let Err(err) = self.store.admit(digest, None, landed) {
            self.forget_pull(peer, digest);
            return Err(err);
        }
        let actions = self.react(Trigger::Pulled(*digest))?;
        let items = self.items_naming(digest);
        if !items.is_empty() {
            self.emit(ChangeKind::Held, items);
        }
        Ok(actions)
    }

    /// A pull from `peer` failed (the transport refused it, or its copy was
    /// refused). The drive tries the next holder that offered the object;
    /// when every one has failed, `sync_tick` starts over.
    pub fn sync_pull_failed(&self, peer: &str, digest: &Blake3) -> Result<Vec<SyncAction>> {
        self.react(Trigger::PullFailed(peer, *digest))
    }

    /// Periodically (the host picks the period): greet every peer again, so
    /// one that missed a frame catches up by version vector, retry pulls whose
    /// every holder failed, and re-ask for wants no peer has offered.
    pub fn sync_tick(&self) -> Result<Vec<SyncAction>> {
        self.react(Trigger::Tick)
    }

    /// A peer went away.
    pub fn sync_close(&self, peer: &str) {
        let mut guard = self.sync.lock();
        let sync = &mut *guard;
        sync.peers.remove(peer);
        for holders in sync.holders.values_mut() {
            holders.retain(|holder| holder != peer);
        }
        sync.inflight.retain(|_, from| from != peer);
    }

    pub fn sync_status(&self) -> SyncStatus {
        let wants = self.wants();
        let sync = self.sync.lock();
        let mut unsourced = 0;
        for digest in &wants {
            let offered = sync.holders.get(digest).is_some_and(|h| !h.is_empty());
            if !offered && !sync.inflight.contains_key(digest) {
                unsourced += 1;
            }
        }
        SyncStatus {
            peers: sync.peers.keys().cloned().collect(),
            pulling: sync.inflight.len(),
            wants: wants.len(),
            unsourced,
        }
    }

    fn on_updates(&self, peer: &str, bytes: &[u8]) -> Result<Vec<SyncAction>> {
        let imported = self.import_inner(bytes)?;
        let trigger = Trigger::Imported {
            from: peer,
            before: imported.before,
            after: imported.after,
        };
        let actions = self.react(trigger)?;
        if imported.report.changed {
            self.emit(ChangeKind::Import, imported.report.items);
        }
        Ok(actions)
    }

    /// Add a chunk to `peer`'s update in progress; the whole update once its
    /// last chunk arrives. Chunks arrive in order on one peer's stream, so any
    /// gap or mix-up drops the update (the next `Hello` re-sends it).
    fn assemble(
        &self,
        peer: &str,
        id: u64,
        seq: u32,
        total: u32,
        bytes: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let max = self.options.max_update_bytes;
        let mut sync = self.sync.lock();
        let state = sync.peers.entry(peer.to_owned()).or_default();
        if !(2..=MAX_CHUNKS).contains(&total) || seq >= total {
            state.assembly = None;
            let message = format!("chunk {seq} of {total} is out of range");
            return Err(DriveError::proto(message));
        }
        if seq == 0 {
            state.assembly = Some(Assembly {
                id,
                total,
                next: 0,
                bytes: Vec::new(),
            });
        }
        let Some(assembly) = state.assembly.as_mut() else {
            return Err(DriveError::proto("a chunk arrived without its first"));
        };
        if assembly.id != id || assembly.total != total || assembly.next != seq {
            state.assembly = None;
            return Err(DriveError::proto("an update's chunks arrived out of order"));
        }
        if assembly.bytes.len() + bytes.len() > max {
            state.assembly = None;
            let message = format!("an update is over the drive's {max}-byte limit");
            return Err(DriveError::proto(message));
        }
        assembly.bytes.extend_from_slice(&bytes);
        assembly.next += 1;
        if assembly.next < assembly.total {
            return Ok(None);
        }
        Ok(state.assembly.take().map(|done| done.bytes))
    }

    fn forget_pull(&self, peer: &str, digest: &Blake3) {
        let mut guard = self.sync.lock();
        let sync = &mut *guard;
        if sync.inflight.get(digest).is_some_and(|from| from == peer) {
            sync.inflight.remove(digest);
        }
        let failed = sync.failed.entry(*digest).or_default();
        failed.insert(peer.to_owned());
    }

    /// Apply one trigger, then do what every trigger implies: send each peer
    /// the changes it lacks, ask for new wants, answer peers whose wants this
    /// drive now holds, and start pulls up to the limit.
    fn react(&self, trigger: Trigger<'_>) -> Result<Vec<SyncAction>> {
        let max_frame = self.options.max_frame_bytes;
        let per = digests_per_frame(max_frame);
        let doc = self.doc.lock();
        let ours = doc.oplog_vv();
        let items = catalog(&doc.state(), |digest| self.store.has(digest));
        let wants = self.missing(&items);
        let all_wants: Vec<Blake3> = wants.iter().copied().collect();
        let mut guard = self.sync.lock();
        let sync = &mut *guard;
        let mut out = Vec::new();

        match trigger {
            Trigger::Open(peer) => {
                sync.peers.entry(peer.to_owned()).or_default();
                out.push(hello(peer, &ours));
                digest_frames(&mut out, peer, &all_wants, false, per);
            }
            Trigger::Hello(peer, theirs) => {
                if !sync.peers.contains_key(peer) {
                    out.push(hello(peer, &ours));
                    digest_frames(&mut out, peer, &all_wants, false, per);
                }
                let state = sync.peers.entry(peer.to_owned()).or_default();
                state.vv = Some(theirs);
            }
            Trigger::Imported {
                from,
                before,
                after,
            } => {
                // The peer sent this update and already had everything this
                // drive had before it: it has everything this drive has after.
                if let Some(state) = sync.peers.get_mut(from) {
                    if state.vv.as_ref().is_some_and(|vv| vv.includes_vv(&before)) {
                        state.vv = Some(after);
                    }
                }
            }
            Trigger::Local => {}
            Trigger::Want(peer, digests) => {
                let (held, missing): (Vec<Blake3>, Vec<Blake3>) =
                    digests.into_iter().partition(|d| self.store.has(d));
                digest_frames(&mut out, peer, &held, true, per);
                if let Some(state) = sync.peers.get_mut(peer) {
                    for digest in missing {
                        if state.wanted.len() >= MAX_PEER_WANTED {
                            break;
                        }
                        state.wanted.insert(digest);
                    }
                }
            }
            Trigger::Have(peer, digests) => {
                for digest in digests {
                    if !wants.contains(&digest) {
                        continue;
                    }
                    let holders = sync.holders.entry(digest).or_default();
                    if !holders.iter().any(|holder| holder == peer) {
                        holders.push(peer.to_owned());
                    }
                }
            }
            Trigger::Pulled(digest) => {
                sync.inflight.remove(&digest);
                sync.holders.remove(&digest);
                sync.failed.remove(&digest);
                sync.asked.remove(&digest);
            }
            Trigger::PullFailed(peer, digest) => {
                if sync.inflight.get(&digest).is_some_and(|from| from == peer) {
                    sync.inflight.remove(&digest);
                }
                let failed = sync.failed.entry(digest).or_default();
                failed.insert(peer.to_owned());
            }
            Trigger::Tick => {
                let peers: Vec<String> = sync.peers.keys().cloned().collect();
                for peer in &peers {
                    out.push(hello(peer, &ours));
                }
                // A want whose every holder failed starts over.
                let mut exhausted = Vec::new();
                for (digest, failed) in &sync.failed {
                    let holders = sync.holders.get(digest);
                    if holders.is_none_or(|h| h.iter().all(|p| failed.contains(p))) {
                        exhausted.push(*digest);
                    }
                }
                for digest in exhausted {
                    sync.failed.remove(&digest);
                }
                // Ask again for what no peer has offered.
                let mut lonely = Vec::new();
                for digest in &wants {
                    let offered = sync.holders.get(digest).is_some_and(|h| !h.is_empty());
                    if !offered && !sync.inflight.contains_key(digest) {
                        lonely.push(*digest);
                    }
                }
                for peer in &peers {
                    digest_frames(&mut out, peer, &lonely, false, per);
                }
            }
        }

        // Each peer whose known version vector lacks changes gets them.
        let mut behind = Vec::new();
        for (peer, state) in &sync.peers {
            if let Some(vv) = &state.vv {
                if !vv.includes_vv(&ours) {
                    behind.push((peer.clone(), vv.clone()));
                }
            }
        }
        for (peer, vv) in behind {
            let bytes = doc.export_updates_since(&vv)?;
            out.extend(update_frames(sync, &peer, bytes, max_frame));
            if let Some(state) = sync.peers.get_mut(&peer) {
                state.vv = Some(ours.clone());
            }
        }

        // Wants no peer has been asked for yet go to every peer.
        let fresh: Vec<Blake3> = wants.difference(&sync.asked).copied().collect();
        if !fresh.is_empty() {
            for peer in sync.peers.keys() {
                digest_frames(&mut out, peer, &fresh, false, per);
            }
            sync.asked.extend(fresh);
        }

        // Peers that wanted what this drive now holds hear so.
        for (peer, state) in sync.peers.iter_mut() {
            let mut now = Vec::new();
            for digest in &state.wanted {
                if self.store.has(digest) {
                    now.push(*digest);
                }
            }
            for digest in &now {
                state.wanted.remove(digest);
            }
            digest_frames(&mut out, peer, &now, true, per);
        }

        // Forget the bookkeeping of objects no longer wanted.
        sync.holders.retain(|digest, _| wants.contains(digest));
        sync.failed.retain(|digest, _| wants.contains(digest));
        sync.asked.retain(|digest| wants.contains(digest));

        // Start pulls, up to the limit, each from the first holder that has
        // not failed it.
        let limit = self.options.max_concurrent_pulls;
        for digest in &wants {
            if sync.inflight.len() >= limit {
                break;
            }
            if sync.inflight.contains_key(digest) {
                continue;
            }
            let failed = sync.failed.get(digest);
            let usable = |holder: &&String| failed.is_none_or(|f| !f.contains(*holder));
            let source = sync.holders.get(digest).and_then(|h| h.iter().find(usable));
            if let Some(from) = source.cloned() {
                sync.inflight.insert(*digest, from.clone());
                let into = self.store.temp_path("pull");
                let digest = *digest;
                out.push(SyncAction::Pull { from, digest, into });
            }
        }
        drop(guard);
        drop(doc);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::Hello { vv: vec![1, 2, 3] },
            Frame::Updates { bytes: vec![9; 10] },
            Frame::UpdatesChunk {
                id: 7,
                seq: 1,
                total: 3,
                bytes: vec![4; 5],
            },
            Frame::Want {
                digests: vec![[1; 32], [2; 32]],
            },
            Frame::Have {
                digests: vec![[3; 32]],
            },
        ];
        for frame in frames {
            let bytes = encode_frame(&frame);
            assert_eq!(bytes[0], FRAME_VERSION);
            assert_eq!(decode_frame(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn an_unknown_frame_version_is_eproto() {
        let mut bytes = encode_frame(&Frame::Hello { vv: vec![] });
        bytes[0] = 2;
        let err = decode_frame(&bytes).unwrap_err();
        assert_eq!(err.code(), crate::error::DriveErrorCode::Proto);
        let empty = decode_frame(&[]).unwrap_err();
        assert_eq!(empty.code(), crate::error::DriveErrorCode::Proto);
    }

    #[test]
    fn a_malformed_or_padded_frame_is_eformat() {
        let garbage = decode_frame(&[FRAME_VERSION, 0xff, 0xff, 0xff]).unwrap_err();
        assert_eq!(garbage.code(), crate::error::DriveErrorCode::Format);
        let mut padded = encode_frame(&Frame::Hello { vv: vec![1] });
        padded.push(0);
        let err = decode_frame(&padded).unwrap_err();
        assert_eq!(err.code(), crate::error::DriveErrorCode::Format);
    }

    #[test]
    fn a_long_want_list_is_split_and_an_oversized_one_refused() {
        let digests: Vec<Blake3> = (0..2500u32)
            .map(|i| Blake3::of(&i.to_le_bytes()))
            .collect();
        let mut out = Vec::new();
        digest_frames(&mut out, "peer", &digests, false, MAX_DIGESTS_PER_FRAME);
        assert_eq!(out.len(), 3);
        let mut seen = 0;
        for action in &out {
            let SyncAction::Send { frame, .. } = action else {
                panic!("a want list is only sends");
            };
            let Frame::Want { digests } = decode_frame(frame).unwrap() else {
                panic!("not a Want");
            };
            assert!(digests.len() <= MAX_DIGESTS_PER_FRAME);
            seen += digests.len();
        }
        assert_eq!(seen, 2500);
        let too_many = vec![[0u8; 32]; MAX_DIGESTS_PER_FRAME + 1];
        assert!(digests_of(too_many).is_err());
        assert_eq!(digests_per_frame(4096), (4096 - FRAME_OVERHEAD) / 32);
    }

    #[test]
    fn a_large_update_is_chunked_within_the_frame_bound() {
        let mut sync = SyncState::default();
        let bytes: Vec<u8> = (0..20_000u32).map(|i| (i % 253) as u8).collect();
        let frames = update_frames(&mut sync, "peer", bytes.clone(), 4096);
        assert!(frames.len() > 1);
        let mut joined = Vec::new();
        for (i, action) in frames.iter().enumerate() {
            let SyncAction::Send { frame, .. } = action else {
                panic!("updates are sends");
            };
            let size = frame.len();
            assert!(size <= 4096, "frame {i} is {size} bytes");
            match decode_frame(frame).unwrap() {
                Frame::UpdatesChunk {
                    seq,
                    total,
                    bytes,
                    ..
                } => {
                    assert_eq!(seq as usize, i);
                    assert_eq!(total as usize, frames.len());
                    joined.extend(bytes);
                }
                other => panic!("not a chunk: {other:?}"),
            }
        }
        assert_eq!(joined, bytes);

        let small = update_frames(&mut sync, "peer", vec![1, 2, 3], 4096);
        assert_eq!(small.len(), 1);
    }
}
