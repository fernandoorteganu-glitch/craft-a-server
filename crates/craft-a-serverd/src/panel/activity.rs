//! What the panel is doing for the person right now, and how the last action
//! ended.
//!
//! Starting a server or taking a backup takes longer than a web request may
//! (see `REQUEST_TIMEOUT`), and dropping a half-run lifecycle operation would
//! be dangerous, so long actions run in a spawned task. The browser gets an
//! immediate redirect and the screens show [`Activity`]: a "busy" line while
//! the task runs and the last result afterwards. There is one server and one
//! person at a time in practice, so one global slot is enough; it also stops a
//! second tap from queueing a duplicate action.

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use super::error::PanelError;
use super::strings as s;
use super::{PanelState, mapping};
use crate::lifecycle::{Service, ServiceError, StatusProbe};
use crate::runtime::ContainerRuntime;

/// A result stays on screen this long.
pub const NOTICE_TTL_SECS: i64 = 120;

/// How the last action ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub ok: bool,
    pub text: String,
    pub at_unix: i64,
}

#[derive(Debug, Default)]
struct Inner {
    busy: Option<&'static str>,
    last: Option<Notice>,
}

/// Shared by every screen through the panel state.
#[derive(Debug, Default)]
pub struct Activity {
    inner: Mutex<Inner>,
}

/// What the screens show: a busy line and/or a recent result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Banner {
    pub busy: Option<&'static str>,
    pub notice: Option<Notice>,
}

impl Activity {
    pub fn new() -> Arc<Activity> {
        Arc::new(Activity::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims the slot, or fails if another long action is running.
    pub fn begin(self: &Arc<Self>, what: &'static str) -> Result<BusyGuard, PanelError> {
        let mut g = self.lock();
        if g.busy.is_some() {
            return Err(PanelError::Conflict(s::ERR_BUSY_ACTION.to_owned()));
        }
        g.busy = Some(what);
        g.last = None;
        Ok(BusyGuard {
            activity: Arc::clone(self),
            done: false,
        })
    }

    /// Records the outcome of a quick action that did not need the slot.
    pub fn set_notice(&self, ok: bool, text: &str, now: i64) {
        self.lock().last = Some(Notice {
            ok,
            text: text.to_owned(),
            at_unix: now,
        });
    }

    pub fn banner(&self, now: i64) -> Banner {
        let g = self.lock();
        Banner {
            busy: g.busy,
            notice: g
                .last
                .clone()
                .filter(|n| now.saturating_sub(n.at_unix) < NOTICE_TTL_SECS),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.lock().busy.is_some()
    }
}

/// Holds the busy slot; releases it even if the task dies without finishing.
#[derive(Debug)]
pub struct BusyGuard {
    activity: Arc<Activity>,
    done: bool,
}

impl BusyGuard {
    pub fn finish(mut self, ok: bool, text: &str, now: i64) {
        let mut g = self.activity.lock();
        g.busy = None;
        g.last = Some(Notice {
            ok,
            text: text.to_owned(),
            at_unix: now,
        });
        self.done = true;
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if !self.done {
            self.activity.lock().busy = None;
        }
    }
}

/// Runs a lifecycle operation in the background and reports through
/// [`Activity`]. Returns at once; `Conflict` if something else is running.
pub fn spawn_action<R, P, F, Fut>(
    state: &PanelState<R, P>,
    busy: &'static str,
    done: &'static str,
    action: F,
) -> Result<(), PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
    F: FnOnce(Arc<Service<R, P>>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), ServiceError>> + Send + 'static,
{
    spawn_action_with(state, busy, move |service| async move {
        action(service).await.map(|()| done.to_owned())
    })
}

/// Like [`spawn_action`] for an action whose result text depends on what
/// happened. The text is shown on screen, so it must hold no paths or secrets.
pub fn spawn_action_with<R, P, F, Fut>(
    state: &PanelState<R, P>,
    busy: &'static str,
    action: F,
) -> Result<(), PanelError>
where
    R: ContainerRuntime + 'static,
    P: StatusProbe + 'static,
    F: FnOnce(Arc<Service<R, P>>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<String, ServiceError>> + Send + 'static,
{
    let guard = state.activity().begin(busy)?;
    let service = Arc::clone(state.service());
    let core = Arc::clone(state.core());
    tokio::spawn(async move {
        let outcome = action(service).await;
        let now = core.now();
        match outcome {
            Ok(text) => guard.finish(true, &text, now),
            Err(e) => {
                tracing::warn!(error = %e, "panel action failed");
                guard.finish(false, &mapping::service_error_text(&e), now);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_action_at_a_time_and_the_slot_is_released() {
        let a = Activity::new();
        let guard = a.begin("x");
        assert!(guard.is_ok());
        assert!(matches!(a.begin("y"), Err(PanelError::Conflict(_))));
        assert_eq!(a.banner(0).busy, Some("x"));
        drop(guard);
        assert!(!a.is_busy());
        assert!(a.begin("y").is_ok());
    }

    #[test]
    fn results_expire() {
        let a = Activity::new();
        if let Ok(g) = a.begin("x") {
            g.finish(true, "done", 100);
        }
        let b = a.banner(100 + NOTICE_TTL_SECS - 1);
        assert_eq!(b.busy, None);
        assert_eq!(b.notice.map(|n| n.text), Some("done".to_owned()));
        assert_eq!(a.banner(100 + NOTICE_TTL_SECS).notice, None);
    }

    #[test]
    fn starting_an_action_clears_the_old_result() {
        let a = Activity::new();
        a.set_notice(false, "old", 5);
        let _g = a.begin("x");
        assert_eq!(a.banner(6).notice, None);
    }
}
