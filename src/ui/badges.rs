//! Standardized, accessible status badges (ORCHESTRATOR_PLAN.md §3.6).
//!
//! Provides consistent bracketed indicators (e.g. `[OK]`, `[RPC]`, `[OOM]`) paired
//! with semantic Ratatui styles. Bracketed labels guarantee readability on monochrome
//! terminals and for users with color vision deficiencies.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
    widgets::Cell,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    pub label: &'static str,
    pub fg: Color,
    pub bold: bool,
}

impl Badge {
    pub const fn new(label: &'static str, fg: Color, bold: bool) -> Self {
        Self { label, fg, bold }
    }

    pub fn style(&self) -> Style {
        let mut style = Style::default().fg(self.fg);
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    }

    pub fn span(&self) -> Span<'static> {
        Span::styled(self.label, self.style())
    }

    pub fn cell(&self) -> Cell<'static> {
        Cell::from(self.span())
    }
}

pub const BADGE_OK: Badge = Badge::new("[OK]", Color::Green, true);
pub const BADGE_WARN: Badge = Badge::new("[WARN]", Color::Yellow, true);
pub const BADGE_FAIL: Badge = Badge::new("[FAIL]", Color::Red, true);
pub const BADGE_READY: Badge = Badge::new("[READY]", Color::Green, true);
pub const BADGE_BUSY: Badge = Badge::new("[BUSY]", Color::Yellow, true);
pub const BADGE_OFFLINE: Badge = Badge::new("[OFFLINE]", Color::DarkGray, false);
pub const BADGE_RPC: Badge = Badge::new("[RPC]", Color::Cyan, true);
pub const BADGE_LOCAL: Badge = Badge::new("[LOCAL]", Color::Blue, true);
pub const BADGE_REMOTE: Badge = Badge::new("[REMOTE]", Color::Magenta, true);
pub const BADGE_DISTRIBUTED: Badge = Badge::new("[DISTRIB]", Color::Yellow, true);
pub const BADGE_OOM: Badge = Badge::new("[OOM]", Color::Red, true);
pub const BADGE_IDLE: Badge = Badge::new("[IDLE]", Color::DarkGray, false);
pub const BADGE_STREAMING: Badge = Badge::new("[STREAM]", Color::Cyan, true);
pub const BADGE_FAST: Badge = Badge::new("[FAST]", Color::Green, true);
pub const BADGE_LAN: Badge = Badge::new("[LAN]", Color::Cyan, false);
pub const BADGE_SLOW: Badge = Badge::new("[SLOW]", Color::Yellow, false);
pub const BADGE_UNPROBED: Badge = Badge::new("[?] UNPROBED", Color::DarkGray, false);

/// Render a link quality indicator with both RTT and throughput.
pub fn format_link_quality(rtt_ms: f32, throughput_bps: f64, unknown: bool) -> Span<'static> {
    if unknown || rtt_ms <= 0.0 {
        return BADGE_UNPROBED.span();
    }

    let mb_s = throughput_bps / (1024.0 * 1024.0);
    let (color, icon) = if rtt_ms < 10.0 {
        (Color::Green, "⚡")
    } else if rtt_ms < 50.0 {
        (Color::Cyan, "📶")
    } else {
        (Color::Yellow, "⏳")
    };

    let text = if mb_s > 0.1 {
        format!("{} {:.1}ms · {:.1}MB/s", icon, rtt_ms, mb_s)
    } else {
        format!("{} {:.1}ms", icon, rtt_ms)
    };

    Span::styled(
        text,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_badge_spans_and_cells() {
        assert_eq!(BADGE_OK.label, "[OK]");
        const { assert!(BADGE_OK.bold) };
        let span = BADGE_OK.span();
        assert_eq!(span.content, "[OK]");

        let unknown = format_link_quality(0.0, 0.0, true);
        assert_eq!(unknown.content, "[?] UNPROBED");

        let fast = format_link_quality(2.5, 50.0 * 1024.0 * 1024.0, false);
        assert!(fast.content.contains("2.5ms"));
        assert!(fast.content.contains("50.0MB/s"));
    }
}
