use super::*;
use acop_usage_core::UsageDetail;

fn raw(input: u64, output: u64) -> ReportedTokens {
    ReportedTokens {
        input: Some(input),
        output: Some(output),
        cache_read: Some(0),
        cache_write: Some(0),
    }
}

fn session(input: u64, output: u64) -> UsageSession {
    UsageSession {
        id: "synthetic-session".to_owned(),
        model: Some("last-selected-model".to_owned()),
        started_at: 100.0,
        last_activity_at: 200.0,
        tokens: raw(input, output),
        requests: None,
        details: Vec::new(),
    }
}

fn detail(coverage: Coverage, model: &str, input: u64, output: u64) -> UsageDetail {
    UsageDetail {
        key: format!("detail:{coverage:?}:{model}"),
        model: Some(model.to_owned()),
        task: "synthetic-task".to_owned(),
        first_seen: 100.0,
        last_seen: 200.0,
        time_inferred: false,
        tokens: raw(input, output),
        requests: None,
        coverage,
    }
}

fn normalized(session: &UsageSession) -> SessionUsage {
    normalize(session, InputAccounting::ExcludesCache, false).unwrap()
}

fn advance(
    previous: Option<&SessionUsage>,
    current: &SessionUsage,
    count_initial: bool,
) -> Result<UsageAdvance> {
    super::advance(
        previous.map(UsageCursor::from).as_ref(),
        &UsageCursor::from(current),
        count_initial,
    )
}

fn known(prompt: u64, completion: u64) -> Counts {
    Counts {
        prompt: Some(prompt),
        cached: Some(0),
        completion: Some(completion),
        total: Some(prompt + completion),
        requests: None,
    }
}

#[test]
fn a_component_reset_cannot_be_hidden_by_growth_in_the_same_model() {
    let mut source = session(100, 0);
    let mut main = detail(Coverage::Included, "shared-model", 100, 0);
    main.task = "main".to_owned();
    let mut auxiliary = detail(Coverage::Additional, "shared-model", 100, 0);
    auxiliary.task = "auxiliary".to_owned();
    source.details = vec![main, auxiliary];
    let previous = normalized(&source);
    source.tokens.input = Some(50);
    source.details[0].tokens.input = Some(50);
    source.details[1].tokens.input = Some(160);
    let current = normalized(&source);
    assert_eq!(previous.counts.total, Some(200));
    assert_eq!(current.counts.total, Some(210));
    let result = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(result.kind, AdvanceKind::Reset);
    assert_eq!(result.delta, None);
    assert!(result.attribution_gap);
}

#[test]
fn additional_counter_resets_are_not_hidden_inside_one_task_or_model() {
    let mut source = session(0, 0);
    let mut one = detail(Coverage::Additional, "shared-model", 100, 0);
    one.key = "detail:one".to_owned();
    let mut two = one.clone();
    two.key = "detail:two".to_owned();
    source.details = vec![one, two];
    let previous = normalized(&source);
    source.details[0].tokens.input = Some(50);
    source.details[1].tokens.input = Some(160);
    let current = normalized(&source);
    assert_eq!(current.counts.total, Some(210));
    let result = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(result.kind, AdvanceKind::Reset);
    assert_eq!(result.delta, None);
    assert!(result.counters.is_empty());
}

#[test]
fn removed_counters_cannot_be_hidden_by_growth_elsewhere() {
    let mut source = session(100, 0);
    source
        .details
        .push(detail(Coverage::Additional, "model", 100, 0));
    let previous = normalized(&source);
    let current = normalized(&session(210, 0));
    let result = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(result.kind, AdvanceKind::Reset);
    assert_eq!(result.delta, None);
}

#[test]
fn a_raw_counter_reset_cannot_hide_inside_normalized_prompt_tokens() {
    let mut source = session(100, 0);
    source.tokens.cache_write = Some(10);
    let previous = normalized(&source);
    source.tokens.input = Some(50);
    source.tokens.cache_write = Some(70);
    let current = normalized(&source);
    assert_eq!(previous.counts.total, Some(110));
    assert_eq!(current.counts.total, Some(120));
    let result = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(result.kind, AdvanceKind::Reset);
    assert_eq!(result.delta, None);
}

#[test]
fn main_redistribution_does_not_erase_proven_auxiliary_attribution() {
    let mut source = session(100, 0);
    source
        .details
        .push(detail(Coverage::Additional, "aux-model", 10, 0));
    let previous = normalized(&source);
    source.details[0].tokens.input = Some(15);
    source
        .details
        .push(detail(Coverage::Included, "main-model", 100, 0));
    let current = normalized(&source);
    let result = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(result.delta, Some(known(5, 0)));
    assert_eq!(
        result.models,
        vec![ModelCounts {
            model: Some("aux-model".to_owned()),
            counts: known(5, 0),
        }]
    );
    assert!(result.attribution_gap);
    assert_eq!(result.counters.len(), 1);
    assert_eq!(result.counters[0].key, source.details[0].key);
    assert_eq!(result.counters[0].delta, known(5, 0));
    assert!(!result.counters[0].attribution_gap);
}

#[test]
fn a_cursor_cannot_forge_or_duplicate_its_original_counter_state() {
    let source = normalized(&session(100, 10));
    let mut cursor = UsageCursor::from(&source);
    cursor.counters[0].reported.input = Some(1);
    assert_eq!(
        super::advance(None, &cursor, true),
        Err(StoreUsageFailure::InconsistentReading)
    );
    cursor = UsageCursor::from(&source);
    cursor.counters.push(cursor.counters[0].clone());
    assert_eq!(
        super::advance(None, &cursor, true),
        Err(StoreUsageFailure::InconsistentReading)
    );
}

#[test]
fn additional_usage_does_not_reduce_the_unattributed_main_total() {
    let mut source = session(100, 10);
    source
        .details
        .push(detail(Coverage::Additional, "auxiliary-model", 20, 5));
    let result = normalized(&source);
    assert_eq!(result.counts, known(120, 15));
    assert!(result.allocation_complete);
    assert!(result.issues.is_empty());
    assert_eq!(
        result.models,
        vec![
            ModelCounts {
                model: None,
                counts: known(100, 10),
            },
            ModelCounts {
                model: Some("auxiliary-model".to_owned()),
                counts: known(20, 5),
            },
        ]
    );
}

#[test]
fn complete_main_details_and_auxiliary_rows_are_not_counted_twice() {
    let mut source = session(100, 10);
    source
        .details
        .push(detail(Coverage::Included, "main-model", 100, 10));
    source
        .details
        .push(detail(Coverage::Additional, "auxiliary-model", 20, 5));
    let result = normalized(&source);
    assert_eq!(result.counts, known(120, 15));
    assert!(result.models.iter().all(|model| model.model.is_some()));
    assert!(result.issues.is_empty());
}

#[test]
fn an_included_route_does_not_add_to_the_session_total() {
    let mut source = session(100, 10);
    source
        .details
        .push(detail(Coverage::Included, "route-model", 20, 5));
    let result = normalized(&source);
    assert_eq!(result.counts, known(100, 10));
    assert_eq!(result.models[0].counts, known(80, 5));
    assert!(result.issues.is_empty());
}

#[test]
fn both_cache_components_belong_in_exclusive_input_but_only_once() {
    let mut source = session(100, 5);
    source.tokens.cache_read = Some(20);
    source.tokens.cache_write = Some(10);
    let exclusive = normalized(&source);
    assert_eq!(
        exclusive.counts,
        Counts {
            prompt: Some(130),
            cached: Some(20),
            completion: Some(5),
            total: Some(135),
            requests: None,
        }
    );
    source.tokens.input = Some(130);
    let inclusive = normalize(&source, InputAccounting::IncludesCache, false).unwrap();
    assert_eq!(inclusive.counts, exclusive.counts);
}

#[test]
fn an_unknown_counter_is_not_zero() {
    let mut source = session(100, 10);
    source.tokens.input = None;
    let result = normalized(&source);
    assert_eq!(result.counts.prompt, None);
    assert_eq!(result.counts.total, None);
    assert_eq!(result.counts.completion, Some(10));
    assert!(result.issues.contains(&UsageIssue::UnknownMetric));
    assert_eq!(
        advance(None, &result, true),
        Err(StoreUsageFailure::IncompleteReading)
    );
}

#[test]
fn unknown_cached_detail_does_not_erase_a_reported_inclusive_total() {
    let mut source = session(100, 10);
    source.tokens.cache_read = None;
    source.tokens.cache_write = None;
    let result = normalize(&source, InputAccounting::IncludesCache, false).unwrap();
    assert_eq!(result.counts.prompt, Some(100));
    assert_eq!(result.counts.total, Some(110));
    assert_eq!(result.counts.cached, None);
    assert!(result.issues.contains(&UsageIssue::UnknownMetric));
}

#[test]
fn unknown_task_coverage_does_not_guess_an_aggregate() {
    let mut source = session(100, 10);
    source
        .details
        .push(detail(Coverage::Unknown, "unknown-task-model", 20, 5));
    let result = normalized(&source);
    assert_eq!(result.counts, Counts::default());
    assert!(!result.allocation_complete);
    assert!(result.issues.contains(&UsageIssue::UnknownCoverage));
}

#[test]
fn contradictory_details_are_not_clamped_into_consistent_facts() {
    let mut source = session(100, 10);
    source
        .details
        .push(detail(Coverage::Included, "main-model", 150, 15));
    let result = normalized(&source);
    assert_eq!(result.counts, known(100, 10));
    assert!(!result.allocation_complete);
    assert!(result.issues.contains(&UsageIssue::ConflictingCounters));
    assert_eq!(
        advance(None, &result, true),
        Err(StoreUsageFailure::IncompleteReading)
    );
}

#[test]
fn counter_overflow_refuses_instead_of_saturating() {
    let mut source = session(u64::MAX, 0);
    source
        .details
        .push(detail(Coverage::Additional, "auxiliary-model", 1, 0));
    assert_eq!(
        normalize(&source, InputAccounting::ExcludesCache, false),
        Err(StoreUsageFailure::CounterOverflow)
    );
}

#[test]
fn initial_baseline_and_initial_admission_are_different_decisions() {
    let current = normalized(&session(100, 10));
    let baseline = advance(None, &current, false).unwrap();
    assert_eq!(baseline.kind, AdvanceKind::Baseline);
    assert_eq!(baseline.delta, None);
    assert!(baseline.models.is_empty());
    let admitted = advance(None, &current, true).unwrap();
    assert_eq!(admitted.kind, AdvanceKind::Delta);
    assert_eq!(admitted.delta, Some(known(100, 10)));
}

#[test]
fn repeated_snapshots_add_nothing_and_real_growth_is_a_difference() {
    let previous = normalized(&session(100, 10));
    let repeated = advance(Some(&previous), &previous, true).unwrap();
    assert_eq!(repeated.delta, Some(known(0, 0)));
    assert!(repeated.models.is_empty());
    assert!(!repeated.attribution_gap);
    let current = normalized(&session(110, 11));
    let increment = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(increment.delta, Some(known(10, 1)));
    assert!(!increment.attribution_gap);
}

#[test]
fn model_reclassification_is_not_new_consumption() {
    let previous = normalized(&session(100, 10));
    let mut reclassified = session(100, 10);
    reclassified
        .details
        .push(detail(Coverage::Included, "main-model", 100, 10));
    let current = normalized(&reclassified);
    let increment = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(increment.delta, Some(known(0, 0)));
    assert!(increment.models.is_empty());
    assert!(increment.attribution_gap);
}

#[test]
fn growth_during_reclassification_keeps_only_the_proven_total() {
    let previous = normalized(&session(100, 10));
    let mut reclassified = session(120, 12);
    reclassified
        .details
        .push(detail(Coverage::Included, "main-model", 120, 12));
    let current = normalized(&reclassified);
    let increment = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(increment.delta, Some(known(20, 2)));
    assert_eq!(
        increment.models,
        vec![ModelCounts {
            model: None,
            counts: known(20, 2),
        }]
    );
    assert!(increment.attribution_gap);
    reclassified.tokens = raw(130, 13);
    reclassified.details[0].tokens = raw(130, 13);
    let next = normalized(&reclassified);
    let increment = advance(Some(&current), &next, false).unwrap();
    assert_eq!(increment.delta, Some(known(10, 1)));
    assert_eq!(increment.models[0].model.as_deref(), Some("main-model"));
    assert!(!increment.attribution_gap);
}

#[test]
fn a_rewritten_counter_establishes_a_baseline_not_negative_or_new_usage() {
    let previous = normalized(&session(100, 10));
    let rewritten = normalized(&session(50, 5));
    let reset = advance(Some(&previous), &rewritten, false).unwrap();
    assert_eq!(reset.kind, AdvanceKind::Reset);
    assert_eq!(reset.delta, None);
    assert!(reset.attribution_gap);
    let next = normalized(&session(55, 5));
    assert_eq!(
        advance(Some(&rewritten), &next, false).unwrap().delta,
        Some(known(5, 0))
    );
}

#[test]
fn cursor_identity_and_model_totals_are_checked() {
    let previous = normalized(&session(100, 10));
    let mut current = previous.clone();
    current.id = "another-synthetic-session".to_owned();
    assert_eq!(
        advance(Some(&previous), &current, false),
        Err(StoreUsageFailure::SeriesMismatch)
    );
    current = previous.clone();
    current.models[0].counts = known(50, 5);
    assert_eq!(
        advance(Some(&previous), &current, false),
        Err(StoreUsageFailure::InconsistentReading)
    );
}

#[test]
fn allocations_preserve_source_time_bounds_without_choosing_a_calendar_day() {
    let mut source = session(100, 10);
    source.last_activity_at = 100_000.0;
    let mut auxiliary = detail(Coverage::Additional, "auxiliary-model", 20, 5);
    auxiliary.first_seen = 90_000.0;
    auxiliary.last_seen = 99_000.0;
    source.details.push(auxiliary);
    let result = normalized(&source);
    assert_eq!(result.counts, known(120, 15));
    assert_eq!(
        result.allocations,
        vec![
            UsageAllocation {
                counter_key: source.details[0].key.clone(),
                model: Some("auxiliary-model".to_owned()),
                counts: known(20, 5),
                first_seen: 90_000.0,
                last_seen: 99_000.0,
                time_inferred: false,
            },
            UsageAllocation {
                counter_key: "session".to_owned(),
                model: None,
                counts: known(100, 10),
                first_seen: 100.0,
                last_seen: 100_000.0,
                time_inferred: true,
            },
        ]
    );
}

#[test]
fn cursor_keeps_integer_counters_without_persisting_temporal_allocations() {
    let source = normalized(&session(100, 10));
    let cursor = UsageCursor::from(&source);
    let value = serde_json::to_value(&cursor).unwrap();
    assert_eq!(cursor.id, source.id);
    assert_eq!(cursor.counts, source.counts);
    assert_eq!(cursor.models, source.models);
    assert_eq!(cursor.counters, source.counters);
    assert_eq!(cursor.input_accounting, source.input_accounting);
    assert_eq!(cursor.allocation_complete, source.allocation_complete);
    assert_eq!(cursor.issues, source.issues);
    assert!(value.get("allocations").is_none());
    assert!(value.get("startedAt").is_none());
    assert!(value.get("lastActivityAt").is_none());
    assert_eq!(
        serde_json::from_value::<UsageCursor>(value).unwrap(),
        cursor
    );
}

#[test]
fn reported_requests_follow_coverage_and_advance_independently_of_tokens() {
    let mut source = session(100, 10);
    source.requests = Some(4);
    let mut auxiliary = detail(Coverage::Additional, "auxiliary-model", 20, 5);
    auxiliary.requests = Some(1);
    let mut included = detail(Coverage::Included, "included-model", 10, 0);
    included.requests = Some(1);
    source.details = vec![auxiliary, included];
    let previous = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    assert_eq!(previous.counts.total, Some(135));
    assert_eq!(previous.counts.requests, Some(5));
    assert!(previous.allocation_complete);
    let repeated = advance(Some(&previous), &previous, false).unwrap();
    assert_eq!(repeated.delta.unwrap().requests, Some(0));
    assert!(repeated.models.is_empty());
    source.requests = Some(5);
    let current = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    let delta = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(delta.delta.unwrap().total, Some(0));
    assert_eq!(delta.delta.unwrap().requests, Some(1));
    assert_eq!(delta.counters.len(), 1);
    assert_eq!(delta.models.len(), 1);
    assert_eq!(delta.models[0].model, None);
    assert_eq!(delta.models[0].counts.requests, Some(1));
    assert_eq!(delta.counters[0].delta.requests, Some(1));
}

#[test]
fn unreported_requests_are_not_zero_and_missing_declared_requests_refuse_advance() {
    let source = session(100, 10);
    let unsupported = normalize(&source, InputAccounting::ExcludesCache, false).unwrap();
    assert_eq!(unsupported.counts.requests, None);
    assert!(advance(None, &unsupported, true).is_ok());
    let missing = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    assert_eq!(missing.counts.total, Some(110));
    assert_eq!(missing.counts.requests, None);
    assert!(missing.issues.contains(&UsageIssue::UnknownMetric));
    assert_eq!(
        advance(None, &missing, true),
        Err(StoreUsageFailure::IncompleteReading)
    );
}

#[test]
fn request_counter_resets_are_not_hidden_by_token_growth() {
    let mut source = session(100, 10);
    source.requests = Some(4);
    let previous = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    source.tokens.input = Some(110);
    source.requests = Some(2);
    let current = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    let delta = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(delta.kind, AdvanceKind::Reset);
    assert_eq!(delta.delta, None);
}

#[test]
fn first_observation_of_a_new_metric_is_not_a_historical_increment() {
    let mut source = session(100, 10);
    let previous = normalized(&source);
    source.requests = Some(4);
    let current = normalize(&source, InputAccounting::ExcludesCache, true).unwrap();
    let delta = advance(Some(&previous), &current, false).unwrap();
    assert_eq!(delta.kind, AdvanceKind::Reset);
    assert_eq!(delta.delta, None);
}

#[test]
fn request_counter_aggregation_refuses_overflow() {
    let mut source = session(100, 10);
    source.requests = Some(u64::MAX);
    let mut auxiliary = detail(Coverage::Additional, "auxiliary-model", 20, 5);
    auxiliary.requests = Some(1);
    source.details.push(auxiliary);
    assert_eq!(
        normalize(&source, InputAccounting::ExcludesCache, true),
        Err(StoreUsageFailure::CounterOverflow)
    );
}
