use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token counters an agent reported for a turn or event. Every field is
/// optional: an absent counter is absent data, never a zero.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenUsage {
    pub input: Option<u64>,
    #[serde(default)]
    pub cached_input: Option<u64>,
    #[serde(default)]
    pub cached_write: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
    #[serde(default)]
    pub thought: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
}

/// Extract token counters from a usage object. Field aliases cover the
/// vendor spellings (`input_tokens`, `promptTokens`, `cache_read…`,
/// pi's bare `input`/`cacheRead`); an absent counter stays absent —
/// the adapter never invents a zero.
pub fn from_aliases(value: &Value) -> Option<TokenUsage> {
    let object = value.as_object()?;
    let pick = |names: &[&str]| names.iter().find_map(|name| object.get(*name)?.as_u64());
    let usage = TokenUsage {
        input: pick(&[
            "input",
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
        ]),
        cached_input: pick(&[
            "cacheRead",
            "cached_input_tokens",
            "cachedInputTokens",
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "cache_read_tokens",
        ]),
        cached_write: pick(&[
            "cacheWrite",
            "cached_write_tokens",
            "cachedWriteTokens",
            "cache_write_tokens",
        ]),
        output: pick(&[
            "output",
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
        ]),
        thought: pick(&["thought_tokens", "thoughtTokens", "reasoning_tokens"]),
        total: pick(&["total_tokens", "totalTokens"]),
    };
    (usage.input.is_some()
        || usage.cached_input.is_some()
        || usage.cached_write.is_some()
        || usage.output.is_some()
        || usage.thought.is_some()
        || usage.total.is_some())
    .then_some(usage)
}

pub fn from_opencode_tokens(value: &Value) -> Option<TokenUsage> {
    value.as_object()?;
    Some(TokenUsage {
        input: value["input"].as_u64(),
        cached_input: value["cache"]["read"].as_u64(),
        cached_write: value["cache"]["write"].as_u64(),
        output: value["output"].as_u64(),
        thought: value["reasoning"].as_u64(),
        total: None,
    })
}
