//! Two-tier intelligent request routing and orchestrator classification.
//!
//! Tier 1: Deterministic matching based on explicit `@tag` prefixes and keywords.
//!         Operates with zero inference overhead.
//! Tier 2: Small orchestrator model assisted classification for ambiguous prompts.
//!         Emits strict JSON decisions with hard code-level veto and fallback.

use crate::client::{ChatCompletionRequest, ChatMessage, NexusClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum RouterError {
    #[error("No routes available in cluster")]
    NoRoutesAvailable,

    #[error("Client error consulting orchestrator model: {0}")]
    Client(String),

    #[error("Failed to parse orchestrator model output as JSON: {0}")]
    ParseFailed(String),

    #[error("Router vetoed invalid or hallucinated route: {0}")]
    VetoedRoute(String),
}

/// Target service available in the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    pub node_id: Uuid,
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub is_orchestrator: bool,
}

impl RouteTarget {
    pub fn new(
        node_id: Uuid,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        tags: Vec<String>,
    ) -> Self {
        let is_orchestrator = tags.iter().any(|t| t == "orchestrator" || t == "router");
        Self {
            node_id,
            endpoint: endpoint.into(),
            model: model.into(),
            tags,
            is_orchestrator,
        }
    }

    pub fn matches_tag(&self, tag: &str) -> bool {
        let needle = tag.trim().to_lowercase();
        self.tags.iter().any(|t| t.trim().to_lowercase() == needle)
    }
}

/// Decision made by the router for an incoming prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteDecision {
    /// Deterministic match (Tier 1).
    Direct {
        endpoint: String,
        model: String,
        matched_tag: String,
        confidence: u8,
        clean_prompt: String,
    },
    /// Model-assisted classification (Tier 2).
    Orchestrated {
        endpoint: String,
        model: String,
        route: String,
        reason: String,
        rewritten_prompt: Option<String>,
    },
    /// Safe fallback when no specific route matched or orchestrator vetoed.
    Fallback {
        endpoint: String,
        model: String,
        reason: String,
    },
}

impl RouteDecision {
    pub fn endpoint(&self) -> &str {
        match self {
            Self::Direct { endpoint, .. } => endpoint,
            Self::Orchestrated { endpoint, .. } => endpoint,
            Self::Fallback { endpoint, .. } => endpoint,
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Direct { model, .. } => model,
            Self::Orchestrated { model, .. } => model,
            Self::Fallback { model, .. } => model,
        }
    }

    pub fn effective_prompt<'a>(&'a self, original: &'a str) -> &'a str {
        match self {
            Self::Direct { clean_prompt, .. } => clean_prompt,
            Self::Orchestrated {
                rewritten_prompt: Some(rewritten),
                ..
            } => rewritten.as_str(),
            _ => original,
        }
    }
}

/// Strictly typed schema for orchestrator classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrchestratorChoice {
    pub route: String,
    pub reason: String,
    #[serde(default)]
    pub rewritten_prompt: Option<String>,
}

/// The intelligent two-tier router.
#[derive(Debug, Clone, Default)]
pub struct Router {
    pub custom_keywords: std::collections::HashMap<String, Vec<String>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_preset_keywords(mut self, tag: impl Into<String>, keywords: Vec<String>) -> Self {
        self.custom_keywords.insert(tag.into(), keywords);
        self
    }

    pub fn load_presets_dir(&mut self, dir: &std::path::Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
                {
                    if let Ok(preset) = crate::preset::Preset::load_from_file(&path) {
                        for tag in &preset.tags {
                            self.custom_keywords
                                .entry(tag.clone())
                                .or_default()
                                .extend(preset.route_keywords.clone());
                        }
                    }
                }
            }
        }
    }

    /// Strip code blocks or markdown wrapping around JSON output.
    pub fn sanitize_json_response(raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.starts_with("```") {
            let without_start = trimmed.trim_start_matches('`');
            let without_lang = if let Some(stripped) = without_start.strip_prefix("json") {
                stripped
            } else {
                without_start
            };
            let without_end = without_lang.trim_end_matches('`').trim();
            without_end.to_string()
        } else {
            trimmed.to_string()
        }
    }

    /// Extract explicit `@tag` from beginning of prompt, e.g. `@coder write a quicksort`.
    pub fn extract_explicit_tag(prompt: &str) -> Option<(&str, &str)> {
        let trimmed = prompt.trim();
        if let Some(stripped) = trimmed.strip_prefix('@') {
            let mut parts = stripped.splitn(2, char::is_whitespace);
            let tag = parts.next()?.trim();
            let remainder = parts.next().unwrap_or("").trim();
            if !tag.is_empty() {
                return Some((tag, remainder));
            }
        }
        None
    }

    /// Tier 1: Deterministic matching based on explicit tags and keywords.
    pub fn route_deterministic(
        &self,
        prompt: &str,
        routes: &[RouteTarget],
    ) -> Option<RouteDecision> {
        if routes.is_empty() {
            return None;
        }

        // 1. Explicit tag directive: `@coder ...`, `@general ...`, etc.
        if let Some((explicit_tag, remainder)) = Self::extract_explicit_tag(prompt) {
            if let Some(target) = routes.iter().find(|r| r.matches_tag(explicit_tag)) {
                return Some(RouteDecision::Direct {
                    endpoint: target.endpoint.clone(),
                    model: target.model.clone(),
                    matched_tag: explicit_tag.to_string(),
                    confidence: 100,
                    clean_prompt: if remainder.is_empty() {
                        prompt.to_string()
                    } else {
                        remainder.to_string()
                    },
                });
            }
        }

        // 2. Data-driven preset keyword checks
        let lower = prompt.to_lowercase();
        for (tag, keywords) in &self.custom_keywords {
            if routes.iter().any(|r| r.matches_tag(tag)) {
                for kw in keywords {
                    let kw_lower = kw.to_lowercase();
                    if lower.contains(&kw_lower) {
                        if let Some(target) = routes.iter().find(|r| r.matches_tag(tag)) {
                            return Some(RouteDecision::Direct {
                                endpoint: target.endpoint.clone(),
                                model: target.model.clone(),
                                matched_tag: tag.clone(),
                                confidence: 95,
                                clean_prompt: prompt.to_string(),
                            });
                        }
                    }
                }
            }
        }

        // 3. Built-in fallback keywords
        // Coder keywords
        const CODER_KEYWORDS: &[&str] = &[
            "fn ",
            "def ",
            "class ",
            "struct ",
            "impl ",
            "enum ",
            "import ",
            "function",
            "async ",
            "await ",
            "rust",
            "python",
            "javascript",
            "typescript",
            "compile",
            "compiler",
            "debug",
            "refactor",
            "bug",
            "syntax",
            "algorithm",
            "quicksort",
            "binary search",
            "sql",
            "select ",
            "git ",
            "bash",
            "regex",
            "unit test",
            "panic",
            "error[e",
        ];

        if routes.iter().any(|r| r.matches_tag("coder")) {
            for kw in CODER_KEYWORDS {
                if lower.contains(kw) {
                    if let Some(target) = routes.iter().find(|r| r.matches_tag("coder")) {
                        return Some(RouteDecision::Direct {
                            endpoint: target.endpoint.clone(),
                            model: target.model.clone(),
                            matched_tag: "coder".to_string(),
                            confidence: 90,
                            clean_prompt: prompt.to_string(),
                        });
                    }
                }
            }
        }

        // Vision keywords
        const VISION_KEYWORDS: &[&str] = &[
            "image",
            "photo",
            "picture",
            "look at",
            "visual",
            "ocr",
            "screenshot",
        ];
        if routes.iter().any(|r| r.matches_tag("vision")) {
            for kw in VISION_KEYWORDS {
                if lower.contains(kw) {
                    if let Some(target) = routes.iter().find(|r| r.matches_tag("vision")) {
                        return Some(RouteDecision::Direct {
                            endpoint: target.endpoint.clone(),
                            model: target.model.clone(),
                            matched_tag: "vision".to_string(),
                            confidence: 90,
                            clean_prompt: prompt.to_string(),
                        });
                    }
                }
            }
        }

        // Embeddings / search keywords
        const EMBEDDING_KEYWORDS: &[&str] = &["embed", "similarity", "vector", "cosine"];
        if routes.iter().any(|r| r.matches_tag("embeddings")) {
            for kw in EMBEDDING_KEYWORDS {
                if lower.contains(kw) {
                    if let Some(target) = routes.iter().find(|r| r.matches_tag("embeddings")) {
                        return Some(RouteDecision::Direct {
                            endpoint: target.endpoint.clone(),
                            model: target.model.clone(),
                            matched_tag: "embeddings".to_string(),
                            confidence: 85,
                            clean_prompt: prompt.to_string(),
                        });
                    }
                }
            }
        }

        None
    }

    /// Build the strict classification prompt for Tier 2 orchestrator inference.
    pub fn build_orchestrator_prompt(prompt: &str, available_tags: &[String]) -> String {
        let tag_list = available_tags.join(", ");
        format!(
            "You are the Nexus Router Classifier.\n\
            Available routes: [{tag_list}]\n\n\
            Classify the user prompt below into the single most appropriate route tag from the list.\n\
            Respond ONLY with a valid JSON object matching this schema:\n\
            {{\"route\": \"<tag>\", \"reason\": \"<brief explanation>\", \"rewritten_prompt\": null}}\n\
            Do not include any explanation or markdown formatting outside the JSON object.\n\n\
            User Prompt:\n{prompt}"
        )
    }

    /// Tier 2: Consult small orchestrator model, validate JSON, and enforce hard code-level veto.
    pub async fn route_orchestrated(
        &self,
        prompt: &str,
        routes: &[RouteTarget],
        orch_client: &NexusClient,
    ) -> Result<RouteDecision, RouterError> {
        if routes.is_empty() {
            return Err(RouterError::NoRoutesAvailable);
        }

        // Collect all active route tags for whitelist validation
        let mut available_tags: Vec<String> = Vec::new();
        for r in routes {
            for t in &r.tags {
                if !available_tags.contains(t) {
                    available_tags.push(t.clone());
                }
            }
        }

        let system_instruction = Self::build_orchestrator_prompt(prompt, &available_tags);
        let chat_req = ChatCompletionRequest {
            model: "orchestrator".to_string(),
            messages: vec![
                ChatMessage::system(system_instruction),
                ChatMessage::user(prompt),
            ],
            temperature: Some(0.1), // Near deterministic classification
            top_p: Some(0.9),
            max_tokens: Some(150),
            stream: false,
        };

        let raw_output = orch_client
            .complete_chat(chat_req)
            .await
            .map_err(|e| RouterError::Client(e.to_string()))?;

        let sanitized = Self::sanitize_json_response(&raw_output);
        let choice: OrchestratorChoice = serde_json::from_str(&sanitized)
            .map_err(|e| RouterError::ParseFailed(format!("{e}: output was {sanitized}")))?;

        // CODE-LEVEL VETO: Validate that the model-selected route actually exists in our whitelist!
        let chosen_route = choice.route.trim().to_lowercase();
        let target = routes
            .iter()
            .find(|r| r.matches_tag(&chosen_route))
            .ok_or_else(|| {
                warn!(
                    "Orchestrator model hallucinated invalid route '{}' (available: {:?}). Code-level veto triggered.",
                    chosen_route, available_tags
                );
                RouterError::VetoedRoute(chosen_route.clone())
            })?;

        debug!(
            "Orchestrator routed prompt to '{}' ({}) - Reason: {}",
            chosen_route, target.endpoint, choice.reason
        );

        Ok(RouteDecision::Orchestrated {
            endpoint: target.endpoint.clone(),
            model: target.model.clone(),
            route: chosen_route,
            reason: choice.reason,
            rewritten_prompt: choice.rewritten_prompt,
        })
    }

    /// High-level routing entrypoint executing Tier 1 -> Tier 2 -> Fallback.
    pub async fn route(
        &self,
        prompt: &str,
        routes: &[RouteTarget],
        orch_client: Option<&NexusClient>,
    ) -> RouteDecision {
        if routes.is_empty() {
            return RouteDecision::Fallback {
                endpoint: "http://127.0.0.1:8080".to_string(),
                model: "none".to_string(),
                reason: "no routes available in cluster".to_string(),
            };
        }

        // 1. Try Tier 1 deterministic route
        if let Some(decision) = self.route_deterministic(prompt, routes) {
            return decision;
        }

        // 2. Try Tier 2 orchestrator route if available
        if let Some(client) = orch_client {
            match self.route_orchestrated(prompt, routes, client).await {
                Ok(decision) => return decision,
                Err(err) => {
                    warn!(
                        "Tier 2 orchestrator routing skipped/vetoed: {}. Falling back to default.",
                        err
                    );
                }
            }
        }

        // 3. Fallback: Select primary model (e.g. tagged "general" or first active route)
        let fallback_target = routes
            .iter()
            .find(|r| r.matches_tag("general"))
            .unwrap_or(&routes[0]);

        RouteDecision::Fallback {
            endpoint: fallback_target.endpoint.clone(),
            model: fallback_target.model.clone(),
            reason: "no specific keyword matched and orchestrator unavailable or vetoed"
                .to_string(),
        }
    }

    /// Route prompt with optional Retrieval-Augmented Generation (RAG) context injection.
    ///
    /// If the prompt starts with `@rag` or if `retriever` finds relevant knowledge above
    /// threshold, the prompt is augmented with markdown knowledge context before routing.
    /// Returns `(RouteDecision, augmented_prompt)`.
    pub async fn route_with_rag(
        &self,
        prompt: &str,
        routes: &[RouteTarget],
        orch_client: Option<&NexusClient>,
        retriever: Option<&crate::kb::KnowledgeRetriever>,
        limit: usize,
    ) -> (RouteDecision, String) {
        let (is_explicit_rag, clean_prompt) =
            if let Some((tag, remainder)) = Self::extract_explicit_tag(prompt) {
                if tag.eq_ignore_ascii_case("rag") {
                    (true, remainder)
                } else {
                    (false, prompt)
                }
            } else {
                (false, prompt)
            };

        let augmented_prompt = if let Some(ret) = retriever {
            let min_score = if is_explicit_rag { 0.4 } else { 0.7 };
            match ret.format_rag_context(clean_prompt, limit, min_score).await {
                Ok(Some(ctx)) => format!("{ctx}\n---\nUser Request:\n{clean_prompt}"),
                _ => clean_prompt.to_string(),
            }
        } else {
            clean_prompt.to_string()
        };

        let decision = self.route(&augmented_prompt, routes, orch_client).await;
        (decision, augmented_prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_custom_preset_keywords_routing() {
        let router = Router::new().with_preset_keywords(
            "finance",
            vec!["stock".into(), "portfolio".into(), "dividend".into()],
        );
        let routes = vec![
            RouteTarget::new(
                Uuid::new_v4(),
                "http://fin-node:8080",
                "finance-7b",
                vec!["finance".into()],
            ),
            RouteTarget::new(
                Uuid::new_v4(),
                "http://gen-node:8080",
                "general-3b",
                vec!["general".into()],
            ),
        ];

        let decision = router
            .route_deterministic("what is the dividend yield of this asset?", &routes)
            .expect("should match custom keyword dividend");

        match decision {
            RouteDecision::Direct { matched_tag, .. } => {
                assert_eq!(matched_tag, "finance");
            }
            _ => panic!("expected Direct route"),
        }
    }
}
