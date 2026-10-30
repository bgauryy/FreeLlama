//! What an installed model can do, as Ollama reports it, and the coarse inventory type derived from that.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Completion,
    Tools,
    Vision,
    Audio,
    Thinking,
    Embedding,
    Other,
}

impl Capability {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "completion" => Self::Completion,
            "tools" => Self::Tools,
            "vision" => Self::Vision,
            "audio" => Self::Audio,
            "thinking" => Self::Thinking,
            "embedding" => Self::Embedding,
            _ => Self::Other,
        }
    }
}

/// Coarse inventory classification derived from Ollama's additive capabilities.
///
/// This is intentionally descriptive only. Routing must continue to use the full capability set:
/// a multimodal model is still eligible for ordinary text and coding work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    Generative,
    Multimodal,
    EmbeddingOnly,
    #[default]
    Unknown,
}

impl ModelType {
    #[must_use]
    pub fn from_capabilities(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let capabilities = capabilities.into_iter().collect::<BTreeSet<_>>();
        if capabilities.contains(&Capability::Embedding)
            && !capabilities.contains(&Capability::Completion)
        {
            Self::EmbeddingOnly
        } else if capabilities.contains(&Capability::Completion)
            && (capabilities.contains(&Capability::Vision)
                || capabilities.contains(&Capability::Audio))
        {
            Self::Multimodal
        } else if capabilities.contains(&Capability::Completion) {
            Self::Generative
        } else {
            Self::Unknown
        }
    }
}
