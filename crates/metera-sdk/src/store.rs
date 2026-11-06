use acop_usage_core::{InputAccounting, ReportedTokens};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Counts {
    pub prompt: Option<u64>,
    pub cached: Option<u64>,
    pub completion: Option<u64>,
    pub total: Option<u64>,
    pub requests: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCounts {
    pub model: Option<String>,
    pub counts: Counts,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageCounter {
    pub key: String,
    pub reported: ReportedTokens,
    pub counts: Counts,
    pub models: Vec<ModelCounts>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAllocation {
    pub counter_key: String,
    pub model: Option<String>,
    pub counts: Counts,
    pub first_seen: f64,
    pub last_seen: f64,
    pub time_inferred: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageIssue {
    UnknownMetric,
    UnknownCoverage,
    ConflictingCounters,
    IncompleteAttribution,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUsage {
    pub id: String,
    pub input_accounting: InputAccounting,
    pub started_at: f64,
    pub last_activity_at: f64,
    pub counts: Counts,
    pub models: Vec<ModelCounts>,
    pub counters: Vec<UsageCounter>,
    pub allocations: Vec<UsageAllocation>,
    pub allocation_complete: bool,
    pub issues: Vec<UsageIssue>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageCursor {
    pub id: String,
    pub input_accounting: InputAccounting,
    pub counts: Counts,
    pub models: Vec<ModelCounts>,
    pub counters: Vec<UsageCounter>,
    pub allocation_complete: bool,
    pub issues: Vec<UsageIssue>,
}

impl From<&SessionUsage> for UsageCursor {
    fn from(session: &SessionUsage) -> Self {
        Self {
            id: session.id.clone(),
            input_accounting: session.input_accounting,
            counts: session.counts,
            models: session.models.clone(),
            counters: session.counters.clone(),
            allocation_complete: session.allocation_complete,
            issues: session.issues.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdvanceInput {
    pub previous: Option<UsageCursor>,
    pub current: UsageCursor,
    pub count_initial: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdvanceKind {
    Baseline,
    Delta,
    Reset,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAdvance {
    pub kind: AdvanceKind,
    pub delta: Option<Counts>,
    pub models: Vec<ModelCounts>,
    pub counters: Vec<CounterAdvance>,
    pub attribution_gap: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CounterAdvance {
    pub key: String,
    pub delta: Counts,
    pub models: Vec<ModelCounts>,
    pub attribution_gap: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreUsageFailure {
    CounterOverflow,
    IncompleteReading,
    InconsistentReading,
    SeriesMismatch,
}

impl std::fmt::Display for StoreUsageFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::CounterOverflow => "usage_counter_overflow",
            Self::IncompleteReading => "usage_reading_incomplete",
            Self::InconsistentReading => "usage_reading_inconsistent",
            Self::SeriesMismatch => "usage_series_mismatch",
        })
    }
}

impl std::error::Error for StoreUsageFailure {}
