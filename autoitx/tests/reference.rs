//! Regression tests for the pinned control reference.
//!
//! These drive [`autoitx::reference::ControlRef`] against the mock DLL's
//! topology mode, where "windows" and "controls" are scripted state rather
//! than scripted one-shot answers. The five scenarios the feature exists for
//! each get a dedicated test:
//!
//! - a window closed and replaced by another answering the same selector
//!   (window reuse / generation change);
//! - a control rebuilt under the same identifier with a new handle;
//! - a condition that never holds, bounded by a timeout;
//! - a wait stopped by a cancellation token;
//! - several threads driving one reference at once.
//!
//! Everything here runs on any OS under `mock-loader`.

#![cfg(feature = "mock-loader")]

mod common;

use autoitx::reference::{
    Cancel, CancellationToken, ClickOptions, ControlRef, ReferenceError, Wait,
};
use autoitx::{Control, Error, Selector};
use common::Harness;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Scripted HWNDs, kept as named constants so the expectations read as
/// identities rather than magic numbers.
const WINDOW_HWND: u64 = 0x1000;
const REPLACEMENT_WINDOW_HWND: u64 = 0x1001;
const CONTROL_HWND: u64 = 0x2000;
const REPLACEMENT_CONTROL_HWND: u64 = 0x2001;

/// The human-facing selector the robot binds with.
fn window_selector() -> Selector {
    Selector::from("[CLASS:ui60Modal_W32;TITLE:Post Invoice]")
}

/// The handle selector every post-bind check must use.
fn window_handle_selector(handle: u64) -> Selector {
    Selector::handle(handle)
}

fn control() -> Control {
    Control::id(1001)
}

/// Matches a boxed [`ReferenceError`] by reference, without moving the
/// [`Error`] so its `Debug` rendering stays available for the panic message.
fn reference_error(err: &Error) -> Option<&ReferenceError> {
    match err {
        Error::Reference(boxed) => Some(boxed.as_ref()),
        _ => None,
    }
}

/// Registers one fully-ready window/control pair under both selectors the
/// reference layer will ask about: the binding selector and the pinned
/// handle selector.
fn register_ready_window(h: &Harness, window_handle: u64, control_handle: u64) {
    h.topology_add_window(&window_selector().to_string(), window_handle, true, true);
    h.topology_add_window(
        &window_handle_selector(window_handle).to_string(),
        window_handle,
        true,
        true,
    );
    h.topology_add_control(
        &window_handle_selector(window_handle).to_string(),
        control().as_str(),
        control_handle,
        true,
        true,
        "OK",
    );
}

fn bind_ready(h: &Harness) -> ControlRef {
    h.topology_reset();
    register_ready_window(h, WINDOW_HWND, CONTROL_HWND);
    ControlRef::bind(&h.ai, &window_selector(), &control())
        .expect("the scripted window and control should bind")
}

// ---------------------------------------------------------------------------
// Binding and the happy path
// ---------------------------------------------------------------------------

#[test]
fn bind_pins_both_handles_and_the_first_generation() {
    let h = Harness::new();
    let r = bind_ready(&h);

    assert_eq!(r.window_handle(), WINDOW_HWND);
    assert_eq!(r.control_handle(), CONTROL_HWND);
    assert_eq!(r.generation(), 1, "no recreation has happened");
    assert_eq!(r.selector(), &window_selector());
    assert_eq!(r.control().as_str(), control().as_str());

    // The binding resolves the window and then the control inside it. The
    // control lookup re-resolves the just-pinned HWND through its handle
    // selector, so the window handle is read twice and the control once.
    assert_eq!(
        h.call_names(),
        [
            "AU3_WinGetHandle",
            "AU3_WinGetHandle",
            "AU3_ControlGetHandle"
        ],
        "{:?}",
        h.call_names()
    );
    // And the window is always resolved before its control.
    let first_control = h
        .call_names()
        .iter()
        .position(|n| n == "AU3_ControlGetHandle")
        .expect("a control lookup happened");
    assert!(
        h.call_names()[..first_control]
            .iter()
            .all(|n| n == "AU3_WinGetHandle")
    );
}

#[test]
fn bind_fails_when_no_window_matches() {
    let h = Harness::new();
    h.topology_reset();
    // No windows at all.
    let err = ControlRef::bind(&h.ai, &window_selector(), &control()).unwrap_err();
    assert!(
        matches!(err, Error::WindowNotFound { .. }),
        "expected WindowNotFound, got {err:?}"
    );
}

#[test]
fn bind_fails_when_the_window_has_no_such_control() {
    let h = Harness::new();
    h.topology_reset();
    h.topology_add_window(&window_selector().to_string(), WINDOW_HWND, true, true);
    h.topology_add_window(
        &window_handle_selector(WINDOW_HWND).to_string(),
        WINDOW_HWND,
        true,
        true,
    );

    let err = ControlRef::bind(&h.ai, &window_selector(), &control()).unwrap_err();
    assert!(
        matches!(err, Error::ControlNotFound { .. }),
        "expected ControlNotFound, got {err:?}"
    );
}

#[test]
fn a_ready_reference_reports_every_gate_open() {
    let h = Harness::new();
    let r = bind_ready(&h);

    let readiness = r.readiness().expect("observe should succeed");
    assert!(readiness.is_actionable(), "{readiness:?}");
    assert!(readiness.window_exists);
    assert!(readiness.control_identity_holds);
    assert!(readiness.window_visible && readiness.window_enabled);
    assert!(readiness.control_visible && readiness.control_enabled);
}

#[test]
fn click_verifies_then_acts_through_the_handle_ffi() {
    let h = Harness::new();
    let r = bind_ready(&h);

    let outcome = r
        .click(ClickOptions::default())
        .expect("ready control clicks");

    // The audit trail: the exact HWNDs, a measured duration, and the terminal
    // state that authorised the click.
    assert_eq!(outcome.window_handle, WINDOW_HWND);
    assert_eq!(outcome.control_handle, CONTROL_HWND);
    assert!(outcome.readiness.is_actionable());

    // Every check runs first, and the act itself is a ByHandle call — a
    // selector that now matched something else could not redirect it. The
    // three binding calls precede the verify-and-act sequence.
    let names = h.call_names();
    let verify_start = names
        .iter()
        .position(|n| n == "AU3_WinExists")
        .expect("a verification pass ran");
    assert_eq!(
        &names[verify_start..],
        [
            "AU3_WinExists",
            "AU3_WinGetHandle",
            "AU3_ControlGetHandle",
            "AU3_WinGetState",
            "AU3_ControlCommand",
            "AU3_ControlCommand",
            "AU3_ControlClickByHandle",
        ],
        "{names:?}"
    );
    let click = h
        .calls()
        .into_iter()
        .find(|c| c.starts_with("AU3_ControlClickByHandle"))
        .expect("a ByHandle click was made");
    assert!(
        click.contains(&format!("hwnd:{WINDOW_HWND:#x}"))
            && click.contains(&format!("hwnd:{CONTROL_HWND:#x}")),
        "click did not target the pinned handles: {click}"
    );
}

#[test]
fn send_set_text_and_command_all_target_the_pinned_handles() {
    let h = Harness::new();
    let r = bind_ready(&h);

    r.send(&autoitx::keys!("{TAB}")).expect("send");
    r.set_text("42").expect("set text");
    r.command("Check", "").expect("command");

    let names = h.call_names();
    for expected in [
        "AU3_ControlSendByHandle",
        "AU3_ControlSetTextByHandle",
        "AU3_ControlCommandByHandle",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "missing {expected}: {names:?}"
        );
    }
    // The verification passes use the selector-form ControlCommand; every
    // *action* must instead be a ByHandle call, three of them here.
    let by_handle_calls = names.iter().filter(|n| n.ends_with("ByHandle")).count();
    assert_eq!(
        by_handle_calls, 3,
        "three handle-addressed actions: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Readiness gate
// ---------------------------------------------------------------------------

#[test]
fn a_disabled_control_is_refused_before_any_action_is_attempted() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        false, // disabled
    );

    let err = r.click(ClickOptions::default()).unwrap_err();
    let inner = reference_error(&err).expect("a Reference error");
    match inner {
        ReferenceError::NotReady {
            enabled,
            visible,
            window_handle,
            control_handle,
            ..
        } => {
            assert!(!enabled);
            assert!(visible);
            assert_eq!(*window_handle, WINDOW_HWND);
            assert_eq!(*control_handle, CONTROL_HWND);
        }
        other => panic!("expected NotReady, got {other:?}"),
    }

    assert!(
        !h.call_names()
            .contains(&"AU3_ControlClickByHandle".to_owned()),
        "the action must not run when a gate failed: {:?}",
        h.call_names()
    );
}

#[test]
fn a_hidden_window_is_not_actionable_either() {
    let h = Harness::new();
    let r = bind_ready(&h);
    // Hide the window without recreating it: the control identity survives,
    // only the visibility gate closes.
    h.topology_set_window_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        false, // hidden
        true,
    );

    let err = r.set_text("x").unwrap_err();
    assert!(
        matches!(reference_error(&err), Some(ReferenceError::NotReady { .. })),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Window reuse: the generation guarantee
// ---------------------------------------------------------------------------

#[test]
fn window_reuse_fails_the_reference_and_keeps_failing() {
    let h = Harness::new();
    let r = bind_ready(&h);
    r.click(ClickOptions::default())
        .expect("works before the swap");

    // The pinned window dies. A *different* window then appears answering the
    // original selector — the classic title-prefix / dialog-pop scenario.
    h.topology_set_window_exists(&window_selector().to_string(), false);
    h.topology_set_window_exists(&window_handle_selector(WINDOW_HWND).to_string(), false);
    h.topology_add_window(
        &window_selector().to_string(),
        REPLACEMENT_WINDOW_HWND,
        true,
        true,
    );
    h.topology_add_window(
        &window_handle_selector(REPLACEMENT_WINDOW_HWND).to_string(),
        REPLACEMENT_WINDOW_HWND,
        true,
        true,
    );

    let err = r.click(ClickOptions::default()).unwrap_err();
    match reference_error(&err).expect("a Reference error") {
        ReferenceError::WindowGenerationChanged {
            window_handle,
            generation,
            ..
        } => {
            assert_eq!(*window_handle, WINDOW_HWND);
            assert_eq!(*generation, 2);
        }
        other => panic!("expected WindowGenerationChanged, got {other:?}"),
    }

    // Sticky: even after the original HWND comes back, the reference refuses.
    register_ready_window(&h, WINDOW_HWND, CONTROL_HWND);
    let again = r.readiness().unwrap_err();
    assert!(
        matches!(
            reference_error(&again),
            Some(ReferenceError::WindowGenerationChanged { generation: 2, .. })
        ),
        "the generation error must be sticky, got {again:?}"
    );
    assert_eq!(r.generation(), 2);

    // And the replacement window's HWND must never have been used as a target.
    let leaked = h
        .calls()
        .iter()
        .any(|c| c.contains(&format!("{REPLACEMENT_WINDOW_HWND:#x}")));
    assert!(
        !leaked,
        "the reference silently retargeted the replacement window:\n{}",
        h.log()
    );
}

#[test]
fn a_window_that_simply_disappears_is_distinct_from_a_reused_one() {
    let h = Harness::new();
    let r = bind_ready(&h);

    // Gone, with no successor answering the selector: plain WindowGone.
    h.topology_set_window_exists(&window_selector().to_string(), false);
    h.topology_set_window_exists(&window_handle_selector(WINDOW_HWND).to_string(), false);

    let err = r.readiness().unwrap_err();
    assert!(
        matches!(
            reference_error(&err),
            Some(ReferenceError::WindowGone { .. })
        ),
        "{err:?}"
    );
    // No generation bump and no sticky death: the identity was not replaced,
    // so the same reference may keep polling for the window to return.
    assert_eq!(r.generation(), 1);
}

// ---------------------------------------------------------------------------
// Control replacement and disappearance
// ---------------------------------------------------------------------------

#[test]
fn a_rebuilt_control_with_the_same_id_is_a_replacement_not_a_retarget() {
    let h = Harness::new();
    let r = bind_ready(&h);

    // Same window, same control id, new HWND: what a widget rebuild looks
    // like. A fresh selector lookup would silently drive the new widget.
    h.topology_add_control(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        REPLACEMENT_CONTROL_HWND,
        true,
        true,
        "OK",
    );

    let err = r.click(ClickOptions::default()).unwrap_err();
    match reference_error(&err).expect("a Reference error") {
        ReferenceError::ControlReplaced {
            bound_handle,
            current_handle,
            ..
        } => {
            assert_eq!(*bound_handle, CONTROL_HWND);
            assert_eq!(*current_handle, REPLACEMENT_CONTROL_HWND);
        }
        other => panic!("expected ControlReplaced, got {other:?}"),
    }

    assert!(
        !h.call_names()
            .contains(&"AU3_ControlClickByHandle".to_owned()),
        "no action may run against the replacement: {:?}",
        h.call_names()
    );
}

#[test]
fn a_missing_control_is_observable_and_heals_without_a_rebind() {
    let h = Harness::new();
    let r = bind_ready(&h);

    // The window stays put; only the control vanishes.
    h.topology_remove_control(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
    );

    let err = r.readiness().unwrap_err();
    assert!(
        matches!(
            reference_error(&err),
            Some(ReferenceError::ControlGone { .. })
        ),
        "{err:?}"
    );
    // The window generation is untouched by a control-level disappearance.
    assert_eq!(r.generation(), 1);

    // The control returns under the same handle: no rebind needed.
    h.topology_add_control(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        CONTROL_HWND,
        true,
        true,
        "OK",
    );
    r.readiness()
        .expect("the reference should recover once the control returns");
    assert_eq!(r.generation(), 1);
}

// ---------------------------------------------------------------------------
// Bounded waiting: timeout and cancellation
// ---------------------------------------------------------------------------

#[test]
fn wait_enabled_times_out_instead_of_hanging_and_leaves_no_stuck_state() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        false, // never becomes enabled
    );

    let wait = Wait::timed(Duration::from_millis(80)).polling(Duration::from_millis(5));
    let started = Instant::now();
    let err = r.wait_enabled(wait).unwrap_err();
    let elapsed = started.elapsed();

    match reference_error(&err).expect("a Reference error") {
        ReferenceError::Timeout { operation, waited } => {
            assert_eq!(*operation, "control_ref::wait_enabled");
            assert!(
                *waited >= Duration::from_millis(70),
                "waited only {waited:?}"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(70),
        "returned before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed <= Duration::from_millis(500),
        "a capped nap must not let the wait overshoot far: {elapsed:?}"
    );

    // The lock must be fully released: enabling the control and acting on it
    // from the same thread works immediately, with no stall.
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        true,
    );
    r.click(ClickOptions::default())
        .expect("a timed-out wait must not poison later runs");
}

#[test]
fn wait_ready_returns_as_soon_as_the_control_becomes_ready() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        false,
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            // Re-enable from another thread while the wait polls.
            h.topology_set_control_state(
                &window_handle_selector(WINDOW_HWND).to_string(),
                control().as_str(),
                true,
                true,
            );
        });
        r.wait_ready(Wait::timed(Duration::from_secs(2)).polling(Duration::from_millis(5)))
            .expect("should become ready well within the deadline");
    });
}

#[test]
fn a_cancelled_wait_reports_cancellation_and_does_not_block_later_calls() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        false,
    );

    let token = CancellationToken::new();
    let token_clone = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        token_clone.cancel();
    });

    let err = r
        .wait_enabled(
            Wait::timed(Duration::from_secs(5))
                .polling(Duration::from_millis(5))
                .cancel_with(token),
        )
        .unwrap_err();

    assert!(
        matches!(
            reference_error(&err),
            Some(ReferenceError::Cancelled { .. })
        ),
        "{err:?}"
    );

    // Cancellation is an observation, not a poison: the next call runs.
    h.topology_set_control_state(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        true,
        true,
    );
    r.click(ClickOptions::default())
        .expect("cancellation must not wedge the session");
}

#[test]
fn an_already_cancelled_token_stops_the_wait_on_its_first_pass() {
    let h = Harness::new();
    // The control is fully ready: without the top-of-loop cancellation check,
    // the readiness predicate would win and the token would be ignored.
    let r = bind_ready(&h);

    let token = CancellationToken::new();
    token.cancel();
    let err = r
        .wait_ready(
            Wait::timed(Duration::from_secs(10))
                .polling(Duration::from_millis(1))
                .cancel_with(token),
        )
        .unwrap_err();

    assert!(
        matches!(
            reference_error(&err),
            Some(ReferenceError::Cancelled { .. })
        ),
        "{err:?}"
    );
}

#[test]
fn wait_text_changes_returns_the_old_and_new_text() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_text(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        "saving...",
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            h.topology_set_control_text(
                &window_handle_selector(WINDOW_HWND).to_string(),
                control().as_str(),
                "done",
            );
        });

        let change = r
            .wait_text_changes(
                "saving...",
                Wait::timed(Duration::from_secs(2)).polling(Duration::from_millis(5)),
            )
            .expect("the text should change");
        assert_eq!(change.from, "saving...");
        assert_eq!(change.to, "done");
        assert!(change.readiness.is_actionable());
    });
}

#[test]
fn wait_text_changes_times_out_when_the_text_never_moves() {
    let h = Harness::new();
    let r = bind_ready(&h);
    h.topology_set_control_text(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        "stable",
    );

    let err = r
        .wait_text_changes(
            "stable",
            Wait::timed(Duration::from_millis(60)).polling(Duration::from_millis(5)),
        )
        .unwrap_err();
    assert!(
        matches!(reference_error(&err), Some(ReferenceError::Timeout { .. })),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn control_ref_is_send_and_sync_across_threads() {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ControlRef>();
    assert_send_sync::<CancellationToken>();
}

#[test]
fn many_threads_can_share_one_reference_without_losing_identity() {
    let h = Harness::new();
    let r = Arc::new(bind_ready(&h));

    const THREADS: usize = 8;
    const CLICKS_PER_THREAD: usize = 25;

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let r = Arc::clone(&r);
            handles.push(scope.spawn(move || {
                for _ in 0..CLICKS_PER_THREAD {
                    let outcome = r.click(ClickOptions::default()).expect("every click lands");
                    assert_eq!(outcome.window_handle, WINDOW_HWND);
                    assert_eq!(outcome.control_handle, CONTROL_HWND);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }
    });

    // Every action went through the pinned HWNDs; nothing retargeted.
    let total_clicks = h
        .call_names()
        .iter()
        .filter(|n| *n == "AU3_ControlClickByHandle")
        .count();
    assert_eq!(total_clicks, THREADS * CLICKS_PER_THREAD);

    for call in h.calls() {
        if call.contains("ByHandle(") {
            assert!(
                call.contains(&format!("hwnd:{WINDOW_HWND:#x}")),
                "a ByHandle action left the pinned window: {call}"
            );
        }
    }
}

#[test]
fn a_polling_wait_releases_the_lock_between_passes() {
    let h = Harness::new();
    let r = Arc::new(bind_ready(&h));

    // One thread parks in a text-change wait. The text only changes after
    // 400 ms, so its loop sleeps (lock-free) over and over during that window.
    // Other threads must keep completing actions all the while; holding the
    // lock across a poll nap would stall them until the 400 ms mark.
    let first_worker_action = Arc::new(AtomicBool::new(false));

    let waiter = Arc::clone(&r);
    let waiter_thread = std::thread::spawn(move || {
        waiter.wait_text_changes(
            "OK",
            Wait::timed(Duration::from_secs(2)).polling(Duration::from_millis(10)),
        )
    });

    let mut workers = Vec::new();
    for _ in 0..4 {
        let r = Arc::clone(&r);
        let flag = Arc::clone(&first_worker_action);
        workers.push(std::thread::spawn(move || {
            for _ in 0..20 {
                r.send(&autoitx::keys!("x")).expect("send under contention");
                flag.store(true, Ordering::Release);
            }
        }));
    }

    // Generous deadline: one mock FFI round trip is microseconds; 250 ms is
    // reachable only if the workers were not blocked behind the waiter's nap.
    let progress_deadline = Instant::now() + Duration::from_millis(250);
    while !first_worker_action.load(Ordering::Acquire) && Instant::now() < progress_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        first_worker_action.load(Ordering::Acquire),
        "workers made no progress while the wait was polling"
    );

    std::thread::sleep(Duration::from_millis(400));
    h.topology_set_control_text(
        &window_handle_selector(WINDOW_HWND).to_string(),
        control().as_str(),
        "done",
    );

    let change = waiter_thread
        .join()
        .expect("waiter panicked")
        .expect("the text change should end the wait");
    assert_eq!(change.to, "done");
    for worker in workers {
        worker.join().expect("worker panicked");
    }
}

#[test]
fn a_generation_change_observed_on_one_thread_is_visible_on_every_clone() {
    let h = Harness::new();
    let r = Arc::new(bind_ready(&h));
    let r2 = Arc::clone(&r);

    // Swap the window out from under the reference.
    h.topology_set_window_exists(&window_selector().to_string(), false);
    h.topology_set_window_exists(&window_handle_selector(WINDOW_HWND).to_string(), false);
    h.topology_add_window(
        &window_selector().to_string(),
        REPLACEMENT_WINDOW_HWND,
        true,
        true,
    );
    h.topology_add_window(
        &window_handle_selector(REPLACEMENT_WINDOW_HWND).to_string(),
        REPLACEMENT_WINDOW_HWND,
        true,
        true,
    );

    let first = r.readiness().unwrap_err();
    assert!(
        matches!(
            reference_error(&first),
            Some(ReferenceError::WindowGenerationChanged { .. })
        ),
        "{first:?}"
    );

    // The clone shares the lineage, so it fails the same way without having
    // observed the swap itself — and without another backend round trip.
    let calls_before = h.call_names().len();
    let second = r2.readiness().unwrap_err();
    assert!(
        matches!(
            reference_error(&second),
            Some(ReferenceError::WindowGenerationChanged { .. })
        ),
        "{second:?}"
    );
    assert_eq!(
        h.call_names().len(),
        calls_before,
        "the sticky failure must short-circuit the backend"
    );
}

// ---------------------------------------------------------------------------
// Outcomes are auditable
// ---------------------------------------------------------------------------

#[test]
fn every_action_reports_handles_duration_and_terminal_state() {
    let h = Harness::new();
    let r = bind_ready(&h);

    let click = r.click(ClickOptions::default()).unwrap();
    let send = r.send(&autoitx::keys!("{ENTER}")).unwrap();
    let text = r.set_text("value").unwrap();

    for outcome in [click, send, text] {
        assert_eq!(outcome.window_handle, WINDOW_HWND);
        assert_eq!(outcome.control_handle, CONTROL_HWND);
        assert!(outcome.readiness.is_actionable());
        // A duration is always reported, even when the call is effectively
        // instant; callers log it unconditionally.
        assert!(outcome.elapsed <= Duration::from_secs(1));
    }
}

#[test]
fn cancellation_tokens_report_their_state_for_auditing() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
    let cloned = token.clone();
    token.cancel();
    assert!(cloned.is_cancelled(), "clones share one flag");
}
