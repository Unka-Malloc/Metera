//! Token usage detection: one input mapping for three collection directions.
//!
//! A usage source is a producer of *observations*, not of charges, and it may
//! deliver them in any of the three shapes the C11 profile allows:
//!
//! - **push** — `usage.publish` sends a bounded batch of observations;
//! - **bounded pull** — `usage.query(cursor, limit, scopeRef)` asks for one
//!   bounded page;
//! - **cursor** — the same query resumed from the position a previous page
//!   returned.
//!
//! This crate makes those three directions converge on exactly one mapped value:
//! [`BoundObservation`] — a [`usage::UsageObservation`] whose source identity, epoch,
//! authorized scope and (when the host knows it) logical measurement were
//! attached by the transport, never by the payload. [`collection`] carries the
//! three directions and their bounds, [`binding`] performs the attachment and
//! the refusals, [`normalize`] maps vendor-shaped records (a field-mapped JSON
//! record, a log line, an OTLP-shaped cumulative series) onto the same
//! observation at the boundary, and [`describe`] is the `usage.describe`
//! declaration a source publishes about its own fields and series.
//!
//! Four rules are structural here rather than conventional:
//!
//! - **A source never names itself.** A payload that carries `source`,
//!   `extensionId`, `instanceId`, a generation, a registry epoch, a scope or a
//!   measurement identity is refused; those are the host's facts about the
//!   binding, and accepting them would let a producer attach itself to another
//!   user's run or merge two calls into one charge.
//! - **A cursor belongs to one epoch and one scope.** Resuming from a cursor
//!   whose epoch was replaced is refused, because silently skipping or
//!   re-reading a cumulative series is how a reset turns into a wrong total.
//! - **A missing metric is absent, never zero.** Tokens, cost and model are
//!   optional beyond the observation's identity, and a producer that does not
//!   know a count says [`usage::Quality::Unknown`] rather than claiming nothing
//!   happened.
//! - **A batch is bounded.** The number of observations and the encoded frame
//!   both have limits, so a chatty source cannot turn into a client that stops
//!   responding.
//!
//! The deduplication, correction, cumulative-reset and multi-source
//! single-settlement semantics are the *consumer's* side of this contract; they
//! live with the optional analytics package that reads these values, not in the
//! producer SDK. This crate never computes a charge, never stores a ledger and
//! never opens a network connection.

pub mod agent;
pub mod binding;
pub mod collection;
pub mod describe;
pub mod failure;
pub mod framing;
pub mod metrics;
pub mod names;
pub mod normalize;
pub mod provider;
pub mod usage;

pub use agent::TokenUsage;
pub use binding::{BatchAdmission, BoundObservation, BoundObservationKey, SourceBinding};
pub use collection::{CollectionMode, Cursor, PublishBatch, QueryPage, QueryRequest};
pub use describe::{Aggregation, MetricField, SeriesDeclaration, SourceDescription};
pub use failure::{RecoveryAction, UsageFailure};
pub use metrics::standard;
pub use provider::{ProviderMapping, ProviderUsage};

/// The component every failure from this SDK names, so a client can tell a
/// usage-source refusal from a business one without parsing text.
pub(crate) const COMPONENT: &str = "token_usage_detector";

/// The stage every structural refusal reports.
pub(crate) const STAGE: &str = "usage-source";

/// A permanent refusal about the shape or the binding of an input.
pub(crate) fn refusal(code: &str) -> UsageFailure {
    failure::refusal(code, STAGE)
}

/// A refusal whose next step is real for the caller: fix the request, or
/// reconcile against the durable record before retrying.
pub(crate) fn refusal_with(code: &str, recovery: RecoveryAction) -> UsageFailure {
    refusal(code).with_recovery(recovery)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::UsageObservation;
    use serde_json::json;

    /// The detector maps inputs onto the published contract; it never names a
    /// consuming kernel, so no host crate may appear in its manifest. The
    /// guarded set is the crates that consume this one today — the
    /// `agent-statistics` kernel and `balance-detector` sibling, the `a4aa`
    /// host adapter, the `acop-usage-source` read adapter and the
    /// `fairliny-gateway` — the same direction rule `agent-statistics`
    /// applies to the analytics package above it.
    #[test]
    fn the_manifest_names_no_consumer() {
        let manifest = include_str!("../Cargo.toml");
        for consumer in [
            "agent-statistics",
            "balance-detector",
            "a4aa",
            "acop-usage-source",
            "fairliny-gateway",
        ] {
            assert!(
                !manifest.contains(consumer),
                "{consumer} is above, not below"
            );
        }
    }

    #[test]
    fn usage_refusals_publish_the_detector_component() {
        assert_eq!(
            failure::refusal("usage_mapping_invalid", "usage/validate")
                .component
                .as_ref(),
            COMPONENT
        );
        assert_eq!(
            refusal("usage_mapping_invalid").component.as_ref(),
            COMPONENT
        );
        let failure = UsageObservation::from_value(json!({"source": "self-asserted"}))
            .expect_err("a payload cannot name its own source");
        assert_eq!(failure.code, "usage_source_self_asserted");
        assert_eq!(
            failure.component.as_ref(),
            COMPONENT,
            "validation refusals must not lose the component on the wire"
        );
        let wire = serde_json::to_value(&failure).unwrap();
        assert_eq!(wire["component"], COMPONENT);
    }
}
