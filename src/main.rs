// ClipStack, a tiny clipboard-history popup for Windows.
//
// * Keeps the last MAX_HISTORY text/image clips; the popup shows VISIBLE at a
//   time and the mouse wheel scrolls the rest.
// * Plain middle-click pops a small, no-activate list near the cursor.
// * Left-click an item: copies it AND pastes into whatever field had focus.
// * Right-click a history item: pin it (with a label) to the persistent
//   bottom section; right-click a pin: unpin it.
// * Pinned secrets are masked on screen and stored DPAPI-encrypted on disk.
// * Tray icon: pause middle-click capture, clear history, quit.
//
// Everything runs single-threaded on the UI message loop. The mouse-hook
// callback and the window procedure both execute on this one thread, so the
// global `App` is only ever touched there. We take a `&mut App` for one handler
// and never hold it across a Win32 call that pumps messages (TrackPopupMenu,
// SetForegroundWindow), borrows are scoped accordingly.
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![allow(non_snake_case)]

use std::cell::{RefCell, RefMut};
use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    GlobalFree, LocalFree, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, FrameRect, GetMonitorInfoW, GetTextExtentPoint32W, InvalidateRect,
    MonitorFromPoint, SelectObject, SetBkMode, SetBrushOrgEx, SetStretchBltMode, SetTextColor,
    SetWindowRgn, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FW_NORMAL, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, SRCCOPY, STRETCH_HALFTONE, TRANSPARENT,
};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptProtectMemory, CryptUnprotectData, CryptUnprotectMemory,
    CRYPTPROTECTMEMORY_SAME_PROCESS, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, VirtualLock, VirtualUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyNameTextW, MapVirtualKeyW, RegisterHotKey, SendInput, SetFocus,
    TrackMouseEvent, UnregisterHotKey, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, TME_LEAVE,
    TRACKMOUSEEVENT, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const MAX_HISTORY: usize = 50;
const MAX_PINS: usize = 99; // generous cap; the pin block scrolls past PIN_VISIBLE
const VISIBLE: usize = 20; // history rows shown before it scrolls
const PIN_VISIBLE: usize = 8; // pin rows shown before the pin block scrolls
const SCROLL_STEP: usize = 3;

// Custom window messages.
const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SHOW: u32 = WM_APP + 2; // wparam=x, lparam=y
const WM_APP_HIDE: u32 = WM_APP + 3;
const WM_APP_SCROLL: u32 = WM_APP + 4; // wparam: 1=up, 2=down
const WM_APP_CAPTURED: u32 = WM_APP + 5; // wparam=encoded trigger (0=cancel)
const WM_APP_AUTOCOPY: u32 = WM_APP + 6; // a drag-release: copy the selection

// Tray menu command ids.
const ID_PAUSE: usize = 101;
const ID_CLEAR: usize = 102;
const ID_QUIT: usize = 103;
const ID_STARTUP: usize = 104;
const ID_PERSIST: usize = 105;
const ID_ABOUT: usize = 106;
const ID_AUTOCOPY: usize = 107;
const ID_THEME_BASE: usize = 300; // themes occupy ID_THEME_BASE .. ID_THEME_BASE + THEMES.len()

// HKCU Run-key entry for the optional "launch at startup" toggle.
const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "ClipStack";

// Trigger submenu command ids (kept contiguous for CheckMenuRadioItem).
const ID_TRIG_MIDDLE: usize = 201;
const ID_TRIG_MOUSE4: usize = 202;
const ID_TRIG_MOUSE5: usize = 203;
const ID_TRIG_HOTKEY: usize = 204; // the Ctrl+Shift+V preset
const ID_TRIG_CUSTOM: usize = 205; // "Set custom trigger…" / current custom

/// RegisterHotKey id for whichever keyboard combo is the active trigger.
const ID_HOTKEY: i32 = 1;

// Our own modifier bitmask (independent of the Win32 MOD_* flags).
const M_CTRL: u8 = 1;
const M_SHIFT: u8 = 2;
const M_ALT: u8 = 4;
const M_WIN: u8 = 8;

const TIMER_CLIP: usize = 1;
const HC_ACTION: i32 = 0;

/// App icon (.ico with several sizes), baked into the exe and turned into an
/// HICON at runtime so the build needs no resource compiler.
const ICON_BYTES: &[u8] = include_bytes!("../assets/clipstack.ico");

// ---- Data model -----------------------------------------------------------

struct ImageClip {
    w: usize,
    h: usize,
    rgba: Vec<u8>, // full-resolution RGBA, for pasting back
    tw: i32,       // thumbnail width
    th: i32,       // thumbnail height
    thumb: Vec<u8>, // top-down 32bpp BGRA thumbnail, for drawing
    hash: u64,
}

#[derive(Clone)]
enum Clip {
    Text(String),
    Image(Rc<ImageClip>),
}

struct Pin {
    label: String,
    secret_enc: Vec<u8>, // CryptProtectMemory-encrypted in RAM, pages VirtualLock'd
    len: usize,          // original plaintext byte length (the rest is padding)
}

/// In-progress inline pin labeling. While this is `Some`, the right-clicked
/// history row renders as a text field and the popup briefly holds keyboard
/// focus so it can receive WM_CHAR.
struct Edit {
    hist: usize,           // history index being labeled (unused when renaming)
    secret: String,        // the clip text that will become the pin's secret
    label: Vec<u16>,       // label typed so far (UTF-16, no NUL)
    restore: HWND,         // foreground window to hand focus back to when done
    rename: Option<usize>, // Some(j) = renaming existing pin j; None = new pin from history
}

/// A non-typing mouse button usable as a trigger.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Btn {
    Middle,
    X1, // "back"
    X2, // "forward"
}

/// How the user opens the popup. Exactly one is active; persisted to
/// settings.txt and applied live from the tray. Either a mouse button or a
/// keyboard key, each with an optional modifier mask (`M_*`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Trigger {
    Mouse { btn: Btn, mods: u8 },
    Key { vk: u32, mods: u8 },
}

impl Trigger {
    const MIDDLE: Trigger = Trigger::Mouse { btn: Btn::Middle, mods: 0 };
    const MOUSE4: Trigger = Trigger::Mouse { btn: Btn::X1, mods: 0 };
    const MOUSE5: Trigger = Trigger::Mouse { btn: Btn::X2, mods: 0 };
    const HOTKEY: Trigger = Trigger::Key { vk: 0x56, mods: M_CTRL | M_SHIFT }; // Ctrl+Shift+V

    fn is_key(self) -> bool {
        matches!(self, Trigger::Key { .. })
    }

    /// Tray command id if this exactly matches a preset, else the custom slot.
    fn menu_id(self) -> usize {
        PRESETS
            .iter()
            .find(|&&(_, t, _)| t == self)
            .map(|&(id, _, _)| id)
            .unwrap_or(ID_TRIG_CUSTOM)
    }

    /// Human-readable label, e.g. "Ctrl+Shift+V" or "Ctrl + Mouse 4".
    fn describe(self) -> String {
        match self {
            Trigger::Mouse { btn, mods } => {
                let name = match btn {
                    Btn::Middle => "Middle click",
                    Btn::X1 => "Mouse 4 (back)",
                    Btn::X2 => "Mouse 5 (forward)",
                };
                format!("{}{}", mods_prefix(mods), name)
            }
            Trigger::Key { vk, mods } => format!("{}{}", mods_prefix(mods), vk_name(vk)),
        }
    }
}

/// The trigger presets, defined once and used by the tray submenu, the radio
/// check, and the command handler, so they can't drift out of sync.
const PRESETS: [(usize, Trigger, &str); 4] = [
    (ID_TRIG_MIDDLE, Trigger::MIDDLE, "Middle click"),
    (ID_TRIG_MOUSE4, Trigger::MOUSE4, "Mouse 4 (back)"),
    (ID_TRIG_MOUSE5, Trigger::MOUSE5, "Mouse 5 (forward)"),
    (ID_TRIG_HOTKEY, Trigger::HOTKEY, "Ctrl + Shift + V (keyboard)"),
];

#[derive(Clone, Copy)]
enum RowKind {
    Sep,
    Hist(usize),
    Pin(usize),
}

struct VRow {
    kind: RowKind,
    top: i32,
    bottom: i32,
}

struct App {
    hwnd: HWND,
    hinst: windows_sys::Win32::Foundation::HINSTANCE,
    hook: isize, // WH_MOUSE_LL handle, or 0 when not installed
    history: Vec<Clip>,
    pins: Vec<Pin>,
    last_seq: u32,
    poll_misses: u8, // consecutive polls where a changed clipboard read back empty
    rows: Vec<VRow>,
    scroll: usize,
    pin_scroll: usize,
    auto_copy: bool,             // opt-in: copy a highlighted selection on drag-release
    drag_start: Option<POINT>,   // left-button-down point, for the drag heuristic
    theme_idx: usize,            // index into THEMES
    arm_delete: i32,             // pin row index whose delete is armed (one-click confirm), or -1
    target: HWND,
    paused: bool,
    visible: bool,
    trigger: Trigger,
    hotkey_active: bool,  // a keyboard trigger is currently registered
    capturing: bool,      // listening for a custom trigger
    about: bool,          // showing the About panel
    toast: Option<String>, // transient inline message at the bottom of the popup
    toast_ticks: u32,      // 500ms ticks left before the toast auto-clears
    persist: bool,        // remember history across restarts (opt-in)
    history_dirty: bool,  // history changed since the last flush to disk
    edit: Option<Edit>,  // inline pin-labeling in progress
    caret_on: bool,      // caret blink phase while editing/capturing
    swallow_up: Option<u32>, // button-up message to swallow after a trigger
    hovered: i32,            // index into `rows`, or -1
    tracking_leave: bool,
    font: HFONT,
    item_h: i32,
    sep_h: i32,
    pad: i32,
    width: i32,
    popup_x: i32,
    popup_y: i32,
}

struct Global(RefCell<Option<App>>);
// SAFETY: ClipStack is single-threaded, the message loop, the mouse hook, and
// the window procedure all run on the one UI thread, so `G` is never actually
// shared across threads. The RefCell enforces the single-borrow rule at runtime,
// turning any reentrant aliasing slip into a clean panic instead of UB.
unsafe impl Sync for Global {}
static G: Global = Global(RefCell::new(None));

/// Exclusive access to the single global App. Borrows are kept short and never
/// held across a Win32 call that pumps a message we handle, so reentrancy never
/// aliases.
fn app() -> RefMut<'static, App> {
    RefMut::map(G.0.borrow_mut(), |o| o.as_mut().expect("App not initialized"))
}

/// Like `app()` but yields `None` instead of panicking when a borrow is already
/// held. The low-level mouse hook uses this: Windows can invoke an LL hook
/// re-entrantly while we are mid-mutation (e.g. during `SetWindowPos` showing
/// the popup with a mouse move queued), and there we must skip the event rather
/// than double-borrow and abort the whole process.
fn try_app() -> Option<RefMut<'static, App>> {
    G.0.try_borrow_mut()
        .ok()
        .map(|g| RefMut::map(g, |o| o.as_mut().expect("App not initialized")))
}

// ---- Small helpers --------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_no_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// Dark command-palette palette.
/// A full color palette. Swapping the active one repaints the whole UI.
#[derive(Clone, Copy)]
struct Theme {
    bg: COLORREF,         // window background
    text: COLORREF,       // primary text
    dim: COLORREF,        // secondary text (image dims, placeholder)
    hover_bg: COLORREF,   // hovered row tint
    accent: COLORREF,     // brand accent (pins, arrows, links, divider, frame)
    sep: COLORREF,        // separators
    pin_bullet: COLORREF, // masked pin bullets
    delete: COLORREF,     // hovered-row delete glyph
    field_bg: COLORREF,   // inline label input background
    strong: COLORREF,     // emphasized text on a hovered/active row
}

const THEME_DARK: Theme = Theme {
    bg: rgb(0x1c, 0x1f, 0x26),
    text: rgb(0xe8, 0xe8, 0xea),
    dim: rgb(0x8a, 0x8f, 0x99),
    hover_bg: rgb(0x26, 0x2b, 0x34),
    accent: rgb(0x40, 0xcc, 0x7a),
    sep: rgb(0x2e, 0x33, 0x3d),
    pin_bullet: rgb(0x6b, 0x72, 0x80),
    delete: rgb(0xff, 0x6b, 0x6b),
    field_bg: rgb(0x13, 0x16, 0x1b),
    strong: rgb(0xff, 0xff, 0xff),
};
const THEME_GREY: Theme = Theme {
    bg: rgb(0xc4, 0xc8, 0xcd),
    text: rgb(0x1a, 0x1c, 0x20),
    dim: rgb(0x55, 0x5a, 0x62),
    hover_bg: rgb(0xb0, 0xb5, 0xbb),
    accent: rgb(0x1f, 0x9e, 0x55),
    sep: rgb(0x9a, 0x9f, 0xa6),
    pin_bullet: rgb(0x6b, 0x72, 0x80),
    delete: rgb(0xcf, 0x32, 0x32),
    field_bg: rgb(0xda, 0xdd, 0xe1),
    strong: rgb(0x00, 0x00, 0x00),
};
const THEME_LIGHT: Theme = Theme {
    bg: rgb(0xf5, 0xf6, 0xf8),
    text: rgb(0x1a, 0x1c, 0x22),
    dim: rgb(0x6a, 0x70, 0x78),
    hover_bg: rgb(0xe6, 0xe8, 0xec),
    accent: rgb(0x1f, 0x9e, 0x55),
    sep: rgb(0xd0, 0xd3, 0xd8),
    pin_bullet: rgb(0x9a, 0x9f, 0xa6),
    delete: rgb(0xcf, 0x32, 0x32),
    field_bg: rgb(0xff, 0xff, 0xff),
    strong: rgb(0x00, 0x00, 0x00),
};
const THEME_NORD: Theme = Theme {
    bg: rgb(0x2e, 0x34, 0x40),
    text: rgb(0xd8, 0xde, 0xe9),
    dim: rgb(0x7a, 0x84, 0x99),
    hover_bg: rgb(0x3b, 0x42, 0x52),
    accent: rgb(0x88, 0xc0, 0xd0),
    sep: rgb(0x43, 0x4c, 0x5e),
    pin_bullet: rgb(0x61, 0x6e, 0x88),
    delete: rgb(0xbf, 0x61, 0x6a),
    field_bg: rgb(0x29, 0x2e, 0x39),
    strong: rgb(0xec, 0xef, 0xf4),
};
const THEME_SOLARIZED: Theme = Theme {
    bg: rgb(0xfd, 0xf6, 0xe3),
    text: rgb(0x07, 0x36, 0x42),
    dim: rgb(0x65, 0x7b, 0x83),
    hover_bg: rgb(0xee, 0xe8, 0xd5),
    accent: rgb(0x85, 0x99, 0x00),
    sep: rgb(0xd9, 0xd2, 0xc1),
    pin_bullet: rgb(0x93, 0xa1, 0xa1),
    delete: rgb(0xdc, 0x32, 0x2f),
    field_bg: rgb(0xff, 0xfb, 0xf0),
    strong: rgb(0x00, 0x2b, 0x36),
};
const THEME_CONTRAST: Theme = Theme {
    bg: rgb(0x00, 0x00, 0x00),
    text: rgb(0xff, 0xff, 0xff),
    dim: rgb(0xb0, 0xb0, 0xb0),
    hover_bg: rgb(0x26, 0x26, 0x26),
    accent: rgb(0x3b, 0xe6, 0x84),
    sep: rgb(0x60, 0x60, 0x60),
    pin_bullet: rgb(0x90, 0x90, 0x90),
    delete: rgb(0xff, 0x55, 0x55),
    field_bg: rgb(0x10, 0x10, 0x10),
    strong: rgb(0xff, 0xff, 0xff),
};

/// Selectable themes in tray-menu order; index 0 is the default.
const THEMES: [(&str, Theme); 6] = [
    ("Dark", THEME_DARK),
    ("Grey", THEME_GREY),
    ("Light", THEME_LIGHT),
    ("Nord", THEME_NORD),
    ("Solarized", THEME_SOLARIZED),
    ("High Contrast", THEME_CONTRAST),
];

struct ThemeCell(std::cell::Cell<Theme>);
// SAFETY: single-threaded UI thread only, never shared across threads (like the App global).
unsafe impl Sync for ThemeCell {}
static THEME: ThemeCell = ThemeCell(std::cell::Cell::new(THEME_DARK));

/// The active palette. Cheap Copy, so call sites read it inline.
fn theme() -> Theme {
    THEME.0.get()
}
fn set_theme(idx: usize) {
    THEME.0.set(THEMES.get(idx).map_or(THEME_DARK, |t| t.1));
}
const CORNER_RADIUS: i32 = 12; // rounded-corner diameter for the window region

fn lo_hi(lp: LPARAM) -> (i32, i32) {
    let v = lp as u32;
    ((v & 0xffff) as i16 as i32, ((v >> 16) & 0xffff) as i16 as i32)
}

// ---- Trigger helpers ------------------------------------------------------

/// Which modifier keys are physically held right now (`M_*` bitmask).
fn cur_mods() -> u8 {
    let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
    let mut m = 0;
    if down(VK_CONTROL) {
        m |= M_CTRL;
    }
    if down(VK_SHIFT) {
        m |= M_SHIFT;
    }
    if down(VK_MENU) {
        m |= M_ALT;
    }
    if down(VK_LWIN) || down(VK_RWIN) {
        m |= M_WIN;
    }
    m
}

/// Translate our `M_*` mask into Win32 `MOD_*` flags for RegisterHotKey.
fn to_win32_mods(m: u8) -> u32 {
    let mut r = 0;
    if m & M_CTRL != 0 {
        r |= MOD_CONTROL;
    }
    if m & M_SHIFT != 0 {
        r |= MOD_SHIFT;
    }
    if m & M_ALT != 0 {
        r |= MOD_ALT;
    }
    if m & M_WIN != 0 {
        r |= MOD_WIN;
    }
    r
}

/// "Ctrl+Alt+Shift+Win+" prefix for a modifier mask (empty if none).
fn mods_prefix(m: u8) -> String {
    let mut s = String::new();
    if m & M_CTRL != 0 {
        s.push_str("Ctrl+");
    }
    if m & M_ALT != 0 {
        s.push_str("Alt+");
    }
    if m & M_SHIFT != 0 {
        s.push_str("Shift+");
    }
    if m & M_WIN != 0 {
        s.push_str("Win+");
    }
    s
}

/// Compact "CSAW" token (subset) used in the settings file.
fn mods_token(m: u8) -> String {
    let mut s = String::new();
    if m & M_CTRL != 0 {
        s.push('C');
    }
    if m & M_SHIFT != 0 {
        s.push('S');
    }
    if m & M_ALT != 0 {
        s.push('A');
    }
    if m & M_WIN != 0 {
        s.push('W');
    }
    s
}

fn parse_mods(s: &str) -> u8 {
    let mut m = 0;
    for c in s.chars() {
        match c {
            'C' => m |= M_CTRL,
            'S' => m |= M_SHIFT,
            'A' => m |= M_ALT,
            'W' => m |= M_WIN,
            _ => {}
        }
    }
    m
}

/// True for the modifier virtual-keys themselves (so capture waits for a real key).
fn is_modifier_vk(vk: u32) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C) || (0xA0..=0xA5).contains(&vk)
}

/// Best-effort display name for a virtual key (e.g. "V", "F5", "Space").
fn vk_name(vk: u32) -> String {
    unsafe {
        let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let lparam = (sc << 16) as i32;
        let mut buf = [0u16; 32];
        let n = GetKeyNameTextW(lparam, buf.as_mut_ptr(), buf.len() as i32);
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("key {:#x}", vk)
        }
    }
}

/// Collapse a string into a single-line preview (utf16, no NUL).
fn make_preview(s: &str) -> Vec<u16> {
    let mut out = String::new();
    let mut prev_space = false;
    for ch in s.chars() {
        let c = if ch == '\r' || ch == '\n' || ch == '\t' { ' ' } else { ch };
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
        if out.chars().count() >= 160 {
            break;
        }
    }
    wide_no_nul(out.trim())
}

// ---- Clipboard history ----------------------------------------------------

fn make_thumb(w: usize, h: usize, rgba: &[u8]) -> (i32, i32, Vec<u8>) {
    const BASE_H: usize = 40;
    const MAX_W: usize = 120;
    let (mut tw, mut th) = (w, h);
    if h > BASE_H || w > MAX_W {
        let scale = BASE_H as f32 / h as f32;
        th = BASE_H;
        tw = ((w as f32) * scale).round() as usize;
        if tw > MAX_W {
            let s2 = MAX_W as f32 / tw as f32;
            tw = MAX_W;
            th = ((th as f32) * s2).round() as usize;
        }
    }
    tw = tw.max(1);
    th = th.max(1);
    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th {
        let sy = (y * h / th).min(h - 1);
        for x in 0..tw {
            let sx = (x * w / tw).min(w - 1);
            let si = (sy * w + sx) * 4;
            let di = (y * tw + x) * 4;
            out[di] = rgba[si + 2]; // B
            out[di + 1] = rgba[si + 1]; // G
            out[di + 2] = rgba[si]; // R
            out[di + 3] = 255;
        }
    }
    (tw as i32, th as i32, out)
}

fn add_text(a: &mut App, t: String) {
    a.history_dirty = true;
    if let Some(pos) = a
        .history
        .iter()
        .position(|c| matches!(c, Clip::Text(s) if s == &t))
    {
        let c = a.history.remove(pos);
        a.history.insert(0, c);
        return;
    }
    a.history.insert(0, Clip::Text(t));
    while a.history.len() > MAX_HISTORY {
        if let Some(mut c) = a.history.pop() {
            scrub_clip(&mut c);
        }
    }
}

fn add_image(a: &mut App, w: usize, h: usize, rgba: Vec<u8>) {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return;
    }
    let mut hasher = DefaultHasher::new();
    w.hash(&mut hasher);
    h.hash(&mut hasher);
    rgba.hash(&mut hasher);
    let hash = hasher.finish();
    if let Some(pos) = a
        .history
        .iter()
        .position(|c| matches!(c, Clip::Image(ic) if ic.hash == hash))
    {
        let c = a.history.remove(pos);
        a.history.insert(0, c);
        return;
    }
    let (tw, th, thumb) = make_thumb(w, h, &rgba);
    a.history.insert(
        0,
        Clip::Image(Rc::new(ImageClip { w, h, rgba, tw, th, thumb, hash })),
    );
    while a.history.len() > MAX_HISTORY {
        if let Some(mut c) = a.history.pop() {
            scrub_clip(&mut c);
        }
    }
}

/// Best-effort wipe of a String's backing bytes before it is dropped.
fn scrub_string(s: &mut String) {
    unsafe {
        for b in s.as_mut_vec() {
            *b = 0;
        }
    }
    s.clear();
}

fn scrub_clip(c: &mut Clip) {
    match c {
        Clip::Text(s) => scrub_string(s),
        Clip::Image(ic) => {
            if let Some(ic) = Rc::get_mut(ic) {
                ic.rgba.iter_mut().for_each(|b| *b = 0);
                ic.thumb.iter_mut().for_each(|b| *b = 0);
            }
        }
    }
}

fn poll_clip(a: &mut App) {
    let seq = unsafe { GetClipboardSequenceNumber() };
    if seq == a.last_seq {
        return;
    }
    // Read both formats under a single clipboard open. If the app that just
    // copied still holds the clipboard, the open fails: leave last_seq alone and
    // retry next tick so the clip isn't lost (the "copy twice" bug).
    if unsafe { OpenClipboard(a.hwnd) } == 0 {
        return;
    }
    let img = unsafe { clip_get_image() };
    let txt = if img.is_none() { unsafe { clip_get_text() } } else { None };
    unsafe { CloseClipboard() };
    // Read came back empty even though the sequence changed (a slow app rendering
    // its data on demand): retry a few ticks before giving up, so it still lands
    // on the first copy instead of forcing a second one.
    if img.is_none() && txt.is_none() && a.poll_misses < 4 {
        a.poll_misses += 1;
        return;
    }
    a.poll_misses = 0;
    a.last_seq = seq;
    match (img, txt) {
        (Some((w, h, rgba)), _) => add_image(a, w, h, rgba),
        (None, Some(t)) if !t.is_empty() => add_text(a, t),
        _ => {}
    }
}

fn set_clipboard(a: &mut App, clip: &Clip) {
    unsafe {
        match clip {
            Clip::Text(s) => set_clipboard_text(a.hwnd, s, false),
            Clip::Image(ic) => clip_set_image(a.hwnd, ic.w, ic.h, &ic.rgba),
        }
        // Don't re-ingest our own write on the next poll.
        a.last_seq = GetClipboardSequenceNumber();
    }
}

/// Read clipboard text (CF_UNICODETEXT), or None if there's no text.
unsafe fn clip_get_text() -> Option<String> {
    // Caller already holds the clipboard open.
    let mut out = None;
    let h = GetClipboardData(CF_UNICODETEXT);
    if !h.is_null() {
        let p = GlobalLock(h) as *const u16;
        if !p.is_null() {
            let max = GlobalSize(h) / 2; // bound the scan; the data is NUL-terminated
            let mut len = 0;
            while len < max && *p.add(len) != 0 {
                len += 1;
            }
            out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)));
            GlobalUnlock(h);
        }
    }
    out
}

/// Read a clipboard image (CF_DIB) as top-left-origin RGBA, or None.
/// Caller already holds the clipboard open.
unsafe fn clip_get_image() -> Option<(usize, usize, Vec<u8>)> {
    let mut out = None;
    let h = GetClipboardData(CF_DIB);
    if !h.is_null() {
        let p = GlobalLock(h) as *const u8;
        if !p.is_null() {
            out = dib_to_rgba(std::slice::from_raw_parts(p, GlobalSize(h)));
            GlobalUnlock(h);
        }
    }
    out
}

/// Parse a packed DIB (header + optional masks/palette + pixels) into RGBA.
/// Defensive against malformed clipboard data: every offset is bounds-checked
/// and the dimensions are capped, so a bad DIB yields None, never UB. Handles
/// 24- and 32-bit BI_RGB / BI_BITFIELDS, what real apps put on the clipboard.
fn dib_to_rgba(d: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    if d.len() < 40 {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let i32_at = |o: usize| i32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let u16_at = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);

    let hdr = u32_at(0) as usize; // biSize
    if !(40..=d.len()).contains(&hdr) {
        return None;
    }
    let width = i32_at(4);
    let height = i32_at(8);
    let bpp = u16_at(14) as usize;
    let compression = u32_at(16);
    let clr_used = u32_at(32) as usize;

    if width <= 0 || height == 0 || (bpp != 24 && bpp != 32) {
        return None;
    }
    if compression != 0 && compression != 3 {
        return None; // only BI_RGB / BI_BITFIELDS
    }
    let w = width as usize;
    let h = height.unsigned_abs() as usize;
    let top_down = height < 0;
    if w.checked_mul(h)? > 64_000_000 {
        return None; // sane ~64 MP cap against a hostile header
    }

    let masks = if compression == 3 { 12 } else { 0 };
    // Fully checked so a hostile clrUsed can't overflow the offset (matters only
    // on a hypothetical 32-bit build; harmless and clearer on x86_64).
    let pix_off = hdr
        .checked_add(masks)?
        .checked_add(clr_used.checked_mul(4)?)?;
    let stride = (w * bpp).div_ceil(32) * 4; // rows are DWORD-aligned
    if pix_off.checked_add(stride.checked_mul(h)?)? > d.len() {
        return None;
    }

    let bytespp = bpp / 8;
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let sy = if top_down { row } else { h - 1 - row };
        let src = pix_off + sy * stride;
        for x in 0..w {
            let s = src + x * bytespp;
            let o = (row * w + x) * 4;
            out[o] = d[s + 2]; // R <- DIB byte order is BGRA
            out[o + 1] = d[s + 1]; // G
            out[o + 2] = d[s]; // B
            out[o + 3] = if bpp == 32 { d[s + 3] } else { 255 };
        }
    }
    // 32-bit DIBs often leave alpha as 0 (undefined); treat all-zero as opaque.
    if bpp == 32 && out.chunks_exact(4).all(|p| p[3] == 0) {
        for p in out.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Some((w, h, out))
}

/// Put text on the clipboard (CF_UNICODETEXT). When `exclude` is set, pasting a
/// pinned secret, also tag it out of Windows clipboard history (Win+V) and
/// cloud sync, the way password managers do (these formats have no library, so
/// it's done by hand here either way).
unsafe fn set_clipboard_text(hwnd: HWND, s: &str, exclude: bool) {
    if OpenClipboard(hwnd) == 0 {
        return;
    }
    EmptyClipboard();
    let text: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::slice::from_raw_parts(text.as_ptr() as *const u8, text.len() * 2);
    set_clip_data(CF_UNICODETEXT, global_from(bytes));
    if exclude {
        // Exclusion tags (each a DWORD 0): keep it out of Win+V and the cloud.
        let zero = 0u32.to_ne_bytes();
        for name in [
            "CanIncludeInClipboardHistory",
            "CanUploadToCloudClipboard",
            "ExcludeClipboardContentFromMonitorProcessing",
        ] {
            let fmt = RegisterClipboardFormatW(wide(name).as_ptr());
            if fmt != 0 {
                set_clip_data(fmt, global_from(&zero));
            }
        }
    }
    CloseClipboard();
}

/// Put an RGBA image on the clipboard as a top-down 32-bit CF_DIB.
unsafe fn clip_set_image(hwnd: HWND, w: usize, h: usize, rgba: &[u8]) {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return;
    }
    let mut dib: Vec<u8> = Vec::with_capacity(40 + w * h * 4);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(-(h as i32)).to_le_bytes()); // biHeight (negative = top-down)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&((w * h * 4) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&[0u8; 16]); // x/y ppm, clrUsed, clrImportant
    for px in rgba.chunks_exact(4) {
        dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]); // RGBA -> BGRA
    }
    if OpenClipboard(hwnd) == 0 {
        return;
    }
    EmptyClipboard();
    set_clip_data(CF_DIB, global_from(&dib));
    CloseClipboard();
}

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;

/// Allocate a moveable HGLOBAL holding `bytes`, for SetClipboardData (which
/// takes ownership of it on success).
unsafe fn global_from(bytes: &[u8]) -> *mut c_void {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
    if !h.is_null() {
        let p = GlobalLock(h);
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            GlobalUnlock(h);
        }
    }
    h
}

/// `SetClipboardData`, freeing the handle if the system didn't take ownership
/// (a failed call leaves us owning it, free it so we don't leak).
unsafe fn set_clip_data(fmt: u32, h: *mut c_void) {
    if h.is_null() {
        return;
    }
    if SetClipboardData(fmt, h).is_null() {
        GlobalFree(h);
    }
}

// ---- Pin persistence (DPAPI) ----------------------------------------------

fn dpapi_protect(plain: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let inb = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut outb = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        if CryptProtectData(&inb, null(), null(), null(), null(), 0, &mut outb) == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(outb.pbData, outb.cbData as usize).to_vec();
        LocalFree(outb.pbData as _);
        Some(v)
    }
}

fn dpapi_unprotect(enc: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let inb = CRYPT_INTEGER_BLOB {
            cbData: enc.len() as u32,
            pbData: enc.as_ptr() as *mut u8,
        };
        let mut outb = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        if CryptUnprotectData(&inb, null_mut(), null(), null(), null(), 0, &mut outb) == 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(outb.pbData, outb.cbData as usize).to_vec();
        LocalFree(outb.pbData as _);
        Some(v)
    }
}

// ---- Pinned-secret in-memory protection -----------------------------------
// A pinned secret is never held as plaintext in RAM. It's encrypted with
// CryptProtectMemory (per-process key) and the pages are VirtualLock'd out of
// the pagefile; it's decrypted only for the instant it's pasted or re-saved.

const MEM_BLOCK: usize = 16; // CRYPTPROTECTMEMORY_BLOCK_SIZE

/// Encrypt a secret in RAM. Returns the padded ciphertext (a multiple of 16)
/// and the original byte length.
fn protect_secret(plain: &str) -> (Vec<u8>, usize) {
    let len = plain.len();
    let padded = len.div_ceil(MEM_BLOCK).max(1) * MEM_BLOCK;
    let mut buf = vec![0u8; padded];
    buf[..len].copy_from_slice(plain.as_bytes());
    unsafe {
        CryptProtectMemory(
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            CRYPTPROTECTMEMORY_SAME_PROCESS,
        );
        VirtualLock(buf.as_mut_ptr() as *mut c_void, buf.len()); // best-effort: keep out of pagefile
    }
    (buf, len)
}

/// Decrypt a protected secret into a String for transient use. The caller MUST
/// scrub the returned String as soon as it's done with it.
fn unprotect_secret(enc: &[u8], len: usize) -> String {
    let mut buf = enc.to_vec();
    unsafe {
        CryptUnprotectMemory(
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            CRYPTPROTECTMEMORY_SAME_PROCESS,
        );
    }
    let s = String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned();
    buf.iter_mut().for_each(|b| *b = 0); // wipe the transient plaintext
    s
}

/// Zero + unlock a pin's encrypted secret buffer (on delete/unpin/exit).
fn scrub_pin(p: &mut Pin) {
    if !p.secret_enc.is_empty() {
        unsafe { VirtualUnlock(p.secret_enc.as_mut_ptr() as *mut c_void, p.secret_enc.len()) };
        p.secret_enc.iter_mut().for_each(|b| *b = 0);
    }
    p.secret_enc.clear();
    p.len = 0;
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Path to a per-user file under %LOCALAPPDATA%\ClipStack, creating the
/// directory on demand. One builder for pins, settings, and history.
/// Local (not Roaming) because clipboard data is machine-specific.
fn appdata_file(name: &str) -> std::path::PathBuf {
    let mut p = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    p.push("ClipStack");
    let _ = std::fs::create_dir_all(&p);
    p.push(name);
    p
}

/// One-time lift of data out of the per-machine subfolder
/// (ClipStack\<COMPUTERNAME>\) that versions through 0.4.3 used, so
/// upgrades keep their pins and history. Copies, never overwrites.
fn migrate_data() {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let base = std::path::PathBuf::from(local).join("ClipStack");
    let sub: String = std::env::var("COMPUTERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let src = base.join(if sub.is_empty() { "default".to_string() } else { sub });
    for name in ["pins.dat", "history.dat", "settings.txt"] {
        let (s, d) = (src.join(name), base.join(name));
        if s.exists() && !d.exists() {
            let _ = std::fs::copy(&s, d);
        }
    }
}

/// DPAPI-encrypt `s` and write it to `path`. Writes nothing if encryption
/// fails, there is never a plaintext fallback.
fn write_encrypted(path: std::path::PathBuf, s: &str) {
    if let Some(enc) = dpapi_protect(s.as_bytes()) {
        let _ = std::fs::write(path, enc);
    }
}

/// Read and DPAPI-decrypt `path` back into a string, if present and valid.
fn read_encrypted(path: std::path::PathBuf) -> Option<String> {
    let enc = std::fs::read(path).ok()?;
    let bytes = dpapi_unprotect(&enc)?;
    String::from_utf8(bytes).ok()
}

fn save_pins(a: &App) {
    let mut s = String::new();
    for p in &a.pins {
        let mut secret = unprotect_secret(&p.secret_enc, p.len);
        s.push_str(&escape(&p.label));
        s.push('\t');
        s.push_str(&escape(&secret));
        s.push('\n');
        scrub_string(&mut secret);
    }
    write_encrypted(appdata_file("pins.dat"), &s);
    scrub_string(&mut s); // s briefly held the plaintext secrets
}

fn load_pins() -> Vec<Pin> {
    let mut pins = Vec::new();
    if let Some(mut text) = read_encrypted(appdata_file("pins.dat")) {
        for line in text.lines() {
            if let Some((l, sec)) = line.split_once('\t') {
                let mut secret = unescape(sec);
                let (secret_enc, len) = protect_secret(&secret);
                scrub_string(&mut secret);
                pins.push(Pin { label: unescape(l), secret_enc, len });
            }
        }
        scrub_string(&mut text); // the decrypted file held plaintext secrets
    }
    pins
}

// settings.txt is plaintext, the trigger/persist flags aren't secrets
// (unlike the DPAPI-encrypted pins and history).

fn trigger_line(t: Trigger) -> String {
    match t {
        Trigger::Mouse { btn, mods } => {
            let b = match btn {
                Btn::Middle => "middle",
                Btn::X1 => "x1",
                Btn::X2 => "x2",
            };
            format!("trigger=mouse:{}:{}\n", b, mods_token(mods))
        }
        Trigger::Key { vk, mods } => format!("trigger=key:{}:{}\n", vk, mods_token(mods)),
    }
}

fn save_settings(trigger: Trigger, persist: bool, auto_copy: bool, theme_idx: usize) {
    let mut s = trigger_line(trigger);
    s.push_str(if persist { "persist=1\n" } else { "persist=0\n" });
    s.push_str(if auto_copy { "autocopy=1\n" } else { "autocopy=0\n" });
    s.push_str(&format!(
        "theme={}\n",
        THEMES.get(theme_idx).map_or("Dark", |t| t.0)
    ));
    let _ = std::fs::write(appdata_file("settings.txt"), s);
}

fn load_settings() -> (Trigger, bool, bool, usize) {
    let mut trigger = Trigger::MIDDLE;
    let mut persist = false;
    let mut auto_copy = false;
    let mut theme_idx = 0;
    if let Ok(text) = std::fs::read_to_string(appdata_file("settings.txt")) {
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("trigger=") {
                let mut parts = v.split(':');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("mouse"), Some(b), mods) => {
                        let btn = match b {
                            "x1" => Btn::X1,
                            "x2" => Btn::X2,
                            _ => Btn::Middle,
                        };
                        trigger = Trigger::Mouse { btn, mods: parse_mods(mods.unwrap_or("")) };
                    }
                    (Some("key"), Some(vk), mods) => {
                        if let Ok(vk) = vk.parse::<u32>() {
                            let mods = parse_mods(mods.unwrap_or(""));
                            // Guardrail (also enforced at capture): a keyboard
                            // trigger needs a real vk + a modifier, so a stale or
                            // hand-edited file can't bind a bare key globally.
                            if vk <= 0xFF && mods != 0 {
                                trigger = Trigger::Key { vk, mods };
                            }
                        }
                    }
                    _ => {}
                }
            } else if let Some(v) = line.strip_prefix("persist=") {
                persist = v.trim() == "1";
            } else if let Some(v) = line.strip_prefix("autocopy=") {
                auto_copy = v.trim() == "1";
            } else if let Some(v) = line.strip_prefix("theme=") {
                if let Some(i) = THEMES.iter().position(|t| t.0 == v.trim()) {
                    theme_idx = i;
                }
            }
        }
    }
    (trigger, persist, auto_copy, theme_idx)
}

// ---- History persistence (opt-in, DPAPI-encrypted) ------------------------

/// Persist the text clips (most-recent-first) DPAPI-encrypted to history.dat.
/// Only written when the user opts in; images are skipped, they stay
/// memory-only even when "Remember history" is on.
fn save_history(a: &App) {
    let mut s = String::new();
    for c in &a.history {
        if let Clip::Text(t) = c {
            s.push_str(&escape(t));
            s.push('\n');
        }
    }
    write_encrypted(appdata_file("history.dat"), &s);
}

/// Load persisted text clips, if any.
fn load_history() -> Vec<Clip> {
    let mut history = Vec::new();
    if let Some(text) = read_encrypted(appdata_file("history.dat")) {
        for line in text.lines() {
            if !line.is_empty() {
                history.push(Clip::Text(unescape(line)));
            }
        }
    }
    while history.len() > MAX_HISTORY {
        history.pop();
    }
    history
}

fn clear_history_file() {
    let _ = std::fs::remove_file(appdata_file("history.dat"));
}

// ---- Launch at startup (opt-in HKCU Run key) ------------------------------

/// Full path to our own exe as a NUL-terminated wide string, or None if it
/// can't be determined or is too long for the buffer, so we never write a
/// truncated/garbage Run-key value.
fn exe_path_w() -> Option<Vec<u16>> {
    let mut buf = [0u16; 600];
    let n = unsafe { GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None; // failed, or truncated (not NUL-terminated)
    }
    let mut v = buf[..n].to_vec();
    v.push(0);
    Some(v)
}

/// The exact REG_SZ string we register: the quoted full exe path (no NUL).
fn run_value_w() -> Option<Vec<u16>> {
    let path = exe_path_w()?;
    let mut v = Vec::with_capacity(path.len() + 1);
    v.push(b'"' as u16);
    v.extend_from_slice(&path[..path.len() - 1]); // drop the NUL
    v.push(b'"' as u16);
    Some(v)
}

/// The current Run-key value (NUL-trimmed), if present.
fn read_run_value() -> Option<Vec<u16>> {
    let subkey = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);
    unsafe {
        let mut size: u32 = 0;
        let q = |data: *mut c_void, size: *mut u32| {
            RegGetValueW(HKEY_CURRENT_USER, subkey.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), data, size)
        };
        if q(null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; (size as usize / 2) + 1];
        let mut size2 = (buf.len() * 2) as u32;
        if q(buf.as_mut_ptr() as *mut c_void, &mut size2) != 0 {
            return None;
        }
        let mut len = (size2 as usize / 2).min(buf.len());
        while len > 0 && buf[len - 1] == 0 {
            len -= 1;
        }
        buf.truncate(len);
        Some(buf)
    }
}

/// True only if the Run key exists AND points at *this* exe, so the checkbox
/// stays honest if this portable exe gets moved or renamed.
fn startup_enabled() -> bool {
    match (read_run_value(), run_value_w()) {
        (Some(stored), Some(expected)) => stored == expected,
        _ => false,
    }
}

/// Add or remove the Run-key entry. Only ever called from the tray toggle,
/// never set automatically, so we don't surprise users or trip AV heuristics.
fn set_startup(on: bool) {
    let subkey = wide(RUN_SUBKEY);
    let name = wide(RUN_VALUE);
    unsafe {
        if on {
            if let Some(val) = run_value_w() {
                let mut data = val;
                data.push(0); // NUL-terminate for REG_SZ
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    data.as_ptr() as *const c_void,
                    (data.len() * 2) as u32,
                );
            }
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), name.as_ptr());
        }
    }
}

// ---- Layout & paint -------------------------------------------------------

fn rebuild_rows(a: &mut App) {
    a.rows.clear();
    let n = a.history.len();
    let vis = n.min(VISIBLE);
    let max_scroll = n.saturating_sub(vis);
    if a.scroll > max_scroll {
        a.scroll = max_scroll;
    }
    let mut y = a.pad;
    for i in a.scroll..a.scroll + vis {
        a.rows.push(VRow { kind: RowKind::Hist(i), top: y, bottom: y + a.item_h });
        y += a.item_h;
    }
    let m = a.pins.len();
    if m > 0 {
        a.rows.push(VRow { kind: RowKind::Sep, top: y, bottom: y + a.sep_h });
        y += a.sep_h;
        let pvis = m.min(PIN_VISIBLE);
        a.pin_scroll = a.pin_scroll.min(m - pvis); // pvis <= m, so no underflow
        for j in a.pin_scroll..a.pin_scroll + pvis {
            a.rows.push(VRow { kind: RowKind::Pin(j), top: y, bottom: y + a.item_h });
            y += a.item_h;
        }
    }
}

fn rows_height(a: &App) -> i32 {
    a.rows.last().map(|r| r.bottom).unwrap_or(a.pad) + a.pad
}

fn row_at(a: &App, y: i32) -> Option<usize> {
    for (idx, r) in a.rows.iter().enumerate() {
        if y >= r.top && y < r.bottom {
            return match r.kind {
                RowKind::Sep => None,
                _ => Some(idx),
            };
        }
    }
    None
}

fn show_popup(a: &mut App, cx: i32, cy: i32) {
    if a.history.is_empty() && a.pins.is_empty() {
        return;
    }
    let dpi = unsafe { dpi_for_point(cx, cy) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    a.item_h = (28.0 * scale) as i32;
    a.sep_h = (12.0 * scale) as i32;
    a.pad = (6.0 * scale) as i32;
    a.width = (306.0 * scale) as i32;

    if !a.font.is_null() {
        unsafe { DeleteObject(a.font as _) };
    }
    a.font = unsafe { make_font(scale) };

    a.scroll = 0;
    a.pin_scroll = 0;
    rebuild_rows(a);
    let height = rows_height(a);

    unsafe { place_and_show(a, cx, cy, height) };
    a.hovered = -1;
    unsafe { reconcile_input(a) }; // a keyboard trigger needs the hook while open
}

/// Clip the window to a rounded rectangle. The system takes ownership of the
/// region, so we never free it; replacing it on the next resize is fine.
unsafe fn round_window(hwnd: HWND, w: i32, h: i32) {
    let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, CORNER_RADIUS, CORNER_RADIUS);
    SetWindowRgn(hwnd, rgn, 1);
}

/// Effective DPI of the monitor under a screen point. We read the *target*
/// monitor (where the popup is about to open) rather than GetDpiForWindow,
/// which would report the monitor the window currently sits on, so the very
/// first open on a differently-scaled screen uses the correct scale.
unsafe fn dpi_for_point(cx: i32, cy: i32) -> u32 {
    let hmon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    dx
}

/// Clamp `(cx, cy)` + the window size into the work area, move the window
/// there, round it, and show it. Shared by the history popup and the capture
/// prompt so the placement math lives in exactly one place.
unsafe fn place_and_show(a: &mut App, cx: i32, cy: i32, height: i32) {
    // Use the work area of the monitor *under the cursor*, not the primary one
    // (SPI_GETWORKAREA is primary-only), so the popup lands on the right screen.
    let mut mi: MONITORINFO = std::mem::zeroed();
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let hmon = MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST);
    GetMonitorInfoW(hmon, &mut mi);
    let wa = mi.rcWork;
    let xx = cx.min(wa.right - a.width).max(wa.left);
    let yy = cy.min(wa.bottom - height).max(wa.top);
    a.popup_x = xx;
    a.popup_y = yy;
    SetWindowPos(a.hwnd, HWND_TOPMOST, xx, yy, a.width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    round_window(a.hwnd, a.width, height);
    InvalidateRect(a.hwnd, null(), 1);
    a.visible = true;
}

/// The popup's Segoe UI font at the given DPI scale.
unsafe fn make_font(scale: f32) -> HFONT {
    let face = wide("Segoe UI");
    CreateFontW(
        -((13.0 * scale) as i32),
        0,
        0,
        0,
        FW_NORMAL as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        OUT_DEFAULT_PRECIS as u32,
        0,
        CLEARTYPE_QUALITY as u32,
        0,
        face.as_ptr(),
    )
}

/// Show the single-row "press a trigger" prompt near the cursor.
fn show_capture_prompt(a: &mut App, cx: i32, cy: i32) {
    let dpi = unsafe { dpi_for_point(cx, cy) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    a.item_h = (28.0 * scale) as i32;
    a.pad = (6.0 * scale) as i32;
    a.width = (306.0 * scale) as i32;
    if !a.font.is_null() {
        unsafe { DeleteObject(a.font as _) };
    }
    a.font = unsafe { make_font(scale) };
    let height = a.item_h + a.pad * 2;
    unsafe { place_and_show(a, cx, cy, height) };
    a.caret_on = true;
}

fn relayout(a: &mut App) {
    rebuild_rows(a);
    if a.rows.is_empty() {
        hide_popup(a);
        return;
    }
    let height = rows_height(a);
    unsafe {
        SetWindowPos(a.hwnd, HWND_TOPMOST, a.popup_x, a.popup_y, a.width, height, SWP_NOACTIVATE);
        round_window(a.hwnd, a.width, height);
        InvalidateRect(a.hwnd, null(), 1);
    }
    a.hovered = -1;
}

/// Toggle `WS_EX_NOACTIVATE`. Off lets the popup take keyboard focus (for inline
/// label editing / trigger capture); on restores click-through behavior.
unsafe fn set_no_activate(hwnd: HWND, on: bool) {
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let ex = if on {
        ex | WS_EX_NOACTIVATE as isize
    } else {
        ex & !(WS_EX_NOACTIVATE as isize)
    };
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
}

fn hide_popup(a: &mut App) {
    // Abandon any in-progress label edit (scrub the secret, restore the
    // no-activate style we dropped to take focus). Foreground naturally moves
    // to whatever the user clicked.
    if let Some(mut ed) = a.edit.take() {
        scrub_string(&mut ed.secret);
        unsafe {
            set_no_activate(a.hwnd, true);
        }
    }
    a.caret_on = false;
    a.about = false;
    a.toast = None;
    unsafe { ShowWindow(a.hwnd, SW_HIDE) };
    a.visible = false;
    a.hovered = -1;
    a.arm_delete = -1;
    unsafe { reconcile_input(a) }; // drop the hook again if a keyboard trigger
}

unsafe fn fill_color(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, color: COLORREF) {
    let b = CreateSolidBrush(color);
    let r = RECT { left, top, right, bottom };
    FillRect(hdc, &r, b);
    DeleteObject(b as _);
}

/// A malformed clip (control characters, line/paragraph separators, lone UTF-16
/// surrogates, or an absurd length) can crash DrawTextW deep inside USER32's
/// text engine. Build a safe, capped copy for anything we render in a row. A row
/// only ever shows one ellipsized line, so capping well above the visible width
/// loses nothing.
fn safe_row_text(text: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len().min(256));
    let mut i = 0;
    while i < text.len() && out.len() < 256 {
        let c = text[i];
        if (0xD800..=0xDBFF).contains(&c) {
            // High surrogate: keep only if a low surrogate follows.
            if i + 1 < text.len() && (0xDC00..=0xDFFF).contains(&text[i + 1]) {
                out.push(c);
                out.push(text[i + 1]);
                i += 2;
                continue;
            }
            out.push(0xFFFD);
        } else if (0xDC00..=0xDFFF).contains(&c) {
            out.push(0xFFFD); // lone low surrogate
        } else if c < 0x20 || c == 0x7F || c == 0x0085 || c == 0x2028 || c == 0x2029 {
            out.push(0x20); // control chars + line/paragraph separators
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

unsafe fn draw_text_row(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, text: &[u16]) {
    let safe = safe_row_text(text);
    if safe.is_empty() {
        // Nothing to draw. Critically, an empty Vec's as_ptr() is a dangling
        // pointer, and DrawTextW with DT_END_ELLIPSIS dereferences the buffer
        // even for a zero count, which faults. An empty clip thus crashed paint.
        return;
    }
    let mut tr = RECT { left, top, right, bottom };
    DrawTextW(
        hdc,
        safe.as_ptr(),
        safe.len() as i32,
        &mut tr,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
    );
}

unsafe fn draw_x(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32) {
    let mut tr = RECT { left, top, right, bottom };
    let glyph = wide_no_nul("\u{2715}");
    DrawTextW(
        hdc,
        glyph.as_ptr(),
        glyph.len() as i32,
        &mut tr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

/// Green pushpin (Segoe MDL2) shown on a hovered text row so it's obvious you
/// can pin it, mirrors draw_x, but in the icon font and sized to the row.
unsafe fn draw_pin(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, item_h: i32) {
    let face = wide("Segoe MDL2 Assets");
    let f = CreateFontW(
        -(item_h * 12 / 28), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0,
        DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, 0, CLEARTYPE_QUALITY as u32, 0,
        face.as_ptr(),
    );
    let old = SelectObject(hdc, f as _);
    let mut tr = RECT { left, top, right, bottom };
    let glyph = wide_no_nul("\u{E718}"); // MDL2 "Pin"
    DrawTextW(hdc, glyph.as_ptr(), glyph.len() as i32, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, old);
    DeleteObject(f as _);
}

/// Stacked up/down move arrows (green) on a hovered pin: click the upper half
/// to move it up, the lower half to move it down, so favorites float to the top.
unsafe fn draw_updown(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, item_h: i32) {
    let mid = (top + bottom) / 2;
    // A touch smaller than the row text so the filled triangles don't read chunky.
    let face = wide("Segoe UI");
    let f = CreateFontW(
        -(item_h * 11 / 28), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0,
        DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, 0, CLEARTYPE_QUALITY as u32, 0,
        face.as_ptr(),
    );
    let old = SelectObject(hdc, f as _);
    SetTextColor(hdc, theme().accent);
    let up = wide_no_nul("\u{25B2}");
    let mut tr = RECT { left, top, right, bottom: mid };
    DrawTextW(hdc, up.as_ptr(), up.len() as i32, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    let down = wide_no_nul("\u{25BC}");
    let mut td = RECT { left, top: mid, right, bottom };
    DrawTextW(hdc, down.as_ptr(), down.len() as i32, &mut td, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, old);
    DeleteObject(f as _);
}

/// A super-subtle 2px scroll thumb on the right edge of an overflowing section,
/// sized and positioned from the visible fraction and scroll offset.
unsafe fn draw_scroll_thumb(hdc: HDC, width: i32, y0: i32, y1: i32, total: usize, vis: usize, scroll: usize) {
    if total <= vis {
        return;
    }
    let track = (y1 - y0) as f32;
    let thumb_h = (track * vis as f32 / total as f32).clamp(14.0, track);
    let frac = scroll as f32 / (total - vis) as f32;
    let thumb_y = y0 as f32 + (track - thumb_h) * frac;
    fill_color(hdc, width - 3, thumb_y as i32, width - 1, (thumb_y + thumb_h) as i32, theme().pin_bullet);
}

/// Width in pixels of `text` in the font currently selected into `hdc`.
unsafe fn text_width(hdc: HDC, text: &[u16]) -> i32 {
    let mut sz: SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(hdc, text.as_ptr(), text.len() as i32, &mut sz);
    sz.cx
}

/// A 2px vertical caret bar.
unsafe fn draw_caret(hdc: HDC, x: i32, top: i32, bottom: i32) {
    fill_color(hdc, x, top, x + 2, bottom, theme().text);
}

/// Render the row that's being inline-labeled as a focused text field.
unsafe fn paint_edit_row(hdc: HDC, a: &App, r: &VRow, text_left: i32) {
    let ed = match a.edit.as_ref() {
        Some(e) => e,
        None => return,
    };
    let inset = (a.pad / 2).max(1);
    let (fx0, fx1) = (text_left - a.pad, a.width - a.pad);
    let (fy0, fy1) = (r.top + inset, r.bottom - inset);
    fill_color(hdc, fx0, fy0, fx1, fy1, theme().field_bg);
    let frame = CreateSolidBrush(theme().accent);
    let fr = RECT { left: fx0, top: fy0, right: fx1, bottom: fy1 };
    FrameRect(hdc, &fr, frame);
    DeleteObject(frame as _);

    let tr = a.width - a.pad * 2;
    let (ctop, cbot) = (fy0 + inset, fy1 - inset);
    if ed.label.is_empty() {
        if a.caret_on {
            draw_caret(hdc, text_left, ctop, cbot);
        }
        SetTextColor(hdc, theme().dim);
        let hint = wide_no_nul("Type a label  \u{2014}  Enter to pin, Esc to cancel");
        draw_text_row(hdc, text_left + a.pad, r.top, tr, r.bottom, &hint);
    } else {
        SetTextColor(hdc, theme().strong);
        draw_text_row(hdc, text_left, r.top, tr, r.bottom, &ed.label);
        if a.caret_on {
            let cx = (text_left + text_width(hdc, &ed.label) + 1).min(tr - 2);
            draw_caret(hdc, cx, ctop, cbot);
        }
    }
}

unsafe fn draw_thumb(hdc: HDC, ic: &ImageClip, x: i32, y: i32, maxh: i32) -> i32 {
    let dh = maxh.max(1);
    let dw = (ic.tw * dh / ic.th).max(1);
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = ic.tw;
    bmi.bmiHeader.biHeight = -ic.th; // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    StretchDIBits(
        hdc,
        x,
        y,
        dw,
        dh,
        0,
        0,
        ic.tw,
        ic.th,
        ic.thumb.as_ptr() as *const c_void,
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
    dw
}

const ABOUT_TAGLINE: &str = "A tiny, no-cloud clipboard manager for Windows.";

unsafe fn open_url(hwnd: HWND, url: &str) {
    let verb = wide("open");
    let u = wide(url);
    ShellExecuteW(hwnd, verb.as_ptr(), u.as_ptr(), null(), null(), SW_SHOWNORMAL);
}

/// Begin inline pin-labeling for history item `i` (text clips only, up to
/// MAX_PINS). Shared by the right-click and the pin-glyph click.
unsafe fn start_pin(i: usize) {
    let (start, capped) = {
        let mut a = app();
        if !matches!(a.history.get(i), Some(Clip::Text(_))) {
            (None, false) // image (or gone), can't pin
        } else if a.pins.len() >= MAX_PINS {
            (None, true) // at the limit
        } else {
            let secret = match a.history.get(i) {
                Some(Clip::Text(s)) => s.clone(),
                _ => unreachable!(),
            };
            let restore = a.target;
            a.edit = Some(Edit { hist: i, secret, label: Vec::new(), restore, rename: None });
            a.caret_on = true;
            (Some(a.hwnd), false)
        }
    };
    if let Some(hwnd2) = start {
        // Briefly drop WS_EX_NOACTIVATE and take keyboard focus so the popup
        // receives WM_CHAR; end_edit/hide_popup restore it.
        set_no_activate(hwnd2, false);
        SetForegroundWindow(hwnd2);
        SetFocus(hwnd2);
        InvalidateRect(hwnd2, null(), 1);
    } else if capped {
        let mut a = app();
        a.toast = Some(format!("Pin limit reached ({MAX_PINS}), unpin one first"));
        a.toast_ticks = 5; // ~2.5s on the 500ms tick
        InvalidateRect(a.hwnd, null(), 0);
    }
}

/// Right-click an existing pin to rename it: open the inline editor pre-filled
/// with the current label. Enter saves the new name, Esc or click-away cancels.
unsafe fn start_rename(j: usize) {
    let start = {
        let mut a = app();
        if j >= a.pins.len() {
            None
        } else {
            let label: Vec<u16> = a.pins[j].label.encode_utf16().collect();
            let restore = a.target;
            a.edit = Some(Edit {
                hist: 0,
                secret: String::new(),
                label,
                restore,
                rename: Some(j),
            });
            a.caret_on = true;
            Some(a.hwnd)
        }
    };
    if let Some(hwnd2) = start {
        set_no_activate(hwnd2, false);
        SetForegroundWindow(hwnd2);
        SetFocus(hwnd2);
        InvalidateRect(hwnd2, null(), 1);
    }
}

/// Swap pin `j` with its neighbor (up or down), persist the new order, relayout.
unsafe fn move_pin(j: usize, up: bool, to_end: bool) {
    let mut a = app();
    let m = a.pins.len();
    if (up && j == 0) || (!up && j + 1 >= m) {
        return; // already at the end in that direction
    }
    if to_end {
        // Shift+click: send all the way to the top or bottom.
        let p = a.pins.remove(j);
        let k = if up { 0 } else { a.pins.len() };
        a.pins.insert(k, p);
        if up {
            a.pin_scroll = 0; // show the top so the floated pin is visible
        }
    } else {
        let k = if up { j - 1 } else { j + 1 };
        a.pins.swap(j, k);
        if up && k < a.pin_scroll {
            a.pin_scroll = k; // keep it visible if it floated above the window
        }
    }
    save_pins(&a);
    relayout(&mut a);
}

struct AboutLayout {
    icon: RECT,
    title_y: i32,
    tagline_y: i32,
    web: (i32, i32), // (top, bottom) of the clickable link band
    gh: (i32, i32),
    footer_y: i32,
    height: i32,
}

/// Vertical layout of the About panel, shared by the painter and the click
/// hit-test so the link bands line up exactly with what's drawn.
fn about_layout(a: &App) -> AboutLayout {
    let (pad, lh) = (a.pad, a.item_h);
    let icon_sz = lh * 2;
    let mut y = pad * 2;
    let icon = RECT {
        left: (a.width - icon_sz) / 2,
        top: y,
        right: (a.width + icon_sz) / 2,
        bottom: y + icon_sz,
    };
    y = icon.bottom + pad * 2;
    let title_y = y;
    y += lh;
    let tagline_y = y;
    y += lh + pad;
    let web = (y, y + lh);
    y += lh;
    let gh = (y, y + lh);
    y += lh + pad;
    let footer_y = y;
    y += lh + pad * 2;
    AboutLayout { icon, title_y, tagline_y, web, gh, footer_y, height: y }
}

/// Open the About panel near the cursor, an on-brand dark card, not a MessageBox.
fn show_about(a: &mut App, cx: i32, cy: i32) {
    let dpi = unsafe { dpi_for_point(cx, cy) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    a.item_h = (26.0 * scale) as i32;
    a.pad = (6.0 * scale) as i32;
    a.width = (340.0 * scale) as i32;
    if !a.font.is_null() {
        unsafe { DeleteObject(a.font as _) };
    }
    a.font = unsafe { make_font(scale) };
    a.about = true;
    let height = about_layout(a).height;
    unsafe {
        place_and_show(a, cx, cy, height);
        reconcile_input(a); // keep the hook so a click outside dismisses it
    }
}

unsafe fn draw_center(hdc: HDC, a: &App, y: i32, text: &[u16]) {
    let mut tr = RECT { left: a.pad, top: y, right: a.width - a.pad, bottom: y + a.item_h };
    DrawTextW(
        hdc,
        text.as_ptr(),
        text.len() as i32,
        &mut tr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
}

unsafe fn paint_about(hdc: HDC, a: &App, rc: &RECT) {
    let lay = about_layout(a);
    let sz = lay.icon.right - lay.icon.left;
    // Load a crisp high-res frame (128px) and let the DC's halftone stretch
    // smooth the downscale. Requesting the icon at the small panel size made GDI
    // scale a tiny frame badly, that was the blur.
    let icon = load_app_icon(128, 128);
    SetStretchBltMode(hdc, STRETCH_HALFTONE);
    SetBrushOrgEx(hdc, 0, 0, null_mut());
    DrawIconEx(hdc, lay.icon.left, lay.icon.top, icon, sz, sz, 0, null_mut(), DI_NORMAL);
    DestroyIcon(icon);
    SetTextColor(hdc, theme().text);
    draw_center(hdc, a, lay.title_y, &wide_no_nul(&format!("ClipStack  v{}", env!("CARGO_PKG_VERSION"))));
    SetTextColor(hdc, theme().dim);
    draw_center(hdc, a, lay.tagline_y, &wide_no_nul(ABOUT_TAGLINE));
    SetTextColor(hdc, theme().accent);
    draw_center(hdc, a, lay.web.0, &wide_no_nul("hologramhacks.com"));
    draw_center(hdc, a, lay.gh.0, &wide_no_nul("github.com/HologramHacks/clipstack"));
    SetTextColor(hdc, theme().dim);
    draw_center(hdc, a, lay.footer_y, &wide_no_nul("Built by Brian Jones"));
    let border = CreateSolidBrush(theme().accent);
    FrameRect(hdc, rc, border);
    DeleteObject(border as _);
}

unsafe fn paint(hwnd: HWND) {
    let a = app();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);

    fill_color(hdc, 0, 0, rc.right, rc.bottom, theme().bg);
    let oldf = SelectObject(hdc, a.font as _);
    SetBkMode(hdc, TRANSPARENT as i32);

    if a.capturing {
        SetTextColor(hdc, theme().text);
        let prompt = wide_no_nul("Press a key combo or a mouse button  \u{2014}  Esc to cancel");
        let mut tr = RECT { left: a.pad * 2, top: 0, right: rc.right - a.pad * 2, bottom: rc.bottom };
        DrawTextW(
            hdc,
            prompt.as_ptr(),
            prompt.len() as i32,
            &mut tr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        SelectObject(hdc, oldf);
        let border = CreateSolidBrush(theme().accent);
        FrameRect(hdc, &rc, border);
        DeleteObject(border as _);
        EndPaint(hwnd, &ps);
        return;
    }

    if a.about {
        paint_about(hdc, &a, &rc);
        SelectObject(hdc, oldf);
        EndPaint(hwnd, &ps);
        return;
    }

    let text_left = a.pad * 2;
    let text_right = a.width - a.item_h; // reserve the right column for the ✕
    let pin_col = a.width - a.item_h * 2; // text rows reserve a pin column too
    let bar_w = (a.pad / 2).max(2); // hovered-row accent bar
    for (idx, r) in a.rows.iter().enumerate() {
        let hovered = idx as i32 == a.hovered;
        match r.kind {
            RowKind::Sep => {
                let mid = (r.top + r.bottom) / 2;
                fill_color(hdc, text_left, mid, a.width - text_left, mid + 1, theme().sep);
            }
            RowKind::Hist(i) => {
                if a.edit.as_ref().is_some_and(|e| e.hist == i) {
                    paint_edit_row(hdc, &a, r, text_left);
                    continue;
                }
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, theme().hover_bg);
                    fill_color(hdc, 0, r.top, bar_w, r.bottom, theme().accent);
                }
                match &a.history[i] {
                    Clip::Text(s) => {
                        SetTextColor(hdc, if hovered { theme().strong } else { theme().text });
                        draw_text_row(hdc, text_left, r.top, pin_col, r.bottom, &make_preview(s));
                    }
                    Clip::Image(ic) => {
                        let dw = draw_thumb(hdc, ic, text_left, r.top + a.pad, a.item_h - a.pad * 2);
                        let tx = text_left + dw + a.pad * 2;
                        SetTextColor(hdc, if hovered { rgb(220, 220, 225) } else { theme().dim });
                        let label = format!("image  {} \u{00d7} {}", ic.w, ic.h);
                        draw_text_row(hdc, tx, r.top, text_right, r.bottom, &wide_no_nul(&label));
                    }
                }
                if hovered {
                    if matches!(&a.history[i], Clip::Text(_)) {
                        SetTextColor(hdc, theme().accent);
                        draw_pin(hdc, pin_col, r.top, text_right, r.bottom, a.item_h);
                    }
                    SetTextColor(hdc, theme().delete);
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
            RowKind::Pin(j) => {
                if a.edit.as_ref().is_some_and(|e| e.rename == Some(j)) {
                    paint_edit_row(hdc, &a, r, text_left);
                    continue;
                }
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, theme().hover_bg);
                    fill_color(hdc, 0, r.top, bar_w, r.bottom, theme().accent);
                }
                // Dim masked bullets, then the label in bright text after them.
                let bullets = wide_no_nul(&"\u{2022}".repeat(8));
                SetTextColor(hdc, theme().pin_bullet);
                draw_text_row(hdc, text_left, r.top, pin_col, r.bottom, &bullets);
                let lx = text_left + text_width(hdc, &bullets) + a.pad * 3;
                SetTextColor(hdc, if hovered { theme().strong } else { theme().text });
                draw_text_row(hdc, lx, r.top, pin_col, r.bottom, &wide_no_nul(&a.pins[j].label));
                if hovered && idx as i32 == a.arm_delete {
                    // Armed: a solid red confirm button (inverted ✕), so a stray
                    // first click clearly warns instead of deleting.
                    fill_color(hdc, text_right, r.top + 1, a.width, r.bottom, theme().delete);
                    SetTextColor(hdc, theme().bg);
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                } else if hovered {
                    draw_updown(hdc, pin_col, r.top, text_right, r.bottom, a.item_h);
                    SetTextColor(hdc, theme().delete);
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
        }
    }

    if a.history.len() > VISIBLE {
        let t = a.rows.iter().find(|r| matches!(r.kind, RowKind::Hist(_))).map(|r| r.top);
        let b = a.rows.iter().rev().find(|r| matches!(r.kind, RowKind::Hist(_))).map(|r| r.bottom);
        if let (Some(t), Some(b)) = (t, b) {
            draw_scroll_thumb(hdc, a.width, t, b, a.history.len(), VISIBLE, a.scroll);
        }
    }
    if a.pins.len() > PIN_VISIBLE {
        let t = a.rows.iter().find(|r| matches!(r.kind, RowKind::Pin(_))).map(|r| r.top);
        let b = a.rows.iter().rev().find(|r| matches!(r.kind, RowKind::Pin(_))).map(|r| r.bottom);
        if let (Some(t), Some(b)) = (t, b) {
            draw_scroll_thumb(hdc, a.width, t, b, a.pins.len(), PIN_VISIBLE, a.pin_scroll);
        }
    }

    if let Some(msg) = &a.toast {
        let top = rc.bottom - a.item_h;
        fill_color(hdc, 0, top, rc.right, rc.bottom, theme().field_bg);
        fill_color(hdc, 0, top, rc.right, top + 1, theme().accent); // thin green divider
        SetTextColor(hdc, theme().accent);
        let w = wide_no_nul(msg);
        let mut tr = RECT { left: a.pad * 2, top, right: rc.right - a.pad * 2, bottom: rc.bottom };
        DrawTextW(
            hdc,
            w.as_ptr(),
            w.len() as i32,
            &mut tr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }

    SelectObject(hdc, oldf);
    let border = CreateSolidBrush(theme().sep);
    FrameRect(hdc, &rc, border);
    DeleteObject(border as _);
    EndPaint(hwnd, &ps);
}

// ---- Selection / paste ----------------------------------------------------

fn delete_row(a: &mut App, idx: usize) {
    match a.rows[idx].kind {
        RowKind::Hist(i) => {
            let mut c = a.history.remove(i);
            scrub_clip(&mut c);
            a.history_dirty = true;
        }
        RowKind::Pin(j) => {
            let mut p = a.pins.remove(j);
            scrub_pin(&mut p);
            save_pins(a);
        }
        RowKind::Sep => {}
    }
}

fn commit_row(a: &mut App, idx: usize) -> HWND {
    match a.rows[idx].kind {
        RowKind::Hist(i) => {
            let clip = a.history[i].clone();
            set_clipboard(a, &clip);
            let c = a.history.remove(i);
            a.history.insert(0, c);
            a.history_dirty = true;
            hide_popup(a);
            a.target
        }
        RowKind::Pin(j) => {
            let mut s = unprotect_secret(&a.pins[j].secret_enc, a.pins[j].len);
            unsafe {
                set_clipboard_text(a.hwnd, &s, true); // excluded from Win+V history + cloud
                a.last_seq = GetClipboardSequenceNumber(); // don't re-ingest our own write
            }
            scrub_string(&mut s); // wipe our transient plaintext (the live clipboard copy is inherent)
            hide_popup(a);
            a.target
        }
        RowKind::Sep => null_mut(),
    }
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Synthesize a Ctrl+<vk> chord: 'V' to paste, 'C' to copy (auto-copy).
fn send_combo(vk: u16) {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(vk, false),
        key_input(vk, true),
        key_input(VK_CONTROL, true),
    ];
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

// ---- Inline pin labeling --------------------------------------------------

/// Finish an in-progress inline label edit. On `commit` with a non-empty label,
/// the clip is pinned and the pins file is rewritten; either way the no-activate
/// style is restored and keyboard focus is handed back to the app the user came
/// from. The popup stays open so a freshly added pin is visible.
unsafe fn end_edit(commit: bool) {
    let (hwnd, restore) = {
        let mut a = app();
        let mut ed = match a.edit.take() {
            Some(e) => e,
            None => return,
        };
        if commit {
            let label = String::from_utf16_lossy(&ed.label).trim().to_string();
            if !label.is_empty() {
                if let Some(j) = ed.rename {
                    // Renaming an existing pin: only the label changes.
                    if let Some(p) = a.pins.get_mut(j) {
                        p.label = label;
                        save_pins(&a);
                    }
                } else {
                    let mut secret = std::mem::take(&mut ed.secret);
                    let (secret_enc, len) = protect_secret(&secret);
                    scrub_string(&mut secret);
                    a.pins.push(Pin { label, secret_enc, len });
                    save_pins(&a);
                }
            }
        }
        scrub_string(&mut ed.secret); // no-op if the secret was moved into the pin
        a.caret_on = false;
        relayout(&mut a); // resize for the (possibly) new pin and repaint
        (a.hwnd, ed.restore)
    };
    // Restore the no-activate style we dropped to grab focus, then hand the
    // keyboard back to wherever the user was typing before.
    set_no_activate(hwnd, true);
    if !restore.is_null() {
        SetForegroundWindow(restore);
    }
}

// ---- Input plumbing: hook + hotkey lifecycle ------------------------------

/// Install/remove the mouse hook and the keyboard hotkey to match the current
/// (paused, trigger, visible, capturing) state. This is what makes Pause a
/// *true* zero-footprint stop, and lets a keyboard trigger leave no mouse hook
/// installed while idle (so apps like Houdini keep their middle button).
unsafe fn reconcile_input(a: &mut App) {
    // The mouse hook is needed for: a mouse trigger, the popup being open
    // (wheel-scroll + click-away), capturing a custom trigger, or auto-copy
    // (watching for a selection drag while idle).
    let want_hook =
        !a.paused && (!a.trigger.is_key() || a.visible || a.capturing || a.auto_copy);
    let have_hook = a.hook != 0;
    if want_hook && !have_hook {
        a.hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), a.hinst, 0) as isize;
    } else if !want_hook && have_hook {
        UnhookWindowsHookEx(a.hook as _);
        a.hook = 0;
    }

    let want_hotkey = !a.paused && a.trigger.is_key();
    if want_hotkey && !a.hotkey_active {
        if let Trigger::Key { vk, mods } = a.trigger {
            if RegisterHotKey(a.hwnd, ID_HOTKEY, to_win32_mods(mods) | MOD_NOREPEAT, vk) != 0 {
                a.hotkey_active = true;
            }
        }
    } else if !want_hotkey && a.hotkey_active {
        UnregisterHotKey(a.hwnd, ID_HOTKEY);
        a.hotkey_active = false;
    }
}

/// Tell the user a keyboard combo couldn't be claimed (already in use).
unsafe fn warn_hotkey_taken(hwnd: HWND) {
    let text = wide("That key combination is already in use by Windows or another app. Pick a different one.");
    let title = wide("ClipStack");
    MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONWARNING);
}

/// Switch the active trigger: reconcile hook/hotkey, persist, refresh tooltip.
/// If a keyboard combo can't be registered (already taken), roll back so we
/// never persist or leave the user on a dead trigger.
unsafe fn set_trigger(t: Trigger) {
    let (hwnd, desc, ok) = {
        let mut a = app();
        if a.trigger == t {
            return;
        }
        let prev = a.trigger;
        a.trigger = t;
        reconcile_input(&mut a);
        if t.is_key() && !a.hotkey_active {
            a.trigger = prev;
            reconcile_input(&mut a);
            (a.hwnd, prev.describe(), false)
        } else {
            (a.hwnd, t.describe(), true)
        }
    };
    update_tray_tip(hwnd, &desc);
    if ok {
        let (persist, auto_copy, theme_idx) =
            { let a = app(); (a.persist, a.auto_copy, a.theme_idx) };
        save_settings(t, persist, auto_copy, theme_idx);
    } else {
        warn_hotkey_taken(hwnd);
    }
}

/// Fill a tray icon's tooltip with "ClipStack: {desc} to open".
unsafe fn set_tip(nid: &mut NOTIFYICONDATAW, desc: &str) {
    let tip = wide(&format!("ClipStack: {} to open", desc));
    let n = tip.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip[..n]);
}

unsafe fn update_tray_tip(hwnd: HWND, desc: &str) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_TIP;
    set_tip(&mut nid, desc);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

// ---- Custom-trigger capture -----------------------------------------------

/// Pack a Trigger into a message wparam (0 means "cancelled").
fn encode_trigger(t: Trigger) -> usize {
    match t {
        Trigger::Mouse { btn, mods } => {
            let code = match btn {
                Btn::Middle => 1,
                Btn::X1 => 2,
                Btn::X2 => 3,
            };
            ((mods as usize) << 16) | code
        }
        Trigger::Key { vk, mods } => (1 << 24) | ((mods as usize) << 16) | (vk as usize & 0xffff),
    }
}

fn decode_trigger(w: usize) -> Option<Trigger> {
    if w == 0 {
        return None;
    }
    let mods = ((w >> 16) & 0xff) as u8;
    let code = (w & 0xffff) as u32;
    if (w >> 24) & 0xff == 1 {
        Some(Trigger::Key { vk: code, mods })
    } else {
        let btn = match code {
            2 => Btn::X1,
            3 => Btn::X2,
            _ => Btn::Middle,
        };
        Some(Trigger::Mouse { btn, mods })
    }
}

/// Begin listening for a custom trigger: show a prompt, take keyboard focus
/// (for key combos), and ensure the mouse hook is on (for button choices).
unsafe fn start_capture() {
    let (hwnd, x, y) = {
        let mut a = app();
        if a.capturing {
            return;
        }
        a.target = GetForegroundWindow(); // focus to restore when done
        a.capturing = true;
        reconcile_input(&mut a);
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        (a.hwnd, pt.x, pt.y)
    };
    set_no_activate(hwnd, false);
    show_capture_prompt(&mut app(), x, y);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
}

/// Finish capture: `Some(t)` applies the new trigger, `None` cancels.
unsafe fn finish_capture(new: Option<Trigger>) {
    let (hwnd, restore, desc, failed) = {
        let mut a = app();
        if !a.capturing {
            return;
        }
        a.capturing = false;
        a.caret_on = false;
        set_no_activate(a.hwnd, true);
        a.visible = false;
        ShowWindow(a.hwnd, SW_HIDE);
        let prev = a.trigger;
        let mut failed = false;
        if let Some(t) = new {
            a.trigger = t;
            reconcile_input(&mut a);
            if t.is_key() && !a.hotkey_active {
                a.trigger = prev; // couldn't register the combo, keep the old one
                reconcile_input(&mut a);
                failed = true;
            }
        } else {
            reconcile_input(&mut a);
        }
        (a.hwnd, a.target, a.trigger.describe(), failed)
    };
    if let Some(t) = new {
        if !failed {
            let (persist, auto_copy, theme_idx) =
                { let a = app(); (a.persist, a.auto_copy, a.theme_idx) };
            save_settings(t, persist, auto_copy, theme_idx);
        }
    }
    if !restore.is_null() {
        SetForegroundWindow(restore);
    }
    update_tray_tip(hwnd, &desc);
    if failed {
        warn_hotkey_taken(hwnd);
    }
}

// ---- Low-level mouse hook -------------------------------------------------

/// Does this mouse-down event match the configured mouse trigger?
fn mouse_event_matches(t: Trigger, msg: u32, mouse_data: u32) -> bool {
    if let Trigger::Mouse { btn, mods } = t {
        if cur_mods() != mods {
            return false; // exact modifier match, so e.g. Ctrl+middle-click isn't
                          // swallowed by a plain-middle trigger
        }
        match btn {
            Btn::Middle => msg == WM_MBUTTONDOWN,
            Btn::X1 => msg == WM_XBUTTONDOWN && (mouse_data >> 16) & 0xffff == XBUTTON1 as u32,
            Btn::X2 => msg == WM_XBUTTONDOWN && (mouse_data >> 16) & 0xffff == XBUTTON2 as u32,
        }
    } else {
        false
    }
}

/// The button-up message paired with a given button-down.
fn up_for(down: u32) -> u32 {
    if down == WM_XBUTTONDOWN {
        WM_XBUTTONUP
    } else {
        WM_MBUTTONUP
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION {
        let msg = wp as u32;
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        let pt = info.pt;
        let mut a = match try_app() {
            Some(a) => a,
            // Re-entrant call while the app is already borrowed (Windows can
            // invoke an LL hook during SetWindowPos and friends). Skip this one
            // event instead of double-borrowing and aborting the process.
            None => return CallNextHookEx(null_mut(), code, wp, lp),
        };

        // Auto-copy on highlight (opt-in, off by default): treat a left-button
        // drag as a text selection and copy it on release. Heuristic on purpose.
        if a.auto_copy && !a.visible && !a.capturing && a.edit.is_none() {
            match msg {
                WM_LBUTTONDOWN => a.drag_start = Some(pt),
                WM_LBUTTONUP => {
                    if let Some(s) = a.drag_start.take() {
                        if (pt.x - s.x).abs() + (pt.y - s.y).abs() > 6 {
                            PostMessageW(a.hwnd, WM_APP_AUTOCOPY, 0, 0);
                        }
                    }
                }
                _ => {}
            }
        }

        if a.capturing {
            // Listening for a custom trigger: a non-typing button picks it; a
            // normal left/right click cancels. Swallow buttons so nothing leaks.
            match msg {
                WM_MBUTTONDOWN => {
                    let t = Trigger::Mouse { btn: Btn::Middle, mods: cur_mods() };
                    PostMessageW(a.hwnd, WM_APP_CAPTURED, encode_trigger(t), 0);
                }
                WM_XBUTTONDOWN => {
                    let btn = if (info.mouseData >> 16) & 0xffff == XBUTTON2 as u32 {
                        Btn::X2
                    } else {
                        Btn::X1
                    };
                    let t = Trigger::Mouse { btn, mods: cur_mods() };
                    PostMessageW(a.hwnd, WM_APP_CAPTURED, encode_trigger(t), 0);
                }
                WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                    PostMessageW(a.hwnd, WM_APP_CAPTURED, 0, 0); // cancel
                }
                _ => {}
            }
            return match msg {
                WM_MOUSEMOVE | WM_MOUSEWHEEL => CallNextHookEx(null_mut(), code, wp, lp),
                _ => 1,
            };
        }

        match msg {
            m if !a.paused && mouse_event_matches(a.trigger, m, info.mouseData) => {
                // The trigger button is fully ours: toggle the popup and swallow
                // both the press and its release so the app never sees it (so a
                // Back/Forward button stops navigating while it's mapped here).
                if a.visible {
                    PostMessageW(a.hwnd, WM_APP_HIDE, 0, 0);
                } else {
                    a.target = GetForegroundWindow();
                    PostMessageW(a.hwnd, WM_APP_SHOW, pt.x as usize, pt.y as isize);
                }
                a.swallow_up = Some(up_for(m));
                return 1;
            }
            m if a.swallow_up == Some(m) => {
                a.swallow_up = None;
                return 1;
            }
            WM_MOUSEWHEEL if a.visible => {
                let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
                let dir = if delta > 0 { 1usize } else { 2usize };
                PostMessageW(a.hwnd, WM_APP_SCROLL, dir, info.pt.y as isize);
                return 1;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN if a.visible => {
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(a.hwnd, &mut r);
                let inside = pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
                if !inside {
                    PostMessageW(a.hwnd, WM_APP_HIDE, 0, 0);
                }
            }
            _ => {}
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

// ---- Popup window procedure -----------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_SHOW => {
            show_popup(&mut app(), wp as i32, lp as i32);
            0
        }
        WM_APP_HIDE => {
            hide_popup(&mut app());
            0
        }
        WM_APP_AUTOCOPY => {
            send_combo(0x43); // Ctrl+C: copy the just-released selection; the timer poll ingests it
            0
        }
        WM_APP_SCROLL => {
            let mut a = app();
            if a.edit.is_some() {
                return 0; // freeze the list while labeling so indices can't shift
            }
            let (old_scroll, old_pin) = (a.scroll, a.pin_scroll);
            // Scroll whichever section the cursor is over: pins (below the
            // separator) or history (above it).
            let cy = lp as i32 - a.popup_y;
            let sep_top = a
                .rows
                .iter()
                .find_map(|r| matches!(r.kind, RowKind::Sep).then_some(r.top));
            let over_pins = sep_top.is_some_and(|t| cy >= t);
            match (over_pins, wp) {
                (true, 1) => a.pin_scroll = a.pin_scroll.saturating_sub(SCROLL_STEP),
                (true, 2) => {
                    let m = a.pins.len();
                    let max = m.saturating_sub(m.min(PIN_VISIBLE));
                    a.pin_scroll = (a.pin_scroll + SCROLL_STEP).min(max);
                }
                (false, 1) => a.scroll = a.scroll.saturating_sub(SCROLL_STEP),
                (false, 2) => {
                    let max = a.history.len().saturating_sub(a.history.len().min(VISIBLE));
                    a.scroll = (a.scroll + SCROLL_STEP).min(max);
                }
                _ => {}
            }
            if a.scroll == old_scroll && a.pin_scroll == old_pin {
                return 0; // already at the end in that direction: nothing moved, so no
                          // rebuild and no repaint, which is what caused the end flicker
            }
            a.hovered = -1;
            rebuild_rows(&mut a);
            InvalidateRect(hwnd, null(), 1);
            0
        }
        WM_TIMER => {
            if wp == TIMER_CLIP {
                let mut a = app();
                if a.edit.is_some() {
                    // Blink the label caret while editing.
                    a.caret_on = !a.caret_on;
                    InvalidateRect(hwnd, null(), 0);
                } else if !a.visible {
                    // Don't ingest new clips while the popup is open, it would
                    // shift the history indices the on-screen rows point at.
                    poll_clip(&mut a);
                }
                if a.toast.is_some() {
                    a.toast_ticks = a.toast_ticks.saturating_sub(1);
                    if a.toast_ticks == 0 {
                        a.toast = None;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                // Flush history to disk if it changed and persistence is on. The
                // ~0.5s cadence means a crash loses at most half a second.
                if a.persist && a.history_dirty {
                    save_history(&a);
                    a.history_dirty = false;
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let mut a = app();
            if a.edit.is_some() {
                return 0; // no hover changes while a label field is open
            }
            let (_, y) = lo_hi(lp);
            if !a.tracking_leave {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                a.tracking_leave = true;
            }
            let row = row_at(&a, y).map(|i| i as i32).unwrap_or(-1);
            if row != a.hovered {
                let old = a.hovered;
                a.hovered = row;
                if a.arm_delete >= 0 && a.arm_delete != row {
                    a.arm_delete = -1; // moved off the armed pin: disarm it
                }
                // Repaint only the two affected rows so the rest of the list
                // doesn't flash on hover.
                for r in [old, row] {
                    if let Some(vr) = usize::try_from(r).ok().and_then(|i| a.rows.get(i)) {
                        let rc = RECT { left: 0, top: vr.top, right: a.width, bottom: vr.bottom };
                        InvalidateRect(hwnd, &rc, 1);
                    }
                }
            }
            0
        }
        WM_MOUSELEAVE => {
            let mut a = app();
            a.hovered = -1;
            a.arm_delete = -1; // disarm any pending pin-delete on leave
            a.tracking_leave = false;
            InvalidateRect(hwnd, null(), 1);
            0
        }
        WM_KILLFOCUS => {
            // We only ever hold focus during capture or inline-edit; if we lose
            // it (e.g. the user alt-tabs away mid-capture), post a cancel so
            // capture can't get wedged. It MUST be posted, not handled inline:
            // WM_KILLFOCUS can arrive synchronously while the App borrow is held
            // (ShowWindow(SW_HIDE) on the focused window), so touching app() here
            // would re-borrow and panic. The posted cancel is a no-op unless a
            // capture is actually in progress.
            PostMessageW(hwnd, WM_APP_CAPTURED, 0, 0);
            0
        }
        WM_LBUTTONUP => {
            if app().about {
                let (_, y) = lo_hi(lp);
                let lay = about_layout(&app());
                if y >= lay.web.0 && y < lay.web.1 {
                    open_url(hwnd, "https://hologramhacks.com");
                } else if y >= lay.gh.0 && y < lay.gh.1 {
                    open_url(hwnd, "https://github.com/HologramHacks/clipstack");
                } else {
                    hide_popup(&mut app());
                }
                return 0;
            }
            if app().edit.is_some() {
                return 0; // ignore clicks on rows while labeling; Enter/Esc only
            }
            let (x, y) = lo_hi(lp);
            enum Act {
                Paste(HWND),
                Pin(usize),
                MovePin(usize, bool, bool),
                None,
            }
            let act = {
                let mut a = app();
                let was_armed = a.arm_delete;
                a.arm_delete = -1; // any click disarms a pending pin-delete by default
                match row_at(&a, y) {
                    Some(idx) if x >= a.width - a.item_h => {
                        // Clicked the ✕ on the right edge. Pins require a confirming
                        // second click: the first click arms it (history deletes in one).
                        if matches!(a.rows[idx].kind, RowKind::Pin(_)) && was_armed != idx as i32 {
                            a.arm_delete = idx as i32;
                            InvalidateRect(a.hwnd, null(), 1);
                            Act::None
                        } else {
                            delete_row(&mut a, idx);
                            if a.history.is_empty() && a.pins.is_empty() {
                                hide_popup(&mut a);
                            } else {
                                relayout(&mut a);
                            }
                            Act::None
                        }
                    }
                    Some(idx) if x >= a.width - a.item_h * 2 => {
                        // The affordance column: text history rows pin here; pin
                        // rows move up (top half) or down (bottom half).
                        match a.rows[idx].kind {
                            RowKind::Hist(i) if matches!(a.history.get(i), Some(Clip::Text(_))) => {
                                Act::Pin(i)
                            }
                            RowKind::Pin(j) => {
                                let r = &a.rows[idx];
                                let up = y < (r.top + r.bottom) / 2;
                                Act::MovePin(j, up, cur_mods() & M_SHIFT != 0)
                            }
                            _ => Act::Paste(commit_row(&mut a, idx)),
                        }
                    }
                    Some(idx) => Act::Paste(commit_row(&mut a, idx)),
                    None => Act::None,
                }
            };
            match act {
                Act::Paste(target) if !target.is_null() => {
                    SetForegroundWindow(target);
                    std::thread::sleep(Duration::from_millis(40));
                    send_combo(0x56); // Ctrl+V
                }
                Act::Pin(i) => start_pin(i),
                Act::MovePin(j, up, to_end) => move_pin(j, up, to_end),
                _ => {}
            }
            0
        }
        WM_RBUTTONUP => {
            if app().edit.is_some() {
                return 0; // already labeling; ignore further right-clicks
            }
            let (_x, y) = lo_hi(lp);
            let kind = {
                let a = app();
                row_at(&a, y).map(|idx| a.rows[idx].kind)
            };
            match kind {
                Some(RowKind::Pin(j)) => start_rename(j),
                Some(RowKind::Hist(i)) => start_pin(i),
                _ => {}
            }
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if app().capturing {
                let vk = wp as u32;
                // Apply via the same WM_APP_CAPTURED channel the mouse hook uses,
                // so both capture paths funnel through one handler.
                if vk == 0x1B {
                    PostMessageW(hwnd, WM_APP_CAPTURED, 0, 0); // Esc cancels
                } else if !is_modifier_vk(vk) {
                    let mods = cur_mods();
                    if mods != 0 {
                        let w = encode_trigger(Trigger::Key { vk, mods });
                        PostMessageW(hwnd, WM_APP_CAPTURED, w, 0);
                    }
                    // bare key (no modifier) is rejected by the guardrail; wait
                }
                return 0;
            }
            if msg == WM_KEYDOWN && app().edit.is_some() {
                match wp as u16 {
                    0x1B => end_edit(false), // VK_ESCAPE: cancel
                    0x0D => {
                        // VK_RETURN: pin only when the trimmed label is non-empty.
                        // On empty-Enter we intentionally do nothing, keeping the
                        // field open to keep typing, rather than calling end_edit
                        // (which would close it). Don't "simplify" into end_edit(true).
                        let ready = app().edit.as_ref().is_some_and(|e| {
                            !String::from_utf16_lossy(&e.label).trim().is_empty()
                        });
                        if ready {
                            end_edit(true);
                        }
                    }
                    _ => {}
                }
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CHAR => {
            if app().edit.is_none() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            match wp as u16 {
                0x08 => {
                    // Backspace: drop the last code unit (plus its surrogate pair).
                    let mut a = app();
                    if let Some(e) = a.edit.as_mut() {
                        if let Some(last) = e.label.pop() {
                            if (0xDC00..=0xDFFF).contains(&last)
                                && matches!(e.label.last(), Some(&p) if (0xD800..=0xDBFF).contains(&p)) {
                                    e.label.pop();
                                }
                        }
                    }
                    a.caret_on = true;
                    InvalidateRect(hwnd, null(), 0);
                }
                0x1B | 0x0D | 0x09 => {} // Esc/Enter handled in WM_KEYDOWN; ignore Tab
                c if c < 0x20 => {}      // ignore other control chars
                c => {
                    let mut a = app();
                    if let Some(e) = a.edit.as_mut() {
                        if e.label.len() < 200 {
                            e.label.push(c);
                        }
                    }
                    a.caret_on = true;
                    InvalidateRect(hwnd, null(), 0);
                }
            }
            0
        }
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_HOTKEY => {
            let mut a = app();
            if !a.visible && !a.paused && !a.capturing {
                a.target = GetForegroundWindow();
                let mut pt: POINT = std::mem::zeroed();
                GetCursorPos(&mut pt);
                show_popup(&mut a, pt.x, pt.y);
            }
            0
        }
        WM_APP_CAPTURED => {
            finish_capture(decode_trigger(wp));
            0
        }
        WM_APP_TRAY => {
            let event = (lp as u32) & 0xffff;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU || event == WM_LBUTTONUP {
                show_tray_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match wp & 0xffff {
                ID_PAUSE => {
                    let mut a = app();
                    a.paused = !a.paused;
                    if a.paused {
                        hide_popup(&mut a);
                    }
                    reconcile_input(&mut a); // pause fully removes the hook/hotkey
                }
                ID_CLEAR => {
                    let mut a = app();
                    a.history.iter_mut().for_each(scrub_clip);
                    a.history.clear();
                    a.history_dirty = false;
                    if a.persist {
                        clear_history_file(); // wipe the persisted copy too
                    }
                    hide_popup(&mut a);
                }
                ID_QUIT => {
                    DestroyWindow(hwnd);
                }
                ID_ABOUT => {
                    let mut pt: POINT = std::mem::zeroed();
                    GetCursorPos(&mut pt);
                    show_about(&mut app(), pt.x, pt.y);
                }
                ID_STARTUP => set_startup(!startup_enabled()),
                ID_PERSIST => {
                    let mut a = app();
                    a.persist = !a.persist;
                    save_settings(a.trigger, a.persist, a.auto_copy, a.theme_idx);
                    if a.persist {
                        save_history(&a); // capture what's already in memory
                    } else {
                        clear_history_file(); // stop remembering: delete the file
                    }
                }
                ID_AUTOCOPY => {
                    let mut a = app();
                    a.auto_copy = !a.auto_copy;
                    save_settings(a.trigger, a.persist, a.auto_copy, a.theme_idx);
                    reconcile_input(&mut a); // install/remove the hook for drag-watching
                }
                ID_TRIG_CUSTOM => start_capture(),
                cmd => {
                    if let Some(&(_, t, _)) = PRESETS.iter().find(|&&(id, _, _)| id == cmd) {
                        set_trigger(t);
                    } else if (ID_THEME_BASE..ID_THEME_BASE + THEMES.len()).contains(&cmd) {
                        let mut a = app();
                        a.theme_idx = cmd - ID_THEME_BASE;
                        set_theme(a.theme_idx);
                        save_settings(a.trigger, a.persist, a.auto_copy, a.theme_idx);
                        InvalidateRect(a.hwnd, null(), 1); // repaint in the new theme
                    }
                }
            }
            0
        }
        WM_DESTROY => {
            cleanup(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let (paused, trigger, persist, auto_copy, theme_idx) = {
        let a = app();
        (a.paused, a.trigger, a.persist, a.auto_copy, a.theme_idx)
    };
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let menu = CreatePopupMenu();

    // Trigger submenu, radio-checked on the active choice.
    let sub = CreatePopupMenu();
    for &(id, _, label) in PRESETS.iter() {
        AppendMenuW(sub, MF_STRING, id, wide(label).as_ptr());
    }
    AppendMenuW(sub, MF_SEPARATOR, 0, null());
    let custom_label = if trigger.menu_id() == ID_TRIG_CUSTOM {
        wide(&format!("Custom: {}", trigger.describe()))
    } else {
        wide("Set custom trigger\u{2026}")
    };
    AppendMenuW(sub, MF_STRING, ID_TRIG_CUSTOM, custom_label.as_ptr());
    CheckMenuRadioItem(
        sub,
        ID_TRIG_MIDDLE as u32,
        ID_TRIG_CUSTOM as u32,
        trigger.menu_id() as u32,
        MF_BYCOMMAND,
    );
    AppendMenuW(menu, MF_POPUP, sub as usize, wide("Trigger").as_ptr());
    let mut startup_flags = MF_STRING;
    if startup_enabled() {
        startup_flags |= MF_CHECKED;
    }
    AppendMenuW(menu, startup_flags, ID_STARTUP, wide("Launch at startup").as_ptr());
    let mut persist_flags = MF_STRING;
    if persist {
        persist_flags |= MF_CHECKED;
    }
    AppendMenuW(menu, persist_flags, ID_PERSIST, wide("Remember history").as_ptr());
    let mut autocopy_flags = MF_STRING;
    if auto_copy {
        autocopy_flags |= MF_CHECKED;
    }
    AppendMenuW(menu, autocopy_flags, ID_AUTOCOPY, wide("Auto-copy on highlight").as_ptr());
    // Theme submenu, radio-checked on the active theme.
    let theme_sub = CreatePopupMenu();
    for (i, (name, _)) in THEMES.iter().enumerate() {
        AppendMenuW(theme_sub, MF_STRING, ID_THEME_BASE + i, wide(name).as_ptr());
    }
    CheckMenuRadioItem(
        theme_sub,
        ID_THEME_BASE as u32,
        (ID_THEME_BASE + THEMES.len() - 1) as u32,
        (ID_THEME_BASE + theme_idx) as u32,
        MF_BYCOMMAND,
    );
    AppendMenuW(menu, MF_POPUP, theme_sub as usize, wide("Theme").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());

    let pause_label = if paused { wide("Resume capture") } else { wide("Pause capture") };
    AppendMenuW(menu, MF_STRING, ID_PAUSE, pause_label.as_ptr());
    AppendMenuW(menu, MF_STRING, ID_CLEAR, wide("Clear history").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let about = format!("About ClipStack v{}", env!("CARGO_PKG_VERSION"));
    AppendMenuW(menu, MF_STRING, ID_ABOUT, wide(&about).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit ClipStack").as_ptr());
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

/// Realize the embedded icon into an HICON at the requested size, falling back
/// to the stock application icon if anything goes wrong.
unsafe fn load_app_icon(cx: i32, cy: i32) -> HICON {
    let offset = LookupIconIdFromDirectoryEx(ICON_BYTES.as_ptr(), 1, cx, cy, LR_DEFAULTCOLOR);
    if offset == 0 {
        return LoadIconW(null_mut(), IDI_APPLICATION);
    }
    let bits = ICON_BYTES.as_ptr().add(offset as usize);
    let len = (ICON_BYTES.len() - offset as usize) as u32;
    let hicon = CreateIconFromResourceEx(bits, len, 1, 0x0003_0000, cx, cy, LR_DEFAULTCOLOR);
    if hicon.is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        hicon
    }
}

unsafe fn add_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    let cx = GetSystemMetrics(SM_CXSMICON);
    let cy = GetSystemMetrics(SM_CYSMICON);
    nid.hIcon = load_app_icon(cx, cy);
    let desc = app().trigger.describe();
    set_tip(&mut nid, &desc);
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn cleanup(hwnd: HWND) {
    let mut a = app();
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    Shell_NotifyIconW(NIM_DELETE, &nid);
    KillTimer(hwnd, TIMER_CLIP);
    if a.hook != 0 {
        UnhookWindowsHookEx(a.hook as _);
        a.hook = 0;
    }
    if a.hotkey_active {
        UnregisterHotKey(hwnd, ID_HOTKEY);
        a.hotkey_active = false;
    }
    if !a.font.is_null() {
        DeleteObject(a.font as _);
        a.font = null_mut();
    }
    // If the user opted in, persist the latest history before wiping memory.
    if a.persist && a.history_dirty {
        save_history(&a);
        a.history_dirty = false;
    }
    // Wipe in-memory secrets/clips on exit. By default history never touches
    // disk; only the DPAPI-encrypted pins (and the opt-in history file) persist.
    a.history.iter_mut().for_each(scrub_clip);
    a.history.clear();
    for p in a.pins.iter_mut() {
        scrub_pin(p);
    }
    if let Some(mut ed) = a.edit.take() {
        scrub_string(&mut ed.secret);
    }
}

/// Append a line to %APPDATA%\ClipStack\crash.log. With panic=abort the process
/// dies silently, so the panic hook routes the panic message and its file:line
/// here, turning an intermittent crash into a readable breadcrumb.
fn log_crash(detail: &str) {
    use std::io::Write;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let line = format!("[{secs}] v{}: {detail}\n", env!("CARGO_PKG_VERSION"));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(appdata_file("crash.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| log_crash(&info.to_string())));
    migrate_data(); // one-time lift from the old per-machine subfolder
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(null());

        let class_name = wide("ClipStackWnd");
        // Popup class: drop shadow, and no background brush (we paint it ourselves).
        {
            let wc = WNDCLASSW {
                style: CS_DROPSHADOW,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };
            RegisterClassW(&wc);
        }

        let pins = load_pins();
        let (trigger, persist, auto_copy, theme_idx) = load_settings();
        set_theme(theme_idx);
        let history = if persist { load_history() } else { Vec::new() };

        *G.0.borrow_mut() = Some(App {
            hwnd: null_mut(),
            hinst,
            hook: 0,
            history,
            pins,
            last_seq: 0,
            poll_misses: 0,
            rows: Vec::new(),
            scroll: 0,
            pin_scroll: 0,
            auto_copy,
            drag_start: None,
            theme_idx,
            arm_delete: -1,
            target: null_mut(),
            paused: false,
            visible: false,
            trigger,
            hotkey_active: false,
            capturing: false,
            about: false,
            toast: None,
            toast_ticks: 0,
            persist,
            history_dirty: false,
            edit: None,
            caret_on: false,
            swallow_up: None,
            hovered: -1,
            tracking_leave: false,
            font: null_mut(),
            item_h: 30,
            sep_h: 11,
            pad: 5,
            width: 460,
            popup_x: 0,
            popup_y: 0,
        });

        let title = wide("ClipStack");
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        app().hwnd = hwnd;

        add_tray(hwnd);
        poll_clip(&mut app()); // seed with whatever's on the clipboard now

        // Install the mouse hook and/or keyboard hotkey for the loaded trigger.
        reconcile_input(&mut app());

        SetTimer(hwnd, TIMER_CLIP, 500, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_unescape_roundtrips() {
        for s in [
            "",
            "plain text",
            "line1\nline2\r\nline3",
            "tab\there",
            "back\\slash",
            "mix\\\n\t\\\\end",
            "trailing backslash\\",
        ] {
            assert_eq!(unescape(&escape(s)), s, "round-trip failed for {s:?}");
        }
    }

    #[test]
    fn escape_emits_no_raw_control_chars() {
        let e = escape("a\nb\tc\rd");
        assert!(!e.contains('\n') && !e.contains('\t') && !e.contains('\r'));
    }

    #[test]
    fn mods_token_parse_roundtrips() {
        for m in [
            0,
            M_CTRL,
            M_SHIFT,
            M_ALT,
            M_WIN,
            M_CTRL | M_SHIFT,
            M_CTRL | M_ALT | M_WIN,
            M_CTRL | M_SHIFT | M_ALT | M_WIN,
        ] {
            assert_eq!(parse_mods(&mods_token(m)), m);
        }
    }

    #[test]
    fn parse_mods_ignores_unknown_chars() {
        assert_eq!(parse_mods("CXSqA"), M_CTRL | M_SHIFT | M_ALT);
    }

    #[test]
    fn make_preview_collapses_whitespace_and_trims() {
        assert_eq!(make_preview("a\n\n\nb"), wide_no_nul("a b"));
        assert_eq!(make_preview("  hello   world  "), wide_no_nul("hello world"));
        assert_eq!(make_preview("tab\tsep"), wide_no_nul("tab sep"));
    }

    #[test]
    fn make_preview_caps_length() {
        assert!(make_preview(&"x".repeat(500)).len() <= 160);
    }

    #[test]
    fn trigger_encode_decode_roundtrips() {
        for t in [
            Trigger::Mouse { btn: Btn::Middle, mods: 0 },
            Trigger::Mouse { btn: Btn::X1, mods: M_CTRL },
            Trigger::Mouse { btn: Btn::X2, mods: M_CTRL | M_SHIFT },
            Trigger::Key { vk: 0x56, mods: M_ALT },
            Trigger::Key { vk: 0x74, mods: M_CTRL | M_WIN },
        ] {
            assert_eq!(decode_trigger(encode_trigger(t)), Some(t));
        }
    }

    #[test]
    fn decode_trigger_zero_is_none() {
        assert!(decode_trigger(0).is_none());
    }

    #[test]
    fn dib_to_rgba_parses_32bit_top_down() {
        // 1x1, 32-bit, top-down (negative height), BGRA pixel (10,20,30,40).
        let mut d = vec![0u8; 40];
        d[0] = 40; // biSize
        d[4] = 1; // biWidth = 1
        d[8..12].copy_from_slice(&(-1i32).to_le_bytes()); // biHeight = -1
        d[12] = 1; // biPlanes
        d[14] = 32; // biBitCount
        d.extend_from_slice(&[10, 20, 30, 40]); // B,G,R,A
        let (w, h, rgba) = dib_to_rgba(&d).unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(rgba, vec![30, 20, 10, 40]); // R,G,B,A
    }

    #[test]
    fn dib_to_rgba_rejects_malformed() {
        assert!(dib_to_rgba(&[]).is_none());
        assert!(dib_to_rgba(&[0u8; 10]).is_none()); // too short for a header
        let mut bad_bpp = vec![0u8; 40];
        bad_bpp[0] = 40;
        bad_bpp[4] = 1;
        bad_bpp[8] = 1;
        bad_bpp[14] = 7; // unsupported bit depth
        assert!(dib_to_rgba(&bad_bpp).is_none());
        // Header claims 1000x1000 but carries no pixel data, bounds check rejects.
        let mut huge = vec![0u8; 40];
        huge[0] = 40;
        huge[4..8].copy_from_slice(&1000i32.to_le_bytes());
        huge[8..12].copy_from_slice(&1000i32.to_le_bytes());
        huge[14] = 32;
        assert!(dib_to_rgba(&huge).is_none());
    }
}
