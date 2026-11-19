use std::sync::atomic::AtomicBool;

use gix_error::{ResultExt, bail, message};
use gix_hash::ObjectId;
use gix_ref::transaction::{PreviousValue, RefEdit};
#[cfg(feature = "async-network-client")]
use gix_transport::client::async_io::Transport;
#[cfg(feature = "blocking-network-client")]
use gix_transport::client::blocking_io::Transport;
use gix_utils::progress::NestedProgress;

use crate::{
    Repository, Result,
    remote::push::{Outcome, Prepare, Status, Update, pack, plan},
};

impl<T> Prepare<'_, '_, '_, T>
where
    T: Transport,
{
    /// Send all [pending](Status::Pending) updates to the remote along with the objects it needs to apply them,
    /// and update the remote-tracking references of those which the remote applied, or which already had the desired value.
    /// Nothing is sent and no reference is changed in [dry-run mode](super::Options::dry_run).
    ///
    /// `progress` receives information about creating the pack and the progress of the remote,
    /// and `should_interrupt` stops sending the pack if set.
    ///
    /// Note that it's not an error if the remote rejects updates, which is visible in their [status](Update::status).
    #[gix_protocol::bisync::bisync]
    pub async fn send<P>(mut self, progress: P, should_interrupt: &AtomicBool) -> Result<Outcome>
    where
        P: NestedProgress,
        P::SubProgress: 'static,
    {
        let _span = gix_trace::coarse!("remote::push::Prepare::send()");
        let repo = self.con.remote.repo;
        let mut remote_messages = Vec::new();
        if self.options.dry_run {
            return Ok(Outcome {
                handshake: self.handshake,
                updates: self.updates,
                remote_messages,
            });
        }

        let pending: Vec<usize> = (0..self.updates.len())
            .filter(|idx| self.updates[*idx].status == Status::Pending)
            .collect();
        if !pending.is_empty() {
            let null = ObjectId::null(repo.object_hash());
            let commands: Vec<_> = pending
                .iter()
                .map(|idx| {
                    let update = &self.updates[*idx];
                    gix_protocol::push::Command {
                        ref_name: update.remote_ref.as_bstr().into(),
                        old_id: update.remote_id.unwrap_or(null),
                        new_id: update.new_id.unwrap_or(null),
                    }
                })
                .collect();
            let tips: Vec<ObjectId> = pending.iter().filter_map(|idx| self.updates[*idx].new_id).collect();
            let remote_tips: Vec<ObjectId> = self
                .handshake
                .refs
                .iter()
                .flatten()
                .filter_map(plan::advertised_id)
                .map(|(_name, id)| id)
                .collect();
            let outcome = gix_protocol::push(
                &commands,
                |out, progress, should_interrupt| {
                    pack::write(repo, &tips, &remote_tips, out, progress, should_interrupt)
                },
                progress,
                should_interrupt,
                gix_protocol::push::Context {
                    handshake: &self.handshake,
                    transport: &mut self.con.transport.inner,
                    user_agent: repo.config.user_agent_tuple(),
                    trace_packetlines: self.con.trace,
                },
                gix_protocol::push::Options {
                    atomic: self.options.atomic,
                    push_options: std::mem::take(&mut self.options.push_options),
                    quiet: false,
                },
            )
            .await?;
            if let Some(reason) = outcome.report.unpack_error {
                bail!(message!("The remote couldn't unpack the objects we sent: {reason}"));
            }
            for idx in pending {
                let update = &mut self.updates[idx];
                update.status = match outcome
                    .report
                    .refs
                    .iter()
                    .find(|status| status.ref_name == update.remote_ref.as_bstr())
                {
                    Some(status) => match &status.rejection {
                        None => Status::Ok,
                        Some(reason) => Status::RemoteRejected(reason.clone()),
                    },
                    None => Status::RemoteRejected("the remote didn't report a status".into()),
                };
            }
            remote_messages = outcome.remote_messages;
        }
        update_tracking_refs(repo, &self.updates)?;
        Ok(Outcome {
            handshake: self.handshake,
            updates: self.updates,
            remote_messages,
        })
    }
}

/// Set the remote-tracking references of all `updates` that the remote has now to the value it has.
fn update_tracking_refs(repo: &Repository, updates: &[Update]) -> Result<()> {
    let mut edits = Vec::new();
    for update in updates {
        let (Status::Ok | Status::UpToDate, Some(name)) = (&update.status, &update.tracking_ref) else {
            continue;
        };
        let current = repo
            .try_find_reference(name)?
            .and_then(|reference| reference.target().try_id().map(ToOwned::to_owned));
        if current == update.new_id {
            continue;
        }
        edits.push(match update.new_id {
            Some(id) => RefEdit::update(name.clone(), id, PreviousValue::Any, "update by push"),
            None => RefEdit::delete(name.clone(), PreviousValue::Any),
        });
    }
    if edits.is_empty() {
        return Ok(());
    }
    let mut repo = repo.clone();
    repo.committer_or_set_generic_fallback()?;
    repo.edit_references(edits)
        .or_raise(|| message("Could not update remote-tracking references after pushing"))?;
    Ok(())
}
