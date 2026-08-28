use crate::{config::ModelConfig, runtime::ReasoningEffort};

const BASIC_REASONING: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const QWEN_38_REASONING: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::XHigh,
];
const GPT_54_REASONING: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const GPT_56_REASONING: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];
const GPT_5_REASONING: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const OPENAI_MODEL_PRIORITY: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.6",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5.3-codex",
    "gpt-5.2",
    "gpt-5.1",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-4.1",
    "gpt-4.1-mini",
    "gpt-4o",
    "gpt-4o-mini",
];

pub fn model_config(id: String) -> ModelConfig {
    match known_defaults(&id) {
        Some(defaults) => defaults.into_config(id),
        None => ModelConfig {
            name: id.clone(),
            provider: String::new(),
            id,
            max_context_tokens: 32_768,
            temperature: Some(1.0),
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            vision: false,
        },
    }
}

pub fn supports_chat_completions(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let id = id.rsplit('/').next().unwrap_or(&id);
    ![
        "babbage",
        "computer-use",
        "dall-e",
        "davinci",
        "gpt-audio",
        "gpt-image",
        "gpt-realtime",
        "omni-moderation",
        "sora",
        "text-embedding",
        "text-moderation",
        "tts",
        "whisper",
    ]
    .iter()
    .any(|prefix| id.starts_with(prefix))
        && !id.contains("deep-research")
        && !id.contains("search-preview")
        && !id.contains("transcribe")
        && !((id.starts_with("gpt-5") || id.starts_with('o')) && id.contains("-pro"))
}

pub fn prioritize_openai_models(models: &mut [ModelConfig]) {
    models.sort_by_key(|model| {
        OPENAI_MODEL_PRIORITY
            .iter()
            .position(|id| *id == model.id)
            .unwrap_or(usize::MAX)
    });
}

struct Defaults {
    max_context_tokens: u64,
    temperature: Option<f32>,
    reasoning_effort: Option<ReasoningEffort>,
    reasoning_efforts: &'static [ReasoningEffort],
    vision: bool,
}

impl Defaults {
    fn into_config(self, id: String) -> ModelConfig {
        ModelConfig {
            name: id.clone(),
            provider: String::new(),
            id,
            max_context_tokens: self.max_context_tokens,
            temperature: self.temperature,
            reasoning_effort: self.reasoning_effort,
            reasoning_efforts: self.reasoning_efforts.to_vec(),
            vision: self.vision,
        }
    }
}

fn known_defaults(id: &str) -> Option<Defaults> {
    let id = id.to_ascii_lowercase();
    let family = id.rsplit('/').next().unwrap_or(&id);
    let defaults = if id.contains("qwen3.8") {
        Defaults {
            max_context_tokens: 262_144,
            temperature: Some(1.0),
            reasoning_effort: Some(ReasoningEffort::XHigh),
            reasoning_efforts: QWEN_38_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-5.6") {
        Defaults {
            max_context_tokens: 1_050_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: GPT_56_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-5.5") {
        Defaults {
            max_context_tokens: 1_050_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: GPT_54_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-5.4") {
        Defaults {
            max_context_tokens: if id.contains("mini") || id.contains("nano") {
                400_000
            } else {
                1_050_000
            },
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::None),
            reasoning_efforts: GPT_54_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-5.2") {
        Defaults {
            max_context_tokens: 400_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::None),
            reasoning_efforts: GPT_54_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-5.1") {
        Defaults {
            max_context_tokens: 400_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::None),
            reasoning_efforts: &GPT_54_REASONING[..4],
            vision: true,
        }
    } else if id.contains("gpt-5") {
        Defaults {
            max_context_tokens: 400_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: GPT_5_REASONING,
            vision: true,
        }
    } else if id.contains("gpt-4.1") {
        Defaults {
            max_context_tokens: 1_047_576,
            temperature: Some(1.0),
            reasoning_effort: None,
            reasoning_efforts: &[],
            vision: true,
        }
    } else if id.contains("gpt-4o") {
        Defaults {
            max_context_tokens: 128_000,
            temperature: Some(1.0),
            reasoning_effort: None,
            reasoning_efforts: &[],
            vision: true,
        }
    } else if id.contains("gpt-oss") {
        Defaults {
            max_context_tokens: 131_072,
            temperature: Some(1.0),
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: BASIC_REASONING,
            vision: false,
        }
    } else if family.starts_with("o1") || family.starts_with("o3") || family.starts_with("o4") {
        Defaults {
            max_context_tokens: 200_000,
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: BASIC_REASONING,
            vision: true,
        }
    } else {
        return None;
    };
    Some(defaults)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_38_uses_official_thinking_defaults() {
        let model = model_config("Qwen/Qwen3.8-27B".into());
        assert_eq!(model.max_context_tokens, 262_144);
        assert_eq!(model.temperature, Some(1.0));
        assert_eq!(model.reasoning_effort, Some(ReasoningEffort::XHigh));
        assert_eq!(model.reasoning_efforts, QWEN_38_REASONING);
        assert!(model.vision);
    }

    #[test]
    fn filters_known_non_chat_models() {
        assert!(!supports_chat_completions("text-embedding-3-small"));
        assert!(!supports_chat_completions("gpt-image-1"));
        assert!(!supports_chat_completions("gpt-5.5-pro"));
        assert!(supports_chat_completions("local/unknown-chat-model"));
    }

    #[test]
    fn new_openai_models_do_not_send_temperature_with_reasoning() {
        let model = model_config("gpt-5.6-sol".into());
        assert_eq!(model.temperature, None);
        assert_eq!(model.reasoning_effort, Some(ReasoningEffort::Medium));
        assert!(model.reasoning_efforts.contains(&ReasoningEffort::Max));
    }

    #[test]
    fn latest_openai_models_are_pinned_above_the_rest() {
        let mut models = [
            model_config("old-model".into()),
            model_config("gpt-5.4-mini".into()),
            model_config("gpt-5.6-sol".into()),
        ];

        prioritize_openai_models(&mut models);

        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[1].id, "gpt-5.4-mini");
        assert_eq!(models[2].id, "old-model");
    }
}
