use acop_usage_core::{Coverage, InputAccounting, ReportedTokens, UsageSession};
use metera_sdk::store::{
    AdvanceKind, CounterAdvance, Counts, ModelCounts, SessionUsage, StoreUsageFailure,
    UsageAdvance, UsageAllocation, UsageCounter, UsageCursor, UsageIssue,
};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, StoreUsageFailure>;

fn sum(a: Option<u64>, b: Option<u64>) -> Result<Option<u64>> {
    match (a, b) {
        (Some(a), Some(b)) => a
            .checked_add(b)
            .map(Some)
            .ok_or(StoreUsageFailure::CounterOverflow),
        _ => Ok(None),
    }
}

fn counts(
    prompt: Option<u64>,
    cached: Option<u64>,
    completion: Option<u64>,
    requests: Option<u64>,
) -> Result<Counts> {
    Ok(Counts {
        prompt,
        cached,
        completion,
        total: sum(prompt, completion)?,
        requests,
    })
}

fn zero(requests: Option<u64>) -> Counts {
    Counts {
        prompt: Some(0),
        cached: Some(0),
        completion: Some(0),
        total: Some(0),
        requests: requests.map(|_| 0),
    }
}

fn plus(a: Counts, b: Counts) -> Result<Counts> {
    counts(
        sum(a.prompt, b.prompt)?,
        sum(a.cached, b.cached)?,
        sum(a.completion, b.completion)?,
        sum(a.requests, b.requests)?,
    )
}

fn complete(value: Counts) -> Result<[u64; 3]> {
    let (Some(prompt), Some(cached), Some(completion), Some(total)) =
        (value.prompt, value.cached, value.completion, value.total)
    else {
        return Err(StoreUsageFailure::IncompleteReading);
    };
    if cached > prompt || prompt.checked_add(completion) != Some(total) {
        return Err(StoreUsageFailure::InconsistentReading);
    }
    Ok([prompt - cached, cached, completion])
}

fn reported(
    value: ReportedTokens,
    accounting: InputAccounting,
    requests: Option<u64>,
    issues: &mut BTreeSet<UsageIssue>,
) -> Result<Counts> {
    let prompt = match accounting {
        InputAccounting::ExcludesCache => {
            sum(sum(value.input, value.cache_read)?, value.cache_write)?
        }
        InputAccounting::IncludesCache => value.input,
    };
    if let Some(prompt) = prompt
        && (value.cache_read.is_some_and(|cached| cached > prompt)
            || sum(value.cache_read, value.cache_write)?.is_some_and(|cached| cached > prompt))
    {
        issues.insert(UsageIssue::ConflictingCounters);
    }
    let result = counts(prompt, value.cache_read, value.output, requests)?;
    if matches!(complete(result), Err(StoreUsageFailure::IncompleteReading)) {
        issues.insert(UsageIssue::UnknownMetric);
    }
    Ok(result)
}

fn residual(total: Counts, included: Counts, issues: &mut BTreeSet<UsageIssue>) -> Result<Counts> {
    let mut difference = |total: Option<u64>, included: Option<u64>| match (total, included) {
        (Some(total), Some(included)) => match total.checked_sub(included) {
            Some(value) => Some(value),
            None => {
                issues.insert(UsageIssue::ConflictingCounters);
                None
            }
        },
        _ => None,
    };
    let result = counts(
        difference(total.prompt, included.prompt),
        difference(total.cached, included.cached),
        difference(total.completion, included.completion),
        difference(total.requests, included.requests),
    )?;
    if matches!(
        complete(result),
        Err(StoreUsageFailure::InconsistentReading)
    ) {
        issues.insert(UsageIssue::ConflictingCounters);
    }
    Ok(result)
}

fn add_model(
    models: &mut BTreeMap<Option<String>, Counts>,
    model: Option<String>,
    value: Counts,
) -> Result<()> {
    let previous = models
        .get(&model)
        .copied()
        .unwrap_or_else(|| zero(value.requests));
    models.insert(model, plus(previous, value)?);
    Ok(())
}

fn model_vector(models: BTreeMap<Option<String>, Counts>) -> Vec<ModelCounts> {
    models
        .into_iter()
        .filter(|(_, counts)| has_usage(*counts))
        .map(|(model, counts)| ModelCounts { model, counts })
        .collect()
}

fn has_usage(value: Counts) -> bool {
    value.total != Some(0) || value.requests.is_some_and(|requests| requests > 0)
}

pub(crate) fn normalize(
    session: &UsageSession,
    accounting: InputAccounting,
    requests_supported: bool,
) -> Result<SessionUsage> {
    let mut issues = BTreeSet::new();
    if requests_supported
        && (session.requests.is_none()
            || session
                .details
                .iter()
                .any(|detail| detail.requests.is_none()))
    {
        issues.insert(UsageIssue::UnknownMetric);
    }
    let summary = reported(session.tokens, accounting, session.requests, &mut issues)?;
    let mut included = zero(summary.requests);
    let mut additional = zero(summary.requests);
    let mut models = BTreeMap::new();
    let mut main_models = BTreeMap::new();
    let mut counters = BTreeMap::new();
    let mut allocations = Vec::new();
    for detail in &session.details {
        let value = reported(detail.tokens, accounting, detail.requests, &mut issues)?;
        match detail.coverage {
            Coverage::Included => {
                included = plus(included, value)?;
                add_model(&mut main_models, detail.model.clone(), value)?;
            }
            Coverage::Additional => {
                additional = plus(additional, value)?;
                if detail.key.is_empty()
                    || detail.key == "session"
                    || counters
                        .insert(
                            detail.key.clone(),
                            UsageCounter {
                                key: detail.key.clone(),
                                reported: detail.tokens,
                                counts: value,
                                models: model_vector(BTreeMap::from([(
                                    detail.model.clone(),
                                    value,
                                )])),
                            },
                        )
                        .is_some()
                {
                    return Err(StoreUsageFailure::InconsistentReading);
                }
            }
            Coverage::Unknown => {
                issues.insert(UsageIssue::UnknownCoverage);
            }
        }
        add_model(&mut models, detail.model.clone(), value)?;
        if has_usage(value) {
            allocations.push(UsageAllocation {
                counter_key: if detail.coverage == Coverage::Included {
                    "session".to_owned()
                } else {
                    detail.key.clone()
                },
                model: detail.model.clone(),
                counts: value,
                first_seen: detail.first_seen,
                last_seen: detail.last_seen,
                time_inferred: detail.time_inferred,
            });
        }
    }
    let remainder = residual(summary, included, &mut issues)?;
    add_model(&mut models, None, remainder)?;
    add_model(&mut main_models, None, remainder)?;
    counters.insert(
        "session".to_owned(),
        UsageCounter {
            key: "session".to_owned(),
            reported: session.tokens,
            counts: summary,
            models: model_vector(main_models),
        },
    );
    if has_usage(remainder) {
        allocations.push(UsageAllocation {
            counter_key: "session".to_owned(),
            model: None,
            counts: remainder,
            first_seen: session.started_at,
            last_seen: session.last_activity_at,
            time_inferred: true,
        });
    }
    let mut totals = plus(summary, additional)?;
    if issues.contains(&UsageIssue::UnknownCoverage) {
        totals = Counts::default();
    }
    let modeled = models
        .values()
        .try_fold(zero(totals.requests), |a, b| plus(a, *b))?;
    let allocation_complete = issues.is_empty()
        && complete(totals).is_ok()
        && models.values().all(|value| complete(*value).is_ok())
        && modeled == totals;
    if !allocation_complete {
        issues.insert(UsageIssue::IncompleteAttribution);
    }
    Ok(SessionUsage {
        id: session.id.clone(),
        input_accounting: accounting,
        started_at: session.started_at,
        last_activity_at: session.last_activity_at,
        counts: totals,
        models: model_vector(models),
        counters: counters.into_values().collect(),
        allocations,
        allocation_complete,
        issues: issues.into_iter().collect(),
    })
}

fn model_map(total: Counts, models: &[ModelCounts]) -> Result<BTreeMap<Option<String>, Counts>> {
    complete(total)?;
    let mut result = BTreeMap::new();
    for model in models {
        complete(model.counts)?;
        if model.counts.requests.is_some() != total.requests.is_some()
            || result.insert(model.model.clone(), model.counts).is_some()
        {
            return Err(StoreUsageFailure::InconsistentReading);
        }
    }
    let modeled = result
        .values()
        .try_fold(zero(total.requests), |a, b| plus(a, *b))?;
    if modeled != total {
        return Err(StoreUsageFailure::InconsistentReading);
    }
    result.retain(|_, value| has_usage(*value));
    Ok(result)
}

struct CounterView {
    reported: ReportedTokens,
    counts: Counts,
    models: BTreeMap<Option<String>, Counts>,
}

fn counter_map(cursor: &UsageCursor) -> Result<BTreeMap<&str, CounterView>> {
    if !cursor.issues.is_empty() || !cursor.allocation_complete {
        return Err(StoreUsageFailure::IncompleteReading);
    }
    let expected_models = model_map(cursor.counts, &cursor.models)?;
    let mut result = BTreeMap::new();
    let mut total = zero(cursor.counts.requests);
    let mut models = BTreeMap::new();
    for counter in &cursor.counters {
        let mut issues = BTreeSet::new();
        if reported(
            counter.reported,
            cursor.input_accounting,
            counter.counts.requests,
            &mut issues,
        )? != counter.counts
            || !issues.is_empty()
            || counter.counts.requests.is_some() != cursor.counts.requests.is_some()
        {
            return Err(StoreUsageFailure::InconsistentReading);
        }
        let reading = CounterView {
            reported: counter.reported,
            counts: counter.counts,
            models: model_map(counter.counts, &counter.models)?,
        };
        total = plus(total, reading.counts)?;
        for (model, counts) in &reading.models {
            add_model(&mut models, model.clone(), *counts)?;
        }
        if counter.key.is_empty() || result.insert(counter.key.as_str(), reading).is_some() {
            return Err(StoreUsageFailure::InconsistentReading);
        }
    }
    if !result.contains_key("session") || total != cursor.counts || models != expected_models {
        return Err(StoreUsageFailure::InconsistentReading);
    }
    Ok(result)
}

fn subtract(current: Counts, previous: Counts) -> Result<Option<Counts>> {
    let current_parts = complete(current)?;
    let previous_parts = complete(previous)?;
    if current_parts
        .iter()
        .zip(previous_parts)
        .any(|(current, previous)| *current < previous)
    {
        return Ok(None);
    }
    let requests = match (current.requests, previous.requests) {
        (Some(current), Some(previous)) => {
            let Some(delta) = current.checked_sub(previous) else {
                return Ok(None);
            };
            Some(delta)
        }
        (None, None) => None,
        _ => return Err(StoreUsageFailure::InconsistentReading),
    };
    counts(
        Some(current.prompt.unwrap() - previous.prompt.unwrap()),
        Some(current.cached.unwrap() - previous.cached.unwrap()),
        Some(current.completion.unwrap() - previous.completion.unwrap()),
        requests,
    )
    .map(Some)
}

pub(crate) fn advance(
    previous: Option<&UsageCursor>,
    current: &UsageCursor,
    count_initial: bool,
) -> Result<UsageAdvance> {
    let current_counters = counter_map(current)?;
    let Some(previous) = previous else {
        return Ok(UsageAdvance {
            kind: if count_initial {
                AdvanceKind::Delta
            } else {
                AdvanceKind::Baseline
            },
            delta: count_initial.then_some(current.counts),
            models: if count_initial {
                current.models.clone()
            } else {
                Vec::new()
            },
            counters: if count_initial {
                current_counters
                    .into_iter()
                    .filter(|(_, counter)| has_usage(counter.counts))
                    .map(|(key, counter)| CounterAdvance {
                        key: key.to_owned(),
                        delta: counter.counts,
                        models: model_vector(counter.models),
                        attribution_gap: false,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            attribution_gap: false,
        });
    };
    if previous.id != current.id {
        return Err(StoreUsageFailure::SeriesMismatch);
    }
    let previous_counters = counter_map(previous)?;
    if previous.input_accounting != current.input_accounting
        || previous.counts.requests.is_some() != current.counts.requests.is_some()
        || previous_counters
            .keys()
            .any(|key| !current_counters.contains_key(key))
    {
        return Ok(reset());
    }
    let mut total = zero(current.counts.requests);
    let mut models = BTreeMap::new();
    let mut counters = Vec::new();
    let mut attribution_gap = false;
    for (key, current) in current_counters {
        let old = previous_counters.get(key);
        if old.is_some_and(|old| raw_decreased(current.reported, old.reported)) {
            return Ok(reset());
        }
        let Some(delta) = subtract(
            current.counts,
            old.map_or_else(|| zero(current.counts.requests), |old| old.counts),
        )?
        else {
            return Ok(reset());
        };
        let empty = BTreeMap::new();
        let (model_deltas, redistributed) = model_delta(
            delta,
            &current.models,
            old.map_or(&empty, |old| &old.models),
        )?;
        attribution_gap |= redistributed;
        total = plus(total, delta)?;
        for (model, counts) in &model_deltas {
            add_model(&mut models, model.clone(), *counts)?;
        }
        if has_usage(delta) {
            counters.push(CounterAdvance {
                key: key.to_owned(),
                delta,
                models: model_vector(model_deltas),
                attribution_gap: redistributed,
            });
        }
    }
    Ok(UsageAdvance {
        kind: AdvanceKind::Delta,
        delta: Some(total),
        models: model_vector(models),
        counters,
        attribution_gap,
    })
}

fn reset() -> UsageAdvance {
    UsageAdvance {
        kind: AdvanceKind::Reset,
        delta: None,
        models: Vec::new(),
        counters: Vec::new(),
        attribution_gap: true,
    }
}

fn raw_decreased(current: ReportedTokens, previous: ReportedTokens) -> bool {
    let values = |tokens: ReportedTokens| {
        [
            tokens.input,
            tokens.output,
            tokens.cache_read,
            tokens.cache_write,
        ]
    };
    values(current)
        .into_iter()
        .zip(values(previous))
        .any(|(current, previous)| match (current, previous) {
            (Some(current), Some(previous)) => current < previous,
            (None, Some(_)) => true,
            _ => false,
        })
}

fn model_delta(
    delta: Counts,
    current: &BTreeMap<Option<String>, Counts>,
    previous: &BTreeMap<Option<String>, Counts>,
) -> Result<(BTreeMap<Option<String>, Counts>, bool)> {
    let keys = current
        .keys()
        .chain(previous.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut model_deltas = BTreeMap::new();
    let mut redistributed = false;
    for key in keys {
        match subtract(
            current
                .get(&key)
                .copied()
                .unwrap_or_else(|| zero(delta.requests)),
            previous
                .get(&key)
                .copied()
                .unwrap_or_else(|| zero(delta.requests)),
        )? {
            Some(value) => {
                model_deltas.insert(key, value);
            }
            None => redistributed = true,
        }
    }
    let modeled = model_deltas
        .values()
        .try_fold(zero(delta.requests), |a, b| plus(a, *b))?;
    redistributed |= modeled != delta;
    if redistributed {
        model_deltas.clear();
        model_deltas.insert(None, delta);
    }
    Ok((model_deltas, redistributed))
}
#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
