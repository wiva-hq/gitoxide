//! A client of the batch API using the `basic` transfer adapter.
use std::{
    collections::HashSet,
    io::Read,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use gix_error::{Error, ErrorExt, OptionExt, Result, ResultExt, bail, message};
use gix_utils::progress::{Progress, StepShared};

use crate::{
    Endpoint, Operation, Pointer, Store, batch,
    endpoint::{Authenticated, SshCommand},
    http,
    store::Partial,
};

/// A function to obtain, store or erase credentials, like a [`gix_credentials::helper::Cascade`] does.
pub type AuthenticateFn =
    Box<dyn FnMut(gix_credentials::helper::Action) -> Result<Option<gix_credentials::protocol::Outcome>> + Send>;

/// Actions expiring within this time are considered expired, to not use them while they expire.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5);

/// The maximum size of JSON and error responses to read.
const MAX_RESPONSE_SIZE: u64 = 64 * 1024 * 1024;

/// Settings of the [`Client`], with defaults like those of `git-lfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The maximum amount of objects to transfer at the same time, as configured with `lfs.concurrentTransfers`.
    pub concurrency: usize,
    /// The maximum amount of objects per batch request, as configured with `lfs.transfer.batchSize`.
    pub batch_size: usize,
    /// How often to retry a request that failed transiently, as configured with `lfs.transfer.maxRetries`.
    pub max_retries: u32,
    /// The longest time to wait between retries without a `Retry-After` header, as configured with
    /// `lfs.transfer.maxRetryDelay`.
    pub max_retry_delay: Duration,
    /// How often to try the verify action of an upload, as configured with `lfs.transfer.maxVerifies`.
    pub max_verifies: u32,
    /// If `true`, credentials are obtained before the first request, as configured with `lfs.<url>.access = basic`.
    pub basic_access: bool,
    /// The full name of the reference the objects belong to, sent along with batch requests.
    pub reference: Option<String>,
    /// If `true`, credentials may be sent over unencrypted `http://` connections.
    pub allow_http_credentials: bool,
    /// The program to run `git-lfs-authenticate` with if the endpoint is reached over SSH.
    pub ssh_command: SshCommand,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            concurrency: 8,
            batch_size: 100,
            max_retries: 8,
            max_retry_delay: Duration::from_secs(10),
            max_verifies: 3,
            basic_access: false,
            reference: None,
            allow_http_credentials: false,
            ssh_command: SshCommand::default(),
        }
    }
}

/// The result of [`Client::download()`] and [`Client::upload()`].
#[derive(Debug, Default)]
pub struct Outcome {
    /// The objects that were transferred.
    pub transferred: Vec<Pointer>,
    /// The objects that didn't need a transfer, as they already were in the local store when downloading,
    /// or on the server when uploading.
    pub skipped: Vec<Pointer>,
    /// The objects that couldn't be transferred.
    pub failed: Vec<Failure>,
}

/// An object that couldn't be transferred.
#[derive(Debug)]
pub struct Failure {
    /// The object.
    pub pointer: Pointer,
    /// Why it couldn't be transferred, classified by its cause, like [`Error::is_not_found()`] if the server
    /// doesn't have it.
    pub error: Error,
}

/// A client for one LFS endpoint.
pub struct Client {
    http: Box<dyn http::Client>,
    endpoint: Endpoint,
    options: Options,
    auth: Mutex<Auth>,
}

struct Auth {
    authenticate: Option<AuthenticateFn>,
    credentials: Option<Credentials>,
    /// Incremented whenever `credentials` change, to tell if a request was rejected with the current ones.
    generation: u64,
    ssh: Option<(Operation, Authenticated)>,
}

struct Credentials {
    authorization: String,
    next: gix_credentials::helper::NextAction,
    approved: bool,
}

/// An action of a batch response, ready to be used.
#[derive(Clone)]
struct Action {
    href: String,
    headers: Vec<(String, String)>,
    expires_at: Option<SystemTime>,
}

impl Action {
    fn from_batch(action: batch::Action) -> Self {
        Action {
            expires_at: crate::time::expiry(action.expires_in, action.expires_at.as_deref(), SystemTime::now()),
            href: action.href,
            headers: action.header.into_iter().collect(),
        }
    }

    fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|at| at <= SystemTime::now() + EXPIRY_MARGIN)
    }
}

struct Job {
    pointer: Pointer,
    action: Action,
    verify: Option<Action>,
}

/// What to do after a request.
enum Next {
    /// Look at the response.
    Response(http::Response),
    /// Send the request again.
    Retry,
}

/// Lifecycle
impl Client {
    /// Create a client for `endpoint` sending requests with `http`.
    ///
    /// `authenticate` is called to obtain credentials when the server asks for them, and to store or erase
    /// them depending on whether they were accepted.
    pub fn new(
        http: Box<dyn http::Client>,
        endpoint: Endpoint,
        authenticate: Option<AuthenticateFn>,
        options: Options,
    ) -> Self {
        Client {
            http,
            endpoint,
            options,
            auth: Mutex::new(Auth {
                authenticate,
                credentials: None,
                generation: 0,
                ssh: None,
            }),
        }
    }

    /// The endpoint this client talks to.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

/// Transfers
impl Client {
    /// Download the objects of `pointers` that aren't in `store` yet into it.
    ///
    /// Up to [`Options::concurrency`] objects are transferred at once, and `progress` is initialized with the
    /// amount of bytes to download. Downloads interrupted by a connection failure are resumed, and kept in
    /// the store to be resumed by the next call if `interrupt` is set, in which case a cancellation error
    /// is returned.
    pub fn download(
        &self,
        store: &Store,
        pointers: &[Pointer],
        progress: &mut dyn Progress,
        interrupt: &AtomicBool,
    ) -> Result<Outcome> {
        self.transfer(Operation::Download, store, pointers, progress, interrupt)
    }

    /// Upload the objects of `pointers` from `store` to the server, unless it already has them.
    ///
    /// Objects missing in `store` are reported as failures. `interrupt` is checked between objects.
    pub fn upload(
        &self,
        store: &Store,
        pointers: &[Pointer],
        progress: &mut dyn Progress,
        interrupt: &AtomicBool,
    ) -> Result<Outcome> {
        self.transfer(Operation::Upload, store, pointers, progress, interrupt)
    }

    /// Send one batch request for `pointers` and return the response of the server.
    pub fn batch(&self, operation: Operation, pointers: &[Pointer], interrupt: &AtomicBool) -> Result<batch::Response> {
        let request = batch::Request {
            operation,
            transfers: vec!["basic".into()],
            reference: self.options.reference.clone().map(|name| batch::Ref { name }),
            objects: pointers.iter().map(object_spec).collect(),
            hash_algo: "sha256".into(),
        };
        let body = serde_json::to_vec(&request).or_raise(|| message("Could not serialize a batch request"))?;
        let mut attempt = 0;
        loop {
            check_interrupt(interrupt)?;
            let (base_url, mut headers) = self.api_target(operation)?;
            let url = format!("{base_url}/objects/batch");
            headers.push(("Accept".into(), batch::MEDIA_TYPE.into()));
            headers.push(("Content-Type".into(), batch::MEDIA_TYPE.into()));
            if self.options.basic_access {
                self.ensure_credentials(&url)?;
            }
            let generation = self.add_credentials(&mut headers);
            let result = self.http.request(http::Request {
                method: http::Method::Post,
                url: url.clone(),
                headers,
                body: http::Body::Bytes(body.clone()),
            });
            let response = match self.handle(result, &url, generation, &mut attempt, interrupt)? {
                Next::Retry => continue,
                Next::Response(response) => response,
            };
            if !is_success(response.status) {
                bail!(status_error(response, "LFS batch request", &url));
            }
            self.approve_credentials();
            let mut json = Vec::new();
            response
                .body
                .take(MAX_RESPONSE_SIZE)
                .read_to_end(&mut json)
                .or_raise(|| message!("Could not read the response of '{url}'").retryable())?;
            return serde_json::from_slice(&json)
                .or_raise(|| message!("Could not parse the response of '{url}'").validation());
        }
    }

    fn transfer(
        &self,
        operation: Operation,
        store: &Store,
        pointers: &[Pointer],
        progress: &mut dyn Progress,
        interrupt: &AtomicBool,
    ) -> Result<Outcome> {
        let mut outcome = Outcome::default();
        let mut seen = HashSet::new();
        let mut todo = Vec::new();
        for pointer in pointers {
            if !seen.insert(pointer.oid) {
                continue;
            }
            match operation {
                _ if pointer.size == 0 => outcome.skipped.push(pointer.clone()),
                Operation::Download if store.contains(pointer) => outcome.skipped.push(pointer.clone()),
                Operation::Upload if !store.contains(pointer) => outcome.failed.push(Failure {
                    pointer: pointer.clone(),
                    error: message!("LFS object {} is not in the local store", pointer.oid)
                        .not_found()
                        .raise(),
                }),
                _ => todo.push(pointer.clone()),
            }
        }
        let total: u64 = todo.iter().map(|pointer| pointer.size).sum();
        progress.init(
            Some(usize::try_from(total).unwrap_or(usize::MAX)),
            gix_utils::progress::bytes(),
        );
        let counter = progress.counter();
        for chunk in todo.chunks(self.options.batch_size.max(1)) {
            check_interrupt(interrupt)?;
            let response = self.batch(operation, chunk, interrupt)?;
            if let Some(transfer) = response.transfer.as_deref().filter(|transfer| *transfer != "basic") {
                bail!(
                    message!("The LFS server chose the transfer adapter '{transfer}', but only 'basic' is supported")
                        .unsupported()
                );
            }
            let mut jobs = Vec::new();
            for pointer in chunk {
                match self.job(operation, pointer, &response.objects) {
                    Ok(Some(job)) => jobs.push(job),
                    Ok(None) => outcome.skipped.push(pointer.clone()),
                    Err(error) => outcome.failed.push(Failure {
                        pointer: pointer.clone(),
                        error,
                    }),
                }
            }
            for (pointer, result) in self.run(operation, store, jobs, &counter, interrupt) {
                match result {
                    Ok(true) => outcome.transferred.push(pointer),
                    Ok(false) => outcome.skipped.push(pointer),
                    Err(error) => outcome.failed.push(Failure { pointer, error }),
                }
            }
        }
        check_interrupt(interrupt)?;
        Ok(outcome)
    }

    /// Turn the answer for `pointer` among `objects` into a job, or return `None` if there is nothing to do.
    fn job(&self, operation: Operation, pointer: &Pointer, objects: &[batch::ObjectResponse]) -> Result<Option<Job>> {
        let hex = pointer.oid.to_hex();
        let object = objects
            .iter()
            .find(|object| object.oid == hex)
            .ok_or_raise(|| message!("The LFS server did not answer for object {hex}"))?;
        if let Some(error) = &object.error {
            bail!(object_error(error, pointer));
        }
        let actions = object.actions.clone();
        Ok(match operation {
            Operation::Download => {
                let action = actions
                    .and_then(|actions| actions.download)
                    .ok_or_raise(|| message!("The LFS server did not offer a download of object {hex}"))?;
                Some(Job {
                    pointer: pointer.clone(),
                    action: Action::from_batch(action),
                    verify: None,
                })
            }
            Operation::Upload => actions.and_then(|actions| {
                Some(Job {
                    pointer: pointer.clone(),
                    action: Action::from_batch(actions.upload?),
                    verify: actions.verify.map(Action::from_batch),
                })
            }),
        })
    }

    /// Ask the server for new actions for the object of `job`, returning `false` if there is nothing left to do.
    fn refresh(&self, operation: Operation, job: &mut Job, interrupt: &AtomicBool) -> Result<bool> {
        let response = self.batch(operation, std::slice::from_ref(&job.pointer), interrupt)?;
        let Some(fresh) = self.job(operation, &job.pointer, &response.objects)? else {
            return Ok(false);
        };
        if fresh.action.is_expired() {
            bail!(message!(
                "The LFS server sent an expired action for object {}",
                job.pointer.oid
            ));
        }
        *job = fresh;
        Ok(true)
    }

    fn run(
        &self,
        operation: Operation,
        store: &Store,
        jobs: Vec<Job>,
        counter: &StepShared,
        interrupt: &AtomicBool,
    ) -> Vec<(Pointer, Result<bool>)> {
        let workers = self.options.concurrency.clamp(1, jobs.len().max(1));
        let jobs = Mutex::new(jobs.into_iter());
        let results = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let Some(mut job) = lock(&jobs).next() else {
                            break;
                        };
                        let result = if interrupt.load(Ordering::Relaxed) {
                            Err(cancelled())
                        } else {
                            match operation {
                                Operation::Download => {
                                    self.download_object(store, &mut job, counter, interrupt).map(|()| true)
                                }
                                Operation::Upload => self.upload_object(store, &mut job, counter, interrupt),
                            }
                        };
                        lock(&results).push((job.pointer, result));
                    }
                });
            }
        });
        results.into_inner().unwrap_or_else(PoisonError::into_inner)
    }

    fn download_object(&self, store: &Store, job: &mut Job, counter: &StepShared, interrupt: &AtomicBool) -> Result {
        let mut partial = store.partial(&job.pointer)?;
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            check_interrupt(interrupt)?;
            if job.action.is_expired() && !self.refresh(Operation::Download, job, interrupt)? {
                bail!(message!(
                    "The LFS server did not offer a download of object {}",
                    job.pointer.oid
                ));
            }
            let mut headers = job.action.headers.clone();
            if partial.len() > 0 {
                headers.push((
                    "Range".into(),
                    format!("bytes={}-{}", partial.len(), job.pointer.size - 1),
                ));
            }
            let generation = self.add_action_credentials(&job.action.href, &mut headers);
            let url = job.action.href.clone();
            let result = self.http.request(http::Request {
                method: http::Method::Get,
                url: url.clone(),
                headers,
                body: http::Body::Empty,
            });
            let mut response = match self.handle(result, &url, generation, &mut attempt, interrupt)? {
                Next::Retry => continue,
                Next::Response(response) => response,
            };
            match response.status {
                200 => partial.restart()?,
                206 if content_range_start(&response) == Some(partial.len()) => {}
                206 | 416 if attempt < self.options.max_retries => {
                    attempt += 1;
                    partial.restart()?;
                    continue;
                }
                403 if !refreshed => {
                    refreshed = true;
                    if !self.refresh(Operation::Download, job, interrupt)? {
                        bail!(message!(
                            "The LFS server did not offer a download of object {}",
                            job.pointer.oid
                        ));
                    }
                    continue;
                }
                _ => bail!(status_error(response, "LFS download", &url)),
            }
            if !copy_body(&mut response.body, &mut partial, job.pointer.size, counter, interrupt)?
                || partial.len() < job.pointer.size
            {
                if attempt >= self.options.max_retries {
                    bail!(
                        message!(
                            "The download of LFS object {} stopped after {} of {} bytes",
                            job.pointer.oid,
                            partial.len(),
                            job.pointer.size
                        )
                        .retryable()
                    );
                }
                self.sleep(self.backoff(attempt), interrupt)?;
                attempt += 1;
                continue;
            }
            return partial.finish(store, &job.pointer);
        }
    }

    fn upload_object(
        &self,
        store: &Store,
        job: &mut Job,
        counter: &StepShared,
        interrupt: &AtomicBool,
    ) -> Result<bool> {
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            check_interrupt(interrupt)?;
            if job.action.is_expired() && !self.refresh(Operation::Upload, job, interrupt)? {
                return Ok(false);
            }
            let file = store
                .open(&job.pointer)?
                .ok_or_raise(|| message!("LFS object {} is not in the local store", job.pointer.oid).not_found())?;
            let mut headers = job.action.headers.clone();
            if !has_header(&headers, "Content-Type") {
                headers.push(("Content-Type".into(), "application/octet-stream".into()));
            }
            let generation = self.add_action_credentials(&job.action.href, &mut headers);
            let url = job.action.href.clone();
            let sent = Arc::new(AtomicUsize::new(0));
            let result = self.http.request(http::Request {
                method: http::Method::Put,
                url: url.clone(),
                headers,
                body: http::Body::Stream {
                    reader: Box::new(CountingReader {
                        inner: file,
                        progress: counter.clone(),
                        sent: sent.clone(),
                    }),
                    len: job.pointer.size,
                },
            });
            let next = self.handle(result, &url, generation, &mut attempt, interrupt);
            if !matches!(&next, Ok(Next::Response(response)) if is_success(response.status)) {
                counter.fetch_sub(sent.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            let response = match next? {
                Next::Retry => continue,
                Next::Response(response) => response,
            };
            match response.status {
                status if is_success(status) => break,
                403 if !refreshed => {
                    refreshed = true;
                    if !self.refresh(Operation::Upload, job, interrupt)? {
                        return Ok(false);
                    }
                }
                _ => bail!(status_error(response, "LFS upload", &url)),
            }
        }
        if let Some(verify) = job.verify.clone() {
            self.verify(&job.pointer, &verify, interrupt)?;
        }
        Ok(true)
    }

    fn verify(&self, pointer: &Pointer, action: &Action, interrupt: &AtomicBool) -> Result {
        let body =
            serde_json::to_vec(&object_spec(pointer)).or_raise(|| message("Could not serialize a verify request"))?;
        let mut attempt = 0;
        let mut verifies = 1;
        loop {
            check_interrupt(interrupt)?;
            let mut headers = action.headers.clone();
            headers.push(("Accept".into(), batch::MEDIA_TYPE.into()));
            headers.push(("Content-Type".into(), batch::MEDIA_TYPE.into()));
            let generation = self.add_action_credentials(&action.href, &mut headers);
            let result = self.http.request(http::Request {
                method: http::Method::Post,
                url: action.href.clone(),
                headers,
                body: http::Body::Bytes(body.clone()),
            });
            let response = match self.handle(result, &action.href, generation, &mut attempt, interrupt)? {
                Next::Retry => continue,
                Next::Response(response) => response,
            };
            if is_success(response.status) {
                return Ok(());
            }
            if verifies >= self.options.max_verifies {
                bail!(status_error(response, "LFS verify", &action.href));
            }
            verifies += 1;
            self.sleep(self.backoff(verifies), interrupt)?;
        }
    }
}

/// Requests
impl Client {
    /// Decide what to do with the `result` of a request to `url` sent with the credentials of `generation`,
    /// retrying transient failures and obtaining credentials when needed.
    fn handle(
        &self,
        result: Result<http::Response>,
        url: &str,
        generation: Option<u64>,
        attempt: &mut u32,
        interrupt: &AtomicBool,
    ) -> Result<Next> {
        let response = match result {
            Ok(response) => response,
            Err(err) => {
                if *attempt >= self.options.max_retries || !err.can_retry() {
                    return Err(err);
                }
                gix_trace::debug!("Retrying a request to '{}' after: {err}", redact(url));
                self.sleep(self.backoff(*attempt), interrupt)?;
                *attempt += 1;
                return Ok(Next::Retry);
            }
        };
        if response.status == 401
            && let Some(generation) = generation
        {
            self.on_unauthorized(url, generation)?;
            return Ok(Next::Retry);
        }
        if is_retryable_status(response.status) && *attempt < self.options.max_retries {
            let delay = response
                .header("Retry-After")
                .and_then(|value| crate::time::parse_retry_after(value, SystemTime::now()))
                .unwrap_or_else(|| self.backoff(*attempt));
            self.sleep(delay, interrupt)?;
            *attempt += 1;
            return Ok(Next::Retry);
        }
        Ok(Next::Response(response))
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let delay = Duration::from_millis(250).saturating_mul(1 << attempt.min(16));
        delay.min(self.options.max_retry_delay)
    }

    fn sleep(&self, duration: Duration, interrupt: &AtomicBool) -> Result {
        let until = SystemTime::now() + duration;
        while let Ok(left) = until.duration_since(SystemTime::now()) {
            check_interrupt(interrupt)?;
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
        Ok(())
    }

    /// Return the base URL of the API and the headers to send for `operation`, authenticating over SSH if needed.
    fn api_target(&self, operation: Operation) -> Result<(String, Vec<(String, String)>)> {
        let Some(ssh) = &self.endpoint.ssh else {
            return Ok((self.endpoint.url.clone(), Vec::new()));
        };
        let mut auth = lock(&self.auth);
        let is_valid = auth.ssh.as_ref().is_some_and(|(cached_operation, authenticated)| {
            *cached_operation == operation
                && authenticated
                    .expires_at
                    .is_none_or(|at| at > SystemTime::now() + EXPIRY_MARGIN)
        });
        if !is_valid {
            let authenticated = ssh
                .authenticate(&self.options.ssh_command, operation)?
                .unwrap_or_default();
            auth.ssh = Some((operation, authenticated));
        }
        let (_, authenticated) = auth.ssh.as_ref().expect("set above");
        Ok((
            authenticated.url.clone().unwrap_or_else(|| self.endpoint.url.clone()),
            authenticated.headers.clone(),
        ))
    }

    /// Add the credentials to `headers` unless they already authorize the request, and return their generation.
    fn add_credentials(&self, headers: &mut Vec<(String, String)>) -> Option<u64> {
        if has_header(headers, "Authorization") {
            return None;
        }
        let auth = lock(&self.auth);
        if let Some(credentials) = &auth.credentials {
            headers.push(("Authorization".into(), credentials.authorization.clone()));
        }
        Some(auth.generation)
    }

    /// Like [`Self::add_credentials()`], but only if `href` is on the same server as the API.
    fn add_action_credentials(&self, href: &str, headers: &mut Vec<(String, String)>) -> Option<u64> {
        let api_url = {
            let auth = lock(&self.auth);
            auth.ssh
                .as_ref()
                .and_then(|(_, authenticated)| authenticated.url.clone())
                .unwrap_or_else(|| self.endpoint.url.clone())
        };
        if origin(href)?.eq_ignore_ascii_case(origin(&api_url)?) {
            self.add_credentials(headers)
        } else {
            None
        }
    }

    fn ensure_credentials(&self, url: &str) -> Result {
        let mut auth = lock(&self.auth);
        if auth.credentials.is_none() {
            acquire_credentials(&mut auth, url, self.options.allow_http_credentials)?;
        }
        Ok(())
    }

    /// React to a request to `url` being rejected with 401 while sent with credentials of `generation`.
    ///
    /// Return `Ok` if the request should be sent again with new credentials.
    fn on_unauthorized(&self, url: &str, generation: u64) -> Result {
        let mut auth = lock(&self.auth);
        if auth.generation != generation {
            return Ok(());
        }
        if let Some(credentials) = auth.credentials.take() {
            auth.generation += 1;
            if let Some(authenticate) = auth.authenticate.as_mut()
                && let Err(_err) = authenticate(credentials.next.erase())
            {
                gix_trace::warn!("Could not erase rejected credentials: {_err}");
            }
            bail!(message!("The LFS server at '{}' rejected the credentials", redact(url)).unauthenticated());
        }
        acquire_credentials(&mut auth, url, self.options.allow_http_credentials)
    }

    /// Store credentials once they were accepted.
    fn approve_credentials(&self) {
        let mut auth = lock(&self.auth);
        let Auth {
            authenticate,
            credentials,
            ..
        } = &mut *auth;
        if let (Some(authenticate), Some(credentials)) = (authenticate.as_mut(), credentials.as_mut())
            && !credentials.approved
        {
            credentials.approved = true;
            if let Err(_err) = authenticate(credentials.next.clone().store()) {
                gix_trace::warn!("Could not store accepted credentials: {_err}");
            }
        }
    }
}

fn acquire_credentials(auth: &mut Auth, url: &str, allow_http: bool) -> Result {
    let url = redact(url);
    if !allow_http
        && url
            .get(..7)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
    {
        bail!(message!("Refusing to send credentials to '{url}' over unencrypted http").unauthenticated());
    }
    let authenticate = auth.authenticate.as_mut().ok_or_raise(|| {
        message!("The LFS server at '{url}' requires credentials, but none can be obtained").unauthenticated()
    })?;
    let outcome = authenticate(gix_credentials::helper::Action::get_for_url(url))?
        .ok_or_raise(|| message!("No credentials were provided for '{url}'").unauthenticated())?;
    let authorization = match outcome.credentials {
        gix_sec::identity::Credentials::UsernamePassword { username, password, .. } => {
            use base64::Engine;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            )
        }
        gix_sec::identity::Credentials::Encoded {
            authtype, credential, ..
        } => format!("{authtype} {credential}"),
    };
    auth.credentials = Some(Credentials {
        authorization,
        next: outcome.next,
        approved: false,
    });
    auth.generation += 1;
    Ok(())
}

/// Append the body to `partial` until it ends or the object is complete, and return `false` if reading failed.
fn copy_body(
    body: &mut dyn Read,
    partial: &mut Partial,
    size: u64,
    counter: &StepShared,
    interrupt: &AtomicBool,
) -> Result<bool> {
    let mut buf = vec![0; 64 * 1024];
    while partial.len() <= size {
        check_interrupt(interrupt)?;
        let read = match body.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_err) => {
                gix_trace::debug!("Download stopped after {} bytes: {_err}", partial.len());
                return Ok(false);
            }
        };
        partial.write(&buf[..read])?;
        counter.fetch_add(read, Ordering::Relaxed);
    }
    Ok(true)
}

/// A reader of an object to upload, which reports progress.
struct CountingReader {
    inner: std::fs::File,
    progress: StepShared,
    sent: Arc<AtomicUsize>,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.progress.fetch_add(read, Ordering::Relaxed);
        self.sent.fetch_add(read, Ordering::Relaxed);
        Ok(read)
    }
}

fn object_spec(pointer: &Pointer) -> batch::ObjectSpec {
    batch::ObjectSpec {
        oid: pointer.oid.to_hex(),
        size: pointer.size,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cancelled() -> Error {
    gix_error::cancelled("The LFS transfer was interrupted").raise()
}

fn check_interrupt(interrupt: &AtomicBool) -> Result {
    if interrupt.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    Ok(())
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(key, _)| key.eq_ignore_ascii_case(name))
}

/// The first byte of a `Content-Range: bytes <first>-<last>/<size>` header.
fn content_range_start(response: &http::Response) -> Option<u64> {
    let range = response.header("Content-Range")?.trim().strip_prefix("bytes ")?;
    range.split_once('-')?.0.trim().parse().ok()
}

/// The scheme and authority of `url`.
fn origin(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")? + 3;
    let authority_len = url[scheme_end..]
        .find(['/', '?', '#'])
        .unwrap_or(url.len() - scheme_end);
    Some(&url[..scheme_end + authority_len])
}

/// Strip the query from `url`, as it may carry signatures.
fn redact(url: &str) -> &str {
    url.split_once('?').map_or(url, |(base, _)| base)
}

fn status_error(response: http::Response, what: &str, url: &str) -> Error {
    let status = response.status;
    let mut body = Vec::new();
    let detail = response
        .body
        .take(64 * 1024)
        .read_to_end(&mut body)
        .ok()
        .and_then(|_| serde_json::from_slice::<batch::ErrorResponse>(&body).ok())
        .map(|error| format!(": {}", error.message))
        .unwrap_or_default();
    let message = message!("{what} at '{}' failed with HTTP status {status}{detail}", redact(url));
    classify(message, status).raise()
}

fn object_error(error: &batch::ObjectError, pointer: &Pointer) -> Error {
    let message = message!(
        "The LFS server refused object {} with code {}: {}",
        pointer.oid,
        error.code,
        error.message
    );
    classify(message, error.code).raise()
}

fn classify(message: gix_error::Message, status: u16) -> gix_error::Message {
    match status {
        401 => message.unauthenticated(),
        403 => message.permission_denied(),
        404 | 410 => message.not_found(),
        422 => message.validation(),
        status if is_retryable_status(status) => message.retryable(),
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::origin;

    #[test]
    fn origin_of_urls() {
        assert_eq!(
            origin("https://example.com/repo.git/info/lfs"),
            Some("https://example.com")
        );
        assert_eq!(origin("http://127.0.0.1:8080?x=y"), Some("http://127.0.0.1:8080"));
        assert_eq!(origin("https://example.com"), Some("https://example.com"), "no path");
        assert_eq!(origin("example.com/path"), None, "a scheme is required");
    }
}
