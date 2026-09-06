//! AbortSignal → CancellationToken adapter (Phase 5 commit 5.9).
//!
//! napi-rs 3.8's `AbortSignal` is the FromNapiValue wrapper around a JS
//! `AbortSignal` object. It is intentionally `!Send` (`Rc<RefCell<...>>`
//! inside) because its `on_abort` callback fires on the JS main thread.
//! That means we must convert the signal into a thread-safe token
//! **synchronously**, before any `.await`, so the returned future stays
//! Send. The adapter here does exactly that: register an on-abort hook
//! that trips a `tokio_util::sync::CancellationToken`, then drop the
//! signal. The token is cheap to clone and flows freely across threads.
//!
//! SPEC §4.6 notes AbortSignal is advisory in Phase 5 — we check the
//! token at yield points (before + after long ops) but can't interrupt
//! an in-flight `tokio::fs::read` mid-syscall. Covers the common "user
//! navigated away before the read finished" case; Phase 10 search will
//! carry the token into a multi-step loop where it bites harder.
//!
//! napi-rs 3.8's API (confirmed against the installed crate source):
//!   - Import path: `napi::bindgen_prelude::AbortSignal`
//!   - Register: `signal.on_abort(|| { ... })` — takes `Fn() + 'static`
//!   - No direct `is_aborted()` query; rely on the callback to flip state

use napi::bindgen_prelude::{AbortSignal, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::error::fx_error_to_napi;

/// Owns a synchronously registered operation until its worker finishes or is
/// dropped. Keeping cleanup in Drop also releases the id after a panic or a
/// failure to schedule the future, so a retry cannot inherit a stale token.
pub(crate) struct RegisteredOperation {
    operations: Arc<Mutex<HashMap<String, CancellationToken>>>,
    id: String,
    pub(crate) token: CancellationToken,
}

impl RegisteredOperation {
    pub(crate) fn new(
        operations: Arc<Mutex<HashMap<String, CancellationToken>>>,
        id: String,
    ) -> Result<Self> {
        let token = CancellationToken::new();
        {
            let mut active = operations.lock();
            if active.contains_key(&id) {
                return Err(fx_error_to_napi(mille_core::FxError::InvalidInput(
                    format!("duplicate operationId already in flight: {id}"),
                )));
            }
            active.insert(id.clone(), token.clone());
        }
        Ok(Self {
            operations,
            id,
            token,
        })
    }
}

impl Drop for RegisteredOperation {
    fn drop(&mut self) {
        self.operations.lock().remove(&self.id);
    }
}

/// Convert an optional JS `AbortSignal` into a `CancellationToken`.
///
/// Must be called on the JS main thread (AbortSignal is `!Send`). The
/// returned token is fully `Send + Sync` and can move into async bodies.
/// Callers who pass `None` get a token that never fires.
pub(crate) fn signal_to_token(signal: Option<AbortSignal>) -> CancellationToken {
    let token = CancellationToken::new();
    if let Some(sig) = signal {
        let token_clone = token.clone();
        sig.on_abort(move || {
            token_clone.cancel();
        });
    }
    token
}

/// Guard to be called at yield points. Returns `ECANCELED` as a
/// structured FX reason when the token has been tripped.
pub(crate) fn check_cancelled(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(fx_error_to_napi(mille_core::FxError::Cancelled))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_signal_yields_live_token() {
        let t = signal_to_token(None);
        assert!(!t.is_cancelled());
        assert!(check_cancelled(&t).is_ok());
    }

    #[test]
    fn check_cancelled_returns_fx_canceled_reason() {
        let t = CancellationToken::new();
        t.cancel();
        let err = check_cancelled(&t).expect_err("cancelled token must error");
        assert!(
            err.reason.starts_with("FX|ECANCELED||"),
            "unexpected reason: {}",
            err.reason
        );
    }
}
