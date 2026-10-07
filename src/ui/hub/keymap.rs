//! Declarative hub keymap — single source of truth for help + routing.

use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubAction {
    Quit,
    TabChat,
    TabModels,
    TabCluster,
    TabSettings,
    TabTunnel,
    TabAgents,
    TabLogs,
    OpenCommandPalette,
    NextTab,
    PrevTab,
    UnloadModel,
    ConfirmHotSwap,
    CancelHotSwap,
    ConfirmTarget,
    CancelModal,
    ModelsNext,
    ModelsPrev,
    ModelsRefresh,
    ModelsEnter,
    ModelsContextInc,
    ModelsContextDec,
    ModelsDownload,
    ModelsTransfer,
    ModelsPush,
    ClusterRefresh,
    Help,
}

#[derive(Debug, Clone, Copy)]
pub struct KeyBinding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub action: HubAction,
    pub label: &'static str,
    pub scope: KeyScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScope {
    Global,
    Models,
    Cluster,
    HotSwap,
    TargetSelect,
}

pub fn global_bindings() -> &'static [KeyBinding] {
    &[
        KeyBinding {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            action: HubAction::Quit,
            label: "Ctrl+C Quit",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabChat,
            label: "F1 Chat",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(2),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabModels,
            label: "F2 Models",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(3),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabCluster,
            label: "F3 Cluster",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(4),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabSettings,
            label: "F4 Settings",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(5),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabTunnel,
            label: "F5 Tunnel",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(6),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabAgents,
            label: "F6 Agents",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::F(7),
            modifiers: KeyModifiers::NONE,
            action: HubAction::TabLogs,
            label: "F7 Logs",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            action: HubAction::OpenCommandPalette,
            label: "Ctrl+P Palette",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            action: HubAction::NextTab,
            label: "Tab next",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::SHIFT,
            action: HubAction::PrevTab,
            label: "Shift+Tab prev",
            scope: KeyScope::Global,
        },
        KeyBinding {
            code: KeyCode::Char('?'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::Help,
            label: "? Help",
            scope: KeyScope::Global,
        },
    ]
}

pub fn models_bindings() -> &'static [KeyBinding] {
    &[
        KeyBinding {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsPrev,
            label: "↑/k Prev model",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsNext,
            label: "↓/j Next model",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsRefresh,
            label: "R Rescan",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsEnter,
            label: "Enter Select target",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::UnloadModel,
            label: "U Unload",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('+'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsContextInc,
            label: "+ Context",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('-'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsContextDec,
            label: "- Context",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsDownload,
            label: "D Download URL",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsTransfer,
            label: "T Pull from peer",
            scope: KeyScope::Models,
        },
        KeyBinding {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ModelsPush,
            label: "S Send to peer",
            scope: KeyScope::Models,
        },
    ]
}

pub fn hot_swap_bindings() -> &'static [KeyBinding] {
    &[
        KeyBinding {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::ConfirmHotSwap,
            label: "Y Confirm",
            scope: KeyScope::HotSwap,
        },
        KeyBinding {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            action: HubAction::ConfirmHotSwap,
            label: "Enter Confirm",
            scope: KeyScope::HotSwap,
        },
        KeyBinding {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::NONE,
            action: HubAction::CancelHotSwap,
            label: "N Cancel",
            scope: KeyScope::HotSwap,
        },
        KeyBinding {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            action: HubAction::CancelHotSwap,
            label: "Esc Cancel",
            scope: KeyScope::HotSwap,
        },
    ]
}

/// Resolve a key to an action within a scope (first match wins).
pub fn resolve(scope: KeyScope, code: KeyCode, modifiers: KeyModifiers) -> Option<HubAction> {
    let tables: &[&[KeyBinding]] = match scope {
        KeyScope::Global => &[global_bindings()],
        KeyScope::Models => &[models_bindings(), global_bindings()],
        KeyScope::HotSwap => &[hot_swap_bindings()],
        KeyScope::TargetSelect => &[],
        KeyScope::Cluster => &[global_bindings()],
    };
    for table in tables {
        for b in *table {
            if b.code == code && modifiers_match(b.modifiers, modifiers) {
                return Some(b.action);
            }
        }
    }
    // Alt+1..4 tab shortcuts
    if modifiers.contains(KeyModifiers::ALT) {
        return match code {
            KeyCode::Char('1') => Some(HubAction::TabChat),
            KeyCode::Char('2') => Some(HubAction::TabModels),
            KeyCode::Char('3') => Some(HubAction::TabCluster),
            KeyCode::Char('4') => Some(HubAction::TabSettings),
            _ => None,
        };
    }
    // Models j/k aliases
    if scope == KeyScope::Models {
        match code {
            KeyCode::Char('k') => return Some(HubAction::ModelsPrev),
            KeyCode::Char('j') => return Some(HubAction::ModelsNext),
            KeyCode::Char('R') => return Some(HubAction::ModelsRefresh),
            KeyCode::Char('U') => return Some(HubAction::UnloadModel),
            KeyCode::Char('=') => return Some(HubAction::ModelsContextInc),
            KeyCode::Char('D') => return Some(HubAction::ModelsDownload),
            KeyCode::Char('T') => return Some(HubAction::ModelsTransfer),
            KeyCode::Char('S') => return Some(HubAction::ModelsPush),
            _ => {}
        }
    }
    None
}

fn modifiers_match(expected: KeyModifiers, actual: KeyModifiers) -> bool {
    if expected.is_empty() {
        !actual.contains(KeyModifiers::CONTROL) && !actual.contains(KeyModifiers::ALT)
    } else {
        actual.contains(expected)
    }
}

/// Help overlay lines derived from the declarative tables.
pub fn help_lines() -> Vec<String> {
    let mut lines = vec!["Nexus-LLM Hub Keys".to_string(), String::new()];
    lines.push("Global:".to_string());
    for b in global_bindings() {
        lines.push(format!("  {}", b.label));
    }
    lines.push(String::new());
    lines.push("Models:".to_string());
    for b in models_bindings() {
        lines.push(format!("  {}", b.label));
    }
    lines.push(String::new());
    lines.push("Hot-swap:".to_string());
    for b in hot_swap_bindings() {
        lines.push(format!("  {}", b.label));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f_keys_resolve_tabs() {
        assert_eq!(
            resolve(KeyScope::Global, KeyCode::F(2), KeyModifiers::NONE),
            Some(HubAction::TabModels)
        );
        assert_eq!(
            resolve(KeyScope::Global, KeyCode::Char('3'), KeyModifiers::ALT),
            Some(HubAction::TabCluster)
        );
    }

    #[test]
    fn hot_swap_y_confirms() {
        assert_eq!(
            resolve(KeyScope::HotSwap, KeyCode::Char('y'), KeyModifiers::NONE),
            Some(HubAction::ConfirmHotSwap)
        );
        assert_eq!(
            resolve(KeyScope::HotSwap, KeyCode::Esc, KeyModifiers::NONE),
            Some(HubAction::CancelHotSwap)
        );
    }

    #[test]
    fn test_models_d_t_s_keys() {
        assert_eq!(
            resolve(KeyScope::Models, KeyCode::Char('d'), KeyModifiers::NONE),
            Some(HubAction::ModelsDownload)
        );
        assert_eq!(
            resolve(KeyScope::Models, KeyCode::Char('t'), KeyModifiers::NONE),
            Some(HubAction::ModelsTransfer)
        );
        assert_eq!(
            resolve(KeyScope::Models, KeyCode::Char('s'), KeyModifiers::NONE),
            Some(HubAction::ModelsPush)
        );
    }
}
