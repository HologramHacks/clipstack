//! Popup geometry: how many rows each section shows, where they sit, and how
//! the keyboard selection moves through them.
//!
//! Everything here is pure integer math over counts and offsets, with no
//! Windows types and no access to the app state, so it can be unit tested
//! directly and shared with a non-Windows front end.

pub const HIST_MAX_VISIBLE: usize = 20; // history rows shown before it scrolls
pub const PIN_MAX_VISIBLE: usize = 20; // pin rows shown before the pin block scrolls
pub const TOTAL_VISIBLE: usize = 28; // combined row budget (the old 20 + 8)
pub const SCROLL_STEP: usize = 3;
/// History rows kept visible when pins are claiming space.
const HIST_RESERVE: usize = 8;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RowKind {
    Sep,
    Hist(usize),
    Pin(usize),
}

#[derive(Debug)]
pub struct VRow {
    pub kind: RowKind,
    pub top: i32,
    pub bottom: i32,
}

/// Row heights, in device pixels at the current DPI.
#[derive(Clone, Copy)]
pub struct Metrics {
    pub item_h: i32,
    pub sep_h: i32,
    pub pad: i32,
}

/// The laid-out popup. `scroll`/`pin_scroll` come back clamped, so the caller
/// stores what was actually used rather than what it asked for.
pub struct Laid {
    pub rows: Vec<VRow>,
    pub vis: usize,
    pub pin_vis: usize,
    pub scroll: usize,
    pub pin_scroll: usize,
}

/// Visible-row split for the popup within a `total` row budget (TOTAL_VISIBLE,
/// or less when the monitor cannot fit that): `(history_rows, pin_rows)`. Pins
/// claim what they need up to their cap, but history always keeps room for up
/// to HIST_RESERVE rows (fewer only when history itself is shorter).
pub fn split_rows(hist: usize, pins: usize, total: usize) -> (usize, usize) {
    let pvis = pins
        .min(PIN_MAX_VISIBLE)
        .min(total.saturating_sub(hist.min(HIST_RESERVE)));
    (hist.min(HIST_MAX_VISIBLE).min(total.saturating_sub(pvis)), pvis)
}

/// Lay out the visible rows for `hist` history clips and `pins` pins.
///
/// Every emitted `Hist(i)` has `i < hist` and every `Pin(j)` has `j < pins`,
/// which the paint path relies on: it indexes both collections directly, and a
/// stale index there is an abort, not a blank row.
pub fn build_rows(
    hist: usize,
    pins: usize,
    budget: usize,
    scroll: usize,
    pin_scroll: usize,
    m: Metrics,
) -> Laid {
    let (vis, pin_vis) = split_rows(hist, pins, budget);
    let scroll = scroll.min(hist.saturating_sub(vis));
    let pin_scroll = pin_scroll.min(pins.saturating_sub(pin_vis));

    let mut rows = Vec::with_capacity(vis + pin_vis + 1);
    let mut y = m.pad;
    for i in scroll..scroll + vis {
        rows.push(VRow { kind: RowKind::Hist(i), top: y, bottom: y + m.item_h });
        y += m.item_h;
    }
    if pin_vis > 0 {
        rows.push(VRow { kind: RowKind::Sep, top: y, bottom: y + m.sep_h });
        y += m.sep_h;
        for j in pin_scroll..pin_scroll + pin_vis {
            rows.push(VRow { kind: RowKind::Pin(j), top: y, bottom: y + m.item_h });
            y += m.item_h;
        }
    }
    Laid { rows, vis, pin_vis, scroll, pin_scroll }
}

/// Total popup height for a laid-out row set.
pub fn rows_height(rows: &[VRow], pad: i32) -> i32 {
    rows.last().map(|r| r.bottom).unwrap_or(pad) + pad
}

/// Index of the selectable row at client-y `y`, if any. Separators are not
/// selectable and read as no row.
pub fn row_at(rows: &[VRow], y: i32) -> Option<usize> {
    rows.iter().position(|r| y >= r.top && y < r.bottom && !matches!(r.kind, RowKind::Sep))
}

/// Pin index of `row`, if that row is a pin. Keeps the armed delete pointed at
/// a pin rather than at a screen position.
pub fn pin_at_row(rows: &[VRow], row: i32) -> Option<usize> {
    match usize::try_from(row).ok().and_then(|r| rows.get(r))?.kind {
        RowKind::Pin(j) => Some(j),
        _ => None,
    }
}

/// One Up/Down step through the visible rows: the new selection plus history
/// and pin scroll deltas (each -1, 0, or 1). At a section edge the section
/// scrolls first (revealing the next item under a fixed selection); only past
/// its true end does the selection cross into the neighboring section.
/// `can_scroll_*` say whether that section has one more item in the direction
/// of travel.
pub fn nav_step(
    rows: &[VRow],
    hovered: i32,
    down: bool,
    can_scroll_hist: bool,
    can_scroll_pins: bool,
) -> (i32, i32, i32) {
    let sel_ok = |i: usize| !matches!(rows[i].kind, RowKind::Sep);
    let cur = match usize::try_from(hovered).ok().filter(|&i| i < rows.len() && sel_ok(i)) {
        Some(i) => i,
        None => {
            // Nothing selected yet: enter the list at the near end.
            let first = if down {
                (0..rows.len()).find(|&i| sel_ok(i))
            } else {
                (0..rows.len()).rev().find(|&i| sel_ok(i))
            };
            return (first.map_or(-1, |i| i as i32), 0, 0);
        }
    };
    let step = |from: usize| {
        if down {
            (from + 1..rows.len()).find(|&i| sel_ok(i))
        } else {
            (0..from).rev().find(|&i| sel_ok(i))
        }
    };
    let in_hist = matches!(rows[cur].kind, RowKind::Hist(_));
    let at_edge = match step(cur) {
        None => true,
        Some(j) => matches!(rows[j].kind, RowKind::Hist(_)) != in_hist,
    };
    if at_edge {
        let d = if down { 1 } else { -1 };
        if in_hist && can_scroll_hist {
            return (cur as i32, d, 0);
        }
        if !in_hist && can_scroll_pins {
            return (cur as i32, 0, d);
        }
    }
    (step(cur).map_or(cur as i32, |j| j as i32), 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { item_h: 28, sep_h: 12, pad: 6 };

    #[test]
    fn split_rows_gives_pins_the_rows_they_earn() {
        let t = TOTAL_VISIBLE;
        // 8 or fewer pins: identical to the old fixed 20/8 layout.
        assert_eq!(split_rows(50, 0, t), (20, 0));
        assert_eq!(split_rows(50, 8, t), (20, 8));
        assert_eq!(split_rows(5, 3, t), (5, 3));
        // More pins: pins grow, history shrinks toward the 28-row budget.
        assert_eq!(split_rows(50, 12, t), (16, 12));
        assert_eq!(split_rows(50, 20, t), (8, 20));
        // Pins past their cap scroll instead of growing further.
        assert_eq!(split_rows(50, 99, t), (8, 20));
        // Short history never pads the budget.
        assert_eq!(split_rows(3, 25, t), (3, 20));
    }

    #[test]
    fn split_rows_respects_a_clamped_budget() {
        // Small monitor: both sections share what fits, history keeps its 8.
        assert_eq!(split_rows(50, 30, 16), (8, 8));
        // Short history cedes its reserve to pins.
        assert_eq!(split_rows(2, 30, 16), (2, 14));
        // No pins: history takes the whole clamped budget.
        assert_eq!(split_rows(50, 0, 10), (10, 0));
        // Degenerate budget never underflows.
        assert_eq!(split_rows(50, 30, 1), (1, 0));
    }

    /// The invariant the paint path depends on: it indexes `history[i]` and
    /// `pins[j]` straight from the row kinds, so an out-of-range index is an
    /// abort rather than a cosmetic glitch. Swept over the whole practical
    /// input space rather than a few hand-picked cases.
    #[test]
    fn build_rows_never_emits_an_out_of_range_index() {
        for hist in [0usize, 1, 7, 20, 50] {
            for pins in [0usize, 1, 8, 20, 99] {
                for budget in [1usize, 5, 16, TOTAL_VISIBLE] {
                    // Ask for far more scroll than is available on purpose.
                    for (scroll, pin_scroll) in [(0, 0), (5, 5), (999, 999)] {
                        let laid = build_rows(hist, pins, budget, scroll, pin_scroll, M);
                        for r in &laid.rows {
                            match r.kind {
                                RowKind::Hist(i) => assert!(
                                    i < hist,
                                    "hist {i} out of range for {hist} clips (budget {budget})"
                                ),
                                RowKind::Pin(j) => assert!(
                                    j < pins,
                                    "pin {j} out of range for {pins} pins (budget {budget})"
                                ),
                                RowKind::Sep => {}
                            }
                        }
                        assert!(laid.vis + laid.pin_vis <= budget, "over the row budget");
                        assert!(laid.scroll + laid.vis <= hist, "scrolled past the history end");
                        assert!(
                            laid.pin_scroll + laid.pin_vis <= pins,
                            "scrolled past the pin end"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn build_rows_stacks_rows_without_gaps_or_overlap() {
        let laid = build_rows(10, 5, TOTAL_VISIBLE, 0, 0, M);
        assert_eq!(laid.rows.first().unwrap().top, M.pad, "first row starts after the padding");
        for pair in laid.rows.windows(2) {
            assert_eq!(pair[0].bottom, pair[1].top, "rows must be contiguous");
        }
        // Exactly one separator, and only when both sections are present.
        assert_eq!(laid.rows.iter().filter(|r| r.kind == RowKind::Sep).count(), 1);
        assert!(build_rows(10, 0, TOTAL_VISIBLE, 0, 0, M)
            .rows
            .iter()
            .all(|r| r.kind != RowKind::Sep));
    }

    /// A separator with no pins under it would be a stray divider.
    #[test]
    fn build_rows_omits_the_separator_when_pins_cannot_fit() {
        let laid = build_rows(50, 30, 1, 0, 0, M);
        assert_eq!(laid.pin_vis, 0);
        assert!(laid.rows.iter().all(|r| r.kind != RowKind::Sep));
    }

    fn nav_rows(h: usize, p: usize) -> Vec<VRow> {
        build_rows(h, p, TOTAL_VISIBLE, 0, 0, M).rows
    }

    #[test]
    fn row_at_finds_rows_and_skips_separators() {
        let rows = nav_rows(3, 2);
        assert_eq!(row_at(&rows, rows[0].top), Some(0));
        assert_eq!(row_at(&rows, rows[0].bottom - 1), Some(0));
        let sep = rows.iter().position(|r| r.kind == RowKind::Sep).unwrap();
        assert_eq!(row_at(&rows, rows[sep].top), None, "separators are not selectable");
        assert_eq!(row_at(&rows, -5), None);
        assert_eq!(row_at(&rows, 100_000), None);
    }

    #[test]
    fn pin_at_row_maps_rows_to_pins() {
        let rows = nav_rows(3, 2);
        assert_eq!(pin_at_row(&rows, 0), None, "a history row is not a pin");
        assert_eq!(pin_at_row(&rows, 4), Some(0));
        assert_eq!(pin_at_row(&rows, 5), Some(1));
        assert_eq!(pin_at_row(&rows, -1), None);
        assert_eq!(pin_at_row(&rows, 99), None);
    }

    #[test]
    fn nav_enters_at_the_near_end() {
        let rows = nav_rows(3, 2);
        assert_eq!(nav_step(&rows, -1, true, false, false), (0, 0, 0)); // Down: first hist
        assert_eq!(nav_step(&rows, -1, false, false, false), (5, 0, 0)); // Up: last pin
    }

    #[test]
    fn nav_steps_within_a_section_and_skips_the_separator() {
        let rows = nav_rows(3, 2);
        assert_eq!(nav_step(&rows, 0, true, false, false), (1, 0, 0));
        assert_eq!(nav_step(&rows, 2, true, false, false), (4, 0, 0)); // hist end -> first pin
        assert_eq!(nav_step(&rows, 4, false, false, false), (2, 0, 0)); // first pin -> hist end
    }

    #[test]
    fn nav_scrolls_a_section_before_leaving_it() {
        let rows = nav_rows(3, 2);
        // More history below: Down at the last hist row scrolls, selection stays.
        assert_eq!(nav_step(&rows, 2, true, true, false), (2, 1, 0));
        // More history above: Up at the first hist row scrolls up.
        assert_eq!(nav_step(&rows, 0, false, true, false), (0, -1, 0));
        // Pins likewise, in both directions.
        assert_eq!(nav_step(&rows, 5, true, false, true), (5, 0, 1));
        assert_eq!(nav_step(&rows, 4, false, false, true), (4, 0, -1));
    }

    #[test]
    fn nav_stops_at_the_true_ends() {
        let rows = nav_rows(3, 2);
        assert_eq!(nav_step(&rows, 5, true, false, false), (5, 0, 0)); // bottom: stay
        assert_eq!(nav_step(&rows, 0, false, false, false), (0, 0, 0)); // top: stay
        let hist_only = nav_rows(2, 0);
        assert_eq!(nav_step(&hist_only, 1, true, false, false), (1, 0, 0));
    }

    #[test]
    fn rows_height_covers_the_last_row_plus_padding() {
        let rows = nav_rows(3, 0);
        assert_eq!(rows_height(&rows, M.pad), rows.last().unwrap().bottom + M.pad);
        assert_eq!(rows_height(&[], M.pad), M.pad * 2, "empty list is just padding");
    }
}
