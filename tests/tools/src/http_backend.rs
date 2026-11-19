use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bstr::ByteSlice;

/// A web server for all repositories at or below a directory, which is stopped automatically on drop.
///
/// It runs `git http-backend` for each request, which serves fetches and pushes.
pub struct GitHttpBackend {
    /// The base url under which all repositories are hosted, like `http://127.0.0.1:port`.
    pub url: String,
    shutdown: Arc<AtomicBool>,
    server_addr: SocketAddr,
    listener_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for GitHttpBackend {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        TcpStream::connect(self.server_addr).ok();
        if let Some(listener_thread) = self.listener_thread.take() {
            listener_thread.join().ok();
        }
    }
}

/// Serve all repositories at or below `project_root` over HTTP with `git http-backend`, isolated like [`git_command()`](crate::git_command()).
///
/// If `basic_auth` is `Some((user, password))`, requests without these credentials are answered with
/// `401 Unauthorized`, as servers do that require authentication. Pushes are always allowed.
pub fn spawn_git_http_backend(
    project_root: impl AsRef<Path>,
    basic_auth: Option<(&str, &str)>,
) -> std::io::Result<GitHttpBackend> {
    let project_root = project_root.as_ref().to_owned();
    let authorization =
        basic_auth.map(|(user, password)| format!("Basic {}", base64(format!("{user}:{password}").as_bytes())));
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let server_addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let listener_thread = std::thread::spawn({
        let shutdown = shutdown.clone();
        move || {
            for stream in listener.incoming() {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                // Errors show up as failures of the client, which is under test.
                serve(stream, &project_root, authorization.as_deref()).ok();
            }
        }
    });
    Ok(GitHttpBackend {
        url: format!("http://{server_addr}"),
        shutdown,
        server_addr,
        listener_thread: Some(listener_thread),
    })
}

/// Answer a single request on `stream` and close the connection.
fn serve(stream: TcpStream, project_root: &Path, authorization: Option<&str>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let request_line = read_line(&mut reader)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(std::io::Error::other(format!("invalid request line {request_line:?}")));
    };
    let mut headers = HashMap::new();
    loop {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    if headers
        .get("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut body = Vec::new();
    if let Some(len) = headers.get("content-length") {
        let len = len.parse().map_err(std::io::Error::other)?;
        body.resize(len, 0);
        reader.read_exact(&mut body)?;
    } else if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let size_line = read_line(&mut reader)?;
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(std::io::Error::other)?;
            if size == 0 {
                while !read_line(&mut reader)?.is_empty() {}
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            read_line(&mut reader)?;
        }
    }

    if let Some(expected) = authorization
        && headers.get("authorization").map(String::as_str) != Some(expected)
    {
        return out.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut child = crate::git_command(project_root)
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", "gitoxide")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", headers.get("content-type").map_or("", String::as_str))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env(
            "HTTP_GIT_PROTOCOL",
            headers.get("git-protocol").map_or("", String::as_str),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || stdin.write_all(&body));
    let output = child.wait_with_output()?;
    writer
        .join()
        .map_err(|_| std::io::Error::other("writing the request body panicked"))??;

    // The CGI response starts with headers, of which `Status` becomes the status line.
    let (cgi_headers, cgi_body) = output
        .stdout
        .find(b"\r\n\r\n")
        .map(|pos| (&output.stdout[..pos], &output.stdout[pos + 4..]))
        .or_else(|| {
            output
                .stdout
                .find(b"\n\n")
                .map(|pos| (&output.stdout[..pos], &output.stdout[pos + 2..]))
        })
        .ok_or_else(|| std::io::Error::other("git http-backend didn't send headers"))?;
    let mut status = "200 OK".to_owned();
    let mut response = Vec::new();
    for line in ByteSlice::lines(cgi_headers) {
        let line = line.to_str_lossy();
        match line.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("status") => value.trim().clone_into(&mut status),
            _ => response.extend_from_slice(format!("{line}\r\n").as_bytes()),
        }
    }
    out.write_all(format!("HTTP/1.1 {status}\r\n").as_bytes())?;
    out.write_all(&response)?;
    out.write_all(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", cgi_body.len()).as_bytes())?;
    out.write_all(cgi_body)?;
    out.flush()
}

fn read_line(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let bytes = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        for idx in 0..4 {
            if idx <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * idx)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        for (input, expected) in [
            ("", ""),
            ("a", "YQ=="),
            ("ab", "YWI="),
            ("abc", "YWJj"),
            ("user:pass", "dXNlcjpwYXNz"),
        ] {
            assert_eq!(
                super::base64(input.as_bytes()),
                expected,
                "{input:?} is encoded with padding"
            );
        }
    }
}
