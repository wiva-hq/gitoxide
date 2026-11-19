#[cfg(feature = "async-network-client")]
use gix_transport::client::async_io::Transport;
#[cfg(feature = "blocking-network-client")]
use gix_transport::client::blocking_io::Transport;

use gix_utils::progress::Progress;

use crate::{
    Remote, Result,
    bstr::BStr,
    error::{ResultExt, message},
    types::RemoteDetached,
};

/// A function that performs a given credential action, trying to obtain credentials for an operation that needs it.
pub type AuthenticateFn<'a> =
    Box<dyn FnMut(gix_credentials::helper::Action) -> Result<Option<gix_credentials::protocol::Outcome>> + 'a>;

/// A type to represent an ongoing connection to a remote host, typically with the connection already established.
///
/// It can be used to perform a variety of operations with the remote without worrying about protocol details,
/// much like a remote procedure call.
pub struct Connection<'remote, 'auth, 'repo, T>
where
    T: Transport,
{
    pub(crate) remote: &'remote Remote<'repo>,
    pub(crate) authenticate: Option<AuthenticateFn<'auth>>,
    pub(crate) transport_options: Option<Box<dyn std::any::Any>>,
    pub(crate) transport: gix_protocol::SendFlushOnDrop<T>,
    pub(crate) handshake: Option<gix_protocol::Handshake>,
    pub(crate) trace: bool,
}

/// Like [`Connection`], but without borrowing its remote or repository.
pub(crate) struct ConnectionDetached<'a, T>
where
    T: Transport,
{
    pub(crate) remote: RemoteDetached,
    pub(crate) authenticate: Option<AuthenticateFn<'a>>,
    pub(crate) transport_options: Option<Box<dyn std::any::Any>>,
    pub(crate) transport: gix_protocol::SendFlushOnDrop<T>,
    pub(crate) handshake: Option<gix_protocol::Handshake>,
    pub(crate) trace: bool,
}

mod access;

///
pub mod ref_map;

///
pub mod fetch;

///
pub mod push;

/// Configure `transport` with `transport_options`, which are set from the configuration of `repo` for the remote named
/// `remote_name` if unset, and perform the handshake for `service`.
/// Authenticate with `authenticate`, or with the credentials configured in `repo` if `None`.
#[gix_protocol::bisync::bisync]
#[expect(clippy::too_many_arguments)]
pub(crate) async fn configure_and_handshake<T>(
    transport: &mut T,
    authenticate: Option<&mut AuthenticateFn<'_>>,
    transport_options: &mut Option<Box<dyn std::any::Any>>,
    repo: &crate::Repository,
    remote_name: Option<&BStr>,
    service: gix_transport::Service,
    handshake_parameters: Vec<(String, Option<String>)>,
    progress: &mut impl Progress,
) -> Result<gix_protocol::Handshake>
where
    T: Transport,
{
    let mut credentials_storage;
    let authenticate = match authenticate {
        Some(f) => f,
        None => {
            credentials_storage = access::configured_credentials_for_current_url(repo.clone());
            &mut credentials_storage
        }
    };
    if transport_options.is_none() {
        let url = transport.to_url();
        *transport_options = repo
            .transport_options(url.as_ref(), remote_name)
            .or_raise(|| message!("Failed to configure the transport before connecting to {url:?}"))?;
    }
    if let Some(config) = transport_options.as_ref() {
        transport
            .configure(&**config)
            .or_raise(|| message("Failed to configure the transport layer"))?;
    }
    gix_protocol::handshake(transport, service, authenticate, handshake_parameters, progress).await
}
