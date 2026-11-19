use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gix_lfs::{Client, Endpoint, Pointer, Store, client};
use gix_utils::progress::Discard;

use crate::content_pointer;

/// What the test server does, and what it saw.
#[derive(Default)]
struct State {
    /// The objects on the server by hex id.
    objects: HashMap<String, Vec<u8>>,
    /// The `Authorization` header every request must have.
    require_auth: Option<String>,
    /// Statuses to answer the next batch requests with.
    batch_failures: Vec<u16>,
    /// If set, the first batch response has actions that already expired.
    expire_first_batch: bool,
    /// Ids of objects to report as missing in batch responses.
    missing: HashSet<String>,
    /// If set, the next download stops half-way by closing the connection.
    drop_next_download: bool,
    /// If set, the next download sets this flag half-way and then stalls.
    interrupt_next_download: Option<Arc<AtomicBool>>,
    /// If set, downloads send content that doesn't match the object id.
    corrupt_downloads: bool,
    /// Requests as `<METHOD> <path>` with ` <range>` for downloads with a `Range` header.
    log: Vec<String>,
}

struct Server {
    base: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(State::default()));
        let server_state = state.clone();
        let server_base = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = server_state.clone();
                let base = server_base.clone();
                std::thread::spawn(move || {
                    // Failures are clients going away, which some tests provoke.
                    serve(stream, &state, &base).ok();
                });
            }
        });
        Ok(Server { base, state })
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            url: format!("{}/repo.git/info/lfs", self.base),
            ssh: None,
        }
    }

    fn client(&self, authenticate: Option<client::AuthenticateFn>) -> gix_error::Result<Client> {
        let http = gix_lfs::http::reqwest::Client::new(&Default::default())?;
        Ok(Client::new(
            Box::new(http),
            self.endpoint(),
            authenticate,
            client::Options {
                allow_http_credentials: true,
                max_retry_delay: Duration::from_millis(50),
                ..Default::default()
            },
        ))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("no panics while locked")
    }

    fn add(&self, content: &[u8]) -> Pointer {
        let pointer = content_pointer(content);
        self.state().objects.insert(pointer.oid.to_hex(), content.to_vec());
        pointer
    }

    fn requests(&self, prefix: &str) -> Vec<String> {
        self.state()
            .log
            .iter()
            .filter(|line| line.starts_with(prefix))
            .cloned()
            .collect()
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> std::io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let len = headers
        .get("content-length")
        .and_then(|len| len.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    write_head(stream, status, headers, body.len())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_head(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], len: usize) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} Status\r\nContent-Length: {len}\r\nConnection: close\r\n");
    for (name, value) in headers {
        for part in [*name, ": ", value.as_str(), "\r\n"] {
            head.push_str(part);
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn json(value: &serde_json::Value) -> (Vec<(&'static str, String)>, Vec<u8>) {
    (
        vec![("Content-Type", gix_lfs::batch::MEDIA_TYPE.to_owned())],
        value.to_string().into_bytes(),
    )
}

fn serve(mut stream: TcpStream, state: &Mutex<State>, base: &str) -> std::io::Result<()> {
    let request = read_request(&stream)?;
    let lock = || state.lock().expect("no panics while locked");
    let range = request.headers.get("range").cloned();
    lock().log.push(match &range {
        Some(range) => format!("{} {} {range}", request.method, request.path),
        None => format!("{} {}", request.method, request.path),
    });
    let required = lock().require_auth.clone();
    if let Some(required) = required
        && request.headers.get("authorization") != Some(&required)
    {
        return respond(
            &mut stream,
            401,
            &[("LFS-Authenticate", "Basic realm=\"test\"".into())],
            br#"{"message":"credentials needed"}"#,
        );
    }

    let lfs = "/repo.git/info/lfs";
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", path) if path == format!("{lfs}/objects/batch") => {
            let failure = {
                let mut state = lock();
                (!state.batch_failures.is_empty()).then(|| state.batch_failures.remove(0))
            };
            if let Some(status) = failure {
                return respond(&mut stream, status, &[("Retry-After", "0".into())], b"");
            }
            let batch: serde_json::Value = serde_json::from_slice(&request.body).expect("valid batch request");
            assert_eq!(
                batch["transfers"],
                serde_json::json!(["basic"]),
                "only basic is offered"
            );
            assert_eq!(batch["hash_algo"], "sha256", "the hash is announced");
            let expired = std::mem::take(&mut lock().expire_first_batch);
            let expiry = if expired {
                serde_json::json!({"expires_at": "2001-02-03T04:05:06Z"})
            } else {
                serde_json::json!({"expires_in": 3600})
            };
            let operation = batch["operation"].as_str().unwrap_or_default().to_owned();
            let objects: Vec<_> = batch["objects"]
                .as_array()
                .expect("objects are listed")
                .iter()
                .map(|object| {
                    let oid = object["oid"].as_str().unwrap_or_default();
                    let size = &object["size"];
                    let state = lock();
                    if state.missing.contains(oid) {
                        return serde_json::json!({"oid": oid, "size": size, "error": {"code": 404, "message": "Object does not exist"}});
                    }
                    let action = |kind: &str| {
                        let mut action = serde_json::json!({
                            "href": format!("{base}/{kind}/{oid}"),
                            "header": {"X-Test-Action": kind},
                        });
                        action
                            .as_object_mut()
                            .expect("an object")
                            .extend(expiry.as_object().expect("an object").clone());
                        action
                    };
                    match operation.as_str() {
                        "download" => serde_json::json!({"oid": oid, "size": size, "actions": {"download": action("download")}}),
                        _ if state.objects.contains_key(oid) => serde_json::json!({"oid": oid, "size": size}),
                        _ => serde_json::json!({"oid": oid, "size": size, "actions": {
                            "upload": action("upload"),
                            "verify": {"href": format!("{base}/verify")},
                        }}),
                    }
                })
                .collect();
            let (headers, body) = json(&serde_json::json!({"transfer": "basic", "objects": objects}));
            respond(&mut stream, 200, &headers, &body)
        }
        ("GET", path) if path.starts_with("/download/") => {
            assert_eq!(
                request.headers.get("x-test-action").map(String::as_str),
                Some("download"),
                "action headers are sent"
            );
            let oid = &path["/download/".len()..];
            let (Some(mut content), corrupt, drop_half, interrupt) = ({
                let mut state = lock();
                (
                    state.objects.get(oid).cloned(),
                    state.corrupt_downloads,
                    std::mem::take(&mut state.drop_next_download),
                    state.interrupt_next_download.take(),
                )
            }) else {
                return respond(&mut stream, 404, &[], b"");
            };
            if corrupt {
                content[0] ^= 1;
            }
            let start: usize = range
                .as_deref()
                .and_then(|range| range.strip_prefix("bytes="))
                .and_then(|range| range.split_once('-'))
                .and_then(|(start, _)| start.parse().ok())
                .unwrap_or(0);
            let (status, headers) = if start > 0 {
                (
                    206,
                    vec![(
                        "Content-Range",
                        format!("bytes {start}-{}/{}", content.len() - 1, content.len()),
                    )],
                )
            } else {
                (200, Vec::new())
            };
            let body = &content[start..];
            write_head(&mut stream, status, &headers, body.len())?;
            let half = body.len() / 2;
            if drop_half {
                stream.write_all(&body[..half])?;
                stream.flush()?;
                return stream.shutdown(std::net::Shutdown::Both);
            }
            if let Some(interrupt) = interrupt {
                stream.write_all(&body[..half])?;
                stream.flush()?;
                std::thread::sleep(Duration::from_millis(100));
                interrupt.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(500));
                stream.write_all(&body[half..])?;
                return stream.flush();
            }
            stream.write_all(body)?;
            stream.flush()
        }
        ("PUT", path) if path.starts_with("/upload/") => {
            assert_eq!(
                request.headers.get("content-type").map(String::as_str),
                Some("application/octet-stream"),
                "uploads are binary"
            );
            let oid = path["/upload/".len()..].to_owned();
            lock().objects.insert(oid, request.body);
            respond(&mut stream, 200, &[], b"")
        }
        ("POST", "/verify") => {
            let object: serde_json::Value = serde_json::from_slice(&request.body).expect("valid verify request");
            let oid = object["oid"].as_str().unwrap_or_default();
            let is_valid = lock()
                .objects
                .get(oid)
                .is_some_and(|content| Some(content.len() as u64) == object["size"].as_u64());
            respond(&mut stream, if is_valid { 200 } else { 422 }, &[], b"")
        }
        _ => respond(&mut stream, 404, &[], b""),
    }
}

fn content(seed: u8) -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8 ^ seed).collect()
}

fn store() -> std::io::Result<(gix_testtools::tempfile::TempDir, Store)> {
    let tmp = gix_testtools::tempfile::TempDir::new()?;
    let store = Store::at(tmp.path().join("lfs"));
    Ok((tmp, store))
}

fn assert_stored(store: &Store, pointer: &Pointer, content: &[u8]) -> std::io::Result<()> {
    assert_eq!(
        std::fs::read(store.object_path(&pointer.oid))?,
        content,
        "the object is stored"
    );
    Ok(())
}

#[test]
fn download_skips_present_objects_and_reports_missing_ones() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let present = server.add(&content(1));
    let missing = content_pointer(b"not on the server");
    server.state().missing.insert(missing.oid.to_hex());

    let client = server.client(None)?;
    let interrupt = AtomicBool::new(false);
    let outcome = client.download(
        &store,
        &[present.clone(), missing.clone(), present.clone()],
        &mut Discard,
        &interrupt,
    )?;
    assert_eq!(
        outcome.transferred,
        std::slice::from_ref(&present),
        "duplicates are downloaded once"
    );
    assert_eq!(outcome.failed.len(), 1, "one object is missing");
    assert_eq!(outcome.failed[0].pointer, missing);
    assert!(
        outcome.failed[0].error.is_not_found(),
        "a per-object 404 is a not-found error"
    );
    assert_stored(&store, &present, &content(1))?;

    let outcome = client.download(&store, std::slice::from_ref(&present), &mut Discard, &interrupt)?;
    assert_eq!(
        outcome.skipped,
        [present],
        "objects in the store aren't downloaded again"
    );
    assert_eq!(
        server.requests("POST").len(),
        1,
        "and no batch request is needed for them"
    );
    Ok(())
}

#[test]
fn expired_actions_are_renewed() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(2));
    server.state().expire_first_batch = true;

    let outcome = server.client(None)?.download(
        &store,
        std::slice::from_ref(&pointer),
        &mut Discard,
        &AtomicBool::new(false),
    )?;
    assert_eq!(outcome.transferred, std::slice::from_ref(&pointer));
    assert_eq!(
        server.requests("POST").len(),
        2,
        "the expired action was replaced by asking for the object again"
    );
    assert_eq!(server.requests("GET").len(), 1, "the expired action was never used");
    assert_stored(&store, &pointer, &content(2))?;
    Ok(())
}

#[test]
fn credentials_are_obtained_on_401_and_stored_once_accepted() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(3));
    server.state().require_auth = Some("Basic dXNlcjpwYXNz".into());

    let (authenticate, actions) = credentials("pass");
    let outcome = server.client(Some(authenticate))?.download(
        &store,
        std::slice::from_ref(&pointer),
        &mut Discard,
        &AtomicBool::new(false),
    )?;
    assert_eq!(outcome.transferred, std::slice::from_ref(&pointer));
    assert_eq!(
        *actions.lock().expect("not poisoned"),
        ["get", "store"],
        "credentials were requested once and approved after being accepted"
    );
    assert_eq!(
        server.requests("POST").len(),
        2,
        "the batch request was repeated with credentials"
    );
    assert_stored(&store, &pointer, &content(3))?;
    Ok(())
}

#[test]
fn rejected_credentials_are_erased() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(4));
    server.state().require_auth = Some("Basic dXNlcjpwYXNz".into());

    let (authenticate, actions) = credentials("wrong");
    let err = server
        .client(Some(authenticate))?
        .download(&store, &[pointer], &mut Discard, &AtomicBool::new(false))
        .expect_err("the credentials are wrong");
    assert!(err.is_unauthenticated(), "the error says why");
    assert_eq!(
        *actions.lock().expect("not poisoned"),
        ["get", "erase"],
        "wrong credentials are erased"
    );

    let err = server
        .client(None)?
        .download(
            &store,
            &[content_pointer(&content(4))],
            &mut Discard,
            &AtomicBool::new(false),
        )
        .expect_err("credentials are required");
    assert!(
        err.is_unauthenticated(),
        "without a way to obtain credentials, the error says so too"
    );
    Ok(())
}

#[test]
fn broken_downloads_are_resumed() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(5));
    server.state().drop_next_download = true;

    let outcome = server.client(None)?.download(
        &store,
        std::slice::from_ref(&pointer),
        &mut Discard,
        &AtomicBool::new(false),
    )?;
    assert_eq!(outcome.transferred, std::slice::from_ref(&pointer));
    let downloads = server.requests("GET");
    assert_eq!(downloads.len(), 2, "the download was continued");
    assert_eq!(
        downloads[1],
        format!("GET /download/{} bytes=100000-199999", pointer.oid),
        "only the missing part was requested"
    );
    assert_stored(&store, &pointer, &content(5))?;
    Ok(())
}

#[test]
fn interrupted_downloads_keep_their_progress_for_the_next_attempt() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(6));
    let interrupt = Arc::new(AtomicBool::new(false));
    server.state().interrupt_next_download = Some(interrupt.clone());
    let client = server.client(None)?;

    let err = client
        .download(&store, std::slice::from_ref(&pointer), &mut Discard, &interrupt)
        .expect_err("the download was interrupted");
    assert!(err.is_cancelled(), "interruptions are reported as cancellation");
    assert!(!store.contains(&pointer), "the object isn't complete");
    let partial = store.root().join("incomplete").join(format!("{}.part", pointer.oid));
    assert!(std::fs::metadata(&partial)?.len() > 0, "what was received is kept");

    interrupt.store(false, Ordering::SeqCst);
    let outcome = client.download(&store, std::slice::from_ref(&pointer), &mut Discard, &interrupt)?;
    assert_eq!(outcome.transferred, std::slice::from_ref(&pointer));
    let downloads = server.requests("GET");
    assert!(
        downloads[1].contains(" bytes="),
        "the second download continues where the first one stopped: {downloads:?}"
    );
    assert_stored(&store, &pointer, &content(6))?;
    assert!(!partial.exists(), "the partial download was moved into place");
    Ok(())
}

#[test]
fn corrupt_downloads_are_discarded() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(7));
    server.state().corrupt_downloads = true;

    let outcome = server.client(None)?.download(
        &store,
        std::slice::from_ref(&pointer),
        &mut Discard,
        &AtomicBool::new(false),
    )?;
    assert_eq!(outcome.failed.len(), 1, "the object failed to download");
    assert!(
        outcome.failed[0].error.is_corrupted(),
        "because its content doesn't match its id"
    );
    assert!(!store.contains(&pointer), "and it was not stored");
    assert!(
        !store
            .root()
            .join("incomplete")
            .join(format!("{}.part", pointer.oid))
            .exists(),
        "nor kept for resumption"
    );
    Ok(())
}

#[test]
fn transient_batch_failures_are_retried() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let pointer = server.add(&content(8));
    server.state().batch_failures = vec![503, 429];

    let outcome = server.client(None)?.download(
        &store,
        std::slice::from_ref(&pointer),
        &mut Discard,
        &AtomicBool::new(false),
    )?;
    assert_eq!(outcome.transferred, [pointer]);
    assert_eq!(server.requests("POST").len(), 3, "two failures were retried");

    server.state().batch_failures = vec![400];
    let err = server
        .client(None)?
        .download(
            &store,
            &[content_pointer(b"other")],
            &mut Discard,
            &AtomicBool::new(false),
        )
        .expect_err("400 isn't retried");
    assert!(!err.is_retryable(), "and the error isn't classified as transient");
    Ok(())
}

#[test]
fn uploads_send_missing_objects_and_verify_them() -> gix_testtools::Result {
    let server = Server::start()?;
    let (_tmp, store) = store()?;
    let data = content(9);
    let pointer = store.write_from(&mut data.as_slice())?;
    let not_local = content_pointer(b"not in the local store");

    let client = server.client(None)?;
    let interrupt = AtomicBool::new(false);
    let outcome = client.upload(&store, &[pointer.clone(), not_local.clone()], &mut Discard, &interrupt)?;
    assert_eq!(outcome.transferred, std::slice::from_ref(&pointer));
    assert_eq!(outcome.failed.len(), 1, "objects must be present locally");
    assert_eq!(outcome.failed[0].pointer, not_local);
    assert!(outcome.failed[0].error.is_not_found());
    assert_eq!(
        server.state().objects.get(&pointer.oid.to_hex()),
        Some(&data),
        "the server received the object"
    );
    assert_eq!(server.requests("POST /verify").len(), 1, "the upload was verified");

    let outcome = client.upload(&store, std::slice::from_ref(&pointer), &mut Discard, &interrupt)?;
    assert_eq!(outcome.skipped, [pointer], "objects the server has aren't sent again");
    assert_eq!(server.requests("PUT").len(), 1);
    Ok(())
}

/// Return a function providing `user` and `password` as credentials, and the actions it was called with.
fn credentials(password: &'static str) -> (client::AuthenticateFn, Arc<Mutex<Vec<&'static str>>>) {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let recorded = actions.clone();
    let authenticate: client::AuthenticateFn = Box::new(move |action| {
        let mut recorded = recorded.lock().expect("not poisoned");
        Ok(match action {
            gix_credentials::helper::Action::Get(_) => {
                recorded.push("get");
                Some(gix_credentials::protocol::Outcome {
                    credentials: gix_sec::identity::Credentials::UsernamePassword {
                        username: "user".into(),
                        password: password.into(),
                        oauth_refresh_token: None,
                    },
                    next: gix_credentials::protocol::Context::default().into(),
                })
            }
            gix_credentials::helper::Action::Store(_) => {
                recorded.push("store");
                None
            }
            gix_credentials::helper::Action::Erase(_) => {
                recorded.push("erase");
                None
            }
        })
    });
    (authenticate, actions)
}
