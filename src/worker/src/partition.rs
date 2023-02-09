use common::types::{
    EntryIndex, InvocationId, LeaderEpoch, PartitionId, PeerId, ServiceId, ServiceInvocation,
    ServiceInvocationId,
};
use futures::{stream, Sink, SinkExt, Stream, StreamExt};
use invoker::{InvokeInputJournal, InvokerInputSender};
use journal::raw::{RawEntry, RawEntryCodec};
use journal::{CompletionResult, Entry, JournalRevision};
use service_protocol::codec::ProtobufRawEntryCodec;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::ops::DerefMut;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tracing::{debug, info};

mod effects;
mod state_machine;

use crate::partition::effects::{ActuatorMessage, Collector, Effect, Effects, OutboxMessage};
pub(crate) use state_machine::Command;
use storage_api::WriteTransaction;
use storage_rocksdb::RocksDBWriteTransaction;

type StateMachine = state_machine::StateMachine<ProtobufRawEntryCodec>;

#[derive(Debug)]
pub(super) struct PartitionProcessor<
    CmdStream,
    ProposalSink,
    RawEntryCodec,
    InvokerInputSender,
    Storage,
> {
    peer_id: PeerId,
    partition_id: PartitionId,

    storage: Storage,

    command_stream: CmdStream,
    proposal_sink: ProposalSink,

    invoker_tx: InvokerInputSender,

    state_machine: StateMachine,

    _entry_codec: PhantomData<RawEntryCodec>,
}

#[derive(Debug)]
pub(super) struct RocksDBJournalReader;

impl invoker::JournalReader for RocksDBJournalReader {
    type JournalStream = stream::Empty<journal::raw::RawEntry>;
    type Error = ();
    type Future = futures::future::Pending<
        Result<(invoker::JournalMetadata, Self::JournalStream), Self::Error>,
    >;

    fn read_journal(&self, _sid: &ServiceInvocationId) -> Self::Future {
        // TODO implement this
        unimplemented!("Implement JournalReader")
    }
}

impl<CmdStream, ProposalSink, RawEntryCodec, InvokerInputSender, Storage>
    PartitionProcessor<CmdStream, ProposalSink, RawEntryCodec, InvokerInputSender, Storage>
where
    CmdStream: Stream<Item = consensus::Command<Command>>,
    ProposalSink: Sink<Command>,
    RawEntryCodec: journal::raw::RawEntryCodec + Debug,
    RawEntryCodec::Error: Debug,
    InvokerInputSender: invoker::InvokerInputSender + Clone,
    InvokerInputSender::Error: Debug,
    Storage: storage_api::Storage,
{
    pub(super) fn new(
        peer_id: PeerId,
        partition_id: PartitionId,
        command_stream: CmdStream,
        proposal_sink: ProposalSink,
        invoker_tx: InvokerInputSender,
        storage: Storage,
    ) -> Self {
        Self {
            peer_id,
            partition_id,
            command_stream,
            proposal_sink,
            invoker_tx,
            state_machine: StateMachine::default(),
            storage,
            _entry_codec: Default::default(),
        }
    }

    pub(super) async fn run(self) {
        let PartitionProcessor {
            peer_id,
            partition_id,
            command_stream,
            mut state_machine,
            invoker_tx,
            mut storage,
            proposal_sink,
            ..
        } = self;
        tokio::pin!(command_stream);
        tokio::pin!(proposal_sink);

        let mut effects = Effects::default();

        let mut leadership_state = LeadershipState::follower(partition_id, invoker_tx);

        loop {
            let mut actuator_stream = leadership_state.actuator_stream();

            tokio::select! {
                command = command_stream.next() => {
                    if let Some(command) = command {
                        match command {
                            consensus::Command::Apply(fsm_command) => {
                                effects.clear();
                                state_machine.on_apply(fsm_command, &mut effects, &storage).expect("State machine application must not fail");

                                let mut message_collector = leadership_state.message_collector();

                                let storage_helper = StorageWriterHelper::new(storage.transaction());
                                effects::interpret_effects::<<Storage as storage_api::Storage>::WriteTransactionType<'_>, MessageCollector<'_, InvokerInputSender>, RawEntryCodec>(&mut effects, &storage_helper, &mut message_collector).expect("Effect interpreter must not fail");
                                storage_helper.into_transaction().commit();

                                message_collector.send().await.expect("Actuator message sending must not fail");
                            }
                            consensus::Command::BecomeLeader(leader_epoch) => {
                                info!(%peer_id, %partition_id, %leader_epoch, "Become leader.");
                                leadership_state = leadership_state.become_leader(leader_epoch, &storage).await;
                            }
                            consensus::Command::BecomeFollower => {
                                info!(%peer_id, %partition_id, "Become follower.");
                                leadership_state = leadership_state.become_follower().await;
                            },
                            consensus::Command::ApplySnapshot => {
                                unimplemented!("Not supported yet.");
                            }
                            consensus::Command::CreateSnapshot => {
                                unimplemented!("Not supported yet.");
                            }
                        }
                    } else {
                        break;
                    }
                },
                actuator_message = actuator_stream.next() => {
                    let actuator_message = actuator_message.expect("Actuator stream must be open");
                    Self::propose_actuator_message(actuator_message, &mut proposal_sink).await;
                }
            }
        }

        debug!(%peer_id, %partition_id, "Shutting partition processor down.");
        leadership_state.become_follower().await;
    }

    async fn propose_actuator_message(
        actuator_message: invoker::OutputEffect,
        proposal_sink: &mut Pin<&mut ProposalSink>,
    ) {
        // Err only if the consensus module is shutting down
        let _ = proposal_sink.send(Command::Invoker(actuator_message)).await;
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum InvocationStatus {
    Invoked(InvocationId),
    Suspended(InvocationId),
    Free,
}

struct StorageWriterHelper<Txn> {
    transaction: Txn,
}

impl<Txn> StorageWriterHelper<Txn> {
    fn new(transaction: Txn) -> Self {
        Self { transaction }
    }

    fn into_transaction(self) -> Txn {
        self.transaction
    }

    fn write_invocation_status(&self, service_id: &ServiceId, status: &InvocationStatus) {
        unimplemented!()
    }

    fn create_journal(&self, service_id: &ServiceId, method_name: impl AsRef<str>) {
        unimplemented!()
    }

    fn store_journal_entry(
        &self,
        service_id: &ServiceId,
        entry_index: EntryIndex,
        raw_entry: RawEntry,
    ) -> JournalRevision {
        unimplemented!()
    }

    fn store_completion_result(
        &self,
        service_id: &ServiceId,
        entry_index: EntryIndex,
        completion_result: CompletionResult,
    ) {
        unimplemented!()
    }

    fn enqueue_into_inbox(&self, seq_number: u64, service_invocation: ServiceInvocation) {
        unimplemented!()
    }

    fn enqueue_into_outbox(&self, seq_number: u64, message: OutboxMessage) {
        unimplemented!()
    }

    fn store_inbox_seq_number(&self, seq_number: u64) {
        unimplemented!()
    }

    fn store_outbox_seq_number(&self, seq_number: u64) {
        unimplemented!()
    }

    fn write_state(&self, service_id: &ServiceId, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) {
        unimplemented!()
    }

    fn clear_state(&self, service_id: &ServiceId, key: impl AsRef<[u8]>) {
        unimplemented!()
    }

    fn store_timer(
        &self,
        service_invocation_id: &ServiceInvocationId,
        wake_up_time: u64,
        entry_index: EntryIndex,
    ) {
        unimplemented!()
    }

    fn delete_timer(&self, service_id: &ServiceId, wake_up_time: u64, entry_index: EntryIndex) {
        unimplemented!()
    }

    fn read_completion_result(
        &self,
        service_id: &ServiceId,
        entry_index: EntryIndex,
    ) -> Option<CompletionResult> {
        unimplemented!()
    }

    fn read_journal_entry(
        &self,
        service_id: &ServiceId,
        entry_index: EntryIndex,
    ) -> Option<RawEntry> {
        unimplemented!()
    }

    fn truncate_outbox(&self, outbox_sequence_number: u64) {
        unimplemented!()
    }

    fn truncate_inbox(&self, service_id: &ServiceId, inbox_sequence_number: u64) {
        unimplemented!()
    }

    fn drop_journal(&self, service_id: &ServiceId) {
        unimplemented!()
    }
}

enum MessageCollector<'a, I> {
    Active {
        leadership_state: &'a mut LeadershipState<I>,
        messages: Vec<ActuatorMessage>,
    },
    Inactive,
}

impl<'a, I> MessageCollector<'a, I>
where
    I: InvokerInputSender,
    I::Error: Debug,
{
    async fn send(self) -> Result<(), I::Error> {
        match self {
            MessageCollector::Active {
                leadership_state,
                messages,
            } => leadership_state.send_actuator_messages(messages).await,
            MessageCollector::Inactive => Ok(()),
        }
    }
}

impl<'a, I> Collector for MessageCollector<'a, I> {
    fn collect(&mut self, message: ActuatorMessage) {
        match self {
            MessageCollector::Active { messages, .. } => {
                messages.push(message);
            }
            MessageCollector::Inactive => {}
        }
    }
}

enum LeadershipState<InvokerInputSender> {
    Follower {
        partition_id: PartitionId,
        invoker_tx: InvokerInputSender,
    },

    Leader {
        partition_id: PartitionId,
        leader_epoch: LeaderEpoch,
        invoker_rx: mpsc::Receiver<invoker::OutputEffect>,
        invoker_tx: InvokerInputSender,
    },
}

impl<InvokerInputSender> LeadershipState<InvokerInputSender>
where
    InvokerInputSender: invoker::InvokerInputSender,
    InvokerInputSender::Error: Debug,
{
    fn follower(partition_id: PartitionId, invoker_tx: InvokerInputSender) -> Self {
        Self::Follower {
            partition_id,
            invoker_tx,
        }
    }

    async fn become_leader<S: storage_api::Storage>(
        self,
        leader_epoch: LeaderEpoch,
        storage: &S,
    ) -> Self {
        if let LeadershipState::Follower { .. } = self {
            self.unchecked_become_leader(leader_epoch, storage).await
        } else {
            self.become_follower()
                .await
                .unchecked_become_leader(leader_epoch, storage)
                .await
        }
    }

    async fn unchecked_become_leader<S: storage_api::Storage>(
        self,
        leader_epoch: LeaderEpoch,
        storage: &S,
    ) -> Self {
        if let LeadershipState::Follower {
            partition_id,
            mut invoker_tx,
            ..
        } = self
        {
            let (tx, rx) = mpsc::channel(1);

            invoker_tx
                .register_partition((partition_id, leader_epoch), tx)
                .await
                .expect("Invoker should be running");

            let mut invoked_invocations = Self::invoked_invocations(storage);

            while let Some(service_invocation_id) = invoked_invocations.next().await {
                invoker_tx
                    .invoke(
                        (partition_id, leader_epoch),
                        service_invocation_id,
                        InvokeInputJournal::NoCachedJournal,
                    )
                    .await
                    .expect("Invoker should be running");
            }

            LeadershipState::Leader {
                partition_id,
                leader_epoch,
                invoker_rx: rx,
                invoker_tx,
            }
        } else {
            unreachable!("This method should only be called if I am a follower!");
        }
    }

    async fn become_follower(self) -> Self {
        if let LeadershipState::Leader {
            partition_id,
            leader_epoch,
            mut invoker_tx,
            ..
        } = self
        {
            invoker_tx
                .abort_all_partition((partition_id, leader_epoch))
                .await
                .expect("Invoker should be running");
            Self::follower(partition_id, invoker_tx)
        } else {
            self
        }
    }

    fn message_collector(&mut self) -> MessageCollector<'_, InvokerInputSender> {
        match self {
            LeadershipState::Follower { .. } => MessageCollector::Inactive,
            LeadershipState::Leader { .. } => MessageCollector::Active {
                leadership_state: self,
                messages: Vec::new(),
            },
        }
    }

    fn actuator_stream(&mut self) -> ActuatorStream<'_, InvokerInputSender> {
        ActuatorStream { inner: self }
    }

    async fn send_actuator_messages(
        &mut self,
        messages: Vec<ActuatorMessage>,
    ) -> Result<(), InvokerInputSender::Error> {
        match self {
            LeadershipState::Leader {
                partition_id,
                leader_epoch,
                invoker_tx,
                ..
            } => {
                for message in messages.into_iter() {
                    match message {
                        ActuatorMessage::Invoke(service_invocation_id) => {
                            invoker_tx
                                .invoke(
                                    (partition_id.clone(), leader_epoch.clone()),
                                    service_invocation_id,
                                    InvokeInputJournal::NoCachedJournal,
                                )
                                .await?
                        }
                        ActuatorMessage::NewOutboxMessage(seq_number) => {
                            // ignore for the time being
                        }
                        ActuatorMessage::RegisterTimer { .. } => {
                            // we don't have a timer service yet :-(
                        }
                        ActuatorMessage::AckStoredEntry {
                            service_invocation_id,
                            journal_revision,
                            entry_index,
                        } => {
                            invoker_tx
                                .notify_stored_entry_ack(
                                    (partition_id.clone(), leader_epoch.clone()),
                                    service_invocation_id,
                                    journal_revision,
                                    entry_index,
                                )
                                .await?;
                        }
                        ActuatorMessage::ForwardCompletion {
                            service_invocation_id,
                            journal_revision,
                            completion,
                        } => {
                            invoker_tx
                                .notify_completion(
                                    (partition_id.clone(), leader_epoch.clone()),
                                    service_invocation_id,
                                    journal_revision,
                                    completion,
                                )
                                .await?
                        }
                    }
                }
            }
            LeadershipState::Follower { .. } => {
                debug!("Ignore sending actuator messages because I am a follower.");
            }
        }

        Ok(())
    }

    fn invoked_invocations<S>(_storage: &S) -> impl Stream<Item = ServiceInvocationId> {
        stream::empty()
    }
}

struct ActuatorStream<'a, InvokerInputSender> {
    inner: &'a mut LeadershipState<InvokerInputSender>,
}

impl<'a, InvokerInputSender> Stream for ActuatorStream<'a, InvokerInputSender> {
    type Item = invoker::OutputEffect;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.deref_mut().inner {
            LeadershipState::Leader { invoker_rx, .. } => invoker_rx.poll_recv(cx),
            LeadershipState::Follower { .. } => Poll::Pending,
        }
    }
}
