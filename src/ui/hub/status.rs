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
}
