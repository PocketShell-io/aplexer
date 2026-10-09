//! Durable submission reservations shared by initial and deferred pane delivery.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubmissionStatus {
    /// Observed leaving the recipient's composer.
    Submitted,
    /// Written with Enter to a recipient whose input cannot be observed.
    Injected,
    AlreadySubmitted,
    RecipientAcked,
    NotReady,
    DeliveryUncertain,
}

#[derive(Debug, Serialize)]
pub struct SubmissionOutcome {
    pub id: Uuid,
    pub status: SubmissionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl SubmissionOutcome {
    fn new(id: Uuid, status: SubmissionStatus) -> Self {
        Self {
            id,
            status,
            detail: None,
        }
    }

    fn failed(id: Uuid, status: SubmissionStatus, error: anyhow::Error) -> Self {
        Self {
            id,
            status,
            detail: Some(format!("{error:#}")),
        }
    }
}

fn prior_submission(
    mp: &MessagePaths,
    message: &MessageEnvelope,
) -> Result<Option<SubmissionStatus>> {
    if message.delivery == Delivery::Pane {
        return Ok(Some(SubmissionStatus::AlreadySubmitted));
    }
    if let Recipient::Tag {
        session_id: Some(id),
        ..
    } = message.to
    {
        if read_cursor_locked(mp, id)?.is_acked(message.id) {
            return Ok(Some(SubmissionStatus::RecipientAcked));
        }
    }
    if mp
        .msgs_dir
        .join(format!("{}.attempt", message.id))
        .try_exists()?
    {
        return Ok(Some(SubmissionStatus::DeliveryUncertain));
    }
    Ok(None)
}

/// Keep the mailbox lock through bounded transport so ack, GC, and another
/// submit cannot race the reservation. The marker survives process death;
/// absence of a transport response never authorizes another input write.
pub fn submit_message_in(
    mp: &MessagePaths,
    workspace: &Path,
    id: Uuid,
    ready: impl FnOnce(&MessageEnvelope) -> Result<()>,
    submit: impl FnOnce(&MessageEnvelope) -> Result<SubmissionStatus>,
) -> Result<SubmissionOutcome> {
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), false)?;
    let message = read_message_in(mp, workspace, id)?;
    if let Some(status) = prior_submission(mp, &message)? {
        return Ok(SubmissionOutcome::new(id, status));
    }
    if let Err(error) = ready(&message) {
        return Ok(SubmissionOutcome::failed(
            id,
            SubmissionStatus::NotReady,
            error,
        ));
    }
    perform_submission(mp, &message, submit)
}

fn perform_submission(
    mp: &MessagePaths,
    message: &MessageEnvelope,
    submit: impl FnOnce(&MessageEnvelope) -> Result<SubmissionStatus>,
) -> Result<SubmissionOutcome> {
    let id = message.id;
    atomic_write_json(&mp.msgs_dir.join(format!("{id}.attempt")), &now_ms())?;
    let result =
        submit(message).and_then(|status| mark_pane_delivered_locked(mp, message).map(|()| status));
    match result {
        Ok(status) => Ok(SubmissionOutcome::new(id, status)),
        Err(error) => Ok(SubmissionOutcome::failed(
            id,
            SubmissionStatus::DeliveryUncertain,
            error,
        )),
    }
}

/// Called under the mailbox lock: a message and its reservation expire together.
pub(crate) fn remove_message_and_attempt(path: &Path) -> Result<bool> {
    let removed = match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("remove mailbox message {}", path.display()))
        }
    };
    match fs::remove_file(path.with_extension("attempt")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove expired submission reservation"),
    }
    Ok(removed)
}
