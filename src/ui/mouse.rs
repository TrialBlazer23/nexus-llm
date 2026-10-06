//! Mouse hit-test helpers for Hub TUI click / scroll support.

use ratatui::layout::Rect;

/// True if `(x, y)` lies inside `rect` (inclusive of origin, exclusive of far edge).
pub fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(rect.width)
        && y < rect.y.saturating_add(rect.height)
}

/// Map an x coordinate within a horizontal tabs strip to a 0-based tab index.
///
/// Divides the inner width of `tabs_area` evenly across `tab_count` tabs.
/// Returns `None` when the point is outside the strip or `tab_count` is zero.
pub fn tab_index_at(x: u16, y: u16, tabs_area: Rect, tab_count: usize) -> Option<usize> {
    if tab_count == 0 || !point_in_rect(x, y, tabs_area) {
        return None;
    }
    // Account for typical border+padding: usable width is inside the block.
    let inner_x = tabs_area.x.saturating_add(1);
    let inner_w = tabs_area.width.saturating_sub(2).max(1);
    if x < inner_x {
        return Some(0);
    }
    let offset = (x - inner_x) as usize;
    let slot = inner_w as usize / tab_count;
    if slot == 0 {
        return Some(0);
    }
    Some((offset / slot).min(tab_count - 1))
}

/// Map a y coordinate within a vertical list area to a 0-based row index.
///
/// `list_area` should be the bordered list widget rect; the first content row
/// is assumed at `list_area.y + 1` (under the top border).
pub fn list_row_at(y: u16, list_area: Rect, row_count: usize) -> Option<usize> {
    if row_count == 0 || !point_in_rect(list_area.x, y, list_area) {
        return None;
    }
    let top = list_area.y.saturating_add(1);
    if y < top {
        return None;
    }
    let idx = (y - top) as usize;
    if idx >= row_count {
        None
    } else {
        Some(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_in_rect_basic() {
        let r = Rect::new(10, 5, 20, 8);
        assert!(point_in_rect(10, 5, r));
        assert!(point_in_rect(29, 12, r));
        assert!(!point_in_rect(30, 5, r));
        assert!(!point_in_rect(10, 13, r));
    }

    #[test]
    fn tab_index_divides_evenly() {
        let area = Rect::new(0, 0, 42, 3); // inner width 40 → 10 per tab
        assert_eq!(tab_index_at(1, 1, area, 4), Some(0));
        assert_eq!(tab_index_at(11, 1, area, 4), Some(1));
        assert_eq!(tab_index_at(21, 1, area, 4), Some(2));
        assert_eq!(tab_index_at(31, 1, area, 4), Some(3));
        assert_eq!(tab_index_at(5, 10, area, 4), None);
    }

    #[test]
    fn list_row_skips_border() {
        let area = Rect::new(0, 0, 40, 12);
        assert_eq!(list_row_at(0, area, 5), None); // top border
        assert_eq!(list_row_at(1, area, 5), Some(0));
        assert_eq!(list_row_at(3, area, 5), Some(2));
        assert_eq!(list_row_at(6, area, 5), None);
    }
}
