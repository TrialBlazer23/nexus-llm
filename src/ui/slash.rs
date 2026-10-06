//! Slash-command parser for Hub chat (`/unload`, `/preset`, …).

/// Parsed Hub slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    Unload,
    Preset(String),
    Host(String),
    Context(usize),
    Temp(f32),
    Clear,
    Help,
}

/// Hints shown when the input buffer starts with `/`.
pub const SLASH_HINTS: &[&str] = &[
    "/unload           — unload active local model",
    "/preset <name>    — apply persona (system prompt + temp)",
    "/host <endpoint>  — point chat at an OpenAI-compatible host",
    "/context <n>      — set model context window (tokens)",
    "/temp <f>         — set sampling temperature",
    "/clear            — clear conversation history",
    "/help             — show this list",
];

/// Parse a trimmed input that begins with `/`.
pub fn parse(input: &str) -> Result<SlashCommand, String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Err("not a slash command".to_string());
    }

    let without = trimmed.trim_start_matches('/');
    let mut parts = without.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<&str> = parts.collect();

    match cmd.as_str() {
        "unload" => Ok(SlashCommand::Unload),
        "clear" => Ok(SlashCommand::Clear),
        "help" | "?" => Ok(SlashCommand::Help),
        "preset" => {
            let name = rest.join(" ");
            if name.is_empty() {
                Err("usage: /preset <name>".to_string())
            } else {
                Ok(SlashCommand::Preset(name))
            }
        }
        "host" => {
            let endpoint = rest.join(" ");
            if endpoint.is_empty() {
                Err("usage: /host <endpoint>".to_string())
            } else {
                Ok(SlashCommand::Host(endpoint))
            }
        }
        "context" | "ctx" => {
            let raw = rest.first().copied().unwrap_or("");
            if raw.is_empty() {
                return Err("usage: /context <tokens>".to_string());
            }
            let n: usize = raw
                .parse()
                .map_err(|_| format!("invalid context size '{}'", raw))?;
            if n < 512 {
                return Err("context must be at least 512".to_string());
            }
            Ok(SlashCommand::Context(n))
        }
        "temp" | "temperature" => {
            let raw = rest.first().copied().unwrap_or("");
            if raw.is_empty() {
                return Err("usage: /temp <float>".to_string());
            }
            let t: f32 = raw
                .parse()
                .map_err(|_| format!("invalid temperature '{}'", raw))?;
            if !(0.0..=2.0).contains(&t) {
                return Err("temperature must be between 0.0 and 2.0".to_string());
            }
            Ok(SlashCommand::Temp(t))
        }
        "" => Err("type a command after / — see /help".to_string()),
        other => Err(format!("unknown command '/{}' — try /help", other)),
    }
}

/// Whether the input buffer should show the slash-hint popup.
pub fn should_show_hints(buffer: &str) -> bool {
    let t = buffer.trim_start();
    t == "/" || (t.starts_with('/') && !t.contains(char::is_whitespace) && parse(t).is_err())
}

/// Filter hint lines matching the current partial command.
pub fn matching_hints(buffer: &str) -> Vec<&'static str> {
    let t = buffer.trim_start();
    if !t.starts_with('/') {
        return Vec::new();
    }
    let prefix = t.to_ascii_lowercase();
    SLASH_HINTS
        .iter()
        .copied()
        .filter(|h| {
            let cmd = h.split_whitespace().next().unwrap_or("");
            cmd.starts_with(&prefix) || prefix == "/"
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_commands() {
        assert_eq!(parse("/unload"), Ok(SlashCommand::Unload));
        assert_eq!(parse("/clear"), Ok(SlashCommand::Clear));
        assert_eq!(parse("/help"), Ok(SlashCommand::Help));
        assert_eq!(
            parse("/preset coder"),
            Ok(SlashCommand::Preset("coder".into()))
        );
        assert_eq!(
            parse("/host http://192.168.1.10:8080"),
            Ok(SlashCommand::Host("http://192.168.1.10:8080".into()))
        );
        assert_eq!(parse("/context 8192"), Ok(SlashCommand::Context(8192)));
        assert_eq!(parse("/temp 0.3"), Ok(SlashCommand::Temp(0.3)));
    }

    #[test]
    fn rejects_bad_args() {
        assert!(parse("/preset").is_err());
        assert!(parse("/context 100").is_err());
        assert!(parse("/temp 9.0").is_err());
        assert!(parse("/nope").is_err());
    }

    #[test]
    fn hint_matching() {
        assert!(should_show_hints("/"));
        assert!(should_show_hints("/un"));
        assert!(!should_show_hints("/unload"));
        assert!(!should_show_hints("hello"));
        let hints = matching_hints("/pre");
        assert!(hints.iter().any(|h| h.starts_with("/preset")));
    }
}
