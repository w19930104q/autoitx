//! A fake `AutoItX3.dll`.
//!
//! Exports every `AU3_*` symbol with the real calling convention and
//! signatures, records what it was called with, and returns scripted values.
//! Pointing `AUTOITX_DLL` at the built artifact lets the whole marshalling
//! layer — UTF-16 round-tripping, output buffers, `AU3_error`, argument order
//! across all 117 functions — be tested on macOS as an ordinary `cargo test`.
//!
//! The exports are generated from [`autoitx_sys::au3_functions`], the same list
//! that generates the real bindings. That is the point: a signature fixed in
//! one place is fixed in both, and a passing mock test proves the *real*
//! declaration is callable rather than a copy of it that happens to agree.
//!
//! Built as a `cdylib`, so it is a `.dylib` on macOS and a `.dll` on Windows.
//! `libloading` opens either.
//!
//! Not published.

use autoitx_sys::{AU3_INTDEFAULT, DWORD, HWND, PCWSTR, POINT, PWSTR, RECT};
use std::collections::VecDeque;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Recorded state
// ---------------------------------------------------------------------------

/// One recorded call, rendered as `NAME(arg, arg, ...)`.
static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// What the next output-string function should write back.
static NEXT_STRING: Mutex<Option<Vec<u16>>> = Mutex::new(None);
/// What integer-returning functions should return once the queue is empty.
static NEXT_INT: Mutex<i32> = Mutex::new(0);
/// Scripted return values, consumed in order.
///
/// Needed to exercise sequences where the answer changes between calls — an
/// escalating close, for instance, where the window exists, ignores the close
/// request, and only goes away after the process is killed. A single fixed
/// value cannot express that.
static RETURN_QUEUE: Mutex<std::collections::VecDeque<i32>> =
    Mutex::new(std::collections::VecDeque::new());
/// What `AU3_error` reports. Kept separate from the queue on purpose — see
/// [`Ret`].
static NEXT_ERROR: Mutex<i32> = Mutex::new(0);

/// The next scripted integer: the queue if it has one, else the fixed value.
///
/// Written without a `let` chain: those need Rust 1.88, and this workspace's
/// MSRV is 1.85.
fn next_int() -> i32 {
    let queued = RETURN_QUEUE.lock().ok().and_then(|mut q| q.pop_front());
    match queued {
        Some(v) => v,
        None => NEXT_INT.lock().map(|g| *g).unwrap_or(0),
    }
}

fn record(call: String) {
    if let Ok(mut g) = CALLS.lock() {
        g.push(call);
    }
}

// ---------------------------------------------------------------------------
// Argument rendering
// ---------------------------------------------------------------------------

/// One captured argument, kept structured rather than pre-rendered.
///
/// Structure is what lets the log post-pass recognise an output buffer: in
/// AutoItX's ABI a `PWSTR` is *always* immediately followed by its capacity, so
/// the pair is detectable by walking the captured list. Doing it here, at
/// runtime, avoids asking `macro_rules!` to pattern-match on types — which it
/// cannot do without ambiguity.
enum ArgVal {
    Int(i32),
    Str(String),
    OutBuf(PWSTR),
    Handle(HWND),
    RectOut(bool),
    PointOut(bool),
}

/// Captures one FFI argument.
///
/// Implemented per concrete pointer type so the macro needs no type dispatch.
trait Arg {
    fn capture(&self) -> ArgVal;
}

impl Arg for i32 {
    fn capture(&self) -> ArgVal {
        ArgVal::Int(*self)
    }
}

impl Arg for PCWSTR {
    /// Decodes an input wide string — the assertion that actually matters, and
    /// the reason non-ASCII round-tripping is testable without Windows.
    fn capture(&self) -> ArgVal {
        if self.is_null() {
            return ArgVal::Str("\0null".to_owned());
        }
        // AutoItX's contract is that every LPCWSTR is NUL-terminated; the code
        // under test must uphold it, and violating it is exactly the bug this
        // mock exists to surface.
        let mut len = 0usize;
        // SAFETY: walking to the NUL terminator the ABI guarantees.
        while unsafe { *self.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` wide chars were just proven readable.
        let slice = unsafe { std::slice::from_raw_parts(*self, len) };
        ArgVal::Str(String::from_utf16_lossy(slice))
    }
}

impl Arg for PWSTR {
    fn capture(&self) -> ArgVal {
        ArgVal::OutBuf(*self)
    }
}

impl Arg for HWND {
    fn capture(&self) -> ArgVal {
        ArgVal::Handle(*self)
    }
}

impl Arg for *mut RECT {
    fn capture(&self) -> ArgVal {
        ArgVal::RectOut(self.is_null())
    }
}

impl Arg for *mut POINT {
    fn capture(&self) -> ArgVal {
        ArgVal::PointOut(self.is_null())
    }
}

/// Renders the captured arguments and services any output buffer among them.
///
/// The `(PWSTR, i32)` pair is AutoItX's universal output-string convention, so
/// recognising it here covers every such function with no per-function work.
fn finish(name: &str, args: &[ArgVal]) -> String {
    let mut parts = Vec::with_capacity(args.len());

    for (i, a) in args.iter().enumerate() {
        parts.push(match a {
            // The sentinel is named, so a test asserting a parameter was
            // defaulted need not know the magic number.
            ArgVal::Int(v) if *v == AU3_INTDEFAULT => "INTDEFAULT".to_owned(),
            ArgVal::Int(v) => v.to_string(),
            ArgVal::Str(s) if s == "\0null" => "null".to_owned(),
            ArgVal::Str(s) => format!("{s:?}"),
            ArgVal::Handle(h) if h.is_null() => "hwnd:null".to_owned(),
            ArgVal::Handle(h) => format!("hwnd:{:#x}", *h as usize),
            ArgVal::RectOut(true) => "rect:null".to_owned(),
            ArgVal::RectOut(false) => "rect:out".to_owned(),
            ArgVal::PointOut(true) => "point:null".to_owned(),
            ArgVal::PointOut(false) => "point:out".to_owned(),
            ArgVal::OutBuf(buf) => {
                if let Some(ArgVal::Int(size)) = args.get(i + 1) {
                    fill_out_buffer(name, args, *buf, *size);
                }
                if buf.is_null() { "null" } else { "out" }.to_owned()
            }
        });
    }

    format!("{name}({})", parts.join(", "))
}

// ---------------------------------------------------------------------------
// Return values
// ---------------------------------------------------------------------------

/// Supplies a value for each return type the ABI uses.
///
/// Takes the function name because one function must not behave like the
/// others: `AU3_error` is a status read that the safe layer performs after
/// *every* call. If it drew from the scripted queue like an operation does, it
/// would consume half the script and shift every subsequent answer onto the
/// wrong call.
trait Ret: Sized {
    fn mock(name: &str) -> Self;

    /// Topology-aware variant. The default ignores the arguments and behaves
    /// exactly like [`mock`](Self::mock), so types whose answers never depend
    /// on what was passed need no override.
    fn mock_with(name: &str, args: &[ArgVal]) -> Self {
        let _ = args;
        Self::mock(name)
    }
}

impl Ret for i32 {
    fn mock(name: &str) -> Self {
        if name == "AU3_error" {
            return NEXT_ERROR.lock().map(|g| *g).unwrap_or(0);
        }
        next_int()
    }

    fn mock_with(name: &str, args: &[ArgVal]) -> Self {
        if name == "AU3_error" {
            return NEXT_ERROR.lock().map(|g| *g).unwrap_or(0);
        }
        // Only intercept when a test enabled topology; queue mode must keep
        // its exact pre-topology behaviour.
        if topology_enabled() {
            if let Some(value) = topology_int(name, args) {
                return value;
            }
        }
        next_int()
    }
}

impl Ret for DWORD {
    fn mock(name: &str) -> Self {
        let _ = name;
        next_int() as Self
    }

    fn mock_with(name: &str, args: &[ArgVal]) -> Self {
        if topology_enabled() {
            if let Some(value) = topology_int(name, args) {
                return value as Self;
            }
        }
        next_int() as Self
    }
}

impl Ret for HWND {
    fn mock(name: &str) -> Self {
        let _ = name;
        // Drawn from the same scripted queue as the integers, so a test can
        // hand out a handle: `script_ints(&[0x1234])`. A 0 stays null, which is
        // how AutoItX says "no window".
        next_int() as usize as Self
    }

    fn mock_with(name: &str, args: &[ArgVal]) -> Self {
        if topology_enabled() {
            if let Some(handle) = topology_handle(name, args) {
                return handle;
            }
        }
        next_int() as usize as Self
    }
}

/// Writes the scripted string into an output buffer, honouring `buf_size`.
///
/// AutoItX writes at most `buf_size` wide chars **including** the NUL and never
/// reports how much room it needed — so the mock truncates the same way, which
/// is what makes the safe layer's grow-and-retry loop testable.
fn fill_out_buffer(name: &str, args: &[ArgVal], buf: PWSTR, buf_size: i32) {
    // Topology answers its own string-producing calls first.
    if topology_fill_string(name, args, buf, buf_size) {
        return;
    }
    if buf.is_null() || buf_size <= 0 {
        return;
    }
    // The queued string models values that change between calls; the fixed
    // slot remains the fallback for one-shot scripts.
    let queued = STRING_QUEUE.lock().ok().and_then(|mut q| q.pop_front());
    let scripted = queued.or_else(|| NEXT_STRING.lock().ok().and_then(|g| g.clone()));
    let src = scripted.unwrap_or_default();
    let cap = buf_size as usize;
    let n = src.len().min(cap - 1);
    // SAFETY: the caller guarantees `buf` is writable for `buf_size` wide
    // chars; `n + 1 <= cap` by construction.
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
}

// ---------------------------------------------------------------------------
// Scripted window/control topology
// ---------------------------------------------------------------------------
//
// The queue machinery above answers "what should the next call return?"; it
// cannot answer "does this window still exist, and what controls does it
// have?". The safe layer's pinned control references need exactly that kind of
// stateful, identity-preserving answer, so the mock can also run as a tiny
// window manager: tests declare windows and their controls, and the AU3 calls
// the reference type makes are answered from this topology instead of the
// queue.
//
// The two modes never overlap: topology is empty until a test enables it, and
// every pre-existing test keeps the queue's exact behaviour.

/// One scripted control.
#[derive(Clone)]
struct MockControl {
    /// The identifier string the safe layer passes, e.g. `[ID:1001]`.
    id: String,
    /// Its HWND; the identity the reference type pins and re-checks.
    handle: usize,
    visible: bool,
    enabled: bool,
    text: Vec<u16>,
}

/// One scripted window.
#[derive(Clone)]
struct MockWindow {
    /// The selector string the safe layer passes, e.g. `[HANDLE:1a2b]`.
    selector: String,
    handle: usize,
    exists: bool,
    visible: bool,
    enabled: bool,
    controls: Vec<MockControl>,
}

/// The scripted topology, plus the flag that switches the mock into it.
static TOPOLOGY: Mutex<Option<Vec<MockWindow>>> = Mutex::new(None);
/// Scripted output strings, consumed in FIFO order.
///
/// The fixed [`NEXT_STRING`] slot cannot model a value that changes between
/// calls — waiting for text to change reads the same control twice. The queue
/// fills that gap; when empty, the fixed slot applies as before.
static STRING_QUEUE: Mutex<VecDeque<Vec<u16>>> = Mutex::new(VecDeque::new());

/// Decodes an input wide pointer into an owned `String`.
///
/// # Safety
///
/// Same contract as [`Arg::capture`] for [`PCWSTR`]: NUL-terminated and
/// readable.
unsafe fn read_wide(p: PCWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: caller guarantees the NUL-terminated, readable string.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` code units were just proven readable.
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    String::from_utf16_lossy(slice)
}

/// Whether topology mode is on.
fn topology_enabled() -> bool {
    TOPOLOGY.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Looks up a window by the exact selector string it was registered with.
fn find_window(selector: &str) -> Option<usize> {
    let guard = TOPOLOGY.lock().ok()?;
    let windows = guard.as_ref()?;
    windows
        .iter()
        .position(|w| w.selector == selector && w.exists)
}

/// Looks up a control inside an existing window.
fn find_control(window_index: usize, control: &str) -> Option<usize> {
    let guard = TOPOLOGY.lock().ok()?;
    let windows = guard.as_ref()?;
    let window = windows.get(window_index)?;
    window.controls.iter().position(|c| c.id == control)
}

/// Topology-aware answer for an integer-returning AU3 call.
///
/// Returns `None` outside topology mode, or for functions topology does not
/// model, so the queue path remains the fallback.
fn topology_int(name: &str, args: &[ArgVal]) -> Option<i32> {
    let selector = |i: usize| match args.get(i)? {
        ArgVal::Str(s) => Some(s.clone()),
        _ => None,
    };

    match name {
        "AU3_WinExists" | "AU3_WinActive" => {
            let sel = selector(0)?;
            Some(i32::from(find_window(&sel).is_some()))
        }
        "AU3_WinGetState" => {
            let index = find_window(&selector(0)?)?;
            let guard = TOPOLOGY.lock().ok()?;
            let window = guard.as_ref()?.get(index)?;
            let mut bits = 1; // EXISTS
            if window.visible {
                bits |= 2;
            }
            if window.enabled {
                bits |= 4;
            }
            Some(bits)
        }
        "AU3_ControlEnable" | "AU3_ControlDisable" | "AU3_ControlShow" | "AU3_ControlHide"
        | "AU3_ControlFocus" => {
            // The selector forms, taking (title, text, control).
            let win_sel = selector(0)?;
            let ctrl_id = selector(2)?;
            let window = find_window(&win_sel)?;
            let control = find_control(window, &ctrl_id)?;
            let mut guard = TOPOLOGY.lock().ok()?;
            let entry = guard.as_mut()?.get_mut(window)?.controls.get_mut(control)?;
            match name {
                "AU3_ControlEnable" => entry.enabled = true,
                "AU3_ControlDisable" => entry.enabled = false,
                "AU3_ControlShow" => entry.visible = true,
                "AU3_ControlHide" => entry.visible = false,
                _ => {}
            }
            Some(1)
        }
        // Every ByHandle action the pinned reference actually drives. Topology
        // only needs to acknowledge them; identity was checked before the call.
        "AU3_ControlClickByHandle"
        | "AU3_ControlSendByHandle"
        | "AU3_ControlSetTextByHandle"
        | "AU3_ControlEnableByHandle"
        | "AU3_ControlDisableByHandle"
        | "AU3_ControlFocusByHandle"
        | "AU3_ControlShowByHandle"
        | "AU3_ControlHideByHandle" => Some(1),
        _ => None,
    }
}

/// Topology-aware answer for an HWND-returning AU3 call.
fn topology_handle(name: &str, args: &[ArgVal]) -> Option<HWND> {
    match name {
        "AU3_WinGetHandle" => {
            let sel = match args.first()? {
                ArgVal::Str(s) => s.clone(),
                _ => return None,
            };
            let index = find_window(&sel)?;
            let guard = TOPOLOGY.lock().ok()?;
            let handle = guard.as_ref()?.get(index)?.handle;
            Some(handle as HWND)
        }
        "AU3_ControlGetHandle" => {
            // (HWND parent, PCWSTR control id)
            let parent = match args.first()? {
                ArgVal::Handle(h) => *h as usize,
                _ => return None,
            };
            let ctrl_id = match args.get(1)? {
                ArgVal::Str(s) => s.clone(),
                _ => return None,
            };
            let guard = TOPOLOGY.lock().ok()?;
            let windows = guard.as_ref()?;
            // The parent is an HWND, and tests may register several selector
            // aliases of the same window under that one HWND. The control's
            // identity belongs to the HWND, not the alias, so every matching
            // window entry gets a look.
            windows
                .iter()
                .filter(|w| w.exists && w.handle == parent)
                .find_map(|w| w.controls.iter().find(|c| c.id == ctrl_id))
                .map(|c| c.handle as HWND)
        }
        _ => None,
    }
}

/// Fills an output buffer from topology, where the call produces one.
///
/// Returns `true` when topology answered, `false` to fall through to the
/// scripted-string machinery.
fn topology_fill_string(name: &str, args: &[ArgVal], buf: PWSTR, buf_size: i32) -> bool {
    if !topology_enabled() || buf.is_null() || buf_size <= 0 {
        return false;
    }

    let write = |value: &[u16]| {
        let cap = buf_size as usize;
        let n = value.len().min(cap - 1);
        // SAFETY: caller guarantees `buf` is writable for `buf_size` units.
        unsafe {
            std::ptr::copy_nonoverlapping(value.as_ptr(), buf, n);
            *buf.add(n) = 0;
        }
    };

    /// Resolves `(window index, control index)` for the selector-form call
    /// shape `(title, text, control, ...)`.
    fn selector_target(args: &[ArgVal]) -> Option<(usize, usize)> {
        let (ArgVal::Str(win_sel), ArgVal::Str(ctrl_id)) = (args.first()?, args.get(2)?) else {
            return None;
        };
        let window = find_window(win_sel)?;
        let control = find_control(window, ctrl_id)?;
        Some((window, control))
    }

    /// Resolves the target for the ByHandle call shape `(hwnd, hctrl, ...)`.
    ///
    /// Like [`topology_handle`], this looks past selector aliases: several
    /// topology entries may share one window HWND, and the control belongs to
    /// the HWND rather than whichever alias registered it.
    fn handle_target(args: &[ArgVal]) -> Option<(usize, usize)> {
        let (ArgVal::Handle(parent), ArgVal::Handle(hctrl)) = (args.first()?, args.get(1)?) else {
            return None;
        };
        let guard = TOPOLOGY.lock().ok()?;
        let windows = guard.as_ref()?;
        windows.iter().enumerate().find_map(|(wi, w)| {
            if !w.exists || w.handle != *parent as usize {
                return None;
            }
            w.controls
                .iter()
                .position(|c| c.handle == *hctrl as usize)
                .map(|ci| (wi, ci))
        })
    }

    match name {
        "AU3_ControlGetText" => {
            let Some((window, control)) = selector_target(args) else {
                return false;
            };
            let Ok(guard) = TOPOLOGY.lock() else {
                return false;
            };
            let Some(text) = guard
                .as_ref()
                .and_then(|w| w.get(window))
                .and_then(|w| w.controls.get(control))
                .map(|c| c.text.clone())
            else {
                return false;
            };
            write(&text);
            true
        }
        "AU3_ControlCommand" | "AU3_ControlCommandByHandle" => {
            // The command sits at index 3 (selector form) or 2 (handle form);
            // the output buffer is always the penultimate argument.
            let command_index = if name == "AU3_ControlCommand" { 3 } else { 2 };
            let Some(ArgVal::Str(command)) = args.get(command_index) else {
                return false;
            };
            let target = if name == "AU3_ControlCommand" {
                selector_target(args)
            } else {
                handle_target(args)
            };
            let Some((window, control)) = target else {
                return false;
            };

            let answer = {
                let Ok(guard) = TOPOLOGY.lock() else {
                    return false;
                };
                let Some(entry) = guard
                    .as_ref()
                    .and_then(|w| w.get(window))
                    .and_then(|w| w.controls.get(control))
                else {
                    return false;
                };
                match command.as_str() {
                    "IsVisible" => {
                        if entry.visible {
                            "1"
                        } else {
                            "0"
                        }
                    }
                    "IsEnabled" => {
                        if entry.enabled {
                            "1"
                        } else {
                            "0"
                        }
                    }
                    _ => "",
                }
            };
            let wide: Vec<u16> = answer.encode_utf16().chain(std::iter::once(0)).collect();
            write(&wide);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The generated exports
// ---------------------------------------------------------------------------

/// Emits one `#[no_mangle] extern "system"` shim per declared AU3 function.
macro_rules! mock_impl {
    ($(
        $(#[$meta:meta])*
        fn $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)? ;
    )+) => {
        $(
            #[doc = concat!("Mock of `", stringify!($name), "`.")]
            ///
            /// # Safety
            ///
            /// Called across FFI with the AutoItX ABI. Pointer arguments must
            /// satisfy the same contract the real DLL requires.
            // The exported symbol must match the DLL's name byte for byte, so
            // Rust's naming convention cannot apply here.
            #[allow(non_snake_case)]
            #[unsafe(no_mangle)]
            pub unsafe extern "system" fn $name($($arg: $ty),*) $(-> $ret)? {
                // `finish` both renders the log line and services any output
                // buffer it finds, so no per-function special-casing is needed.
                let captured = [$(Arg::capture(&$arg)),*];
                record(finish(
                    stringify!($name),
                    &captured,
                ));

                $(<$ret as Ret>::mock_with(stringify!($name), &captured))?
            }
        )+
    };
}

autoitx_sys::au3_functions!(mock_impl);

// ---------------------------------------------------------------------------
// Test-control surface (not part of the AutoItX ABI)
// ---------------------------------------------------------------------------

/// Clears the call log and any scripted return values.
///
/// # Safety
///
/// Called across FFI; takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_reset() {
    if let Ok(mut g) = CALLS.lock() {
        g.clear();
    }
    if let Ok(mut g) = NEXT_STRING.lock() {
        *g = None;
    }
    if let Ok(mut g) = NEXT_INT.lock() {
        *g = 0;
    }
    if let Ok(mut q) = RETURN_QUEUE.lock() {
        q.clear();
    }
    if let Ok(mut g) = NEXT_ERROR.lock() {
        *g = 0;
    }
    if let Ok(mut g) = TOPOLOGY.lock() {
        *g = None;
    }
    if let Ok(mut q) = STRING_QUEUE.lock() {
        q.clear();
    }
}

/// Scripts what `AU3_error` reports after the next call.
///
/// # Safety
///
/// Called across FFI; takes a plain integer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_set_next_error(v: i32) {
    if let Ok(mut g) = NEXT_ERROR.lock() {
        *g = v;
    }
}

/// Number of AU3 calls recorded since the last reset.
///
/// # Safety
///
/// Called across FFI; takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_call_count() -> i32 {
    CALLS.lock().map(|g| g.len() as i32).unwrap_or(-1)
}

/// Copies the call log into `buf` as NUL-terminated UTF-8, one call per line.
///
/// Returns the number of bytes that *would* be needed, so a caller can retry
/// with a larger buffer.
///
/// # Safety
///
/// `buf` must be writable for `buf_size` bytes, or null to query the size.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_take_log(buf: *mut u8, buf_size: i32) -> i32 {
    let joined = CALLS
        .lock()
        .map(|g| g.join("\n"))
        .unwrap_or_else(|_| String::new());
    let bytes = joined.as_bytes();
    let needed = bytes.len() + 1;

    if !buf.is_null() && buf_size > 0 {
        let n = bytes.len().min(buf_size as usize - 1);
        // SAFETY: caller guarantees `buf` is writable for `buf_size` bytes,
        // and `n + 1 <= buf_size`.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
            *buf.add(n) = 0;
        }
    }
    needed as i32
}

/// Scripts what the next output-string function writes back.
///
/// # Safety
///
/// `s` must be a NUL-terminated wide string, or null to clear.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_set_next_string(s: PCWSTR) {
    let value = if s.is_null() {
        None
    } else {
        let mut len = 0usize;
        // SAFETY: caller guarantees NUL termination.
        while unsafe { *s.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` wide chars were just proven readable.
        Some(unsafe { std::slice::from_raw_parts(s, len) }.to_vec())
    };
    if let Ok(mut g) = NEXT_STRING.lock() {
        *g = value;
    }
}

/// Scripts what integer-returning functions return once the queue is empty.
///
/// # Safety
///
/// Called across FFI; takes a plain integer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_set_next_int(v: i32) {
    if let Ok(mut g) = NEXT_INT.lock() {
        *g = v;
    }
}

/// Queues one integer return value, consumed before the fixed one.
///
/// Call repeatedly to script a sequence of differing answers.
///
/// # Safety
///
/// Called across FFI; takes a plain integer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_push_int(v: i32) {
    if let Ok(mut q) = RETURN_QUEUE.lock() {
        q.push_back(v);
    }
}

// ---------------------------------------------------------------------------
// Topology control surface (not part of the AutoItX ABI)
// ---------------------------------------------------------------------------

/// Enables topology mode with an empty window list.
///
/// # Safety
///
/// Called across FFI; takes no arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_reset() {
    if let Ok(mut g) = TOPOLOGY.lock() {
        *g = Some(Vec::new());
    }
}

/// Adds or replaces a window in the topology.
///
/// Registering a selector that already exists replaces that entry, which is
/// how a test simulates a window being destroyed and recreated — the caller
/// chooses whether the new window keeps the same HWND (reuse) or takes a new
/// one.
///
/// # Safety
///
/// `selector` must be a NUL-terminated wide string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_add_window(
    selector: PCWSTR,
    handle: usize,
    visible: i32,
    enabled: i32,
) {
    // SAFETY: caller guarantees NUL termination.
    let selector = unsafe { read_wide(selector) };
    let window = MockWindow {
        selector,
        handle,
        exists: true,
        visible: visible != 0,
        enabled: enabled != 0,
        controls: Vec::new(),
    };
    if let Ok(mut g) = TOPOLOGY.lock() {
        let windows = g.get_or_insert_with(Vec::new);
        if let Some(existing) = windows.iter_mut().find(|w| w.selector == window.selector) {
            *existing = window;
        } else {
            windows.push(window);
        }
    }
}

/// Adds or replaces a control in an existing topology window.
///
/// Re-adding the same control id with a different handle simulates a widget
/// rebuild: the identifier survives, the pinned handle does not.
///
/// # Safety
///
/// The two strings must be NUL-terminated wide strings; `text` may be null for
/// an empty control.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_add_control(
    window_selector: PCWSTR,
    control_id: PCWSTR,
    handle: usize,
    visible: i32,
    enabled: i32,
    text: PCWSTR,
) {
    // SAFETY: caller guarantees NUL termination; null reads as "".
    let window_selector = unsafe { read_wide(window_selector) };
    // SAFETY: caller guarantees NUL termination.
    let control_id = unsafe { read_wide(control_id) };
    let text = if text.is_null() {
        Vec::new()
    } else {
        let mut len = 0usize;
        // SAFETY: caller guarantees NUL termination.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were just proven readable.
        unsafe { std::slice::from_raw_parts(text, len) }.to_vec()
    };

    let control = MockControl {
        id: control_id,
        handle,
        visible: visible != 0,
        enabled: enabled != 0,
        text,
    };

    if let Ok(mut g) = TOPOLOGY.lock() {
        let Some(windows) = g.as_mut() else {
            return;
        };
        let Some(window) = windows.iter_mut().find(|w| w.selector == window_selector) else {
            return;
        };
        if let Some(existing) = window.controls.iter_mut().find(|c| c.id == control.id) {
            *existing = control;
        } else {
            window.controls.push(control);
        }
    }
}

/// Toggles whether a topology window reports as existing.
///
/// # Safety
///
/// `selector` must be a NUL-terminated wide string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_set_window_exists(selector: PCWSTR, exists: i32) {
    // SAFETY: caller guarantees NUL termination.
    let selector = unsafe { read_wide(selector) };
    if let Ok(mut g) = TOPOLOGY.lock() {
        if let Some(windows) = g.as_mut() {
            if let Some(window) = windows.iter_mut().find(|w| w.selector == selector) {
                window.exists = exists != 0;
            }
        }
    }
}

/// Updates a window's visible/enabled flags without recreating it.
///
/// Unlike [`MOCK_topology_add_window`], this leaves the window's controls in
/// place, so a readiness test can hide the window without losing the control
/// identity.
///
/// # Safety
///
/// `selector` must be a NUL-terminated wide string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_set_window_state(
    selector: PCWSTR,
    visible: i32,
    enabled: i32,
) {
    // SAFETY: caller guarantees NUL termination.
    let selector = unsafe { read_wide(selector) };
    if let Ok(mut g) = TOPOLOGY.lock() {
        if let Some(windows) = g.as_mut() {
            if let Some(window) = windows.iter_mut().find(|w| w.selector == selector) {
                window.visible = visible != 0;
                window.enabled = enabled != 0;
            }
        }
    }
}

/// Removes a control from its window, the state behind `ControlGone`.
///
/// The window itself is untouched, so re-adding the same control with the same
/// handle models a transient rebuild with no generation change.
///
/// # Safety
///
/// The two strings must be NUL-terminated wide strings.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_remove_control(
    window_selector: PCWSTR,
    control_id: PCWSTR,
) {
    // SAFETY: caller guarantees NUL termination.
    let window_selector = unsafe { read_wide(window_selector) };
    // SAFETY: caller guarantees NUL termination.
    let control_id = unsafe { read_wide(control_id) };
    if let Ok(mut g) = TOPOLOGY.lock() {
        let Some(windows) = g.as_mut() else {
            return;
        };
        let Some(window) = windows.iter_mut().find(|w| w.selector == window_selector) else {
            return;
        };
        window.controls.retain(|c| c.id != control_id);
    }
}

/// Updates one control's visibility/enablement flags.
///
/// # Safety
///
/// The two strings must be NUL-terminated wide strings.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_set_control_state(
    window_selector: PCWSTR,
    control_id: PCWSTR,
    visible: i32,
    enabled: i32,
) {
    // SAFETY: caller guarantees NUL termination.
    let window_selector = unsafe { read_wide(window_selector) };
    // SAFETY: caller guarantees NUL termination.
    let control_id = unsafe { read_wide(control_id) };
    if let Ok(mut g) = TOPOLOGY.lock() {
        let Some(windows) = g.as_mut() else {
            return;
        };
        let Some(window) = windows.iter_mut().find(|w| w.selector == window_selector) else {
            return;
        };
        let Some(control) = window.controls.iter_mut().find(|c| c.id == control_id) else {
            return;
        };
        control.visible = visible != 0;
        control.enabled = enabled != 0;
    }
}

/// Replaces a control's text, the state change `wait_text_changes` polls for.
///
/// # Safety
///
/// The strings must be NUL-terminated wide strings; `text` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_topology_set_control_text(
    window_selector: PCWSTR,
    control_id: PCWSTR,
    text: PCWSTR,
) {
    // SAFETY: caller guarantees NUL termination.
    let window_selector = unsafe { read_wide(window_selector) };
    // SAFETY: caller guarantees NUL termination.
    let control_id = unsafe { read_wide(control_id) };
    let text = if text.is_null() {
        Vec::new()
    } else {
        let mut len = 0usize;
        // SAFETY: caller guarantees NUL termination.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were just proven readable.
        unsafe { std::slice::from_raw_parts(text, len) }.to_vec()
    };
    if let Ok(mut g) = TOPOLOGY.lock() {
        let Some(windows) = g.as_mut() else {
            return;
        };
        let Some(window) = windows.iter_mut().find(|w| w.selector == window_selector) else {
            return;
        };
        let Some(control) = window.controls.iter_mut().find(|c| c.id == control_id) else {
            return;
        };
        control.text = text;
    }
}

/// Queues one output string, consumed before the fixed slot.
///
/// The queue form is what lets a test script two different reads of the same
/// call — the shape behind "wait until the text changes".
///
/// # Safety
///
/// `s` must be a NUL-terminated wide string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn MOCK_push_string(s: PCWSTR) {
    if s.is_null() {
        return;
    }
    let mut len = 0usize;
    // SAFETY: caller guarantees NUL termination.
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just proven readable.
    let value = unsafe { std::slice::from_raw_parts(s, len) }.to_vec();
    if let Ok(mut q) = STRING_QUEUE.lock() {
        q.push_back(value);
    }
}
