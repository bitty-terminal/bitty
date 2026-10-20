//! Deterministic tests for the Core publication-control driver.
//!
//! Every test uses an in-memory peer and the pinned pure record codec. No TLS
//! handshake, provider, credential, handler, socket, thread, or clock is
//! involved. The live TLS adapter is compile-checked by the feature build and
//! its session access is exercised by the launch-seam fixture tests.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Cursor;
use std::rc::Rc;

use bitty_ipc_api::provider_publication::{
    AckBody, AckId, BoundBody, CONTROL_FRAME_MAX_BYTES, Capability, ControlError, ControlRecord,
    Digest, ErrorCode, EstablishedBody, EstablishmentEcho, InvalidateBody, Kind, LifetimeBody,
    LifetimeId, ModelEntry, Nonce, ProjectionBody, ProviderEntry, ProviderRoutingProjection,
    PublicationId, Reason, ReceiptId, RecordBody, Sender, SessionId, Tuple,
};

use super::framing::{self, ControlIoError};
use super::{
    ControlEvent, ControlPhase, ControlRecordIo, LaunchBinding, MetadataView, PublicationControl,
    PublicationControlError, RevocationReason,
};
use crate::ai_publication_launch::LaunchIdentity;

const LAUNCH_NONCE: [u8; 32] = [0x11; 32];
const ACK_BYTES: [u8; 32] = [0x33; 32];
const RECEIPT_BYTES: [u8; 32] = [0x44; 32];
const EMPTY_PROJECTION_DIGEST: &str =
    "c2c6d4a1a7b593a4888956ebb74b129d30e4f37fc980cb43279cbb2e26783dbb";

// ── in-memory peer ──────────────────────────────────────────────────────────

#[derive(Default)]
struct PeerState {
    inbound: VecDeque<ControlRecord>,
    outbound: Vec<ControlRecord>,
    fail_receive: bool,
    fail_send: bool,
}

#[derive(Clone, Default)]
struct PeerHandle(Rc<RefCell<PeerState>>);

impl PeerHandle {
    fn peer(&self) -> TestPeer {
        TestPeer {
            state: Rc::clone(&self.0),
        }
    }

    fn queue(&self, record: ControlRecord) {
        self.0.borrow_mut().inbound.push_back(record);
    }

    fn take_sent(&self) -> Vec<ControlRecord> {
        std::mem::take(&mut self.0.borrow_mut().outbound)
    }

    fn sent(&self) -> Vec<ControlRecord> {
        self.0.borrow().outbound.clone()
    }

    fn fail_receive(&self) {
        self.0.borrow_mut().fail_receive = true;
    }

    fn fail_send(&self) {
        self.0.borrow_mut().fail_send = true;
    }
}

struct TestPeer {
    state: Rc<RefCell<PeerState>>,
}

impl ControlRecordIo for TestPeer {
    fn send(&mut self, record: &ControlRecord) -> Result<(), ControlIoError> {
        let mut state = self.state.borrow_mut();
        if state.fail_send {
            return Err(ControlIoError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test peer send failure",
            )));
        }
        state.outbound.push(record.clone());
        Ok(())
    }

    fn receive(&mut self) -> Result<ControlRecord, ControlIoError> {
        let mut state = self.state.borrow_mut();
        if state.fail_receive {
            return Err(ControlIoError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "test peer receive failure",
            )));
        }
        state.inbound.pop_front().ok_or(ControlIoError::Closed)
    }
}

// ── fixtures ────────────────────────────────────────────────────────────────

fn binding() -> LaunchBinding {
    LaunchBinding::new(
        LaunchIdentity::new("bitty-ai-helper", "1.2.3", "bitty-ai-helper", [0xab; 32], 7),
        Nonce::new([0x01; 32]),
        SessionId::new(LAUNCH_NONCE),
        Nonce::new([0x03; 32]),
        Nonce::new([0x04; 32]),
        Digest::new([0x05; 32]),
        Digest::new([0x06; 32]),
    )
}

fn expected_echo() -> EstablishmentEcho {
    binding().expected_echo()
}

fn lifetime() -> LifetimeId {
    LifetimeId::new([0x22; 32])
}

fn second_lifetime() -> LifetimeId {
    LifetimeId::new([0x23; 32])
}

fn third_lifetime() -> LifetimeId {
    LifetimeId::new([0x24; 32])
}

fn empty_projection() -> ProviderRoutingProjection {
    ProviderRoutingProjection::empty()
}

fn populated_projection() -> ProviderRoutingProjection {
    let model = ModelEntry::new(
        "model-a",
        vec![Capability::Streaming, Capability::Text],
        8192,
        1,
        2,
    )
    .expect("model entry");
    let provider = ProviderEntry::new("provider-a", vec![model]).expect("provider entry");
    ProviderRoutingProjection::new(vec![provider]).expect("projection")
}

fn record(sender: Sender, sequence: u64, body: RecordBody) -> ControlRecord {
    record_with_session(sender, sequence, SessionId::new(LAUNCH_NONCE), body)
}

fn record_with_session(
    sender: Sender,
    sequence: u64,
    session_id: SessionId,
    body: RecordBody,
) -> ControlRecord {
    ControlRecord {
        sender,
        sequence,
        session_id,
        body,
    }
}

fn established(lifetime_id: LifetimeId) -> RecordBody {
    RecordBody::Established(EstablishedBody {
        echo: expected_echo(),
        lifetime_id,
    })
}

fn projection(
    lifetime_id: LifetimeId,
    feed_revision: u64,
    payload: ProviderRoutingProjection,
) -> RecordBody {
    RecordBody::Projection(ProjectionBody {
        lifetime_id,
        feed_revision,
        payload,
    })
}

fn ack(lifetime_id: LifetimeId, feed_revision: u64, tuple: Tuple, ack_id: AckId) -> RecordBody {
    RecordBody::Ack(AckBody {
        lifetime_id,
        feed_revision,
        tuple,
        ack_id,
    })
}

fn bound(
    lifetime_id: LifetimeId,
    feed_revision: u64,
    tuple: Tuple,
    receipt: ReceiptId,
) -> RecordBody {
    RecordBody::Bound(BoundBody {
        lifetime_id,
        feed_revision,
        tuple,
        receipt,
    })
}

fn invalidate(lifetime_id: LifetimeId, feed_revision: u64, reason: Reason) -> RecordBody {
    RecordBody::Invalidate(InvalidateBody {
        lifetime_id,
        feed_revision,
        reason,
    })
}

fn lifetime_record(lifetime_id: LifetimeId, feed_revision: u64) -> RecordBody {
    RecordBody::Lifetime(LifetimeBody {
        lifetime_id,
        feed_revision,
    })
}

struct Script {
    handle: PeerHandle,
    inbound: u64,
}

impl Script {
    fn new() -> Self {
        Self {
            handle: PeerHandle::default(),
            inbound: 0,
        }
    }

    fn control(&self) -> PublicationControl<TestPeer> {
        PublicationControl::new(self.handle.peer(), binding()).expect("driver construction")
    }

    fn push(&mut self, body: RecordBody) -> u64 {
        self.inbound += 1;
        let sequence = self.inbound;
        self.handle.queue(record(Sender::Helper, sequence, body));
        sequence
    }

    fn push_established(&mut self) -> LifetimeId {
        let lifetime_id = lifetime();
        self.push(established(lifetime_id));
        lifetime_id
    }

    fn push_initial_projection(&mut self, feed_revision: u64) -> LifetimeId {
        let lifetime_id = lifetime();
        self.push(established(lifetime_id));
        self.push(projection(lifetime_id, feed_revision, empty_projection()));
        lifetime_id
    }

    fn push_ack(&mut self, lifetime_id: LifetimeId, feed_revision: u64, tuple: Tuple) -> AckId {
        let ack_id = AckId::new(ACK_BYTES);
        self.push(ack(lifetime_id, feed_revision, tuple, ack_id));
        ack_id
    }

    fn push_bound(
        &mut self,
        lifetime_id: LifetimeId,
        feed_revision: u64,
        tuple: Tuple,
    ) -> ReceiptId {
        let receipt = ReceiptId::new(RECEIPT_BYTES);
        self.push(bound(lifetime_id, feed_revision, tuple, receipt));
        receipt
    }

    fn take_sent(&self) -> Vec<ControlRecord> {
        self.handle.take_sent()
    }

    fn sent(&self) -> Vec<ControlRecord> {
        self.handle.sent()
    }
}

fn drive_to_ready(script: &mut Script, control: &mut PublicationControl<TestPeer>) -> LifetimeId {
    let lifetime_id = script.push_initial_projection(1);
    assert_eq!(
        control.establish().expect("establishment"),
        lifetime_id,
        "fixture lifetime"
    );
    assert!(matches!(
        control.receive().expect("projection"),
        ControlEvent::ProjectionAdopted {
            feed_revision: 1,
            snapshot_version: 1,
            publication_version: 1,
            ..
        }
    ));
    let sent = script.take_sent();
    assert_eq!(sent.len(), 1, "only Establish was sent");
    lifetime_id
}

fn drive_to_bound(script: &mut Script) -> (PublicationControl<TestPeer>, LifetimeId, Tuple) {
    let mut control = script.control();
    let lifetime_id = drive_to_ready(script, &mut control);
    let tuple = control.send_publish().expect("publish");
    script.push_ack(lifetime_id, 1, tuple);
    assert!(matches!(
        control.receive().expect("ack"),
        ControlEvent::PublicationAcknowledged { .. }
    ));
    control.send_bind().expect("bind");
    let receipt = script.push_bound(lifetime_id, 1, tuple);
    assert_eq!(
        control.receive().expect("bound"),
        ControlEvent::PublicationBound { tuple, receipt }
    );
    script.take_sent();
    (control, lifetime_id, tuple)
}

fn assert_closed(control: &PublicationControl<TestPeer>) {
    assert_eq!(control.phase(), ControlPhase::Closed);
    assert!(control.is_closed());
    assert!(control.revocation_reason().is_some());
}

// ── golden path ─────────────────────────────────────────────────────────────

#[test]
#[cfg(unix)]
fn binding_snapshots_the_prepared_launch() {
    let prepared = crate::ai_publication_launch::prepare(
        LaunchIdentity::new("bitty-ai-helper", "1.2.3", "bitty-ai-helper", [0xab; 32], 7),
        [0x01; 32],
    )
    .expect("prepared launch");
    let binding = LaunchBinding::from_prepared(&prepared).expect("binding");

    assert_eq!(
        binding.launch_nonce(),
        SessionId::new(*prepared.launch_nonce())
    );
    let echo = binding.expected_echo();
    assert_eq!(echo.core_instance_nonce, Nonce::new([0x01; 32]));
    assert_eq!(
        echo.control_channel_nonce,
        Nonce::new(*prepared.control_channel_nonce())
    );
    assert_eq!(
        echo.data_channel_nonce,
        Nonce::new(*prepared.data_channel_nonce())
    );
    assert_eq!(
        echo.core_leaf_digest,
        Digest::new(prepared.core_cert_sha256().expect("core pin"))
    );
    assert_eq!(
        echo.helper_leaf_digest,
        Digest::new(prepared.helper_cert_sha256().expect("helper pin"))
    );
    assert_eq!(echo.component_name, "bitty-ai-helper");
    assert_eq!(echo.descriptor_version, "1.2.3");
    assert_eq!(echo.executable_name, "bitty-ai-helper");
    assert_eq!(echo.executable_sha256, Digest::new([0xab; 32]));
    assert_eq!(echo.component_protocol_version, 7);
}

#[test]
fn golden_establish_projection_publish_ack_bind_bound() {
    let mut script = Script::new();
    let mut control = script.control();

    script.push(established(lifetime()));
    script.push(projection(lifetime(), 1, empty_projection()));

    assert_eq!(control.establish().expect("establish"), lifetime());
    assert_eq!(control.phase(), ControlPhase::AwaitProjection);
    assert_eq!(control.lifetime_id(), Some(lifetime()));

    let adopted = control.receive().expect("projection");
    assert_eq!(
        adopted,
        ControlEvent::ProjectionAdopted {
            lifetime_id: lifetime(),
            feed_revision: 1,
            snapshot_version: 1,
            publication_version: 1,
        }
    );
    assert_eq!(control.phase(), ControlPhase::Ready);
    let view: &MetadataView = control.view().expect("adopted view");
    assert_eq!(view.feed_revision(), 1);
    assert_eq!(control.snapshot_version(), Some(1));
    assert_eq!(view.providers(), &[]);
    assert_eq!(
        view.canonical_bytes(),
        br#"{"domain":"provider-routing","providers":[]}"#
    );
    assert_eq!(view.digest().to_hex(), EMPTY_PROJECTION_DIGEST);
    assert_eq!(view.projection(), &empty_projection());

    let tuple = control.send_publish().expect("publish");
    assert_eq!(
        tuple.publication_id,
        PublicationId::from_parts(SessionId::new(LAUNCH_NONCE), 1)
    );
    assert_eq!(
        tuple.publication_id.to_hex(),
        format!("{}0000000000000001", "11".repeat(32))
    );
    assert_eq!(tuple.snapshot_version, 1);
    assert_eq!(tuple.digest.to_hex(), EMPTY_PROJECTION_DIGEST);
    assert_eq!(control.phase(), ControlPhase::AwaitAck);

    let sent = script.take_sent();
    assert_eq!(sent.len(), 2, "Establish then Publish");
    assert_eq!(sent[0].sequence, 1);
    assert_eq!(sent[0].sender, Sender::Core);
    assert_eq!(sent[0].session_id, SessionId::new(LAUNCH_NONCE));
    let RecordBody::Establish(echo) = &sent[0].body else {
        panic!("first record is Establish");
    };
    assert_eq!(echo, &expected_echo());
    assert_eq!(sent[1].sequence, 2);
    let RecordBody::Publish(publish) = &sent[1].body else {
        panic!("second record is Publish");
    };
    assert_eq!(publish.lifetime_id, lifetime());
    assert_eq!(publish.feed_revision, 1);
    assert_eq!(publish.tuple, tuple);
    assert_eq!(publish.payload, empty_projection());

    let ack_id = script.push_ack(lifetime(), 1, tuple);
    assert_eq!(
        control.receive().expect("ack"),
        ControlEvent::PublicationAcknowledged { ack_id }
    );
    assert_eq!(control.phase(), ControlPhase::AckAccepted);
    control.send_bind().expect("bind");
    assert_eq!(control.phase(), ControlPhase::AwaitBound);

    let sent = script.take_sent();
    assert_eq!(sent.len(), 1, "Bind only");
    assert_eq!(sent[0].sequence, 3);
    let RecordBody::Bind(bind) = &sent[0].body else {
        panic!("third record is Bind");
    };
    assert_eq!(bind.lifetime_id, lifetime());
    assert_eq!(bind.feed_revision, 1);
    assert_eq!(bind.tuple, tuple);
    assert_eq!(bind.ack_id, ack_id);

    let receipt = script.push_bound(lifetime(), 1, tuple);
    assert_eq!(
        control.receive().expect("bound"),
        ControlEvent::PublicationBound { tuple, receipt }
    );
    assert_eq!(control.phase(), ControlPhase::Bound);
    assert_eq!(control.receipt(), Some(receipt));
    assert_eq!(control.current_tuple(), Some(tuple));
    assert_eq!(control.next_publication_version(), 2);

    // Slice boundary: no data-channel admission record is ever sent.
    let sent = script.take_sent();
    assert!(sent.is_empty());
}

#[test]
fn publication_version_can_advance_without_a_new_source_view() {
    let mut script = Script::new();
    let mut control = script.control();
    drive_to_ready(&mut script, &mut control);
    assert_eq!(control.snapshot_version(), Some(1));
    assert_eq!(control.next_publication_version(), 2);

    // A publication-only mint does not adopt a new source view: only the
    // publication version advances, while the tuple keeps snapshot version 1.
    let publication_version = control
        .allocate_publication_version()
        .expect("publication version");
    assert_eq!(publication_version, 2);
    assert_eq!(control.next_publication_version(), 3);
    assert_eq!(control.snapshot_version(), Some(1), "source view unchanged");

    let tuple = control
        .mint_tuple(
            publication_version,
            control.snapshot_version().expect("snapshot"),
            empty_projection().digest(),
        )
        .expect("tuple");
    assert_eq!(tuple.publication_id.version(), 2);
    assert_eq!(tuple.snapshot_version, 1);

    // Minting the next source-view version does not adopt it; the current
    // view keeps its snapshot until a Projection replaces it.
    let next_snapshot = control
        .allocate_source_view_version()
        .expect("source-view version");
    assert_eq!(next_snapshot, 2);
    assert_eq!(control.snapshot_version(), Some(1));
}

#[test]
fn populated_projection_is_adopted_canonically() {
    let mut script = Script::new();
    let mut control = script.control();
    let payload = populated_projection();
    script.push(established(lifetime()));
    script.push(projection(lifetime(), 1, payload.clone()));

    control.establish().expect("establish");
    control.receive().expect("projection");

    let view = control.view().expect("view");
    assert_eq!(view.providers().len(), 1);
    assert_eq!(view.providers()[0].id(), "provider-a");
    assert_eq!(view.providers()[0].models()[0].name(), "model-a");
    assert_eq!(view.providers()[0].models()[0].capabilities().len(), 2);
    assert_eq!(view.canonical_bytes(), payload.canonical_bytes());
    assert_eq!(view.digest(), payload.digest());

    let tuple = control.send_publish().expect("publish");
    assert_eq!(tuple.digest, payload.digest());
    let sent = script.take_sent();
    let RecordBody::Publish(publish) = &sent.last().expect("publish record").body else {
        panic!("last record is Publish");
    };
    assert_eq!(publish.payload.canonical_bytes(), payload.canonical_bytes());
}

// ── establishment and envelope negatives ────────────────────────────────────

#[test]
fn wrong_echo_field_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let mut echo = expected_echo();
    echo.descriptor_version = "9.9.9".to_owned();
    script.push(RecordBody::Established(EstablishedBody {
        echo,
        lifetime_id: lifetime(),
    }));

    let error = control.establish().expect_err("echo mismatch");
    assert!(matches!(
        error,
        PublicationControlError::EstablishmentMismatch {
            field: "descriptor_version"
        }
    ));
    assert_eq!(
        control.revocation_reason(),
        Some(RevocationReason::EstablishmentMismatch)
    );
    assert_closed(&control);
    assert_eq!(script.sent().len(), 1, "only Establish was sent");
}

#[test]
fn zero_lifetime_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    script.push(established(LifetimeId::ZERO));

    let error = control.establish().expect_err("zero lifetime");
    assert!(matches!(error, PublicationControlError::LifetimeNotFresh));
    assert_closed(&control);
}

#[test]
fn session_mismatch_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    script.push_established();
    let mut mismatched = established(lifetime());
    if let RecordBody::Established(body) = &mut mismatched {
        body.echo.launch_nonce = SessionId::new([0x99; 32]);
    }
    script.handle.queue(record_with_session(
        Sender::Helper,
        2,
        SessionId::new([0x99; 32]),
        mismatched,
    ));

    control.establish().expect("establish");
    let error = control.receive().expect_err("session mismatch");
    assert!(matches!(error, PublicationControlError::SessionMismatch));
    assert_closed(&control);
}

#[test]
fn sequence_gap_revokes() {
    let script = Script::new();
    let mut control = script.control();
    script
        .handle
        .queue(record(Sender::Helper, 2, established(lifetime())));

    let error = control.establish().expect_err("sequence gap");
    assert!(matches!(error, PublicationControlError::SequenceMismatch));
    assert_closed(&control);
}

#[test]
fn wrong_sender_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);

    script.handle.queue(record(
        Sender::Core,
        3,
        invalidate(lifetime_id, 1, Reason::SourceMutation),
    ));
    let error = control.receive().expect_err("wrong sender");
    assert!(matches!(
        error,
        PublicationControlError::UnexpectedSender {
            sender: Sender::Core
        }
    ));
    assert_closed(&control);
}

// ── projection and transaction negatives ────────────────────────────────────

#[test]
fn wrong_projection_lifetime_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    script.push(established(lifetime()));
    script.push(projection(second_lifetime(), 1, empty_projection()));

    control.establish().expect("establish");
    let error = control.receive().expect_err("wrong projection lifetime");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn duplicate_projection_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    script.push(projection(lifetime_id, 1, empty_projection()));

    let error = control.receive().expect_err("duplicate projection");
    assert!(matches!(
        error,
        PublicationControlError::UnexpectedRecord {
            kind: Kind::Projection,
            phase: ControlPhase::Ready,
        }
    ));
    assert_closed(&control);
}

#[test]
fn zero_ack_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.send_publish().expect("publish");
    script.push(ack(lifetime_id, 1, tuple, AckId::ZERO));

    let error = control.receive().expect_err("zero ack");
    assert!(matches!(error, PublicationControlError::IdentifierNotFresh));
    assert_closed(&control);
}

#[test]
fn wrong_ack_tuple_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.send_publish().expect("publish");
    let mut wrong = tuple;
    wrong.snapshot_version = 9;
    script.push(ack(lifetime_id, 1, wrong, AckId::new(ACK_BYTES)));

    let error = control.receive().expect_err("wrong ack tuple");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn duplicate_ack_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.send_publish().expect("publish");
    let ack_id = script.push_ack(lifetime_id, 1, tuple);
    control.receive().expect("ack");

    script.push(ack(lifetime_id, 1, tuple, ack_id));
    let error = control.receive().expect_err("duplicate ack");
    assert!(matches!(
        error,
        PublicationControlError::UnexpectedRecord {
            kind: Kind::Ack,
            ..
        }
    ));
    assert_closed(&control);
}

#[test]
fn out_of_order_bound_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.current_tuple().expect("pending tuple");
    script.push(bound(lifetime_id, 1, tuple, ReceiptId::new(RECEIPT_BYTES)));

    let error = control.receive().expect_err("bound before bind");
    assert!(matches!(
        error,
        PublicationControlError::UnexpectedRecord {
            kind: Kind::Bound,
            phase: ControlPhase::Ready,
        }
    ));
    assert_closed(&control);
}

#[test]
fn wrong_bound_tuple_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.send_publish().expect("publish");
    script.push_ack(lifetime_id, 1, tuple);
    control.receive().expect("ack");
    control.send_bind().expect("bind");

    let mut wrong = tuple;
    wrong.digest = Digest::new([0x55; 32]);
    script.push(bound(lifetime_id, 1, wrong, ReceiptId::new(RECEIPT_BYTES)));
    let error = control.receive().expect_err("wrong bound tuple");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn receipt_must_be_fresh_and_distinct_from_ack() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    let tuple = control.send_publish().expect("publish");
    script.push_ack(lifetime_id, 1, tuple);
    control.receive().expect("ack");
    control.send_bind().expect("bind");

    script.push(bound(lifetime_id, 1, tuple, ReceiptId::new(ACK_BYTES)));
    let error = control.receive().expect_err("receipt equals ack");
    assert!(matches!(error, PublicationControlError::IdentifierNotFresh));
    assert_closed(&control);
}

#[test]
fn error_record_closes() {
    let mut script = Script::new();
    let mut control = script.control();
    drive_to_ready(&mut script, &mut control);
    script.push(RecordBody::Error(ErrorCode::Unavailable));

    let error = control.receive().expect_err("peer error");
    assert!(matches!(
        error,
        PublicationControlError::PeerError {
            code: ErrorCode::Unavailable
        }
    ));
    assert_eq!(
        control.revocation_reason(),
        Some(RevocationReason::PeerError)
    );
    assert_closed(&control);
}

// ── rotation, mutation, and predecessor window ──────────────────────────────

#[test]
fn helper_registry_mutation_rotates_and_republishes() {
    let mut script = Script::new();
    let (mut control, lifetime_id, old_tuple) = drive_to_bound(&mut script);

    script.push(invalidate(lifetime_id, 1, Reason::RegistryMutation));
    assert_eq!(
        control.receive().expect("mutation invalidate"),
        ControlEvent::Invalidated {
            lifetime_id,
            feed_revision: 1,
            reason: Reason::RegistryMutation,
            correlation_only: false,
        }
    );
    assert_eq!(control.phase(), ControlPhase::AwaitLifetime);
    assert!(control.view().is_none());
    assert!(control.receipt().is_none());
    assert!(control.current_tuple().is_none());

    script.push(lifetime_record(second_lifetime(), 2));
    assert_eq!(
        control.receive().expect("lifetime"),
        ControlEvent::LifetimeRotated {
            previous: lifetime_id,
            lifetime_id: second_lifetime(),
            feed_revision: 2,
        }
    );
    assert_eq!(control.phase(), ControlPhase::AwaitProjection);
    assert_eq!(control.lifetime_id(), Some(second_lifetime()));

    script.push(projection(second_lifetime(), 2, empty_projection()));
    assert_eq!(
        control.receive().expect("rotation projection"),
        ControlEvent::ProjectionAdopted {
            lifetime_id: second_lifetime(),
            feed_revision: 2,
            snapshot_version: 2,
            publication_version: 2,
        }
    );
    assert_eq!(control.snapshot_version(), Some(2));
    let new_tuple = control.send_publish().expect("republish");
    assert_eq!(new_tuple.publication_id.version(), 2);
    assert_eq!(new_tuple.snapshot_version, 2);
    assert_ne!(new_tuple, old_tuple);

    let sent = script.take_sent();
    let RecordBody::Publish(publish) = &sent.last().expect("publish").body else {
        panic!("last record is Publish");
    };
    assert_eq!(publish.lifetime_id, second_lifetime());
    assert_eq!(publish.feed_revision, 2);
    assert_eq!(publish.tuple, new_tuple);
}

#[test]
fn core_invalidation_rotates_and_republishes() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);

    control
        .invalidate(Reason::SourceMutation)
        .expect("local invalidation");
    assert_eq!(control.phase(), ControlPhase::AwaitLifetime);
    let sent = script.take_sent();
    assert_eq!(sent.len(), 1, "Invalidate only");
    assert_eq!(sent[0].sequence, 4, "after Establish, Publish, Bind");
    assert!(matches!(
        &sent[0].body,
        RecordBody::Invalidate(body)
            if body.lifetime_id == lifetime_id
                && body.feed_revision == 1
                && body.reason == Reason::SourceMutation
    ));

    // A crossed helper invalidation for the same lifetime is consumed once as
    // correlation while the replacement is already on its way.
    script.push(invalidate(lifetime_id, 1, Reason::Reset));
    assert_eq!(
        control.receive().expect("crossed invalidate"),
        ControlEvent::Invalidated {
            lifetime_id,
            feed_revision: 1,
            reason: Reason::Reset,
            correlation_only: true,
        }
    );
    assert_eq!(control.phase(), ControlPhase::AwaitLifetime);

    script.push(lifetime_record(second_lifetime(), 1));
    script.push(projection(second_lifetime(), 1, empty_projection()));
    control.receive().expect("lifetime");
    assert_eq!(
        control.receive().expect("rotation projection"),
        ControlEvent::ProjectionAdopted {
            lifetime_id: second_lifetime(),
            feed_revision: 1,
            snapshot_version: 2,
            publication_version: 2,
        }
    );
    assert_eq!(control.snapshot_version(), Some(2));
    let tuple = control.send_publish().expect("republish");
    assert_eq!(tuple.publication_id.version(), 2);
    assert_eq!(tuple.snapshot_version, 2);

    // The rotation and republish completed with the one crossed
    // invalidation already consumed. Replaying the same predecessor now must
    // be refused instead of granting a second correlation window.
    script.push(invalidate(lifetime_id, 1, Reason::Reset));
    let error = control
        .receive()
        .expect_err("replayed crossed predecessor after rotation");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert!(control.is_closed());
}

#[test]
fn duplicate_crossed_invalidation_after_lifetime_is_refused() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);

    control
        .invalidate(Reason::SourceMutation)
        .expect("local invalidation");
    script.push(invalidate(lifetime_id, 1, Reason::Reset));
    assert_eq!(
        control.receive().expect("crossed invalidate"),
        ControlEvent::Invalidated {
            lifetime_id,
            feed_revision: 1,
            reason: Reason::Reset,
            correlation_only: true,
        }
    );

    script.push(lifetime_record(second_lifetime(), 1));
    assert_eq!(
        control.receive().expect("lifetime"),
        ControlEvent::LifetimeRotated {
            previous: lifetime_id,
            lifetime_id: second_lifetime(),
            feed_revision: 1,
        }
    );
    assert_eq!(control.phase(), ControlPhase::AwaitProjection);

    // The crossed invalidation was already consumed before Lifetime. The
    // retired predecessor window carries that fact, so a replay after the
    // rotation is refused even before the replacement Projection arrives.
    script.push(invalidate(lifetime_id, 1, Reason::Reset));
    let error = control
        .receive()
        .expect_err("duplicate crossed predecessor after lifetime");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert!(control.is_closed());
}

#[test]
fn invalidate_is_busy_during_publish_without_revoking() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    control.send_publish().expect("publish");
    assert_eq!(control.phase(), ControlPhase::AwaitAck);

    let error = control
        .invalidate(Reason::SourceMutation)
        .expect_err("busy invalidation");
    assert!(matches!(
        error,
        PublicationControlError::Busy {
            phase: ControlPhase::AwaitAck
        }
    ));
    assert_eq!(control.phase(), ControlPhase::AwaitAck);
    assert!(!control.is_closed());

    // The transaction still completes normally.
    let tuple = control.current_tuple().expect("pending tuple");
    script.push_ack(lifetime_id, 1, tuple);
    control.receive().expect("ack");
}

#[test]
fn core_shutdown_closes_authority() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);

    control
        .invalidate(Reason::Shutdown)
        .expect("shutdown invalidation");
    assert_eq!(control.phase(), ControlPhase::Closed);
    assert_eq!(
        control.revocation_reason(),
        Some(RevocationReason::PeerWithdrewAuthority)
    );
    let sent = script.take_sent();
    assert!(matches!(
        &sent.last().expect("invalidate").body,
        RecordBody::Invalidate(body)
            if body.lifetime_id == lifetime_id && body.reason == Reason::Shutdown
    ));
    assert!(matches!(
        control.receive(),
        Err(PublicationControlError::Closed)
    ));
}

#[test]
fn helper_grant_loss_closes_authority() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = drive_to_ready(&mut script, &mut control);
    script.push(invalidate(lifetime_id, 1, Reason::GrantLoss));

    assert_eq!(
        control.receive().expect("grant loss"),
        ControlEvent::AuthorityClosed {
            reason: Reason::GrantLoss
        }
    );
    assert_closed(&control);
    assert_eq!(
        control.revocation_reason(),
        Some(RevocationReason::PeerWithdrewAuthority)
    );
}

#[test]
fn crossed_predecessor_invalidation_is_consumed_once() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);

    script.push(invalidate(lifetime_id, 1, Reason::SourceMutation));
    script.push(lifetime_record(second_lifetime(), 1));
    script.push(projection(second_lifetime(), 1, empty_projection()));
    control.receive().expect("invalidate");
    control.receive().expect("lifetime");
    control.receive().expect("projection");

    script.push(invalidate(lifetime_id, 1, Reason::SourceMutation));
    assert_eq!(
        control.receive().expect("crossed predecessor"),
        ControlEvent::Invalidated {
            lifetime_id,
            feed_revision: 1,
            reason: Reason::SourceMutation,
            correlation_only: true,
        }
    );
    assert_eq!(control.phase(), ControlPhase::Ready);

    script.push(invalidate(lifetime_id, 1, Reason::SourceMutation));
    let error = control.receive().expect_err("second crossed predecessor");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn outrun_predecessor_window_revokes() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);

    script.push(invalidate(lifetime_id, 1, Reason::SourceMutation));
    script.push(lifetime_record(second_lifetime(), 1));
    script.push(projection(second_lifetime(), 1, empty_projection()));
    control.receive().expect("invalidate");
    control.receive().expect("lifetime");
    control.receive().expect("projection");

    script.push(invalidate(second_lifetime(), 1, Reason::Reset));
    script.push(lifetime_record(third_lifetime(), 1));
    script.push(projection(third_lifetime(), 1, empty_projection()));
    control.receive().expect("second invalidate");
    control.receive().expect("second lifetime");
    control.receive().expect("second projection");

    // The predecessor window now holds L2; L1 is older than the retained
    // window and must never be revived.
    script.push(invalidate(lifetime_id, 1, Reason::SourceMutation));
    let error = control.receive().expect_err("outrun predecessor");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn duplicate_lifetime_identifier_revokes() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);
    control
        .invalidate(Reason::SourceMutation)
        .expect("invalidate");
    script.push(lifetime_record(lifetime_id, 1));

    let error = control.receive().expect_err("repeated lifetime");
    assert!(matches!(error, PublicationControlError::LifetimeNotFresh));
    assert_closed(&control);
}

#[test]
fn decreasing_feed_revision_revokes() {
    let mut script = Script::new();
    let mut control = script.control();
    let lifetime_id = lifetime();
    script.push(established(lifetime_id));
    script.push(projection(lifetime_id, 2, empty_projection()));
    control.establish().expect("establish");
    control.receive().expect("projection");

    script.push(invalidate(lifetime_id, 2, Reason::SourceMutation));
    control.receive().expect("invalidate");
    script.push(lifetime_record(second_lifetime(), 1));
    let error = control.receive().expect_err("decreasing revision");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

#[test]
fn rotation_projection_revision_mismatch_revokes() {
    let mut script = Script::new();
    let (mut control, lifetime_id, _) = drive_to_bound(&mut script);
    script.push(invalidate(lifetime_id, 1, Reason::RegistryMutation));
    control.receive().expect("invalidate");
    script.push(lifetime_record(second_lifetime(), 2));
    control.receive().expect("lifetime");
    script.push(projection(second_lifetime(), 1, empty_projection()));

    let error = control.receive().expect_err("stale rotation revision");
    assert!(matches!(error, PublicationControlError::StaleAuthority));
    assert_closed(&control);
}

// ── local preconditions, framing, and transport failures ────────────────────

#[test]
fn local_preconditions_do_not_revoke() {
    let mut script = Script::new();
    let mut control = script.control();

    assert!(matches!(
        control.send_publish(),
        Err(PublicationControlError::NotReady {
            phase: ControlPhase::Unestablished
        })
    ));
    assert!(matches!(
        control.send_bind(),
        Err(PublicationControlError::NotReady { .. })
    ));
    assert!(matches!(
        control.receive(),
        Err(PublicationControlError::NotReady { .. })
    ));
    assert!(!control.is_closed());

    drive_to_ready(&mut script, &mut control);
    assert!(matches!(
        control.send_bind(),
        Err(PublicationControlError::NotReady {
            phase: ControlPhase::Ready
        })
    ));
    assert!(!control.is_closed());
    assert!(control.send_publish().is_ok());
}

#[test]
fn registry_mutation_reason_is_refused_locally() {
    let mut script = Script::new();
    let mut control = script.control();
    drive_to_ready(&mut script, &mut control);

    let error = control
        .invalidate(Reason::RegistryMutation)
        .expect_err("helper-owned reason");
    assert!(matches!(
        error,
        PublicationControlError::HelperOwnedInvalidationReason {
            reason: Reason::RegistryMutation
        }
    ));
    assert!(!control.is_closed());
    assert_eq!(control.phase(), ControlPhase::Ready);
}

#[test]
fn transport_failures_revoke() {
    let script = Script::new();
    let mut control = script.control();
    script.handle.fail_receive();
    let error = control.establish().expect_err("receive failure");
    assert!(matches!(
        error,
        PublicationControlError::Transport(ControlIoError::Io(_))
    ));
    assert_eq!(
        control.revocation_reason(),
        Some(RevocationReason::ControlTransport)
    );
    assert_closed(&control);

    let mut script = Script::new();
    let mut control = script.control();
    drive_to_ready(&mut script, &mut control);
    script.handle.fail_send();
    let error = control.send_publish().expect_err("send failure");
    assert!(matches!(
        error,
        PublicationControlError::Transport(ControlIoError::Io(_))
    ));
    assert_closed(&control);
}

#[test]
fn framing_round_trips_one_record() {
    let record = record(Sender::Core, 1, RecordBody::Establish(expected_echo()));
    let encoded = record.encode().expect("encode");
    let mut bytes = Vec::new();
    framing::send_record(&mut bytes, &record).expect("write");
    assert_eq!(
        &bytes[..4],
        &u32::try_from(encoded.len())
            .expect("length fits")
            .to_be_bytes()
    );
    assert_eq!(&bytes[4..], encoded.as_slice());

    let mut cursor = Cursor::new(bytes);
    assert_eq!(framing::receive_record(&mut cursor).expect("read"), record);
}

#[test]
fn framing_refuses_zero_oversized_short_and_truncated_frames() {
    let mut cursor = Cursor::new(0u32.to_be_bytes().to_vec());
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::ZeroLength)
    ));

    let oversized = u32::try_from(CONTROL_FRAME_MAX_BYTES + 1).expect("fits");
    let mut cursor = Cursor::new(oversized.to_be_bytes().to_vec());
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::FrameTooLarge)
    ));

    let mut cursor = Cursor::new(8u32.to_be_bytes().to_vec());
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::FrameShorterThanHeader { actual: 8 })
    ));

    let mut bytes = 100u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(&[0_u8; 10]);
    let mut cursor = Cursor::new(bytes);
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::TruncatedBody {
            expected: 100,
            received: 10
        })
    ));

    let mut cursor = Cursor::new(vec![0_u8; 2]);
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::TruncatedPrefix { received: 2 })
    ));

    let mut cursor = Cursor::new(Vec::new());
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::Closed)
    ));
}

#[test]
fn framing_refuses_trailing_bytes() {
    let record = record(Sender::Helper, 1, established(lifetime()));
    let mut frame = record.encode().expect("encode");
    frame.push(0);
    let mut bytes = u32::try_from(frame.len())
        .expect("fits")
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(&frame);

    let mut cursor = Cursor::new(bytes);
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::Codec(ControlError::TrailingBytes { .. }))
    ));
}

#[test]
fn framing_refuses_a_reserved_kind() {
    let record = record(Sender::Helper, 1, established(lifetime()));
    let mut frame = record.encode().expect("encode");
    frame[6..8].copy_from_slice(&9u16.to_be_bytes());
    let mut bytes = u32::try_from(frame.len())
        .expect("fits")
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(&frame);

    let mut cursor = Cursor::new(bytes);
    assert!(matches!(
        framing::receive_record(&mut cursor),
        Err(ControlIoError::Codec(ControlError::ReservedKind))
    ));
}
