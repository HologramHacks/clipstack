// ClipStack for macOS, experimental v0 skeleton.
//
// Compile-verified on CI only; this code has never run on real Mac hardware.
// v0 scope, mirroring the smallest useful slice of the Windows build:
// * Menu bar status item ("CS" text title) with a Quit menu.
// * NSPasteboard polled every 500ms via changeCount; text clips only,
//   most-recent-first, deduped to the front, capped at MAX_HISTORY.
// * A borderless non-activating NSPanel popup listing history rows,
//   toggled by a global Cmd+Shift+V hotkey (Carbon RegisterEventHotKey)
//   at the mouse location.
// * Click a row or press Enter: set the pasteboard, hide the panel, and
//   synthesize Cmd+V into the previously focused app via CGEvent.
// * Esc hides; Up/Down move the selection.
//
// ponytail: no pins, no images, no persistence, no themes, no scrolling in
// v0; win.rs holds the full model to port piece by piece once this runs on
// hardware.
//
// Threading mirrors win.rs: everything runs on the main thread (AppKit
// requires it), so the global state lives in a RefCell that is only ever
// touched there. Borrows are short and never held across an AppKit call
// that could re-enter our handlers.

use std::cell::{RefCell, RefMut};
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSMenu,
    NSMenuItem, NSPanel, NSPasteboard, NSPasteboardTypeString, NSStatusBar, NSStatusItem,
    NSTextField, NSVariableStatusItemLength, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

const MAX_HISTORY: usize = 50;
const VISIBLE_ROWS: usize = 20; // ponytail: rows past this are unreachable; add scrolling later
const ROW_H: f64 = 24.0;
const WIDTH: f64 = 460.0;
const PAD: f64 = 6.0;

// macOS virtual key codes (kVK_*).
const KEY_RETURN: u16 = 36;
const KEY_ESCAPE: u16 = 53;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;
const KVK_ANSI_V: u16 = 9;

// ---- Global state ---------------------------------------------------------

struct Mac {
    history: Vec<String>, // most recent first
    sel: usize,           // selected row index into history
    change_count: isize,  // last NSPasteboard.changeCount we ingested
    panel: Retained<ClipPanel>,
    rows: Vec<Retained<NSTextField>>, // one label per visible history row
    _status: Retained<NSStatusItem>,  // kept alive for the app lifetime
}

struct Global(RefCell<Option<Mac>>);
// SAFETY: AppKit is main-thread-only and run() pins everything there, so this
// is never actually shared across threads; the RefCell turns any re-entrant
// aliasing slip into a clean panic instead of UB (same pattern as win.rs).
unsafe impl Sync for Global {}
static G: Global = Global(RefCell::new(None));

fn app() -> RefMut<'static, Mac> {
    RefMut::map(G.0.borrow_mut(), |o| o.as_mut().expect("Mac app not initialized"))
}

// ---- Panel (popup window) -------------------------------------------------

define_class!(
    // SAFETY: NSPanel has no special subclassing requirements for these
    // overrides, and ClipPanel implements no Drop and has no ivars.
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "ClipStackPanel"]
    struct ClipPanel;

    impl ClipPanel {
        // Borderless windows refuse key status by default; we need it for
        // Esc/Enter/arrow handling. The panel is non-activating, so taking
        // key status still leaves the previous app active for the paste.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            on_key(event.keyCode());
        }

        // Rows are plain non-editable labels, so clicks fall through the
        // responder chain to the window. ponytail: real per-row views with
        // hover states later.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            on_click(event.locationInWindow());
        }

        // The panel doubles as the pasteboard-poll timer target: it is the
        // one NSObject subclass we already have.
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            poll_pasteboard();
        }
    }
);

fn make_panel(mtm: MainThreadMarker) -> Retained<ClipPanel> {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, ROW_H + PAD * 2.0));
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let this = ClipPanel::alloc(mtm).set_ivars(());
    let panel: Retained<ClipPanel> = unsafe {
        msg_send![
            super(this),
            initWithContentRect: rect,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    panel.setFloatingPanel(true);
    panel
}

// ---- History --------------------------------------------------------------

/// Insert a captured clip at the front, deduping an existing copy to the
/// front instead (same behavior as add_text in win.rs).
fn push_history(history: &mut Vec<String>, t: String) {
    if t.is_empty() {
        return;
    }
    if let Some(pos) = history.iter().position(|c| c == &t) {
        let c = history.remove(pos);
        history.insert(0, c);
        return;
    }
    history.insert(0, t);
    history.truncate(MAX_HISTORY);
}

/// Collapse a clip into a single-line row preview (same idea as win.rs
/// make_preview, minus the UTF-16).
fn preview(s: &str) -> String {
    let mut out = String::new();
    let mut prev_space = true; // leading whitespace collapses away
    for ch in s.chars() {
        let c = if ch.is_whitespace() { ' ' } else { ch };
        if c == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
            out.push(' ');
        } else if !c.is_control() {
            prev_space = false;
            out.push(c);
        }
        if out.chars().count() >= 120 {
            break;
        }
    }
    out.trim_end().to_string()
}

fn poll_pasteboard() {
    let pb = NSPasteboard::generalPasteboard();
    let count = pb.changeCount();
    let mut a = app();
    if count == a.change_count {
        return;
    }
    a.change_count = count;
    let Some(s) = pb.stringForType(unsafe { NSPasteboardTypeString }) else {
        return;
    };
    push_history(&mut a.history, s.to_string());
}

// ---- Popup show/hide ------------------------------------------------------

/// Rebuild the row labels to match the current history and size the panel.
fn rebuild_rows(mtm: MainThreadMarker) {
    let mut a = app();
    for r in a.rows.drain(..) {
        r.removeFromSuperview();
    }
    let shown = a.history.len().min(VISIBLE_ROWS);
    let n = shown.max(1); // an empty history still shows one placeholder row
    let height = PAD * 2.0 + n as f64 * ROW_H;
    a.panel
        .setFrame_display(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height)), false);
    let content = a.panel.contentView().expect("panel has a content view");
    let make_row = |i: usize, text: &str| {
        let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        // Content view coordinates are bottom-left origin: row i counts from the top.
        label.setFrame(NSRect::new(
            NSPoint::new(PAD, height - PAD - (i + 1) as f64 * ROW_H),
            NSSize::new(WIDTH - PAD * 2.0, ROW_H),
        ));
        label.setBackgroundColor(Some(&NSColor::selectedContentBackgroundColor()));
        label.setDrawsBackground(false);
        content.addSubview(&label);
        label
    };
    if shown == 0 {
        let label = make_row(0, "(clipboard history is empty)");
        a.rows.push(label);
    } else {
        for i in 0..shown {
            let text = preview(&a.history[i]);
            let label = make_row(i, &text);
            a.rows.push(label);
        }
    }
    a.sel = 0;
    drop(a);
    update_selection();
}

/// Highlight the selected row (background on/off; the color is preset).
fn update_selection() {
    let a = app();
    if a.history.is_empty() {
        return;
    }
    for (i, r) in a.rows.iter().enumerate() {
        r.setDrawsBackground(i == a.sel);
    }
}

fn show_popup(mtm: MainThreadMarker) {
    rebuild_rows(mtm);
    let a = app();
    // ponytail: no work-area clamping in v0 (win.rs clamps to the monitor);
    // a popup near the screen edge can hang off it.
    a.panel.setFrameTopLeftPoint(NSEvent::mouseLocation());
    a.panel.makeKeyAndOrderFront(None);
}

fn hide_popup() {
    app().panel.orderOut(None);
}

fn toggle_popup() {
    let Some(mtm) = MainThreadMarker::new() else {
        return; // Carbon dispatches hotkeys on the main run loop; this never fires elsewhere
    };
    let visible = app().panel.isVisible();
    if visible {
        hide_popup();
    } else {
        show_popup(mtm);
    }
}

// ---- Input handling -------------------------------------------------------

fn on_key(code: u16) {
    match code {
        KEY_ESCAPE => hide_popup(),
        KEY_RETURN => paste_selected(),
        KEY_DOWN => {
            let mut a = app();
            let last = a.history.len().min(VISIBLE_ROWS).saturating_sub(1);
            a.sel = (a.sel + 1).min(last);
            drop(a);
            update_selection();
        }
        KEY_UP => {
            let mut a = app();
            a.sel = a.sel.saturating_sub(1);
            drop(a);
            update_selection();
        }
        _ => {} // swallow silently; calling super would beep
    }
}

fn on_click(p: NSPoint) {
    let mut a = app();
    let height = a.panel.frame().size.height;
    let row = ((height - PAD - p.y) / ROW_H).floor();
    if row < 0.0 {
        return;
    }
    let row = row as usize;
    if row >= a.history.len().min(VISIBLE_ROWS) {
        return;
    }
    a.sel = row;
    drop(a);
    update_selection();
    paste_selected();
}

/// Put the selected clip on the pasteboard, hide the popup, and paste it
/// into whatever app had focus (the panel never activated us, so focus is
/// still theirs).
fn paste_selected() {
    let text = {
        let a = app();
        match a.history.get(a.sel) {
            Some(t) => t.clone(),
            None => return,
        }
    };
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
    // Don't re-ingest our own write on the next poll (mirrors last_seq in win.rs).
    app().change_count = pb.changeCount();
    hide_popup();
    // ponytail: no delay before the synthetic Cmd+V; if focus hand-back is
    // too slow on real hardware, add a short dispatch_after here.
    synthesize_cmd_v();
}

// ---- Cmd+V synthesis (CoreGraphics C API) ---------------------------------
// Raw FFI instead of another crate: three functions is all we need.
// ponytail: requires the Accessibility permission on real hardware; v0 has
// no prompt or fallback if it is missing (the events are silently dropped).

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *const c_void);
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

const FLAG_CMD: u64 = 1 << 20; // kCGEventFlagMaskCommand
const HID_TAP: u32 = 0; // kCGHIDEventTap

fn synthesize_cmd_v() {
    unsafe {
        for down in [true, false] {
            let e = CGEventCreateKeyboardEvent(null(), KVK_ANSI_V, down);
            if e.is_null() {
                continue;
            }
            CGEventSetFlags(e, FLAG_CMD);
            CGEventPost(HID_TAP, e);
            CFRelease(e);
        }
    }
}

// ---- Global hotkey (Carbon) -----------------------------------------------
// RegisterEventHotKey is the one Carbon API with no AppKit replacement that
// works without the Input Monitoring permission. The handler runs on the
// main run loop, so it can touch the global state directly.

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_handler: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
}

extern "C" fn hotkey_handler(_call: *mut c_void, _event: *mut c_void, _user: *mut c_void) -> i32 {
    toggle_popup();
    0 // noErr
}

/// Register Cmd+Shift+V. ponytail: fixed combo, no configurable trigger yet
/// (win.rs has the full Trigger model to port).
fn register_hotkey() {
    const KEYB: u32 = u32::from_be_bytes(*b"keyb"); // kEventClassKeyboard
    const HOTKEY_PRESSED: u32 = 5; // kEventHotKeyPressed
    const CMD: u32 = 0x100; // cmdKey
    const SHIFT: u32 = 0x200; // shiftKey
    unsafe {
        let target = GetApplicationEventTarget();
        let spec = EventTypeSpec { event_class: KEYB, event_kind: HOTKEY_PRESSED };
        let mut handler = null_mut();
        InstallEventHandler(target, hotkey_handler, 1, &spec, null_mut(), &mut handler);
        let hk_id = EventHotKeyID { signature: u32::from_be_bytes(*b"clip"), id: 1 };
        let mut hk = null_mut();
        RegisterEventHotKey(KVK_ANSI_V as u32, CMD | SHIFT, hk_id, target, 0, &mut hk);
    }
}

// ---- Entry point ----------------------------------------------------------

pub fn run() {
    let mtm = MainThreadMarker::new().expect("ClipStack must run on the main thread");
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let panel = make_panel(mtm);

    // Menu bar item. ponytail: plain "CS" text title; a template image and
    // the pause/clear/settings entries from the Windows tray come later.
    let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status.button(mtm) {
        button.setTitle(&NSString::from_str("CS"));
    }
    let menu = NSMenu::new(mtm);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit ClipStack"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    menu.addItem(&quit);
    status.setMenu(Some(&menu));

    *G.0.borrow_mut() = Some(Mac {
        history: Vec::new(),
        sel: 0,
        change_count: -1, // ingest whatever is on the pasteboard at startup
        panel: panel.clone(),
        rows: Vec::new(),
        _status: status,
    });

    // Pasteboard poll every 500ms, the same cadence as the Windows build.
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.5,
            &panel,
            sel!(tick:),
            None,
            true,
        )
    };
    register_hotkey();
    ns_app.run();
}

// ---- Tests (pure logic only; run on the macOS CI job) ---------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_history_dedupes_to_front_and_caps() {
        let mut h = Vec::new();
        push_history(&mut h, "a".into());
        push_history(&mut h, "b".into());
        push_history(&mut h, "a".into()); // dup moves to front, no growth
        assert_eq!(h, vec!["a".to_string(), "b".to_string()]);
        push_history(&mut h, String::new()); // empty is ignored
        assert_eq!(h.len(), 2);
        for i in 0..2 * MAX_HISTORY {
            push_history(&mut h, format!("clip {i}"));
        }
        assert_eq!(h.len(), MAX_HISTORY);
        assert_eq!(h[0], format!("clip {}", 2 * MAX_HISTORY - 1)); // newest first
    }

    #[test]
    fn preview_collapses_whitespace_and_caps() {
        assert_eq!(preview("a\n\n\nb"), "a b");
        assert_eq!(preview("  hello   world  "), "hello world");
        assert!(preview(&"x".repeat(500)).chars().count() <= 120);
    }
}
