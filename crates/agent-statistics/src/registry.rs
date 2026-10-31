//! The model-identity port this crate consumes.
//!
//! A host above this crate owns the concrete catalogue: where its snapshot
//! comes from, how it refreshes and which provider aliases it declares. This
//! crate only names the questions it asks — the catalogue revision reported
//! with a scan, the historical identity of a recorded selector, the canonical
//! entry for an already-published model, whether a label names a source rather
//! than a model, and the display typography of an unresolved selector.
//!
//! [`EmptyModelRegistry`] is the fail-closed answer for a process that composed
//! no catalogue. It resolves nothing except the execution-mode identities this
//! crate itself confirms, and its empty revision tells a reader that no
//! catalogue answered.

/// One model identity resolved from a catalogue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModel {
    pub id: String,
    pub display_name: String,
}

/// The catalogue questions the statistics engine asks its host.
///
/// Implementations are read-only after construction: a report captures one
/// catalog revision and resolves every selector against that same snapshot, so
/// two rows in one report cannot disagree about a model's identity.
pub trait ModelRegistry: Send + Sync {
    /// The revision of the snapshot every answer in this report came from.
    fn revision(&self) -> &str;

    /// Resolve a recorded selector for historical usage. A provider's current
    /// floating route is deliberately not followed: only stable model facts and
    /// explicit aliases that preserve the target's full version and variant
    /// admit an older record.
    fn resolve_historical(
        &self,
        raw: &str,
        provider_id: Option<&str>,
        source_agent_id: Option<&str>,
    ) -> Option<ResolvedModel>;

    /// Resolve an already-published canonical ID without following provider
    /// routes or display aliases.
    fn canonical_model(&self, id: &str) -> Option<ResolvedModel>;

    /// Whether `value` names a catalogue provider or an admitted Agent label
    /// rather than a model.
    fn is_provider_label(&self, value: &str) -> bool;

    /// Display typography only. Formatting never establishes an alias or a
    /// canonical model.
    fn display_name(&self, raw: &str) -> String {
        model_display_name(raw)
    }
}

/// The fail-closed registry for a host that composed no catalogue.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyModelRegistry;

impl ModelRegistry for EmptyModelRegistry {
    fn revision(&self) -> &str {
        ""
    }

    fn resolve_historical(
        &self,
        raw: &str,
        _provider_id: Option<&str>,
        _source_agent_id: Option<&str>,
    ) -> Option<ResolvedModel> {
        confirmed_execution_model(&normalize(raw.trim()))
    }

    fn canonical_model(&self, id: &str) -> Option<ResolvedModel> {
        id.contains('/')
            .then(|| confirmed_execution_model(&normalize(id)))
            .flatten()
    }

    fn is_provider_label(&self, _value: &str) -> bool {
        false
    }
}

/// Format both catalogue names and unresolved native IDs in one Rust-owned
/// projection.
pub fn model_display_name(raw: &str) -> String {
    let mut label = raw.trim();
    while let Some((prefix, rest)) = label.split_once('/') {
        if prefix.chars().any(char::is_whitespace) || rest.trim().is_empty() {
            break;
        }
        label = rest.trim();
    }
    if let Some((prefix, rest)) = label.split_once(':')
        && !prefix
            .chars()
            .any(|c| c.is_whitespace() || c.is_ascii_digit())
        && rest.contains(['-', '_'])
    {
        label = rest.trim();
    }
    let spaced = label.replace(['-', '_'], " ");
    let mut words = spaced.split_whitespace().peekable();
    let mut display = Vec::new();
    let mut found_version = false;
    while let Some(word) = words.next() {
        let mut word = word.to_owned();
        if !found_version && word.chars().any(|c| c.is_ascii_digit()) {
            found_version = true;
            let numeric = word
                .strip_prefix('v')
                .or_else(|| word.strip_prefix('V'))
                .unwrap_or(&word);
            if version_component(numeric) {
                while let Some(next) = words.peek().filter(|next| version_component(next)) {
                    word.push('.');
                    word.push_str(next);
                    words.next();
                }
            }
        }
        display.push(display_word(&word));
    }
    if display.len() > 1 && display.last().is_some_and(|word| word == "Default") {
        display.pop();
    }
    if let Some(index) = display.windows(2).position(|words| {
        words[0] == "GPT" && words[1].chars().next().is_some_and(|c| c.is_ascii_digit())
    }) {
        let version = display.remove(index + 1);
        display[index].push('-');
        display[index].push_str(&version);
    }
    display.join(" ")
}

fn version_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 2
        && value.bytes().all(|c| c.is_ascii_digit())
        && (value.len() == 1 || !value.starts_with('0'))
}

fn display_word(word: &str) -> String {
    let lower = word.to_lowercase();
    let brand = match lower.as_str() {
        "gpt" => Some("GPT"),
        "deepseek" => Some("DeepSeek"),
        "openai" => Some("OpenAI"),
        "chatgpt" => Some("ChatGPT"),
        "minimax" => Some("MiniMax"),
        "xai" => Some("xAI"),
        "glm" => Some("GLM"),
        "ai" => Some("AI"),
        "api" => Some("API"),
        "oss" => Some("OSS"),
        "moe" => Some("MoE"),
        _ => None,
    };
    if let Some(brand) = brand {
        return brand.to_owned();
    }
    // Keep deliberate catalogue casing such as QwQ and CodeGeeX. Lowercase
    // native slugs still receive the default title treatment.
    if word.chars().skip(1).any(char::is_uppercase) {
        return word.to_owned();
    }
    let numeric = word.trim_matches(['(', ')', '[', ']']);
    if numeric.chars().next().is_some_and(|c| c.is_ascii_digit())
        && numeric
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'k' | 'K' | 'm' | 'M' | 'b' | 'B'))
    {
        return word.to_ascii_uppercase();
    }
    let Some((offset, letter)) = lower
        .char_indices()
        .find(|(_, letter)| letter.is_alphabetic())
    else {
        return lower;
    };
    format!(
        "{}{}{}",
        &lower[..offset],
        letter.to_uppercase(),
        &lower[offset + letter.len_utf8()..]
    )
}

/// Two official execution-mode identities supplement catalogues that list
/// serving tiers as separate rows. This is deliberately not a generic `-fast`
/// rule: other products use Fast as a distinct model name.
fn confirmed_execution_model(key: &str) -> Option<ResolvedModel> {
    if !matches!(
        key,
        "composer2.5" | "composer2.5fast" | "cursorcomposer2.5" | "cursorcomposer2.5fast"
    ) {
        return None;
    }
    Some(ResolvedModel {
        id: "cursor/composer-2.5".to_owned(),
        display_name: "Composer 2.5".to_owned(),
    })
}

#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum Alias {
    Unique(usize),
    Ambiguous,
}

/// A catalogue fixture for this crate's own tests. Production hosts answer the
/// port with their own catalogue; this double exists so engine assertions stay
/// independent of any real file or network reader.
#[cfg(test)]
pub(crate) struct TestRegistry {
    models: Vec<ResolvedModel>,
    aliases: HashMap<String, Alias>,
    canonical_ids: HashMap<String, Alias>,
    canonical_aliases: HashMap<String, Alias>,
    historical_aliases: HashMap<String, Alias>,
    wrappers: HashSet<String>,
    revision: String,
}

#[cfg(test)]
impl TestRegistry {
    pub(crate) fn empty() -> Self {
        Self {
            models: Vec::new(),
            aliases: HashMap::new(),
            canonical_ids: HashMap::new(),
            canonical_aliases: HashMap::new(),
            historical_aliases: HashMap::new(),
            wrappers: HashSet::new(),
            revision: String::new(),
        }
    }

    pub(crate) fn from_catalog(catalog: Value) -> anyhow::Result<Self> {
        let parsed: TestCatalog = serde_json::from_value(catalog.clone())?;
        anyhow::ensure!(!parsed.models.is_empty(), "model_registry_catalog_empty");
        let mut registry = Self::empty();
        let mut ids = BTreeMap::<String, usize>::new();
        for (id, facts) in &parsed.models {
            if !id.contains('/') {
                continue;
            }
            registry.insert_model(&mut ids, id, facts);
        }
        anyhow::ensure!(!registry.models.is_empty(), "model_registry_catalog_empty");
        // Stable model IDs and names must not acquire the changing destination
        // of a provider's floating endpoint alias when projecting older usage.
        registry.canonical_aliases = registry.aliases.clone();
        for (provider_id, provider) in &parsed.providers {
            registry.wrappers.insert(normalize(provider_id));
            registry.wrappers.insert(normalize(&provider.name));
            for (id, facts) in &provider.models {
                let Some(base) = facts.base_model.as_deref() else {
                    continue;
                };
                let Some(index) = ids.get(base).copied() else {
                    continue;
                };
                if let Some(key) = stable_version_alias(id, &registry.models[index]) {
                    insert_index(&mut registry.historical_aliases, key, index);
                }
                registry.insert_alias(id, index);
                registry.insert_alias(&format!("{provider_id}/{id}"), index);
                registry.insert_alias(&format!("{} {id}", provider.name), index);
                if let Some(name) = facts.name.as_deref() {
                    registry.insert_alias(name, index);
                    registry.insert_alias(&format!("{} {name}", provider.name), index);
                }
            }
        }
        registry.revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&catalog)?));
        Ok(registry)
    }

    fn insert_model(
        &mut self,
        ids: &mut BTreeMap<String, usize>,
        id: &str,
        facts: &TestModelFacts,
    ) {
        if ids.contains_key(id) {
            return;
        }
        let name = facts.name.as_deref().unwrap_or(id);
        let index = self.models.len();
        self.models.push(ResolvedModel {
            id: id.to_owned(),
            display_name: model_display_name(name),
        });
        ids.insert(id.to_owned(), index);
        insert_index(&mut self.canonical_ids, id.to_lowercase(), index);
        self.insert_alias(id, index);
        self.insert_alias(name, index);
        if let Some((_, short)) = id.split_once('/') {
            self.insert_alias(short, index);
        }
    }

    fn insert_alias(&mut self, alias: &str, index: usize) {
        let key = normalize(alias);
        if !key.is_empty() {
            insert_index(&mut self.aliases, key, index);
        }
    }
}

#[cfg(test)]
impl ModelRegistry for TestRegistry {
    fn revision(&self) -> &str {
        &self.revision
    }

    fn resolve_historical(
        &self,
        raw: &str,
        _provider_id: Option<&str>,
        source_agent_id: Option<&str>,
    ) -> Option<ResolvedModel> {
        let raw = raw.trim();
        if raw.is_empty() || self.is_provider_label(raw) {
            return None;
        }
        let source = source_agent_id.map(normalize);
        let mut pending = VecDeque::from([raw.to_owned()]);
        let mut seen = HashSet::new();
        let mut resolved: Option<ResolvedModel> = None;
        while let Some(candidate) = pending.pop_front() {
            if !seen.insert(candidate.clone()) {
                continue;
            }
            let key = normalize(&candidate);
            let exact = candidate.to_lowercase();
            let model = match self
                .canonical_ids
                .get(&exact)
                .or_else(|| self.canonical_aliases.get(&key))
                .or_else(|| self.historical_aliases.get(&key))
            {
                Some(Alias::Ambiguous) => return None,
                Some(Alias::Unique(index)) => Some(self.models[*index].clone()),
                None => confirmed_execution_model(&key),
            };
            if let Some(model) = model {
                if resolved
                    .as_ref()
                    .is_some_and(|previous| previous.id != model.id)
                {
                    return None;
                }
                resolved = Some(model);
                continue;
            }
            if let Some(unwrapped) = strip_suffix(&candidate) {
                pending.push_back(unwrapped);
            }
            let mut prefix = String::new();
            let mut previous_digit = false;
            for (offset, character) in candidate.char_indices() {
                if character.is_alphanumeric() {
                    prefix.extend(character.to_lowercase());
                    previous_digit = character.is_ascii_digit();
                    continue;
                }
                if self.wrappers.contains(&prefix) || source.as_deref() == Some(&prefix) {
                    let rest =
                        candidate[offset..].trim_start_matches(|c: char| !c.is_alphanumeric());
                    if !rest.is_empty() {
                        pending.push_back(rest.to_owned());
                    }
                }
                if matches!(character, '.' | '-' | '_')
                    && previous_digit
                    && candidate[offset + character.len_utf8()..]
                        .chars()
                        .next()
                        .is_some_and(|next| next.is_ascii_digit())
                {
                    prefix.push('.');
                }
                previous_digit = false;
            }
        }
        resolved
    }

    fn canonical_model(&self, id: &str) -> Option<ResolvedModel> {
        match self.canonical_ids.get(&id.to_lowercase()) {
            Some(Alias::Unique(index)) => Some(self.models[*index].clone()),
            Some(Alias::Ambiguous) => None,
            None if id.contains('/') => confirmed_execution_model(&normalize(id)),
            None => None,
        }
    }

    fn is_provider_label(&self, value: &str) -> bool {
        let key = normalize(value);
        !key.is_empty() && self.wrappers.contains(&key)
    }
}

#[cfg(test)]
#[derive(Debug, Default, serde::Deserialize)]
struct TestCatalog {
    #[serde(default)]
    models: BTreeMap<String, TestModelFacts>,
    #[serde(default)]
    providers: BTreeMap<String, TestProvider>,
}

#[cfg(test)]
#[derive(Debug, Default, serde::Deserialize)]
struct TestModelFacts {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_model: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Default, serde::Deserialize)]
struct TestProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: BTreeMap<String, TestModelFacts>,
}

#[cfg(test)]
fn stable_version_alias(alias: &str, model: &ResolvedModel) -> Option<String> {
    let mut alias = alias.to_owned();
    while let Some(unwrapped) = strip_suffix(&alias) {
        alias = unwrapped;
    }
    let key = normalize(&alias);
    if !key.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let short_id = model.id.split_once('/')?.1;
    for name in [short_id, &model.display_name] {
        let mut boundary = true;
        for (offset, character) in name.char_indices() {
            if boundary && normalize(&name[offset..]) == key {
                return Some(key);
            }
            if character.is_ascii_digit() {
                break;
            }
            boundary = !character.is_alphanumeric();
        }
    }
    None
}

#[cfg(test)]
fn strip_suffix(value: &str) -> Option<String> {
    let value = value.trim_end();
    for (offset, character) in value.char_indices() {
        if !matches!(character, '-' | '_' | '.' | ' ' | '[' | '(') {
            continue;
        }
        let suffix = value[offset + character.len_utf8()..]
            .trim_end_matches([']', ')'])
            .trim();
        if suffix_matches(suffix) {
            let unwrapped = value[..offset].trim_end();
            if !unwrapped.is_empty() {
                return Some(unwrapped.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
fn suffix_matches(suffix: &str) -> bool {
    let lower = suffix.to_lowercase();
    let effort = lower.strip_suffix(" fast").unwrap_or(&lower);
    if matches!(
        effort,
        "high"
            | "xhigh"
            | "low"
            | "medium"
            | "max"
            | "ultra"
            | "minimal"
            | "none"
            | "extra high"
            | "extra-high"
            | "extra_high"
            | "fast"
    ) {
        return true;
    }
    let token = lower
        .strip_suffix(" context")
        .or_else(|| lower.strip_suffix("-context"))
        .or_else(|| lower.strip_suffix("_context"))
        .unwrap_or(&lower);
    let Some(unit) = token.chars().last() else {
        return false;
    };
    if !matches!(unit, 'k' | 'm') {
        return false;
    }
    let number = &token[..token.len() - unit.len_utf8()];
    !number.is_empty()
        && number.chars().next().is_some_and(|c| c.is_ascii_digit())
        && number.chars().all(|c| c.is_ascii_digit() || c == '.')
}

#[cfg(test)]
fn insert_index<K: Eq + std::hash::Hash>(indexes: &mut HashMap<K, Alias>, key: K, index: usize) {
    indexes
        .entry(key)
        .and_modify(|previous| {
            if !matches!(previous, Alias::Unique(existing) if *existing == index) {
                *previous = Alias::Ambiguous;
            }
        })
        .or_insert(Alias::Unique(index));
}

fn normalize(value: &str) -> String {
    let mut normalized = String::new();
    let mut characters = value.chars().peekable();
    let mut previous_digit = false;
    while let Some(character) = characters.next() {
        if character.is_alphanumeric() {
            normalized.extend(character.to_lowercase());
        } else if matches!(character, '.' | '-' | '_')
            && previous_digit
            && characters.peek().is_some_and(char::is_ascii_digit)
        {
            normalized.push('.');
        }
        previous_digit = character.is_ascii_digit();
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_shared_typography_not_identity() {
        assert_eq!(
            model_display_name("cursor/composer-2.5-fast"),
            "Composer 2.5 Fast"
        );
        assert_eq!(model_display_name("openai/gpt-5.1"), "GPT-5.1");
        assert_eq!(model_display_name("QwQ-32B"), "QwQ 32B");
        assert_eq!(
            model_display_name("private/custom-model-v2"),
            "Custom Model V2"
        );
        assert_eq!(
            model_display_name("Claude 3.5 Sonnet Default"),
            "Claude 3.5 Sonnet"
        );
    }

    #[test]
    fn empty_registry_only_confirms_execution_mode_identities() {
        let registry = EmptyModelRegistry;
        assert_eq!(registry.revision(), "");
        assert!(!registry.is_provider_label("cursor"));
        assert_eq!(
            registry.resolve_historical("composer-2.5-fast", None, Some("cursor")),
            Some(ResolvedModel {
                id: "cursor/composer-2.5".to_owned(),
                display_name: "Composer 2.5".to_owned(),
            })
        );
        assert_eq!(
            registry.canonical_model("cursor/composer-2.5").unwrap().id,
            "cursor/composer-2.5"
        );
        assert!(registry.resolve_historical("unknown", None, None).is_none());
    }

    #[test]
    fn test_registry_resolves_aliases_but_not_floating_historical_routes() {
        let registry = TestRegistry::from_catalog(serde_json::json!({
            "models": {
                "lab/model-1":{"name":"First Model"},
                "lab/model-2":{"name":"Second Model"}
            },
            "providers": {
                "relay-a":{"models":{"lab/flash-latest":{"base_model":"lab/model-1"}}},
                "relay-b":{"models":{"lab/flash-latest":{"base_model":"lab/model-2"}}}
            }
        }))
        .unwrap();
        assert_eq!(
            registry.resolve_historical("model-1", None, Some("opencode")),
            Some(ResolvedModel {
                id: "lab/model-1".to_owned(),
                display_name: "First Model".to_owned(),
            })
        );
        assert!(
            registry
                .resolve_historical("lab/flash-latest", None, Some("opencode"))
                .is_none()
        );
        assert!(registry.canonical_model("lab/flash-latest").is_none());
        assert!(registry.canonical_model("lab/model-1").is_some());
    }
}
