use std::time::Duration;

use gix_lfs::{Endpoint, Operation, client, endpoint::SshCommand};

use crate::{
    Repository, Result,
    bstr::{BStr, BString, ByteSlice},
    error::{ResultExt, message},
    remote::Direction,
};

/// Configuration
impl super::Platform<'_> {
    /// The local object store, at `lfs.storage` relative to the common Git directory, or `<git-common-dir>/lfs`.
    pub fn store(&self) -> Result<gix_lfs::Store> {
        let repo = self.repo;
        let root = match repo
            .config
            .resolved
            .string_filter("lfs.storage", &mut repo.filter_config_section())
            .filter(|path| !path.is_empty())
        {
            Some(path) => repo.common_dir().join(gix_path::from_bstr(path.as_bstr())?),
            None => repo.common_dir().join("lfs"),
        };
        Ok(gix_lfs::Store::at(root))
    }

    /// The name of the remote to use for `operation` if none is given, like `git-lfs` determines it.
    ///
    /// This is the remote of the current branch, then `remote.lfsPushDefault` for uploads, `remote.lfsDefault`, and finally
    /// the default remote of the repository.
    pub fn remote_name(&self, operation: Operation) -> Result<Option<BString>> {
        let repo = self.repo;
        let direction = direction(operation);
        if let Some(head) = repo.head_name()?
            && let Some(name) = repo.branch_remote_name(head.as_ref().shorten(), direction)
        {
            return Ok(Some(name.as_bstr().to_owned()));
        }
        let config = &repo.config.resolved;
        let mut filter = repo.filter_config_section();
        Ok((operation == Operation::Upload)
            .then(|| config.string_filter("remote.lfsPushDefault", &mut filter))
            .flatten()
            .or_else(|| config.string_filter("remote.lfsDefault", &mut filter))
            .or_else(|| repo.remote_default_name(direction)))
    }

    /// Determine the LFS endpoint of the remote named `remote_name` (or the [default one](Self::remote_name())) for
    /// `operation`, or return `None` if there is none, as the remote is local or there is no remote.
    ///
    /// The first of `lfs.pushUrl` and `remote.<name>.lfsPushUrl` for uploads, `lfs.url`, and `remote.<name>.lfsUrl` is used,
    /// before deriving the endpoint from the URL of the remote.
    /// If the repository is fully trusted, these keys are also read from `.lfsconfig` in the worktree (or in the tree of
    /// `HEAD` in bare repositories), with Git configuration taking precedence.
    pub fn endpoint(&self, remote_name: Option<&BStr>, operation: Operation) -> Result<Option<Endpoint>> {
        let remote_name = match remote_name {
            Some(name) => Some(name.to_owned()),
            None => self.remote_name(operation)?,
        };
        let lfsconfig = self.lfsconfig()?;
        let lookup = |subsection: Option<&BStr>, section: &str, key: &str| {
            self.repo
                .config
                .resolved
                .string_filter_by(section, subsection, key, &mut self.repo.filter_config_section())
                .or_else(|| {
                    lfsconfig
                        .as_ref()
                        .and_then(|file| file.string_by(section, subsection, key))
                })
                .filter(|value| !value.is_empty())
        };
        let mut configured = Vec::new();
        if operation == Operation::Upload {
            configured.push(lookup(None, "lfs", "pushurl"));
            if let Some(name) = &remote_name {
                configured.push(lookup(Some(name.as_bstr()), "remote", "lfspushurl"));
            }
        }
        configured.push(lookup(None, "lfs", "url"));
        if let Some(name) = &remote_name {
            configured.push(lookup(Some(name.as_bstr()), "remote", "lfsurl"));
        }
        if let Some(url) = configured.into_iter().flatten().next() {
            return Endpoint::from_configured_url(url.as_bstr()).map(Some);
        }
        let Some(remote_name) = remote_name else {
            return Ok(None);
        };
        let remote = self.repo.find_remote(remote_name.as_bstr())?;
        let Some(url) = remote.url(direction(operation)) else {
            return Ok(None);
        };
        Endpoint::from_remote_url(url)
    }

    /// Read `.lfsconfig` if the repository is trusted, from the worktree, or from `HEAD` if there is no worktree.
    fn lfsconfig(&self) -> Result<Option<gix_config::File>> {
        let repo = self.repo;
        if repo.git_dir_trust() != gix_sec::Trust::Full {
            return Ok(None);
        }
        let bytes = match repo.workdir() {
            Some(workdir) => {
                let path = workdir.join(".lfsconfig");
                match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(err) => {
                        return Err(err).or_raise(|| message!("Could not read '{}'", path.display()));
                    }
                }
            }
            None => {
                let Ok(tree) = repo.head_tree() else {
                    return Ok(None);
                };
                let Some(entry) = tree.find_entry(".lfsconfig") else {
                    return Ok(None);
                };
                repo.find_blob(entry.object_id())?.data.clone()
            }
        };
        gix_config::File::from_bytes_no_includes(&bytes, gix_config::file::Metadata::api(), Default::default())
            .or_raise(|| message("Could not parse '.lfsconfig'"))
            .map(Some)
    }

    /// Return the transfer settings for `endpoint` as configured with `lfs.*` keys.
    pub fn client_options(&self, endpoint: &Endpoint) -> Result<client::Options> {
        let repo = self.repo;
        let config = &repo.config.resolved;
        let mut filter = repo.filter_config_section();
        let mut options = client::Options::default();
        let mut positive = |key: &str| -> Result<Option<u64>> {
            Ok(config
                .integer_filter(key, &mut filter)?
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| *value > 0))
        };
        if let Some(value) = positive("lfs.concurrentTransfers")? {
            options.concurrency = value as usize;
        }
        if let Some(value) = positive("lfs.transfer.batchSize")? {
            options.batch_size = value as usize;
        }
        if let Some(value) = positive("lfs.transfer.maxRetries")? {
            options.max_retries = value as u32;
        }
        if let Some(value) = positive("lfs.transfer.maxRetryDelay")? {
            options.max_retry_delay = Duration::from_secs(value);
        }
        if let Some(value) = positive("lfs.transfer.maxVerifies")? {
            options.max_verifies = value as u32;
        }
        options.basic_access = self
            .lfsconfig()?
            .and_then(|file| file.string_by("lfs", Some(endpoint.url.as_bytes().as_bstr()), "access"))
            .into_iter()
            .chain(config.string_filter_by("lfs", Some(endpoint.url.as_bytes().as_bstr()), "access", &mut filter))
            .last()
            .is_some_and(|access| access.eq_ignore_ascii_case(b"basic"));
        options.reference = repo.head_name()?.map(|name| name.as_bstr().to_string());
        options.ssh_command = ssh_command(repo)?;
        Ok(options)
    }

    /// Return a function to obtain credentials for `endpoint` through the credential helpers of the repository.
    pub fn authenticate_fn(&self, endpoint: &Endpoint) -> Result<client::AuthenticateFn> {
        let url = gix_url::parse(endpoint.url.as_str()).or_raise(|| message("Invalid LFS endpoint URL"))?;
        let (mut cascade, _action, prompt) = self.repo.config_snapshot().credential_helpers(url)?;
        Ok(Box::new(move |action| cascade.invoke(action, prompt.clone())))
    }

    /// Create a client for the LFS endpoint of `remote_name` (or the [default remote](Self::remote_name())) for
    /// `operation`, sending requests with `http`, or return `None` if there is no endpoint.
    pub fn client_with_http(
        &self,
        remote_name: Option<&BStr>,
        operation: Operation,
        http: Box<dyn gix_lfs::http::Client>,
    ) -> Result<Option<gix_lfs::Client>> {
        let Some(endpoint) = self.endpoint(remote_name, operation)? else {
            return Ok(None);
        };
        let options = self.client_options(&endpoint)?;
        let authenticate = self.authenticate_fn(&endpoint)?;
        Ok(Some(gix_lfs::Client::new(http, endpoint, Some(authenticate), options)))
    }

    /// Like [`client_with_http()`](Self::client_with_http()), but sending requests with `reqwest` configured by the
    /// `http.*` configuration of the remote.
    #[cfg(feature = "blocking-http-transport-reqwest")]
    pub fn client(&self, remote_name: Option<&BStr>, operation: Operation) -> Result<Option<gix_lfs::Client>> {
        let Some(endpoint) = self.endpoint(remote_name, operation)? else {
            return Ok(None);
        };
        let remote_name = match remote_name {
            Some(name) => Some(name.to_owned()),
            None => self.remote_name(operation)?,
        };
        let mut http_options = gix_lfs::http::Options::default();
        if let Some(options) = self
            .repo
            .transport_options(endpoint.url.as_str(), remote_name.as_ref().map(|name| name.as_bstr()))?
            && let Some(options) = options.downcast_ref::<gix_transport::client::blocking_io::http::Options>()
        {
            http_options = gix_lfs::http::Options {
                user_agent: options.user_agent.clone(),
                extra_headers: options.extra_headers.clone(),
                proxy: options.proxy.clone().filter(|proxy| !proxy.is_empty()),
                connect_timeout: options.connect_timeout,
            };
        }
        let http = gix_lfs::http::reqwest::Client::new(&http_options)?;
        let options = self.client_options(&endpoint)?;
        let authenticate = self.authenticate_fn(&endpoint)?;
        Ok(Some(gix_lfs::Client::new(
            Box::new(http),
            endpoint,
            Some(authenticate),
            options,
        )))
    }
}

fn direction(operation: Operation) -> Direction {
    match operation {
        Operation::Download => Direction::Fetch,
        Operation::Upload => Direction::Push,
    }
}

/// The SSH program as configured with `core.sshCommand`, or `GIT_SSH` which is never run through a shell.
fn ssh_command(repo: &Repository) -> Result<SshCommand> {
    use crate::config::tree::{Core, gitoxide};
    let config = &repo.config.resolved;
    let mut trusted = repo.filter_config_section();
    if let Some(command) = config.string_filter(Core::SSH_COMMAND, &mut trusted) {
        return Ok(SshCommand {
            program: gix_path::from_bstr(command.as_bstr())?.into_owned().into(),
            without_shell: false,
        });
    }
    if let Some(command) = config.string_filter(gitoxide::Ssh::COMMAND_WITHOUT_SHELL_FALLBACK, &mut trusted) {
        return Ok(SshCommand {
            program: gix_path::from_bstr(command.as_bstr())?.into_owned().into(),
            without_shell: true,
        });
    }
    Ok(SshCommand::default())
}
