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
// the label dialog's nested loop) — borrows are scoped accordingly.
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]
#![allow(static_mut_refs)]

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    FrameRect, GetStockObject, InvalidateRect, SelectObject, SetBkMode, SetTextColor, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_GUI_FONT,
    DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL,
    HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber};
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
    hinst: windows_sys::Win32::Foundation::HINSTANCE,
    clipboard: Option<arboard::Clipboard>,
    history: Vec<Clip>,
    pins: Vec<Pin>,
    last_seq: u32,
    rows: Vec<VRow>,
    scroll: usize,
    target: HWND,
    paused: bool,
    visible: bool,
    modal: bool,
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

static mut G: Option<App> = None;

fn app() -> &'static mut App {
    // SAFETY: only called on the single UI thread; callers never hold two live
    // borrows at once.
    unsafe { (*std::ptr::addr_of_mut!(G)).as_mut().unwrap() }
}

// ---- Small helpers --------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_no_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

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

/// Lower-cased base name of the process that currently owns the clipboard,
/// e.g. "rdpclip.exe" for content redirected from the RDP/AVD client machine.
fn clipboard_owner_exe() -> Option<String> {
    unsafe {
        let owner = GetClipboardOwner();
        if owner.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(owner, &mut pid);
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        Some(
            full.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&full)
                .to_lowercase(),
        )
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
    // On this VM (RDP/AVD), clips redirected from the local client machine are
    // owned by rdpclip.exe — ignore them so we only keep clips made in the VM.
    if clipboard_owner_exe().as_deref() == Some("rdpclip.exe") {
        return;
    }
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
    a.item_h = (30.0 * scale) as i32;
    a.sep_h = (11.0 * scale) as i32;
    a.pad = (5.0 * scale) as i32;
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
        InvalidateRect(a.hwnd, null(), 1);
    }
    a.visible = true;
    a.hovered = -1;
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
        InvalidateRect(a.hwnd, null(), 1);
    }
    a.hovered = -1;
}

fn hide_popup(a: &mut App) {
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

    fill_color(hdc, 0, 0, rc.right, rc.bottom, rgb(252, 252, 252));
    let oldf = SelectObject(hdc, a.font as _);
    SetBkMode(hdc, TRANSPARENT as i32);

    let text_right = a.width - a.item_h; // reserve the right column for the ✕
    for (idx, r) in a.rows.iter().enumerate() {
        let hovered = idx as i32 == a.hovered;
        match r.kind {
            RowKind::Sep => {
                let mid = (r.top + r.bottom) / 2;
                fill_color(hdc, a.pad * 2, mid, a.width - a.pad * 2, mid + 1, rgb(208, 208, 208));
            }
            RowKind::Hist(i) => {
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, rgb(0, 120, 215));
                }
                match &a.history[i] {
                    Clip::Text(s) => {
                        SetTextColor(hdc, if hovered { rgb(255, 255, 255) } else { rgb(30, 30, 30) });
                        draw_text_row(hdc, a.pad * 2, r.top, text_right, r.bottom, &make_preview(s));
                    }
                    Clip::Image(ic) => {
                        let dw = draw_thumb(hdc, ic, a.pad * 2, r.top + a.pad, a.item_h - a.pad * 2);
                        let tx = a.pad * 2 + dw + a.pad * 2;
                        SetTextColor(hdc, if hovered { rgb(235, 235, 235) } else { rgb(120, 120, 120) });
                        let label = format!("image  {} \u{00d7} {}", ic.w, ic.h);
                        draw_text_row(hdc, tx, r.top, text_right, r.bottom, &wide_no_nul(&label));
                    }
                }
                if hovered {
                    SetTextColor(hdc, rgb(255, 255, 255));
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
            RowKind::Pin(j) => {
                if hovered {
                    fill_color(hdc, 0, r.top, a.width, r.bottom, rgb(0, 120, 215));
                }
                SetTextColor(hdc, if hovered { rgb(255, 255, 255) } else { rgb(70, 70, 70) });
                let txt = format!("{}    {}", "\u{2022}".repeat(8), a.pins[j].label);
                draw_text_row(hdc, a.pad * 2, r.top, text_right, r.bottom, &wide_no_nul(&txt));
                if hovered {
                    SetTextColor(hdc, rgb(255, 255, 255));
                    draw_x(hdc, text_right, r.top, a.width, r.bottom);
                }
            }
        }
    }

    SelectObject(hdc, oldf);
    let border = CreateSolidBrush(rgb(150, 150, 150));
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

// ---- Label input dialog ---------------------------------------------------

struct DlgData {
    done: bool,
    ok: bool,
    edit: HWND,
}

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DlgData;
    match msg {
        WM_COMMAND => {
            if !data.is_null() {
                match (wp & 0xffff) as i32 {
                    x if x == IDOK as i32 => {
                        (*data).ok = true;
                        (*data).done = true;
                    }
                    x if x == IDCANCEL as i32 => (*data).done = true,
                    _ => {}
                }
            }
            0
        }
        WM_CLOSE => {
            if !data.is_null() {
                (*data).done = true;
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn prompt_text(
    parent: HWND,
    hinst: windows_sys::Win32::Foundation::HINSTANCE,
    title: &str,
    label: &str,
    default: &str,
) -> Option<String> {
    unsafe {
        let mut data = DlgData { done: false, ok: false, edit: null_mut() };
        let (cw, ch) = (380, 158);
        let mut wa: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as _, 0);
        let x = (wa.left + wa.right) / 2 - cw / 2;
        let y = (wa.top + wa.bottom) / 2 - ch / 2;

        let cls = wide("ClipStackInput");
        let t = wide(title);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
            cls.as_ptr(),
            t.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            cw,
            ch,
            parent,
            null_mut(),
            hinst,
            null(),
        );
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut data as *mut _ as isize);

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let mk = |class: &[u16], text: Vec<u16>, style: u32, ex: u32, bx, by, bw, bh, id: usize| {
            let h = CreateWindowExW(
                ex,
                class.as_ptr(),
                text.as_ptr(),
                style,
                bx,
                by,
                bw,
                bh,
                hwnd,
                id as *mut c_void,
                hinst,
                null(),
            );
            SendMessageW(h, WM_SETFONT, font as usize, 1);
            h
        };
        let static_cls = wide("STATIC");
        let edit_cls = wide("EDIT");
        let btn_cls = wide("BUTTON");
        mk(&static_cls, wide(label), WS_CHILD | WS_VISIBLE, 0, 14, 12, cw - 40, 20, 0);
        let h_edit = mk(
            &edit_cls,
            wide(default),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            14,
            38,
            cw - 44,
            26,
            0,
        );
        data.edit = h_edit;
        mk(
            &btn_cls,
            wide("OK"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            0,
            cw - 196,
            82,
            84,
            30,
            IDOK as usize,
        );
        mk(
            &btn_cls,
            wide("Cancel"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            0,
            cw - 102,
            82,
            84,
            30,
            IDCANCEL as usize,
        );

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(h_edit);

        let mut msg: MSG = std::mem::zeroed();
        while !data.done && GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let result = if data.ok {
            let len = GetWindowTextLengthW(h_edit);
            let mut buf = vec![0u16; (len + 1) as usize];
            let n = GetWindowTextW(h_edit, buf.as_mut_ptr(), len + 1);
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        } else {
            None
        };
        DestroyWindow(hwnd);
        result
    }
}

// ---- Low-level mouse hook -------------------------------------------------

unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION {
        let msg = wp as u32;
        let info = &*(lp as *const MSLLHOOKSTRUCT);
        let pt = info.pt;
        let a = app();
        match msg {
            WM_MBUTTONDOWN if !a.visible && !a.paused && !a.modal => {
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
            show_popup(app(), wp as i32, lp as i32);
            0
        }
        WM_APP_HIDE => {
            hide_popup(app());
            0
        }
        WM_APP_SCROLL => {
            let a = app();
            let max_scroll = a.history.len().saturating_sub(a.history.len().min(VISIBLE));
            match wp {
                1 => a.scroll = a.scroll.saturating_sub(SCROLL_STEP),
                2 => a.scroll = (a.scroll + SCROLL_STEP).min(max_scroll),
                _ => {}
            }
            a.hovered = -1;
            rebuild_rows(a);
            InvalidateRect(hwnd, null(), 1);
            0
        }
        WM_TIMER => {
            if wp == TIMER_CLIP {
                poll_clip(app());
            }
            0
        }
        WM_MOUSEMOVE => {
            let a = app();
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
            let row = row_at(a, y).map(|i| i as i32).unwrap_or(-1);
            if row != a.hovered {
                a.hovered = row;
                InvalidateRect(hwnd, null(), 1);
            }
            0
        }
        WM_MOUSELEAVE => {
            let a = app();
            a.hovered = -1;
            a.tracking_leave = false;
            InvalidateRect(hwnd, null(), 1);
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = lo_hi(lp);
            let target = {
                let a = app();
                match row_at(a, y) {
                    Some(idx) if x >= a.width - a.item_h => {
                        // Clicked the ✕ delete affordance on the right edge.
                        delete_row(a, idx);
                        if a.history.is_empty() && a.pins.is_empty() {
                            hide_popup(a);
                        } else {
                            relayout(a);
                        }
                        null_mut()
                    }
                    Some(idx) => commit_row(a, idx),
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
            let (_x, y) = lo_hi(lp);
            let kind = {
                let a = app();
                row_at(a, y).map(|idx| a.rows[idx].kind)
            };
            match kind {
                Some(RowKind::Pin(j)) => {
                    let a = app();
                    a.pins.remove(j);
                    save_pins(a);
                    relayout(a);
                }
                Some(RowKind::Hist(i)) => {
                    let secret = {
                        let a = app();
                        match &a.history[i] {
                            Clip::Text(s) => Some(s.clone()),
                            Clip::Image(_) => None, // can't pin an image as a password
                        }
                    };
                    if let Some(secret) = secret {
                        let (parent, hinst) = {
                            let a = app();
                            a.modal = true;
                            (a.hwnd, a.hinst)
                        };
                        hide_popup(app());
                        let label = prompt_text(parent, hinst, "Pin to ClipStack", "Label for this pin:", "");
                        app().modal = false;
                        if let Some(label) = label {
                            let label = label.trim().to_string();
                            if !label.is_empty() {
                                let a = app();
                                a.pins.push(Pin { label, secret });
                                save_pins(a);
                            }
                        }
                    }
                }
                _ => {}
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
                ID_PAUSE => app().paused = !app().paused,
                ID_CLEAR => {
                    let a = app();
                    a.history.iter_mut().for_each(scrub_clip);
                    a.history.clear();
                    hide_popup(a);
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

unsafe fn add_tray(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
    let tip = wide("ClipStack \u{2014} middle-click for clipboard history");
    let n = tip.len().min(nid.szTip.len());
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn cleanup(hwnd: HWND) {
    let a = app();
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
}

fn register_class(
    hinst: windows_sys::Win32::Foundation::HINSTANCE,
    name: &[u16],
    wndproc_fn: WNDPROC,
    hbr: windows_sys::Win32::Graphics::Gdi::HBRUSH,
) {
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: wndproc_fn,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: null_mut(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: hbr,
        lpszMenuName: null(),
        lpszClassName: name.as_ptr(),
    };
    unsafe { RegisterClassW(&wc) };
}

fn main() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(null());

        let class_name = wide("ClipStackWnd");
        let dlg_class = wide("ClipStackInput");
        // Popup class: no background brush, we paint it ourselves.
        {
            let wc = WNDCLASSW {
                style: 0,
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
        let dlg_bg = CreateSolidBrush(rgb(240, 240, 240));
        register_class(hinst, &dlg_class, Some(dlg_proc), dlg_bg);

        let mut clipboard = arboard::Clipboard::new().ok();
        let pins = load_pins();
        let _ = clipboard.as_mut();

        G = Some(App {
            hwnd: null_mut(),
            hook: 0,
            hinst,
            clipboard,
            history: Vec::new(),
            pins,
            last_seq: 0,
            rows: Vec::new(),
            scroll: 0,
            target: null_mut(),
            paused: false,
            visible: false,
            modal: false,
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
        poll_clip(app()); // seed with whatever's on the clipboard now

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
