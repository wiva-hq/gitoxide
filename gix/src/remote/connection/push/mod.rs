//! Update references on a remote with local objects, as `git push` does.
//!
//! [Connect](crate::Remote::connect()) to the remote in the [push direction](crate::remote::Direction::Push),
//! [prepare](crate::remote::Connection::prepare_push()) to see which references would change, and [send](crate::remote::push::Prepare::send())
//! the updates.
use gix_error::{OptionExt, bail, unsupported};
use gix_hash::ObjectId;
#[cfg(feature = "async-network-client")]
use gix_transport::client::async_io::Transport;
#[cfg(feature = "blocking-network-client")]
use gix_transport::client::blocking_io::Transport;
use gix_utils::progress::Progress;

use crate::{
    Result,
    bstr::BString,
    remote::{Connection, connection::configure_and_handshake},
};

mod pack;
mod plan;
mod send;

/// Options for [`Connection::prepare_push()`].
#[derive(Default, Debug, Clone)]
pub struct Options {
    /// Push refspecs like `main`, `+refs/heads/a:refs/heads/b`, `:refs/heads/gone` or `refs/tags/*:refs/tags/*` to use
    /// instead of `remote.<name>.push`, or of what `push.default` implies if that isn't set.
    pub refspecs: Vec<gix_refspec::RefSpec>,
    /// Updates that may overwrite the remote reference only if it has an expected value, as `git push --force-with-lease` does.
    pub leases: Vec<Lease>,
    /// If `true`, the remote applies all updates or none of them, and a local rejection prevents sending anything.
    pub atomic: bool,
    /// Strings to pass to the hooks of the remote.
    pub push_options: Vec<BString>,
    /// If `true`, determine all updates without sending them.
    pub dry_run: bool,
    /// Parameters in the form of `(name, optional value)` to add to the handshake.
    pub handshake_parameters: Vec<(String, Option<String>)>,
}

/// A reference on the remote that may be overwritten only if it still has an expected value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The name of the remote reference, either in full like `refs/heads/main` or abbreviated like `main`.
    pub ref_name: BString,
    /// The value the remote reference must have, with the null id meaning that it must not exist.
    /// If `None`, the value of its remote-tracking reference is expected, and the update is rejected if there is none.
    pub expected: Option<ObjectId>,
}

/// An update of a reference on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The local reference that provided `new_id`, if `new_id` wasn't given as revision.
    pub local_ref: Option<gix_ref::FullName>,
    /// The value to set the remote reference to, or `None` to delete it.
    pub new_id: Option<ObjectId>,
    /// The reference on the remote.
    pub remote_ref: gix_ref::FullName,
    /// The value of `remote_ref` as advertised by the remote, or `None` if it doesn't exist.
    pub remote_id: Option<ObjectId>,
    /// The local reference that tracks `remote_ref` as configured by the fetch refspecs of the remote.
    /// It's updated to `new_id` once the remote accepted the update.
    pub tracking_ref: Option<gix_ref::FullName>,
    /// If `true`, `remote_id` may be overwritten even if it's not an ancestor of `new_id`.
    pub force: bool,
    /// What happened to the update.
    pub status: Status,
}

/// The status of an [`Update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The remote reference already has the desired value, nothing is sent.
    UpToDate,
    /// The update was rejected locally, nothing is sent.
    Rejected(Rejection),
    /// The update will be sent, or would have been sent in dry-run mode.
    Pending,
    /// The remote applied the update.
    Ok,
    /// The remote refused the update for the given reason.
    RemoteRejected(BString),
}

/// The reason an update was rejected locally, similar to how `git push` rejects updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The remote value isn't an ancestor of the new value, and the update isn't forced.
    NonFastForward,
    /// The remote value is unknown locally, so it can't be an ancestor of the new value. Fetching it first helps.
    FetchFirst,
    /// The remote value or the new value isn't a commit, so the update must be forced.
    NeedsForce,
    /// A tag exists on the remote, and tags are only overwritten when forced.
    AlreadyExists,
    /// The remote value doesn't match the expectation of a [`Lease`].
    Stale,
    /// The remote doesn't support deleting references.
    DeleteUnsupported,
    /// Another update of an atomic push was rejected.
    AtomicPushFailed,
}

/// The outcome of [`Prepare::send()`].
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The outcome of the handshake with the remote, with the references it advertised.
    pub handshake: gix_protocol::Handshake,
    /// All updates, with their final status.
    pub updates: Vec<Update>,
    /// Lines the remote sent for display that aren't progress, like the output of its hooks.
    pub remote_messages: Vec<BString>,
}

/// The updates computed by [`Connection::prepare_push()`], ready to be [sent](Prepare::send()).
///
/// Dropping it without sending ends the interaction with the remote without changing anything.
pub struct Prepare<'remote, 'auth, 'repo, T>
where
    T: Transport,
{
    con: Connection<'remote, 'auth, 'repo, T>,
    handshake: gix_protocol::Handshake,
    updates: Vec<Update>,
    options: Options,
}

impl<T> Prepare<'_, '_, '_, T>
where
    T: Transport,
{
    /// Return all updates, those which are [pending](Status::Pending) will be sent.
    pub fn updates(&self) -> &[Update] {
        &self.updates
    }

    /// Return the outcome of the handshake, with the references the remote advertised.
    pub fn handshake(&self) -> &gix_protocol::Handshake {
        &self.handshake
    }
}

impl<'remote, 'auth, 'repo, T> Connection<'remote, 'auth, 'repo, T>
where
    T: Transport,
{
    /// Perform a handshake with the remote to learn the values of its references, and determine the updates of
    /// these from the refspecs in `options`, or from the remote's push refspecs, or from `push.default`, in that order.
    ///
    /// Updates are rejected locally like `git push` does, most notably if they aren't fast-forwards and not forced.
    /// Note that the connection should have been made for the [push direction](crate::remote::Direction::Push).
    ///
    /// ### Configuration
    ///
    /// - `push.default` determines what to push if neither `options` nor the remote provide refspecs.
    /// - `gitoxide.userAgent` is read to obtain the application user agent for git servers and for HTTP servers as well.
    #[gix_protocol::bisync::bisync]
    pub async fn prepare_push(
        mut self,
        mut progress: impl Progress,
        options: Options,
    ) -> Result<Prepare<'remote, 'auth, 'repo, T>> {
        let _span = gix_trace::coarse!("remote::Connection::prepare_push()");
        let repo = self.remote.repo;
        let handshake = configure_and_handshake(
            &mut self.transport.inner,
            self.authenticate.as_mut(),
            &mut self.transport_options,
            repo,
            self.remote.name().map(crate::remote::Name::as_bstr),
            gix_transport::Service::ReceivePack,
            options.handshake_parameters.clone(),
            &mut progress,
        )
        .await?;
        let remote_object_format = handshake
            .capabilities
            .capability("object-format")
            .and_then(|c| c.value().map(ToOwned::to_owned))
            .unwrap_or_else(|| "sha1".into());
        if remote_object_format != repo.object_hash().to_string().as_str() {
            bail!(unsupported(format!(
                "Cannot push to a remote that uses {remote_object_format} while the local repository uses {} for object hashes",
                repo.object_hash()
            )));
        }
        let advertised = handshake
            .refs
            .as_deref()
            .ok_or_raise(|| unsupported("The remote didn't advertise its references for pushing"))?;
        let updates = plan::updates(self.remote, advertised, &handshake.capabilities, &options)?;
        Ok(Prepare {
            con: self,
            handshake,
            updates,
            options,
        })
    }
}
