//! Language-neutral Memory protocol. No persistent store, ranking, or provider implementation.

use serde::de::{DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const API_MAJOR: u32 = 1;
pub const API_MINOR: u32 = 0;
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_TRANSACTION_IO: usize = 16 * MAX_FRAME;
pub const MAX_TEXT: usize = 32 * 1024;
pub const MAX_LEARNING_RESULT: usize = 64 * 1024;
const MAX_DEPTH: usize = 24;
const MAX_COLLECTION: usize = 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Recall,
    Records,
    Context,
    Learning,
    Associations,
    HostLlm,
    Commands,
    /// Optional human-management extension; granted by a sealed host caller, never tool JSON.
    Management,
    /// Offline migration advertisement, never a normal provider-context grant.
    LegacyAdoptionV1,
    LegacyAdoptionV2,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unavailable,
    Disabled,
    UnsupportedCapability,
    InvalidRequest,
    IncompatibleProtocol,
    PermissionDenied,
    NotFound,
    Conflict,
    Busy,
    Timeout,
    AmbiguousResult,
    RecoveryRequired,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryError {
    pub code: ErrorCode,
    pub message: String,
}

impl MemoryError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for MemoryError {}

pub type Result<T> = std::result::Result<T, MemoryError>;

pub trait Validate {
    fn validate(&self) -> Result<()>;
}

pub fn text(value: &str, max: usize, multiline: bool) -> Result<()> {
    if value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
    {
        return Err(MemoryError::invalid(
            "text exceeds its limit or contains control characters",
        ));
    }
    Ok(())
}

pub fn opaque_id(value: &str) -> Result<()> {
    text(value, 256, false)?;
    if value.is_empty() {
        return Err(MemoryError::invalid("empty identifier"));
    }
    Ok(())
}

fn finite_score(value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(MemoryError::invalid(
            "score must be finite and between zero and one",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u32,
    pub minor: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Initialized {
    pub protocol: ProtocolVersion,
    pub provider: ProviderIdentity,
    pub capabilities: Vec<Capability>,
}

impl Validate for Initialized {
    fn validate(&self) -> Result<()> {
        if self.protocol.major != API_MAJOR || self.protocol.minor > API_MINOR {
            return Err(MemoryError::new(
                ErrorCode::IncompatibleProtocol,
                "provider protocol is incompatible",
            ));
        }
        opaque_id(&self.provider.id)?;
        opaque_id(&self.provider.version)?;
        if self.capabilities.len() > 8
            || self.capabilities.iter().collect::<BTreeSet<_>>().len() != self.capabilities.len()
        {
            return Err(MemoryError::invalid("duplicate or excessive capabilities"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub context_tokens: usize,
    pub recall_tokens: usize,
    pub max_records: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderContext {
    pub generation: u64,
    pub user_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub cwd: String,
    pub device_id: String,
    pub device_aliases: Vec<String>,
    pub session_id: String,
    pub turn_id: String,
    pub boundary_id: String,
    pub data_root: String,
    pub settings: BTreeMap<String, Value>,
    pub granted_capabilities: Vec<Capability>,
    pub budgets: Budgets,
}

impl Validate for ProviderContext {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.user_id,
            &self.workspace_id,
            &self.device_id,
            &self.session_id,
            &self.turn_id,
            &self.boundary_id,
        ] {
            opaque_id(value)?;
        }
        for path in [&self.workspace_root, &self.cwd, &self.data_root] {
            text(path, 32768, false)?;
            if !std::path::Path::new(path).is_absolute() {
                return Err(MemoryError::invalid("context paths must be absolute"));
            }
        }
        if self.granted_capabilities.iter().any(|cap| {
            matches!(
                cap,
                Capability::LegacyAdoptionV1 | Capability::LegacyAdoptionV2
            )
        }) {
            return Err(MemoryError::invalid(
                "offline adoption is not a context capability",
            ));
        }
        if self.device_aliases.len() > 32
            || self.settings.len() > 32
            || self.granted_capabilities.len() > 8
            || self
                .granted_capabilities
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != self.granted_capabilities.len()
            || self.budgets.context_tokens > 4096
            || self.budgets.recall_tokens > 4096
            || self.budgets.max_records > 100
        {
            return Err(MemoryError::invalid("context limits exceeded"));
        }
        for alias in &self.device_aliases {
            opaque_id(alias)?;
        }
        for (key, value) in &self.settings {
            text(key, 64, false)?;
            if serde_json::to_vec(value)
                .map_err(|_| MemoryError::invalid("invalid setting"))?
                .len()
                > 4096
            {
                return Err(MemoryError::invalid("setting is too large"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Initialize<'a> {
    pub protocol: ProtocolVersion,
    pub context: &'a ProviderContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Device,
    Project,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub kind: String,
    pub source: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub content: String,
    pub scope: Scope,
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    pub revision: u64,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

impl Validate for Record {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.id)?;
        text(&self.content, MAX_TEXT, true)?;
        if self.content.is_empty() || self.revision == 0 || self.metadata.len() > 32 {
            return Err(MemoryError::invalid("invalid record"));
        }
        if let Some(workspace) = &self.workspace_id {
            opaque_id(workspace)?;
        }
        if self.scope == Scope::Project && self.workspace_id.is_none() {
            return Err(MemoryError::invalid(
                "project record has no workspace identity",
            ));
        }
        if let Some(confidence) = self.confidence {
            finite_score(confidence)?;
        }
        if let Some(provenance) = &self.provenance {
            text(&provenance.kind, 64, false)?;
            for field in [
                &provenance.source,
                &provenance.session_id,
                &provenance.turn_id,
            ]
            .into_iter()
            .flatten()
            {
                text(field, 1024, false)?;
            }
        }
        if serde_json::to_vec(&self.metadata)
            .map_err(|_| MemoryError::invalid("invalid metadata"))?
            .len()
            > 8192
        {
            return Err(MemoryError::invalid("metadata limit exceeded"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordResult {
    pub record: Record,
}

impl Validate for RecordResult {
    fn validate(&self) -> Result<()> {
        self.record.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    pub record: Record,
    pub score: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hits {
    pub items: Vec<Hit>,
}

impl Validate for Hits {
    fn validate(&self) -> Result<()> {
        if self.items.len() > 100 {
            return Err(MemoryError::invalid("too many search results"));
        }
        let mut ids = BTreeSet::new();
        for hit in &self.items {
            hit.record.validate()?;
            finite_score(hit.score)?;
            if !ids.insert(&hit.record.id) {
                return Err(MemoryError::invalid("duplicate record in results"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextItem {
    pub id: String,
    pub text: String,
    pub scope: Scope,
    pub score: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextItems {
    pub items: Vec<ContextItem>,
}

impl Validate for ContextItems {
    fn validate(&self) -> Result<()> {
        if self.items.len() > 64 {
            return Err(MemoryError::invalid("too many context items"));
        }
        let mut bytes = 0usize;
        let mut ids = BTreeSet::new();
        for item in &self.items {
            opaque_id(&item.id)?;
            text(&item.text, MAX_TEXT, true)?;
            if let Some(score) = item.score {
                finite_score(score)?;
            }
            bytes = bytes.saturating_add(item.text.len());
            if bytes > 128 * 1024 || !ids.insert(&item.id) {
                return Err(MemoryError::invalid("invalid context collection"));
            }
        }
        Ok(())
    }
}

/// Bounded graph weights are evidence for an existing host Skill, never permission to load one.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AffinityItem {
    pub node: String,
    pub score: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AffinityResult {
    pub items: Vec<AffinityItem>,
}
impl Validate for AffinityResult {
    fn validate(&self) -> Result<()> {
        if self.items.len() > 100 {
            return Err(MemoryError::invalid("too many affinity results"));
        }
        let mut nodes = BTreeSet::new();
        for item in &self.items {
            opaque_id(&item.node)?;
            finite_score(item.score)?;
            if !item.node.starts_with("skill:") || item.node.len() == 6 || !nodes.insert(&item.node)
            {
                return Err(MemoryError::invalid(
                    "invalid or duplicate Skill affinity node",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceChannel {
    Standing,
    Recall,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSource {
    pub id: String,
    pub revision: u64,
    pub scope: Scope,
    pub workspace_id: Option<String>,
    pub content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedFact {
    pub handle: String,
    pub channel: EvidenceChannel,
    pub source: EvidenceSource,
    /// The exact record body supplied to the host renderer, before wrapper escaping.
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedEvidence {
    pub provider_id: String,
    pub generation: u64,
    pub user_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub boundary_id: String,
    pub facts: Vec<RenderedFact>,
}

impl Validate for EvidenceSource {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.id)?;
        sha256_text(&self.content_sha256)?;
        if self.revision == 0 || (self.scope == Scope::Project && self.workspace_id.is_none()) {
            return Err(MemoryError::invalid("invalid evidence source"));
        }
        if let Some(workspace) = &self.workspace_id {
            opaque_id(workspace)?;
        }
        Ok(())
    }
}

impl Validate for RenderedEvidence {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.provider_id,
            &self.user_id,
            &self.workspace_id,
            &self.session_id,
            &self.turn_id,
            &self.boundary_id,
        ] {
            opaque_id(value)?;
        }
        if self.generation == 0 || self.facts.len() > 64 {
            return Err(MemoryError::invalid("invalid rendered evidence envelope"));
        }
        let mut handles = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for fact in &self.facts {
            if !fact.handle.starts_with('m')
                || fact.handle.len() < 2
                || fact.handle[1..].bytes().any(|byte| !byte.is_ascii_digit())
                || !handles.insert(&fact.handle)
                || !ids.insert(&fact.source.id)
            {
                return Err(MemoryError::invalid(
                    "duplicate or invalid rendered evidence",
                ));
            }
            fact.source.validate()?;
            text(&fact.text, MAX_TEXT, true)?;
            if fact.text.trim().is_empty()
                || (fact.source.scope == Scope::Project
                    && fact.source.workspace_id.as_ref() != Some(&self.workspace_id))
            {
                return Err(MemoryError::invalid(
                    "invalid rendered evidence body or workspace",
                ));
            }
        }
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > 32 * 1024) {
            return Err(MemoryError::invalid(
                "rendered evidence exceeds aggregate limit",
            ));
        }
        Ok(())
    }
}

impl RenderedEvidence {
    pub fn validate_used(&self, used: &[String]) -> Result<()> {
        let valid = self
            .facts
            .iter()
            .map(|fact| fact.handle.as_str())
            .collect::<BTreeSet<_>>();
        let mut seen = BTreeSet::new();
        if used.len() > valid.len() {
            return Err(MemoryError::invalid("too many useful-use claims"));
        }
        for handle in used {
            if !seen.insert(handle) || !valid.contains(handle.as_str()) {
                return Err(MemoryError::invalid(
                    "unknown or duplicate useful-use claim",
                ));
            }
        }
        Ok(())
    }
}

/// Bounded host-owned completed-turn input. The digest binds text and rendered evidence to the
/// explicit turn scope; it is an integrity/idempotency binding, not executable authentication.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTurn {
    pub operation_id: String,
    pub user_text: String,
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub confirmed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_evidence: Option<RenderedEvidence>,
}

impl Validate for ProcessTurn {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.operation_id)?;
        if self.operation_id.trim() != self.operation_id || self.user_text.trim().is_empty() {
            return Err(MemoryError::invalid(
                "invalid completed-turn identity or user text",
            ));
        }
        text(&self.user_text, MAX_TEXT, true)?;
        text(&self.transcript, MAX_TEXT, true)?;
        if let Some(evidence) = &self.rendered_evidence {
            evidence.validate()?;
        }
        if self.payload_sha256.as_ref().is_some_and(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) || (!self.transcript.is_empty() && self.payload_sha256.is_none())
        {
            return Err(MemoryError::invalid(
                "invalid or missing completed-turn digest",
            ));
        }
        Ok(())
    }
}

impl ProcessTurn {
    /// Canonical JSON bytes whose SHA256 the host and provider compare before any effects.
    pub fn payload_bytes(&self, context: &ProviderContext) -> Result<Vec<u8>> {
        serde_json::to_vec(&serde_json::json!({
            "user_text":self.user_text,"transcript":self.transcript,
            "session_id":context.session_id,"turn_id":context.turn_id,
            "generation":context.generation,"workspace_id":context.workspace_id,
            "rendered_evidence":self.rendered_evidence,
        }))
        .map_err(|_| MemoryError::invalid("cannot encode completed-turn payload"))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCandidate {
    pub text: String,
    pub anchor: Option<String>,
    pub importance: u8,
}

/// A provider-owned no-tool extraction request. The host supplies no route or credentials.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareJob {
    pub operation_id: String,
    pub turn: ProcessTurn,
    pub tool_calls: u32,
    pub recovered: bool,
}

impl Validate for PrepareJob {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.operation_id)?;
        self.turn.validate()?;
        if self.operation_id != format!("{}-prepare", self.turn.operation_id)
            || self.turn.payload_sha256.is_none()
            || self.turn.dry_run
            || self.turn.confirmed
            || self.tool_calls > 100_000
        {
            return Err(MemoryError::invalid("invalid prepared learning request"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelJob {
    pub id: String,
    pub system: String,
    pub input: String,
    pub corpus_sha256: String,
    pub max_output_tokens: u32,
    pub expires_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<RenderedEvidence>,
    #[serde(default)]
    pub injected_handles: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareJobResult {
    pub operation_id: String,
    pub turn_operation_id: String,
    pub payload_sha256: String,
    pub job: Option<ModelJob>,
}

fn sha256_text(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(MemoryError::invalid("invalid SHA256 binding"));
    }
    Ok(())
}

impl Validate for PrepareJobResult {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.operation_id)?;
        opaque_id(&self.turn_operation_id)?;
        sha256_text(&self.payload_sha256)?;
        if self.operation_id != format!("{}-prepare", self.turn_operation_id) {
            return Err(MemoryError::invalid("prepared learning identity mismatch"));
        }
        if let Some(job) = &self.job {
            opaque_id(&job.id)?;
            sha256_text(&job.corpus_sha256)?;
            text(&job.system, MAX_TEXT, true)?;
            text(&job.input, MAX_TEXT, true)?;
            if job.system.trim().is_empty()
                || job.input.trim().is_empty()
                || !(1..=2048).contains(&job.max_output_tokens)
                || job.expires_unix_secs == 0
            {
                return Err(MemoryError::invalid("invalid no-tool model job"));
            }
            if let Some(evidence) = &job.evidence {
                evidence.validate()?;
                evidence.validate_used(&job.injected_handles)?;
            } else if !job.injected_handles.is_empty() {
                return Err(MemoryError::invalid("job handles lack rendered evidence"));
            }
            if job.injected_handles.len() > 64
                || job.injected_handles.iter().collect::<BTreeSet<_>>().len()
                    != job.injected_handles.len()
            {
                return Err(MemoryError::invalid("duplicate prepared evidence handles"));
            }
        }
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > MAX_LEARNING_RESULT) {
            return Err(MemoryError::invalid("prepared job exceeds aggregate limit"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteJob {
    pub operation_id: String,
    pub prepare_operation_id: String,
    pub job_id: String,
    pub payload_sha256: String,
    pub completion: String,
}

impl Validate for CompleteJob {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.operation_id)?;
        opaque_id(&self.prepare_operation_id)?;
        opaque_id(&self.job_id)?;
        sha256_text(&self.payload_sha256)?;
        text(&self.completion, MAX_TEXT, true)?;
        if self.prepare_operation_id != format!("{}-prepare", self.operation_id) {
            return Err(MemoryError::invalid("completion identity mismatch"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningMode {
    /// Only deterministic user-text extraction ran. No transcript/model extraction is implied.
    Offline,
    /// Core executed the provider's prepared prompt using the completed turn's exact model route.
    SameTurnModel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessTurnResult {
    pub accepted: bool,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_sha256: Option<String>,
    #[serde(default)]
    pub useful: Vec<String>,
    #[serde(default)]
    pub already_useful: Vec<String>,
    pub learning_mode: LearningMode,
    pub added: Vec<String>,
    pub reinforced: Vec<String>,
    pub review: Vec<String>,
    pub skipped_passive: bool,
    pub session_candidates: Vec<SessionCandidate>,
}

impl Validate for ProcessTurnResult {
    fn validate(&self) -> Result<()> {
        opaque_id(&self.operation_id)?;
        if !self.accepted || self.operation_id.trim() != self.operation_id {
            return Err(MemoryError::invalid(
                "invalid completed-turn acknowledgement",
            ));
        }
        if self.payload_sha256.as_ref().is_some_and(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(MemoryError::invalid(
                "invalid completed-turn receipt digest",
            ));
        }
        for ids in [
            &self.added,
            &self.reinforced,
            &self.review,
            &self.useful,
            &self.already_useful,
        ] {
            if ids.len() > 100 {
                return Err(MemoryError::invalid("too many learning results"));
            }
            for id in ids {
                opaque_id(id)?;
            }
            if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
                return Err(MemoryError::invalid("duplicate learning result"));
            }
        }
        if self
            .useful
            .iter()
            .any(|id| self.already_useful.contains(id))
        {
            return Err(MemoryError::invalid("contradictory useful-use result"));
        }
        if self.session_candidates.len() > 32 {
            return Err(MemoryError::invalid("too many session candidates"));
        }
        for candidate in &self.session_candidates {
            text(&candidate.text, 1600, true)?;
            if candidate.text.trim().is_empty() || !(1..=10).contains(&candidate.importance) {
                return Err(MemoryError::invalid("invalid session candidate"));
            }
            if let Some(anchor) = &candidate.anchor {
                text(anchor, 4096, false)?;
            }
        }
        if serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > MAX_LEARNING_RESULT) {
            return Err(MemoryError::invalid(
                "completed-turn receipt exceeds aggregate limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Capabilities,
    Health,
    SessionStart,
    SessionEnd,
    SessionReset,
    Recall,
    Store,
    Get,
    Update,
    Forget,
    Search,
    List,
    BuildContext,
    ProcessTurn,
    ConfirmUse,
    OperationStatus,
    Associate,
    Affinity,
    PrepareJob,
    CompleteJob,
    Command,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Health => "health",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::SessionReset => "session_reset",
            Self::Recall => "recall",
            Self::Store => "store",
            Self::Get => "get",
            Self::Update => "update",
            Self::Forget => "forget",
            Self::Search => "search",
            Self::List => "list",
            Self::BuildContext => "build_context",
            Self::ProcessTurn => "process_turn",
            Self::ConfirmUse => "confirm_use",
            Self::OperationStatus => "operation_status",
            Self::Associate => "associate",
            Self::Affinity => "affinity",
            Self::PrepareJob => "prepare_job",
            Self::CompleteJob => "complete_job",
            Self::Command => "command",
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        serde_json::from_value(Value::String(name.into()))
            .map_err(|_| MemoryError::invalid("unknown Memory operation"))
    }

    pub fn capability(self) -> Option<Capability> {
        match self {
            Self::Capabilities
            | Self::Health
            | Self::SessionStart
            | Self::SessionEnd
            | Self::SessionReset => None,
            Self::Recall => Some(Capability::Recall),
            Self::Store | Self::Get | Self::Update | Self::Forget | Self::Search | Self::List => {
                Some(Capability::Records)
            }
            Self::BuildContext => Some(Capability::Context),
            Self::ProcessTurn | Self::ConfirmUse | Self::OperationStatus => {
                Some(Capability::Learning)
            }
            Self::Associate | Self::Affinity => Some(Capability::Associations),
            Self::PrepareJob | Self::CompleteJob => Some(Capability::HostLlm),
            Self::Command => Some(Capability::Commands),
        }
    }

    pub fn long_deadline(self) -> bool {
        matches!(
            self,
            Self::ProcessTurn | Self::PrepareJob | Self::CompleteJob | Self::Command
        )
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct Request<'a, P> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: &'a P,
}

pub fn encode_request<P: Serialize>(id: u64, method: &str, params: &P) -> Result<Vec<u8>> {
    if id == 0 {
        return Err(MemoryError::invalid("request ID must be positive"));
    }
    text(method, 64, false)?;
    if !matches!(method, "initialize" | "shutdown") {
        Method::from_name(method)?;
    }
    let mut bytes = serde_json::to_vec(&Request {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .map_err(|_| MemoryError::invalid("cannot serialize request"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err(MemoryError::invalid("request frame exceeds limit"));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: i64,
    message: String,
    data: MemoryError,
}

/// A correlated semantic error is a valid reply, not a framing/decoding failure.
#[derive(Debug)]
pub enum Reply<T> {
    Result(T),
    Error(MemoryError),
}

#[cfg(test)]
pub fn decode_response<T: DeserializeOwned>(frame: &[u8], expected_id: u64) -> Result<T> {
    match decode_reply(frame, expected_id)? {
        Reply::Result(value) => Ok(value),
        Reply::Error(error) => Err(error),
    }
}

pub fn decode_reply<T: DeserializeOwned>(frame: &[u8], expected_id: u64) -> Result<Reply<T>> {
    if frame.len() > MAX_FRAME
        || !frame.ends_with(b"\n")
        || frame[..frame.len().saturating_sub(1)].contains(&b'\n')
    {
        return Err(MemoryError::invalid("invalid response framing"));
    }
    let mut object = strict_json(frame)?
        .as_object()
        .cloned()
        .ok_or_else(|| MemoryError::invalid("response must be an object"))?;
    if object.remove("jsonrpc") != Some(Value::String("2.0".into()))
        || object.remove("id").and_then(|id| id.as_u64()) != Some(expected_id)
    {
        return Err(MemoryError::invalid(
            "response protocol or request ID mismatch",
        ));
    }
    if object.len() != 1 {
        return Err(MemoryError::invalid(
            "response requires exactly one result or error",
        ));
    }
    if let Some(value) = object.remove("result") {
        return serde_json::from_value(value)
            .map(Reply::Result)
            .map_err(|_| MemoryError::invalid("response result schema mismatch"));
    }
    let wire: WireError = serde_json::from_value(
        object
            .remove("error")
            .ok_or_else(|| MemoryError::invalid("unknown response field"))?,
    )
    .map_err(|_| MemoryError::invalid("response error schema mismatch"))?;
    if !(-32768..=-32000).contains(&wire.code) {
        return Err(MemoryError::invalid("invalid JSON-RPC error code"));
    }
    text(&wire.message, 1024, false)?;
    text(&wire.data.message, 1024, false)?;
    Ok(Reply::Error(wire.data))
}

// The seed enforces nesting before recursion and refuses duplicate keys before decoding typed data.
pub fn strict_json(bytes: &[u8]) -> Result<Value> {
    struct Seed(usize);
    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = Value;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> std::result::Result<Value, D::Error> {
            if self.0 > MAX_DEPTH {
                return Err(serde::de::Error::custom("JSON nesting limit"));
            }
            deserializer.deserialize_any(StrictVisitor(self.0))
        }
    }
    struct StrictVisitor(usize);
    impl<'de> Visitor<'de> for StrictVisitor {
        type Value = Value;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded duplicate-free JSON")
        }
        fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Value, E> {
            Ok(Value::Bool(value))
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Value, E> {
            Ok(value.into())
        }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Value, E> {
            Ok(value.into())
        }
        fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Value, E> {
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| E::custom("nonfinite JSON number"))
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Value, E> {
            if value.len() > 128 * 1024 {
                return Err(E::custom("JSON string limit"));
            }
            Ok(Value::String(value.to_owned()))
        }
        fn visit_string<E: serde::de::Error>(self, value: String) -> std::result::Result<Value, E> {
            self.visit_str(&value)
        }
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element_seed(Seed(self.0 + 1))? {
                if values.len() == MAX_COLLECTION {
                    return Err(serde::de::Error::custom("JSON array limit"));
                }
                values.push(value);
            }
            Ok(Value::Array(values))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
            let mut values = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if key.len() > 256 || values.len() == MAX_COLLECTION || values.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate or excessive JSON keys"));
                }
                values.insert(key, map.next_value_seed(Seed(self.0 + 1))?);
            }
            Ok(Value::Object(values))
        }
    }
    if bytes.len() > MAX_FRAME {
        return Err(MemoryError::invalid("JSON frame exceeds limit"));
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = Seed(0)
        .deserialize(&mut decoder)
        .map_err(|_| MemoryError::invalid("invalid or unbounded JSON"))?;
    decoder
        .end()
        .map_err(|_| MemoryError::invalid("trailing JSON data"))?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adoption_is_advertised_but_never_granted_in_normal_context() {
        let capability: Capability = serde_json::from_value(json!("legacy_adoption_v1")).unwrap();
        assert_eq!(capability, Capability::LegacyAdoptionV1);
        assert_eq!(
            serde_json::to_value(capability).unwrap(),
            json!("legacy_adoption_v1")
        );
        assert!(Method::from_name("legacy_adoption_v1").is_err());
        let root = std::env::temp_dir();
        let mut context: ProviderContext = serde_json::from_value(json!({
            "generation": 1, "user_id": "fixture", "workspace_id": "fixture",
            "workspace_root": root, "cwd": root, "device_id": "fixture",
            "device_aliases": [], "session_id": "fixture", "turn_id": "fixture",
            "boundary_id": "fixture", "data_root": root, "settings": {},
            "granted_capabilities": ["records"],
            "budgets": {"context_tokens": 300, "recall_tokens": 300, "max_records": 10}
        }))
        .unwrap();
        context.validate().unwrap();
        for (wire, capability) in [
            ("legacy_adoption_v1", Capability::LegacyAdoptionV1),
            ("legacy_adoption_v2", Capability::LegacyAdoptionV2),
        ] {
            assert_eq!(
                serde_json::from_value::<Capability>(json!(wire)).unwrap(),
                capability
            );
            assert_eq!(serde_json::to_value(capability).unwrap(), json!(wire));
            assert!(Method::from_name(wire).is_err());
            context.granted_capabilities = vec![Capability::Records, capability];
            assert_eq!(
                context.validate().unwrap_err().message,
                "offline adoption is not a context capability"
            );
        }
    }

    #[test]
    fn operation_names_round_trip_and_unknown_requests_are_refused() {
        for method in [
            Method::Capabilities,
            Method::Health,
            Method::SessionStart,
            Method::SessionEnd,
            Method::SessionReset,
            Method::Recall,
            Method::Store,
            Method::Get,
            Method::Update,
            Method::Forget,
            Method::Search,
            Method::List,
            Method::BuildContext,
            Method::ProcessTurn,
            Method::ConfirmUse,
            Method::OperationStatus,
            Method::Associate,
            Method::Affinity,
            Method::PrepareJob,
            Method::CompleteJob,
            Method::Command,
        ] {
            let name = method.name();
            assert_eq!(Method::from_name(name).unwrap().name(), name);
            assert_eq!(serde_json::to_value(method).unwrap(), json!(name));
            let frame = encode_request(1, name, &json!({})).unwrap();
            assert_eq!(strict_json(&frame).unwrap()["method"], name);
        }
        for name in ["initialize", "shutdown"] {
            assert!(encode_request(1, name, &json!({})).is_ok());
        }
        for name in ["", "unknown", "Recall", "prepareJob", "recall\n"] {
            assert!(Method::from_name(name).is_err());
            assert!(encode_request(1, name, &json!({})).is_err());
        }
        assert!(encode_request(0, "health", &json!({})).is_err());
    }

    #[test]
    fn strict_decoder_refuses_duplicate_depth_and_size() {
        for bytes in [
            br#"{"a":1,"a":2}"#.as_slice(),
            br#"{"a":[{"x":1,"x":2}]}"#,
            b"{}{}",
            b"\xff",
        ] {
            assert!(strict_json(bytes).is_err());
        }
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 2),
            "]".repeat(MAX_DEPTH + 2)
        );
        assert!(strict_json(nested.as_bytes()).is_err());
        assert!(strict_json(&vec![b' '; MAX_FRAME + 1]).is_err());
        assert_eq!(
            strict_json(br#"{"a":[true,null,1.5]}"#).unwrap(),
            json!({"a":[true,null,1.5]})
        );
    }

    #[test]
    fn completed_turn_schemas_are_bounded_and_explicitly_offline() {
        let mut turn = serde_json::json!({"operation_id":"turn-fixture","user_text":"fixture"});
        serde_json::from_value::<ProcessTurn>(turn.clone())
            .unwrap()
            .validate()
            .unwrap();
        turn["transcript"] = serde_json::json!("assistant: untrusted transcript");
        assert!(serde_json::from_value::<ProcessTurn>(turn.clone())
            .unwrap()
            .validate()
            .is_err());
        turn["payload_sha256"] = serde_json::json!("a".repeat(64));
        serde_json::from_value::<ProcessTurn>(turn.clone())
            .unwrap()
            .validate()
            .unwrap();
        for digest in ["A".repeat(64), "g".repeat(64), "a".repeat(63)] {
            turn["payload_sha256"] = serde_json::json!(digest);
            assert!(serde_json::from_value::<ProcessTurn>(turn.clone())
                .unwrap()
                .validate()
                .is_err());
        }
        let result = serde_json::json!({"accepted":true,"operation_id":"turn-fixture",
            "learning_mode":"offline","added":[],"reinforced":[],"review":[],
            "useful":[],"already_useful":[],"skipped_passive":true,"session_candidates":[]});
        serde_json::from_value::<ProcessTurnResult>(result.clone())
            .unwrap()
            .validate()
            .unwrap();
        for candidate in [
            serde_json::json!({"text":"fixture","anchor":null,"importance":0}),
            serde_json::json!({"text":"fixture","anchor":null,"importance":11}),
            serde_json::json!({"text":" ","anchor":null,"importance":5}),
            serde_json::json!({"text":"x".repeat(1601),"anchor":null,"importance":5}),
        ] {
            let mut invalid = result.clone();
            invalid["session_candidates"] = serde_json::json!([candidate]);
            assert!(serde_json::from_value::<ProcessTurnResult>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
        let mut oversized = result.clone();
        oversized["session_candidates"] = serde_json::json!((0..32)
            .map(|_| serde_json::json!({
                "text":"fixture","anchor":"a".repeat(4096),"importance":5
            }))
            .collect::<Vec<_>>());
        assert!(serde_json::from_value::<ProcessTurnResult>(oversized)
            .unwrap()
            .validate()
            .is_err());
        let mut invalid = result.clone();
        invalid["learning_mode"] = serde_json::json!("pretend_model_extraction");
        assert!(serde_json::from_value::<ProcessTurnResult>(invalid).is_err());
        let mut invalid = result;
        invalid["policy"] = serde_json::json!("untrusted");
        assert!(serde_json::from_value::<ProcessTurnResult>(invalid).is_err());
    }

    #[test]
    fn prepared_learning_schemas_reject_route_authority_and_invalid_bounds() {
        let request = json!({"operation_id":"turn-fixture-prepare", "turn":{
            "operation_id":"turn-fixture","user_text":"fixture","payload_sha256":"a".repeat(64)
        },"tool_calls":3,"recovered":false});
        serde_json::from_value::<PrepareJob>(request.clone())
            .unwrap()
            .validate()
            .unwrap();
        for (key, value) in [
            ("operation_id", json!("another-prepare")),
            ("tool_calls", json!(100_001)),
        ] {
            let mut invalid = request.clone();
            invalid[key] = value;
            assert!(serde_json::from_value::<PrepareJob>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
        for (key, value) in [
            ("dry_run", json!(true)),
            ("confirmed", json!(true)),
            ("payload_sha256", Value::Null),
        ] {
            let mut invalid = request.clone();
            invalid["turn"][key] = value;
            assert!(serde_json::from_value::<PrepareJob>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
        let result = json!({"operation_id":"turn-fixture-prepare","turn_operation_id":"turn-fixture",
            "payload_sha256":"a".repeat(64),"job":{"id":"job-fixture","system":"extract facts",
            "input":"fixture input","corpus_sha256":"b".repeat(64),"max_output_tokens":2048,"expires_unix_secs":1}});
        serde_json::from_value::<PrepareJobResult>(result.clone())
            .unwrap()
            .validate()
            .unwrap();
        for (key, value) in [
            ("system", json!("x".repeat(MAX_TEXT + 1))),
            ("input", json!("unsafe\u{001b}")),
            ("corpus_sha256", json!("B".repeat(64))),
            ("max_output_tokens", json!(0)),
            ("max_output_tokens", json!(2049)),
            ("expires_unix_secs", json!(0)),
        ] {
            let mut invalid = result.clone();
            invalid["job"][key] = value;
            assert!(
                serde_json::from_value::<PrepareJobResult>(invalid)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{key}"
            );
        }
        for key in ["api_key", "base_url", "model", "tools", "approval"] {
            let mut invalid = result.clone();
            invalid["job"][key] = json!("untrusted authority");
            assert!(
                serde_json::from_value::<PrepareJobResult>(invalid).is_err(),
                "{key}"
            );
        }
        let complete = json!({"operation_id":"turn-fixture","prepare_operation_id":"turn-fixture-prepare",
            "job_id":"job-fixture","payload_sha256":"a".repeat(64),"completion":"{\"facts\":[]}"});
        serde_json::from_value::<CompleteJob>(complete.clone())
            .unwrap()
            .validate()
            .unwrap();
        for (key, value) in [
            ("prepare_operation_id", json!("wrong-prepare")),
            ("completion", json!("x".repeat(MAX_TEXT + 1))),
        ] {
            let mut invalid = complete.clone();
            invalid[key] = value;
            assert!(serde_json::from_value::<CompleteJob>(invalid)
                .unwrap()
                .validate()
                .is_err());
        }
    }

    #[test]
    fn responses_are_correlated_and_exclusive() {
        for frame in [
            b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n".as_slice(),
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{},\"error\":{}}\n",
            b"[]\n",
            b"{}",
            b"{}\n{}\n",
        ] {
            assert!(decode_response::<Value>(frame, 1).is_err());
        }
        let good = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"items\":[]}}\n";
        decode_response::<ContextItems>(good, 1)
            .unwrap()
            .validate()
            .unwrap();
        let denied = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"unsupported\",\"data\":{\"code\":\"unsupported_capability\",\"message\":\"unsupported\"}}}\n";
        assert_eq!(
            decode_response::<Value>(denied, 1).unwrap_err().code,
            ErrorCode::UnsupportedCapability
        );
    }

    #[test]
    fn scores_and_records_are_validated_before_adoption() {
        let record = Record {
            id: "r1".into(),
            content: "fixture".into(),
            scope: Scope::Project,
            workspace_id: Some("fixture-project".into()),
            metadata: BTreeMap::new(),
            revision: 1,
            archived: false,
            confidence: Some(1.0),
            provenance: None,
        };
        record.validate().unwrap();
        let mut invalid = record.clone();
        invalid.confidence = Some(f64::NAN);
        assert!(invalid.validate().is_err());
        let duplicated = Hits {
            items: vec![
                Hit {
                    record: record.clone(),
                    score: 0.5,
                },
                Hit { record, score: 0.6 },
            ],
        };
        assert!(duplicated.validate().is_err());
    }
}
