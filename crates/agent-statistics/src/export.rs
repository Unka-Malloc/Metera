//! Prometheus/OpenMetrics exposition of what the journal and the gateway
//! counters retain.
//!
//! The host mounts the scrape route; this module is the read-only projection
//! underneath it: retained facts and a gateway document in, exposition text
//! out. There is no HTTP here, no store access beyond the journal's own read
//! port and no new dependency — the same journal the host settles into is the
//! journal a scrape summarizes. Mounting the route is the host's trust
//! decision: the exposition carries operational source and scope identifiers,
//! so the host is expected to serve it behind its own access boundary — the
//! loopback or authenticated listener it already uses — never as a public
//! endpoint.
//!
//! Three rules mirror the store's own:
//!
//! - **Unknown stays unwritten.** A retained fact carrying no value emits no
//!   sample; it is counted under `metera_usage_unknown_facts` instead, so a
//!   scrape can never turn "nobody knows" into "nothing was used".
//! - **Sums stay exact.** Token totals add through the published
//!   [`ExactNumber`] decimal, never binary floating point; what the text
//!   carries is what the store wrote.
//! - **Cardinality stays bounded.** Model labels fold into `Other` past the
//!   same bound the recorder applies per day, and every family truncates at
//!   a fixed series limit: a scrape is a summary, never a second ledger.
//!
//! The families are deliberately flat:
//!
//! | Metric | Type | Labels |
//! |:---|:---|:---|
//! | `metera_gateway_agent_requests_total` | counter | `agent`, `day` |
//! | `metera_gateway_model_requests_total` | counter | `day`, `model` |
//! | `metera_usage_tokens` | gauge | `kind`, `model`, `source` |
//! | `metera_usage_unknown_facts` | gauge | `kind`, `model`, `source` |
//! | `metera_balance_remaining` | gauge | `metric`, `scope`, `source`, `unit` |
//! | `metera_export_truncated` | gauge | — |
//!
//! A fact under `reading:{day}:{model}` names its model; every other fact
//! shape counts as `Others`, the unattributed model the contract already
//! names. A balance reading is a gauge, not a sum: a fact whose metric sits
//! under the `balance` namespace exports the latest `observed_at` reading,
//! which is how the host keeps a remaining amount from being added to its
//! own earlier readings.

use acativa::usage_ledger::LedgerResult;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use token_usage_detector::metrics as general;
use token_usage_detector::usage::ExactNumber;

use crate::contract::UNATTRIBUTED_MODEL;
use crate::gateway::{MAX_MODELS_PER_DAY, UsageDocument};
use crate::journal::{MAX_FACT_PAGE, MeteringFact};
use crate::source_host::UsageSourceHost;

/// The most series one exposition family emits. Past it, the projection
/// truncates deterministically rather than growing a scrape without bound.
pub const MAX_EXPORTED_SERIES: usize = 256;

/// The most retained facts one scrape folds. Past it the scan truncates —
/// the exposition is a projection, never a second pass over history — and
/// the text says so explicitly through `metera_export_truncated`.
pub const MAX_EXPORT_FACTS: usize = 100_000;

/// The label a counter family folds overflow into, matching the recorder's
/// own `Other` model bucket.
const FOLDED_LABEL: &str = "Other";

/// The exposition text for the retained facts and the request counters:
/// `# HELP`/`# TYPE` headers, escaped label values, sorted series, and the
/// OpenMetrics `# EOF` terminator. Deterministic for the same inputs.
pub fn exposition(facts: &[MeteringFact], gateway: Option<&UsageDocument>) -> String {
    let mut projection = FactProjection::default();
    projection.scan(facts, usize::MAX);
    projection.exposition(gateway)
}

/// Read the journal's retained facts and project them with the gateway
/// counters into exposition text.
///
/// The journal is paged through its own `MeteringFactPort::read`, never
/// opened or written a second time: the host that composes the route hands
/// this the same host it settles through. Each page folds into bounded
/// per-series state as it arrives — a scrape's memory is the projection,
/// not the journal — and the scan stops at [`MAX_EXPORT_FACTS`], emitting
/// `metera_export_truncated` so a partial projection cannot pass for the
/// whole record.
pub fn render(host: &UsageSourceHost, gateway: Option<&UsageDocument>) -> LedgerResult<String> {
    let mut projection = FactProjection::default();
    let mut cursor = None;
    loop {
        let page = host.read(cursor.as_deref(), MAX_FACT_PAGE)?;
        cursor = page.next_cursor;
        projection.scan(&page.facts, MAX_EXPORT_FACTS);
        if projection.truncated || cursor.is_none() {
            break;
        }
    }
    Ok(projection.exposition(gateway))
}

/// One `(source, model, kind)` row of the token projection.
type TokenRow = (String, String, String, bool, Option<ExactNumber>);

/// The streaming projection a scrape folds retained facts into: bounded
/// per-series state, a scanned-fact count and an explicit truncation flag.
/// The journal is never materialized — state stays proportional to the
/// series it projects, capped by the scan limit.
/// One `(metric, scope, source, unit)` balance series key.
type BalanceKey = (String, String, String, String);

/// The newest reading per balance key: the reading's instant in epoch
/// nanoseconds and what it carried — a count, or `None` when the newest
/// reading is unknown.
type BalanceLatest = BTreeMap<BalanceKey, (i128, Option<ExactNumber>)>;

#[derive(Default)]
struct FactProjection {
    token_rows: Vec<TokenRow>,
    balance_latest: BalanceLatest,
    scanned: usize,
    truncated: bool,
}

impl FactProjection {
    /// Fold one page of retained facts. Rows past `cap` mark the projection
    /// truncated instead of growing it — the flag is how the exposition
    /// admits it summarized a prefix.
    fn scan<'a>(&mut self, facts: impl IntoIterator<Item = &'a MeteringFact>, cap: usize) {
        for fact in facts {
            if self.scanned >= cap {
                self.truncated = true;
                return;
            }
            self.fold(fact);
        }
    }

    /// Fold one retained fact into the series it belongs to.
    fn fold(&mut self, fact: &MeteringFact) {
        self.scanned += 1;
        if is_balance_metric(&fact.metric) {
            // A balance reading is a gauge, not a sum: the latest
            // `observed_at` reading decides what the gauge says — including
            // when the latest reading is unknown, which supersedes the older
            // number rather than leaving it published. Instants compare, not
            // spellings: RFC3339 offsets give one moment different strings.
            let value = fact.value.as_deref().and_then(ExactNumber::parse);
            let instant = observed_instant(&fact.observed_at);
            let key = (
                fact.metric.clone(),
                fact.scope_ref.clone(),
                fact.source_ref.clone(),
                fact.unit.clone(),
            );
            let newer = self
                .balance_latest
                .get(&key)
                .is_none_or(|(latest, _)| instant > *latest);
            if newer {
                self.balance_latest.insert(key, (instant, value));
            }
            return;
        }
        if fact.unit != "tokens" {
            return;
        }
        let Some(kind) = token_kind(&fact.metric) else {
            return;
        };
        let model = reading_model(&fact.fact_id)
            .unwrap_or(UNATTRIBUTED_MODEL)
            .to_owned();
        let value = fact.value.as_deref().and_then(ExactNumber::parse);
        self.token_rows.push((
            fact.source_ref.clone(),
            model,
            kind,
            fact.value.is_some(),
            value,
        ));
    }

    /// Emit the exposition text for what the scan folded. Truncation is
    /// never silent: a scan that hit the fact cap or a family that emitted
    /// only its first [`MAX_EXPORTED_SERIES`] rows both set
    /// `metera_export_truncated`, so a partial projection cannot pass for
    /// the whole record.
    fn exposition(&self, gateway: Option<&UsageDocument>) -> String {
        let mut out = String::new();
        let truncated = self.truncated
            | write_balance_family(&mut out, &self.balance_latest)
            | write_gateway_families(&mut out, gateway)
            | write_token_families(&mut out, &self.token_rows);
        if truncated {
            header(
                &mut out,
                "metera_export_truncated",
                "Whether the retained-fact scan hit its cap or a family truncated at the series limit, so this exposition covers only part of the retained record.",
                "gauge",
            );
            series(&mut out, "metera_export_truncated", &[], "1");
        }
        out.push_str("# EOF\n");
        out
    }
}

/// One `(source, model, kind)` series over the token metrics.
type TokenKey = (String, String, String);

/// The kind label a token-unit metric exports under: the shared counters keep
/// their contract kinds; a provider-native token counter keeps its own leaf
/// name, so `openai.usage/cache_write_tokens` reads as `cache_write_tokens`
/// rather than disappearing. The kind never spans separators — it is the
/// metric's own last segment.
fn token_kind(metric: &str) -> Option<String> {
    match metric {
        general::TOKENS_INPUT => Some("input".to_owned()),
        general::TOKENS_OUTPUT => Some("output".to_owned()),
        general::TOKENS_CACHED_INPUT => Some("cached_input".to_owned()),
        general::TOKENS_TOTAL => Some("total".to_owned()),
        _ => metric.rsplit(['.', '/']).next().map(str::to_owned),
    }
}

/// The model a `reading:{day}:{model}` fact identity names, when it names
/// one. Any other identity shape — a measurement, an observation — is no
/// model claim, and the fact counts as unattributed.
fn reading_model(fact_id: &str) -> Option<&str> {
    let mut segments = fact_id.splitn(3, '#');
    segments.next()?;
    let identity = segments.next()?;
    let rest = identity.strip_prefix("reading:")?;
    let (_, model) = rest.split_once(':')?;
    Some(model.strip_suffix(":unknown").unwrap_or(model))
}

/// Whether the metric sits under the `balance` namespace, where a retained
/// fact reads as a remaining amount rather than an amount of work done.
fn is_balance_metric(metric: &str) -> bool {
    metric.split(['.', '-', '/']).next() == Some("balance")
}

/// One reading's instant in epoch nanoseconds; an unreadable timestamp
/// proves no freshness and orders before everything. Nanosecond precision
/// is the comparison itself: two readings inside one second still have an
/// order, and the newer one must win.
fn observed_instant(observed_at: &str) -> i128 {
    OffsetDateTime::parse(observed_at, &Rfc3339)
        .map(|at| at.unix_timestamp_nanos())
        .unwrap_or(i128::MIN)
}

/// Exact addition over the published decimal, through the same negated
/// subtraction the journal uses: `0.1` has no exact binary form and a total
/// is never a float.
fn add_exact(left: ExactNumber, right: ExactNumber) -> Option<ExactNumber> {
    let canonical = right.to_canonical_string();
    let negated = match canonical.strip_prefix('-') {
        Some(magnitude) => magnitude.to_owned(),
        None => format!("-{canonical}"),
    };
    left.checked_sub(ExactNumber::parse(&negated)?)
}

/// Escape one label value for the exposition format: backslash, quote and
/// newline are the only characters the grammar cannot carry.
fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// One `name{labels} value` line; labels arrive already sorted.
fn series(out: &mut String, name: &str, labels: &[(&str, &str)], value: &str) {
    out.push_str(name);
    out.push('{');
    for (index, (key, label)) in labels.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{key}=\"{}\"", escape_label(label));
    }
    let _ = writeln!(out, "}} {value}");
}

/// One family's `# HELP` and `# TYPE` header pair.
fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// The gateway request counters, per day and calling agent or model.
/// Returns whether any family truncated at the series limit.
fn write_gateway_families(out: &mut String, document: Option<&UsageDocument>) -> bool {
    let Some(document) = document else {
        return false;
    };
    let mut agents: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut models: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut agents_overflowed: BTreeSet<(String, String)> = BTreeSet::new();
    let mut models_overflowed: BTreeSet<(String, String)> = BTreeSet::new();
    for (day, usage) in &document.days {
        for (index, (agent, count)) in usage.agents.iter().enumerate() {
            let label = match index < MAX_MODELS_PER_DAY {
                true => agent.as_str(),
                false => FOLDED_LABEL,
            };
            accumulate(
                &mut agents,
                &mut agents_overflowed,
                (label.to_owned(), day.clone()),
                *count,
            );
        }
        for (index, (model, count)) in usage.models.iter().enumerate() {
            let label = match index < MAX_MODELS_PER_DAY {
                true => model.as_str(),
                false => FOLDED_LABEL,
            };
            accumulate(
                &mut models,
                &mut models_overflowed,
                (day.clone(), label.to_owned()),
                *count,
            );
        }
    }
    if !agents.is_empty() {
        header(
            out,
            "metera_gateway_agent_requests_total",
            "Requests the local LLM gateway handled, per day and calling agent.",
            "counter",
        );
        for ((agent, day), count) in agents.iter().take(MAX_EXPORTED_SERIES) {
            series(
                out,
                "metera_gateway_agent_requests_total",
                &[("agent", agent), ("day", day)],
                &count.to_string(),
            );
        }
    }
    if !models.is_empty() {
        header(
            out,
            "metera_gateway_model_requests_total",
            "Requests the local LLM gateway handled, per day and model.",
            "counter",
        );
        for ((day, model), count) in models.iter().take(MAX_EXPORTED_SERIES) {
            series(
                out,
                "metera_gateway_model_requests_total",
                &[("day", day), ("model", model)],
                &count.to_string(),
            );
        }
    }
    agents.len() > MAX_EXPORTED_SERIES
        || models.len() > MAX_EXPORTED_SERIES
        || !agents_overflowed.is_empty()
        || !models_overflowed.is_empty()
}

/// Fold one counted series into its family map with checked addition: a
/// total the counter cannot hold leaves the map and is remembered as
/// unrepresentable, so the exposition drops it and flags the scrape
/// instead of wrapping — or panicking under debug arithmetic.
fn accumulate(
    family: &mut BTreeMap<(String, String), u64>,
    overflowed: &mut BTreeSet<(String, String)>,
    key: (String, String),
    count: u64,
) {
    if overflowed.contains(&key) {
        return;
    }
    match family.get(&key).copied().unwrap_or(0).checked_add(count) {
        Some(total) => {
            family.insert(key, total);
        }
        None => {
            family.remove(&key);
            overflowed.insert(key);
        }
    }
}

/// The token totals and the unknown readings over the folded rows.
/// Returns whether any family truncated at the series limit.
fn write_token_families(out: &mut String, rows: &[TokenRow]) -> bool {
    // Model labels fold past the bound in a second pass, so a crowded
    // scrape stays bounded instead of dropping a total.
    // The unattributed bucket never consumes a model slot.
    let distinct: BTreeSet<String> = rows
        .iter()
        .map(|(_, model, ..)| model.clone())
        .filter(|model| model != UNATTRIBUTED_MODEL)
        .collect();
    let folded: BTreeSet<String> = distinct.into_iter().skip(MAX_MODELS_PER_DAY).collect();
    let mut totals: BTreeMap<TokenKey, ExactNumber> = BTreeMap::new();
    let mut unknown: BTreeMap<TokenKey, u64> = BTreeMap::new();
    // A series that could not be summed honestly — a counted reading that
    // did not parse, or an exact sum that overflowed — is tainted: it
    // leaves the totals and lands under the unknown readings instead of
    // emitting a silently incomplete total. Taint is not truncation: every
    // tainted reading is represented in `metera_usage_unknown_facts`, so
    // nothing was omitted.
    let mut tainted: BTreeSet<TokenKey> = BTreeSet::new();
    for (source, model, kind, counted, value) in rows {
        let model = match folded.contains(model) {
            true => FOLDED_LABEL.to_owned(),
            false => model.clone(),
        };
        let key = (source.clone(), model, kind.clone());
        if *counted {
            if tainted.contains(&key) {
                *unknown.entry(key).or_default() += 1;
            } else if let Some(value) = *value {
                match totals.remove(&key) {
                    Some(total) => match add_exact(total, value) {
                        Some(sum) => {
                            totals.insert(key, sum);
                        }
                        None => {
                            tainted.insert(key.clone());
                            *unknown.entry(key).or_default() += 1;
                        }
                    },
                    None => {
                        totals.insert(key, value);
                    }
                }
            } else {
                // A reading the journal counted but the projection cannot
                // parse has no honest number — withdraw any total the
                // series already emitted and taint it the way an overflow
                // does rather than skipping it silently.
                totals.remove(&key);
                tainted.insert(key.clone());
                *unknown.entry(key).or_default() += 1;
            }
        } else {
            *unknown.entry(key).or_default() += 1;
        }
    }
    if !totals.is_empty() {
        header(
            out,
            "metera_usage_tokens",
            "Token totals the usage journal retains, by host-bound source, model and token kind.",
            "gauge",
        );
        for ((source, model, kind), total) in totals.iter().take(MAX_EXPORTED_SERIES) {
            series(
                out,
                "metera_usage_tokens",
                &[("kind", kind), ("model", model), ("source", source)],
                &total.to_canonical_string(),
            );
        }
    }
    if !unknown.is_empty() {
        header(
            out,
            "metera_usage_unknown_facts",
            "Retained readings the producer did not count, by host-bound source, model and token kind.",
            "gauge",
        );
        for ((source, model, kind), count) in unknown.iter().take(MAX_EXPORTED_SERIES) {
            series(
                out,
                "metera_usage_unknown_facts",
                &[("kind", kind), ("model", model), ("source", source)],
                &count.to_string(),
            );
        }
    }
    totals.len() > MAX_EXPORTED_SERIES || unknown.len() > MAX_EXPORTED_SERIES
}

/// The balance gauges: one latest reading per retained balance fact — and
/// none at all where the latest reading is unknown, which is a state a
/// stale number must not keep answering for.
/// Returns whether the family truncated at the series limit.
fn write_balance_family(out: &mut String, latest: &BalanceLatest) -> bool {
    let current = || latest.values().filter(|(_, value)| value.is_some());
    if current().count() == 0 {
        return false;
    }
    header(
        out,
        "metera_balance_remaining",
        "Remaining account balance or quota share the journal retains, by scope and metric.",
        "gauge",
    );
    for ((metric, scope, source, unit), (_, value)) in latest
        .iter()
        .filter(|(_, (_, value))| value.is_some())
        .take(MAX_EXPORTED_SERIES)
    {
        let value = value.as_ref().expect("filtered to current readings");
        series(
            out,
            "metera_balance_remaining",
            &[
                ("metric", metric),
                ("scope", scope),
                ("source", source),
                ("unit", unit),
            ],
            &value.to_canonical_string(),
        );
    }
    current().count() > MAX_EXPORTED_SERIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::UsageDay;
    use crate::journal::SettlementEligibility;
    use token_usage_detector::usage::Quality;

    fn fact(
        fact_id: &str,
        scope: &str,
        metric: &str,
        value: Option<&str>,
        unit: &str,
        source: &str,
        observed_at: &str,
    ) -> MeteringFact {
        MeteringFact {
            fact_id: fact_id.to_owned(),
            scope_ref: scope.to_owned(),
            metric: metric.to_owned(),
            value: value.map(str::to_owned),
            unit: unit.to_owned(),
            quality: match value {
                Some(_) => Quality::Reported,
                None => Quality::Unknown,
            },
            eligibility: SettlementEligibility::SettlementEligible,
            source_ref: source.to_owned(),
            observed_at: observed_at.to_owned(),
        }
    }

    fn document() -> UsageDocument {
        UsageDocument {
            schema_version: "licoup.llm-gateway-usage.v1".to_owned(),
            days: BTreeMap::from([(
                "2026-07-15".to_owned(),
                UsageDay {
                    agents: BTreeMap::from([
                        ("claude-code".to_owned(), 2),
                        ("codex".to_owned(), 1),
                    ]),
                    models: BTreeMap::from([("kimi-k2".to_owned(), 3)]),
                },
            )]),
        }
    }

    #[test]
    fn the_exposition_is_deterministic_golden_text() {
        let facts = vec![
            fact(
                "agent:codex#reading:2026-07-14:kimi-k2#licoup.tokens.input",
                "agent:codex",
                "licoup.tokens.input",
                Some("100"),
                "tokens",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "agent:codex#reading:2026-07-15:kimi-k2#licoup.tokens.input",
                "agent:codex",
                "licoup.tokens.input",
                Some("75.50"),
                "tokens",
                "native:codex",
                "2026-07-15T00:00:00Z",
            ),
            fact(
                "agent:codex#reading:2026-07-14:kimi-k2#licoup.tokens.output",
                "agent:codex",
                "licoup.tokens.output",
                Some("20"),
                "tokens",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "agent:codex#reading:2026-07-14:kimi-k2:unknown#licoup.tokens.total",
                "agent:codex",
                "licoup.tokens.total",
                None,
                "tokens",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "account:acme#balance:credit#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.20"),
                "USD",
                "native:acme",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "account:acme#balance:credit#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.10"),
                "USD",
                "native:acme",
                "2026-07-15T00:00:00Z",
            ),
        ];
        let expected = "\
# HELP metera_balance_remaining Remaining account balance or quota share the journal retains, by scope and metric.
# TYPE metera_balance_remaining gauge
metera_balance_remaining{metric=\"balance.acme/credit.remaining\",scope=\"account:acme\",source=\"native:acme\",unit=\"USD\"} 4.10
# HELP metera_gateway_agent_requests_total Requests the local LLM gateway handled, per day and calling agent.
# TYPE metera_gateway_agent_requests_total counter
metera_gateway_agent_requests_total{agent=\"claude-code\",day=\"2026-07-15\"} 2
metera_gateway_agent_requests_total{agent=\"codex\",day=\"2026-07-15\"} 1
# HELP metera_gateway_model_requests_total Requests the local LLM gateway handled, per day and model.
# TYPE metera_gateway_model_requests_total counter
metera_gateway_model_requests_total{day=\"2026-07-15\",model=\"kimi-k2\"} 3
# HELP metera_usage_tokens Token totals the usage journal retains, by host-bound source, model and token kind.
# TYPE metera_usage_tokens gauge
metera_usage_tokens{kind=\"input\",model=\"kimi-k2\",source=\"native:codex\"} 175.50
metera_usage_tokens{kind=\"output\",model=\"kimi-k2\",source=\"native:codex\"} 20
# HELP metera_usage_unknown_facts Retained readings the producer did not count, by host-bound source, model and token kind.
# TYPE metera_usage_unknown_facts gauge
metera_usage_unknown_facts{kind=\"total\",model=\"kimi-k2\",source=\"native:codex\"} 1
# EOF
";
        assert_eq!(exposition(&facts, Some(&document())), expected);
        // A second read of the same inputs is byte-identical.
        assert_eq!(exposition(&facts, Some(&document())), expected);
    }

    #[test]
    fn a_scan_past_the_fact_cap_marks_the_exposition_truncated() {
        let facts: Vec<MeteringFact> = (0..3)
            .map(|index| {
                fact(
                    &format!("a#reading:2026-07-14:m{index}#licoup.tokens.input"),
                    "a",
                    "licoup.tokens.input",
                    Some("1"),
                    "tokens",
                    "native:a",
                    "2026-07-14T00:00:00Z",
                )
            })
            .collect();
        let mut projection = FactProjection::default();
        projection.scan(&facts, 2);
        assert_eq!(projection.scanned, 2);
        assert!(projection.truncated);
        let text = projection.exposition(None);
        assert!(text.contains("metera_export_truncated{} 1"));
        // The uncapped path stays silent about truncation.
        assert!(!exposition(&facts, None).contains("metera_export_truncated"));
    }

    #[test]
    fn the_newest_balance_reading_decides_whether_a_gauge_exists() {
        // A newer unknown supersedes the older number rather than leaving
        // the stale gauge published.
        let facts = vec![
            fact(
                "a#b#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.10"),
                "USD",
                "native:acme",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "a#c#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                None,
                "USD",
                "native:acme",
                "2026-07-15T00:00:00Z",
            ),
        ];
        assert!(!exposition(&facts, None).contains("metera_balance_remaining{"));
        // An older unknown does not shadow a newer number either.
        let facts = vec![
            fact(
                "a#b#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                None,
                "USD",
                "native:acme",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "a#c#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.10"),
                "USD",
                "native:acme",
                "2026-07-15T00:00:00Z",
            ),
        ];
        assert!(exposition(&facts, None).contains("metera_balance_remaining{"));
    }

    #[test]
    fn balance_recency_compares_instants_not_spellings() {
        // "02:00:00+02:00" sorts after "00:30:00Z" as a string but names the
        // earlier instant: recency follows the instant, so the 4.10 reading
        // stays the gauge.
        let facts = vec![
            fact(
                "a#b#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.10"),
                "USD",
                "native:acme",
                "2026-07-14T00:30:00Z",
            ),
            fact(
                "a#c#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("99"),
                "USD",
                "native:acme",
                "2026-07-14T02:00:00+02:00",
            ),
        ];
        assert!(
            exposition(&facts, None).contains("metera_balance_remaining{metric=\"balance.acme/credit.remaining\",scope=\"account:acme\",source=\"native:acme\",unit=\"USD\"} 4.10")
        );

        // A negative offset does the same for an unknown reading: its
        // "23:59:00-01:00" instant is the newer one even though the string
        // sorts before the older numeric reading's "Z" spelling.
        let facts = vec![
            fact(
                "a#b#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                Some("4.10"),
                "USD",
                "native:acme",
                "2026-07-14T00:10:00Z",
            ),
            fact(
                "a#c#balance.acme/credit.remaining",
                "account:acme",
                "balance.acme/credit.remaining",
                None,
                "USD",
                "native:acme",
                "2026-07-13T23:30:00-01:00",
            ),
        ];
        assert!(!exposition(&facts, None).contains("metera_balance_remaining{"));
    }

    #[test]
    fn a_tainted_series_reports_unknown_without_claiming_truncation() {
        // A counted reading the exact parser cannot hold taints its series:
        // the unknown family carries it, and nothing was actually omitted —
        // so the truncation marker stays silent.
        let facts = vec![fact(
            "a#reading:2026-07-14:m#licoup.tokens.input",
            "a",
            "licoup.tokens.input",
            Some("9999999999999999999999999999"),
            "tokens",
            "native:a",
            "2026-07-14T00:00:00Z",
        )];
        let text = exposition(&facts, None);
        assert!(text.contains("metera_usage_unknown_facts{kind=\"input\""));
        assert!(!text.contains("metera_usage_tokens{"));
        assert!(!text.contains("metera_export_truncated"));
    }

    #[test]
    fn each_family_marks_the_exposition_when_its_series_truncates() {
        // Balance: one series over the cap.
        let facts: Vec<MeteringFact> = (0..=MAX_EXPORTED_SERIES)
            .map(|index| {
                fact(
                    &format!("s#id:{index}#balance.x/m{index}"),
                    "s",
                    &format!("balance.x/m{index}"),
                    Some("1"),
                    "USD",
                    "native:a",
                    "2026-07-14T00:00:00Z",
                )
            })
            .collect();
        assert!(exposition(&facts, None).contains("metera_export_truncated{} 1"));

        // Tokens: distinct sources past the cap.
        let facts: Vec<MeteringFact> = (0..=MAX_EXPORTED_SERIES)
            .map(|index| {
                fact(
                    "s#reading:2026-07-14:m#licoup.tokens.input",
                    "s",
                    "licoup.tokens.input",
                    Some("1"),
                    "tokens",
                    &format!("native:{index}"),
                    "2026-07-14T00:00:00Z",
                )
            })
            .collect();
        assert!(exposition(&facts, None).contains("metera_export_truncated{} 1"));

        // Gateway: one agent over more days than the series cap.
        let mut document = document();
        for index in 0..=MAX_EXPORTED_SERIES {
            document.days.insert(
                format!("day-{index:04}"),
                UsageDay {
                    agents: BTreeMap::from([("codex".to_owned(), 1)]),
                    models: BTreeMap::new(),
                },
            );
        }
        assert!(exposition(&[], Some(&document)).contains("metera_export_truncated{} 1"));
    }

    #[test]
    fn an_unsumable_series_leaves_the_totals_and_reports_unknown() {
        // A reading past the decimal bound cannot join the exact sum; the
        // series must not emit the remaining addends as a complete total.
        let facts = vec![
            // A valid reading first, then a malformed one for the same
            // series: the earlier total must be withdrawn, not emitted
            // beside the unknown reading.
            fact(
                "s#reading:2026-07-14:m#licoup.tokens.input",
                "s",
                "licoup.tokens.input",
                Some("2"),
                "tokens",
                "native:a",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "s#reading:2026-07-15:m#licoup.tokens.input",
                "s",
                "licoup.tokens.input",
                Some("9999999999999999999999999999"),
                "tokens",
                "native:a",
                "2026-07-15T00:00:00Z",
            ),
        ];
        let text = exposition(&facts, None);
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with("metera_usage_tokens{")),
            "a tainted series emits no precise-looking total: {text}"
        );
        assert!(
            text.contains(
                "metera_usage_unknown_facts{kind=\"input\",model=\"m\",source=\"native:a\"} 1"
            ),
            "the unsumable series reports under unknown facts: {text}"
        );
        // Taint is represented in the unknown family, not omitted data: the
        // truncation marker stays silent.
        assert!(!text.contains("metera_export_truncated"), "{text}");
    }

    #[test]
    fn an_unrepresentable_gateway_count_drops_the_aggregate_and_truncates() {
        // Two series folding into "Other" can out-sum the counter: the
        // wrapped total must never reach the exposition — the aggregate
        // drops and the scrape admits it summarized.
        let mut agents = BTreeMap::new();
        let mut models = BTreeMap::new();
        for index in 0..MAX_MODELS_PER_DAY {
            agents.insert(format!("agent-{index}"), 1);
            models.insert(format!("model-{index}"), 1);
        }
        agents.insert("zz-fold-a".to_owned(), u64::MAX);
        agents.insert("zz-fold-b".to_owned(), 1);
        models.insert("zz-fold-a".to_owned(), u64::MAX);
        models.insert("zz-fold-b".to_owned(), 1);
        let document = UsageDocument {
            schema_version: "licoup.llm-gateway-usage.v1".to_owned(),
            days: BTreeMap::from([("2026-07-15".to_owned(), UsageDay { agents, models })]),
        };
        let text = exposition(&[], Some(&document));
        assert!(
            !text.contains("metera_gateway_agent_requests_total{agent=\"Other\""),
            "an unrepresentable folded total is dropped, not wrapped: {text}"
        );
        assert!(
            !text.contains("metera_gateway_model_requests_total{model=\"Other\""),
            "{text}"
        );
        assert!(
            text.contains("metera_gateway_agent_requests_total{agent=\"agent-0\""),
            "representable series still export: {text}"
        );
        assert!(text.contains("metera_export_truncated{} 1"), "{text}");
    }

    #[test]
    fn empty_state_is_a_terminator_only() {
        assert_eq!(exposition(&[], None), "# EOF\n");
        assert!(exposition(&[], Some(&document())).contains("gateway"));
    }

    #[test]
    fn labels_escape_and_non_model_identities_stay_unattributed() {
        let facts = vec![fact(
            "scope#measurement:call-1#licoup.tokens.input",
            "scope",
            "licoup.tokens.input",
            Some("5"),
            "tokens",
            "native:\"quoted\"",
            "2026-07-14T00:00:00Z",
        )];
        let text = exposition(&facts, None);
        assert!(text.contains(
            "metera_usage_tokens{kind=\"input\",model=\"Others\",source=\"native:\\\"quoted\\\"\"} 5"
        ));
    }

    #[test]
    fn foreign_units_and_metrics_are_not_token_totals() {
        let facts = vec![
            fact(
                "agent:codex#reading:2026-07-14:m#licoup.cost",
                "agent:codex",
                "licoup.cost",
                Some("0.01"),
                "USD",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
            fact(
                "agent:codex#reading:2026-07-14:m#licoup.tokens.input",
                "agent:codex",
                "licoup.tokens.input",
                Some("9"),
                "requests",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
        ];
        assert_eq!(exposition(&facts, None), "# EOF\n");
    }

    #[test]
    fn provider_native_token_counters_keep_their_leaf_kind() {
        let facts = vec![
            fact(
                "agent:codex#reading:2026-07-14:kimi-k2#openai.usage/cache_write_tokens",
                "agent:codex",
                "openai.usage/cache_write_tokens",
                Some("40"),
                "tokens",
                "native:codex",
                "2026-07-14T00:00:00Z",
            ),
            // A remaining-token quota is a balance gauge, never a consumption
            // total, even though its unit is tokens.
            fact(
                "account:acme#balance:quota#balance.acme/quota.tokens",
                "account:acme",
                "balance.acme/quota.tokens",
                Some("9000"),
                "tokens",
                "native:acme",
                "2026-07-14T00:00:00Z",
            ),
        ];
        let text = exposition(&facts, None);
        assert!(text.contains(
            "metera_usage_tokens{kind=\"cache_write_tokens\",model=\"kimi-k2\",source=\"native:codex\"} 40"
        ));
        assert!(text.contains(
            "metera_balance_remaining{metric=\"balance.acme/quota.tokens\",scope=\"account:acme\",source=\"native:acme\",unit=\"tokens\"} 9000"
        ));
        assert!(!text.contains("kind=\"tokens\""));
    }

    #[test]
    fn render_pages_through_the_read_port() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("metera-export-{nonce}"));
        std::fs::create_dir_all(&root).expect("synthetic root");
        let params = serde_json::json!({"stateRoot": root.to_string_lossy()});
        let mut host = UsageSourceHost::open(&params).expect("host");
        host.record_all(
            "native:codex",
            vec![
                fact(
                    "agent:codex#reading:2026-07-14:kimi-k2#licoup.tokens.input",
                    "agent:codex",
                    "licoup.tokens.input",
                    Some("5"),
                    "tokens",
                    "native:codex",
                    "2026-07-14T00:00:00Z",
                ),
                fact(
                    "agent:codex#reading:2026-07-15:kimi-k2#licoup.tokens.input",
                    "agent:codex",
                    "licoup.tokens.input",
                    Some("7"),
                    "tokens",
                    "native:codex",
                    "2026-07-15T00:00:00Z",
                ),
            ],
        )
        .expect("recorded");
        let text = render(&host, Some(&document())).expect("exposition");
        assert!(text.contains(
            "metera_usage_tokens{kind=\"input\",model=\"kimi-k2\",source=\"native:codex\"} 12"
        ));
        assert!(text.contains("metera_gateway_agent_requests_total"));
        let _ = std::fs::remove_dir_all(root);
    }
}
