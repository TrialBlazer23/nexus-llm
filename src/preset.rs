use crate::client::ChatMessage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use thiserror::Error;
use tracing::debug;

#[derive(Error, Debug)]
pub enum PresetError {
    #[error("I/O error accessing preset file: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML deserialization error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("Preset '{0}' not found in presets directory")]
    NotFound(String),
}

/// Prompt template formats supported by local models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatTemplate {
    ChatML,
    Llama3,
    Alpaca,
}

impl Default for ChatTemplate {
    fn default() -> Self {
        Self::ChatML
    }
}

/// Persona configuration holding generation hyperparameters and system prompts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub template: ChatTemplate,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    pub system_prompt: String,
}

fn default_temperature() -> f32 {
    0.7
}

fn default_top_p() -> f32 {
    0.9
}

fn default_max_tokens() -> usize {
    2048
}

impl Preset {
    /// Load preset configuration from a YAML file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, PresetError> {
        let content = fs::read_to_string(path)?;
        let preset: Preset = serde_yaml::from_str(&content)?;
        debug!("Loaded preset '{}': {:?}", preset.name, preset);
        Ok(preset)
    }

    /// Load preset by name from a directory, checking `<presets_dir>/<name>.yaml` or `.yml`.
    pub fn load_by_name(name: &str, presets_dir: &Path) -> Result<Self, PresetError> {
        let yaml_path = presets_dir.join(format!("{}.yaml", name));
        if yaml_path.is_file() {
            return Self::load_from_file(yaml_path);
        }

        let yml_path = presets_dir.join(format!("{}.yml", name));
        if yml_path.is_file() {
            return Self::load_from_file(yml_path);
        }

        // Check fallback built-ins
        match name {
            "coder" => Ok(Self::coder()),
            "general" => Ok(Self::general()),
            _ => Err(PresetError::NotFound(name.to_string())),
        }
    }

    /// Default built-in "coder" persona.
    pub fn coder() -> Self {
        Self {
            name: "coder".to_string(),
            description: "Systems programming and Rust architecture assistant".to_string(),
            template: ChatTemplate::ChatML,
            temperature: 0.2,
            top_p: 0.95,
            max_tokens: 4096,
            system_prompt: "You are an expert systems programmer and Rust engineer. You write concise, performant, and memory-safe code adhering to strict platform constraints.".to_string(),
        }
    }

    /// Default built-in "general" persona.
    pub fn general() -> Self {
        Self {
            name: "general".to_string(),
            description: "Balanced conversational assistant".to_string(),
            template: ChatTemplate::Llama3,
            temperature: 0.7,
            top_p: 0.90,
            max_tokens: 2048,
            system_prompt: "You are Nexus, a helpful, precise, and concise AI assistant running on a distributed heterogeneous edge cluster.".to_string(),
        }
    }

    /// Format messages into a complete text prompt using the configured chat template.
    pub fn format_prompt(&self, messages: &[ChatMessage]) -> String {
        match self.template {
            ChatTemplate::ChatML => {
                let mut out = String::new();
                out.push_str(&format!("<|im_start|>system\n{}<|im_end|>\n", self.system_prompt.trim()));

                for msg in messages {
                    if msg.role == "system" {
                        continue; // System prompt already rendered
                    }
                    out.push_str(&format!("<|im_start|>{}\n{}<|im_end|>\n", msg.role, msg.content));
                }
                out.push_str("<|im_start|>assistant\n");
                out
            }

            ChatTemplate::Llama3 => {
                let mut out = String::new();
                out.push_str(&format!(
                    "<|start_header_id|>system<|end_header_id|>\n\n{}<|eot_id|>",
                    self.system_prompt.trim()
                ));

                for msg in messages {
                    if msg.role == "system" {
                        continue;
                    }
                    out.push_str(&format!(
                        "<|start_header_id|>{}<|end_header_id|>\n\n{}<|eot_id|>",
                        msg.role, msg.content
                    ));
                }
                out.push_str("<|start_header_id|>assistant<|end_header_id|>\n\n");
                out
            }

            ChatTemplate::Alpaca => {
                let mut out = String::new();
                out.push_str(&format!("### Instruction:\n{}\n\n", self.system_prompt.trim()));

                for msg in messages {
                    if msg.role == "user" {
                        out.push_str(&format!("{}\n\n", msg.content));
                    } else if msg.role == "assistant" {
                        out.push_str(&format!("### Response:\n{}\n\n### Instruction:\n", msg.content));
                    }
                }
                out.push_str("### Response:\n");
                out
            }
        }
    }

    /// Save preset to a destination path.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), PresetError> {
        let yaml_str = serde_yaml::to_string(self)?;
        if let Some(parent) = path.as_ref().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, yaml_str)?;
        Ok(())
    }
}
