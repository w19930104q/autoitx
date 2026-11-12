//! Pinned, auditable control references with bounded waits.
//!
//! The methods on [`AutoIt`](crate::AutoIt) take a fresh
//! [`Selector`]/[`Control`] pair on every call. That is the right shape for a
//! single operation, and the wrong shape for an unattended one: between two
//! calls the application can pop an error dialog, change a title, or replace a
//! control, and a general selector then resolves to *whatever happens to match
//! now*. The keystroke meant for the invoice field can land in the dialog that
//! appeared on top of it, with success reported back.
//!
//! A [`ControlRef`] closes that gap by turning "find it each time" into one
//! auditable action:
//!
//! 1. **Bind once.** [`ControlRef::bind`] resolves the window and the control to
//!    their HWNDs immediately and records the window's *generation* — how many
//!    times that identity has been recreated inside this reference.
//! 2. **Verify before every action.** Each call takes the automation lock for
//!    its whole duration and, under it, proves that the same window handle still
//!    exists, that re-resolving the control returns the same control handle, and
//!    that both are visible and enabled. No thread can interleave between the
//!    check and the act.
//! 3. **Fail, never retarget.** If the window was closed and another window
//!    inherited its slot (window reuse), the generation changes and every call —
//!    including all later calls on the same reference — fails with
//!    [`ReferenceError::WindowGenerationChanged`]. If the control handle changed
//!    while the window lived, it fails with
//!    [`ReferenceError::ControlReplaced`]. The target is never silently rewritten.
//! 4. **Report what actually happened.** Every action returns a
//!    [`ControlOutcome`] carrying the resolved handles, the elapsed time, and
//!    the terminal readiness state.
//!
//! ```no_run
//! use autoitx::reference::{ControlRef, Wait};
//! use autoitx::{AutoIt, Control, Selector};
//! use std::time::Duration;
//!
//! let ai = AutoIt::new()?;
//! let ok = ControlRef::bind(
//!     &ai,
//!     &Selector::from("[CLASS:ui60Modal_W32;TITLE:Post Invoice]"),
//!     &Control::id(1001),
//! )?;
//!
//! // Wait up to 30 s for the button to become enabled (and stay
//! // visible/existing), then click it. A dialog popping up in the meantime
//! // cannot absorb the click.
//! ok.wait_enabled(Wait::timed(Duration::from_secs(30)))?;
//! let outcome = ok.click(Default::default())?;
//! println!("clicked control {:#x} in window {:#x}, took {:?}",
//!          outcome.control_handle, outcome.window_handle, outcome.elapsed);
//! # Ok::<(), autoitx::Error>(())
//! ```
//!
//! # Cancellation
//!
//! [`Wait`] accepts a [`CancellationToken`], so a supervisor can stop a wait —
//! for instance when the whole job is shutting down. Cancellation is observed
//! between poll passes, and it is an error ([`ReferenceError::Cancelled`]), not
//! a silent return: the caller learns the condition was never met. A wait that
//! times out or is cancelled leaves no lock held and no thread parked, so the
//! next call runs immediately.
//!
//! # Platform
//!
//! Controls are addressed by Win32 HWND, so this module exists on Windows and
//! wherever the mock DLL can be loaded, like the rest of the control API.

use crate::error::{Error, Result};
use crate::options::WinState;
use crate::{AutoIt, Control, MouseButton, Point, Selector};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Something a wait can ask "should I stop now?".
///
/// The trait is the seam instead of a concrete channel so callers are not
/// forced onto a particular executor or cancellation framework. The provided
/// [`CancellationToken`] covers the ordinary case; tests and adapters can
/// implement this over anything that already has a shutdown signal.
pub trait Cancel {
    /// Whether the wait has been asked to stop.
    fn is_cancelled(&self) -> bool;
}

/// The trivial implementation: nothing ever cancels.
impl Cancel for () {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// A shareable cancellation flag for [`Wait`].
///
/// Cheap to clone; every clone shares one flag, so a supervisor can hold a
/// clone and cancel waits running on worker threads.
///
/// ```
/// use autoitx::reference::{Cancel, CancellationToken};
///
/// let token = CancellationToken::new();
/// assert!(!token.is_cancelled());
/// token.cancel();
/// assert!(token.is_cancelled());
/// ```
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<std::sync::atomic::AtomicBool>);

impl CancellationToken {
    /// A token that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals every wait holding a clone of this token to stop at its next
    /// poll pass.
    ///
    /// Sticky on purpose: a wait that observed cancellation must not later
    /// resume because the flag was cleared — the job that owned the wait is
    /// shutting down, and resuming it would act on a stale plan.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Cancel for CancellationToken {
    fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Wait specification
// ---------------------------------------------------------------------------

/// The poll interval used when [`Wait`] does not name one.
///
/// Matches AutoIt's own default `WinWaitDelay`, so the waits here pace
/// themselves exactly like AutoIt's built-in ones unless told otherwise.
const DEFAULT_POLL: Duration = Duration::from_millis(250);

/// A bounded wait: a deadline, a poll interval, and an optional cancel signal.
///
/// A timeout is mandatory — an unattended robot must be able to give up rather
/// than hang forever — which is why there is no `Wait::forever`. Construct with
/// [`timed`](Self::timed) and adjust with the builder methods.
///
/// ```
/// use autoitx::reference::Wait;
/// use std::time::Duration;
///
/// // Up to ten seconds, polling at the default WinWaitDelay (250 ms).
/// let w = Wait::timed(Duration::from_secs(10));
///
/// // Tighter polling for a control expected to flip quickly.
/// let fast = Wait::timed(Duration::from_secs(2)).polling(Duration::from_millis(20));
/// assert_eq!(fast.timeout, Duration::from_secs(2));
/// assert_eq!(fast.poll, Duration::from_millis(20));
/// ```
#[derive(Clone)]
pub struct Wait {
    /// The maximum time the condition may take to hold.
    pub timeout: Duration,
    /// How often the condition is re-checked.
    pub poll: Duration,
    cancel: Option<Arc<dyn Cancel + Send + Sync>>,
}

impl Wait {
    /// A bounded wait with the default poll interval and no cancellation.
    #[must_use]
    pub fn timed(timeout: Duration) -> Self {
        Self {
            timeout,
            poll: DEFAULT_POLL,
            cancel: None,
        }
    }

    /// Overrides the poll interval.
    ///
    /// A zero interval polls back-to-back; useful in tests against the mock,
    /// rarely what production wants because each pass is a set of OS calls.
    #[must_use]
    pub fn polling(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// Attaches a cancellation signal.
    ///
    /// Anything implementing [`Cancel`] works; the common choice is a cloned
    /// [`CancellationToken`] the supervisor also holds.
    #[must_use]
    pub fn cancel_with(mut self, token: impl Cancel + Send + Sync + 'static) -> Self {
        self.cancel = Some(Arc::new(token));
        self
    }

    /// Whether the attached signal, if any, has fired.
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c.is_cancelled())
    }
}

impl std::fmt::Debug for Wait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wait")
            .field("timeout", &self.timeout)
            .field("poll", &self.poll)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a pinned [`ControlRef`] operation did not run, or stopped waiting.
///
/// The two identity variants are the point of the whole type: they distinguish
/// "the thing I named is not ready" from "the thing I named no longer exists
/// and something else is in its place". The second must never be papered over.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReferenceError {
    /// The window was closed and its slot reused — or otherwise recreated —
    /// after this reference was bound.
    ///
    /// The selector may well match *a* window now; it is not the window the
    /// reference pinned. This error is sticky: once the generation has moved,
    /// every subsequent call on the same reference fails the same way, so a
    /// retry loop cannot quietly start driving the replacement window.
    #[error(
        "window generation changed for {selector}: the pinned window {window_handle:#x} \
         was recreated (generation {generation}); refusing to retarget"
    )]
    WindowGenerationChanged {
        /// The selector the reference was bound with.
        selector: Box<Selector>,
        /// The window handle that was pinned.
        window_handle: u64,
        /// The generation reached after the change, starting at 1.
        generation: u64,
    },

    /// Re-resolving the control returned a different handle while the window
    /// lived.
    ///
    /// A rebuilt widget often keeps the same `ClassnameNN` or control id, which
    /// is exactly when a fresh selector lookup silently drives the new widget.
    /// The handle comparison catches it where the identifier cannot.
    #[error(
        "control matching {control} in window {window_handle:#x} was replaced: \
         bound {bound_handle:#x}, now resolves to {current_handle:#x}"
    )]
    ControlReplaced {
        /// The control identifier the reference was bound with.
        control: Control,
        /// The window the controls belong to.
        window_handle: u64,
        /// The handle captured at bind time.
        bound_handle: u64,
        /// The handle a fresh lookup returns now.
        current_handle: u64,
    },

    /// The pinned window no longer exists.
    ///
    /// Reported distinctly from a generation change: this is plain disappearance
    /// with no successor observed, so a bounded wait may keep polling for it to
    /// return — whereas a generation change ends the reference for good.
    #[error("the pinned window {window_handle:#x} no longer exists")]
    WindowGone {
        /// The handle that was pinned.
        window_handle: u64,
    },

    /// The window existed, but a control matching the identifier did not.
    #[error("window {window_handle:#x} currently has no control matching {control}")]
    ControlGone {
        /// The control identifier that matched nothing.
        control: Control,
        /// The window that was searched.
        window_handle: u64,
    },

    /// The target existed but was not visible and/or enabled when the action
    /// required both.
    #[error(
        "control {control_handle:#x} in window {window_handle:#x} is not ready: \
         visible={visible}, enabled={enabled}"
    )]
    NotReady {
        /// The window handle.
        window_handle: u64,
        /// The control handle.
        control_handle: u64,
        /// Whether the window was reported visible.
        visible: bool,
        /// Whether the window and control were reported enabled.
        enabled: bool,
    },

    /// A bounded wait elapsed without the condition holding.
    #[error("{operation} timed out after {waited:?}")]
    Timeout {
        /// What was being waited for.
        operation: &'static str,
        /// How long the wait actually ran.
        waited: Duration,
    },

    /// A wait stopped because its [`CancellationToken`] fired.
    #[error("{operation} was cancelled after {waited:?}")]
    Cancelled {
        /// What was being waited for.
        operation: &'static str,
        /// How long the wait ran before cancellation was observed.
        waited: Duration,
    },
}

// ---------------------------------------------------------------------------
// Observed state
// ---------------------------------------------------------------------------

/// The readiness of a pinned reference, as observed in one locked pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlReadiness {
    /// The window exists.
    pub window_exists: bool,
    /// The control's handle still matches the bound one.
    pub control_identity_holds: bool,
    /// The window reports visible.
    pub window_visible: bool,
    /// The window reports enabled.
    pub window_enabled: bool,
    /// The control reports visible.
    pub control_visible: bool,
    /// The control reports enabled.
    pub control_enabled: bool,
}

impl ControlReadiness {
    /// Every gate an action takes: present, identity intact, visible and
    /// enabled on both window and control.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        self.window_exists
            && self.control_identity_holds
            && self.window_visible
            && self.window_enabled
            && self.control_visible
            && self.control_enabled
    }
}

/// What an audited action actually did.
///
/// Returned by every [`ControlRef`] action so the outcome can be logged or
/// asserted against: the handles that were used, how long the whole
/// verify-and-act sequence took, and the state observed in the verification
/// pass that authorised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlOutcome {
    /// The window handle the action targeted.
    pub window_handle: u64,
    /// The control handle the action targeted.
    pub control_handle: u64,
    /// Wall-clock duration of the locked verify-and-act sequence.
    pub elapsed: Duration,
    /// The readiness observed immediately before the action ran.
    pub readiness: ControlReadiness,
}

// ---------------------------------------------------------------------------
// The reference itself
// ---------------------------------------------------------------------------

/// Everything a reference must keep proving about itself, behind one lock.
struct Lineage {
    /// How many times the pinned identity has been recreated. Starts at 1.
    generation: u64,
    /// The bound window HWND.
    pinned_window: u64,
    /// The bound control HWND.
    bound_control: u64,
    /// Set once a generation change has been observed, after which every call
    /// fails without even touching the backend. This is what makes the
    /// refusal sticky across threads and retries: the failure lives in shared
    /// state, not in the local result of one call.
    dead: bool,
}

/// A window-and-control identity pinned at bind time.
///
/// Cheap to clone and `Send + Sync`: clones share the lineage, so a generation
/// change observed on one thread is immediately visible to every other thread
/// using the reference. See the [module docs](self) for the guarantee model.
#[derive(Clone)]
pub struct ControlRef {
    ai: AutoIt,
    selector: Selector,
    control: Control,
    lineage: Arc<Mutex<Lineage>>,
}

impl std::fmt::Debug for ControlRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lineage = self.lineage.lock();
        f.debug_struct("ControlRef")
            .field("selector", &self.selector)
            .field("control", &self.control.as_str())
            .field("window_handle", &lineage.pinned_window)
            .field("control_handle", &lineage.bound_control)
            .field("generation", &lineage.generation)
            .field("dead", &lineage.dead)
            .finish()
    }
}

impl ControlRef {
    /// Resolves the selector and control to handles and pins them.
    ///
    /// The resolution happens under one session lock, so the control handle
    /// cannot belong to a different window than the window handle even for an
    /// instant.
    ///
    /// # Errors
    ///
    /// [`Error::WindowNotFound`] if no window matches,
    /// [`Error::ControlNotFound`] if the window has no such control.
    pub fn bind(ai: &AutoIt, window: &Selector, control: &Control) -> Result<Self> {
        let session = ai.session();
        let window_handle = session.win_get_handle(window)?;
        let control_handle =
            session.control_get_handle(&Selector::handle(window_handle), control)?;
        Ok(Self {
            ai: ai.clone(),
            selector: window.clone(),
            control: control.clone(),
            lineage: Arc::new(Mutex::new(Lineage {
                generation: 1,
                pinned_window: window_handle,
                bound_control: control_handle,
                dead: false,
            })),
        })
    }

    /// The window handle captured at bind time.
    #[must_use]
    pub fn window_handle(&self) -> u64 {
        self.lineage.lock().pinned_window
    }

    /// The control handle captured at bind time.
    #[must_use]
    pub fn control_handle(&self) -> u64 {
        self.lineage.lock().bound_control
    }

    /// The current generation. `1` means the pinned window has never been
    /// recreated; anything higher means the reference has entered its sticky
    /// failure state.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lineage.lock().generation
    }

    /// The selector the reference was bound with.
    #[must_use]
    pub fn selector(&self) -> &Selector {
        &self.selector
    }

    /// The control identifier the reference was bound with.
    #[must_use]
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// One locked verify pass: identity first, then visibility and readiness.
    ///
    /// All the AU3 calls in this method happen under one [`crate::Session`],
    /// which is what makes "check then act" indivisible — another thread cannot
    /// get between the verification and whatever the caller does next while the
    /// session is still held.
    fn observe(&self) -> Result<ControlReadiness> {
        let session = self.ai.session();
        self.observe_locked(&session)
    }

    /// Reads the current readiness without mutating anything.
    ///
    /// # Errors
    ///
    /// Any of the identity variants in [`ReferenceError`], plus ordinary
    /// backend failures.
    pub fn readiness(&self) -> Result<ControlReadiness> {
        self.observe()
    }

    /// Runs `f` under the session once the reference passes every gate.
    ///
    /// The session is held across the verification *and* `f`, so no other
    /// thread can interleave an action between the check and the act.
    fn with_verified<R>(
        &self,
        f: impl FnOnce(&crate::Session<'_>, u64, u64) -> Result<R>,
    ) -> Result<(R, ControlReadiness, Duration)> {
        let started = Instant::now();
        let session = self.ai.session();

        let (pinned_window, bound_control) = {
            let lineage = self.lineage.lock();
            (lineage.pinned_window, lineage.bound_control)
        };

        let readiness = self.observe_locked(&session)?;
        if !readiness.is_actionable() {
            return Err(ReferenceError::NotReady {
                window_handle: pinned_window,
                control_handle: bound_control,
                visible: readiness.window_visible && readiness.control_visible,
                enabled: readiness.window_enabled && readiness.control_enabled,
            }
            .into());
        }

        let value = f(&session, pinned_window, bound_control)?;
        Ok((value, readiness, started.elapsed()))
    }

    /// Same checks as [`observe`](Self::observe) but reusing a session the
    /// caller already holds — the reentrant lock makes nesting free.
    ///
    /// Every public path funnels through here, which is how the generation
    /// guarantee holds for one-shot actions as well as waits: a
    /// [`click`](Self::click) issued the instant after a window was reused has
    /// to fail exactly as a poll would.
    fn observe_locked(&self, session: &crate::Session<'_>) -> Result<ControlReadiness> {
        let (pinned_window, bound_control, generation, already_dead) = {
            let lineage = self.lineage.lock();
            (
                lineage.pinned_window,
                lineage.bound_control,
                lineage.generation,
                lineage.dead,
            )
        };

        // A previously observed generation change is terminal: do not even
        // consult the backend, so a retry loop can never quietly accept the
        // replacement window after it settles.
        if already_dead {
            return Err(ReferenceError::WindowGenerationChanged {
                selector: Box::new(self.selector.clone()),
                window_handle: pinned_window,
                generation,
            }
            .into());
        }

        let window_sel = Selector::handle(pinned_window);

        // 1. Does the pinned window still exist? The handle selector addresses
        //    that exact HWND rather than re-running the original title match.
        if !session.win_exists(&window_sel)? {
            // The pinned HWND is gone. A hit on the original selector means
            // another window now answers the name: the slot was reused and the
            // generation moves, permanently. With no successor it is a plain
            // disappearance, which a wait may still poll through.
            if session.win_exists(&self.selector)? {
                let next = generation + 1;
                let mut lineage = self.lineage.lock();
                lineage.dead = true;
                lineage.generation = next;
                return Err(ReferenceError::WindowGenerationChanged {
                    selector: Box::new(self.selector.clone()),
                    window_handle: pinned_window,
                    generation: next,
                }
                .into());
            }
            return Err(ReferenceError::WindowGone {
                window_handle: pinned_window,
            }
            .into());
        }

        // 2. Does the control still resolve to the same HWND? A widget rebuild
        //    can keep the identifier and change the handle; that is replacement,
        //    not readiness.
        let current_control = match session.control_get_handle(&window_sel, &self.control) {
            Ok(handle) => handle,
            // A missing control is an observable state, not a panic: a wait may
            // legitimately poll until it appears.
            Err(Error::ControlNotFound { .. }) => {
                return Err(ReferenceError::ControlGone {
                    control: self.control.clone(),
                    window_handle: pinned_window,
                }
                .into());
            }
            Err(other) => return Err(other),
        };
        if current_control != bound_control {
            return Err(ReferenceError::ControlReplaced {
                control: self.control.clone(),
                window_handle: pinned_window,
                bound_handle: bound_control,
                current_handle: current_control,
            }
            .into());
        }

        // 3. Window-level state, reported through the error flag.
        let win_state = session.win_get_state(&window_sel)?;

        // 4. Control-level state, via the same ControlCommand vocabulary
        //    AutoIt scripts use. The DLL returns the answer as text.
        let control_visible =
            control_bool_command(session, &window_sel, &self.control, "IsVisible")?;
        let control_enabled =
            control_bool_command(session, &window_sel, &self.control, "IsEnabled")?;

        Ok(ControlReadiness {
            window_exists: true,
            control_identity_holds: true,
            window_visible: win_state.contains(WinState::VISIBLE),
            window_enabled: win_state.contains(WinState::ENABLED),
            control_visible,
            control_enabled,
        })
    }

    /// Polls `predicate` until it holds, the deadline passes, or cancellation
    /// fires. The lock is released between passes, like
    /// [`AutoIt::wait_for_any`](crate::AutoIt::wait_for_any).
    fn poll_until(
        &self,
        operation: &'static str,
        wait: &Wait,
        mut predicate: impl FnMut(ControlReadiness) -> bool,
    ) -> Result<ControlReadiness> {
        let start = Instant::now();
        loop {
            // Cancellation is checked before the condition on purpose: a token
            // fired during an earlier pass must win even if the condition has
            // since become true, since the owning job is shutting down.
            if wait.cancelled() {
                return Err(ReferenceError::Cancelled {
                    operation,
                    waited: start.elapsed(),
                }
                .into());
            }

            // One observe pass carries every identity gate, including the
            // sticky generation check — a recreation ends the wait permanently.
            let readiness = self.observe()?;
            if predicate(readiness) {
                return Ok(readiness);
            }
            if start.elapsed() >= wait.timeout {
                return Err(ReferenceError::Timeout {
                    operation,
                    waited: start.elapsed(),
                }
                .into());
            }
            // Sleep without the lock: a parked lock would stall every other
            // thread's automation for the whole wait. The remaining time caps
            // the nap so a wait can never overshoot its deadline into the next
            // operation.
            let remaining = wait.timeout.saturating_sub(start.elapsed());
            self.ai.sleep(wait.poll.min(remaining));
        }
    }

    /// Waits until the control is visible and enabled on both levels.
    ///
    /// # Errors
    ///
    /// [`ReferenceError::Timeout`], [`ReferenceError::Cancelled`], or any
    /// identity error if the target changes while waiting.
    pub fn wait_ready(&self, wait: Wait) -> Result<ControlReadiness> {
        self.poll_until("control_ref::wait_ready", &wait, |r| r.is_actionable())
    }

    /// Waits until the control is enabled (it must also stay visible and the
    /// window must stay present).
    ///
    /// The common form of an ERP dialog: fields exist from the start, but the
    /// confirm button stays greyed until validation passes.
    pub fn wait_enabled(&self, wait: Wait) -> Result<ControlReadiness> {
        self.poll_until("control_ref::wait_enabled", &wait, |r| r.is_actionable())
    }

    /// Waits until the control's text differs from `previous`.
    ///
    /// Pass the text read immediately before the action that should change it,
    /// including `""` for a field expected to populate. Equality is on the
    /// full string the backend reports.
    pub fn wait_text_changes(&self, previous: &str, wait: Wait) -> Result<TextChange> {
        let operation = "control_ref::wait_text";
        let start = Instant::now();
        loop {
            // Same ordering as the other waits: cancellation first, then the
            // locked identity/readiness pass.
            if wait.cancelled() {
                return Err(ReferenceError::Cancelled {
                    operation,
                    waited: start.elapsed(),
                }
                .into());
            }
            let session = self.ai.session();
            let pinned_window = self.lineage.lock().pinned_window;
            let readiness = self.observe_locked(&session)?;
            let current =
                session.control_get_text(&Selector::handle(pinned_window), &self.control)?;
            if current != previous {
                return Ok(TextChange {
                    from: previous.to_owned(),
                    to: current,
                    elapsed: start.elapsed(),
                    readiness,
                });
            }
            if start.elapsed() >= wait.timeout {
                return Err(ReferenceError::Timeout {
                    operation,
                    waited: start.elapsed(),
                }
                .into());
            }
            // Release the lock before sleeping, same as the other waits.
            let remaining = wait.timeout.saturating_sub(start.elapsed());
            drop(session);
            self.ai.sleep(wait.poll.min(remaining));
        }
    }

    /// Clicks the control after a full verification pass.
    ///
    /// The click itself goes through `AU3_ControlClickByHandle`, so even a
    /// selector that would now match something else cannot redirect it.
    pub fn click(&self, opts: ClickOptions) -> Result<ControlOutcome> {
        let (_, readiness, elapsed) = self.with_verified(|session, window, control| {
            session.control_click_by_handle(
                window,
                control,
                opts.button.as_str(),
                opts.clicks,
                opts.at,
            )
        })?;
        Ok(self.outcome(readiness, elapsed))
    }

    /// Sends keystrokes straight to the pinned control, after verification.
    ///
    /// Uses `AU3_ControlSendByHandle`; unlike
    /// [`AutoIt::send`](crate::AutoIt::send), this does not require focus.
    pub fn send(&self, keys: &crate::Keys) -> Result<ControlOutcome> {
        let (_, readiness, elapsed) = self.with_verified(|session, window, control| {
            session.control_send_by_handle(window, control, keys)
        })?;
        Ok(self.outcome(readiness, elapsed))
    }

    /// Replaces the pinned control's text, after verification.
    pub fn set_text(&self, text: &str) -> Result<ControlOutcome> {
        let (_, readiness, elapsed) = self.with_verified(|session, window, control| {
            session.control_set_text_by_handle(window, control, text)
        })?;
        Ok(self.outcome(readiness, elapsed))
    }

    /// Runs a widget-specific `ControlCommand` against the pinned control.
    ///
    /// `command`/`extra` are AutoIt's own vocabulary (`"Check"`,
    /// `"SelectString"`, ...), passed through untouched.
    pub fn command(&self, command: &str, extra: &str) -> Result<CommandResult> {
        let (result, readiness, elapsed) = self.with_verified(|session, window, control| {
            session.control_command_by_handle(window, control, command, extra)
        })?;
        Ok(CommandResult {
            result,
            outcome: self.outcome(readiness, elapsed),
        })
    }

    fn outcome(&self, readiness: ControlReadiness, elapsed: Duration) -> ControlOutcome {
        let (window_handle, control_handle) = {
            let lineage = self.lineage.lock();
            (lineage.pinned_window, lineage.bound_control)
        };
        ControlOutcome {
            window_handle,
            control_handle,
            elapsed,
            readiness,
        }
    }
}

/// Options for [`ControlRef::click`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickOptions {
    /// Which mouse button.
    pub button: MouseButton,
    /// How many clicks.
    pub clicks: u32,
    /// A point inside the control, or its centre when `None`.
    pub at: Option<Point>,
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            button: MouseButton::Left,
            clicks: 1,
            at: None,
        }
    }
}

/// The result of waiting for a control's text to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChange {
    /// The text the control had when the wait started.
    pub from: String,
    /// The first different text observed.
    pub to: String,
    /// How long the wait took.
    pub elapsed: Duration,
    /// Readiness observed in the pass that saw the new text.
    pub readiness: ControlReadiness,
}

/// The result of a [`ControlRef::command`], with its audit outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// What the command wrote back.
    pub result: String,
    /// Handles, timing and terminal state.
    pub outcome: ControlOutcome,
}

/// Reads a boolean `ControlCommand` (`IsVisible`, `IsEnabled`, ...).
///
/// AutoIt reports these as the strings `"1"`/`"0"`; anything else is treated as
/// false rather than guessed at.
fn control_bool_command(
    session: &crate::Session<'_>,
    window: &Selector,
    control: &Control,
    command: &str,
) -> Result<bool> {
    let answer = session.control_command(window, control, command, "")?;
    Ok(answer.trim() == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_sticky_and_shared_across_clones() {
        let a = CancellationToken::new();
        let b = a.clone();
        assert!(!a.is_cancelled());
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled(), "a clone must observe the flag");
        b.cancel();
        assert!(a.is_cancelled());
    }

    #[test]
    fn the_unit_signal_never_cancels() {
        assert!(!().is_cancelled());
    }

    #[test]
    fn wait_carries_a_mandatory_timeout_and_default_poll() {
        let w = Wait::timed(Duration::from_secs(5));
        assert_eq!(w.timeout, Duration::from_secs(5));
        assert_eq!(w.poll, DEFAULT_POLL);
        assert!(!w.cancelled());
        let w = w.polling(Duration::from_millis(10));
        assert_eq!(w.poll, Duration::from_millis(10));
    }

    #[test]
    fn wait_records_an_attached_token() {
        let token = CancellationToken::new();
        let w = Wait::timed(Duration::from_secs(1)).cancel_with(token);
        assert!(!w.cancelled());
    }

    #[test]
    fn actionable_requires_every_gate() {
        let all = ControlReadiness {
            window_exists: true,
            control_identity_holds: true,
            window_visible: true,
            window_enabled: true,
            control_visible: true,
            control_enabled: true,
        };
        assert!(all.is_actionable());

        let mut missing_one = all;
        missing_one.control_enabled = false;
        assert!(!missing_one.is_actionable());

        let mut hidden = all;
        hidden.window_visible = false;
        assert!(!hidden.is_actionable());

        let mut replaced = all;
        replaced.control_identity_holds = false;
        assert!(!replaced.is_actionable());
    }

    #[test]
    fn click_options_default_to_one_centre_left_click() {
        assert_eq!(
            ClickOptions::default(),
            ClickOptions {
                button: MouseButton::Left,
                clicks: 1,
                at: None,
            }
        );
    }
}
