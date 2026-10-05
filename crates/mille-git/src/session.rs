//! Per-checkout sessions and the registry that de-dupes them.
//!
//! One session per checkout root (realpath), however many caller paths open
//! it: `open('/repo/packages/app')` and `open('/repo/packages/other')` share
//! one session, one gitdir watch and one status queue. Each `open` takes a
//! reference; `close` drops one, and only the last `close` tears the session
//! down (SPEC mille-git §9.2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use crate::identity::{Discovered, GitIdentity};
use crate::paths;

pub(crate) struct Session {
    /// Realpath of the checkout root (or of the caller path for
    /// `kind: none`). The registry key and the hint-routing prefix (§11.2.1).
    pub(crate) real_root: PathBuf,
    identity: RwLock<GitIdentity>,
    /// `gix::Repository` is `Send` but not `Sync`: sessions keep the
    /// thread-safe handle and call `to_thread_local()` per operation (§9.2).
    pub(crate) repo: Option<gix::ThreadSafeRepository>,
    /// Fired by the last `close` (and by engine drop): in-flight reads stop,
    /// timers stop re-arming.
    pub(crate) cancel: CancellationToken,
}

impl Session {
    pub(crate) fn new(discovered: Discovered, real_root: PathBuf) -> Self {
        Self {
            real_root,
            identity: RwLock::new(discovered.identity),
            repo: discovered.repo,
            cancel: CancellationToken::new(),
        }
    }

    pub(crate) fn identity(&self) -> GitIdentity {
        self.identity.read().clone()
    }

    /// Re-read the cheap HEAD fields (branch switch, commit) from the repo.
    pub(crate) fn refresh_head(&self) -> GitIdentity {
        if let Some(repo) = &self.repo {
            let (head, branch, detached) = crate::identity::read_head(&repo.to_thread_local());
            let mut identity = self.identity.write();
            identity.head = head;
            identity.branch = branch;
            identity.detached = detached;
        }
        self.identity()
    }

    pub(crate) fn shutdown(&self) {
        self.cancel.cancel();
    }
}

struct Entry {
    session: Arc<Session>,
    refcount: usize,
    aliases: Vec<PathBuf>,
}

#[derive(Default)]
pub(crate) struct Registry {
    by_key: HashMap<PathBuf, Entry>,
    aliases: HashMap<PathBuf, PathBuf>,
}

impl Registry {
    /// Resolve any spelling a caller may use — the exact path given to
    /// `open`, the checkout root, or a realpath of either — to its session.
    pub(crate) fn lookup(&self, path: &Path) -> Option<Arc<Session>> {
        self.key_for(path)
            .and_then(|key| self.by_key.get(&key))
            .map(|entry| entry.session.clone())
    }

    fn key_for(&self, path: &Path) -> Option<PathBuf> {
        let lexical = paths::normalize_lexical(path);
        if let Some(key) = self.aliases.get(&lexical) {
            return Some(key.clone());
        }
        let real = paths::canonicalize_lenient(&lexical);
        if self.by_key.contains_key(&real) {
            return Some(real);
        }
        self.aliases.get(&real).cloned()
    }

    /// A second `open` of a caller path already registered: take a reference
    /// without rediscovering.
    pub(crate) fn join_known(&mut self, caller: &Path) -> Option<Arc<Session>> {
        let key = self.aliases.get(caller)?.clone();
        let entry = self.by_key.get_mut(&key)?;
        entry.refcount += 1;
        Some(entry.session.clone())
    }

    /// Insert a freshly discovered session, or join the one already keyed
    /// by the same realpath (a sibling package of an open checkout, or a
    /// concurrent `open` that won the race). Returns `(session, created)`.
    pub(crate) fn insert_or_join(
        &mut self,
        discovered: Discovered,
        caller: &Path,
    ) -> (Arc<Session>, bool) {
        let key = paths::canonicalize_lenient(&discovered.root);
        let mut spellings = vec![
            caller.to_path_buf(),
            paths::normalize_lexical(&discovered.root),
            key.clone(),
        ];
        spellings.push(paths::canonicalize_lenient(caller));
        let created = !self.by_key.contains_key(&key);
        let entry = self.by_key.entry(key.clone()).or_insert_with(|| Entry {
            session: Arc::new(Session::new(discovered, key.clone())),
            refcount: 0,
            aliases: Vec::new(),
        });
        entry.refcount += 1;
        for spelling in spellings {
            if !entry.aliases.contains(&spelling) {
                entry.aliases.push(spelling.clone());
            }
            self.aliases.insert(spelling, key.clone());
        }
        (entry.session.clone(), created)
    }

    /// Drop one reference; returns the session when that was the last one.
    pub(crate) fn release(&mut self, path: &Path) -> Option<Arc<Session>> {
        let key = self.key_for(path)?;
        let entry = self.by_key.get_mut(&key)?;
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount > 0 {
            return None;
        }
        let entry = self.by_key.remove(&key)?;
        for alias in &entry.aliases {
            self.aliases.remove(alias);
        }
        Some(entry.session)
    }

    pub(crate) fn drain(&mut self) -> Vec<Arc<Session>> {
        self.aliases.clear();
        self.by_key
            .drain()
            .map(|(_, entry)| entry.session)
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }

    /// The realpath key a spelling resolves to. Diagnostic.
    pub(crate) fn real_root_of(&self, path: &Path) -> Option<PathBuf> {
        self.lookup(path).map(|session| session.real_root.clone())
    }
}
