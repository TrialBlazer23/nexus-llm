pub mod chat;
pub mod cluster_view;
pub mod dashboard;
pub mod hub;
pub mod markdown;
pub mod models;
pub mod models_view;
pub mod session_logger;
pub mod settings_view;
pub mod slash;
pub mod tunnel_view;

use std::sync::Once;

static TUI_PANIC_HOOK: Once = Once::new();

/// Install a panic hook that restores the terminal (raw mode + alternate screen).
/// Safe to call from multiple TUI entry points; only installs once.
pub fn install_tui_panic_hook() {
    TUI_PANIC_HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture,
            );
            original(info);
        }));
    });
}
