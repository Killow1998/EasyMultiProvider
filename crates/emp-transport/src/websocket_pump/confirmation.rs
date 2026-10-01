//! A per-command reply confirms worker execution, separately from queue admission.
use super::PumpCommand;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

pub(super) struct QueuedCommand {
    pub(super) command: PumpCommand,
    reply: Option<(u64, SyncSender<WriteAcknowledgement>)>,
}

struct WriteAcknowledgement {
    id: u64,
    bytes: usize,
    result: Result<(), u16>,
}

impl QueuedCommand {
    pub(super) fn plain(command: PumpCommand) -> Self {
        Self {
            command,
            reply: None,
        }
    }

    pub(super) fn confirmed_text(id: u64, text: String) -> (Self, WriteReceipt) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let receipt = WriteReceipt {
            id,
            bytes: text.len(),
            receiver,
        };
        (
            Self {
                command: PumpCommand::Text(text),
                reply: Some((id, sender)),
            },
            receipt,
        )
    }

    pub(super) fn acknowledge(&self, bytes: usize, result: Result<(), u16>) {
        if let Some((id, reply)) = &self.reply {
            // A dropped requester must never block the socket owner.
            let _ = reply.try_send(WriteAcknowledgement {
                id: *id,
                bytes,
                result,
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationError {
    WorkerStopped,
    InvalidReceipt,
    WriteFailed(u16),
}

/// A successful queue send is admission; this receipt confirms the socket write.
/// It does not claim that the remote application accepted or executed a turn.
/// Consume the terminal result once, then drop the receipt.
pub struct WriteReceipt {
    id: u64,
    bytes: usize,
    receiver: Receiver<WriteAcknowledgement>,
}

impl WriteReceipt {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn try_result(&self) -> Result<bool, ConfirmationError> {
        match self.receiver.try_recv() {
            Ok(reply) if reply.id != self.id || reply.bytes != self.bytes => {
                Err(ConfirmationError::InvalidReceipt)
            }
            Ok(reply) => reply
                .result
                .map(|()| true)
                .map_err(ConfirmationError::WriteFailed),
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => Err(ConfirmationError::WorkerStopped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_rejects_wrong_delivery_and_distinguishes_failure_from_disconnection() {
        for (id, bytes, result, expected) in [
            (7, 4, Ok(()), Ok(true)),
            (8, 4, Ok(()), Err(ConfirmationError::InvalidReceipt)),
            (7, 3, Ok(()), Err(ConfirmationError::InvalidReceipt)),
            (7, 4, Err(502), Err(ConfirmationError::WriteFailed(502))),
        ] {
            let (command, receipt) = QueuedCommand::confirmed_text(7, "text".into());
            assert_eq!(receipt.try_result(), Ok(false));
            command
                .reply
                .as_ref()
                .unwrap()
                .1
                .send(WriteAcknowledgement { id, bytes, result })
                .unwrap();
            assert_eq!(receipt.try_result(), expected);
        }
        let (command, receipt) = QueuedCommand::confirmed_text(7, "text".into());
        drop(command);
        assert_eq!(receipt.try_result(), Err(ConfirmationError::WorkerStopped));
    }
}
