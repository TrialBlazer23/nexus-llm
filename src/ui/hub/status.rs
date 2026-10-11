//! Pure footer activity line and unload target choice.
//!
//! Callers draw the TUI and send control-plane unloads. This module does neither.
//! The local supervisor model stays separate from the chat host and chat model.
//! The footer shows both when they differ.

use uuid::Uuid;

pub struct Activity {
    pub local_model: String,
    pub chat_host: String,
    pub chat_model: String,
}

pub fn format_activity_line(activity: &Activity) -> String {
    let local = activity.local_model.trim();
    let host = activity.chat_host.trim();
    let chat = activity.chat_model.trim();
    let same = host.eq_ignore_ascii_case("local") && chat == local;
    if same {
        format!("local {local}")
    } else {
        format!("local {local} · {host} {chat}")
    }
}

pub enum UnloadChoice {
    Local,
    Peer { endpoint: String },
}

pub fn choose_unload(selected: Uuid, local: Uuid, endpoint: &str) -> UnloadChoice {
    if selected == local {
        UnloadChoice::Local
    } else {
        UnloadChoice::Peer {
            endpoint: endpoint.to_string(),
        }
    }
}

pub struct SessionView {
    pub local_model: String,
    pub chat_host: String,
    pub chat_model: String,
}

pub fn on_local_load(view: &mut SessionView, model: &str) {
    view.local_model = model.to_string();
    view.chat_host = "Local".into();
    view.chat_model = model.to_string();
}

pub fn on_remote_load(view: &mut SessionView, host: &str, model: &str) {
    view.chat_host = host.to_string();
    view.chat_model = model.to_string();
}

pub fn on_local_unload(view: &mut SessionView) {
    view.local_model = "None (Idle)".into();
    if view.chat_host.eq_ignore_ascii_case("local") {
        view.chat_model = "None (Idle)".into();
    }
}

pub fn on_peer_unloaded(view: &mut SessionView, name: &str, success: bool) {
    if success && view.chat_host == name {
        view.chat_host = "Local".into();
        view.chat_model = view.local_model.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn activity(local: &str, host: &str, chat: &str) -> Activity {
        Activity {
            local_model: local.into(),
            chat_host: host.into(),
            chat_model: chat.into(),
        }
    }

    #[test]
    fn footer_collapses_when_chat_is_the_local_model() {
        let line = format_activity_line(&activity("general.gguf", "Local", "general.gguf"));
        assert_eq!(line, "local general.gguf");
    }

    #[test]
    fn footer_keeps_the_front_door_beside_a_burst_peer() {
        let line = format_activity_line(&activity("general.gguf", "phone-a", "coder-7b.gguf"));
        assert_eq!(line, "local general.gguf · phone-a coder-7b.gguf");
    }

    #[test]
    fn footer_shows_idle_local_beside_a_peer() {
        let line = format_activity_line(&activity("None (Idle)", "phone-a", "coder-7b.gguf"));
        assert_eq!(line, "local None (Idle) · phone-a coder-7b.gguf");
    }

    #[test]
    fn same_uuid_unloads_local_supervisor() {
        let id = Uuid::nil();
        assert!(matches!(
            choose_unload(id, id, "http://127.0.0.1:18081"),
            UnloadChoice::Local
        ));
    }

    #[test]
    fn other_uuid_unloads_that_peers_control_endpoint() {
        let local = Uuid::nil();
        let peer = Uuid::from_u128(7);
        match choose_unload(peer, local, "http://127.0.0.1:18081") {
            UnloadChoice::Peer { endpoint } => assert_eq!(endpoint, "http://127.0.0.1:18081"),
            UnloadChoice::Local => panic!("remote peer must not stop the hub supervisor"),
        }
    }

    #[test]
    fn remote_load_keeps_the_local_model() {
        let mut view = SessionView {
            local_model: "general.gguf".into(),
            chat_host: "Local".into(),
            chat_model: "general.gguf".into(),
        };
        on_remote_load(&mut view, "phone-a", "coder-7b.gguf");
        assert_eq!(view.local_model, "general.gguf");
        assert_eq!(view.chat_host, "phone-a");
        assert_eq!(view.chat_model, "coder-7b.gguf");
        assert_eq!(
            format_activity_line(&Activity {
                local_model: view.local_model.clone(),
                chat_host: view.chat_host.clone(),
                chat_model: view.chat_model.clone(),
            }),
            "local general.gguf · phone-a coder-7b.gguf"
        );
    }

    #[test]
    fn peer_unload_restores_chat_to_the_front_door() {
        let mut view = SessionView {
            local_model: "general.gguf".into(),
            chat_host: "phone-a".into(),
            chat_model: "coder-7b.gguf".into(),
        };
        on_peer_unloaded(&mut view, "phone-a", true);
        assert_eq!(view.local_model, "general.gguf");
        assert_eq!(view.chat_host, "Local");
        assert_eq!(view.chat_model, "general.gguf");
        on_peer_unloaded(&mut view, "other", false);
        assert_eq!(view.chat_host, "Local");
    }

    #[test]
    fn local_unload_clears_local_chat_and_leaves_a_burst_target() {
        let mut view = SessionView {
            local_model: "general.gguf".into(),
            chat_host: "phone-a".into(),
            chat_model: "coder-7b.gguf".into(),
        };
        on_local_unload(&mut view);
        assert_eq!(view.local_model, "None (Idle)");
        assert_eq!(view.chat_host, "phone-a");
        assert_eq!(view.chat_model, "coder-7b.gguf");
    }
}
