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
    /// Answers typed questions about a state in one forward pass (`/v1/systemone`).
    Decision,
    /// Turns text into speech (`/v1/audio/speech`).
    Speech,
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
            "decision" => Self::Decision,
            "speech" => Self::Speech,
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
    /// A decision model: answers typed questions through `/v1/systemone`, never chats.
    Decision,
    /// A speech model: turns text into audio through `/v1/audio/speech`, never chats.
    Speech,
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
        } else if capabilities.contains(&Capability::Decision)
            && !capabilities.contains(&Capability::Completion)
        {
            Self::Decision
        } else if capabilities.contains(&Capability::Speech)
            && !capabilities.contains(&Capability::Completion)
        {
            Self::Speech
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
