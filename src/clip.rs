//! The clipboard data model and the pure transforms over it: decoding what
//! another app put on the clipboard, scaling images for display, and turning
//! clip text into something safe to render or store.
//!
//! No Windows types and no app state, so all of it is unit testable and none
//! of it has to be rewritten for a second platform.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

pub const MAX_HISTORY: usize = 50;
pub const MAX_PINS: usize = 99; // generous cap; the pin block scrolls past its visible window

pub struct ImageClip {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>, // full-resolution RGBA, for pasting back
    pub tw: i32,       // thumbnail width
    pub th: i32,       // thumbnail height
    pub thumb: Vec<u8>, // top-down 32bpp BGRA thumbnail, for drawing
    pub hash: u64,
}

#[derive(Clone)]
pub enum Clip {
    Text(String),
    Image(Rc<ImageClip>),
}

// ---- Image scaling --------------------------------------------------------

/// Box-filter RGBA -> top-down 32bpp BGRA at the given size: every source
/// pixel in a destination pixel's footprint is averaged, which keeps
/// downscaled screenshot text readable where nearest sampling shredded it.
/// Shared by the row thumbnails and the hover preview.
pub fn scale_bgra(w: usize, h: usize, rgba: &[u8], tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th * 4];
    for y in 0..th {
        let sy0 = y * h / th;
        let sy1 = ((y + 1) * h).div_ceil(th).min(h).max(sy0 + 1);
        for x in 0..tw {
            let sx0 = x * w / tw;
            let sx1 = ((x + 1) * w).div_ceil(tw).min(w).max(sx0 + 1);
            let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let si = (sy * w + sx) * 4;
                    r += rgba[si] as u64;
                    g += rgba[si + 1] as u64;
                    b += rgba[si + 2] as u64;
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)) as u64;
            let di = (y * tw + x) * 4;
            out[di] = (b / n) as u8;
            out[di + 1] = (g / n) as u8;
            out[di + 2] = (r / n) as u8;
            out[di + 3] = 255;
        }
    }
    out
}

/// Shrink (never grow) `w x h` to fit inside `max_w x max_h`.
pub fn fit_box(w: usize, h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    let (max_w, max_h) = (max_w.max(1), max_h.max(1));
    if w <= max_w && h <= max_h {
        return (w.max(1), h.max(1));
    }
    let s = (max_w as f32 / w.max(1) as f32).min(max_h as f32 / h.max(1) as f32);
    (((w as f32 * s) as usize).max(1), ((h as f32 * s) as usize).max(1))
}

/// A row-height thumbnail: `(width, height, BGRA pixels)`.
pub fn make_thumb(w: usize, h: usize, rgba: &[u8]) -> (i32, i32, Vec<u8>) {
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
    let out = scale_bgra(w, h, rgba, tw, th);
    (tw as i32, th as i32, out)
}

// ---- Untrusted clipboard image data ---------------------------------------

/// Parse a packed DIB (header + optional masks/palette + pixels) into RGBA.
/// Defensive against malformed clipboard data: every offset is bounds-checked
/// and the dimensions are capped, so a bad DIB yields None, never UB. Handles
/// 24- and 32-bit BI_RGB / BI_BITFIELDS, what real apps put on the clipboard.
pub fn dib_to_rgba(d: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
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
    if bpp == 32 && out.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        for p in out.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
    }
    Some((w, h, out))
}

// ---- Text for display and storage -----------------------------------------

/// Collapse a string into a single-line preview (utf16, no NUL).
pub fn make_preview(s: &str) -> Vec<u16> {
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
    out.trim().encode_utf16().collect()
}

/// A malformed clip (control characters, line/paragraph separators, lone UTF-16
/// surrogates, or an absurd length) can crash DrawTextW deep inside USER32's
/// text engine. Build a safe, capped copy for anything we render in a row. A row
/// only ever shows one ellipsized line, so capping well above the visible width
/// loses nothing.
pub fn safe_row_text(text: &[u16]) -> Vec<u16> {
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

/// Suggested pin label from the clip text: first line, trimmed, capped short
/// enough that a pasted blob or key can't become a label by accident.
pub fn suggest_label(s: &str) -> String {
    let line: String = s.lines().next().unwrap_or("").trim().chars().take(40).collect();
    line.trim_end().to_string()
}

/// Escape a clip for the one-record-per-line storage format.
pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

pub fn unescape(s: &str) -> String {
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

// ---- History -------------------------------------------------------------

/// Best-effort wipe of a String's backing bytes before it is dropped.
/// Volatile so the write cannot be optimized away as a dead store.
pub fn scrub_string(s: &mut String) {
    unsafe {
        for b in s.as_mut_vec() {
            std::ptr::write_volatile(b, 0);
        }
    }
    s.clear();
}

pub fn scrub_clip(c: &mut Clip) {
    match c {
        Clip::Text(s) => scrub_string(s),
        Clip::Image(ic) => {
            if let Some(ic) = Rc::get_mut(ic) {
                ic.rgba.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
                ic.thumb.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
            }
        }
    }
}

/// Push a text clip to the front, promoting an existing copy instead of
/// duplicating it, and drop the oldest clips past MAX_HISTORY.
pub fn add_text(history: &mut Vec<Clip>, t: String) {
    if let Some(pos) = history
        .iter()
        .position(|c| matches!(c, Clip::Text(s) if s == &t))
    {
        let c = history.remove(pos);
        history.insert(0, c);
        return;
    }
    history.insert(0, Clip::Text(t));
    trim_history(history);
}

/// Push an image clip to the front, deduplicated by content hash.
pub fn add_image(history: &mut Vec<Clip>, w: usize, h: usize, rgba: Vec<u8>) {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return;
    }
    let mut hasher = DefaultHasher::new();
    w.hash(&mut hasher);
    h.hash(&mut hasher);
    rgba.hash(&mut hasher);
    let hash = hasher.finish();
    if let Some(pos) = history
        .iter()
        .position(|c| matches!(c, Clip::Image(ic) if ic.hash == hash))
    {
        let c = history.remove(pos);
        history.insert(0, c);
        return;
    }
    let (tw, th, thumb) = make_thumb(w, h, &rgba);
    history.insert(0, Clip::Image(Rc::new(ImageClip { w, h, rgba, tw, th, thumb, hash })));
    trim_history(history);
}

/// Drop and scrub anything past the history cap.
pub fn trim_history(history: &mut Vec<Clip>) {
    while history.len() > MAX_HISTORY {
        if let Some(mut c) = history.pop() {
            scrub_clip(&mut c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(c: &Clip) -> &str {
        match c {
            Clip::Text(s) => s,
            _ => panic!("expected a text clip"),
        }
    }

    /// Build a packed DIB the way an app would hand one to the clipboard.
    fn dib(w: i32, h: i32, bpp: u16, pixels: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes()); // biSize
        d.extend_from_slice(&w.to_le_bytes());
        d.extend_from_slice(&h.to_le_bytes()); // negative = top-down
        d.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        d.extend_from_slice(&bpp.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        d.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        d.extend_from_slice(&[0u8; 16]); // ppm, clrUsed, clrImportant
        d.extend_from_slice(pixels);
        d
    }

    /// Bottom-up 24bpp is what most Windows apps actually put on the clipboard,
    /// and it exercises both the row flip and the DWORD-aligned stride padding.
    #[test]
    fn dib_bottom_up_24bpp_flips_rows_and_honors_stride() {
        // 2x2, 24bpp: each row is 6 bytes of pixels padded to 8.
        // Bottom-up, so the first row in the buffer is the BOTTOM screen row.
        let px = [
            0, 0, 255, /* B G R = red */ 0, 255, 0, /* green */ 0, 0, // padding
            255, 0, 0, /* blue */ 255, 255, 255, /* white */ 0, 0, // padding
        ];
        let (w, h, out) = dib_to_rgba(&dib(2, 2, 24, &px)).expect("valid DIB");
        assert_eq!((w, h), (2, 2));
        // Top screen row must be the LAST row of the buffer: blue, white.
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "top-left should be blue");
        assert_eq!(&out[4..8], &[255, 255, 255, 255], "top-right should be white");
        // Bottom screen row: red, green.
        assert_eq!(&out[8..12], &[255, 0, 0, 255], "bottom-left should be red");
        assert_eq!(&out[12..16], &[0, 255, 0, 255], "bottom-right should be green");
    }

    #[test]
    fn dib_top_down_32bpp_keeps_row_order_and_fills_zero_alpha() {
        // Negative height = top-down. Alpha left at 0, which must read as opaque.
        let px = [0, 0, 255, 0, 0, 255, 0, 0];
        let (w, h, out) = dib_to_rgba(&dib(2, -1, 32, &px)).expect("valid DIB");
        assert_eq!((w, h), (2, 1));
        assert_eq!(&out[0..4], &[255, 0, 0, 255], "zero alpha must become opaque");
        assert_eq!(&out[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn dib_rejects_malformed_headers_instead_of_indexing_past_the_buffer() {
        assert!(dib_to_rgba(&[]).is_none(), "empty");
        assert!(dib_to_rgba(&[0u8; 39]).is_none(), "shorter than a header");
        assert!(dib_to_rgba(&dib(2, 2, 16, &[0u8; 16])).is_none(), "unsupported bpp");
        assert!(dib_to_rgba(&dib(0, 2, 32, &[0u8; 16])).is_none(), "zero width");
        assert!(dib_to_rgba(&dib(2, 0, 32, &[0u8; 16])).is_none(), "zero height");
        // Header promises more pixels than the buffer holds.
        assert!(dib_to_rgba(&dib(64, 64, 32, &[0u8; 16])).is_none(), "truncated pixels");
        // Hostile dimensions past the megapixel cap.
        assert!(dib_to_rgba(&dib(40_000, 40_000, 32, &[0u8; 16])).is_none(), "absurd size");
    }

    #[test]
    fn scale_bgra_box_averages_when_shrinking() {
        // Black + white side by side collapse to one mid-gray BGRA pixel.
        let rgba: [u8; 8] = [0, 0, 0, 255, 255, 255, 255, 255];
        let out = scale_bgra(2, 1, &rgba, 1, 1);
        assert_eq!(&out[..4], &[127, 127, 127, 255]);
        // Downscale 4x4 solid red: stays solid red (B=0, G=0, R=255).
        let red: Vec<u8> = (0..16).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let out = scale_bgra(4, 4, &red, 2, 2);
        assert_eq!(&out[..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn fit_box_shrinks_but_never_grows() {
        assert_eq!(fit_box(100, 50, 800, 600), (100, 50)); // already fits: untouched
        assert_eq!(fit_box(1920, 1080, 800, 900), (800, 450)); // width-bound
        assert_eq!(fit_box(1000, 2000, 800, 900), (450, 900)); // height-bound
        assert_eq!(fit_box(0, 0, 800, 600), (1, 1)); // degenerate input never yields zero
    }

    /// This function exists because a malformed clip crashed DrawTextW.
    #[test]
    fn safe_row_text_neutralizes_what_crashed_the_text_engine() {
        let keep: Vec<u16> = "ok".encode_utf16().collect();
        assert_eq!(safe_row_text(&keep), keep);
        // A valid surrogate pair survives intact.
        let pair: Vec<u16> = "\u{1F600}".encode_utf16().collect();
        assert_eq!(safe_row_text(&pair), pair);
        // Lone surrogates, either half, become the replacement char.
        assert_eq!(safe_row_text(&[0xD800]), vec![0xFFFD]);
        assert_eq!(safe_row_text(&[0xDC00]), vec![0xFFFD]);
        // A high surrogate with a non-low follower is also lone.
        assert_eq!(safe_row_text(&[0xD800, 0x41]), vec![0xFFFD, 0x41]);
        // Control chars and the separators that break the layout engine.
        assert_eq!(safe_row_text(&[0x01, 0x7F, 0x0085, 0x2028, 0x2029]), vec![0x20; 5]);
        // Capped, so an absurd clip cannot be handed to the renderer.
        assert_eq!(safe_row_text(&[0x41; 1000]).len(), 256);
        assert!(safe_row_text(&[]).is_empty());
    }

    #[test]
    fn make_preview_collapses_whitespace_and_trims() {
        let p = |s: &str| String::from_utf16_lossy(&make_preview(s));
        assert_eq!(p("  hello   world  "), "hello world");
        assert_eq!(p("line1\nline2\tline3"), "line1 line2 line3");
        assert_eq!(p("\r\n\t "), "");
        assert!(p(&"x".repeat(500)).chars().count() <= 160);
    }

    #[test]
    fn suggest_label_takes_a_short_first_line() {
        assert_eq!(suggest_label("Aria"), "Aria");
        assert_eq!(suggest_label("  padded  "), "padded");
        assert_eq!(suggest_label("first line\nsecond line"), "first line");
        assert_eq!(suggest_label(""), "");
        assert_eq!(suggest_label("\n\n"), "");
        let long = "word ".repeat(20);
        let s = suggest_label(&long);
        assert!(s.chars().count() <= 40 && !s.ends_with(' '), "got {s:?}");
    }

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
    fn add_text_promotes_a_repeat_instead_of_duplicating_it() {
        let mut h = Vec::new();
        add_text(&mut h, "first".into());
        add_text(&mut h, "second".into());
        add_text(&mut h, "first".into()); // copied again later
        assert_eq!(h.len(), 2, "a repeat must not add a row");
        assert_eq!(text_of(&h[0]), "first", "and it moves to the top");
        assert_eq!(text_of(&h[1]), "second");
    }

    #[test]
    fn history_is_capped_and_drops_the_oldest() {
        let mut h = Vec::new();
        for i in 0..MAX_HISTORY + 10 {
            add_text(&mut h, format!("clip {i}"));
        }
        assert_eq!(h.len(), MAX_HISTORY);
        assert_eq!(text_of(&h[0]), &format!("clip {}", MAX_HISTORY + 9), "newest first");
        let oldest_kept = format!("clip {}", 10);
        assert_eq!(text_of(&h[MAX_HISTORY - 1]), &oldest_kept, "oldest rolled off");
    }

    #[test]
    fn add_image_rejects_buffers_that_do_not_match_their_dimensions() {
        let mut h = Vec::new();
        add_image(&mut h, 2, 2, vec![0u8; 4]); // needs 2*2*4 = 16 bytes
        add_image(&mut h, 0, 5, vec![0u8; 64]);
        assert!(h.is_empty(), "a mismatched buffer must be refused, not indexed");
        add_image(&mut h, 2, 2, vec![7u8; 16]);
        assert_eq!(h.len(), 1);
        add_image(&mut h, 2, 2, vec![7u8; 16]); // identical content
        assert_eq!(h.len(), 1, "identical images dedupe by hash");
    }

    #[test]
    fn scrub_string_zeroes_the_buffer_before_clearing() {
        let mut s = String::from("secret");
        let ptr = s.as_ptr();
        let len = s.len();
        scrub_string(&mut s);
        assert!(s.is_empty());
        // The allocation is still ours (capacity is unchanged by clear).
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert!(bytes.iter().all(|&b| b == 0), "plaintext survived the scrub");
    }
}
