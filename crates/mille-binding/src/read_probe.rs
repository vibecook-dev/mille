//! Test-only read probe (petition M10). CI cannot evict an iCloud folder, so
//! a probe build lets the JS suites park reads of a chosen path on the helper
//! thread — exactly where the kernel would block — and release them later.
//! Compiled only with `read-probe` (which `panic-probe` builds turn on);
//! published artifacts carry neither the hook nor these entry points.

use std::sync::Arc;

use mille_core::ReadHook;

#[cfg(feature = "read-probe")]
pub mod imp {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use mille_core::{ReadHook, ReadKind, ReadProgress};
    use napi::bindgen_prelude::{Error, Result};
    use parking_lot::{Condvar, Mutex};

    #[derive(Default)]
    pub struct Probe {
        /// `(kind, path)`; a `None` kind parks every read of the path.
        parked: Mutex<HashSet<(Option<ReadKind>, PathBuf)>>,
        released: Condvar,
        entered: AtomicU32,
        inside: AtomicU32,
        peak: AtomicU32,
    }

    impl ReadHook for Probe {
        fn before_read(&self, kind: ReadKind, path: &Path, _progress: &ReadProgress) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(inside, Ordering::SeqCst);
            let exact = (Some(kind), path.to_path_buf());
            let any = (None, path.to_path_buf());
            let mut parked = self.parked.lock();
            while parked.contains(&exact) || parked.contains(&any) {
                self.released
                    .wait_for(&mut parked, Duration::from_millis(20));
            }
            drop(parked);
            self.inside.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn probe() -> &'static Arc<Probe> {
        static PROBE: OnceLock<Arc<Probe>> = OnceLock::new();
        PROBE.get_or_init(|| Arc::new(Probe::default()))
    }

    fn kind(name: Option<String>) -> Result<Option<ReadKind>> {
        Ok(match name.as_deref() {
            None | Some("any") => None,
            Some("stat") => Some(ReadKind::Stat),
            Some("list") => Some(ReadKind::ListDir),
            Some("ignore") => Some(ReadKind::IgnoreFiles),
            Some("open") => Some(ReadKind::OpenDir),
            Some(other) => return Err(Error::from_reason(format!("unknown read kind {other}"))),
        })
    }

    #[napi(object)]
    pub struct ReadProbeStatsJs {
        /// Reads that reached the helper thread since the last reset.
        pub entered: u32,
        /// Reads parked right now.
        pub inside: u32,
        /// Most reads parked at once since the last reset.
        pub peak: u32,
    }

    /// Park every read of `path` (or only one `kind`: "stat" | "list" |
    /// "ignore" | "open") on the helper thread until released.
    #[napi(js_name = "__readProbePark", catch_unwind)]
    pub fn park(path: String, kind_name: Option<String>) -> Result<()> {
        let kind = kind(kind_name)?;
        probe().parked.lock().insert((kind, PathBuf::from(path)));
        Ok(())
    }

    #[napi(js_name = "__readProbeRelease", catch_unwind)]
    pub fn release(path: String, kind_name: Option<String>) -> Result<()> {
        let kind = kind(kind_name)?;
        let probe = probe();
        probe.parked.lock().remove(&(kind, PathBuf::from(path)));
        probe.released.notify_all();
        Ok(())
    }

    /// Release everything and zero the counters.
    #[napi(js_name = "__readProbeReset", catch_unwind)]
    pub fn reset() {
        let probe = probe();
        probe.parked.lock().clear();
        probe.released.notify_all();
        probe.entered.store(0, Ordering::SeqCst);
        probe
            .peak
            .store(probe.inside.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    #[napi(js_name = "__readProbeStats", catch_unwind)]
    pub fn stats() -> ReadProbeStatsJs {
        let probe = probe();
        ReadProbeStatsJs {
            entered: probe.entered.load(Ordering::SeqCst),
            inside: probe.inside.load(Ordering::SeqCst),
            peak: probe.peak.load(Ordering::SeqCst),
        }
    }
}

/// The hook every explorer's read pool runs with: the probe in a probe
/// build, nothing otherwise.
pub(crate) fn hook() -> Option<Arc<dyn ReadHook>> {
    #[cfg(feature = "read-probe")]
    {
        Some(Arc::clone(imp::probe()) as Arc<dyn ReadHook>)
    }
    #[cfg(not(feature = "read-probe"))]
    {
        None
    }
}
