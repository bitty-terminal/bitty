//! Core-owned metadata snapshot and AI request producer.
//!
//! This module is deliberately limited to a bounded, read-only metadata view
//! and request construction. It does not select providers, read credentials,
//! select models, or perform model I/O. The helper remains responsible for
//! method schemas, consent, and tolerant E2 correlator handling.
//!
//! `ServiceMetadata` is the initial Core metadata slice: service/version
//! identifiers and declared capabilities only. Provider/model names and
//! routing facts are deliberately deferred instead of being overloaded into
//! those strings. A future contract can add them through a typed extension
//! point without widening this credential-free boundary implicitly.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use bitty_ipc::frame::MAX_FRAME_BYTES;

use crate::ai_helper::{HelperError, ResidentHelper};

/// The initial version of a Core-owned metadata snapshot.
pub const SNAPSHOT_VERSION_INITIAL: u64 = 0;

/// Maximum metadata records retained by the producer.
pub const MAX_METADATA_ENTRIES: usize = 64;
/// Maximum UTF-8 bytes in a service identifier.
pub const MAX_METADATA_SERVICE_BYTES: usize = 128;
/// Maximum UTF-8 bytes in a service version.
pub const MAX_METADATA_VERSION_BYTES: usize = 64;
/// Maximum capabilities listed on one metadata record.
pub const MAX_METADATA_CAPABILITIES: usize = 16;
/// Maximum UTF-8 bytes in one capability name.
pub const MAX_METADATA_CAPABILITY_BYTES: usize = 64;
/// Maximum aggregate bytes in one metadata snapshot.
pub const MAX_METADATA_SNAPSHOT_BYTES: usize = 16 * 1024;

/// Maximum nesting accepted while validating request params.
pub const MAX_AI_PARAMS_DEPTH: usize = 32;
/// Maximum object members accepted in each params object, including nested
/// objects. Duplicate members are rejected at every object depth.
pub const MAX_AI_PARAMS_FIELDS: usize = 256;

/// All methods in the accepted helper-side `ai.*` surface.
pub const AI_METHODS: &[&str] = &[
    "ai.provider_list",
    "ai.model_select",
    "ai.model_fallback",
    "ai.model_cost",
    "ai.context_read",
    "ai.extension_manifest",
    "ai.extension_contribute",
    "ai.extension_activate",
    "ai.cache_hint",
    "ai.mcp_pin",
    "ai.mcp_snapshot",
    "ai.provider_complete",
    "ai.tool_dispatch",
];

/// One credential-free service metadata record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceMetadata {
    service: String,
    version: String,
    capabilities: Vec<String>,
}

impl ServiceMetadata {
    /// Create a bounded metadata record from identifier-shaped public data.
    ///
    /// Metadata is deliberately limited to bounded ASCII identifiers. The
    /// identifier grammar admits names such as `token-counting`; only
    /// unambiguous credential-shaped syntax is rejected. This shape is a
    /// boundary policy for data that the caller declares as metadata; it does
    /// not inspect credentials or perform filesystem, network, or provider
    /// I/O.
    pub fn try_new(
        service: &str,
        version: &str,
        capabilities: &[String],
    ) -> Result<Self, MetadataError> {
        validate_text(service, MAX_METADATA_SERVICE_BYTES, "service")?;
        validate_text(version, MAX_METADATA_VERSION_BYTES, "version")?;
        if capabilities.len() > MAX_METADATA_CAPABILITIES {
            return Err(MetadataError::TooManyCapabilities {
                actual: capabilities.len(),
                limit: MAX_METADATA_CAPABILITIES,
            });
        }
        let mut copied = Vec::with_capacity(capabilities.len());
        for capability in capabilities {
            validate_text(capability, MAX_METADATA_CAPABILITY_BYTES, "capability")?;
            copied.push(capability.clone());
        }
        let record = Self {
            service: service.to_owned(),
            version: version.to_owned(),
            capabilities: copied,
        };
        if record.encoded_size() > MAX_METADATA_SNAPSHOT_BYTES {
            return Err(MetadataError::SnapshotTooLarge {
                actual: record.encoded_size(),
                limit: MAX_METADATA_SNAPSHOT_BYTES,
            });
        }
        Ok(record)
    }

    /// Service identifier, without provider or credential semantics.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Declared metadata version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Credential-free capability labels.
    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    fn encoded_size(&self) -> usize {
        self.service
            .len()
            .saturating_add(self.version.len())
            .saturating_add(self.capabilities.iter().map(String::len).sum())
    }
}

fn validate_text(value: &str, limit: usize, field: &'static str) -> Result<(), MetadataError> {
    if value.is_empty() {
        return Err(MetadataError::EmptyField { field });
    }
    if value.len() > limit {
        return Err(MetadataError::FieldTooLarge {
            field,
            actual: value.len(),
            limit,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(MetadataError::ControlCharacter { field });
    }
    if looks_like_credential(value) {
        return Err(MetadataError::CredentialLikeValue { field });
    }
    let allow_slash = field == "service";
    if !is_identifier_shape(value, allow_slash) {
        return Err(MetadataError::InvalidShape { field });
    }
    Ok(())
}

fn is_identifier_shape(value: &str, allow_slash: bool) -> bool {
    let bytes = value.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    let Some(&last) = bytes.last() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    rest.iter().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(*byte, b'.' | b'-' | b'_')
            || (allow_slash && *byte == b'/')
    })
}

fn looks_like_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let has_userinfo = lower.find('@').is_some_and(|at| {
        lower
            .get(..at)
            .is_some_and(|userinfo| userinfo.contains(':'))
    });
    let has_url_userinfo = lower.find("://").is_some_and(|scheme_end| {
        lower
            .get(scheme_end.saturating_add(3)..)
            .and_then(|rest| rest.find('/').map(|slash| &rest[..slash]))
            .is_some_and(|authority| authority.contains('@'))
    });
    let bearer_or_basic = lower.starts_with("bearer ") || lower.starts_with("basic ");
    let assignment = [
        "api_key=",
        "api-key=",
        "apikey=",
        "access_token=",
        "refresh_token=",
        "authorization=",
        "client_secret=",
        "password=",
        "secret=",
        "token=",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix));
    let secret_key = ["sk-", "pk-", "rk-", "ak-"].iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|suffix| {
            ["live-", "test-", "prod-", "project-", "service-"]
                .iter()
                .any(|marker| suffix.starts_with(marker))
                || (suffix.len() >= 32
                    && suffix.bytes().any(|byte| byte.is_ascii_alphabetic())
                    && suffix.bytes().any(|byte| byte.is_ascii_digit()))
        })
    });
    let known_key_prefix = ["ghp_", "github_pat_", "xoxb-", "xoxp-"]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    let jwt = lower.starts_with("eyj") && lower.matches('.').count() >= 2;
    has_userinfo
        || has_url_userinfo
        || bearer_or_basic
        || assignment
        || secret_key
        || known_key_prefix
        || jwt
}

/// Errors returned while admitting metadata into the bounded producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    /// A required text field was empty.
    EmptyField { field: &'static str },
    /// A text field exceeded its bound.
    FieldTooLarge {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    /// A text field contains a control character.
    ControlCharacter { field: &'static str },
    /// A text field resembles a credential or credential transport value.
    CredentialLikeValue { field: &'static str },
    /// A text field is not an accepted ASCII identifier shape.
    InvalidShape { field: &'static str },
    /// A record listed too many capabilities.
    TooManyCapabilities { actual: usize, limit: usize },
    /// The metadata snapshot would exceed its aggregate bound.
    SnapshotTooLarge { actual: usize, limit: usize },
    /// The snapshot contains two records for one service.
    DuplicateService,
    /// The snapshot would exceed its record bound.
    TooManyEntries { actual: usize, limit: usize },
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(f, "metadata {field} is empty"),
            Self::FieldTooLarge {
                field,
                actual,
                limit,
            } => write!(f, "metadata {field} is {actual} bytes, limit is {limit}"),
            Self::ControlCharacter { field } => {
                write!(f, "metadata {field} contains a control character")
            }
            Self::CredentialLikeValue { field } => {
                write!(f, "metadata {field} resembles a credential value")
            }
            Self::InvalidShape { field } => {
                write!(f, "metadata {field} is not an accepted identifier")
            }
            Self::TooManyCapabilities { actual, limit } => {
                write!(f, "metadata has {actual} capabilities, limit is {limit}")
            }
            Self::SnapshotTooLarge { actual, limit } => {
                write!(f, "metadata snapshot is {actual} bytes, limit is {limit}")
            }
            Self::DuplicateService => f.write_str("metadata contains a duplicate service"),
            Self::TooManyEntries { actual, limit } => {
                write!(f, "metadata has {actual} entries, limit is {limit}")
            }
        }
    }
}

impl std::error::Error for MetadataError {}

/// Immutable bounded metadata snapshot copied from [`AiMetadataProducer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiMetadataSnapshot {
    version: u64,
    entries: Vec<ServiceMetadata>,
}

impl AiMetadataSnapshot {
    /// Construct an empty snapshot at the initial version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: SNAPSHOT_VERSION_INITIAL,
            entries: Vec::new(),
        }
    }

    /// Current monotonic snapshot version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Metadata records in deterministic insertion order.
    #[must_use]
    pub fn entries(&self) -> &[ServiceMetadata] {
        &self.entries
    }
}

impl Default for AiMetadataSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed extension point for metadata that is intentionally deferred from the
/// initial service slice. No variant currently carries provider, model, or
/// routing data; adding one requires a separately bounded, credential-free
/// contract instead of overloading [`ServiceMetadata`] strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataExtension {
    /// Provider/model names and routing facts are not published by this slice.
    DeferredRoutingFacts,
}

/// State of the Core-to-integration metadata publication handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSyncState {
    /// No current snapshot has been offered to the integration boundary.
    Unpublished,
    /// The current snapshot was offered but not acknowledged.
    Published { version: u64 },
    /// The current snapshot may be used for requests.
    Acknowledged { version: u64 },
    /// A mutation happened after the last acknowledgement.
    Stale {
        acknowledged_version: u64,
        current_version: u64,
    },
    /// A publication was superseded before it could be acknowledged.
    StalePublication {
        published_version: u64,
        current_version: u64,
    },
}

/// Immutable, typed publication of one metadata snapshot.
#[derive(Debug, Clone)]
pub struct AiMetadataPublication {
    snapshot: AiMetadataSnapshot,
    mutation_epoch: Arc<u8>,
    identity: Arc<u8>,
}

impl AiMetadataPublication {
    /// Version carried by this publication.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.snapshot.version()
    }

    /// Immutable metadata copied into this publication.
    #[must_use]
    pub fn snapshot(&self) -> &AiMetadataSnapshot {
        &self.snapshot
    }
}

/// Error returned when a publication acknowledgement is no longer current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataSyncError {
    /// The supplied publication does not match the current Core snapshot.
    StalePublication {
        publication_version: u64,
        current_version: u64,
    },
}

impl fmt::Display for MetadataSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StalePublication {
                publication_version,
                current_version,
            } => write!(
                f,
                "metadata publication version {publication_version} is stale; current version is {current_version}"
            ),
        }
    }
}

impl std::error::Error for MetadataSyncError {}

/// Core-owned bounded metadata/version producer for S-1.
///
/// Successful record mutations bump the version once. A no-op replacement or
/// removal does not bump it. `refresh` always creates a new observation
/// boundary, even when the records are byte-for-byte unchanged; `invalidate`
/// always bumps and clears the records. Every bump saturates at `u64::MAX`.
///
/// This is intentionally a single mutable owner. Use [`Self::snapshot`] or
/// [`Self::publish`] when an immutable copy is needed; cloning the producer
/// would create two independent version authorities and is not supported.
///
/// ```compile_fail
/// use bitty_runtime::AiMetadataProducer;
/// let _copy = AiMetadataProducer::new().clone();
/// ```
#[derive(Debug, Default)]
pub struct AiMetadataProducer {
    snapshot: AiMetadataSnapshot,
    mutation_epoch: Arc<u8>,
    current_publication: Option<AiMetadataPublication>,
    acknowledged_identity: Option<Arc<u8>>,
}

impl AiMetadataProducer {
    /// Create an empty producer at version zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: AiMetadataSnapshot::new(),
            mutation_epoch: Arc::new(0),
            current_publication: None,
            acknowledged_identity: None,
        }
    }

    /// Return the current version without exposing mutable state.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.snapshot.version
    }

    /// Return a bounded immutable copy of the current metadata.
    #[must_use]
    pub fn snapshot(&self) -> AiMetadataSnapshot {
        self.snapshot.clone()
    }

    /// Return the current publication/acknowledgement state.
    #[must_use]
    pub fn sync_state(&self) -> SnapshotSyncState {
        let Some(publication) = self.current_publication.as_ref() else {
            return SnapshotSyncState::Unpublished;
        };
        let version = publication.version();
        if !Arc::ptr_eq(&publication.mutation_epoch, &self.mutation_epoch) {
            return match self.acknowledged_identity.as_ref() {
                Some(_) => SnapshotSyncState::Stale {
                    acknowledged_version: version,
                    current_version: self.version(),
                },
                None => SnapshotSyncState::StalePublication {
                    published_version: version,
                    current_version: self.version(),
                },
            };
        }
        if self
            .acknowledged_identity
            .as_ref()
            .is_some_and(|identity| Arc::ptr_eq(identity, &publication.identity))
        {
            SnapshotSyncState::Acknowledged { version }
        } else {
            SnapshotSyncState::Published { version }
        }
    }

    /// Publish the current immutable snapshot to the integration boundary.
    ///
    /// Core does not have the application event loop, Lua, or IPC endpoint
    /// that consumes this publication. The eventual caller must pass the
    /// returned value across that boundary. The current local test/integration
    /// seam is [`Self::acknowledge_local_publication`]; it does not prove that
    /// a helper accepted the publication. A future helper adapter must own the
    /// acceptance proof before this seam is replaced by an endpoint receipt.
    pub fn publish(&mut self) -> AiMetadataPublication {
        let publication = AiMetadataPublication {
            snapshot: self.snapshot(),
            mutation_epoch: Arc::clone(&self.mutation_epoch),
            identity: Arc::new(0),
        };
        self.current_publication = Some(publication.clone());
        self.acknowledged_identity = None;
        publication
    }

    /// Mark an exact publication as accepted by a local test or integration
    /// seam. This is deliberately named to avoid implying helper acceptance;
    /// no application/helper endpoint exists in this crate yet.
    pub fn acknowledge_local_publication(
        &mut self,
        publication: &AiMetadataPublication,
    ) -> Result<(), MetadataSyncError> {
        let Some(current) = self.current_publication.as_ref() else {
            return Err(MetadataSyncError::StalePublication {
                publication_version: publication.version(),
                current_version: self.version(),
            });
        };
        if !Arc::ptr_eq(&publication.identity, &current.identity)
            || !Arc::ptr_eq(&publication.mutation_epoch, &self.mutation_epoch)
        {
            return Err(MetadataSyncError::StalePublication {
                publication_version: publication.version(),
                current_version: self.version(),
            });
        }
        self.acknowledged_identity = Some(Arc::clone(&publication.identity));
        Ok(())
    }

    /// Insert or replace one record. Returns whether state changed.
    pub fn upsert(&mut self, record: ServiceMetadata) -> Result<bool, MetadataError> {
        if let Some(index) = self
            .snapshot
            .entries
            .iter()
            .position(|entry| entry.service == record.service)
        {
            if self.snapshot.entries[index] == record {
                return Ok(false);
            }
            let next_size = self
                .snapshot
                .entries
                .iter()
                .enumerate()
                .filter(|(entry_index, _)| *entry_index != index)
                .map(|(_, entry)| entry.encoded_size())
                .sum::<usize>()
                .saturating_add(record.encoded_size());
            if next_size > MAX_METADATA_SNAPSHOT_BYTES {
                return Err(MetadataError::SnapshotTooLarge {
                    actual: next_size,
                    limit: MAX_METADATA_SNAPSHOT_BYTES,
                });
            }
            self.snapshot.entries[index] = record;
            self.bump();
            return Ok(true);
        }
        if self.snapshot.entries.len() >= MAX_METADATA_ENTRIES {
            return Err(MetadataError::TooManyEntries {
                actual: self.snapshot.entries.len().saturating_add(1),
                limit: MAX_METADATA_ENTRIES,
            });
        }
        let next_size = self
            .snapshot
            .entries
            .iter()
            .map(ServiceMetadata::encoded_size)
            .sum::<usize>()
            .saturating_add(record.encoded_size());
        if next_size > MAX_METADATA_SNAPSHOT_BYTES {
            return Err(MetadataError::SnapshotTooLarge {
                actual: next_size,
                limit: MAX_METADATA_SNAPSHOT_BYTES,
            });
        }
        self.snapshot.entries.push(record);
        self.bump();
        Ok(true)
    }

    /// Remove a record by service identifier. Returns whether state changed.
    pub fn remove(&mut self, service: &str) -> bool {
        let Some(index) = self
            .snapshot
            .entries
            .iter()
            .position(|entry| entry.service == service)
        else {
            return false;
        };
        self.snapshot.entries.remove(index);
        self.bump();
        true
    }

    /// Replace the complete bounded snapshot and create a fresh observation.
    pub fn refresh(&mut self, entries: &[ServiceMetadata]) -> Result<(), MetadataError> {
        validate_entries(entries)?;
        self.snapshot.entries = entries.to_vec();
        self.bump();
        Ok(())
    }

    /// Invalidate all metadata and advance the version, even if already empty.
    pub fn invalidate(&mut self) {
        self.snapshot.entries.clear();
        self.bump();
    }

    fn bump(&mut self) {
        self.snapshot.version = self.snapshot.version.saturating_add(1);
        self.mutation_epoch = Arc::new(0);
    }
}

fn validate_entries(entries: &[ServiceMetadata]) -> Result<(), MetadataError> {
    if entries.len() > MAX_METADATA_ENTRIES {
        return Err(MetadataError::TooManyEntries {
            actual: entries.len(),
            limit: MAX_METADATA_ENTRIES,
        });
    }
    let mut total = 0usize;
    for (index, entry) in entries.iter().enumerate() {
        total = total.saturating_add(entry.encoded_size());
        if total > MAX_METADATA_SNAPSHOT_BYTES {
            return Err(MetadataError::SnapshotTooLarge {
                actual: total,
                limit: MAX_METADATA_SNAPSHOT_BYTES,
            });
        }
        if entries[..index]
            .iter()
            .any(|previous| previous.service == entry.service)
        {
            return Err(MetadataError::DuplicateService);
        }
    }
    Ok(())
}

/// Errors returned by the shared Core AI request producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiRequestError {
    /// Metadata was not published and acknowledged for the current version.
    MetadataNotSynchronized(SnapshotSyncState),
    /// The method is not part of the fixed helper-side surface.
    UnsupportedMethod,
    /// Params were not a bounded JSON object.
    MalformedParams,
    /// Params contain a duplicate top-level field.
    DuplicateField,
    /// Params already contain the Core-owned correlator field.
    SnapshotVersionAlreadyPresent,
    /// The caller supplied a payload above the framing limit.
    PayloadTooLarge { actual: usize, limit: usize },
    /// The helper rejected the framed request.
    Helper(HelperError),
}

impl fmt::Display for AiRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MetadataNotSynchronized(state) => {
                write!(f, "AI metadata is not synchronized: {state:?}")
            }
            Self::UnsupportedMethod => f.write_str("unsupported ai method"),
            Self::MalformedParams => f.write_str("ai params must be a bounded JSON object"),
            Self::DuplicateField => f.write_str("ai params contain a duplicate field"),
            Self::SnapshotVersionAlreadyPresent => {
                f.write_str("ai params already contain snapshot_version")
            }
            Self::PayloadTooLarge { actual, limit } => {
                write!(f, "ai request is {actual} bytes, limit is {limit}")
            }
            Self::Helper(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for AiRequestError {}

impl From<HelperError> for AiRequestError {
    fn from(error: HelperError) -> Self {
        Self::Helper(error)
    }
}

/// Shared Core producer for every outbound `ai.*` request.
///
/// This owner is deliberately not cloneable. The application integration
/// boundary must hold one mutable producer and explicitly acknowledge each
/// publication; immutable snapshots/publications remain cloneable.
///
/// ```compile_fail
/// use bitty_runtime::AiRequestProducer;
/// let _copy = AiRequestProducer::new().clone();
/// ```
#[derive(Debug, Default)]
pub struct AiRequestProducer {
    metadata: AiMetadataProducer,
}

impl AiRequestProducer {
    /// Create a producer with an empty version-zero snapshot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metadata: AiMetadataProducer::new(),
        }
    }

    /// Access the bounded metadata/version owner.
    #[must_use]
    pub fn metadata(&self) -> &AiMetadataProducer {
        &self.metadata
    }

    /// Mutably access the metadata owner. Request versions are read at build
    /// time, so mutations take effect on the next request.
    #[must_use]
    pub fn metadata_mut(&mut self) -> &mut AiMetadataProducer {
        &mut self.metadata
    }

    /// Publish the current Core metadata snapshot for integration handoff.
    pub fn publish_metadata(&mut self) -> AiMetadataPublication {
        self.metadata.publish()
    }

    /// Mark the exact publication accepted by the local test/integration seam.
    /// This does not claim that a helper endpoint accepted it; that adapter is
    /// intentionally not implemented in this crate.
    pub fn acknowledge_local_metadata(
        &mut self,
        publication: &AiMetadataPublication,
    ) -> Result<(), MetadataSyncError> {
        self.metadata.acknowledge_local_publication(publication)
    }

    /// Return the synchronization state used by request admission.
    #[must_use]
    pub fn sync_state(&self) -> SnapshotSyncState {
        self.metadata.sync_state()
    }

    /// Build a structured IPC request with exactly one current version field.
    pub fn build(
        &self,
        request_id: u64,
        method: &str,
        params: &[u8],
    ) -> Result<Vec<u8>, AiRequestError> {
        let state = self.metadata.sync_state();
        if !matches!(state, SnapshotSyncState::Acknowledged { .. }) {
            return Err(AiRequestError::MetadataNotSynchronized(state));
        }
        build_request(request_id, method, params, self.metadata.version())
    }

    /// Build and submit one AI request through [`ResidentHelper::submit_frame`].
    pub fn submit(
        &self,
        helper: &mut ResidentHelper,
        request_id: u64,
        method: &str,
        params: &[u8],
    ) -> Result<(), AiRequestError> {
        let request = self.build(request_id, method, params)?;
        helper.submit_frame(&request).map_err(AiRequestError::from)
    }
}

fn build_request(
    request_id: u64,
    method: &str,
    params: &[u8],
    snapshot_version: u64,
) -> Result<Vec<u8>, AiRequestError> {
    if !AI_METHODS.contains(&method) {
        return Err(AiRequestError::UnsupportedMethod);
    }
    if params.len() > MAX_FRAME_BYTES {
        return Err(AiRequestError::PayloadTooLarge {
            actual: params.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let params = trim_json_whitespace(params);
    let params = if params.is_empty() { b"{}" } else { params };
    if params.len() > MAX_FRAME_BYTES {
        return Err(AiRequestError::PayloadTooLarge {
            actual: params.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    if std::str::from_utf8(params).is_err() {
        return Err(AiRequestError::MalformedParams);
    }
    let object = JsonObject::parse(params).map_err(|error| match error {
        JsonError::Duplicate => AiRequestError::DuplicateField,
        JsonError::SnapshotVersion => AiRequestError::SnapshotVersionAlreadyPresent,
        JsonError::Malformed => AiRequestError::MalformedParams,
    })?;
    let inner = &params[object.open + 1..object.close];
    let inner = trim_json_whitespace(inner);
    let mut output =
        Vec::with_capacity(params.len().saturating_add(method.len()).saturating_add(64));
    output.extend_from_slice(br#"{"v":1,"id":"#);
    output.extend_from_slice(request_id.to_string().as_bytes());
    output.extend_from_slice(b",\"method\":\"");
    output.extend_from_slice(method.as_bytes());
    output.extend_from_slice(b"\",\"params\":{");
    output.extend_from_slice(inner);
    if !inner.is_empty() {
        output.push(b',');
    }
    output.extend_from_slice(b"\"snapshot_version\":");
    output.extend_from_slice(snapshot_version.to_string().as_bytes());
    output.extend_from_slice(b"}}");
    if output.len() > MAX_FRAME_BYTES {
        return Err(AiRequestError::PayloadTooLarge {
            actual: output.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    Ok(output)
}

fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn trim_json_whitespace(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|byte| !is_json_whitespace(*byte))
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|byte| !is_json_whitespace(*byte))
        .map_or(start, |index| index.saturating_add(1));
    &input[start..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonError {
    Malformed,
    Duplicate,
    SnapshotVersion,
}

struct JsonObject {
    open: usize,
    close: usize,
}

impl JsonObject {
    fn parse(input: &[u8]) -> Result<Self, JsonError> {
        let mut parser = JsonParser {
            input,
            position: 0,
            depth: 0,
        };
        parser.skip_whitespace();
        if parser.input.get(parser.position) != Some(&b'{') {
            return Err(JsonError::Malformed);
        }
        let open = parser.position;
        parser.parse_object(true)?;
        parser.skip_whitespace();
        if parser.position != parser.input.len() {
            return Err(JsonError::Malformed);
        }
        Ok(Self {
            open,
            close: parser.position.saturating_sub(1),
        })
    }
}

struct JsonParser<'a> {
    input: &'a [u8],
    position: usize,
    depth: usize,
}

impl JsonParser<'_> {
    fn parse_value(&mut self) -> Result<(), JsonError> {
        self.skip_whitespace();
        self.depth = self.depth.saturating_add(1);
        if self.depth > MAX_AI_PARAMS_DEPTH {
            return Err(JsonError::Malformed);
        }
        let result = match self.input.get(self.position).copied() {
            Some(b'{') => self.parse_object(false),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string(),
            Some(b't') => self.parse_literal(b"true"),
            Some(b'f') => self.parse_literal(b"false"),
            Some(b'n') => self.parse_literal(b"null"),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.parse_number(),
            _ => Err(JsonError::Malformed),
        };
        self.depth = self.depth.saturating_sub(1);
        result
    }

    fn parse_object(&mut self, root: bool) -> Result<(), JsonError> {
        self.position = self.position.saturating_add(1);
        let mut fields: Option<HashSet<String>> = None;
        self.skip_whitespace();
        if self.input.get(self.position) == Some(&b'}') {
            self.position = self.position.saturating_add(1);
            return Ok(());
        }
        loop {
            self.skip_whitespace();
            if fields
                .as_ref()
                .is_some_and(|fields| fields.len() >= MAX_AI_PARAMS_FIELDS)
            {
                return Err(JsonError::Malformed);
            }
            let key = self.parse_key()?;
            let fields = fields.get_or_insert_with(|| HashSet::with_capacity(1));
            let is_snapshot_version = root && key == SNAPSHOT_VERSION_KEY;
            if !fields.insert(key) {
                return if is_snapshot_version {
                    Err(JsonError::SnapshotVersion)
                } else {
                    Err(JsonError::Duplicate)
                };
            }
            if is_snapshot_version {
                return Err(JsonError::SnapshotVersion);
            }
            self.skip_whitespace();
            if self.input.get(self.position) != Some(&b':') {
                return Err(JsonError::Malformed);
            }
            self.position = self.position.saturating_add(1);
            self.parse_value()?;
            self.skip_whitespace();
            match self.input.get(self.position) {
                Some(b',') => self.position = self.position.saturating_add(1),
                Some(b'}') => {
                    self.position = self.position.saturating_add(1);
                    return Ok(());
                }
                _ => return Err(JsonError::Malformed),
            }
        }
    }

    fn parse_array(&mut self) -> Result<(), JsonError> {
        self.position = self.position.saturating_add(1);
        self.skip_whitespace();
        if self.input.get(self.position) == Some(&b']') {
            self.position = self.position.saturating_add(1);
            return Ok(());
        }
        loop {
            self.parse_value()?;
            self.skip_whitespace();
            match self.input.get(self.position) {
                Some(b',') => self.position = self.position.saturating_add(1),
                Some(b']') => {
                    self.position = self.position.saturating_add(1);
                    return Ok(());
                }
                _ => return Err(JsonError::Malformed),
            }
        }
    }

    fn parse_string(&mut self) -> Result<(), JsonError> {
        if self.input.get(self.position) != Some(&b'"') {
            return Err(JsonError::Malformed);
        }
        self.position = self.position.saturating_add(1);
        loop {
            let byte = self
                .input
                .get(self.position)
                .copied()
                .ok_or(JsonError::Malformed)?;
            self.position = self.position.saturating_add(1);
            match byte {
                b'"' => return Ok(()),
                b'\\' => {
                    let escape = self
                        .input
                        .get(self.position)
                        .copied()
                        .ok_or(JsonError::Malformed)?;
                    self.position = self.position.saturating_add(1);
                    if escape == b'u' {
                        self.parse_unicode_escape()?;
                    } else if !matches!(
                        escape,
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        return Err(JsonError::Malformed);
                    }
                }
                byte if byte < 0x20 => return Err(JsonError::Malformed),
                _ => {}
            }
        }
    }

    fn parse_key(&mut self) -> Result<String, JsonError> {
        if self.input.get(self.position) != Some(&b'"') {
            return Err(JsonError::Malformed);
        }
        self.position = self.position.saturating_add(1);
        let mut key = String::new();
        loop {
            let byte = self
                .input
                .get(self.position)
                .copied()
                .ok_or(JsonError::Malformed)?;
            self.position = self.position.saturating_add(1);
            match byte {
                b'"' => return Ok(key),
                b'\\' => {
                    let escape = self
                        .input
                        .get(self.position)
                        .copied()
                        .ok_or(JsonError::Malformed)?;
                    self.position = self.position.saturating_add(1);
                    let character = if escape == b'u' {
                        self.parse_unicode_escape()?
                    } else {
                        match escape {
                            b'"' => '"',
                            b'\\' => '\\',
                            b'/' => '/',
                            b'b' => '\u{08}',
                            b'f' => '\u{0c}',
                            b'n' => '\n',
                            b'r' => '\r',
                            b't' => '\t',
                            _ => return Err(JsonError::Malformed),
                        }
                    };
                    key.push(character);
                }
                byte if byte < 0x20 => return Err(JsonError::Malformed),
                byte => {
                    let start = self.position.saturating_sub(1);
                    let remaining = self.input.get(start..).ok_or(JsonError::Malformed)?;
                    let scalar_width = match byte {
                        0x00..=0x7f => 1,
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(JsonError::Malformed),
                    };
                    let text = std::str::from_utf8(
                        remaining.get(..scalar_width).ok_or(JsonError::Malformed)?,
                    )
                    .map_err(|_| JsonError::Malformed)?;
                    let character = text.chars().next().ok_or(JsonError::Malformed)?;
                    self.position = start.saturating_add(character.len_utf8());
                    key.push(character);
                }
            }
        }
    }

    fn parse_literal(&mut self, literal: &[u8]) -> Result<(), JsonError> {
        let end = self.position.saturating_add(literal.len());
        if self.input.get(self.position..end) != Some(literal) {
            return Err(JsonError::Malformed);
        }
        self.position = end;
        Ok(())
    }

    fn parse_number(&mut self) -> Result<(), JsonError> {
        let start = self.position;
        if self.input.get(self.position) == Some(&b'-') {
            self.position = self.position.saturating_add(1);
        }
        match self.input.get(self.position) {
            Some(b'0') => self.position = self.position.saturating_add(1),
            Some(byte) if byte.is_ascii_digit() => {
                while self
                    .input
                    .get(self.position)
                    .is_some_and(u8::is_ascii_digit)
                {
                    self.position = self.position.saturating_add(1);
                }
            }
            _ => return Err(JsonError::Malformed),
        }
        if self.input.get(self.position) == Some(&b'.') {
            self.position = self.position.saturating_add(1);
            let before = self.position;
            while self
                .input
                .get(self.position)
                .is_some_and(u8::is_ascii_digit)
            {
                self.position = self.position.saturating_add(1);
            }
            if before == self.position {
                return Err(JsonError::Malformed);
            }
        }
        if self
            .input
            .get(self.position)
            .is_some_and(|byte| *byte == b'e' || *byte == b'E')
        {
            self.position = self.position.saturating_add(1);
            if self
                .input
                .get(self.position)
                .is_some_and(|byte| *byte == b'+' || *byte == b'-')
            {
                self.position = self.position.saturating_add(1);
            }
            let before = self.position;
            while self
                .input
                .get(self.position)
                .is_some_and(u8::is_ascii_digit)
            {
                self.position = self.position.saturating_add(1);
            }
            if before == self.position {
                return Err(JsonError::Malformed);
            }
        }
        if start == self.position {
            Err(JsonError::Malformed)
        } else {
            Ok(())
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .input
            .get(self.position)
            .is_some_and(|byte| is_json_whitespace(*byte))
        {
            self.position = self.position.saturating_add(1);
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, JsonError> {
        let code_unit = read_unicode_escape(self.input, &mut self.position)?;
        if (0xD800..=0xDBFF).contains(&code_unit) {
            if self
                .input
                .get(self.position..self.position.saturating_add(2))
                != Some(br"\u")
            {
                return Err(JsonError::Malformed);
            }
            self.position = self.position.saturating_add(2);
            let low = read_unicode_escape(self.input, &mut self.position)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(JsonError::Malformed);
            }
            let scalar = 0x1_0000 + (u32::from(code_unit - 0xD800) << 10) + u32::from(low - 0xDC00);
            return char::from_u32(scalar).ok_or(JsonError::Malformed);
        }
        if (0xDC00..=0xDFFF).contains(&code_unit) {
            return Err(JsonError::Malformed);
        }
        char::from_u32(u32::from(code_unit)).ok_or(JsonError::Malformed)
    }
}

const SNAPSHOT_VERSION_KEY: &str = "snapshot_version";

fn read_unicode_escape(input: &[u8], position: &mut usize) -> Result<u16, JsonError> {
    let end = position.saturating_add(4);
    let digits = input.get(*position..end).ok_or(JsonError::Malformed)?;
    let mut value = 0u16;
    for digit in digits {
        let digit = u16::try_from(hex_value(*digit).ok_or(JsonError::Malformed)?)
            .map_err(|_| JsonError::Malformed)?;
        value = value
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit))
            .ok_or(JsonError::Malformed)?;
    }
    *position = end;
    Ok(value)
}

fn hex_value(digit: u8) -> Option<u32> {
    match digit {
        b'0'..=b'9' => Some(u32::from(digit - b'0')),
        b'a'..=b'f' => Some(u32::from(digit - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(digit - b'A' + 10)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, version: &str) -> ServiceMetadata {
        ServiceMetadata::try_new(name, version, &[]).expect("bounded metadata")
    }

    fn synchronized_producer() -> AiRequestProducer {
        let mut producer = AiRequestProducer::new();
        let publication = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&publication)
            .expect("current publication");
        producer
    }

    #[test]
    fn metadata_initializes_and_bumps_only_on_mutation() {
        let mut producer = AiMetadataProducer::new();
        assert_eq!(producer.version(), SNAPSHOT_VERSION_INITIAL);
        assert!(producer.upsert(record("svc", "1")).expect("upsert"));
        assert_eq!(producer.version(), 1);
        assert!(!producer.upsert(record("svc", "1")).expect("no-op"));
        assert_eq!(producer.version(), 1);
        assert!(producer.upsert(record("svc", "2")).expect("replace"));
        assert_eq!(producer.version(), 2);
        assert!(producer.remove("svc"));
        assert_eq!(producer.version(), 3);
        assert!(!producer.remove("missing"));
        assert_eq!(producer.version(), 3);
    }

    #[test]
    fn refresh_and_invalidate_create_new_observation_boundaries() {
        let mut producer = AiMetadataProducer::new();
        let entries = [record("svc", "1")];
        producer.refresh(&entries).expect("refresh");
        assert_eq!(producer.version(), 1);
        producer
            .refresh(&entries)
            .expect("same refresh still advances");
        assert_eq!(producer.version(), 2);
        producer.invalidate();
        assert_eq!(producer.version(), 3);
        assert!(producer.snapshot().entries().is_empty());
    }

    #[test]
    fn metadata_limits_and_duplicate_refresh_are_fail_closed() {
        let mut producer = AiMetadataProducer::new();
        let long = "x".repeat(MAX_METADATA_SERVICE_BYTES + 1);
        assert!(matches!(
            ServiceMetadata::try_new(&long, "1", &[]),
            Err(MetadataError::FieldTooLarge { .. })
        ));
        let entries = [record("svc", "1"), record("svc", "2")];
        assert_eq!(
            producer.refresh(&entries),
            Err(MetadataError::DuplicateService)
        );
        assert_eq!(producer.version(), SNAPSHOT_VERSION_INITIAL);
    }

    #[test]
    fn every_ai_method_gets_one_current_snapshot_version() {
        let producer = synchronized_producer();
        for (id, method) in AI_METHODS.iter().enumerate() {
            let request = producer
                .build(id as u64, method, br#"{"unknown":true}"#)
                .expect("supported request");
            let text = std::str::from_utf8(&request).expect("utf8 request");
            assert!(text.contains("\"snapshot_version\":0"), "{text}");
            assert_eq!(text.matches("snapshot_version").count(), 1, "{text}");
            assert!(
                text.contains("\"unknown\":true"),
                "unknown fields stay helper-owned"
            );
        }
    }

    #[test]
    fn malformed_duplicate_and_unknown_method_inputs_fail_closed() {
        let producer = synchronized_producer();
        assert_eq!(
            producer.build(1, "ai.unknown", b"{}"),
            Err(AiRequestError::UnsupportedMethod)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"#),
            Err(AiRequestError::MalformedParams)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"x":1,"x":2}"#),
            Err(AiRequestError::DuplicateField)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"x":1,"\u0078":2}"#),
            Err(AiRequestError::DuplicateField)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"snapshot_version":3}"#),
            Err(AiRequestError::SnapshotVersionAlreadyPresent)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"snapshot\u005fversion":3}"#),
            Err(AiRequestError::SnapshotVersionAlreadyPresent)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", &[b'{', 0xff, b'}']),
            Err(AiRequestError::MalformedParams)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", b"\x0b{\"x\":1}"),
            Err(AiRequestError::MalformedParams)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", b"{\"x\":1}\x0c"),
            Err(AiRequestError::MalformedParams)
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"outer":{"name":1,"name":2}}"#,),
            Err(AiRequestError::DuplicateField)
        );
        assert_eq!(
            producer.build(
                1,
                "ai.provider_list",
                br#"{"outer":{"name":1,"\u006eame":2}}"#,
            ),
            Err(AiRequestError::DuplicateField)
        );
        assert_eq!(
            producer.build(
                1,
                "ai.provider_list",
                "{\"outer\":{\"\\uD83D\\uDE00\":1,\"😀\":2}}".as_bytes(),
            ),
            Err(AiRequestError::DuplicateField)
        );
        assert_eq!(
            producer.build(
                1,
                "ai.provider_list",
                br#"{"outer":{"\uD83D\uDE00":1,"\uD83D\uDE01":2}}"#,
            ),
            Ok(br#"{"v":1,"id":1,"method":"ai.provider_list","params":{"outer":{"\uD83D\uDE00":1,"\uD83D\uDE01":2},"snapshot_version":0}}"#.to_vec()),
        );
        for malformed in [
            br#"{"x":"\uD800"}"#.as_slice(),
            br#"{"x":"\uDC00"}"#.as_slice(),
            br#"{"x":"\uD83D\u0041"}"#.as_slice(),
            br#"{"x":"\u12G4"}"#.as_slice(),
        ] {
            assert_eq!(
                producer.build(1, "ai.provider_list", malformed),
                Err(AiRequestError::MalformedParams)
            );
        }
    }

    #[test]
    fn metadata_boundary_rejects_credential_like_values_but_keeps_service_facts() {
        let legitimate = ServiceMetadata::try_new(
            "openai/chat",
            "v1.2.3",
            &[
                "streaming".to_owned(),
                "tool-calls".to_owned(),
                "token-counting".to_owned(),
            ],
        )
        .expect("service facts are accepted");
        assert_eq!(legitimate.service(), "openai/chat");

        for identifier in ["pk-plugin", "rk-client", "ak-client", "capability-token"] {
            ServiceMetadata::try_new(identifier, "v1", &[]).unwrap_or_else(|error| {
                panic!("identifier must be accepted: {identifier}: {error}")
            });
        }

        for value in [
            "Bearer abc123",
            "user:password@example.test",
            "https://user:password@example.test",
            "sk-live-abc123",
            "sk-abcdefghijklmnopqrstuvwxyz0123456789",
            "pk-live-abc123",
            "rk-abcdefghijklmnopqrstuvwxyz0123456789",
            "access_token=abc123",
            "eyJhbGciOiJIUzI1NiJ9.x.y",
        ] {
            assert!(
                ServiceMetadata::try_new(value, "v1", &[]).is_err(),
                "credential-like service value must be rejected: {value}"
            );
            assert!(
                ServiceMetadata::try_new("provider", value, &[]).is_err(),
                "credential-like version value must be rejected: {value}"
            );
            assert!(
                ServiceMetadata::try_new("provider", "v1", &[value.to_owned()]).is_err(),
                "credential-like capability value must be rejected: {value}"
            );
        }
    }

    #[test]
    fn request_envelope_is_structured_and_preserves_valid_params() {
        let producer = synchronized_producer();
        let request = producer
            .build(7, "ai.model_select", br#" { "required": ["text"] } "#)
            .expect("request");
        assert_eq!(
            request,
            br#"{"v":1,"id":7,"method":"ai.model_select","params":{"required": ["text"],"snapshot_version":0}}"#
        );
    }

    #[test]
    fn nested_params_are_bounded_and_payloads_are_bounded() {
        let producer = synchronized_producer();
        let mut nested = String::new();
        for _ in 0..=MAX_AI_PARAMS_DEPTH {
            nested.push_str("{\"x\":");
        }
        nested.push('0');
        for _ in 0..=MAX_AI_PARAMS_DEPTH {
            nested.push('}');
        }
        assert_eq!(
            producer.build(1, "ai.provider_list", nested.as_bytes()),
            Err(AiRequestError::MalformedParams)
        );
        let huge = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            producer.build(1, "ai.provider_list", &huge),
            Err(AiRequestError::PayloadTooLarge { .. })
        ));

        let mut fields = String::from("{");
        for index in 0..=MAX_AI_PARAMS_FIELDS {
            if index != 0 {
                fields.push(',');
            }
            fields.push_str(&format!("\"field{index}\":0"));
        }
        fields.push('}');
        assert_eq!(
            producer.build(1, "ai.provider_list", fields.as_bytes()),
            Err(AiRequestError::MalformedParams)
        );

        let mut nested_fields = String::from("{\"nested\":{");
        for index in 0..=MAX_AI_PARAMS_FIELDS {
            if index != 0 {
                nested_fields.push(',');
            }
            nested_fields.push_str(&format!("\"field{index}\":0"));
        }
        nested_fields.push_str("}}");
        assert_eq!(
            producer.build(1, "ai.provider_list", nested_fields.as_bytes()),
            Err(AiRequestError::MalformedParams)
        );

        let mut whitespace_prefixed = vec![b' '; MAX_FRAME_BYTES + 1];
        whitespace_prefixed.extend_from_slice(b"{}");
        assert_eq!(
            producer.build(1, "ai.provider_list", &whitespace_prefixed),
            Err(AiRequestError::PayloadTooLarge {
                actual: MAX_FRAME_BYTES + 3,
                limit: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn long_shared_prefix_json_keys_decode_with_bounded_progress() {
        let mut params = String::from("{");
        let prefix = "a".repeat(512);
        for index in 0..(MAX_AI_PARAMS_FIELDS - 1) {
            if index != 0 {
                params.push(',');
            }
            params.push('"');
            params.push_str(&prefix);
            params.push_str(&format!("{index:03}"));
            params.push_str("\":0");
        }
        params.push('}');

        let producer = synchronized_producer();
        assert!(
            producer
                .build(1, "ai.provider_list", params.as_bytes())
                .is_ok()
        );
    }

    #[test]
    fn request_uses_nonzero_version_after_refresh_and_invalidation() {
        let mut producer = AiRequestProducer::new();
        let initial = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&initial)
            .expect("initial publication");
        producer
            .metadata_mut()
            .refresh(&[record("provider", "v1")])
            .expect("refresh");
        let refreshed_publication = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&refreshed_publication)
            .expect("refreshed publication");
        let refreshed = producer
            .build(1, "ai.provider_list", b"{}")
            .expect("request after refresh");
        assert!(refreshed.ends_with(br#""snapshot_version":1}}"#));

        producer.metadata_mut().invalidate();
        let invalidated_publication = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&invalidated_publication)
            .expect("invalidated publication");
        let invalidated = producer
            .build(2, "ai.provider_list", b"{}")
            .expect("request after invalidation");
        assert!(invalidated.ends_with(br#""snapshot_version":2}}"#));
    }

    #[test]
    fn snapshot_version_saturates() {
        let mut producer = AiMetadataProducer::new();
        producer.snapshot.version = u64::MAX;
        let publication = producer.publish();
        producer
            .acknowledge_local_publication(&publication)
            .expect("maximum-version publication");
        producer.invalidate();
        assert_eq!(producer.version(), u64::MAX);
        assert!(matches!(
            producer.sync_state(),
            SnapshotSyncState::Stale {
                acknowledged_version: u64::MAX,
                current_version: u64::MAX,
            }
        ));
        let refreshed = producer.publish();
        producer
            .acknowledge_local_publication(&refreshed)
            .expect("new saturated publication");
        assert_eq!(
            producer.sync_state(),
            SnapshotSyncState::Acknowledged { version: u64::MAX }
        );
    }

    #[test]
    fn requests_require_new_ack_after_saturated_mutation() {
        let mut producer = AiRequestProducer::new();
        producer.metadata.snapshot.version = u64::MAX;
        let publication = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&publication)
            .expect("maximum-version publication");
        assert!(producer.build(1, "ai.provider_list", b"{}").is_ok());

        producer.metadata_mut().invalidate();
        assert!(matches!(
            producer.build(1, "ai.provider_list", b"{}"),
            Err(AiRequestError::MetadataNotSynchronized(
                SnapshotSyncState::Stale {
                    acknowledged_version: u64::MAX,
                    current_version: u64::MAX,
                }
            ))
        ));

        let refreshed = producer.publish_metadata();
        producer
            .acknowledge_local_metadata(&refreshed)
            .expect("new maximum-version publication");
        assert!(producer.build(1, "ai.provider_list", b"{}").is_ok());
    }

    #[test]
    fn requests_fail_closed_until_current_publication_is_acknowledged() {
        let mut producer = AiRequestProducer::new();
        assert!(matches!(
            producer.build(1, "ai.provider_list", b"{}"),
            Err(AiRequestError::MetadataNotSynchronized(
                SnapshotSyncState::Unpublished
            ))
        ));
        let publication = producer.publish_metadata();
        assert!(matches!(
            producer.build(1, "ai.provider_list", b"{}"),
            Err(AiRequestError::MetadataNotSynchronized(
                SnapshotSyncState::Published { version: 0 }
            ))
        ));
        producer
            .acknowledge_local_metadata(&publication)
            .expect("publication is current");
        assert!(producer.build(1, "ai.provider_list", b"{}").is_ok());

        producer.metadata_mut().invalidate();
        assert!(matches!(
            producer.build(1, "ai.provider_list", b"{}"),
            Err(AiRequestError::MetadataNotSynchronized(
                SnapshotSyncState::Stale {
                    acknowledged_version: 0,
                    current_version: 1
                }
            ))
        ));
    }

    #[test]
    fn stale_publication_cannot_be_acknowledged_after_refresh() {
        let mut producer = AiMetadataProducer::new();
        let old = producer.publish();
        let replay = old.clone();
        let current = producer.publish();
        assert_eq!(
            producer.acknowledge_local_publication(&replay),
            Err(MetadataSyncError::StalePublication {
                publication_version: 0,
                current_version: 0,
            })
        );
        producer
            .acknowledge_local_publication(&current)
            .expect("current publication");
        producer
            .refresh(&[record("provider", "v1")])
            .expect("refresh");
        assert_eq!(
            producer.acknowledge_local_publication(&old),
            Err(MetadataSyncError::StalePublication {
                publication_version: 0,
                current_version: 1,
            })
        );
    }

    #[test]
    fn parser_accepts_raw_non_ascii_values_and_rejects_trailing_bytes() {
        let producer = synchronized_producer();
        assert!(
            producer
                .build(1, "ai.provider_list", b"{\"label\":\"caf\xC3\xA9\"}")
                .is_ok()
        );
        assert_eq!(
            producer.build(1, "ai.provider_list", br#"{"label":"ok"}x"#),
            Err(AiRequestError::MalformedParams)
        );
    }
}
