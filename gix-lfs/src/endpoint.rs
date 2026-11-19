//! Finding the LFS server of a remote, and authenticating with it over SSH.
use std::{ffi::OsString, process::Stdio, time::SystemTime};

use bstr::{BStr, ByteSlice};
use gix_error::{OptionExt, Result, ResultExt, bail, message};

/// The direction of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// Objects are fetched from the server.
    Download,
    /// Objects are sent to the server.
    Upload,
}

impl Operation {
    /// The name of the operation as used by the batch API and `git-lfs-authenticate`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Download => "download",
            Operation::Upload => "upload",
        }
    }
}

/// The location of an LFS server, which serves the batch API at `<url>/objects/batch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The base URL of the LFS API, without trailing slash.
    pub url: String,
    /// The SSH host to obtain the actual URL and authentication from, if the remote is reached over SSH.
    pub ssh: Option<Ssh>,
}

/// The information needed to run `git-lfs-authenticate` on an SSH host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
    /// The user to log in as.
    pub user: Option<String>,
    /// The host to connect to.
    pub host: String,
    /// The port to connect to if not the default.
    pub port: Option<u16>,
    /// The path of the repository on the host.
    pub path: String,
}

/// The answer of `git-lfs-authenticate`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Authenticated {
    /// The URL of the LFS API to use instead of the one derived from the remote URL.
    pub url: Option<String>,
    /// Headers to send with each API request, typically `Authorization`.
    pub headers: Vec<(String, String)>,
    /// The time at which `headers` stop being valid.
    pub expires_at: Option<SystemTime>,
}

/// The program to use for SSH connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommand {
    /// The program or shell script to run, like `core.sshCommand`, defaulting to `ssh`.
    pub program: OsString,
    /// If `true`, `program` is executed directly, like `GIT_SSH`, instead of possibly as shell script.
    pub without_shell: bool,
}

impl Default for SshCommand {
    fn default() -> Self {
        SshCommand {
            program: "ssh".into(),
            without_shell: false,
        }
    }
}

/// Lifecycle
impl Endpoint {
    /// Use `url`, as configured with `lfs.url` or `remote.<name>.lfsurl`, as the endpoint.
    ///
    /// HTTP URLs are used as is. SSH URLs denote a host to run `git-lfs-authenticate` on, with the API served
    /// over HTTPS on the same host.
    pub fn from_configured_url(url: &BStr) -> Result<Self> {
        let parsed = gix_url::parse(url).or_raise(|| message!("Could not parse LFS URL '{url}'"))?;
        match parsed.scheme {
            gix_url::Scheme::Http | gix_url::Scheme::Https => Ok(Endpoint {
                url: url.to_str_lossy().trim_end_matches('/').to_owned(),
                ssh: None,
            }),
            gix_url::Scheme::Ssh => ssh_endpoint(&parsed, false),
            _ => bail!(message!("LFS URL '{url}' must use http, https or ssh").unsupported()),
        }
    }

    /// Derive the endpoint from the URL of a Git remote, like `git-lfs` does, or return `None` if the remote is local.
    ///
    /// For HTTP remotes this is the remote URL with `.git/info/lfs` appended, unless it already ends in `.git`.
    /// For SSH and Git remotes, the same is done to an HTTPS URL on the same host.
    pub fn from_remote_url(url: &gix_url::Url) -> Result<Option<Self>> {
        Ok(Some(match url.scheme {
            gix_url::Scheme::Http | gix_url::Scheme::Https => {
                let mut url = url.clone();
                url.user = None;
                url.password = None;
                Endpoint {
                    url: append_info_lfs(url.to_bstring().to_str_lossy().into_owned()),
                    ssh: None,
                }
            }
            gix_url::Scheme::Ssh => ssh_endpoint(url, true)?,
            gix_url::Scheme::Git => Endpoint {
                url: append_info_lfs(https_url(url)?),
                ssh: None,
            },
            _ => return Ok(None),
        }))
    }
}

fn ssh_endpoint(url: &gix_url::Url, is_remote: bool) -> Result<Endpoint> {
    let https = https_url(url)?;
    let path = url.path.to_str_lossy().into_owned();
    if path.starts_with('-') {
        bail!(message!("The path of '{}' could be mistaken for an option", url.to_bstring()).validation());
    }
    let host = url
        .host_argument_safe()
        .ok_or_raise(|| message!("The host of '{}' can't be passed to ssh safely", url.to_bstring()).validation())?;
    let user = match url.user() {
        Some(_) => Some(
            url.user_argument_safe()
                .ok_or_raise(|| {
                    message!("The user of '{}' can't be passed to ssh safely", url.to_bstring()).validation()
                })?
                .to_owned(),
        ),
        None => None,
    };
    Ok(Endpoint {
        url: if is_remote { append_info_lfs(https) } else { https },
        ssh: Some(Ssh {
            user,
            host: host.to_owned(),
            port: url.port,
            path,
        }),
    })
}

fn https_url(url: &gix_url::Url) -> Result<String> {
    let host = url
        .host()
        .ok_or_raise(|| message!("Remote URL '{}' has no host", url.to_bstring()).validation())?;
    let path = url.path.to_str_lossy();
    let path = path.trim_start_matches('/');
    Ok(format!("https://{host}/{path}"))
}

fn append_info_lfs(url: String) -> String {
    let url = url.trim_end_matches('/');
    if url.as_bytes().ends_with(b".git") {
        format!("{url}/info/lfs")
    } else {
        format!("{url}.git/info/lfs")
    }
}

impl Ssh {
    /// Run `git-lfs-authenticate` for `operation` on the host with `command` and return its answer.
    ///
    /// Return `None` if the command failed on the host, typically because the server doesn't provide it, in
    /// which case the endpoint is used without SSH authentication, as `git-lfs` does.
    pub fn authenticate(&self, command: &SshCommand, operation: Operation) -> Result<Option<Authenticated>> {
        let destination = match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        };
        let program_name = std::path::Path::new(&command.program)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let is_putty = program_name.contains("plink") || program_name.contains("putty");
        let mut args: Vec<OsString> = Vec::new();
        if program_name == "tortoiseplink" {
            args.push("-batch".into());
        }
        if let Some(port) = self.port {
            args.push(if is_putty { "-P" } else { "-p" }.into());
            args.push(port.to_string().into());
        }
        args.push(destination.into());
        args.push(
            format!(
                "git-lfs-authenticate {} {}",
                gix_quote::single(self.path.as_str().into()),
                operation.as_str()
            )
            .into(),
        );
        let mut prepare = gix_command::prepare(command.program.clone())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if !command.without_shell {
            prepare = prepare.command_may_be_shell_script();
        }
        let output = prepare
            .spawn()
            .and_then(std::process::Child::wait_with_output)
            .or_raise(|| {
                message!(
                    "Could not run '{}' to authenticate with LFS",
                    command.program.to_string_lossy()
                )
            })?;
        if !output.status.success() {
            gix_trace::warn!(
                "git-lfs-authenticate failed on {}, continuing without SSH authentication: {}",
                self.host,
                output.stderr.trim().as_bstr()
            );
            return Ok(None);
        }
        parse_authenticate_response(&output.stdout, SystemTime::now()).map(Some)
    }
}

#[derive(serde::Deserialize)]
struct AuthenticateResponse {
    href: Option<String>,
    #[serde(default)]
    header: std::collections::BTreeMap<String, String>,
    expires_in: Option<i64>,
    expires_at: Option<String>,
}

fn parse_authenticate_response(stdout: &[u8], now: SystemTime) -> Result<Authenticated> {
    let response: AuthenticateResponse = serde_json::from_slice(stdout)
        .or_raise(|| message("Could not parse the answer of git-lfs-authenticate").validation())?;
    Ok(Authenticated {
        url: response.href.map(|href| href.trim_end_matches('/').to_owned()),
        headers: response.header.into_iter().collect(),
        expires_at: crate::time::expiry(response.expires_in, response.expires_at.as_deref(), now),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::parse_authenticate_response;

    #[test]
    fn authenticate_response() -> gix_error::TestResult {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let auth = parse_authenticate_response(
            br#"{"href":"https://lfs.example.com/repo/","header":{"Authorization":"RemoteAuth token"},"expires_in":60}"#,
            now,
        )?;
        assert_eq!(
            auth.url.as_deref(),
            Some("https://lfs.example.com/repo"),
            "trailing slashes are removed"
        );
        assert_eq!(
            auth.headers,
            [("Authorization".to_owned(), "RemoteAuth token".to_owned())],
            "headers are kept"
        );
        assert_eq!(
            auth.expires_at,
            Some(now + Duration::from_secs(60)),
            "expires_in is relative to now"
        );
        assert!(
            parse_authenticate_response(b"not json", now).is_err(),
            "garbage is refused"
        );
        Ok(())
    }
}
