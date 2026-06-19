// ClipStack — a tiny clipboard-history popup for Windows.
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
// SetForegroundWindow) — borrows are scoped accordingly.
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

use std::borrow::Cow;
use std::cell::{RefCell, RefMut};
use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    LocalFree, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, FrameRect, GetTextExtentPoint32W, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, SetWindowRgn, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FW_NORMAL, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::WM_MOUSELEAVE;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, SetFocus, TrackMouseEvent, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, TME_LEAVE, TRACKMOUSEEVENT, VK_CONTROL,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const MAX_HISTORY: usize = 50;
const VISIBLE: usize = 15;
const SCROLL_STEP: usize = 3;

// Custom window messages.
const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SHOW: u32 = WM_APP + 2; // wparam=x, lparam=y
const WM_APP_HIDE: u32 = WM_APP + 3;
const WM_APP_SCROLL: u32 = WM_APP + 4; // wparam: 1=up, 2=down

// Tray menu command ids.
const ID_PAUSE: usize = 101;
const ID_CLEAR: usize = 102;
const ID_QUIT: usize = 103;

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
    secret: String,
}

/// In-progress inline pin labeling. While this is `Some`, the right-clicked
/// history row renders as a text field and the popup briefly holds keyboard
/// focus so it can receive WM_CHAR.
struct Edit {
    hist: usize,     // history index being labeled
    secret: String,  // the clip text that will become the pin's secret
    label: Vec<u16>, // label typed so far (UTF-16, no NUL)
    restore: HWND,   // foreground window to hand focus back to when done
}

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
    hook: isize,
    clipboard: Option<arboard::Clipboard>,
    history: Vec<Clip>,
    pins: Vec<Pin>,
    last_seq: u32,
    rows: Vec<VRow>,
    scroll: usize,
    target: HWND,
    paused: bool,
    visible: bool,
    edit: Option<Edit>, // inline pin-labeling in progress
    caret_on: bool,     // caret blink phase while editing
    swallow_mup: bool,
    hovered: i32, // index into `rows`, or -1
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
// SAFETY: ClipStack is single-threaded — the message loop, the mouse hook, and
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
const COL_BG: COLORREF = rgb(0x1c, 0x1f, 0x26); // window background
const COL_TEXT: COLORREF = rgb(0xe8, 0xe8, 0xea); // primary text
const COL_DIM: COLORREF = rgb(0x8a, 0x8f, 0x99); // secondary text (image dims, placeholder)
const COL_HOVER_BG: COLORREF = rgb(0x26, 0x2b, 0x34); // hovered row tint
const COL_ACCENT: COLORREF = rgb(0xff, 0x7a, 0x33); // orange accent (from the icon)
const COL_SEP: COLORREF = rgb(0x2e, 0x33, 0x3d); // separators + frame
const COL_PIN_BULLET: COLORREF = rgb(0x6b, 0x72, 0x80); // masked pin bullets
const COL_DELETE: COLORREF = rgb(0xff, 0x6b, 0x6b); // hovered-row ✕ delete glyph
const COL_FIELD_BG: COLORREF = rgb(0x13, 0x16, 0x1b); // inline label input background
const CORNER_RADIUS: i32 = 12; // rounded-corner diameter for the window region

fn lo_hi(lp: LPARAM) -> (i32, i32) {
    let v = lp as u32;
    ((v & 0xffff) as i16 as i32, ((v >> 16) & 0xffff) as i16 as i32)
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
    a.last_seq = seq;
    let img = a.clipboard.as_mut().and_then(|c| c.get_image().ok());
    if let Some(img) = img {
        add_image(a, img.width, img.height, img.bytes.into_owned());
        return;
    }
    let txt = a.clipboard.as_mut().and_then(|c| c.get_text().ok());
    if let Some(t) = txt {
        if !t.is_empty() {
            add_text(a, t);
        }
    }
}

fn set_clipboard(a: &mut App, clip: &Clip) {
    if let Some(cb) = a.clipboard.as_mut() {
        match clip {
            Clip::Text(s) => {
                let _ = cb.set_text(s.clone());
            }
            Clip::Image(ic) => {
                let _ = cb.set_image(arboard::ImageData {
                    width: ic.w,
                    height: ic.h,
                    bytes: Cow::Owned(ic.rgba.clone()),
                });
            }
        }
    }
    // Don't re-ingest our own write on the next poll.
    a.last_seq = unsafe { GetClipboardSequenceNumber() };
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

/// Per-user pins file: %APPDATA%\ClipStack\pins.dat (created on demand).
fn pins_path() -> std::path::PathBuf {
    let mut p = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    p.push("ClipStack");
    let _ = std::fs::create_dir_all(&p);
    p.push("pins.dat");
    p
}

fn save_pins(a: &App) {
    let mut s = String::new();
    for p in &a.pins {
        s.push_str(&escape(&p.label));
        s.push('\t');
        s.push_str(&escape(&p.secret));
        s.push('\n');
    }
    if let Some(enc) = dpapi_protect(s.as_bytes()) {
        let _ = std::fs::write(pins_path(), enc);
    }
}

fn load_pins() -> Vec<Pin> {
    let mut pins = Vec::new();
    if let Ok(enc) = std::fs::read(pins_path()) {
        if let Some(bytes) = dpapi_unprotect(&enc) {
            if let Ok(text) = String::from_utf8(bytes) {
                for line in text.lines() {
                    if let Some((l, sec)) = line.split_once('\t') {
                        pins.push(Pin {
                            label: unescape(l),
                            secret: unescape(sec),
                        });
                    }
                }
            }
        }
    }
    pins
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
    if !a.pins.is_empty() {
        a.rows.push(VRow { kind: RowKind::Sep, top: y, bottom: y + a.sep_h });
        y += a.sep_h;
        for j in 0..a.pins.len() {
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
    let dpi = unsafe { GetDpiForWindow(a.hwnd) };
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    a.item_h = (34.0 * scale) as i32;
    a.sep_h = (12.0 * scale) as i32;
    a.pad = (6.0 * scale) as i32;
    a.width = (460.0 * scale) as i32;

    if !a.font.is_null() {
        unsafe { DeleteObject(a.font as _) };
    }
    let face = wide("Segoe UI");
    a.font = unsafe {
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
    };

    a.scroll = 0;
    rebuild_rows(a);
    let height = rows_height(a);

    let mut wa: RECT = unsafe { std::mem::zeroed() };
    unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as _, 0) };
    let mut xx = cx;
    let mut yy = cy;
    if xx + a.width > wa.right {
        xx = wa.right - a.width;
    }
    if yy + height > wa.bottom {
        yy = wa.bottom - height;
    }
    if xx < wa.left {
        xx = wa.left;
    }
    if yy < wa.top {
        yy = wa.top;
    }
    a.popup_x = xx;
    a.popup_y = yy;

    unsafe {
        SetWindowPos(a.hwnd, HWND_TOPMOST, xx, yy, a.width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        round_window(a.hwnd, a.width, height);
        InvalidateRect(a.hwnd, null(), 1);
    }
    a.visible = true;
    a.hovered = -1;
}

/// Clip the window to a rounded rectangle. The system takes ownership of the
/// region, so we never free it; replacing it on the next resize is fine.
unsafe fn round_window(hwnd: HWND, w: i32, h: i32) {
    let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, CORNER_RADIUS, CORNER_RADIUS);
    SetWindowRgn(hwnd, rgn, 1);
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

fn hide_popup(a: &mut App) {
    // Abandon any in-progress label edit (scrub the secret, restore the
    // no-activate style we dropped to take focus). Foreground naturally moves
    // to whatever the user clicked.
    if let Some(mut ed) = a.edit.take() {
        scrub_string(&mut ed.secret);
        unsafe {
            let ex = GetWindowLongPtrW(a.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(a.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
        }
    }
    a.caret_on = false;
    unsafe { ShowWindow(a.hwnd, SW_HIDE) };
    a.visible = false;
    a.hovered = -1;
}

unsafe fn fill_color(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, color: COLORREF) {
    let b = CreateSolidBrush(color);
    let r = RECT { left, top, right, bottom };
    FillRect(hdc, &r, b);
    DeleteObject(b as _);
}

unsafe fn draw_text_row(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, text: &[u16]) {
    let mut tr = RECT { left, top, right, bottom };
    DrawTextW(
        hdc,
        text.as_ptr(),
        text.len() as i32,
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

/// Width in pixels of `text` in the font currently selected into `hdc`.
unsafe fn text_width(hdc: HDC, text: &[u16]) -> i32 {
    let mut sz: SIZE = std::mem::zeroed();
    GetTextExtentPoint32W(hdc, text.as_ptr(), text.len() as i32, &mut sz);
    sz.cx
}

/// A 2px vertical caret bar.
unsafe fn draw_caret(hdc: HDC, x: i32, top: i32, bottom: i32) {
    fill_color(hdc, x, top, x + 2, bottom, COL_TEXT);
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
    fill_color(hdc, fx0, fy0, fx1, fy1, COL_FIELD_BG);
    let frame = CreateSolidBrush(COL_ACCENT);
    let fr = RECT { left: fx0, top: fy0, right: fx1, bottom: fy1 };
    FrameRect(hdc, &fr, frame);
    DeleteObject(frame as _);

    let tr = a.width - a.pad * 2;
    let (ctop, cbot) = (fy0 + inset, fy1 - inset);
    if ed.label.is_empty() {
        if a.caret_on {
            draw_caret(hdc, text_left, ctop, cbot);
        }
        SetTextColor(hdc, COL_DIM);
        let hint = wide_no_nul("Type a label  \u{2014}  Enter to pin, Esc to cancel");
        draw_text_row(hdc, text_left + a.pad, r.top, tr, r.bottom, &hint);
    } else {
        SetTextColor(hdc, rgb(255, 255, 255));
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
    bmi.bmiHeader.biCompression = BI_RGB as u32;
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

unsafe fn paint(hwnd: HWND) {
    let a = app();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);

    fill_color(hdc, 0, 0, rc.right, rc.bottom, COL_BG);
    let oldf = SelectObject(hdc, a.font as _);
    SetBkMode(hdc, TRANSPARENT as i32);

    let text_left = a.pad * 2;
    let text_right = a.width - a.item_h; // reserve the right column for the ✕
    let bar_w = (a.pad / 2).max(2); // hovered-row accent bar
    for (idx, r) in a.rows.iter().enumerate() {
        let hovered = idx as i32 == a.hovered;
        match r.kind {
            RowKind::Sep => {
                let mid = (r.top + r.bottom) / 2;
                fill_color(hdc, text_left, mid, a.width - text_left, mid + 1, COL_SEP);
            }
            RowKind::Hist(i) => {
                if a.edit.as_ref().map_or(false, |e| e.hist == i) {
                    paint_edit_row(hdc, &*a, r, text_left);
                    continue;
                }
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, COL_HOVER_BG);
                    fill_color(hdc, 0, r.top, bar_w, r.bottom, COL_ACCENT);
                }
                match &a.history[i] {
                    Clip::Text(s) => {
                        SetTextColor(hdc, if hovered { rgb(255, 255, 255) } else { COL_TEXT });
                        draw_text_row(hdc, text_left, r.top, text_right, r.bottom, &make_preview(s));
                    }
                    Clip::Image(ic) => {
                        let dw = draw_thumb(hdc, ic, text_left, r.top + a.pad, a.item_h - a.pad * 2);
                        let tx = text_left + dw + a.pad * 2;
                        SetTextColor(hdc, if hovered { rgb(220, 220, 225) } else { COL_DIM });
                        let label = format!("image  {} \u{00d7} {}", ic.w, ic.h);
                        draw_text_row(hdc, tx, r.top, text_right, r.bottom, &wide_no_nul(&label));
                    }
                }
                if hovered {
                    SetTextColor(hdc, COL_DELETE);
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
            RowKind::Pin(j) => {
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, COL_HOVER_BG);
                    fill_color(hdc, 0, r.top, bar_w, r.bottom, COL_ACCENT);
                }
                // Dim masked bullets, then the label in bright text after them.
                let bullets = wide_no_nul(&"\u{2022}".repeat(8));
                SetTextColor(hdc, COL_PIN_BULLET);
                draw_text_row(hdc, text_left, r.top, text_right, r.bottom, &bullets);
                let lx = text_left + text_width(hdc, &bullets) + a.pad * 3;
                SetTextColor(hdc, if hovered { rgb(255, 255, 255) } else { COL_TEXT });
                draw_text_row(hdc, lx, r.top, text_right, r.bottom, &wide_no_nul(&a.pins[j].label));
                if hovered {
                    SetTextColor(hdc, COL_DELETE);
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
        }
    }

    SelectObject(hdc, oldf);
    let border = CreateSolidBrush(COL_SEP);
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
        }
        RowKind::Pin(j) => {
            let mut p = a.pins.remove(j);
            scrub_string(&mut p.secret);
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
            hide_popup(a);
            a.target
        }
        RowKind::Pin(j) => {
            let s = a.pins[j].secret.clone();
            set_clipboard(a, &Clip::Text(s));
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

fn send_paste() {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(0x56, false), // 'V'
        key_input(0x56, true),
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
                let secret = std::mem::take(&mut ed.secret);
                a.pins.push(Pin { label, secret });
                save_pins(&*a);
            }
        }
        scrub_string(&mut ed.secret); // no-op if the secret was moved into the pin
        a.caret_on = false;
        relayout(&mut *a); // resize for the (possibly) new pin and repaint
        (a.hwnd, ed.restore)
    };
    // Restore the no-activate style we dropped to grab focus, then hand the
    // keyboard back to wherever the user was typing before.
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
    if !restore.is_null() {
        SetForegroundWindow(restore);
    }
}

// ---- Low-level mouse hook -------------------------------------------------

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION {
        let msg = wp as u32;
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        let pt = info.pt;
        let mut a = app();
        match msg {
            WM_MBUTTONDOWN if !a.visible && !a.paused => {
                a.target = GetForegroundWindow();
                a.swallow_mup = true;
                PostMessageW(a.hwnd, WM_APP_SHOW, pt.x as usize, pt.y as isize);
                return 1;
            }
            WM_MBUTTONUP if a.swallow_mup => {
                a.swallow_mup = false;
                return 1;
            }
            WM_MOUSEWHEEL if a.visible => {
                let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
                let dir = if delta > 0 { 1usize } else { 2usize };
                PostMessageW(a.hwnd, WM_APP_SCROLL, dir, 0);
                return 1;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN if a.visible => {
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
        WM_APP_SCROLL => {
            let mut a = app();
            if a.edit.is_some() {
                return 0; // freeze the list while labeling so indices can't shift
            }
            let max_scroll = a.history.len().saturating_sub(a.history.len().min(VISIBLE));
            match wp {
                1 => a.scroll = a.scroll.saturating_sub(SCROLL_STEP),
                2 => a.scroll = (a.scroll + SCROLL_STEP).min(max_scroll),
                _ => {}
            }
            a.hovered = -1;
            rebuild_rows(&mut *a);
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
                    // Don't ingest new clips while the popup is open — it would
                    // shift the history indices the on-screen rows point at.
                    poll_clip(&mut *a);
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let mut a = app();
            if a.edit.is_some() {
                return 0; // no hover changes while a label field is open
            }
            let (_x, y) = lo_hi(lp);
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
            let row = row_at(&*a, y).map(|i| i as i32).unwrap_or(-1);
            if row != a.hovered {
                a.hovered = row;
                InvalidateRect(hwnd, null(), 1);
            }
            0
        }
        WM_MOUSELEAVE => {
            let mut a = app();
            a.hovered = -1;
            a.tracking_leave = false;
            InvalidateRect(hwnd, null(), 1);
            0
        }
        WM_LBUTTONUP => {
            if app().edit.is_some() {
                return 0; // ignore clicks on rows while labeling; Enter/Esc only
            }
            let (x, y) = lo_hi(lp);
            let target = {
                let mut a = app();
                match row_at(&*a, y) {
                    Some(idx) if x >= a.width - a.item_h => {
                        // Clicked the ✕ delete affordance on the right edge.
                        delete_row(&mut *a, idx);
                        if a.history.is_empty() && a.pins.is_empty() {
                            hide_popup(&mut *a);
                        } else {
                            relayout(&mut *a);
                        }
                        null_mut()
                    }
                    Some(idx) => commit_row(&mut *a, idx),
                    None => null_mut(),
                }
            };
            if !target.is_null() {
                SetForegroundWindow(target);
                std::thread::sleep(Duration::from_millis(40));
                send_paste();
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
                row_at(&*a, y).map(|idx| a.rows[idx].kind)
            };
            match kind {
                Some(RowKind::Pin(j)) => {
                    let mut a = app();
                    let mut p = a.pins.remove(j);
                    scrub_string(&mut p.secret);
                    save_pins(&*a);
                    relayout(&mut *a);
                }
                Some(RowKind::Hist(i)) => {
                    // Begin inline labeling: the row turns into a text field.
                    // Only text clips can be pinned (an image isn't a password).
                    let hwnd2 = {
                        let mut a = app();
                        let secret = match &a.history[i] {
                            Clip::Text(s) => Some(s.clone()),
                            Clip::Image(_) => None,
                        };
                        match secret {
                            Some(secret) => {
                                let restore = a.target;
                                a.edit = Some(Edit { hist: i, secret, label: Vec::new(), restore });
                                a.caret_on = true;
                                Some(a.hwnd)
                            }
                            None => None,
                        }
                    };
                    if let Some(hwnd2) = hwnd2 {
                        // Briefly drop WS_EX_NOACTIVATE and take keyboard focus so
                        // the popup receives WM_CHAR. end_edit/hide_popup restore it.
                        let ex = GetWindowLongPtrW(hwnd2, GWL_EXSTYLE);
                        SetWindowLongPtrW(hwnd2, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE as isize));
                        SetForegroundWindow(hwnd2);
                        SetFocus(hwnd2);
                        InvalidateRect(hwnd2, null(), 1);
                    }
                }
                _ => {}
            }
            0
        }
        WM_KEYDOWN => {
            if app().edit.is_none() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            match wp as u16 {
                0x1B => end_edit(false), // VK_ESCAPE: cancel
                0x0D => {
                    // VK_RETURN: pin only when the trimmed label is non-empty.
                    let ready = app().edit.as_ref().map_or(false, |e| {
                        !String::from_utf16_lossy(&e.label).trim().is_empty()
                    });
                    if ready {
                        end_edit(true);
                    }
                }
                _ => {}
            }
            0
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
                            if (0xDC00..=0xDFFF).contains(&last) {
                                if matches!(e.label.last(), Some(&p) if (0xD800..=0xDBFF).contains(&p)) {
                                    e.label.pop();
                                }
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
        WM_APP_TRAY => {
            let event = (lp as u32) & 0xffff;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU || event == WM_LBUTTONUP {
                show_tray_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            match (wp & 0xffff) as usize {
                ID_PAUSE => {
                    let p = !app().paused;
                    app().paused = p;
                }
                ID_CLEAR => {
                    let mut a = app();
                    a.history.iter_mut().for_each(scrub_clip);
                    a.history.clear();
                    hide_popup(&mut *a);
                }
                ID_QUIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
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
    let paused = app().paused;
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let menu = CreatePopupMenu();
    let pause_label = if paused { wide("Resume middle-click") } else { wide("Pause middle-click") };
    AppendMenuW(menu, MF_STRING, ID_PAUSE, pause_label.as_ptr());
    AppendMenuW(menu, MF_STRING, ID_CLEAR, wide("Clear history").as_ptr());
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
    let tip = wide("ClipStack \u{2014} middle-click for clipboard history");
    let n = tip.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip[..n]);
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
    if !a.font.is_null() {
        DeleteObject(a.font as _);
        a.font = null_mut();
    }
    // Wipe in-memory secrets/clips on exit (history is never written to disk;
    // only the DPAPI-encrypted pins persist by design).
    a.history.iter_mut().for_each(scrub_clip);
    a.history.clear();
    for p in a.pins.iter_mut() {
        scrub_string(&mut p.secret);
    }
    if let Some(mut ed) = a.edit.take() {
        scrub_string(&mut ed.secret);
    }
}

fn main() {
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

        let clipboard = arboard::Clipboard::new().ok();
        let pins = load_pins();

        *G.0.borrow_mut() = Some(App {
            hwnd: null_mut(),
            hook: 0,
            clipboard,
            history: Vec::new(),
            pins,
            last_seq: 0,
            rows: Vec::new(),
            scroll: 0,
            target: null_mut(),
            paused: false,
            visible: false,
            edit: None,
            caret_on: false,
            swallow_mup: false,
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

        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinst, 0);
        app().hook = hook as isize;

        SetTimer(hwnd, TIMER_CLIP, 500, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
