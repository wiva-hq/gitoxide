//! Update references on a remote by talking to `git-receive-pack`.
//!
//! ### Order of operations
//!
//! * [handshake](crate::handshake()) with [`Service::ReceivePack`](crate::transport::Service::ReceivePack), which yields the
//!   references the remote advertises along with its capabilities.
//! * Compute the [commands](crate::push::Command) to send from the advertised references, and the objects the remote lacks.
//! * [push](crate::push()) the commands along with a pack of these objects, and receive a [report](crate::push::Report) with the
//!   status of each reference.
//!
//! The remote only supports protocol versions 0 and 1 for pushing, so references are always part of the handshake.
//! The pack is produced by the caller, and with each reference update being checked by the remote,
//! this module doesn't try to determine if an update is a fast-forward.
use bstr::BString;

///
pub mod report;
pub use report::Report;

#[cfg(any(feature = "blocking-client", feature = "async-client"))]
pub(crate) mod function;

/// A request to change a single reference on the remote.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Command {
    /// The full name of the reference on the remote, like `refs/heads/main`.
    pub ref_name: BString,
    /// The value the remote reference must have for the update to happen, or the null id if it must not exist.
    pub old_id: gix_hash::ObjectId,
    /// The value to set the remote reference to, or the null id to delete it.
    pub new_id: gix_hash::ObjectId,
}

impl Command {
    /// Return `true` if this command deletes the remote reference.
    pub fn is_delete(&self) -> bool {
        self.new_id.is_null()
    }
}

/// Options for use in [`push()`](crate::push()).
#[derive(Default, Debug, Clone)]
pub struct Options {
    /// If `true`, the remote applies either all commands or none of them.
    pub atomic: bool,
    /// Strings to pass to the hooks of the remote, as `git push --push-option` does.
    pub push_options: Vec<BString>,
    /// If `true`, ask the remote to not send progress information.
    pub quiet: bool,
}

/// The arguments for [`push()`](crate::push()) that are needed to communicate with the remote.
#[cfg(feature = "handshake")]
pub struct Context<'a, T> {
    /// The outcome of the handshake with the remote, performed for [`Service::ReceivePack`](crate::transport::Service::ReceivePack).
    pub handshake: &'a crate::Handshake,
    /// The transport over which the handshake was performed.
    pub transport: &'a mut T,
    /// How to self-identify, sent if the remote advertises the `agent` capability.
    ///
    /// This could be read from the `gitoxide.userAgent` configuration variable.
    pub user_agent: crate::command::Feature,
    /// If `true`, output all packetlines using the `gix-trace` machinery.
    pub trace_packetlines: bool,
}

/// The result of [`push()`](crate::push()).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Outcome {
    /// What the remote did with each command.
    pub report: Report,
    /// Lines the remote sent for display that aren't progress information, like the output of its hooks.
    pub remote_messages: Vec<BString>,
}
