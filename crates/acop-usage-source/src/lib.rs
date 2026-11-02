//! Agent-side usage acquisition read through ACOP transcripts.
//!
//! This crate is the read path the C11 profile expects for agent stores:
//! ACOP's codec owns session discovery and the declared `TranscriptUsage`
//! spec — which root holds counters, which record shape carries them and
//! which fields mean input, cached input, output and total — and this
//! adapter projects that declaration straight onto the persisted lines.
//! No per-agent store knowledge lives here; adding an agent is adding a
//! codec, not touching this crate.
//!
//! The output is `usage` wire payloads ready for a
//! `token-usage-detector` [`token_usage_detector::SourceBinding`]: the
//! caller supplies the epoch and scope because those are the transport's
//! facts, not the store's. Counters a record does not report stay absent —
//! a missing field is never emitted as zero, and a record shape the spec
//! does not match is skipped, never guessed at.
//!
//! The adapter is strictly read-only: it opens session files, never writes
//! to the store, and carries no ledger, no charge and no network surface.

use acop::{Codec, SessionEntry, StoreScan, TranscriptUsage, UsageTemporality, store};
use serde_json::{Value, json};
use std::path::Path;
use token_usage_detector::metrics;
use token_usage_detector::usage::wire;

/// One metric a `TranscriptUsage` field list maps to, in declaration order.
fn counter_fields(usage: &TranscriptUsage) -> [(&'static str, &'static [&'static str]); 4] {
    [
        (metrics::TOKENS_INPUT, usage.input),
        (metrics::TOKENS_CACHED_INPUT, usage.cached),
        (metrics::TOKENS_OUTPUT, usage.output),
        (metrics::TOKENS_TOTAL, usage.total),
    ]
}

/// Timestamp fields raw transcript lines commonly carry; first present wins.
const RECORD_TIME_FIELDS: &[&str] = &["timestamp", "ts", "time", "created_at"];

/// The wire shape one counter record produces; the temporality is the
/// store's own declaration, never inferred here.
fn counter_metric(value: impl Into<String>, temporality: UsageTemporality) -> Value {
    let temporality = match temporality {
        UsageTemporality::Delta => "delta",
        UsageTemporality::Cumulative => "cumulative",
    };
    json!({
        "value": value.into(),
        "unit": "tokens",
        "temporality": temporality,
        "quality": "reported",
    })
}

/// Resolve a dotted `a.b.c` path inside a JSON value.
fn dotted<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |node, segment| node.get(segment))
}

/// Read a numeric counter field as decimal text; non-numeric is absent.
fn counter_value(container: &Value, field: &str) -> Option<String> {
    let value = dotted(container, field)?;
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => text.parse::<i64>().ok().map(|_| text.clone()),
        _ => None,
    }
}

/// A session-backed usage source over one agent codec.
///
/// Built from the codec's declared [`Codec::transcript_usage`]; a codec
/// that declares no transcript counters yields `None` and the agent simply
/// contributes no transcript observations.
pub struct TranscriptSource<'a> {
    codec: &'a dyn Codec,
    usage: TranscriptUsage,
}

/// What one session file yielded: the payloads to publish and the count of
/// undecodable lines, which the caller may surface but the store never
/// explains twice.
pub struct Collected {
    pub payloads: Vec<Value>,
    pub skipped_lines: usize,
}

impl<'a> TranscriptSource<'a> {
    /// The usage source a codec declares, when it declares one.
    pub fn of(codec: &'a dyn Codec) -> Option<Self> {
        codec.transcript_usage().map(|usage| Self { codec, usage })
    }

    /// ACOP's session scan narrowed to the root the spec binds counters to.
    pub fn sessions(&self, home: &Path) -> acop::Result<StoreScan> {
        let mut scan = store::scan(self.codec, home)?;
        scan.sessions.retain(|entry| entry.kind == self.usage.root);
        Ok(scan)
    }

    /// Read one session and emit every counter record's payload.
    ///
    /// File-backed sessions are line-read with the spec applied verbatim.
    /// Container-backed sessions (`session.key` set — a database row, not
    /// a file) go through the codec's `decode_session` instead: each
    /// canonical `meta` record's `{subtype, data}` is rewrapped as
    /// `{type, payload}` so the spec's raw-record paths still resolve —
    /// the codec stays the only authority on how a row becomes a record.
    pub fn collect(
        &self,
        entry: &SessionEntry,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> acop::Result<Collected> {
        if entry.session.key.is_some() {
            return self.collect_decoded(entry, source_epoch, scope_ref, fallback_observed_at);
        }
        let text = std::fs::read_to_string(&entry.session.path)
            .map_err(acop::Error::io(&entry.session.path))?;
        Ok(self.collect_lines(
            &entry.session.id,
            text.lines(),
            source_epoch,
            scope_ref,
            fallback_observed_at,
        ))
    }

    /// The container-store path: canonical records out of `decode_session`,
    /// meta bodies rewrapped into the raw shape the spec's paths name.
    fn collect_decoded(
        &self,
        entry: &SessionEntry,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> acop::Result<Collected> {
        let decoded = self.codec.decode_session(&entry.session)?;
        let mut payloads = Vec::new();
        for record in &decoded.conversation.records {
            let acop::Body::Meta { subtype, data } = &record.body else {
                continue;
            };
            let raw = json!({"type": subtype, "payload": data});
            let Some(metrics) = self.counters(&raw) else {
                continue;
            };
            let observed_at = record.ts.as_deref().unwrap_or(fallback_observed_at);
            payloads.push(json!({
                "schema": wire::USAGE,
                "observationId": observation_id(&entry.session.id, record.ordinal as usize),
                "revision": 1,
                "operation": "upsert",
                "sourceEpoch": source_epoch,
                "scopeRef": scope_ref,
                "observedAt": observed_at,
                "metrics": metrics,
            }));
        }
        Ok(Collected {
            payloads,
            skipped_lines: decoded.skipped.len(),
        })
    }

    /// The line-level projection [`collect`] applies, exposed for callers
    /// holding session text already.
    pub fn collect_lines(
        &self,
        session_id: &str,
        lines: impl Iterator<Item = impl AsRef<str>>,
        source_epoch: &str,
        scope_ref: &str,
        fallback_observed_at: &str,
    ) -> Collected {
        let mut payloads = Vec::new();
        let mut skipped_lines = 0;
        for (index, line) in lines.enumerate() {
            let line = line.as_ref().trim();
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                skipped_lines += 1;
                continue;
            };
            let Some(metrics) = self.counters(&record) else {
                continue;
            };
            let observed_at = RECORD_TIME_FIELDS
                .iter()
                .find_map(|field| record.get(field).and_then(Value::as_str))
                .unwrap_or(fallback_observed_at);
            payloads.push(json!({
                "schema": wire::USAGE,
                "observationId": observation_id(session_id, index),
                "revision": 1,
                "operation": "upsert",
                "sourceEpoch": source_epoch,
                "scopeRef": scope_ref,
                "observedAt": observed_at,
                "metrics": metrics,
            }));
        }
        Collected {
            payloads,
            skipped_lines,
        }
    }

    /// Apply the spec to one raw record: match the discriminator, read the
    /// counter object, map the declared fields. Absent counters stay absent.
    fn counters(&self, record: &Value) -> Option<Value> {
        if let Some((path, expected)) = self.usage.record
            && dotted(record, path).and_then(Value::as_str) != Some(expected)
        {
            return None;
        }
        let container = dotted(record, self.usage.container)?;
        let mut metrics = serde_json::Map::new();
        for (metric, fields) in counter_fields(&self.usage) {
            if let Some(value) = fields
                .iter()
                .find_map(|field| counter_value(container, field))
            {
                metrics.insert(
                    (*metric).to_owned(),
                    counter_metric(value, self.usage.temporality),
                );
            }
        }
        if metrics.is_empty() {
            return None;
        }
        // `includes` keeps a total from being summed with its own subsets:
        // cached input inside input, input and output inside a reported total.
        if metrics.contains_key(metrics::TOKENS_INPUT)
            && metrics.contains_key(metrics::TOKENS_CACHED_INPUT)
        {
            metrics[metrics::TOKENS_INPUT]["includes"] = json!([metrics::TOKENS_CACHED_INPUT]);
        }
        if metrics.contains_key(metrics::TOKENS_TOTAL) {
            let subsets: Vec<String> = [metrics::TOKENS_INPUT, metrics::TOKENS_OUTPUT]
                .iter()
                .filter(|metric| metrics.contains_key(**metric))
                .map(|metric| (*metric).to_owned())
                .collect();
            metrics[metrics::TOKENS_TOTAL]["includes"] = json!(subsets);
        }
        Some(Value::Object(metrics))
    }
}

/// A bounded, deterministic observation id: the session's own id plus the
/// record's position inside it.
fn observation_id(session_id: &str, line_index: usize) -> String {
    const LIMIT: usize = token_usage_detector::usage::MAX_OBSERVATION_ID_BYTES;
    let suffix = format!("#{line_index}");
    let head: String = session_id
        .chars()
        .take(LIMIT.saturating_sub(suffix.len()))
        .collect();
    format!("{head}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use token_usage_detector::usage::UsageObservation;

    const SPEC: TranscriptUsage = TranscriptUsage {
        root: "synthetic-session-store",
        record: Some(("payload.type", "token_count")),
        container: "payload.info.last_token_usage",
        input: &["input_tokens"],
        cached: &["cached_input_tokens"],
        output: &["output_tokens"],
        total: &[],
        temporality: UsageTemporality::Delta,
    };

    fn source() -> TranscriptSource<'static> {
        TranscriptSource {
            codec: &NeverCodec,
            usage: SPEC,
        }
    }

    struct NeverCodec;
    impl Codec for NeverCodec {
        fn id(&self) -> &'static str {
            "synthetic"
        }
        fn history(&self) -> Vec<acop::HistoryRoot> {
            Vec::new()
        }
        fn session_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn capabilities(&self) -> &'static acop::CapabilityMatrix {
            &acop::CapabilityMatrix::READ_ONLY
        }
        fn decode(&self, _path: &Path) -> acop::Result<acop::codec::Decoded> {
            unreachable!("tests never decode")
        }
        fn encode(&self, _conversation: &acop::Conversation) -> acop::Result<acop::codec::Encoded> {
            unreachable!("tests never encode")
        }
        fn store_path(
            &self,
            _home: &Path,
            _conversation: &acop::Conversation,
        ) -> acop::Result<std::path::PathBuf> {
            unreachable!("tests never import")
        }
        fn redact_line(&self, _line: &Value, _pattern: &str) -> Option<Value> {
            unreachable!("tests never redact")
        }
    }

    fn lines(lines: &[&str]) -> Collected {
        source().collect_lines(
            "session-1",
            lines.iter(),
            "epoch-1",
            "scope-1",
            "2026-10-08T00:00:00Z",
        )
    }

    #[test]
    fn declared_counters_become_token_metrics() {
        let collected = lines(&[
            r#"{"type":"session_meta","payload":{"id":"s-1"}}"#,
            r#"{"type":"token_usage_record","timestamp":"2026-10-08T12:00:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":42,"cached_input_tokens":10,"output_tokens":7}}}}"#,
        ]);
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert_eq!(observation.observed_at, "2026-10-08T12:00:00Z");
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].value.as_deref(),
            Some("42")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_INPUT].includes,
            vec![metrics::TOKENS_CACHED_INPUT.to_owned()]
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_CACHED_INPUT]
                .value
                .as_deref(),
            Some("10")
        );
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("7")
        );
        assert!(!observation.metrics.contains_key(metrics::TOKENS_TOTAL));
    }

    #[test]
    fn absent_counters_stay_absent_and_bad_lines_are_counted() {
        let collected = lines(&[
            r#"{"type":"token_usage_record","payload":{"type":"token_count","info":{"last_token_usage":{}}}}"#,
            "not json",
            r#"{"type":"token_usage_record","payload":{"type":"token_count","info":{"last_token_usage":{"output_tokens":5}}}}"#,
        ]);
        assert_eq!(collected.skipped_lines, 1);
        assert_eq!(collected.payloads.len(), 1);
        let observation =
            UsageObservation::from_value(collected.payloads[0].clone()).expect("wire payload");
        assert!(!observation.metrics.contains_key(metrics::TOKENS_INPUT));
        assert_eq!(
            observation.metrics[metrics::TOKENS_OUTPUT].value.as_deref(),
            Some("5")
        );
    }

    #[test]
    fn the_discriminator_rejects_other_records() {
        let collected = lines(&[
            r#"{"type":"event_msg","payload":{"type":"turn_diff","info":{"last_token_usage":{"input_tokens":9}}}}"#,
            r#"{"payload":{"type":"other","info":{"last_token_usage":{"input_tokens":9}}}}"#,
        ]);
        assert!(collected.payloads.is_empty());
    }
}
