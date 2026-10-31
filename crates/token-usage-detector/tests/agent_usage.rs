use serde_json::json;
use token_usage_detector::agent::{TokenUsage, from_aliases, from_opencode_tokens};

#[test]
fn aliases_preserve_reported_counts_without_deriving_a_total() {
    let usage = from_aliases(&json!({
        "input_tokens": 120,
        "cache_read_input_tokens": 30,
        "cacheWrite": 5,
        "output_tokens": 8,
        "reasoning_tokens": 2
    }))
    .unwrap();
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(120),
            cached_input: Some(30),
            cached_write: Some(5),
            output: Some(8),
            thought: Some(2),
            total: None,
        }
    );
}

#[test]
fn aliases_keep_existing_precedence_and_u64_filtering() {
    let usage = from_aliases(&json!({
        "input": -1,
        "input_tokens": 7,
        "inputTokens": 11,
        "output": "3",
        "output_tokens": 3,
        "total_tokens": 10
    }))
    .unwrap();
    assert_eq!(usage.input, Some(7));
    assert_eq!(usage.output, Some(3));
    assert_eq!(usage.total, Some(10));
    assert_eq!(
        from_aliases(&json!({"input": 1, "input_tokens": 9}))
            .unwrap()
            .input,
        Some(1)
    );
}

#[test]
fn missing_invalid_and_zero_remain_distinct() {
    for value in [
        json!(null),
        json!([]),
        json!({}),
        json!({"input_tokens": -1, "output_tokens": "9"}),
        json!({"other": 4}),
    ] {
        assert_eq!(from_aliases(&value), None);
    }
    assert_eq!(from_aliases(&json!({"input": 0})).unwrap().input, Some(0));
}

#[test]
fn opencode_reads_nested_cache_without_fabricating_total() {
    let usage = from_opencode_tokens(&json!({
        "input": 5,
        "cache": {"read": 2, "write": 1},
        "output": 3,
        "reasoning": 4,
        "total": 99
    }))
    .unwrap();
    assert_eq!(
        usage,
        TokenUsage {
            input: Some(5),
            cached_input: Some(2),
            cached_write: Some(1),
            output: Some(3),
            thought: Some(4),
            total: None,
        }
    );
    assert_eq!(
        from_opencode_tokens(&json!({})),
        Some(TokenUsage::default())
    );
    assert_eq!(from_opencode_tokens(&json!(null)), None);
}

#[test]
fn transport_dto_preserves_the_existing_wire_contract() {
    let usage = TokenUsage {
        input: Some(9),
        cached_input: Some(2),
        ..Default::default()
    };
    let wire = serde_json::to_value(&usage).unwrap();
    assert_eq!(
        wire,
        json!({
            "input": 9,
            "cachedInput": 2,
            "cachedWrite": null,
            "output": null,
            "thought": null,
            "total": null
        })
    );
    assert_eq!(serde_json::from_value::<TokenUsage>(wire).unwrap(), usage);
    assert!(serde_json::from_value::<TokenUsage>(json!({"input": 1, "unknown": 2})).is_err());
}
