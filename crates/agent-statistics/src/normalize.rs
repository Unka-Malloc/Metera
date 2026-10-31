//! Bounded normalization of explicit token-usage records from local agent history.

use crate::contract::{
    ADDITIVE_CACHE_READ_FIELDS, ADDITIVE_CACHE_WRITE_FIELDS, CounterReading, counter_reading,
};
use serde_json::{Map, Value, json};

const MAX_USAGE_NESTING_DEPTH: usize = 4;

pub fn extract_token_usage(value: &Value) -> Option<Value> {
    let mut usage = UsageFields::default();
    collect_token_usage(value, 0, &mut usage);
    let mut result = usage.to_json()?;
    if let Some(object) = result.as_object_mut() {
        object.extend(request_usage_metadata(value));
    }
    Some(result)
}

/// Copy actual request options alongside counters while excluding conversation
/// content, capability defaults, and arbitrary metadata values.
pub fn request_usage_metadata(value: &Value) -> Map<String, Value> {
    let mut result = Map::new();
    let mut objects = vec![value];
    let mut start = 0;
    for _ in 0..=MAX_USAGE_NESTING_DEPTH {
        let end = objects.len();
        for index in start..end {
            let candidate = objects[index];
            if !result.contains_key("reasoningEffort")
                && let Some(effort) = [
                    "reasoning_effort",
                    "reasoningEffort",
                    "effort",
                    "thinking_level",
                    "thinkingLevel",
                ]
                .into_iter()
                .filter_map(|key| candidate.get(key).and_then(Value::as_str))
                .find(|value| {
                    let value = value.trim();
                    !value.is_empty()
                        && !["default", "unknown", "unspecified"]
                            .into_iter()
                            .any(|missing| value.eq_ignore_ascii_case(missing))
                })
            {
                result.insert("reasoningEffort".to_owned(), json!(effort));
            }
            if !result.contains_key("fast") {
                let fast = ["fast", "fast_mode", "fastMode"]
                    .into_iter()
                    .find_map(|key| candidate.get(key).and_then(Value::as_bool))
                    .or_else(|| {
                        ["service_tier", "serviceTier"]
                            .into_iter()
                            .filter_map(|key| candidate.get(key).and_then(Value::as_str))
                            .find_map(|tier| match tier {
                                "fast" | "priority" => Some(true),
                                "default" => Some(false),
                                _ => None,
                            })
                    });
                if let Some(fast) = fast {
                    result.insert("fast".to_owned(), json!(fast));
                }
            }
            for key in [
                "usage",
                "token_usage",
                "tokenUsage",
                "message",
                "data",
                "payload",
                "request",
                "metadata",
                "options",
                "reasoning",
                "generationConfig",
                "output_config",
                "thinking",
                "collaboration_mode",
                "settings",
            ] {
                if let Some(child) = candidate.get(key).filter(|value| value.is_object()) {
                    objects.push(child);
                }
            }
        }
        start = end;
    }
    result
}

#[derive(Default)]
pub struct UsageFields {
    prompt_tokens: u64,
    cached_input_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    explicit_fields: usize,
    normalized_additive_semantics: bool,
    /// An addition that overflowed poisons the whole record: a saturated
    /// count is a number the record never wrote, so `to_json` refuses it.
    tainted: bool,
}

impl UsageFields {
    /// Checked accumulation into one counter; an overflow marks the record
    /// tainted rather than emitting a saturated count.
    fn add_prompt(&mut self, amount: u64) {
        match self.prompt_tokens.checked_add(amount) {
            Some(sum) => self.prompt_tokens = sum,
            None => self.tainted = true,
        }
    }

    fn add_cached_input(&mut self, amount: u64) {
        match self.cached_input_tokens.checked_add(amount) {
            Some(sum) => self.cached_input_tokens = sum,
            None => self.tainted = true,
        }
    }

    fn add_completion(&mut self, amount: u64) {
        match self.completion_tokens.checked_add(amount) {
            Some(sum) => self.completion_tokens = sum,
            None => self.tainted = true,
        }
    }

    fn add_total(&mut self, amount: u64) {
        match self.total_tokens.checked_add(amount) {
            Some(sum) => self.total_tokens = sum,
            None => self.tainted = true,
        }
    }

    pub fn to_json(&self) -> Option<Value> {
        // A tainted record produced no usage: refusing to emit is the only
        // honest answer where a wrong count would pass for a real one.
        if self.explicit_fields == 0 || self.tainted {
            return None;
        }
        let mut prompt_tokens = self.prompt_tokens;
        let mut completion_tokens = self.completion_tokens;
        let field_total = prompt_tokens.checked_add(completion_tokens)?;
        let total_tokens = if self.normalized_additive_semantics {
            field_total
        } else if self.total_tokens > 0 {
            self.total_tokens
        } else {
            field_total
        };
        if !self.normalized_additive_semantics && field_total != total_tokens {
            if field_total > total_tokens {
                completion_tokens = completion_tokens.min(total_tokens);
                prompt_tokens = total_tokens.saturating_sub(completion_tokens);
            } else if prompt_tokens > 0 {
                prompt_tokens = prompt_tokens.min(total_tokens);
                completion_tokens = total_tokens.saturating_sub(prompt_tokens);
            } else {
                completion_tokens = completion_tokens.min(total_tokens);
                prompt_tokens = total_tokens.saturating_sub(completion_tokens);
            }
        }
        Some(json!({
            "promptTokens": prompt_tokens,
            "cachedInputTokens": self.cached_input_tokens.min(prompt_tokens),
            "completionTokens": completion_tokens,
            "totalTokens": total_tokens,
            "source": "explicit"
        }))
    }
}

pub fn collect_token_usage(value: &Value, depth: usize, usage: &mut UsageFields) {
    if depth > MAX_USAGE_NESTING_DEPTH {
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for key in [
        "usage",
        "tokenUsage",
        "token_usage",
        "usageMetadata",
        "usage_metadata",
        "tokenCount",
        "token_count",
        "responseUsage",
        "response_usage",
        "gen_ai.usage",
        "tokens",
        "message",
        "data",
        "payload",
        "status",
    ] {
        let Some(child) = object.get(key) else {
            continue;
        };
        let mut nested = UsageFields::default();
        collect_token_usage(child, depth + 1, &mut nested);
        if nested.explicit_fields > 0 {
            *usage = nested;
            return;
        }
    }
    let normalized_input_output = object.contains_key("input") && object.contains_key("output");
    // Additive semantics are detected over the same alias inventory the
    // counters fold from: every recognized additive spelling marks the
    // record additive — a spelling one side counted and the other did not
    // would let a pre-cache total reconcile the folded count back down.
    let additive_shape = object.contains_key("inputOther")
        || object.contains_key("input_other")
        || ADDITIVE_CACHE_READ_FIELDS
            .iter()
            .chain(ADDITIVE_CACHE_WRITE_FIELDS)
            .any(|key| object.contains_key(*key));
    usage.normalized_additive_semantics |= (normalized_input_output
        && object.get("cache").and_then(Value::as_object).is_some())
        || additive_shape;
    let base_prompt = token_count_field(
        object,
        &[
            "promptTokens",
            "prompt_tokens",
            "inputTokens",
            "input_tokens",
            "totalInputTokens",
            "total_input_tokens",
            "promptTokenCount",
            "prompt_token_count",
            "inputOther",
            "input_other",
            "input",
        ],
        usage,
    );
    let fields_before_cached = usage.explicit_fields;
    // Only spellings reported *inside* the base count belong here: Anthropic's
    // `cache_read_input_tokens` is not one — Anthropic reports it beside
    // `cache_creation_input_tokens`, outside `input_tokens`, so it folds in
    // the additive read below rather than being capped by the base.
    let mut cached_subset = token_count_field(
        object,
        &[
            "cachedInputTokens",
            "cached_input_tokens",
            "inputCachedTokens",
            "input_cached_tokens",
            "totalCachedTokens",
            "total_cached_tokens",
            "cachedContentTokenCount",
            "cached_content_token_count",
        ],
        usage,
    );
    if usage.explicit_fields == fields_before_cached {
        for key in ["prompt_tokens_details", "input_tokens_details"] {
            let Some(details) = object.get(key).and_then(Value::as_object) else {
                continue;
            };
            match counter_reading(details, &["cached_tokens"]) {
                CounterReading::Counted(cached) => {
                    cached_subset = cached;
                    usage.explicit_fields += 1;
                    break;
                }
                CounterReading::Uncounted => usage.tainted = true,
                CounterReading::Absent => {}
            }
        }
    }
    let cache_read = token_count_field(object, ADDITIVE_CACHE_READ_FIELDS, usage);
    let cache_write = token_count_field(object, ADDITIVE_CACHE_WRITE_FIELDS, usage);
    usage.add_prompt(base_prompt);
    usage.add_prompt(cache_read);
    usage.add_prompt(cache_write);
    usage.add_cached_input(cached_subset.min(base_prompt));
    usage.add_cached_input(cache_read);
    if let Some(cache) = object.get("cache").and_then(Value::as_object) {
        let normalized_cache_read = token_count_field(cache, &["read"], usage);
        let normalized_cache_write = token_count_field(cache, &["write"], usage);
        usage.add_prompt(normalized_cache_read);
        usage.add_prompt(normalized_cache_write);
        usage.add_cached_input(normalized_cache_read);
    }
    if normalized_input_output {
        let count = token_count_field(object, &["reasoning"], usage);
        usage.add_completion(count);
    }
    let count = token_count_field(
        object,
        &[
            "completionTokens",
            "completion_tokens",
            "outputTokens",
            "output_tokens",
            "responseTokens",
            "response_tokens",
            "totalOutputTokens",
            "total_output_tokens",
            "candidatesTokenCount",
            "candidates_token_count",
            "output",
        ],
        usage,
    );
    usage.add_completion(count);
    let count = token_count_field(
        object,
        &[
            "reasoningTokens",
            "reasoning_tokens",
            "thoughtsTokenCount",
            "thoughts_token_count",
            "totalThoughtTokens",
            "total_thought_tokens",
        ],
        usage,
    );
    usage.add_completion(count);
    let count = token_count_field(
        object,
        &["totalToolUseTokens", "total_tool_use_tokens"],
        usage,
    );
    usage.add_completion(count);
    let count = token_total_field(
        object,
        &[
            "totalTokens",
            "total_tokens",
            "totalTokenCount",
            "total_token_count",
            "total",
        ],
        usage,
    );
    usage.add_total(count);
}

fn token_count_field(object: &Map<String, Value>, keys: &[&str], usage: &mut UsageFields) -> u64 {
    match counter_reading(object, keys) {
        CounterReading::Counted(count) => {
            usage.explicit_fields += 1;
            count
        }
        // A counter the record named but never counted is unknown, not
        // zero: folding it as absent would emit a precise partial total,
        // so the record is refused downstream.
        CounterReading::Uncounted => {
            usage.tainted = true;
            0
        }
        CounterReading::Absent => 0,
    }
}

/// The authoritative total reconciles the folded counts but is not one of
/// them: a named-but-uncounted total leaves the fold honest, so it never
/// taints the record.
fn token_total_field(object: &Map<String, Value>, keys: &[&str], usage: &mut UsageFields) -> u64 {
    match counter_reading(object, keys) {
        CounterReading::Counted(count) => {
            usage.explicit_fields += 1;
            count
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_nested_cached_details_are_input_subsets_and_flat_fields_win() {
        for details in ["prompt_tokens_details", "input_tokens_details"] {
            let mut record = json!({"prompt_tokens":12,"completion_tokens":3,"total_tokens":15});
            record[details] = json!({"cached_tokens":4});
            let usage = extract_token_usage(&json!({"usage":record})).unwrap();
            assert_eq!(usage["promptTokens"], 12);
            assert_eq!(usage["cachedInputTokens"], 4);
            assert_eq!(usage["totalTokens"], 15);
            record["cached_input_tokens"] = json!(0);
            assert_eq!(
                extract_token_usage(&record).unwrap()["cachedInputTokens"],
                0
            );
        }
    }

    #[test]
    fn an_overflowing_additive_count_refuses_the_record() {
        // A saturated count is a number the record never wrote: every
        // additive fold is checked, and an overflow refuses the record
        // rather than emitting a clamped total.
        for usage in [
            json!({"prompt_tokens": u64::MAX, "cache_read_input_tokens": 1}),
            json!({"prompt_tokens": u64::MAX, "cache_creation_input_tokens": 1}),
            json!({"prompt_tokens": u64::MAX, "output_tokens": 1}),
        ] {
            assert!(
                extract_token_usage(&json!({"usage": usage})).is_none(),
                "an overflowing additive record is refused: {usage}"
            );
        }
    }

    #[test]
    fn a_named_but_uncounted_counter_refuses_the_record() {
        // `null` or non-numeric is not zero: the record named a counter it
        // never counted, so folding it as absent would emit a precise
        // partial total.
        for usage in [
            json!({"prompt_tokens": 100, "cache_read_input_tokens": null, "completion_tokens": 20, "total_tokens": 120}),
            json!({"prompt_tokens": 100, "cache_creation_input_tokens": "n/a", "completion_tokens": 20}),
            json!({"prompt_tokens": null, "completion_tokens": 20}),
            json!({"prompt_tokens": 100, "completion_tokens": "unknown"}),
            json!({"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": null}}),
        ] {
            assert!(
                extract_token_usage(&json!({"usage": usage})).is_none(),
                "a named-but-uncounted counter refuses the record: {usage}"
            );
        }
    }

    #[test]
    fn an_uncounted_reconciliation_total_still_folds() {
        // The reported total only reconciles counted constituents — it is
        // never folded itself — so a named-but-uncounted total leaves the
        // fold honest.
        let usage = extract_token_usage(
            &json!({"usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": null}}),
        )
        .unwrap();
        assert_eq!(usage["totalTokens"], 120);
    }

    #[test]
    fn usage_normalization_preserves_actual_options_without_copying_content() {
        let usage=extract_token_usage(&json!({"effort":"high", "service_tier":"default", "usage":{"input_tokens":5,"output_tokens":2}, "collaboration_mode":{"settings":{"reasoning_effort":"xhigh","fast":true}},"text":"private-content-canary"})).unwrap();
        assert_eq!(usage["totalTokens"], 7);
        assert_eq!(usage["reasoningEffort"], "high");
        assert_eq!(usage["fast"], false);
        assert!(!usage.to_string().contains("private-content-canary"));
    }

    #[test]
    fn normalized_cache_usage_is_additive_and_bounded_to_explicit_fields() {
        let usage = extract_token_usage(&json!({
            "usage": {
                "input": 10,
                "output": 4,
                "reasoning": 2,
                "cache": {"read": 3, "write": 1}
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 14);
        assert_eq!(usage["cachedInputTokens"], 3);
        assert_eq!(usage["completionTokens"], 6);
        assert_eq!(usage["totalTokens"], 20);
        assert!(extract_token_usage(&json!({"estimated": 99})).is_none());
    }

    #[test]
    fn inconsistent_explicit_total_is_reconciled_without_overcounting() {
        let usage = extract_token_usage(&json!({
            "prompt_tokens": "8",
            "completion_tokens": 7,
            "total_tokens": 10
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 3);
        assert_eq!(usage["completionTokens"], 7);
        assert_eq!(usage["totalTokens"], 10);
    }

    #[test]
    fn gemini_usage_metadata_is_read_without_tokenizing_content() {
        let usage = extract_token_usage(&json!({
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 40,
                "candidatesTokenCount": 12,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 115
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 100);
        assert_eq!(usage["cachedInputTokens"], 40);
        assert_eq!(usage["completionTokens"], 15);
        assert_eq!(usage["totalTokens"], 115);
    }

    #[test]
    fn anthropic_cache_categories_are_additive_to_the_base_input() {
        // The real Anthropic shape: `input_tokens` excludes both cache
        // categories — `cache_read_input_tokens` sits in the same additive
        // position as `cache_creation_input_tokens` — so the input folds to
        // 10 + 30 + 5 with the cache-read share as the cached subset.
        let usage = extract_token_usage(&json!({
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 5,
                "output_tokens": 8
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 45);
        assert_eq!(usage["cachedInputTokens"], 30);
        assert_eq!(usage["completionTokens"], 8);
        assert_eq!(usage["totalTokens"], 53);
    }

    #[test]
    fn kimi_status_usage_keeps_additive_cache_categories() {
        let usage = extract_token_usage(&json!({
            "token_usage": {
                "input_other": 80,
                "input_cache_read": 20,
                "input_cache_creation": 5,
                "output": 15
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 105);
        assert_eq!(usage["cachedInputTokens"], 20);
        assert_eq!(usage["completionTokens"], 15);
        assert_eq!(usage["totalTokens"], 120);
    }

    #[test]
    fn openclaw_response_usage_reads_explicit_cache_counters() {
        let usage = extract_token_usage(&json!({
            "responseUsage": {
                "inputTokens": 80,
                "outputTokens": 15,
                "cacheRead": 20,
                "cacheWrite": 5,
                "totalTokens": 95
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 105);
        assert_eq!(usage["cachedInputTokens"], 20);
        assert_eq!(usage["completionTokens"], 15);
        assert_eq!(usage["totalTokens"], 120);
    }

    #[test]
    fn every_recognized_additive_spelling_marks_the_record_additive() {
        // `cacheWriteInputTokens` folds into the prompt and marks the record
        // additive: the provider's pre-cache total may not reconcile the
        // folded sum back down — counting and semantics share one table.
        let usage = extract_token_usage(&json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "cacheWriteInputTokens": 5,
                "total_tokens": 120
            }
        }))
        .unwrap();
        assert_eq!(usage["promptTokens"], 105);
        assert_eq!(usage["completionTokens"], 20);
        assert_eq!(usage["totalTokens"], 125);
    }
}
