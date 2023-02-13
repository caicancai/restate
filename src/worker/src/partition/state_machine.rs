use bytes::Bytes;
use common::types::{EntryIndex, Response, ServiceId, ServiceInvocation, ServiceInvocationId};
use journal::raw::{RawEntry, RawEntryCodec};
use journal::{
    BackgroundInvokeEntry, ClearStateEntry, CompleteAwakeableEntry, Completion, CompletionResult,
    Entry, EntryType, InvokeEntry, InvokeRequest, JournalRevision, SetStateEntry, SleepEntry,
};
use std::fmt::Debug;
use std::marker::PhantomData;
use tracing::debug;

pub(super) use crate::partition::effects::Effects;
use crate::partition::effects::OutboxMessage;
use crate::partition::InvocationStatus;

#[derive(Debug, thiserror::Error)]
pub enum Error<S, C> {
    #[error("failed to read from state reader")]
    State(S),
    #[error("failed to deserialize state")]
    Codec(C),
}

#[derive(Debug)]
pub(crate) enum Command {
    Invoker(invoker::OutputEffect),
    Timer {
        service_invocation_id: ServiceInvocationId,
        entry_index: EntryIndex,
        timestamp: u64,
    },
    OutboxTruncation(u64),
    Invocation(ServiceInvocation),
    Response(Response),
}

pub(super) struct JournalStatus {
    pub(super) revision: JournalRevision,
    pub(super) length: u32,
}

pub(super) trait StateReader {
    type Error;

    fn get_invocation_status(
        &self,
        service_id: &ServiceId,
    ) -> Result<InvocationStatus, Self::Error>;

    fn peek_inbox(
        &self,
        service_id: &ServiceId,
    ) -> Result<Option<(u64, ServiceInvocation)>, Self::Error>;

    fn get_journal_status(&self, service_id: &ServiceId) -> Result<JournalStatus, Self::Error>;
}

#[derive(Debug, Default)]
pub(super) struct StateMachine<Codec> {
    // initialized from persistent storage
    inbox_seq_number: u64,
    outbox_seq_number: u64,

    _codec: PhantomData<Codec>,
}

/// Unwraps the inner value of a given enum variant.
///
/// # Panics
/// If the enum variant does not match the given enum variant, it panics.
///
/// # Example
///
/// ```
/// enum Enum {
///     A(u64),
///     B(String),
/// }
///
/// let variant = Enum::A(42);
///
/// let inner = enum_inner!(variant, Enum::A);
/// assert_eq!(inner, 42);
/// ```
///
/// ## Expansion
///
/// The given example will expand to:
///
/// ```
/// enum Enum {
///     A(u64),
///     B(String),
/// }
///
/// let variant = Enum::A(42);
///
/// let inner = match variant {
///     Enum::A(inner) => inner,
///     _ => panic!()
/// };
/// ```
macro_rules! enum_inner {
    ($ty:expr, $variant:path) => {
        match $ty {
            $variant(inner) => inner,
            _ => panic!("Unexpected enum type"),
        }
    };
}

impl<Codec> StateMachine<Codec>
where
    Codec: RawEntryCodec,
    Codec::Error: Debug,
{
    /// Applies the given command and returns effects via the provided effects struct
    ///
    /// We pass in the effects message as a mutable borrow to be able to reuse it across
    /// invocations of this methods which lies on the hot path.
    pub(super) fn on_apply<State: StateReader>(
        &mut self,
        command: Command,
        effects: &mut Effects,
        state: &State,
    ) -> Result<(), Error<State::Error, Codec::Error>> {
        debug!(?command, "Apply");

        match command {
            Command::Invocation(service_invocation) => {
                let status = state
                    .get_invocation_status(&service_invocation.id.service_id)
                    .map_err(Error::State)?;

                if status == InvocationStatus::Free {
                    effects.invoke_service(service_invocation);
                } else {
                    effects.enqueue_into_inbox(self.inbox_seq_number, service_invocation);
                    self.inbox_seq_number += 1;
                }
            }
            Command::Response(Response {
                id,
                entry_index,
                result,
            }) => {
                let completion = Completion {
                    entry_index,
                    result: result.into(),
                };

                Self::handle_completion(id, completion, state, effects).map_err(Error::State)?;
            }
            Command::Invoker(invoker::OutputEffect {
                service_invocation_id,
                kind,
            }) => {
                let status = state
                    .get_invocation_status(&service_invocation_id.service_id)
                    .map_err(Error::State)?;

                debug_assert!(
                    matches!(
                        status,
                        InvocationStatus::Invoked(invocation_id) if service_invocation_id.invocation_id == invocation_id
                    ),
                    "Expect to only receive invoker messages when being invoked"
                );

                match kind {
                    invoker::Kind::JournalEntry { entry_index, entry } => {
                        let journal_length = state
                            .get_journal_status(&service_invocation_id.service_id)
                            .map_err(Error::State)?
                            .length;

                        debug_assert_eq!(
                            entry_index,
                            journal_length + 1,
                            "Expect to receive next journal entry"
                        );

                        match entry.header.ty {
                            EntryType::Invoke => {
                                let InvokeEntry { request, .. } = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::Invoke
                                );

                                let service_invocation = Self::create_service_invocation(
                                    request,
                                    Some((service_invocation_id.clone(), entry_index)),
                                );
                                self.send_message(
                                    OutboxMessage::Invocation(service_invocation),
                                    effects,
                                );
                            }
                            EntryType::BackgroundInvoke => {
                                let BackgroundInvokeEntry(request) = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::BackgroundInvoke
                                );

                                let service_invocation =
                                    Self::create_service_invocation(request, None);
                                self.send_message(
                                    OutboxMessage::Invocation(service_invocation),
                                    effects,
                                );
                            }
                            EntryType::CompleteAwakeable => {
                                let entry = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::CompleteAwakeable
                                );

                                let response = Self::create_response_for_awakeable_entry(entry);
                                self.send_message(OutboxMessage::Response(response), effects);
                            }
                            EntryType::SetState => {
                                let SetStateEntry { key, value } = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::SetState
                                );

                                effects.set_state(
                                    service_invocation_id.service_id.clone(),
                                    key,
                                    value,
                                );
                            }
                            EntryType::ClearState => {
                                let ClearStateEntry { key } = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::ClearState
                                );
                                effects.clear_state(service_invocation_id.service_id.clone(), key);
                            }
                            EntryType::Sleep => {
                                let SleepEntry { wake_up_time, .. } = enum_inner!(
                                    Self::deserialize(&entry).map_err(Error::Codec)?,
                                    Entry::Sleep
                                );
                                effects.register_timer(
                                    wake_up_time as u64,
                                    service_invocation_id.clone(),
                                    entry_index,
                                );
                            }

                            // nothing to do
                            EntryType::GetState => {}
                            EntryType::Custom(_) => {}
                            EntryType::PollInputStream => {}
                            EntryType::OutputStream => {}

                            // special handling because we can have a completion present
                            EntryType::Awakeable => {
                                effects.append_awakeable_entry(
                                    service_invocation_id,
                                    entry_index,
                                    entry,
                                );
                                return Ok(());
                            }
                        }

                        effects.append_journal_entry(service_invocation_id, entry_index, entry);
                    }
                    invoker::Kind::Suspended {
                        journal_revision: expected_journal_revision,
                    } => {
                        let actual_journal_revision = state
                            .get_journal_status(&service_invocation_id.service_id)
                            .map_err(Error::State)?
                            .revision;

                        if actual_journal_revision > expected_journal_revision {
                            effects.resume_service(service_invocation_id);
                        } else {
                            effects.suspend_service(service_invocation_id);
                        }
                    }
                    invoker::Kind::End => {
                        self.complete_invocation(
                            service_invocation_id,
                            CompletionResult::Success(Bytes::new()),
                            state,
                            effects,
                        ).map_err(Error::State)?;
                    }
                    invoker::Kind::Failed { error } => {
                        self.complete_invocation(
                            service_invocation_id,
                            CompletionResult::Failure(502, error.to_string().into()),
                            state,
                            effects,
                        ).map_err(Error::State)?;
                    }
                }
            }
            Command::OutboxTruncation(index) => {
                effects.truncate_outbox(index);
            }
            Command::Timer {
                service_invocation_id,
                entry_index,
                timestamp: wake_up_time,
            } => {
                effects.delete_timer(
                    wake_up_time,
                    service_invocation_id.service_id.clone(),
                    entry_index,
                );

                let completion = Completion {
                    entry_index,
                    result: CompletionResult::Success(Bytes::new()),
                };
                Self::handle_completion(service_invocation_id, completion, state, effects).map_err(Error::State)?;
            }
        }

        Ok(())
    }

    fn handle_completion<State: StateReader>(
        service_invocation_id: ServiceInvocationId,
        completion: Completion,
        state: &State,
        effects: &mut Effects,
    ) -> Result<(), State::Error> {
        let status = state.get_invocation_status(&service_invocation_id.service_id)?;

        match status {
            InvocationStatus::Invoked(invocation_id) => {
                if invocation_id == service_invocation_id.invocation_id {
                    effects.store_and_forward_completion(service_invocation_id, completion);
                } else {
                    debug!(
                        ?completion,
                        "Ignoring completion for invocation that is no longer running."
                    );
                }
            }
            InvocationStatus::Suspended(invocation_id) => {
                if invocation_id == service_invocation_id.invocation_id {
                    effects.resume_service(service_invocation_id.clone());
                    effects.store_completion(service_invocation_id, completion);
                } else {
                    debug!(
                        ?completion,
                        "Ignoring completion for invocation that is no longer running."
                    );
                }
            }
            InvocationStatus::Free => {
                debug!(
                    ?completion,
                    "Ignoring completion for invocation that is no longer running."
                )
            }
        }

        Ok(())
    }

    fn complete_invocation<State: StateReader>(
        &mut self,
        service_invocation_id: ServiceInvocationId,
        completion_result: CompletionResult,
        state: &State,
        effects: &mut Effects,
    ) -> Result<(), State::Error> {
        effects.drop_journal(service_invocation_id.service_id.clone());

        if let Some((inbox_sequence_number, service_invocation)) =
            state.peek_inbox(&service_invocation_id.service_id)?
        {
            effects.pop_inbox(service_invocation_id.service_id, inbox_sequence_number);
            effects.invoke_service(service_invocation);
        } else {
            effects.free_service(service_invocation_id.service_id);
        }

        let response = Self::create_response(completion_result);

        self.send_message(OutboxMessage::Response(response), effects);

        Ok(())
    }

    fn send_message(&mut self, message: OutboxMessage, effects: &mut Effects) {
        effects.enqueue_into_outbox(self.outbox_seq_number, message);
        self.outbox_seq_number += 1;
    }

    fn create_service_invocation(
        invoke_request: InvokeRequest,
        response_target: Option<(ServiceInvocationId, EntryIndex)>,
    ) -> ServiceInvocation {
        // We might want to create the service invocation when receiving the journal entry from
        // service endpoint. That way we can fail it fast if the service cannot be resolved.
        unimplemented!()
    }

    fn create_response_for_awakeable_entry(entry: CompleteAwakeableEntry) -> Response {
        unimplemented!()
    }

    fn create_response(result: CompletionResult) -> Response {
        unimplemented!()
    }

    fn deserialize(raw_entry: &RawEntry) -> Result<Entry, Codec::Error> {
        Codec::deserialize(raw_entry)
    }
}
