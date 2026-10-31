//! Metadata-first native-history token extraction and model attribution.

use crate::contract::{
    ADDITIVE_CACHE_READ_FIELDS, ADDITIVE_CACHE_WRITE_FIELDS, CounterReading, HistoryUsageSummary,
    MessageUsage, UsageAccuracy, counter_reading, number_in, text_field,
};
use crate::variant::{UsageVariant, model_label, prefer_recorded_model};
use crate::window::UsageWindow;
use serde_json::Value;

/// Prefers provider/runtime counters and estimates only message segments that
/// are not covered by native usage metadata. The caller persists the result at
/// day grain, so unchanged history is never tokenized again.
pub fn summarize_sessions(sessions: &[Value], calendar: &UsageWindow) -> HistoryUsageSummary {
    summarize_sessions_with_fallback(sessions, calendar, true)
}

pub fn summarize_sessions_exact_only(
    sessions: &[Value],
    calendar: &UsageWindow,
) -> HistoryUsageSummary {
    summarize_sessions_with_fallback(sessions, calendar, false)
}

fn summarize_sessions_with_fallback(
    sessions: &[Value],
    calendar: &UsageWindow,
    estimate_missing: bool,
) -> HistoryUsageSummary {
    let mut summary = HistoryUsageSummary::default();
    for session in sessions {
        if session
            .get("sourcePath")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.trim().is_empty())
        {
            summary
                .source_paths
                .insert("native-history-store".to_owned());
        }
        let messages = session
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let session_model = session_model_label(session);
        let session_date = session_date_key(session, calendar);
        let before = summary.total_tokens();
        if session.get("usage").is_some() {
            let added = add_explicit_usage(
                session,
                &mut summary,
                session_date,
                session_model.clone(),
                calendar,
            ) as u64;
            summary.message_count = summary.message_count.saturating_add(added);
        } else {
            let mut pending_estimates = Vec::<(MessageUsage, String)>::new();
            let added = messages
                .iter()
                .map(|message| {
                    let date_key =
                        message_date_key(message, calendar).or_else(|| session_date.clone());
                    collect_message_usage_tree(
                        message,
                        &mut summary,
                        &mut pending_estimates,
                        date_key,
                        session_model.clone(),
                        calendar,
                        estimate_missing,
                    )
                })
                .sum::<u64>()
                .saturating_add(flush_pending_estimates(
                    &mut pending_estimates,
                    &mut summary,
                ));
            summary.message_count = summary.message_count.saturating_add(added);
        }
        if summary.total_tokens() > before {
            summary.session_count = summary.session_count.saturating_add(1);
        }
    }
    summary
}

fn message_date_key(message: &Value, window: &UsageWindow) -> Option<String> {
    text_field(
        message,
        &[
            "createdAt",
            "updatedAt",
            "timestamp",
            "time",
            "date",
            "created_at",
            "updated_at",
        ],
    )
    .and_then(|value| window.date_key(&value))
}

fn session_date_key(session: &Value, window: &UsageWindow) -> Option<String> {
    text_field(
        session,
        &[
            "updatedAt",
            "createdAt",
            "timestamp",
            "time",
            "date",
            "updated_at",
            "created_at",
        ],
    )
    .and_then(|value| window.date_key(&value))
}

fn add_explicit_usage(
    message: &Value,
    summary: &mut HistoryUsageSummary,
    date_key: Option<String>,
    default_model: Option<String>,
    calendar: &UsageWindow,
) -> bool {
    let Some(date_key) = date_key.filter(|value| calendar.contains(value)) else {
        return false;
    };
    let before = summary.total_tokens();
    let Some(usage) = message_usage(message, default_model) else {
        return false;
    };
    summary.add(usage, Some(date_key));
    summary.total_tokens() > before
}

pub fn message_usage(message: &Value, default_model: Option<String>) -> Option<MessageUsage> {
    let usage = message.get("usage")?;
    let object = usage.as_object()?;
    // A counter that folds into a reported total refuses the whole record
    // when a spelling was named but never counted: folding it as absent
    // would emit a precise partial total the record never wrote.
    let counted = |keys: &[&str]| -> Option<u64> {
        match counter_reading(object, keys) {
            CounterReading::Counted(count) => Some(count),
            CounterReading::Absent => Some(0),
            CounterReading::Uncounted => None,
        }
    };
    let base_prompt = counted(&[
        "promptTokens",
        "prompt_tokens",
        "inputTokens",
        "input_tokens",
    ])?;
    let cache_read = counted(ADDITIVE_CACHE_READ_FIELDS)?;
    let cache_write = counted(ADDITIVE_CACHE_WRITE_FIELDS)?;
    // A record carrying an additive cache spelling has an additive total:
    // the categories sit outside the base count, so the reported total
    // predates or excludes them and cannot reconcile the fields back down.
    let additive = ADDITIVE_CACHE_READ_FIELDS
        .iter()
        .chain(ADDITIVE_CACHE_WRITE_FIELDS)
        .any(|key| object.contains_key(*key));
    // Additive categories fold through checked arithmetic: an overflow
    // refuses the whole record rather than emitting a saturated count the
    // record never wrote.
    let mut prompt_tokens = base_prompt
        .checked_add(cache_read)
        .and_then(|prompt| prompt.checked_add(cache_write))?;
    let mut completion_tokens = counted(&[
        "completionTokens",
        "completion_tokens",
        "outputTokens",
        "output_tokens",
    ])?;
    let field_total = prompt_tokens.checked_add(completion_tokens)?;
    let total_tokens = match additive {
        true => field_total,
        false => number_in(object, &["totalTokens", "total_tokens"])
            .filter(|value| *value > 0)
            .unwrap_or(field_total),
    };
    if field_total != total_tokens {
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
    if total_tokens == 0 {
        return None;
    }
    let model =
        prefer_recorded_model(model_label(usage), message_model_label(message)).or(default_model);
    // A subset inside the base count is capped by the base; the additive
    // cache-read share joins it the way `normalize` folds the same fields.
    let cached_subset = counted(&["cachedInputTokens", "cached_input_tokens"])?;
    let cached_input = cached_subset.min(base_prompt).checked_add(cache_read)?;
    Some(MessageUsage {
        prompt_tokens,
        cached_input_tokens: cached_input.min(prompt_tokens),
        completion_tokens,
        total_tokens,
        model,
        variant: UsageVariant::from_metadata(message),
        accuracy: UsageAccuracy::Exact,
    })
}

fn collect_message_usage_tree(
    message: &Value,
    summary: &mut HistoryUsageSummary,
    pending_estimates: &mut Vec<(MessageUsage, String)>,
    fallback_date: Option<String>,
    default_model: Option<String>,
    calendar: &UsageWindow,
    estimate_missing: bool,
) -> u64 {
    if message.get("usage").is_some() {
        let scope = text_field(message, &["usageScope", "usage_scope"])
            .unwrap_or_default()
            .to_ascii_lowercase();
        let covers_pending = message
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|children| !children.is_empty())
            || matches!(
                scope.as_str(),
                "request-response" | "pending-segment" | "turn" | "session"
            );
        let mut added = if covers_pending {
            pending_estimates.clear();
            0
        } else {
            flush_pending_estimates(pending_estimates, summary)
        };
        if add_explicit_usage(message, summary, fallback_date, default_model, calendar) {
            added = added.saturating_add(1);
        }
        return added;
    }
    if let Some(children) = message.get("messages").and_then(Value::as_array)
        && !children.is_empty()
    {
        return children
            .iter()
            .map(|child| {
                let date_key = message_date_key(child, calendar).or_else(|| fallback_date.clone());
                collect_message_usage_tree(
                    child,
                    summary,
                    pending_estimates,
                    date_key,
                    default_model.clone(),
                    calendar,
                    estimate_missing,
                )
            })
            .sum();
    }
    if !estimate_missing {
        return 0;
    }
    let Some(date_key) = fallback_date.filter(|value| calendar.contains(value)) else {
        return 0;
    };
    let Some(usage) = estimated_message_usage(message, default_model) else {
        return 0;
    };
    pending_estimates.push((usage, date_key));
    0
}

fn flush_pending_estimates(
    pending_estimates: &mut Vec<(MessageUsage, String)>,
    summary: &mut HistoryUsageSummary,
) -> u64 {
    let added = pending_estimates.len() as u64;
    for (usage, day) in pending_estimates.drain(..) {
        summary.add(usage, Some(day));
    }
    added
}

pub fn estimated_message_usage(
    message: &Value,
    default_model: Option<String>,
) -> Option<MessageUsage> {
    let role = text_field(message, &["role", "author"])
        .unwrap_or_default()
        .to_ascii_lowercase();
    if role == "metadata" {
        return None;
    }
    let text = text_field(message, &["text", "content", "message"])?;
    let tokens = estimate_tokens(&text);
    if tokens == 0 {
        return None;
    }
    let completion = matches!(role.as_str(), "agent" | "assistant" | "model" | "reasoning");
    Some(MessageUsage {
        prompt_tokens: if completion { 0 } else { tokens },
        completion_tokens: if completion { tokens } else { 0 },
        total_tokens: tokens,
        model: message_model_label(message).or(default_model),
        variant: UsageVariant::from_metadata(message),
        accuracy: UsageAccuracy::Estimated,
        ..MessageUsage::default()
    })
}

/// Stable, allocation-free fallback used only when native counters are absent.
/// CJK code points are weighted at 0.9 token and other non-space code points at
/// 0.25 token, matching the previous report behavior without floating point.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut weighted_twentieths = 0_u64;
    for character in text.chars().filter(|character| !character.is_whitespace()) {
        weighted_twentieths =
            weighted_twentieths.saturating_add(if is_cjk(character) { 18 } else { 5 });
    }
    weighted_twentieths.saturating_add(19) / 20
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

fn session_model_label(session: &Value) -> Option<String> {
    model_label(session).or_else(|| {
        session
            .pointer("/modelConfig/modelName")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn message_model_label(message: &Value) -> Option<String> {
    model_label(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn window() -> UsageWindow {
        UsageWindow::from_params(&json!({"now": "2026-07-15T12:00:00Z"}))
    }

    #[test]
    fn explicit_usage_prefers_usage_model_over_session_label() {
        let usage = message_usage(
            &json!({
                "model": "grok-4.5",
                "usage": {
                    "promptTokens": 4200,
                    "totalTokens": 4200,
                    "model": "composer-2.5-fast",
                    "source": "cursor-request-usage"
                }
            }),
            Some("grok-4.5".to_owned()),
        )
        .unwrap();
        assert_eq!(usage.model.as_deref(), Some("composer-2.5-fast"));
    }

    #[test]
    fn placeholder_usage_prefers_same_record_evidence_but_not_session_fallback() {
        for record in [
            json!({"model":"actual-model","usage":{"model":"default","totalTokens":12}}),
            json!({"model":"default","message":{"model":"actual-model"},"usage":{"totalTokens":12}}),
            json!({"model":"unknown","response":{"model":"actual-model"},"usage":{"totalTokens":12}}),
            json!({"model":"auto","modelInfo":{"modelName":"actual-model"},"usage":{"totalTokens":12}}),
            json!({"model":"default","responseUsage":{"model":"actual-model"},"usage":{"totalTokens":12}}),
            json!({"model":"default","status":{"tokenUsage":{"model":"actual-model"}},"usage":{"totalTokens":12}}),
        ] {
            let usage = message_usage(&record, Some("session-final-model".into())).unwrap();
            assert_eq!(usage.model.as_deref(), Some("actual-model"));
            assert_eq!(usage.total_tokens, 12);
        }
        let usage = message_usage(&json!({"model":"default","messages":[{"model":"unrelated-model"}],"text":"actual-model","usage":{"totalTokens":7}}),Some("session-final-model".into())).unwrap();
        assert_eq!(usage.model.as_deref(), Some("default"));
        assert_eq!(usage.total_tokens, 7);
    }

    #[test]
    fn explicit_usage_reconciles_totals_cache_and_model() {
        // A reported total that disagrees with plain fields reconciles them
        // down to it — no additive spelling is present to distrust it.
        let usage = message_usage(
            &json!({
                "role": "agent",
                "modelInfo": {"modelName": "default"},
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 110
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 90);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 110);
        assert_eq!(usage.model.as_deref(), Some("default"));

        // An additive cache-read spelling marks the shape: the share sits
        // outside the base count, so the prompt grows to 100 + 120 and the
        // total derives from the parts — the reported 110 cannot reconcile
        // categories it predates.
        let usage = message_usage(
            &json!({
                "role": "agent",
                "usage": {
                    "prompt_tokens": 100,
                    "cache_read_input_tokens": 120,
                    "completion_tokens": 20,
                    "total_tokens": 110
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 220);
        assert_eq!(usage.cached_input_tokens, 120);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 240);
    }

    #[test]
    fn anthropic_cache_categories_are_additive_to_the_base_input() {
        // The real Anthropic body: `input_tokens` excludes both cache
        // categories, so they fold in — 10 + 30 + 5 — with the cache-read
        // share as the cached subset, the same result the normalize path
        // gives this shape.
        let usage = message_usage(
            &json!({
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 8
                }
            }),
            None,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 45);
        assert_eq!(usage.cached_input_tokens, 30);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens, 53);
    }

    #[test]
    fn additive_overflow_refuses_the_record() {
        // Every additive fold is checked: a count that cannot be expressed
        // is no count at all, and saturating it would emit a number the
        // record never wrote.
        for usage in [
            json!({"prompt_tokens": u64::MAX, "cache_read_input_tokens": 1}),
            json!({"prompt_tokens": u64::MAX, "cache_creation_input_tokens": 1}),
            json!({"prompt_tokens": u64::MAX, "completion_tokens": 1}),
            json!({"prompt_tokens": u64::MAX, "cached_input_tokens": u64::MAX, "cache_read_input_tokens": 1}),
        ] {
            assert!(
                message_usage(&json!({"usage": usage}), None).is_none(),
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
            json!({"prompt_tokens": 100, "completion_tokens": 20, "cached_input_tokens": null}),
        ] {
            assert!(
                message_usage(&json!({"usage": usage}), None).is_none(),
                "a named-but-uncounted counter refuses the record: {usage}"
            );
        }
    }

    #[test]
    fn an_uncounted_reconciliation_total_still_folds() {
        // The reported total only reconciles counted constituents — it is
        // never folded itself — so a named-but-uncounted total leaves the
        // fold honest.
        let usage = message_usage(
            &json!({"usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": null}}),
            None,
        )
        .unwrap();
        assert_eq!(usage.total_tokens, 120);
    }

    #[test]
    fn parent_usage_covers_nested_content_exactly_once() {
        let message = json!({
            "createdAt": "2026-07-10T10:00:00Z",
            "messages": [
                {"role": "user", "text": "plain prompt"},
                {"role": "agent", "text": "plain response"}
            ],
            "usage": {
                "prompt_tokens": 8,
                "completion_tokens": 5,
                "total_tokens": 13
            }
        });
        let mut summary = HistoryUsageSummary::default();
        let added = collect_message_usage_tree(
            &message,
            &mut summary,
            &mut Vec::new(),
            Some("2026-07-10".to_owned()),
            Some("model-a".to_owned()),
            &window(),
            true,
        );
        assert_eq!(added, 1);
        assert_eq!(summary.total_tokens(), 13);
        assert_eq!(summary.explicit_records, 1);
    }

    #[test]
    fn text_without_explicit_usage_is_estimated_once() {
        let sessions = [json!({
            "createdAt": "2026-07-10T10:00:00Z",
            "messages": [
                {"role": "user", "text": "not a token counter"},
                {"role": "agent", "text": "also not a token counter"}
            ]
        })];
        let summary = summarize_sessions(&sessions, &window());
        assert!(summary.total_tokens() > 0);
        assert_eq!(summary.explicit_records, 0);
        assert_eq!(summary.estimated_records, 2);
        assert_eq!(summary.confidence(), "low");
    }

    #[test]
    fn estimator_is_bounded_and_language_aware() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("中文"), 2);
    }

    #[test]
    fn missing_model_is_not_inferred_from_another_message() {
        let summary = summarize_sessions_exact_only(
            &[json!({"updatedAt":"2026-07-10T10:00:00Z", "messages":[
                {"model":"known", "usage":{"totalTokens":10}},
                {"usage":{"totalTokens":5}}
            ]})],
            &window(),
        );
        assert_eq!(
            summary.to_json()["dailyUsage"][0]["modelUsage"]["Others"],
            5
        );
    }
}
