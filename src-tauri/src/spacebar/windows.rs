//! Windows hold-Space dictation.
//!
//! A `WH_KEYBOARD_LL` hook classifies Space on the hook thread. That thread
//! only reads atomics and the pure [`super::Machine`]: UI Automation, caret
//! lookup, and the transcription coordinator all run elsewhere, so the hook
//! stays inside the low-level hook timeout.
//!
//! A tap synthesizes one Space with `SendInput` instead of letting the
//! physical key through. An interrupting key gets that Space injected *before*
//! it is forwarded, which is what keeps "hello world" in order. Injected
//! events are ignored, and a thread-local flag covers the re-entrant call
//! `SendInput` makes on this same thread.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{LazyLock, Mutex, Once, OnceLock};
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use tauri::{AppHandle, Manager};
use windows::core::{Interface, BOOL, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, ClientToScreen, CreateEllipticRgn, CreateSolidBrush, DeleteObject, EndPaint,
    FillRect, GetMonitorInfoW, MonitorFromWindow, SetWindowRgn, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, SAFEARRAY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern2,
    IUIAutomationTextRange, IUIAutomationValuePattern, SetWinEventHook, UIA_ComboBoxControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_TextPattern2Id, UIA_ValuePatternId,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU,
    VK_RWIN, VK_SHIFT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow,
    GetGUIThreadInfo, GetMessageW, GetWindowRect, GetWindowThreadProcessId, IsIconic, KillTimer,
    PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, EVENT_OBJECT_FOCUS,
    EVENT_SYSTEM_FOREGROUND, GUITHREADINFO, HHOOK, HWND_MESSAGE, HWND_TOPMOST, KBDLLHOOKSTRUCT,
    LLKHF_ALTDOWN, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED, LWA_ALPHA, MSG, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use super::machine::{Decision, Event, Machine};
use crate::overlay;
use crate::settings::ShortcutActivation;
use crate::TranscriptionCoordinator;

const WM_SYNC: u32 = WM_APP + 1;
const WM_SHOW_DOT: u32 = WM_APP + 2;
const WM_HIDE_DOT: u32 = WM_APP + 3;
const WM_FOCUS_REFRESH: u32 = WM_APP + 4;

const TIMER_HOLD: usize = 1;
const TIMER_PULSE: usize = 2;
const TIMER_FOCUS_DEBOUNCE: usize = 3;
const TIMER_FOCUS_POLL: usize = 4;

const FOCUS_DEBOUNCE_MS: u32 = 40;
const FOCUS_POLL_MS: u32 = 350;
const DOT_BASE_PX: i32 = 16;
/// Coral (COLORREF is 0x00BBGGRR).
const DOT_COLOR: u32 = 0x004C_5DE8;
/// `SendInput` re-enters this hook. The flag is belt-and-suspenders next to
/// `LLKHF_INJECTED`, which some stacks have been observed to clear.
const INJECT_MARKER: usize = 0x4853_5043;

static ENABLED: AtomicBool = AtomicBool::new(false);
static TEXT_FIELD_ONLY: AtomicBool = AtomicBool::new(true);
static BLOCK_FULLSCREEN: AtomicBool = AtomicBool::new(true);
static CARET_INDICATOR: AtomicBool = AtomicBool::new(true);
static THRESHOLD_MS: AtomicU64 = AtomicU64::new(350);
static EDITABLE: AtomicBool = AtomicBool::new(false);
static FULLSCREEN: AtomicBool = AtomicBool::new(false);
static EXCLUDED_NOW: AtomicBool = AtomicBool::new(false);
static CAPTURE_SUSPENDED: AtomicBool = AtomicBool::new(false);
static PIPELINE_ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL_GEN: AtomicU64 = AtomicU64::new(0);
static REHOOK: AtomicBool = AtomicBool::new(false);
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);
static FOCUS_HWND: AtomicIsize = AtomicIsize::new(0);
static DOT_X: AtomicI32 = AtomicI32::new(0);
static DOT_Y: AtomicI32 = AtomicI32::new(0);

static APP: OnceLock<AppHandle> = OnceLock::new();
static EXCLUDED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static WORK_TX: OnceLock<Sender<Job>> = OnceLock::new();
static STARTED: Once = Once::new();

thread_local! {
    static MACHINE: RefCell<Machine> = RefCell::new(Machine::new());
    static INJECTING: Cell<bool> = const { Cell::new(false) };
    static HOOK: Cell<isize> = const { Cell::new(0) };
    static INDICATOR: Cell<isize> = const { Cell::new(0) };
    static PULSE: Cell<u8> = const { Cell::new(0) };
    static AUTOMATION: RefCell<Option<IUIAutomation>> = RefCell::new(None);
}

enum Job {
    Start { cancel_gen: u64 },
    Stop,
}

enum HookResult {
    Forward,
    Swallow,
}

struct Bounds {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

struct Monitor {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

pub fn init(app: &AppHandle) {
    let _ = APP.set(app.clone());
    apply_settings(app);
    STARTED.call_once(|| {
        spawn("spacebar-worker", worker_thread);
        spawn("spacebar-focus", focus_thread);
        spawn("spacebar-hook", hook_thread);
    });
}

pub fn apply_settings(app: &AppHandle) {
    let settings = crate::settings::get_settings(app);
    ENABLED.store(settings.spacebar_dictation_enabled, Ordering::SeqCst);
    TEXT_FIELD_ONLY.store(settings.spacebar_text_field_only, Ordering::SeqCst);
    BLOCK_FULLSCREEN.store(settings.spacebar_block_fullscreen, Ordering::SeqCst);
    CARET_INDICATOR.store(settings.spacebar_caret_indicator, Ordering::SeqCst);
    THRESHOLD_MS.store(
        super::clamp_hold_threshold_ms(settings.spacebar_hold_threshold_ms),
        Ordering::SeqCst,
    );
    let mut excluded = EXCLUDED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    excluded.clear();
    excluded.extend(
        settings
            .spacebar_excluded_apps
            .iter()
            .map(|name| super::normalize_process_name(name))
            .filter(|name| !name.is_empty()),
    );
    drop(excluded);
    post_to(&HOOK_HWND, WM_SYNC);
    post_to(&FOCUS_HWND, WM_FOCUS_REFRESH);
}

pub fn set_shortcut_capture(active: bool) {
    CAPTURE_SUSPENDED.store(active, Ordering::SeqCst);
}

pub fn set_pipeline_active(active: bool) {
    PIPELINE_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn prefer_hook_front() {
    REHOOK.store(true, Ordering::SeqCst);
    post_to(&HOOK_HWND, WM_SYNC);
}

pub fn on_recording_cancelled() {
    CANCEL_GEN.fetch_add(1, Ordering::SeqCst);
    overlay::set_spacebar_overlay_suppressed(false);
    post_to(&HOOK_HWND, WM_HIDE_DOT);
}

fn spawn(name: &str, body: fn()) {
    if let Err(error) = thread::Builder::new().name(name.into()).spawn(body) {
        warn!("failed to start {name}: {error}");
    }
}

fn runtime_armed() -> bool {
    if !ENABLED.load(Ordering::Relaxed)
        || CAPTURE_SUSPENDED.load(Ordering::Relaxed)
        || PIPELINE_ACTIVE.load(Ordering::Relaxed)
    {
        return false;
    }
    if TEXT_FIELD_ONLY.load(Ordering::Relaxed) && !EDITABLE.load(Ordering::Relaxed) {
        return false;
    }
    if BLOCK_FULLSCREEN.load(Ordering::Relaxed) && FULLSCREEN.load(Ordering::Relaxed) {
        return false;
    }
    !EXCLUDED_NOW.load(Ordering::Relaxed)
}

fn post_to(slot: &AtomicIsize, msg: u32) {
    let raw = slot.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if let Err(error) = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0)) } {
        debug!("spacebar PostMessage {msg:#x} failed: {error}");
    }
}

fn enqueue(job: Job) {
    let Some(tx) = WORK_TX.get() else {
        return;
    };
    if tx.send(job).is_err() {
        warn!("spacebar worker is gone; dictation was not delivered");
    }
}

fn hook_thread() {
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        warn!("spacebar hook thread: GetModuleHandle failed");
        return;
    };
    let instance = HINSTANCE(module.0);
    register_class(
        instance,
        windows::core::w!("HandySpaceHookWindow"),
        Some(hook_wnd_proc),
    );
    register_class(
        instance,
        windows::core::w!("HandySpaceCaretDot"),
        Some(dot_wnd_proc),
    );

    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("HandySpaceHookWindow"),
            windows::core::w!("Handy Space"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }) else {
        warn!("spacebar hook thread: message window was not created");
        return;
    };
    HOOK_HWND.store(hwnd.0 as isize, Ordering::Release);

    if let Ok(dot) = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            windows::core::w!("HandySpaceCaretDot"),
            windows::core::w!(""),
            WS_POPUP,
            0,
            0,
            DOT_BASE_PX,
            DOT_BASE_PX,
            None,
            None,
            Some(instance),
            None,
        )
    } {
        INDICATOR.with(|cell| cell.set(dot.0 as isize));
    }

    sync_hook();

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        let _ = unsafe { DispatchMessageW(&msg) };
    }
}

fn register_class(
    instance: HINSTANCE,
    name: windows::core::PCWSTR,
    proc: windows::Win32::UI::WindowsAndMessaging::WNDPROC,
) {
    let class = WNDCLASSW {
        lpfnWndProc: proc,
        hInstance: instance,
        lpszClassName: name,
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&class);
    }
}

unsafe extern "system" fn hook_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_HOLD => {
            let decision = MACHINE.with(|machine| machine.borrow_mut().on_event(Event::Threshold));
            apply_decision(decision);
            LRESULT(0)
        }
        WM_SYNC => {
            sync_hook();
            LRESULT(0)
        }
        WM_SHOW_DOT => {
            show_dot(DOT_X.load(Ordering::Acquire), DOT_Y.load(Ordering::Acquire));
            LRESULT(0)
        }
        WM_HIDE_DOT => {
            hide_dot();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn dot_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_PULSE => {
            pulse(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_dot(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn sync_hook() {
    let enabled = ENABLED.load(Ordering::SeqCst);
    let idle = MACHINE.with(|machine| machine.borrow().is_idle());
    let installed = HOOK.with(|hook| hook.get() != 0);
    if (enabled || !idle) && !installed {
        install_hook();
    } else if !enabled && idle && installed {
        uninstall_hook();
    }
    if REHOOK.load(Ordering::SeqCst) && idle {
        if HOOK.with(|hook| hook.get() != 0) {
            uninstall_hook();
            install_hook();
        }
        REHOOK.store(false, Ordering::SeqCst);
    }
}

fn install_hook() {
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
        Ok(hook) => {
            HOOK.with(|slot| slot.set(hook.0 as isize));
            info!("spacebar dictation hook installed");
        }
        Err(error) => warn!("spacebar dictation hook was not installed: {error}"),
    }
}

fn uninstall_hook() {
    let raw = HOOK.with(|hook| hook.replace(0));
    if raw == 0 {
        return;
    }
    let hook = HHOOK(raw as *mut c_void);
    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
        warn!("spacebar dictation hook was not removed: {error}");
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let outcome = std::panic::catch_unwind(|| classify_key(wparam, lparam));
    match outcome {
        Ok(HookResult::Swallow) => LRESULT(1),
        Ok(HookResult::Forward) | Err(_) => CallNextHookEx(None, code, wparam, lparam),
    }
}

fn classify_key(wparam: WPARAM, lparam: LPARAM) -> HookResult {
    if INJECTING.with(|flag| flag.get()) {
        return HookResult::Forward;
    }
    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    if kb.flags.contains(LLKHF_INJECTED) || kb.flags.contains(LLKHF_LOWER_IL_INJECTED) {
        return HookResult::Forward;
    }

    let message = wparam.0 as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    if !down && !up {
        return HookResult::Forward;
    }

    let idle = MACHINE.with(|machine| machine.borrow().is_idle());
    if !ENABLED.load(Ordering::Relaxed) && idle {
        return HookResult::Forward;
    }

    let space = kb.vkCode == VK_SPACE.0 as u32;
    let event = if space && down {
        let held = MACHINE.with(|machine| machine.borrow().space_held());
        let modifiers = modifiers_down(kb);
        let armed = !held && !modifiers && runtime_armed() && !ime_composing();
        Event::SpaceDown { modifiers, armed }
    } else if space && up {
        Event::SpaceUp
    } else if down {
        Event::OtherDown
    } else {
        return HookResult::Forward;
    };

    let decision = MACHINE.with(|machine| machine.borrow_mut().on_event(event));
    let swallow = decision.swallow;
    apply_decision(decision);
    if swallow {
        HookResult::Swallow
    } else {
        HookResult::Forward
    }
}

fn modifiers_down(kb: &KBDLLHOOKSTRUCT) -> bool {
    kb.flags.contains(LLKHF_ALTDOWN)
        || virtual_down(VK_SHIFT)
        || virtual_down(VK_CONTROL)
        || virtual_down(VK_MENU)
        || virtual_down(VK_LWIN)
        || virtual_down(VK_RWIN)
}

fn virtual_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(i32::from(vk.0)) < 0 }
}

fn ime_composing() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return false;
    }
    let context = unsafe { ImmGetContext(hwnd) };
    if context.0.is_null() {
        return false;
    }
    let length = unsafe { ImmGetCompositionStringW(context, GCS_COMPSTR, None, 0) };
    let _ = unsafe { ImmReleaseContext(hwnd, context) };
    length > 0
}

fn apply_decision(decision: Decision) {
    if decision.cancel_timer {
        let hwnd = hook_window();
        if !hwnd.0.is_null() {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_HOLD) };
        }
    }
    if decision.arm_timer {
        let hwnd = hook_window();
        let ms = THRESHOLD_MS.load(Ordering::Relaxed).clamp(50, 2_000) as u32;
        if !hwnd.0.is_null() {
            unsafe {
                SetTimer(Some(hwnd), TIMER_HOLD, ms, None);
            }
        }
    }
    if decision.inject_space {
        inject_space();
    }
    if decision.start {
        info!("spacebar hold crossed the threshold; dictation starting");
        enqueue(Job::Start {
            cancel_gen: CANCEL_GEN.load(Ordering::SeqCst),
        });
    }
    if decision.stop {
        info!("spacebar released; dictation stopping");
        enqueue(Job::Stop);
    }
    if REHOOK.load(Ordering::SeqCst) && MACHINE.with(|machine| machine.borrow().is_idle()) {
        sync_hook();
    }
}

fn hook_window() -> HWND {
    HWND(HOOK_HWND.load(Ordering::Acquire) as *mut c_void)
}

fn inject_space() {
    let scan = unsafe { MapVirtualKeyW(u32::from(VK_SPACE.0), MAPVK_VK_TO_VSC) } as u16;
    let down = key_input(scan, KEYBD_EVENT_FLAGS::default());
    let up = key_input(scan, KEYEVENTF_KEYUP);
    INJECTING.with(|flag| flag.set(true));
    let sent = unsafe { SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32) };
    INJECTING.with(|flag| flag.set(false));
    if sent != 2 {
        warn!("spacebar tap injected {sent} of 2 keyboard events");
    }
}

fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_SPACE,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_MARKER,
            },
        },
    }
}

fn show_dot(x: i32, y: i32) {
    let raw = INDICATOR.with(|cell| cell.get());
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let size = (DOT_BASE_PX * dpi as i32 / 96).max(DOT_BASE_PX);
    let region = unsafe { CreateEllipticRgn(0, 0, size, size) };
    if !region.0.is_null() {
        // SetWindowRgn takes ownership of the region.
        let _ = unsafe { SetWindowRgn(hwnd, Some(region), true) };
    }
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            size,
            size,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    pulse(hwnd);
    unsafe {
        SetTimer(Some(hwnd), TIMER_PULSE, 40, None);
    }
}

fn hide_dot() {
    let raw = INDICATOR.with(|cell| cell.get());
    if raw == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_PULSE) };
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

fn pulse(hwnd: HWND) {
    let step = PULSE.with(|cell| {
        let next = cell.get().wrapping_add(1);
        cell.set(next);
        next
    });
    let wave = ((step as f32) * 0.28).sin().mul_add(0.5, 0.5);
    let alpha = (170.0 + wave * 85.0) as u8;
    let _ = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA) };
}

fn paint_dot(hwnd: HWND) {
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut paint);
        let brush = CreateSolidBrush(COLORREF(DOT_COLOR));
        if !brush.0.is_null() {
            FillRect(dc, &paint.rcPaint, brush);
            let _ = DeleteObject(brush.into());
        }
        let _ = EndPaint(hwnd, &paint);
    }
}

fn worker_thread() {
    init_sta();
    let (tx, rx) = mpsc::channel();
    let _ = WORK_TX.set(tx);
    while let Ok(job) = rx.recv() {
        match job {
            Job::Start { cancel_gen } => start_dictation(cancel_gen),
            Job::Stop => stop_dictation(),
        }
    }
}

fn start_dictation(cancel_gen: u64) {
    if cancel_gen != CANCEL_GEN.load(Ordering::SeqCst) {
        return;
    }
    let anchor = if CARET_INDICATOR.load(Ordering::Relaxed) {
        find_anchor()
    } else {
        None
    };
    if cancel_gen != CANCEL_GEN.load(Ordering::SeqCst) {
        post_to(&HOOK_HWND, WM_HIDE_DOT);
        return;
    }
    match anchor {
        Some((x, y)) => {
            overlay::set_spacebar_overlay_suppressed(true);
            DOT_X.store(x, Ordering::Release);
            DOT_Y.store(y, Ordering::Release);
            post_to(&HOOK_HWND, WM_SHOW_DOT);
        }
        None => {
            overlay::set_spacebar_overlay_suppressed(false);
            post_to(&HOOK_HWND, WM_HIDE_DOT);
        }
    }
    send_dictation(true);
    if cancel_gen != CANCEL_GEN.load(Ordering::SeqCst) {
        overlay::set_spacebar_overlay_suppressed(false);
        post_to(&HOOK_HWND, WM_HIDE_DOT);
        send_dictation(false);
    }
}

fn stop_dictation() {
    overlay::set_spacebar_overlay_suppressed(false);
    post_to(&HOOK_HWND, WM_HIDE_DOT);
    send_dictation(false);
}

fn send_dictation(pressed: bool) {
    let Some(app) = APP.get() else {
        return;
    };
    let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() else {
        warn!("spacebar dictation: transcription coordinator is not ready");
        return;
    };
    coordinator.send_input(
        "transcribe",
        "spacebar",
        pressed,
        ShortcutActivation::PushToTalk,
        Duration::ZERO,
    );
}

fn focus_thread() {
    init_sta();
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    register_class(
        instance,
        windows::core::w!("HandySpaceFocusWindow"),
        Some(focus_wnd_proc),
    );
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("HandySpaceFocusWindow"),
            windows::core::w!("Handy Space focus"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }) else {
        warn!("spacebar focus watcher window was not created");
        return;
    };
    FOCUS_HWND.store(hwnd.0 as isize, Ordering::Release);
    let _hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_OBJECT_FOCUS,
            None,
            Some(focus_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    unsafe {
        SetTimer(Some(hwnd), TIMER_FOCUS_POLL, FOCUS_POLL_MS, None);
    }
    refresh_focus();

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        let _ = unsafe { DispatchMessageW(&msg) };
    }
}

unsafe extern "system" fn focus_event(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    post_to(&FOCUS_HWND, WM_FOCUS_REFRESH);
}

unsafe extern "system" fn focus_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_FOCUS_REFRESH => {
            unsafe {
                SetTimer(Some(hwnd), TIMER_FOCUS_DEBOUNCE, FOCUS_DEBOUNCE_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_FOCUS_DEBOUNCE => {
            let _ = unsafe { KillTimer(Some(hwnd), TIMER_FOCUS_DEBOUNCE) };
            refresh_focus();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_FOCUS_POLL => {
            refresh_focus();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn refresh_focus() {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        EDITABLE.store(false, Ordering::Relaxed);
        FULLSCREEN.store(false, Ordering::Relaxed);
        EXCLUDED_NOW.store(false, Ordering::Relaxed);
        return;
    }
    let excluded = process_name(hwnd)
        .map(|name| {
            EXCLUDED
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&name)
        })
        .unwrap_or(false);
    EXCLUDED_NOW.store(excluded, Ordering::Relaxed);
    FULLSCREEN.store(window_is_fullscreen(hwnd), Ordering::Relaxed);
    EDITABLE.store(focused_is_editable(), Ordering::Relaxed);
}

fn process_name(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = vec![0u16; 1024];
    let mut length = buffer.len() as u32;
    let queried = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    queried.ok()?;
    let text = String::from_utf16_lossy(&buffer[..length as usize]);
    let name = super::normalize_process_name(&text);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn window_is_fullscreen(hwnd: HWND) -> bool {
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return false;
    }
    let mut window = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_err() {
        return false;
    }
    let Some(monitor) = monitor_of(hwnd) else {
        return false;
    };
    const TOLERANCE: i32 = 8;
    window.left <= monitor.left + TOLERANCE
        && window.top <= monitor.top + TOLERANCE
        && window.right >= monitor.right - TOLERANCE
        && window.bottom >= monitor.bottom - TOLERANCE
}

fn focused_is_editable() -> bool {
    let Some(automation) = automation() else {
        debug!("spacebar focus: UI Automation is unavailable; treating focus as not editable");
        return false;
    };
    let element = match unsafe { automation.GetFocusedElement() } {
        Ok(element) => element,
        Err(error) => {
            debug!("spacebar focus: GetFocusedElement failed: {error}");
            return false;
        }
    };
    element_is_editable(&element)
}

fn element_is_editable(element: &IUIAutomationElement) -> bool {
    let enabled = unsafe { element.CurrentIsEnabled() }
        .map(|value| value.as_bool())
        .unwrap_or(false);
    if !enabled {
        return false;
    }
    if value_is_read_only(element) == Some(true) {
        return false;
    }
    let control_type = unsafe { element.CurrentControlType() }
        .map(|value| value.0)
        .unwrap_or(0);
    if control_type == UIA_EditControlTypeId.0
        || control_type == UIA_DocumentControlTypeId.0
        || control_type == UIA_ComboBoxControlTypeId.0
    {
        return true;
    }
    value_is_read_only(element) == Some(false)
}

fn value_is_read_only(element: &IUIAutomationElement) -> Option<bool> {
    let unknown = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }.ok()?;
    let value = unknown.cast::<IUIAutomationValuePattern>().ok()?;
    Some(
        unsafe { value.CurrentIsReadOnly() }
            .map(|flag| flag.as_bool())
            .unwrap_or(true),
    )
}

fn find_anchor() -> Option<(i32, i32)> {
    let monitor = monitor_of(unsafe { GetForegroundWindow() })?;
    let element = automation()?.let_focused()?;
    if let Some(bounds) = caret_from_text_pattern(&element) {
        if caret_usable(&bounds, &monitor) {
            return Some(place_above(&bounds, &monitor));
        }
    }
    if let Some(bounds) = caret_from_gui_thread() {
        if caret_usable(&bounds, &monitor) {
            return Some(place_above(&bounds, &monitor));
        }
    }
    if element_is_editable(&element) {
        if let Some(bounds) = element_bounds(&element) {
            if !element_too_large(&bounds, &monitor) {
                return Some(place_in_box(&bounds, &monitor));
            }
        }
    }
    None
}

trait Focused {
    fn let_focused(&self) -> Option<IUIAutomationElement>;
}

impl Focused for IUIAutomation {
    fn let_focused(&self) -> Option<IUIAutomationElement> {
        unsafe { self.GetFocusedElement() }.ok()
    }
}

fn caret_from_text_pattern(element: &IUIAutomationElement) -> Option<Bounds> {
    let unknown = unsafe { element.GetCurrentPattern(UIA_TextPattern2Id) }.ok()?;
    let pattern = unknown.cast::<IUIAutomationTextPattern2>().ok()?;
    let mut active = BOOL(0);
    let range: IUIAutomationTextRange = unsafe { pattern.GetCaretRange(&mut active) }.ok()?;
    let array = unsafe { range.GetBoundingRectangles() }.ok()?;
    first_rectangle(array)
}

fn first_rectangle(array: *mut SAFEARRAY) -> Option<Bounds> {
    if array.is_null() {
        return None;
    }
    let bounds = read_first_rectangle(array);
    let _ = unsafe { SafeArrayDestroy(array) };
    bounds
}

/// UI Automation returns caret rectangles as a `VT_R8` array of
/// left, top, width, height.
fn read_first_rectangle(array: *mut SAFEARRAY) -> Option<Bounds> {
    let array = array as *const SAFEARRAY;
    let lower = unsafe { SafeArrayGetLBound(array, 1) }.ok()?;
    let upper = unsafe { SafeArrayGetUBound(array, 1) }.ok()?;
    let count = (upper - lower + 1).max(0) as usize;
    if count < 4 {
        return None;
    }
    let mut data = std::ptr::null_mut();
    unsafe { SafeArrayAccessData(array, &mut data) }.ok()?;
    if data.is_null() {
        let _ = unsafe { SafeArrayUnaccessData(array) };
        return None;
    }
    let bounds = unsafe {
        let values = std::slice::from_raw_parts(data.cast::<f64>(), count);
        Bounds {
            left: values[0] as i32,
            top: values[1] as i32,
            width: values[2] as i32,
            height: values[3] as i32,
        }
    };
    let _ = unsafe { SafeArrayUnaccessData(array) };
    Some(bounds)
}

fn caret_from_gui_thread() -> Option<Bounds> {
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(0, &mut info) }.ok()?;
    if info.hwndCaret.0.is_null() {
        return None;
    }
    let mut origin = POINT {
        x: info.rcCaret.left,
        y: info.rcCaret.top,
    };
    let mut opposite = POINT {
        x: info.rcCaret.right,
        y: info.rcCaret.bottom,
    };
    unsafe {
        if !ClientToScreen(info.hwndCaret, &mut origin).as_bool()
            || !ClientToScreen(info.hwndCaret, &mut opposite).as_bool()
        {
            return None;
        }
    }
    let width = (opposite.x - origin.x).max(1);
    let height = opposite.y - origin.y;
    if height < 1 {
        return None;
    }
    Some(Bounds {
        left: origin.x,
        top: origin.y,
        width,
        height,
    })
}

fn element_bounds(element: &IUIAutomationElement) -> Option<Bounds> {
    let rect = unsafe { element.CurrentBoundingRectangle() }.ok()?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < 1 || height < 1 {
        return None;
    }
    Some(Bounds {
        left: rect.left,
        top: rect.top,
        width,
        height,
    })
}

fn caret_usable(bounds: &Bounds, monitor: &Monitor) -> bool {
    if bounds.height < 1 {
        return false;
    }
    let width = bounds.width.max(1);
    let monitor_width = (monitor.right - monitor.left).max(1);
    let monitor_height = (monitor.bottom - monitor.top).max(1);
    if width > monitor_width / 2 {
        return false;
    }
    !(width > monitor_width * 3 / 4 && bounds.height > monitor_height / 2)
}

fn element_too_large(bounds: &Bounds, monitor: &Monitor) -> bool {
    let monitor_width = (monitor.right - monitor.left).max(1);
    let monitor_height = (monitor.bottom - monitor.top).max(1);
    bounds.width > monitor_width * 3 / 4 && bounds.height > monitor_height / 2
}

fn place_above(bounds: &Bounds, monitor: &Monitor) -> (i32, i32) {
    let size = dot_size();
    let x = bounds.left + bounds.width.max(1) / 2 - size / 2;
    let mut y = bounds.top - size - 2;
    if y < monitor.top {
        y = bounds.top + bounds.height + 2;
    }
    clamp_point(x, y, size, monitor)
}

fn place_in_box(bounds: &Bounds, monitor: &Monitor) -> (i32, i32) {
    let size = dot_size();
    let x = bounds.left + 4;
    let y = bounds.top + bounds.height / 2 - size / 2;
    clamp_point(x, y, size, monitor)
}

fn clamp_point(x: i32, y: i32, size: i32, monitor: &Monitor) -> (i32, i32) {
    let x = x.clamp(monitor.left, (monitor.right - size).max(monitor.left));
    let y = y.clamp(monitor.top, (monitor.bottom - size).max(monitor.top));
    (x, y)
}

fn dot_size() -> i32 {
    let hwnd = unsafe { GetForegroundWindow() };
    let dpi = if hwnd.0.is_null() {
        96
    } else {
        unsafe { GetDpiForWindow(hwnd) }.max(96)
    };
    (DOT_BASE_PX * dpi as i32 / 96).max(DOT_BASE_PX)
}

fn monitor_of(hwnd: HWND) -> Option<Monitor> {
    if hwnd.0.is_null() {
        return None;
    }
    let handle = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(handle, &mut info) }.as_bool() {
        return None;
    }
    Some(Monitor {
        left: info.rcMonitor.left,
        top: info.rcMonitor.top,
        right: info.rcMonitor.right,
        bottom: info.rcMonitor.bottom,
    })
}

fn automation() -> Option<IUIAutomation> {
    AUTOMATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok();
        }
        slot.clone()
    })
}

fn init_sta() {
    // `CoInitializeEx` returns an HRESULT, including S_FALSE when this thread
    // is already in the STA. Only a failure code means UIA will not work.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        debug!("spacebar CoInitializeEx failed: {:#x}", hr.0 as u32);
    }
}
