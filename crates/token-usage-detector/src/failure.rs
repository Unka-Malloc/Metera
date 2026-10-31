//! The structured refusal the usage vocabulary produces.
//!
//! A refusal is a fact the caller reconciles on — a stable code and stage, a
//! retryability flag, the offending field when the refusal is about one, and
//! the recovery vocabulary the wire publishes. Secret bytes never appear in
//! any field.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// What the caller may do next, published in the wire vocabulary.
///
/// `wire()` is the single spelling: serialization goes through it and
/// deserialization is its inverse, so the name the SDK hands a consumer and
/// the name the report carries can never drift into two contracts. The
/// spellings it produces are the only ones this crate ever published —
/// there is no released predecessor on disk or on the wire, so no legacy
/// alias is carried and an unknown spelling is refused outright.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecoveryAction {
    /// The request itself was wrong; fix it before retrying.
    #[default]
    CorrectRequest,
    /// The dependency is temporarily unhealthy; retry after it recovers.
    RetryAfterRecovery,
    /// Reconcile against the durable record, then decide.
    ReconcileBeforeRetry,
}

impl RecoveryAction {
    /// The value the report's refusal vocabulary carries.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::CorrectRequest => "correct_request_and_retry",
            Self::RetryAfterRecovery => "retry_after_recovery",
            Self::ReconcileBeforeRetry => "reconcile_before_retry",
        }
    }

    /// The inverse of [`wire`](Self::wire): the vocabulary back into the
    /// action, or `None` for a spelling the contract never published.
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::CorrectRequest,
            Self::RetryAfterRecovery,
            Self::ReconcileBeforeRetry,
        ]
        .into_iter()
        .find(|action| action.wire() == text)
    }
}

impl Serialize for RecoveryAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire())
    }
}

impl<'de> Deserialize<'de> for RecoveryAction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format_args!("unknown recovery action {text:?}"))
        })
    }
}

/// A refusal with a stable code: the shape the detectors, the journal and the
/// report paths share so callers reconcile on `code`, never on message text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFailure {
    /// Stable reason code, identical through either interface.
    pub code: String,
    /// Where it happened, in `area/action` vocabulary.
    pub stage: String,
    /// The component that produced the failure — a stable published name,
    /// never a sentence. Boxed because a refusal is returned by value from
    /// every operation and this name is written once and never grows.
    pub component: Box<str>,
    /// Whether the caller may retry as-is.
    pub retryable: bool,
    /// The recovery vocabulary the wire publishes.
    pub recovery: RecoveryAction,
    /// The offending field when the refusal is about one.
    pub field: Option<String>,
    /// Public arguments the caller may show — bounded facts only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub presentation_args: BTreeMap<String, String>,
}

impl UsageFailure {
    /// A refusal the caller cannot fix by retrying.
    pub fn permanent(code: impl Into<String>, stage: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            component: Box::default(),
            retryable: false,
            recovery: RecoveryAction::CorrectRequest,
            field: None,
            presentation_args: BTreeMap::new(),
        }
    }

    /// A refusal the caller may retry once the dependency recovers.
    pub fn retryable(code: impl Into<String>, stage: impl Into<String>) -> Self {
        Self {
            retryable: true,
            recovery: RecoveryAction::RetryAfterRecovery,
            ..Self::permanent(code, stage)
        }
    }

    /// Name the producing component.
    pub fn with_component(mut self, component: &str) -> Self {
        self.component = component.into();
        self
    }

    /// Name the recovery the caller should take next.
    pub fn with_recovery(mut self, recovery: RecoveryAction) -> Self {
        self.recovery = recovery;
        self
    }

    /// Name the offending field.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Publish one public argument. Callers publish bounded facts only —
    /// never secret material.
    pub fn with_presentation_arg(mut self, key: &str, value: &str) -> Self {
        self.presentation_args
            .insert(key.to_owned(), value.to_owned());
        self
    }
}

impl fmt::Display for UsageFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{} at {} ({field})", self.code, self.stage),
            None => write!(f, "{} at {}", self.code, self.stage),
        }
    }
}

impl std::error::Error for UsageFailure {}

/// A permanent refusal about the input or the transport frame that carries it.
///
/// Every refusal this crate constructs names the detector component, so a
/// consumer can distinguish a usage-source refusal from a business failure on
/// the wire without parsing text.
pub fn refusal(code: &str, stage: &str) -> UsageFailure {
    UsageFailure::permanent(code, stage).with_component(crate::COMPONENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recovery_vocabulary_is_one_spelling() {
        // The name the SDK hands a consumer and the name the report
        // serializes are the same contract: `wire()` both ways.
        for action in [
            RecoveryAction::CorrectRequest,
            RecoveryAction::RetryAfterRecovery,
            RecoveryAction::ReconcileBeforeRetry,
        ] {
            let encoded = serde_json::to_string(&action).expect("serialize");
            assert_eq!(encoded, format!("\"{}\"", action.wire()));
            assert_eq!(
                serde_json::from_str::<RecoveryAction>(&encoded).expect("deserialize"),
                action
            );
            assert_eq!(RecoveryAction::parse(action.wire()), Some(action));
        }
        // The derive-era snake_case spelling is not a second contract.
        assert!(serde_json::from_str::<RecoveryAction>("\"correct_request\"").is_err());
    }
}
