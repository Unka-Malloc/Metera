//! C11: usage observations and specialist metrics.
//!
//! A usage source is a producer of *observations*, not of charges. It reports
//! what it saw; the host decides what that means for budget and settlement. Three
//! consequences follow, and each is enforced below:
//!
//! - **The source is bound by the transport, never self-declared.** An
//!   observation carries no `extensionId`, no `instanceId` and no principal: the
//!   host already knows which extension and which invocation it is reading from.
//!   A payload that tries to assert one is refused, so no producer can attach
//!   itself to another user's run.
//! - **Correcting is not offsetting.** A correction is a new revision of the same
//!   observation; a retraction withdraws that observation. Neither is a way to
//!   subtract someone else's usage.
//! - **Unknown is a value.** A count the producer does not know is
//!   [`Quality::Unknown`] with no number at all — not a zero, which would be a
//!   claim that nothing happened.
//!
//! Money is held as an exact decimal string and an ISO 4217 code. Binary floating
//! point never enters a ledger, and a cumulative counter is differenced only
//! within one epoch, one series and one origin ([`cumulative_delta`]), so a reset
//! is a new origin rather than a negative amount of work.

use crate::failure::{UsageFailure, refusal};
use crate::names::{is_authority_field, is_namespaced};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const STAGE: &str = "usage/validate";

/// The most fractional digits an exact decimal may carry.
pub const MAX_DECIMAL_SCALE: u32 = 9;

/// The most integer digits an exact decimal may carry, so a scaled value always
/// fits an `i128` without wrapping.
pub const MAX_DECIMAL_DIGITS: usize = 27;

/// The longest observation identity accepted.
pub const MAX_OBSERVATION_ID_BYTES: usize = 160;

/// Fields that describe the *transport binding* rather than the observation.
///
/// They are refused in a payload because the host knows them better than the
/// producer does: who the source is, which instance it came from, and which
/// authority the read is under.
pub const TRANSPORT_BOUND_FIELDS: &[&str] = &[
    "source",
    "extensionid",
    "instanceid",
    "instance",
    "generation",
    "registryepoch",
    "pluginid",
];

/// How a metric value was produced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    /// The producer says it counted it.
    Reported,
    /// The producer says it derived it.
    Estimated,
    /// The producer does not know it. The value is absent, never zero.
    Unknown,
}

impl Quality {
    /// Whether this quality carries a number.
    pub const fn has_value(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// How a metric value relates to the window it was observed in.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Temporality {
    /// The change over the window.
    Delta,
    /// The total since the series origin. Only comparable to another cumulative
    /// value from the same origin.
    Cumulative,
    /// A point-in-time reading, which cannot be summed.
    Gauge,
    /// A reading that is exactly the total for its scope.
    Absolute,
}

/// One metric reading.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricValue {
    /// The exact decimal text, or `None` when the quality is unknown.
    #[serde(default)]
    pub value: Option<String>,
    pub unit: String,
    pub temporality: Temporality,
    pub quality: Quality,
    /// Metrics this reading already contains, so a total is never added to its
    /// own subset — cached input inside input tokens, for example.
    #[serde(default)]
    pub includes: Vec<String>,
}

impl MetricValue {
    /// A reading the producer counted.
    pub fn reported(
        value: impl Into<String>,
        unit: impl Into<String>,
        temporality: Temporality,
    ) -> Self {
        Self {
            value: Some(value.into()),
            unit: unit.into(),
            temporality,
            quality: Quality::Reported,
            includes: Vec::new(),
        }
    }

    /// A reading the producer does not know. There is no number, because a zero
    /// would be a claim.
    pub fn unknown(unit: impl Into<String>, temporality: Temporality) -> Self {
        Self {
            value: None,
            unit: unit.into(),
            temporality,
            quality: Quality::Unknown,
            includes: Vec::new(),
        }
    }

    pub fn with_includes<S: Into<String>>(mut self, includes: impl IntoIterator<Item = S>) -> Self {
        self.includes = includes.into_iter().map(Into::into).collect();
        self
    }

    /// The exact number, when there is one.
    pub fn exact(&self) -> Option<ExactNumber> {
        self.value.as_deref().and_then(ExactNumber::parse)
    }

    /// Structural validation: unit present, value present exactly when the
    /// quality has one, and every `includes` entry namespaced.
    pub fn validate(&self, metric: &str) -> Result<(), UsageFailure> {
        if self.unit.is_empty() {
            return Err(refusal("usage_metric_invalid", STAGE).with_field("metrics.unit"));
        }
        if self.quality.has_value() != self.value.is_some() {
            return Err(refusal("usage_metric_invalid", STAGE).with_field("metrics.value"));
        }
        if self
            .value
            .as_deref()
            .is_some_and(|value| ExactNumber::parse(value).is_none())
        {
            return Err(refusal("usage_metric_invalid", STAGE).with_field("metrics.value"));
        }
        for included in &self.includes {
            if !is_namespaced(included) || included == metric {
                return Err(refusal("usage_metric_invalid", STAGE).with_field("metrics.includes"));
            }
        }
        Ok(())
    }
}

/// A cost as an estimate or a report, never as an assertion about the user's
/// budget.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostObservation {
    /// Exact decimal text, or `None` when the quality is unknown.
    #[serde(default)]
    pub amount: Option<String>,
    /// ISO 4217, three uppercase letters.
    pub currency: String,
    pub quality: Quality,
}

impl CostObservation {
    pub fn validate(&self) -> Result<(), UsageFailure> {
        if self.currency.len() != 3 || !self.currency.bytes().all(|byte| byte.is_ascii_uppercase())
        {
            return Err(refusal("usage_cost_invalid", STAGE).with_field("cost.currency"));
        }
        if self.quality.has_value() != self.amount.is_some() {
            return Err(refusal("usage_cost_invalid", STAGE).with_field("cost.amount"));
        }
        if self
            .amount
            .as_deref()
            .is_some_and(|amount| ExactNumber::parse(amount).is_none())
        {
            return Err(refusal("usage_cost_invalid", STAGE).with_field("cost.amount"));
        }
        Ok(())
    }
}

/// Whether an observation asserts a fact or withdraws one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsageOperation {
    /// Report or revise an observation.
    Upsert,
    /// Withdraw this observation. It cancels this observation's own effect and
    /// nothing else.
    Retract,
}

/// The identity a revision corrects.
///
/// The source is part of the key even though it is not in the payload: the host
/// binds it at the transport, so the same observation id from two sources stays
/// two observations.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationKey {
    pub source_epoch: String,
    pub observation_id: String,
}

/// One usage observation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageObservation {
    pub schema: String,
    pub observation_id: String,
    /// A correction raises the revision. An older revision of the same key never
    /// replaces a newer one.
    pub revision: u32,
    pub operation: UsageOperation,
    /// Reset and replay defence. A new epoch starts new cumulative series.
    pub source_epoch: String,
    /// The invocation or the authorized aggregate range this observation belongs
    /// to, issued by the host.
    pub scope_ref: String,
    /// UTC, as observed by the producer. An anomalous clock is preserved as the
    /// source's own fact and never overrides the host's monotonic clock.
    pub observed_at: String,
    /// The start of the window, or of the cumulative series.
    #[serde(default)]
    pub interval_start: Option<String>,
    /// Namespaced metric to reading. At least one on an upsert.
    pub metrics: BTreeMap<String, MetricValue>,
    #[serde(default)]
    pub cost: Option<CostObservation>,
}

impl UsageObservation {
    /// The key a correction addresses.
    pub fn key(&self) -> ObservationKey {
        ObservationKey {
            source_epoch: self.source_epoch.clone(),
            observation_id: self.observation_id.clone(),
        }
    }

    /// Whether `self` supersedes `existing`, which is true only for the same
    /// observation with a strictly higher revision.
    pub fn supersedes(&self, existing: &Self) -> bool {
        self.key() == existing.key() && self.revision > existing.revision
    }

    /// Structural validation, including the rule that a payload may not assert
    /// its own source.
    pub fn validate(&self) -> Result<(), UsageFailure> {
        if self.schema != self::wire::USAGE {
            return Err(refusal("usage_observation_invalid", STAGE).with_field("schema"));
        }
        if self.observation_id.is_empty()
            || self.observation_id.len() > MAX_OBSERVATION_ID_BYTES
            || self.source_epoch.is_empty()
            || self.scope_ref.is_empty()
            || self.observed_at.is_empty()
            || self.revision < 1
        {
            return Err(refusal("usage_observation_invalid", STAGE).with_field("identity"));
        }
        match self.operation {
            UsageOperation::Upsert if self.metrics.is_empty() => {
                return Err(refusal("usage_observation_invalid", STAGE).with_field("metrics"));
            }
            UsageOperation::Retract if !self.metrics.is_empty() => {
                return Err(refusal("usage_observation_invalid", STAGE).with_field("metrics"));
            }
            _ => {}
        }
        for (metric, reading) in &self.metrics {
            if !is_namespaced(metric) {
                return Err(refusal("usage_observation_invalid", STAGE).with_field("metrics"));
            }
            reading.validate(metric)?;
        }
        if let Some(cost) = &self.cost {
            cost.validate()?;
        }
        Ok(())
    }

    /// Read one observation from its wire form.
    ///
    /// A payload carrying `source`, `extensionId`, `instanceId`, a generation, a
    /// registry epoch, or any authority field is refused: those are the host's
    /// facts about the binding, and accepting them from the payload would let a
    /// producer name a source it is not.
    pub fn from_value(value: Value) -> Result<Self, UsageFailure> {
        if let Some(field) = self_asserted_binding_field(&value)? {
            return Err(refusal("usage_source_self_asserted", STAGE).with_field(&field));
        }
        let observation: Self = serde_json::from_value(value)
            .map_err(|_| refusal("usage_observation_invalid", STAGE).with_field("observation"))?;
        observation.validate()?;
        Ok(observation)
    }
}

/// The traversal work one wire payload may demand of the field scans. Every
/// visited node spends the budget — the members of an empty container and the
/// items of an array alike — so a payload cannot hide unbounded scan work
/// inside shape the counter model never reads, and the depth cap bounds the
/// recursion itself. A payload that outruns either bound is refused rather
/// than under-scanned: a check that could not finish has not passed.
const MAX_WIRE_NODES: usize = 4096;
const MAX_WIRE_DEPTH: usize = 16;

/// The dotted path of the first field `condemns` condemns in `value`, or a
/// refusal when the scan could not complete inside the wire bounds.
pub(crate) fn first_field_matching(
    value: &Value,
    condemns: impl Fn(&str) -> bool,
) -> Result<Option<String>, UsageFailure> {
    struct Scan<'a> {
        condemns: &'a dyn Fn(&str) -> bool,
        remaining: usize,
    }

    impl Scan<'_> {
        fn walk(
            &mut self,
            value: &Value,
            path: &str,
            depth: usize,
        ) -> Result<Option<String>, UsageFailure> {
            if self.remaining == 0 || depth > MAX_WIRE_DEPTH {
                return Err(refusal("usage_observation_invalid", STAGE)
                    .with_field("observation")
                    .with_presentation_arg("limit", &MAX_WIRE_NODES.to_string()));
            }
            self.remaining -= 1;
            match value {
                Value::Object(map) => {
                    for (key, nested) in map {
                        let child = format!("{path}{key}");
                        if (self.condemns)(key) {
                            return Ok(Some(child));
                        }
                        if let Some(found) = self.walk(nested, &format!("{child}."), depth + 1)? {
                            return Ok(Some(found));
                        }
                    }
                }
                Value::Array(items) => {
                    for (index, nested) in items.iter().enumerate() {
                        if let Some(found) =
                            self.walk(nested, &format!("{path}{index}."), depth + 1)?
                        {
                            return Ok(Some(found));
                        }
                    }
                }
                _ => {}
            }
            Ok(None)
        }
    }

    Scan {
        condemns: &condemns,
        remaining: MAX_WIRE_NODES,
    }
    .walk(value, "", 0)
}

/// The dotted path of the first field in `value` that asserts a transport-bound
/// fact, if any — or a refusal when the payload outruns the scan bounds, in
/// which case "no self-asserted field" is a claim the check cannot make.
pub fn self_asserted_binding_field(value: &Value) -> Result<Option<String>, UsageFailure> {
    first_field_matching(value, |key| {
        let normalized: String = key
            .chars()
            .filter(|character| *character != '_' && *character != '-')
            .flat_map(char::to_lowercase)
            .collect();
        is_authority_field(key) || TRANSPORT_BOUND_FIELDS.contains(&normalized.as_str())
    })
}

/// The origin of a cumulative series: an epoch, a metric and a window start.
///
/// Two cumulative readings may be differenced only when all three agree. A
/// producer restart, a reset counter or a different window is a new origin, and
/// the difference across origins is not a quantity of anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesOrigin {
    pub source_epoch: String,
    pub metric: String,
    pub interval_start: Option<String>,
}

impl SeriesOrigin {
    pub fn of(observation: &UsageObservation, metric: &str) -> Self {
        Self {
            source_epoch: observation.source_epoch.clone(),
            metric: metric.to_owned(),
            interval_start: observation.interval_start.clone(),
        }
    }
}

/// The growth of a cumulative counter between two readings of the same series.
///
/// Refuses a comparison the data cannot support — different series, different
/// unit, a reading that is not cumulative, a value that is not known — and
/// refuses a negative result, which means the counter reset and a new origin is
/// needed rather than a negative amount of work.
pub fn cumulative_delta(
    previous: &MetricValue,
    previous_origin: &SeriesOrigin,
    next: &MetricValue,
    next_origin: &SeriesOrigin,
) -> Result<ExactNumber, UsageFailure> {
    if previous_origin != next_origin {
        return Err(refusal("usage_series_mismatch", STAGE).with_field("intervalStart"));
    }
    if previous.temporality != Temporality::Cumulative
        || next.temporality != Temporality::Cumulative
    {
        return Err(refusal("usage_temporality_not_cumulative", STAGE).with_field("temporality"));
    }
    if previous.unit != next.unit {
        return Err(refusal("usage_unit_mismatch", STAGE).with_field("unit"));
    }
    let (Some(before), Some(after)) = (previous.exact(), next.exact()) else {
        return Err(refusal("usage_metric_unknown", STAGE).with_field("value"));
    };
    let delta = after
        .checked_sub(before)
        .ok_or_else(|| refusal("usage_decimal_overflow", STAGE).with_field("value"))?;
    if delta.is_negative() {
        return Err(refusal("usage_cumulative_regressed", STAGE).with_field("value"));
    }
    Ok(delta)
}

/// An exact decimal held as a scaled integer.
///
/// No binary floating point: `0.1` and `0.3` have no exact binary form, and a
/// ledger that cannot represent its own inputs exactly will eventually disagree
/// with itself.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExactNumber {
    mantissa: i128,
    scale: u32,
}

impl ExactNumber {
    /// Parse exact decimal text. Exponent, `NaN`, `inf` and a leading `+` are
    /// refused: the published pattern is `^-?[0-9]+(?:\.[0-9]+)?$`, and a
    /// spelling the schema refuses must not become an amount here.
    pub fn parse(text: &str) -> Option<Self> {
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (integer, fraction) = match digits.split_once('.') {
            Some((integer, fraction)) => (integer, Some(fraction)),
            None => (digits, None),
        };
        if integer.is_empty()
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || integer.len() > MAX_DECIMAL_DIGITS
        {
            return None;
        }
        let scale = match fraction {
            None => 0,
            Some(fraction) => {
                if fraction.is_empty()
                    || fraction.len() as u32 > MAX_DECIMAL_SCALE
                    || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return None;
                }
                fraction.len() as u32
            }
        };
        let mut mantissa: i128 = integer.parse().ok()?;
        if let Some(fraction) = fraction {
            let fraction_value: i128 = fraction.parse().ok()?;
            mantissa = mantissa.checked_mul(10i128.checked_pow(scale)?)?;
            mantissa = mantissa.checked_add(fraction_value)?;
        }
        Some(Self {
            mantissa: if negative {
                mantissa.checked_neg()?
            } else {
                mantissa
            },
            scale,
        })
    }

    pub const fn mantissa(self) -> i128 {
        self.mantissa
    }

    pub const fn scale(self) -> u32 {
        self.scale
    }

    pub const fn is_negative(self) -> bool {
        self.mantissa < 0
    }

    /// Both mantissas aligned to the finer of the two scales — the shared
    /// base of the exact arithmetic below, which never rounds.
    fn aligned(self, other: Self) -> Option<(i128, i128, u32)> {
        let scale = self.scale.max(other.scale);
        let left = self
            .mantissa
            .checked_mul(10i128.checked_pow(scale - self.scale)?)?;
        let right = other
            .mantissa
            .checked_mul(10i128.checked_pow(scale - other.scale)?)?;
        Some((left, right, scale))
    }

    /// Exact subtraction, aligning scales without rounding.
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        let (left, right, scale) = self.aligned(other)?;
        Some(Self {
            mantissa: left.checked_sub(right)?,
            scale,
        })
    }

    /// Exact addition, aligning scales without rounding. The published total
    /// path: a sum is an amount, never a binary float.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        let (left, right, scale) = self.aligned(other)?;
        Some(Self {
            mantissa: left.checked_add(right)?,
            scale,
        })
    }

    /// The canonical text: no exponent, no thousands separators, and exactly the
    /// scale the value was parsed with.
    pub fn to_canonical_string(self) -> String {
        let negative = self.mantissa < 0;
        let magnitude = self.mantissa.unsigned_abs();
        let divisor = 10u128.pow(self.scale);
        let integer = magnitude / divisor;
        let fraction = magnitude % divisor;
        let sign = if negative { "-" } else { "" };
        if self.scale == 0 {
            format!("{sign}{integer}")
        } else {
            format!(
                "{sign}{integer}.{fraction:0width$}",
                width = self.scale as usize
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(metrics: BTreeMap<String, MetricValue>) -> UsageObservation {
        UsageObservation {
            schema: self::wire::USAGE.to_owned(),
            observation_id: "obs-1".to_owned(),
            revision: 1,
            operation: UsageOperation::Upsert,
            source_epoch: "epoch-1".to_owned(),
            scope_ref: "scope-1".to_owned(),
            observed_at: "2026-09-21T00:00:00Z".to_owned(),
            interval_start: Some("2026-09-21T00:00:00Z".to_owned()),
            metrics,
            cost: None,
        }
    }

    fn tokens(value: &str) -> MetricValue {
        MetricValue::reported(value, "tokens", Temporality::Cumulative)
    }

    #[test]
    fn decimals_are_exact_and_never_round() {
        let a = ExactNumber::parse("0.1").expect("0.1");
        let b = ExactNumber::parse("0.3").expect("0.3");
        assert!(a.checked_sub(b).expect("difference").is_negative());
        let delta = ExactNumber::parse("1.50")
            .expect("1.50")
            .checked_sub(ExactNumber::parse("1.00").expect("1.00"))
            .expect("difference");
        assert_eq!(delta.to_canonical_string(), "0.50");
        let sum = ExactNumber::parse("0.1")
            .expect("0.1")
            .checked_add(ExactNumber::parse("0.03").expect("0.03"))
            .expect("sum");
        assert_eq!(sum.to_canonical_string(), "0.13");
        assert!(
            ExactNumber::parse("1e3").is_none(),
            "exponent forms are not amounts"
        );
        assert!(ExactNumber::parse("NaN").is_none());
        assert!(ExactNumber::parse("+1").is_none());
        assert!(ExactNumber::parse("1.2.3").is_none());
        assert!(ExactNumber::parse("").is_none());
    }

    #[test]
    fn unknown_metrics_carry_no_number() {
        let unknown = MetricValue::unknown("ms", Temporality::Gauge);
        assert!(unknown.validate("licoup.duration").is_ok());
        assert_eq!(unknown.value, None);
        let mut lying = unknown.clone();
        lying.value = Some("0".to_owned());
        assert_eq!(
            lying
                .validate("licoup.duration")
                .expect_err("zero is a claim")
                .code,
            "usage_metric_invalid"
        );
    }

    #[test]
    fn cumulative_differences_require_one_series() {
        let previous = observation(BTreeMap::from([(
            "licoup.tokens.input".to_owned(),
            tokens("100"),
        )]));
        let next = observation(BTreeMap::from([(
            "licoup.tokens.input".to_owned(),
            tokens("175"),
        )]));
        let origin = SeriesOrigin::of(&previous, "licoup.tokens.input");
        let delta = cumulative_delta(
            &previous.metrics["licoup.tokens.input"],
            &origin,
            &next.metrics["licoup.tokens.input"],
            &SeriesOrigin::of(&next, "licoup.tokens.input"),
        )
        .expect("delta");
        assert_eq!(delta.to_canonical_string(), "75");

        let mut other_metric = next.clone();
        other_metric.interval_start = Some("2026-09-22T00:00:00Z".to_owned());
        assert_eq!(
            cumulative_delta(
                &previous.metrics["licoup.tokens.input"],
                &origin,
                &other_metric.metrics["licoup.tokens.input"],
                &SeriesOrigin::of(&other_metric, "licoup.tokens.input"),
            )
            .expect_err("different origin")
            .code,
            "usage_series_mismatch"
        );

        assert_eq!(
            cumulative_delta(
                &next.metrics["licoup.tokens.input"],
                &origin,
                &previous.metrics["licoup.tokens.input"],
                &origin,
            )
            .expect_err("reset is not negative usage")
            .code,
            "usage_cumulative_regressed"
        );
    }

    #[test]
    fn a_retraction_withdraws_one_observation_and_carries_no_metric() {
        let mut retract = observation(BTreeMap::new());
        retract.operation = UsageOperation::Retract;
        assert!(retract.validate().is_ok());

        let mut bad = retract.clone();
        bad.metrics
            .insert("licoup.tokens.input".to_owned(), tokens("5"));
        assert!(bad.validate().is_err());
    }

    #[test]
    fn corrections_replace_and_never_offset_another_observation() {
        let first = observation(BTreeMap::from([(
            "licoup.tokens.input".to_owned(),
            tokens("100"),
        )]));
        let mut corrected = first.clone();
        corrected.revision = 2;
        corrected
            .metrics
            .insert("licoup.tokens.input".to_owned(), tokens("120"));
        assert!(corrected.supersedes(&first));
        assert!(!first.supersedes(&corrected));

        let mut elsewhere = corrected.clone();
        elsewhere.observation_id = "obs-2".to_owned();
        assert!(!elsewhere.supersedes(&first));
        assert_ne!(elsewhere.key(), first.key());
    }

    #[test]
    fn a_payload_cannot_assert_its_own_source() {
        let wire = serde_json::json!({
            "schema": self::wire::USAGE,
            "observationId": "obs-1",
            "revision": 1,
            "operation": "upsert",
            "sourceEpoch": "epoch-1",
            "scopeRef": "scope-1",
            "observedAt": "2026-09-21T00:00:00Z",
            "extensionId": "someone.else/agent",
            "metrics": { "licoup.tokens.input": {
                "value": "1", "unit": "tokens", "temporality": "delta", "quality": "reported"
            }}
        });
        let failure = UsageObservation::from_value(wire).expect_err("self-asserted source");
        assert_eq!(failure.code, "usage_source_self_asserted");

        let wire = serde_json::json!({
            "schema": self::wire::USAGE,
            "observationId": "obs-1",
            "revision": 1,
            "operation": "upsert",
            "sourceEpoch": "epoch-1",
            "scopeRef": "scope-1",
            "observedAt": "2026-09-21T00:00:00Z",
            "metrics": { "licoup.tokens.input": {
                "value": "1", "unit": "tokens", "temporality": "delta", "quality": "reported"
            }}
        });
        assert!(UsageObservation::from_value(wire).is_ok());
    }

    #[test]
    fn a_field_scan_that_runs_out_of_budget_refuses_instead_of_passing() {
        // More benign nodes than the scan may visit: the check owes an
        // answer or none at all — never "no self-asserted field" on a
        // payload it could not finish reading.
        let mut payload = serde_json::Map::new();
        for index in 0..MAX_WIRE_NODES + 1 {
            payload.insert(format!("benign_{index}"), serde_json::json!(index));
        }
        let failure = self_asserted_binding_field(&Value::Object(payload))
            .expect_err("the scan could not verify the payload");
        assert_eq!(failure.code, "usage_observation_invalid");
    }

    #[test]
    fn a_payload_deeper_than_the_scan_reaches_is_refused() {
        // A nested shape deeper than the scan's bound: refusing is the only
        // honest answer — what the walk cannot reach it cannot clear.
        let mut deep = serde_json::json!("leaf");
        for _ in 0..=MAX_WIRE_DEPTH {
            deep = serde_json::json!([deep]);
        }
        assert!(self_asserted_binding_field(&deep).is_err());
    }

    #[test]
    fn cost_is_an_exact_decimal_with_a_currency_code() {
        let cost = CostObservation {
            amount: Some("0.001250".to_owned()),
            currency: "USD".to_owned(),
            quality: Quality::Estimated,
        };
        assert!(cost.validate().is_ok());
        assert_eq!(
            cost.amount
                .as_deref()
                .and_then(ExactNumber::parse)
                .expect("exact")
                .to_canonical_string(),
            "0.001250"
        );
        let unknown = CostObservation {
            amount: None,
            currency: "USD".to_owned(),
            quality: Quality::Unknown,
        };
        assert!(unknown.validate().is_ok());
        let mut wrong_currency = cost.clone();
        wrong_currency.currency = "usd".to_owned();
        assert!(wrong_currency.validate().is_err());
    }
}

/// Wire schema names the usage payloads publish.
pub mod wire {
    /// One usage observation — the published wire vocabulary value.
    pub const USAGE: &str = "licoup.usage-observation.v1";
}
