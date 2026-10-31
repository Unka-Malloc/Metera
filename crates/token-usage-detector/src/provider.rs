//! Model-provider usage records: response JSON becomes the same counters an
//! agent record yields.
//!
//! [`crate::agent`] maps the usage an agent writes into its own histories;
//! this module maps the `usage` object a model provider returns on an API
//! response — the other end of the same conversation. The mapping lands on
//! the same [`TokenUsage`]: input and output counts, the cache-read and
//! cache-creation subsets, the reasoning share, and the counters only the
//! provider publishes, which keep their own names under the namespace the
//! adapter declares rather than being dropped or guessed into a shared name.
//!
//! Three rules from the shared model hold here unchanged:
//!
//! - **The record never names its transport facts.** A usage object carrying
//!   `source`, `extensionId`, `instanceId`, a generation, a registry epoch, an
//!   authority field or a measurement identity is refused before it is read:
//!   the provider answers what it counted, never where the answer belongs.
//! - **Absent is absent; named-but-uncounted is unknown.** A counter the
//!   record does not carry stays `None`; one it names without a number — an
//!   explicit `null` or a non-numeric claim — projects as
//!   [`Quality::Unknown`], because a zero would be a claim that nothing was
//!   used.
//! - **Subsets fold the way the agent-side normalization folds them.** Where a
//!   provider reports a cache category or a reasoning count outside its base
//!   count (the Anthropic `cache_read`/`cache_creation` input categories, the
//!   Kimi `input_cache_*` and flat `cache`/`reasoning` spellings), the shared
//!   count is their sum; where the subset already lives inside the base count
//!   (the OpenAI `*_details`, `cached_tokens` and Gemini spellings), it maps
//!   to the subset counter only and is never added twice.
//!
//! The extraction still stops at counters: turning [`ProviderUsage`] into a
//! bound observation is the host's `SourceBinding`, exactly as for the
//! agent-side counters.

use serde_json::Value;
use std::collections::BTreeMap;

use crate::agent::TokenUsage;
use crate::binding::measurement_bound_field;
use crate::failure::UsageFailure;
use crate::metrics as general;
use crate::names::is_namespaced;
use crate::usage::{MetricValue, Quality, Temporality, self_asserted_binding_field};

use crate::refusal;

/// The deepest object nesting the map reads inside one usage record.
/// Provider detail objects (`prompt_tokens_details`, `cache`) sit one level
/// down; anything deeper is not a counter the record named.
const MAX_USAGE_DEPTH: usize = 4;

/// The most fields one usage record may carry before it is refused: a usage
/// object is a small record, and an unbounded one is not a usage report.
pub const MAX_USAGE_FIELDS: usize = 256;

/// The envelope keys a response wraps its usage object under, in search order.
const USAGE_OBJECT_KEYS: &[&str] = &[
    "usage",
    "usageMetadata",
    "usage_metadata",
    "token_usage",
    "tokenUsage",
    "response_usage",
    "responseUsage",
];

/// Base input counters — the provider's own input count, before additive
/// cache categories are folded in.
const BASE_INPUT_ALIASES: &[&str] = &[
    "input_tokens",
    "inputTokens",
    "prompt_tokens",
    "promptTokens",
    "promptTokenCount",
    "prompt_token_count",
    "total_input_tokens",
    "totalInputTokens",
    "input_other",
    "inputOther",
    "input",
];

/// Cache-read counters the provider reports *outside* its input count: they
/// fold into `input` and into `cached_input` alike. Anthropic's
/// `cache_read_input_tokens` sits here — Anthropic reports it beside
/// `cache_creation_input_tokens`, outside `input_tokens`, exactly the
/// uncapped `cached_input` reading `crate::agent` gives the same field.
const ADDITIVE_CACHE_READ_ALIASES: &[&str] = &[
    "cache_read_input_tokens",
    "cacheReadInputTokens",
    "cache_read_tokens",
    "cacheReadTokens",
    "cache_read",
    "cacheRead",
    "input_cache_read",
    "inputCacheRead",
    "cache.read",
];

/// Cache-write and cache-creation counters the provider reports outside its
/// input count: they fold into `input`.
const ADDITIVE_CACHE_WRITE_ALIASES: &[&str] = &[
    "cache_creation_input_tokens",
    "cacheCreationInputTokens",
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_write_tokens",
    "cacheWriteTokens",
    "cache_write",
    "cacheWrite",
    "input_cache_creation",
    "inputCacheCreation",
    "input_cache_write_tokens",
    "inputCacheWriteTokens",
    "cache.write",
];

/// Cache-read counters the provider reports *inside* its input count: they
/// map to `cached_input` only, capped by the base count so a subset can never
/// exceed the whole it claims to be part of. A spelling that lives outside
/// the base count belongs to the additive set instead — capping it by the
/// base would silently drop the share the base does not contain.
const CACHED_SUBSET_ALIASES: &[&str] = &[
    "cached_input_tokens",
    "cachedInputTokens",
    "input_cached_tokens",
    "inputCachedTokens",
    "total_cached_tokens",
    "totalCachedTokens",
    "cached_content_token_count",
    "cachedContentTokenCount",
    "cached_tokens",
    "prompt_cache_hit_tokens",
    "promptCacheHitTokens",
    "prompt_tokens_details.cached_tokens",
    "input_tokens_details.cached_tokens",
];

/// Base output counters — the provider's own output count, before additive
/// reasoning and tool-use categories are folded in.
const BASE_OUTPUT_ALIASES: &[&str] = &[
    "output_tokens",
    "outputTokens",
    "completion_tokens",
    "completionTokens",
    "candidatesTokenCount",
    "candidates_token_count",
    "total_output_tokens",
    "totalOutputTokens",
    "response_tokens",
    "responseTokens",
    "output",
];

/// Reasoning and thinking counters the provider reports outside its output
/// count: they fold into `output` and name the `thought` subset.
const ADDITIVE_REASONING_ALIASES: &[&str] = &[
    "reasoning_tokens",
    "reasoningTokens",
    "thoughtsTokenCount",
    "thoughts_token_count",
    "total_thought_tokens",
    "totalThoughtTokens",
    "reasoning",
];

/// Tool-use counters the provider reports outside its output count: they
/// fold into `output` but are not reasoning.
const ADDITIVE_TOOL_USE_ALIASES: &[&str] = &["total_tool_use_tokens", "totalToolUseTokens"];

/// Reasoning counters the provider reports *inside* its output count: they
/// name the `thought` subset and are never added twice.
const REASONING_SUBSET_ALIASES: &[&str] = &[
    "completion_tokens_details.reasoning_tokens",
    "output_tokens_details.reasoning_tokens",
];

/// Total counters. A total is only ever the count the provider wrote: the
/// map never derives one from the parts.
const TOTAL_ALIASES: &[&str] = &[
    "total_tokens",
    "totalTokens",
    "totalTokenCount",
    "total_token_count",
    "total",
];

/// One leaf of the flattened usage object: either a count, or a field the
/// record named without one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Leaf {
    Count(u64),
    /// `null`, a string, a boolean, a non-integer or an array: present, never
    /// a count.
    Present,
}

/// What one field's alias search resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldReading {
    /// No alias appeared in the record.
    Absent,
    /// An alias appeared but carried no count: the record named the field
    /// and did not count it.
    Unknown,
    /// The count the record carried.
    Count(u64),
}

/// How one provider's records map onto the shared counters.
///
/// The adapter declares the namespace the provider's own counters keep and
/// the quality it claims for the values it read — `Reported` when the
/// provider counted, `Estimated` when it derived, `Unknown` when the record
/// is read but its numbers are not the adapter's claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderMapping {
    /// The namespace provider-native counters keep, e.g. `openai.usage`.
    pub extra_namespace: String,
    /// What the adapter says about the counts it read.
    pub quality: Quality,
}

/// What one provider usage record claimed: the shared counters, the counts
/// only this provider publishes, and the counters it named but did not count.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderUsage {
    /// The counters that fit the shared model. `input` already contains the
    /// additive cache categories a provider reports separately; `output`
    /// already contains the additive reasoning and tool-use categories.
    /// Absent stays absent.
    pub usage: TokenUsage,
    /// Counts with no shared name, kept under `extra_namespace` as
    /// `namespace/path` — `deepseek.usage/prompt_cache_miss_tokens`, for
    /// example — rather than dropped or folded into a metric they are not.
    pub extra: BTreeMap<String, u64>,
    /// Metrics the record named but did not count: an explicit `null` or a
    /// non-numeric claim on a recognized counter. They project as
    /// [`Quality::Unknown`] readings — named, unanswered, never zero.
    pub unknown: Vec<String>,
}

impl ProviderUsage {
    /// Whether the record claimed nothing at all: no shared counter, no
    /// provider-native count and no named-but-uncounted field.
    pub fn is_empty(&self) -> bool {
        self.usage == TokenUsage::default() && self.extra.is_empty() && self.unknown.is_empty()
    }
}

impl ProviderMapping {
    /// Declare the mapping: the namespace provider-native counters keep and
    /// the quality the adapter claims for the values it reads.
    ///
    /// `extra_namespace` must be namespaced and outside `licoup` — that
    /// namespace belongs to the contract, and a vendor claiming it would be
    /// writing metrics the contract itself publishes.
    pub fn new(extra_namespace: impl Into<String>, quality: Quality) -> Result<Self, UsageFailure> {
        let mapping = Self {
            extra_namespace: extra_namespace.into(),
            quality,
        };
        let first = mapping
            .extra_namespace
            .split(['.', '-', '/'])
            .next()
            .unwrap_or_default();
        if !is_namespaced(&mapping.extra_namespace) || first == "licoup" {
            return Err(refusal("usage_mapping_invalid").with_field("extraNamespace"));
        }
        Ok(mapping)
    }

    /// Map one provider response — or a usage object directly — into the
    /// shared counters.
    ///
    /// `Ok(None)` is not an error: the record carried no usage object the map
    /// can read, and silence is a claim of nothing rather than a failure.
    /// A usage object asserting transport or measurement facts is refused
    /// before it is read, exactly as `SourceBinding::bind_value` refuses a
    /// payload that names its own binding.
    pub fn normalize(&self, record: &Value) -> Result<Option<ProviderUsage>, UsageFailure> {
        let Some(object) = locate_usage(record)? else {
            return Ok(None);
        };
        if let Some(field) = self_asserted_binding_field(object)? {
            return Err(refusal("usage_source_self_asserted").with_field(&field));
        }
        if let Some(field) = measurement_bound_field(object)? {
            return Err(refusal("usage_source_self_asserted").with_field(&field));
        }

        let leaves = flatten_object(object)?;

        let base_input = resolve(&leaves, BASE_INPUT_ALIASES);
        let cache_read = resolve(&leaves, ADDITIVE_CACHE_READ_ALIASES);
        let cache_write = resolve(&leaves, ADDITIVE_CACHE_WRITE_ALIASES);
        // A subset reported separately from its base still needs the base to
        // stand under it: without a base count the cached share has nothing
        // to be a subset of, and the record keeps its silence.
        let cached_subset = match (resolve(&leaves, CACHED_SUBSET_ALIASES), base_input) {
            (FieldReading::Count(subset), FieldReading::Count(base)) => {
                FieldReading::Count(subset.min(base))
            }
            (FieldReading::Unknown, _) => FieldReading::Unknown,
            _ => FieldReading::Absent,
        };
        let base_output = resolve(&leaves, BASE_OUTPUT_ALIASES);
        let additive_reasoning = resolve(&leaves, ADDITIVE_REASONING_ALIASES);
        let additive_tool_use = resolve(&leaves, ADDITIVE_TOOL_USE_ALIASES);
        let reasoning_subset = resolve(&leaves, REASONING_SUBSET_ALIASES);

        let input = counted(&[base_input, cache_read, cache_write]);
        let cached_input = counted(&[cached_subset, cache_read]);
        let output = counted(&[base_output, additive_reasoning, additive_tool_use]);
        let thought = first_named(&[additive_reasoning, reasoning_subset]);
        // A record carrying any additive spelling folds its total from the
        // parts: the provider's own total is written before the categories
        // it may not contain, so publishing it beside folded counters would
        // claim the smaller count includes them — the same additive
        // semantics `agent-statistics` normalization gives the record.
        let additive_shape = ADDITIVE_CACHE_READ_ALIASES
            .iter()
            .chain(ADDITIVE_CACHE_WRITE_ALIASES)
            .chain(ADDITIVE_REASONING_ALIASES)
            .chain(ADDITIVE_TOOL_USE_ALIASES)
            .any(|alias| leaves.contains_key(*alias));
        let total = if additive_shape {
            counted(&[input, output])
        } else {
            resolve(&leaves, TOTAL_ALIASES)
        };

        let usage = ProviderUsage {
            usage: TokenUsage {
                input: count_of(input),
                cached_input: count_of(cached_input),
                cached_write: count_of(cache_write),
                output: count_of(output),
                thought: count_of(thought),
                total: count_of(total),
            },
            extra: self.extra_of(&leaves),
            unknown: self.unknown_of(&[
                (input, general::TOKENS_INPUT),
                (cached_input, general::TOKENS_CACHED_INPUT),
                (cache_write, "cache_write_tokens"),
                (output, general::TOKENS_OUTPUT),
                (thought, "reasoning_tokens"),
                (total, general::TOKENS_TOTAL),
            ]),
        };
        if usage.is_empty() {
            return Ok(None);
        }
        Ok(Some(usage))
    }

    /// The namespaced extra counts: every leaf the shared counters did not
    /// claim, named `namespace/path`. A field name that cannot be kept
    /// namespaced is dropped rather than renamed — inventing a name for it
    /// would be claiming a metric the provider never wrote.
    fn extra_of(&self, leaves: &BTreeMap<String, Leaf>) -> BTreeMap<String, u64> {
        let mut extra = BTreeMap::new();
        for (path, leaf) in leaves {
            let Leaf::Count(count) = leaf else {
                continue;
            };
            if is_known_count_path(path) {
                continue;
            }
            let metric = format!("{}/{path}", self.extra_namespace);
            if is_namespaced(&metric) {
                extra.insert(metric, *count);
            }
        }
        extra
    }

    /// The metric names the record claimed without a count, each under the
    /// name the projection emits — the shared metric for the shared
    /// counters, the adapter namespace for the subsets it owns there.
    fn unknown_of(&self, fields: &[(FieldReading, &str)]) -> Vec<String> {
        fields
            .iter()
            .filter(|(reading, _)| matches!(reading, FieldReading::Unknown))
            .map(|(_, metric)| match *metric {
                metric if general::is_standard(metric) => metric.to_owned(),
                metric => format!("{}/{metric}", self.extra_namespace),
            })
            .collect()
    }

    /// Project the shared counters into the metric readings a bound
    /// observation carries.
    ///
    /// Each counted field becomes its declared general metric — input and
    /// total carry their subset relations with them, so a consumer never
    /// adds a total to its own subsets — and the subsets with no general
    /// name (cache write, reasoning) keep the adapter's namespace. A field
    /// the record named but did not count projects as `Unknown` with no
    /// number at all; a mapping declared `Unknown` reads the record but
    /// claims none of its numbers.
    ///
    /// A provider-native extra claims the `tokens` unit only when its leaf
    /// name denotes a token count: the record carried a number and a name,
    /// never a unit, so any other numeric leaf — a latency, a timestamp —
    /// projects the unitless `1` rather than a unit the provider did not
    /// write (see [`extra_unit`]).
    pub fn metrics(&self, usage: &ProviderUsage) -> BTreeMap<String, MetricValue> {
        let mut metrics = BTreeMap::new();
        let mut reading =
            |metric: String, count: Option<u64>, unit: &'static str, includes: Vec<String>| {
                let Some(count) = count else {
                    return;
                };
                let value = if self.quality.has_value() {
                    let mut value =
                        MetricValue::reported(count.to_string(), unit, Temporality::Delta)
                            .with_includes(includes);
                    value.quality = self.quality;
                    value
                } else {
                    MetricValue::unknown(unit, Temporality::Delta)
                };
                metrics.insert(metric, value);
            };
        // Subset relations travel with the projection: a consumer summing
        // metrics must never double-count a part that is already folded into
        // its published aggregate. The namespaced cache-write and reasoning
        // counters fold into input and output whenever they are emitted — the
        // additive fold counted them, and a subset spelling was already
        // inside the base count — so the aggregate names them here.
        let cache_write_metric = format!("{}/cache_write_tokens", self.extra_namespace);
        let reasoning_metric = format!("{}/reasoning_tokens", self.extra_namespace);
        let mut input_includes = vec![general::TOKENS_CACHED_INPUT.to_owned()];
        if usage.usage.cached_write.is_some() {
            input_includes.push(cache_write_metric.clone());
        }
        let output_includes = match usage.usage.thought.is_some() {
            true => vec![reasoning_metric.clone()],
            false => Vec::new(),
        };
        reading(
            general::TOKENS_INPUT.to_owned(),
            usage.usage.input,
            "tokens",
            input_includes,
        );
        reading(
            general::TOKENS_OUTPUT.to_owned(),
            usage.usage.output,
            "tokens",
            output_includes,
        );
        reading(
            general::TOKENS_CACHED_INPUT.to_owned(),
            usage.usage.cached_input,
            "tokens",
            Vec::new(),
        );
        reading(
            general::TOKENS_TOTAL.to_owned(),
            usage.usage.total,
            "tokens",
            vec![
                general::TOKENS_INPUT.to_owned(),
                general::TOKENS_OUTPUT.to_owned(),
            ],
        );
        reading(
            cache_write_metric,
            usage.usage.cached_write,
            "tokens",
            Vec::new(),
        );
        reading(reasoning_metric, usage.usage.thought, "tokens", Vec::new());
        for (metric, count) in &usage.extra {
            let leaf = metric
                .strip_prefix(self.extra_namespace.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(metric.as_str());
            reading(metric.clone(), Some(*count), extra_unit(leaf), Vec::new());
        }
        for metric in &usage.unknown {
            metrics.insert(
                metric.clone(),
                MetricValue::unknown("tokens", Temporality::Delta),
            );
        }
        metrics
    }
}

/// The unit a provider-native extra declares. The record carried a number and
/// a name but no unit, so the projection claims `tokens` only when the leaf
/// name denotes a token count; any other leaf — a latency, a timestamp, a
/// request tally — reads as `1`, the UCUM unitless unit. An empty unit would
/// not survive observation validation, and inventing `tokens` for a duration
/// would be a claim the provider never made.
fn extra_unit(leaf: &str) -> &'static str {
    // The terminal segment names the counted thing: a parent may be
    // token-shaped — `prompt_tokens_details.elapsed_ms` — without the leaf
    // itself counting tokens at all.
    let terminal = leaf.rsplit('.').next().unwrap_or(leaf);
    if terminal.to_ascii_lowercase().contains("token") {
        "tokens"
    } else {
        "1"
    }
}

/// The count a resolved field carries, when it carries one.
fn count_of(reading: FieldReading) -> Option<u64> {
    match reading {
        FieldReading::Count(count) => Some(count),
        _ => None,
    }
}

/// The sum of the parts that counted: parts the record did not carry add
/// nothing, and a part the record named without a count makes the whole
/// sum `Unknown` — a precise number that silently drops the missing share
/// is a claim the record never made, never a partial answer.
fn counted(readings: &[FieldReading]) -> FieldReading {
    if readings
        .iter()
        .any(|reading| matches!(reading, FieldReading::Unknown))
    {
        return FieldReading::Unknown;
    }
    let mut total = 0u64;
    let mut counted = false;
    for reading in readings {
        if let FieldReading::Count(count) = reading {
            counted = true;
            // A sum that exceeds what the record can express is not a
            // count at all: clamping would understate usage while looking
            // precise, so overflow propagates Unknown the way a
            // named-but-uncounted part does.
            let Some(sum) = total.checked_add(*count) else {
                return FieldReading::Unknown;
            };
            total = sum;
        }
    }
    if counted {
        FieldReading::Count(total)
    } else {
        FieldReading::Absent
    }
}

/// The first reading that said anything: a count wins over an unknown, and
/// an absent part never shadows a named one.
fn first_named(readings: &[FieldReading]) -> FieldReading {
    readings
        .iter()
        .copied()
        .find(|reading| matches!(reading, FieldReading::Count(_)))
        .or_else(|| {
            readings
                .iter()
                .copied()
                .find(|reading| *reading != FieldReading::Absent)
        })
        .unwrap_or(FieldReading::Absent)
}

/// Resolve one field through its alias list: the first count the record
/// carries wins; a field named but never counted is unknown; a field the
/// record never named is absent.
fn resolve(leaves: &BTreeMap<String, Leaf>, aliases: &[&str]) -> FieldReading {
    let mut named = false;
    for alias in aliases {
        match leaves.get(*alias) {
            Some(Leaf::Count(count)) => return FieldReading::Count(*count),
            Some(Leaf::Present) => named = true,
            None => {}
        }
    }
    if named {
        FieldReading::Unknown
    } else {
        FieldReading::Absent
    }
}

/// Flatten one object into dotted leaf paths. Objects recurse; every scalar,
/// `null` and array is a leaf — countable when it is an unsigned integer,
/// merely present otherwise. Every member spends `visited` — the members of
/// an empty container included — so a shape too wide to name its counters
/// still outruns the bound, and traversal stops and refuses the moment the
/// budget is exceeded rather than after the map has grown.
fn flatten(
    value: &Value,
    path: &str,
    depth: usize,
    leaves: &mut BTreeMap<String, Leaf>,
    visited: &mut usize,
) -> Result<(), UsageFailure> {
    match value {
        Value::Object(map) => {
            if depth >= MAX_USAGE_DEPTH {
                if !path.is_empty() {
                    leaves.insert(path.to_owned(), Leaf::Present);
                }
            } else {
                for (key, nested) in map {
                    // Each member spends the field budget before it is
                    // visited — an empty container spends exactly one, a
                    // nested object spends its whole subtree — so an
                    // over-budget shape refuses during traversal, never
                    // after the map has grown.
                    *visited += 1;
                    if *visited > MAX_USAGE_FIELDS {
                        return Err(refusal("usage_mapping_invalid")
                            .with_field("usage")
                            .with_presentation_arg("limit", &MAX_USAGE_FIELDS.to_string()));
                    }
                    let child = match path.is_empty() {
                        true => key.clone(),
                        false => format!("{path}.{key}"),
                    };
                    flatten(nested, &child, depth + 1, leaves, visited)?;
                }
            }
        }
        Value::Number(number) => match number.as_u64() {
            Some(count) => {
                leaves.insert(path.to_owned(), Leaf::Count(count));
            }
            None => {
                leaves.insert(path.to_owned(), Leaf::Present);
            }
        },
        _ => {
            leaves.insert(path.to_owned(), Leaf::Present);
        }
    }
    Ok(())
}

/// Every path the shared counters claim, across all nine alias groups — the
/// single inventory presence checks and extra suppression both derive from.
fn known_count_paths() -> impl Iterator<Item = &'static str> {
    [
        BASE_INPUT_ALIASES,
        ADDITIVE_CACHE_READ_ALIASES,
        ADDITIVE_CACHE_WRITE_ALIASES,
        CACHED_SUBSET_ALIASES,
        BASE_OUTPUT_ALIASES,
        ADDITIVE_REASONING_ALIASES,
        ADDITIVE_TOOL_USE_ALIASES,
        REASONING_SUBSET_ALIASES,
        TOTAL_ALIASES,
    ]
    .into_iter()
    .flatten()
    .copied()
}

/// Whether `path` is one the shared counters claim. Membership is derived
/// from the alias groups themselves, so a new spelling needs exactly one
/// entry — in its group — and can never drift out of a second inventory
/// that forgot it.
fn is_known_count_path(path: &str) -> bool {
    known_count_paths().any(|known| known == path)
}

/// Restore presence to a counter path whose value was an object. Flattening
/// writes only leaves, so `prompt_tokens: {"nested": 3}` would collapse into
/// `prompt_tokens.nested` and the named-but-uncounted counter would read
/// absent — a partial aggregate could then publish as precise. Presence is
/// restored only on paths the shared counters claim; an unrecognized
/// container keeps its own name for the provider-native extras.
fn restore_counter_presence(leaves: &mut BTreeMap<String, Leaf>) {
    for path in known_count_paths() {
        if leaves.contains_key(path) {
            continue;
        }
        if leaves.keys().any(|key| {
            key.len() > path.len() && key.starts_with(path) && key.as_bytes()[path.len()] == b'.'
        }) {
            leaves.insert(path.to_owned(), Leaf::Present);
        }
    }
}

/// Flatten one candidate into its leaf map, then restore counter presence —
/// the single pipeline discovery and normalization both run, so a field that
/// discovered the object can never read as absent to the counter resolving
/// it.
fn flatten_object(object: &Value) -> Result<BTreeMap<String, Leaf>, UsageFailure> {
    let mut leaves = BTreeMap::new();
    let mut visited = 0usize;
    flatten(object, "", 0, &mut leaves, &mut visited)?;
    restore_counter_presence(&mut leaves);
    Ok(leaves)
}

/// Whether the object carries a field the map recognizes, whether that field
/// carries a count or only a name — with the same traversal budget the parser
/// enforces, so envelope discovery never materializes an unbounded map.
fn has_known_field(object: &Value) -> Result<bool, UsageFailure> {
    let leaves = flatten_object(object)?;
    Ok(leaves.keys().any(|path| is_known_count_path(path)))
}

/// The usage object one record carries: the first known envelope key whose
/// object holds a recognized field, then the first holding any count — a
/// `usage` envelope declares the object a usage report, so unfamiliar
/// counters inside it still keep their namespaced place — and finally the
/// record itself, which is a usage object only when it names a known field.
/// `None` means the record claims nothing the map reads: an error or status
/// body is not a usage report, however many integers it carries. A candidate
/// or record that breaks the leaf budget is refused rather than scanned.
fn locate_usage(record: &Value) -> Result<Option<&Value>, UsageFailure> {
    let mut candidates: Vec<&Value> = Vec::new();
    for key in USAGE_OBJECT_KEYS {
        if let Some(candidate) = record.get(*key).filter(|value| value.is_object()) {
            candidates.push(candidate);
        }
    }
    // Each candidate is flattened once: the flattened form answers both
    // discovery questions, so a rejected record is never traversed twice and
    // an over-budget one is refused on its first scan.
    let mut flattened: Vec<BTreeMap<String, Leaf>> = Vec::with_capacity(candidates.len());
    for &candidate in &candidates {
        let leaves = flatten_object(candidate)?;
        if leaves.keys().any(|path| is_known_count_path(path)) {
            return Ok(Some(candidate));
        }
        flattened.push(leaves);
    }
    for (index, leaves) in flattened.iter().enumerate() {
        if leaves.values().any(|leaf| matches!(leaf, Leaf::Count(_))) {
            return Ok(Some(candidates[index]));
        }
    }
    if record.is_object() && has_known_field(record)? {
        return Ok(Some(record));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn openai() -> ProviderMapping {
        ProviderMapping::new("openai.usage", Quality::Reported).expect("mapping")
    }

    #[test]
    fn a_namespace_must_be_namespaced_and_outside_the_contract() {
        assert!(ProviderMapping::new("openai.usage", Quality::Reported).is_ok());
        assert!(ProviderMapping::new("openai-usage", Quality::Reported).is_ok());
        assert_eq!(
            ProviderMapping::new("usage", Quality::Reported)
                .expect_err("bare word")
                .field
                .as_deref(),
            Some("extraNamespace")
        );
        for squat in ["licoup.usage", "licoup", "licoup-tokens"] {
            assert!(
                ProviderMapping::new(squat, Quality::Reported).is_err(),
                "{squat} is the contract's own namespace"
            );
        }
    }

    #[test]
    fn the_usage_object_is_found_under_each_envelope_spelling() {
        for key in [
            "usage",
            "usageMetadata",
            "usage_metadata",
            "token_usage",
            "tokenUsage",
            "response_usage",
            "responseUsage",
        ] {
            let mut record = json!({});
            record[key] = json!({"input_tokens": 7});
            let usage = openai().normalize(&record).unwrap().unwrap();
            assert_eq!(usage.usage.input, Some(7), "{key} carries the object");
        }
        // The record itself may already be the usage object.
        let usage = openai()
            .normalize(&json!({"prompt_tokens": 3}))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, Some(3));
        // An object with a recognized spelling beats one with a bare count.
        let usage = openai()
            .normalize(&json!({
                "usage": {"stray": 5},
                "usageMetadata": {"promptTokenCount": 11}
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, Some(11));
    }

    #[test]
    fn an_over_budget_usage_object_is_refused_during_traversal() {
        // Discovery and parsing share the leaf budget: 257 fields refuse
        // the object before the whole map materializes — envelope and bare
        // record alike.
        let mut usage = serde_json::Map::new();
        for i in 0..=MAX_USAGE_FIELDS {
            usage.insert(format!("field_{i}"), json!(i));
        }
        for record in [json!({"usage": usage.clone()}), json!(usage)] {
            assert_eq!(
                openai().normalize(&record).unwrap_err().code,
                "usage_mapping_invalid"
            );
        }
    }

    #[test]
    fn a_wide_object_of_empty_containers_still_outruns_the_budget() {
        // Empty containers produce no leaves but still spend the traversal
        // budget: a shape this wide is as much scan work as a full one, and
        // the bound that refuses it is the same one the parser enforces.
        let mut usage = serde_json::Map::new();
        for i in 0..=MAX_USAGE_FIELDS {
            usage.insert(format!("field_{i}"), json!({}));
        }
        assert_eq!(
            openai()
                .normalize(&json!({"usage": usage}))
                .unwrap_err()
                .code,
            "usage_mapping_invalid"
        );
    }

    #[test]
    fn an_authority_field_hidden_inside_an_array_is_still_found() {
        // The counter map reads an array as an opaque leaf, but the
        // transport check reads through it: an array cannot smuggle a
        // self-asserted binding fact past the refusal.
        let failure = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": 1,
                    "breakdown": [{"stateRoot": "/elsewhere"}]
                }
            }))
            .unwrap_err();
        assert_eq!(failure.code, "usage_source_self_asserted");
    }

    #[test]
    fn the_field_budget_itself_still_parses() {
        let mut usage = serde_json::Map::new();
        for i in 0..MAX_USAGE_FIELDS - 1 {
            usage.insert(format!("field_{i}"), json!(i));
        }
        usage.insert("prompt_tokens".to_owned(), json!(7));
        let usage = openai()
            .normalize(&json!({"usage": usage}))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, Some(7));
    }

    #[test]
    fn openai_details_become_subsets_and_the_rest_stays_namespaced() {
        let usage = openai()
            .normalize(&json!({
                "id": "chatcmpl-1",
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "prompt_tokens_details": {"cached_tokens": 30, "audio_tokens": 2},
                    "completion_tokens_details": {"reasoning_tokens": 5}
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(
            usage.usage,
            TokenUsage {
                input: Some(100),
                cached_input: Some(30),
                cached_write: None,
                output: Some(20),
                thought: Some(5),
                total: Some(120),
            }
        );
        assert_eq!(
            usage.extra["openai.usage/prompt_tokens_details.audio_tokens"],
            2
        );
        assert_eq!(usage.extra.len(), 1);
        assert!(usage.unknown.is_empty());
    }

    #[test]
    fn anthropic_cache_categories_fold_the_way_the_shared_model_folds() {
        // The real Anthropic shape: both cache categories sit outside
        // `input_tokens` — `cache_read_input_tokens` shares the additive
        // position of `cache_creation_input_tokens`, it is not a subset of
        // the base count — so the input folds to 10 + 30 + 5, and the
        // cache-read share is the cached subset `crate::agent` reads from
        // the same field.
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 8
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, Some(45));
        assert_eq!(usage.usage.cached_input, Some(30));
        assert_eq!(usage.usage.cached_write, Some(5));
        assert_eq!(usage.usage.output, Some(8));
        assert!(usage.extra.is_empty());
        let metrics = openai().metrics(&usage);
        assert_eq!(metrics[general::TOKENS_INPUT].value.as_deref(), Some("45"));
        assert_eq!(
            metrics[general::TOKENS_CACHED_INPUT].value.as_deref(),
            Some("30")
        );
    }

    #[test]
    fn named_but_uncounted_is_unknown_never_zero() {
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": null,
                    "output_tokens": "many",
                    "cache_read_input_tokens": null,
                    "total_tokens": 42
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, None);
        assert_eq!(usage.usage.output, None);
        // Additive shape with every part uncounted: the provider's own
        // total is a pre-fold number that cannot claim the categories, so
        // the canonical total is unknown, never the ambiguous 42.
        assert_eq!(usage.usage.total, None);
        assert_eq!(
            usage.unknown,
            vec![
                general::TOKENS_INPUT.to_owned(),
                general::TOKENS_CACHED_INPUT.to_owned(),
                general::TOKENS_OUTPUT.to_owned(),
                general::TOKENS_TOTAL.to_owned(),
            ]
        );
        let metrics = openai().metrics(&usage);
        assert_eq!(metrics[general::TOKENS_INPUT].quality, Quality::Unknown);
        assert_eq!(metrics[general::TOKENS_INPUT].value, None);
        assert_eq!(metrics[general::TOKENS_TOTAL].value, None);
        assert_eq!(metrics[general::TOKENS_TOTAL].quality, Quality::Unknown);
        // An input field that counted nothing still refuses to shadow its
        // subsets: the cached share survives with no base under it.
        assert_eq!(usage.usage.cached_input, None);
    }

    #[test]
    fn an_uncounted_additive_part_makes_the_aggregate_unknown() {
        // A counted base beside a named-but-uncounted additive share is not
        // a total: the record answered part of the sum and stayed silent on
        // the rest, so the aggregate propagates Unknown rather than emit a
        // deceptively precise number that understates the true count.
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": null,
                    "output_tokens": 8,
                    "reasoning_tokens": null,
                    "total_tool_use_tokens": null
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, None);
        assert_eq!(usage.usage.output, None);
        assert_eq!(
            usage.unknown,
            vec![
                general::TOKENS_INPUT.to_owned(),
                general::TOKENS_CACHED_INPUT.to_owned(),
                general::TOKENS_OUTPUT.to_owned(),
                "openai.usage/reasoning_tokens".to_owned(),
                general::TOKENS_TOTAL.to_owned(),
            ]
        );
        let metrics = openai().metrics(&usage);
        for metric in [general::TOKENS_INPUT, general::TOKENS_OUTPUT] {
            assert_eq!(metrics[metric].quality, Quality::Unknown, "{metric}");
            assert_eq!(metrics[metric].value, None, "{metric}");
        }
    }

    #[test]
    fn an_additive_record_folds_its_total_from_the_parts() {
        // The provider's `total_tokens` is written before the additive
        // cache share it does not contain: under additive semantics the
        // canonical total is the folded sum, never the pre-fold count —
        // the same answer `agent-statistics` gives this record.
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": 100,
                    "cache_write_input_tokens": 50,
                    "output_tokens": 10,
                    "total_tokens": 110
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, Some(150));
        assert_eq!(usage.usage.output, Some(10));
        assert_eq!(usage.usage.total, Some(160));
        assert!(
            usage.extra.is_empty(),
            "the provider's pre-fold total is a claimed path, not an extra"
        );
        assert!(usage.unknown.is_empty());
    }

    #[test]
    fn a_numeric_detail_under_a_token_parent_stays_unitless() {
        // `prompt_tokens_details` is token-shaped as a parent, but its
        // `elapsed_ms` leaf measures a duration: the terminal segment
        // decides the unit, not a substring anywhere in the path.
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "prompt_tokens": 10,
                    "prompt_tokens_details": {"elapsed_ms": 250}
                }
            }))
            .unwrap()
            .unwrap();
        let metrics = openai().metrics(&usage);
        assert_eq!(
            metrics["openai.usage/prompt_tokens_details.elapsed_ms"].unit,
            "1"
        );
    }

    #[test]
    fn an_addition_overflow_is_unknown_never_a_clamped_count() {
        for (field, expected) in [
            ("cache_read_input_tokens", general::TOKENS_INPUT),
            ("reasoning_tokens", general::TOKENS_OUTPUT),
            ("total_tool_use_tokens", general::TOKENS_OUTPUT),
        ] {
            let base = match expected {
                general::TOKENS_INPUT => "input_tokens",
                _ => "output_tokens",
            };
            let usage = openai()
                .normalize(&json!({
                    "usage": {
                        (base): u64::MAX,
                        (field): 1,
                    }
                }))
                .unwrap()
                .unwrap();
            assert!(
                usage.unknown.contains(&expected.to_owned()),
                "{field}: u64::MAX + 1 must not emit a clamped count"
            );
        }
    }

    #[test]
    fn transport_and_measurement_self_assertion_is_refused() {
        for (field, value) in [
            ("source", "elsewhere"),
            ("extensionId", "someone.else/agent"),
            ("instanceId", "instance-2"),
            ("generation", "3"),
            ("registryEpoch", "epoch-9"),
            ("principal", "user-2"),
            ("measurementRef", "call-1"),
        ] {
            let mut usage = serde_json::Map::new();
            usage.insert("input_tokens".to_owned(), json!(5));
            usage.insert(field.to_owned(), json!(value));
            let failure = openai()
                .normalize(&json!({"usage": usage}))
                .expect_err("self-asserted binding");
            assert_eq!(failure.code, "usage_source_self_asserted", "{field}");
        }
    }

    #[test]
    fn metrics_project_shared_names_subsets_and_unknowns() {
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "prompt_tokens": 100,
                    "cache_creation_input_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 30},
                    "completion_tokens": 20,
                    "completion_tokens_details": {"reasoning_tokens": 5},
                    "audio_seconds": 4
                }
            }))
            .unwrap()
            .unwrap();
        let metrics = openai().metrics(&usage);
        let input = &metrics[general::TOKENS_INPUT];
        assert_eq!(input.value.as_deref(), Some("105"));
        assert_eq!(
            input.includes,
            vec![
                general::TOKENS_CACHED_INPUT.to_owned(),
                "openai.usage/cache_write_tokens".to_owned(),
            ]
        );
        assert_eq!(
            metrics[general::TOKENS_CACHED_INPUT].value.as_deref(),
            Some("30")
        );
        assert_eq!(metrics[general::TOKENS_OUTPUT].value.as_deref(), Some("20"));
        assert_eq!(
            metrics["openai.usage/cache_write_tokens"].value.as_deref(),
            Some("5")
        );
        assert_eq!(
            metrics["openai.usage/reasoning_tokens"].value.as_deref(),
            Some("5")
        );
        assert_eq!(
            metrics["openai.usage/audio_seconds"].value.as_deref(),
            Some("4")
        );
        assert_eq!(
            metrics[general::TOKENS_TOTAL].value.as_deref(),
            Some("125"),
            "under additive semantics the canonical total is the folded sum"
        );
        for (metric, reading) in &metrics {
            // `audio_seconds` is a number without a token name: it projects
            // unitless rather than claiming `tokens` the record never wrote.
            let expected = match metric.as_str() {
                "openai.usage/audio_seconds" => "1",
                _ => "tokens",
            };
            assert_eq!(reading.unit, expected, "{metric}");
            assert_eq!(reading.temporality, Temporality::Delta, "{metric}");
        }
    }

    #[test]
    fn extras_claim_tokens_only_when_the_leaf_names_tokens() {
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "prompt_tokens": 10,
                    "prompt_cache_miss_tokens": 4,
                    "audio_seconds": 3,
                    "latency_ms": 250,
                    "created": 1700000000i64
                }
            }))
            .unwrap()
            .unwrap();
        let metrics = openai().metrics(&usage);
        // A leaf whose name denotes a token count keeps `tokens`; a latency,
        // a duration or a timestamp projects the UCUM unitless `1` — the
        // record carried the number, never a unit the map could claim.
        assert_eq!(
            metrics["openai.usage/prompt_cache_miss_tokens"].unit,
            "tokens"
        );
        for metric in [
            "openai.usage/audio_seconds",
            "openai.usage/latency_ms",
            "openai.usage/created",
        ] {
            assert_eq!(metrics[metric].unit, "1", "{metric}");
        }
    }

    #[test]
    fn an_object_valued_counter_is_unknown_never_absent() {
        // The record named `prompt_tokens` under an object, not a count:
        // flattening alone would dissolve the path into its children and the
        // counter would read absent — a partial aggregate could then publish
        // as precise. Restored presence keeps the named-but-uncounted
        // invariant, and the nested count keeps its namespaced place.
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "prompt_tokens": {"counted_elsewhere": 3},
                    "completion_tokens": 5,
                    "total_tokens": 5
                }
            }))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage.input, None);
        assert_eq!(usage.usage.output, Some(5));
        assert_eq!(usage.usage.total, Some(5));
        assert!(
            usage.unknown.contains(&general::TOKENS_INPUT.to_owned()),
            "named but not counted reads Unknown: {usage:?}"
        );
        assert_eq!(
            usage.extra["openai.usage/prompt_tokens.counted_elsewhere"],
            3
        );
    }

    #[test]
    fn an_object_valued_counter_still_discovers_the_object() {
        // Discovery and normalization share one flattened form: a usage
        // object whose only recognized field is object-valued is still a
        // usage object, never silently passed over.
        let usage = openai()
            .normalize(&json!({"usage": {"prompt_tokens": {"a": 1}}}))
            .unwrap()
            .unwrap();
        assert!(
            usage.unknown.contains(&general::TOKENS_INPUT.to_owned()),
            "the named counter discovered the object: {usage:?}"
        );
    }

    #[test]
    fn additive_subsets_name_themselves_in_the_aggregate_includes() {
        let usage = openai()
            .normalize(&json!({
                "usage": {
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 8,
                    "reasoning_tokens": 7
                }
            }))
            .unwrap()
            .unwrap();
        let metrics = openai().metrics(&usage);
        // The namespaced counters fold into the aggregates, so the
        // aggregates publish the subset relation — a consumer summing
        // emitted metrics never double-counts a share the fold claimed.
        assert!(
            metrics[general::TOKENS_INPUT]
                .includes
                .contains(&"openai.usage/cache_write_tokens".to_owned()),
            "{:?}",
            metrics[general::TOKENS_INPUT].includes
        );
        assert!(
            metrics[general::TOKENS_OUTPUT]
                .includes
                .contains(&"openai.usage/reasoning_tokens".to_owned()),
            "{:?}",
            metrics[general::TOKENS_OUTPUT].includes
        );
    }

    #[test]
    fn a_mapping_declared_unknown_reads_but_claims_no_number() {
        let estimated = ProviderMapping::new("openai.usage", Quality::Estimated).expect("mapping");
        let usage = estimated
            .normalize(&json!({"usage": {"prompt_tokens": 12}}))
            .unwrap()
            .unwrap();
        let metrics = estimated.metrics(&usage);
        assert_eq!(metrics[general::TOKENS_INPUT].quality, Quality::Estimated);

        let unknown = ProviderMapping::new("openai.usage", Quality::Unknown).expect("mapping");
        let metrics = unknown.metrics(&usage);
        assert_eq!(metrics[general::TOKENS_INPUT].quality, Quality::Unknown);
        assert_eq!(metrics[general::TOKENS_INPUT].value, None);
    }

    #[test]
    fn empty_and_foreign_records_claim_nothing() {
        for record in [
            json!(null),
            json!([]),
            json!({}),
            json!({"usage": {}}),
            json!({"usage": {"model": "x"}}),
            json!({"choices": []}),
            // An error or status body is not a usage record, however many
            // integers it carries.
            json!({"error": {"code": 429}}),
            json!({"created": 1700000000i64}),
        ] {
            assert_eq!(openai().normalize(&record).unwrap(), None);
        }
        // A record of numbers nobody recognizes still keeps them, namespaced.
        let usage = openai()
            .normalize(&json!({"usage": {"web_search_requests": 2}}))
            .unwrap()
            .unwrap();
        assert_eq!(usage.usage, TokenUsage::default());
        assert_eq!(usage.extra["openai.usage/web_search_requests"], 2);
    }
}
