//! Responsive layout engine for mobile and constrained terminals.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use serde::{Deserialize, Serialize};

/// Display mode preference for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    /// Dynamically choose compact mode when terminal width < 85 or height < 24.
    #[default]
    Auto,
    /// Force compact mobile single-column layout.
    Compact,
    /// Force wide multi-column desktop layout.
    Wide,
}

impl LayoutMode {
    pub fn from_str_mode(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "compact" | "mobile" => Self::Compact,
            "wide" | "desktop" => Self::Wide,
            _ => Self::Auto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Compact => "compact",
            Self::Wide => "wide",
        }
    }

    pub fn is_compact(&self, area: Rect) -> bool {
        match self {
            Self::Compact => true,
            Self::Wide => false,
            Self::Auto => area.width < 85 || area.height < 24,
        }
    }
}

/// Computes a centered rectangle for popups/modals, dynamically adapting
/// to mobile screen constraints so modal contents do not clip.
pub fn responsive_centered_rect(percent_x: u16, percent_y: u16, r: Rect, mode: LayoutMode) -> Rect {
    let (eff_px, eff_py) = if mode.is_compact(r) {
        (percent_x.max(92), percent_y.max(80))
    } else {
        (percent_x, percent_y)
    };

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - eff_py) / 2),
            Constraint::Percentage(eff_py),
            Constraint::Percentage((100 - eff_py) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - eff_px) / 2),
            Constraint::Percentage(eff_px),
            Constraint::Percentage((100 - eff_px) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_mode_from_str() {
        assert_eq!(LayoutMode::from_str_mode("auto"), LayoutMode::Auto);
        assert_eq!(LayoutMode::from_str_mode("compact"), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_str_mode("mobile"), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_str_mode("wide"), LayoutMode::Wide);
        assert_eq!(LayoutMode::from_str_mode("desktop"), LayoutMode::Wide);
        assert_eq!(LayoutMode::from_str_mode("invalid"), LayoutMode::Auto);
    }

    #[test]
    fn test_is_compact_auto() {
        let mode = LayoutMode::Auto;
        // Standard mobile portrait
        assert!(mode.is_compact(Rect::new(0, 0, 75, 25)));
        // Low height
        assert!(mode.is_compact(Rect::new(0, 0, 100, 20)));
        // Desktop widescreen
        assert!(!mode.is_compact(Rect::new(0, 0, 120, 40)));
    }

    #[test]
    fn test_is_compact_overrides() {
        let compact = LayoutMode::Compact;
        assert!(compact.is_compact(Rect::new(0, 0, 160, 50)));

        let wide = LayoutMode::Wide;
        assert!(!wide.is_compact(Rect::new(0, 0, 60, 20)));
    }

    #[test]
    fn test_responsive_centered_rect() {
        let mobile_area = Rect::new(0, 0, 80, 24);
        let rect = responsive_centered_rect(60, 20, mobile_area, LayoutMode::Auto);
        // On mobile, percent_x expands to >= 92% and percent_y to >= 80%
        assert!(rect.width >= 70);
        assert!(rect.height >= 18);

        let desktop_area = Rect::new(0, 0, 120, 50);
        let desk_rect = responsive_centered_rect(60, 40, desktop_area, LayoutMode::Auto);
        assert_eq!(desk_rect.width, 72);
        assert_eq!(desk_rect.height, 20);
    }
}
