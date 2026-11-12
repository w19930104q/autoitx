//! An audited action on a control whose window can be replaced under you.
//!
//! The hard case for an unattended document robot: between two calls the
//! application can pop an error dialog, retitle the window, or rebuild the
//! control. A fresh selector lookup then resolves to *whatever matches now* —
//! often the dialog — and the keystroke meant for the invoice field lands in
//! it, with success reported back.
//!
//! [`autoitx::reference::ControlRef`] turns that sequence into one auditable
//! action:
//!
//! 1. bind once, pinning the window and control HWNDs and the window
//!    generation;
//! 2. wait with a hard deadline for the control to become enabled;
//! 3. verify existence, visibility and enablement under one session lock;
//! 4. act through the `*ByHandle` FFI, so a dialog that now matches the
//!    selector cannot absorb the click;
//! 5. log the handles actually used, the elapsed time, and the terminal state.
//!
//! If the pinned window is closed and another window inherits its selector,
//! the generation changes and every later call fails rather than silently
//! retargeting.
//!
//! ```text
//! cargo run --example audited_control
//! ```

// Pinned references are built on the ByHandle control FFI, which only the DLL
// backend exposes. The native macOS backend has no handle-addressed controls,
// so the whole flow is gated like the DLL-only surface in `src/reference.rs`;
// the fallback `main` explains that rather than failing to compile.
#[cfg(any(windows, feature = "mock-loader"))]
use autoitx::reference::{Cancel, CancellationToken, ControlRef, ReferenceError, Wait};
#[cfg(any(windows, feature = "mock-loader"))]
use autoitx::{AutoIt, Control, Selector};
#[cfg(any(windows, feature = "mock-loader"))]
use std::time::Duration;

#[cfg(any(windows, feature = "mock-loader"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ai = AutoIt::new()?;

    // The human-facing selector. It is used exactly once, at bind time; every
    // later check addresses the pinned HWND instead.
    let modal = Selector::from("[CLASS:ui60Modal_W32;TITLE:Post Invoice]");
    let confirm = Control::id(1001);

    let ok = ControlRef::bind(&ai, &modal, &confirm)?;
    println!(
        "bound control {:#x} inside window {:#x} (generation {})",
        ok.control_handle(),
        ok.window_handle(),
        ok.generation()
    );

    // A supervisor can cancel the whole job; the token is shared, so any
    // thread holding a clone stops its wait at the next poll pass.
    let cancel = CancellationToken::new();
    let deadline = Wait::timed(Duration::from_secs(30))
        .polling(Duration::from_millis(250))
        .cancel_with(cancel.clone());

    // The confirm button stays greyed until the form validates. This is the
    // bounded wait: it cannot hang past the deadline, and a window reused
    // underneath it ends the reference instead of being treated as "not yet".
    match ok.wait_enabled(deadline) {
        Ok(readiness) => println!("confirm button became actionable: {readiness:?}"),
        Err(autoitx::Error::Reference(box_err)) => match *box_err {
            ReferenceError::Timeout { waited, .. } => {
                println!("never enabled after {waited:?}; abandoning this document");
                return Ok(());
            }
            ReferenceError::Cancelled { .. } => {
                println!("the job was shut down while waiting");
                return Ok(());
            }
            ReferenceError::WindowGenerationChanged { generation, .. } => {
                println!("the modal was replaced (generation {generation}); refusing to retarget");
                return Ok(());
            }
            other => return Err(other.into()),
        },
        Err(other) => return Err(other.into()),
    }

    // Verify-then-act is indivisible: the session lock spans both, and the
    // click goes to the pinned handles, not to a re-run selector.
    let outcome = ok.click(Default::default())?;
    println!(
        "clicked window {:#x} control {:#x} in {:?}; final readiness {:?}",
        outcome.window_handle, outcome.control_handle, outcome.elapsed, outcome.readiness,
    );

    // Waiting on a status field's text is the other bounded wait. Pass the
    // text read just before the action; the call returns the first change.
    let status = Control::id(2002);
    let status_ref = ControlRef::bind(&ai, &modal, &status)?;
    let previous = ai.control_get_text(&Selector::handle(status_ref.window_handle()), &status)?;
    let change = status_ref.wait_text_changes(&previous, Wait::timed(Duration::from_secs(10)))?;
    println!(
        "status moved from {:?} to {:?} in {:?}",
        change.from, change.to, change.elapsed
    );

    // The token is sticky on purpose: once a job is cancelling, a wait must
    // not resume because the flag was cleared.
    cancel.cancel();
    assert!(cancel.is_cancelled());

    Ok(())
}

#[cfg(not(any(windows, feature = "mock-loader")))]
fn main() {
    println!(
        "audited_control uses the DLL backend's handle-addressed control FFI,\n\
         which the native backend on this platform does not provide.\n\
         Run it on Windows, or with --features mock-loader against the mock DLL."
    );
}
