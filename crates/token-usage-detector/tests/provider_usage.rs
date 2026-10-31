use serde_json::json;
use token_usage_detector::TokenUsage;
use token_usage_detector::metrics as general;
use token_usage_detector::provider::ProviderMapping;
use token_usage_detector::usage::Quality;

fn openai() -> ProviderMapping {
    ProviderMapping::new("openai.usage", Quality::Reported).unwrap()
}

#[test]
fn openai_response_maps_shared_counters_and_keeps_native_extras() {
    let usage = openai()
        .normalize(&json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 30, "audio_tokens": 2},
                "completion_tokens_details": {"reasoning_tokens": 5}
            }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(
        usage.usage,
        TokenUsage {
            input: Some(100),
            cached_input: Some(30),
            cached_write: None,
            output: Some(20),
            thought: Some(5),
            total: Some(120),
        }
    );
    assert_eq!(
        usage.extra,
        [("openai.usage/prompt_tokens_details.audio_tokens", 2)]
            .into_iter()
            .map(|(name, count)| (name.to_owned(), count))
            .collect()
    );
}

#[test]
fn anthropic_and_gemini_spellings_converge_on_the_same_counters() {
    let anthropic = openai()
        .normalize(&json!({
            "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 5,
                "output_tokens": 8
            }
        }))
        .unwrap()
        .unwrap();
    // Anthropic reports both cache categories outside `input_tokens`: the
    // real shape folds to 10 + 30 + 5, with the cache-read share as the
    // cached subset.
    assert_eq!(anthropic.usage.input, Some(45));
    assert_eq!(anthropic.usage.cached_input, Some(30));
    assert_eq!(anthropic.usage.cached_write, Some(5));
    assert_eq!(anthropic.usage.output, Some(8));

    let gemini = openai()
        .normalize(&json!({
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 40,
                "candidatesTokenCount": 12,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 115
            }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(gemini.usage.input, Some(100));
    assert_eq!(gemini.usage.cached_input, Some(40));
    assert_eq!(gemini.usage.output, Some(15));
    assert_eq!(gemini.usage.thought, Some(3));
    assert_eq!(gemini.usage.total, Some(115));
}

#[test]
fn kimi_and_deepseek_shapes_keep_their_native_counts() {
    let kimi = openai()
        .normalize(&json!({
            "token_usage": {
                "input_other": 80,
                "input_cache_read": 20,
                "input_cache_creation": 5,
                "output": 15
            }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(kimi.usage.input, Some(105));
    assert_eq!(kimi.usage.cached_input, Some(20));
    assert_eq!(kimi.usage.cached_write, Some(5));
    assert_eq!(kimi.usage.output, Some(15));

    let deepseek = ProviderMapping::new("deepseek.usage", Quality::Reported).unwrap();
    let usage = deepseek
        .normalize(&json!({
            "usage": {
                "prompt_tokens": 100,
                "prompt_cache_hit_tokens": 40,
                "prompt_cache_miss_tokens": 60,
                "completion_tokens": 20,
                "total_tokens": 120
            }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(usage.usage.input, Some(100));
    assert_eq!(usage.usage.cached_input, Some(40));
    assert_eq!(usage.extra["deepseek.usage/prompt_cache_miss_tokens"], 60);
}

#[test]
fn bare_usage_input_output_and_alias_precedence() {
    let usage = openai()
        .normalize(&json!({"usage": {"input": 12, "output": 5}}))
        .unwrap()
        .unwrap();
    assert_eq!(usage.usage.input, Some(12));
    assert_eq!(usage.usage.output, Some(5));

    // The canonical spelling wins when a proxy echoes two names for one count.
    let usage = openai()
        .normalize(&json!({"usage": {"input": 3, "input_tokens": 9, "prompt_tokens": 7}}))
        .unwrap()
        .unwrap();
    assert_eq!(usage.usage.input, Some(9));
    // Both spellings are recognized, so neither is repeated as an extra.
    assert!(usage.extra.is_empty());
}

#[test]
fn named_but_uncounted_counters_project_as_unknown() {
    let usage = openai()
        .normalize(&json!({
            "usage": {
                "input_tokens": null,
                "output_tokens": 7
            }
        }))
        .unwrap()
        .unwrap();
    assert_eq!(usage.usage.input, None);
    assert_eq!(usage.unknown, vec![general::TOKENS_INPUT.to_owned()]);
    let metrics = openai().metrics(&usage);
    assert_eq!(metrics[general::TOKENS_INPUT].quality, Quality::Unknown);
    assert_eq!(metrics[general::TOKENS_INPUT].value, None);
    assert_eq!(metrics[general::TOKENS_OUTPUT].value.as_deref(), Some("7"));
}

#[test]
fn a_usage_object_cannot_assert_its_binding_or_measurement() {
    for field in [
        "source",
        "extensionId",
        "instanceId",
        "generation",
        "registryEpoch",
        "measurementRef",
        "measurement",
        "logicalMeasurement",
        "principal",
        "effectId",
        "stateRoot",
        "authorized",
    ] {
        let mut usage = serde_json::Map::new();
        usage.insert("input_tokens".to_owned(), json!(5));
        usage.insert(field.to_owned(), json!("self-asserted"));
        let failure = openai()
            .normalize(&json!({"usage": usage}))
            .expect_err("self-asserted field");
        assert_eq!(failure.code, "usage_source_self_asserted", "{field}");
    }
}

#[test]
fn empty_and_foreign_records_claim_nothing() {
    for record in [
        json!(null),
        json!([]),
        json!({}),
        json!({"usage": {}}),
        json!({"usage": {"note": "no counts"}}),
        json!({"choices": [{"finish_reason": "stop"}]}),
        // Error and status bodies are not usage records, however many
        // integers they carry.
        json!({"error": {"code": 429, "message": "rate limited"}}),
        json!({"created": 1700000000i64, "retry_after": 2}),
    ] {
        assert_eq!(openai().normalize(&record).unwrap(), None);
    }
}

#[test]
fn a_mapping_without_a_valid_namespace_is_refused() {
    assert!(ProviderMapping::new("vendor.usage", Quality::Reported).is_ok());
    for bad in ["vendor", "licoup.usage", "licoup-usage", ""] {
        assert!(
            ProviderMapping::new(bad, Quality::Reported).is_err(),
            "{bad} cannot namespace provider extras"
        );
    }
}
