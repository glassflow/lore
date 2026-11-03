//! Downloads model files from Hugging Face into the hf-hub cache layout
//! (`models--<org>--<name>/{refs/main, snapshots/<commit>/<file>}`) that fastembed used, so
//! existing caches keep working. fastembed's own download (hf-hub over ureq) has no timeout:
//! once it stopped at 5.7 MB for good, and the server never started. This one has a deadline,
//! gives up on a connection that sends nothing for a while, retries with backoff, resumes a
//! partial file and logs its progress.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use reqwest::header::{LOCATION, RANGE};
use reqwest::{Response, StatusCode, Url};

/// Default `HF_ENDPOINT`.
pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";
/// An attempt that receives nothing for this long is abandoned and retried.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Retries of one file after its first attempt.
const MAX_RETRIES: u32 = 4;
/// The wait before the first retry. It doubles for each next one: 1s, 2s, 4s, 8s.
const DEFAULT_RETRY_BASE: Duration = Duration::from_secs(1);
/// A progress line is logged each time this many more bytes of a file have arrived.
const PROGRESS_STEP: u64 = 10 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;

/// Where to download from, and how patiently.
#[derive(Debug, Clone)]
pub struct Download {
    /// Base URL of the hub, without a trailing slash.
    pub endpoint: String,
    /// All files, retries included, must be in within this.
    pub deadline: Duration,
    pub stall_timeout: Duration,
    pub retries: u32,
    pub retry_base: Duration,
}

impl Download {
    /// Downloads from `HF_ENDPOINT` (Hugging Face when unset) within `deadline`.
    pub fn new(deadline: Duration) -> Self {
        Self::with_endpoint(&endpoint(std::env::var("HF_ENDPOINT").ok()), deadline)
    }

    pub fn with_endpoint(endpoint: &str, deadline: Duration) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            deadline,
            stall_timeout: STALL_TIMEOUT,
            retries: MAX_RETRIES,
            retry_base: DEFAULT_RETRY_BASE,
        }
    }
}

/// The value of `HF_ENDPOINT`, or Hugging Face when it is unset or empty.
fn endpoint(var: Option<String>) -> String {
    var.map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

/// The snapshot directory that holds `files` of the model repo `repo` under `cache_dir`,
/// downloading them first when they are not all there. Blocks; the download runs on a thread
/// with its own runtime, so this may be called from inside an async runtime too.
pub fn fetch(
    repo: &str,
    files: &[&str],
    cache_dir: &Path,
    download: &Download,
) -> anyhow::Result<PathBuf> {
    let repo_dir = cache_dir.join(format!("models--{}", repo.replace('/', "--")));
    if let Some(snapshot) = cached(&repo_dir, files) {
        return Ok(snapshot);
    }
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting the model download runtime")?
                .block_on(download_all(repo, files, cache_dir, &repo_dir, download))
        })
        .join()
        .map_err(|_| anyhow!("the model download thread panicked"))?
    })
}

/// The snapshot `refs/main` points to, when it has every one of `files`.
fn cached(repo_dir: &Path, files: &[&str]) -> Option<PathBuf> {
    let commit = std::fs::read_to_string(repo_dir.join("refs").join("main")).ok()?;
    let snapshot = repo_dir.join("snapshots").join(commit.trim());
    files
        .iter()
        .all(|f| snapshot.join(f).exists())
        .then_some(snapshot)
}

/// What the error says when the deadline passes in the middle of a file.
struct Status {
    url: String,
    last_error: Option<String>,
}

async fn download_all(
    repo: &str,
    files: &[&str],
    cache_dir: &Path,
    repo_dir: &Path,
    d: &Download,
) -> anyhow::Result<PathBuf> {
    let status = Mutex::new(Status {
        url: format!("{}/{repo}", d.endpoint),
        last_error: None,
    });
    let started = Instant::now();
    tracing::info!(
        repo,
        endpoint = %d.endpoint,
        dir = %cache_dir.display(),
        timeout_secs = d.deadline.as_secs(),
        "downloading the embedding model"
    );
    let result = tokio::time::timeout(
        d.deadline,
        download_files(repo, files, repo_dir, d, &status),
    )
    .await;
    let status = status.into_inner().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(Ok(snapshot)) => {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "embedding model downloaded"
            );
            Ok(snapshot)
        }
        Ok(Err(e)) => Err(anyhow!(
            "downloading the embedding model from {} into {} failed: {e:#}. Copy the models \
             directory of another install there to start without the download",
            status.url,
            cache_dir.display()
        )),
        Err(_) => {
            let last = status
                .last_error
                .map(|e| format!(" (last error: {e})"))
                .unwrap_or_default();
            Err(anyhow!(
                "downloading the embedding model from {} into {} did not finish within {}s{last}. \
                 Check that lore can reach {}, raise LORE_MODEL_DOWNLOAD_TIMEOUT_SECS, or copy \
                 the models directory of another install there",
                status.url,
                cache_dir.display(),
                d.deadline.as_secs_f64(),
                d.endpoint
            ))
        }
    }
}

async fn download_files(
    repo: &str,
    files: &[&str],
    repo_dir: &Path,
    d: &Download,
    status: &Mutex<Status>,
) -> anyhow::Result<PathBuf> {
    let http = reqwest::Client::builder()
        .connect_timeout(d.stall_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("lore/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")?;
    let mut commit: Option<String> = None;
    for file in files {
        // Once the first file named its commit, the others come from that commit too, even if
        // `main` moves on in the meantime.
        let revision = commit.as_deref().unwrap_or("main");
        let url = format!("{}/{repo}/resolve/{revision}/{file}", d.endpoint);
        set_status(status, |s| s.url = url.clone());
        let mut attempt: u32 = 0;
        let got = loop {
            match fetch_file(&http, d, &url, repo_dir, file, commit.as_deref()).await {
                Ok(got) => break got,
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e)) => {
                    if attempt >= d.retries {
                        return Err(e.context(format!("gave up after {} attempts", attempt + 1)));
                    }
                    let error = format!("{e:#}");
                    tracing::warn!(file, attempt, error, "model download failed, retrying");
                    set_status(status, |s| s.last_error = Some(error));
                    tokio::time::sleep(d.retry_base * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
            }
        };
        commit.get_or_insert(got);
    }
    let commit = commit.ok_or_else(|| anyhow!("no files to download"))?;
    // Written last, so a partial download never looks complete, to this code or to hf-hub.
    let refs = repo_dir.join("refs");
    std::fs::create_dir_all(&refs).with_context(|| format!("creating {}", refs.display()))?;
    std::fs::write(refs.join("main"), &commit)
        .with_context(|| format!("writing {}", refs.join("main").display()))?;
    Ok(repo_dir.join("snapshots").join(commit))
}

fn set_status(status: &Mutex<Status>, f: impl FnOnce(&mut Status)) {
    f(&mut status.lock().unwrap_or_else(|e| e.into_inner()));
}

enum Failure {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

fn retry(e: impl Into<anyhow::Error>) -> Failure {
    Failure::Retry(e.into())
}

fn fatal(e: impl Into<anyhow::Error>) -> Failure {
    Failure::Fatal(e.into())
}

/// One attempt at `file` from `url`, into `snapshots/<commit>/<file>`. Returns the commit,
/// which is `commit` when it is known and otherwise read from the hub's `x-repo-commit`.
async fn fetch_file(
    http: &reqwest::Client,
    d: &Download,
    url: &str,
    repo_dir: &Path,
    file: &str,
    commit: Option<&str>,
) -> Result<String, Failure> {
    // A partial file left by an earlier attempt is resumed. Its commit must be known, or the
    // rest could come from another version of the file.
    let earlier = commit.map(|c| partial_path(repo_dir, c, file));
    let offset = earlier
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len());
    let (mut res, header_commit) = send(http, d, url, offset).await?;

    let resumed = match res.status() {
        StatusCode::OK => false,
        StatusCode::PARTIAL_CONTENT if offset > 0 => true,
        StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
            // The partial file is complete or longer than the file now: start over.
            if let Some(p) = &earlier {
                let _ = std::fs::remove_file(p);
            }
            return Err(retry(anyhow!("HTTP 416 resuming at byte {offset}")));
        }
        s if s.is_client_error() && !matches!(s.as_u16(), 408 | 429) => {
            return Err(fatal(anyhow!("HTTP {s}")));
        }
        s => return Err(retry(anyhow!("HTTP {s}"))),
    };
    let commit = commit
        .map(str::to_string)
        .or(header_commit)
        .ok_or_else(|| fatal(anyhow!("the response has no x-repo-commit header")))?;
    let partial = partial_path(repo_dir, &commit, file);

    if let Some(dir) = partial.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))
            .map_err(fatal)?;
    }
    let mut out = if resumed {
        OpenOptions::new().append(true).open(&partial)
    } else {
        File::create(&partial)
    }
    .with_context(|| format!("opening {}", partial.display()))
    .map_err(fatal)?;

    let mut done = if resumed { offset } else { 0 };
    let total = res.content_length().map(|n| n + done);
    let total_mb = total.map_or_else(|| "unknown".to_string(), |t| (t >> 20).to_string());
    let mut next_log = (done / PROGRESS_STEP + 1) * PROGRESS_STEP;
    loop {
        let chunk = match tokio::time::timeout(d.stall_timeout, res.chunk()).await {
            Err(_) => {
                return Err(retry(anyhow!(
                    "no data for {}s after {done} bytes of {file}",
                    d.stall_timeout.as_secs_f64()
                )));
            }
            Ok(Err(e)) => return Err(retry(e)),
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        out.write_all(&chunk)
            .with_context(|| format!("writing {}", partial.display()))
            .map_err(fatal)?;
        done += chunk.len() as u64;
        if done >= next_log {
            tracing::info!(
                file,
                downloaded_mb = done >> 20,
                total_mb,
                "downloading the embedding model"
            );
            next_log += PROGRESS_STEP;
        }
    }
    out.flush()
        .with_context(|| format!("writing {}", partial.display()))
        .map_err(fatal)?;
    drop(out);
    if let Some(total) = total
        && done != total
    {
        return Err(retry(anyhow!(
            "the connection closed after {done} of {total} bytes of {file}"
        )));
    }
    let path = repo_dir.join("snapshots").join(&commit).join(file);
    std::fs::rename(&partial, &path)
        .with_context(|| format!("moving {} to {}", partial.display(), path.display()))
        .map_err(fatal)?;
    Ok(commit)
}

fn partial_path(repo_dir: &Path, commit: &str, file: &str) -> PathBuf {
    repo_dir
        .join("snapshots")
        .join(commit)
        .join(format!("{file}.incomplete"))
}

/// GETs `url` from byte `offset` on, following redirects. Also returns the first
/// `x-repo-commit` header on the way: the hub sends it, the CDN it redirects to does not.
async fn send(
    http: &reqwest::Client,
    d: &Download,
    url: &str,
    offset: u64,
) -> Result<(Response, Option<String>), Failure> {
    let mut url = Url::parse(url)
        .with_context(|| format!("parsing {url}"))
        .map_err(fatal)?;
    let mut commit = None;
    for _ in 0..=MAX_REDIRECTS {
        let mut req = http.get(url.clone());
        if offset > 0 {
            req = req.header(RANGE, format!("bytes={offset}-"));
        }
        let res = tokio::time::timeout(d.stall_timeout, req.send())
            .await
            .map_err(|_| {
                retry(anyhow!(
                    "no response from {url} for {}s",
                    d.stall_timeout.as_secs_f64()
                ))
            })?
            .map_err(retry)?;
        if commit.is_none() {
            commit = res
                .headers()
                .get("x-repo-commit")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
        if !res.status().is_redirection() {
            return Ok((res, commit));
        }
        let location = res
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| retry(anyhow!("HTTP {} without a location", res.status())))?;
        url = url
            .join(location)
            .with_context(|| format!("parsing the redirect to {location}"))
            .map_err(fatal)?;
    }
    Err(fatal(anyhow!("more than {MAX_REDIRECTS} redirects")))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    use super::*;

    const REPO: &str = "org/model";
    const FILES: [&str; 2] = ["config.json", "onnx/model.onnx"];

    /// What the test server answers one request with.
    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        /// Send this many bytes of the body, then nothing more.
        stall_at: Option<usize>,
    }

    impl Reply {
        fn new(status: u16, body: &[u8]) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_vec(),
                stall_at: None,
            }
        }

        fn header(mut self, name: &'static str, value: &str) -> Self {
            self.headers.push((name, value.to_string()));
            self
        }
    }

    type Requests = Arc<Mutex<Vec<String>>>;

    /// An HTTP/1.1 server answering each connection's one request with `handler(path, head)`.
    /// `None` answers nothing and keeps the connection open. Returns the base URL and the
    /// heads of the requests it got, lowercased.
    fn server(
        handler: impl Fn(&str, &str) -> Option<Reply> + Send + Sync + 'static,
    ) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests: Requests = Default::default();
        let handler = Arc::new(handler);
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (handler, seen) = (handler.clone(), seen.clone());
                std::thread::spawn(move || answer(stream, &*handler, &seen));
            }
        });
        (base, requests)
    }

    fn answer(
        mut stream: TcpStream,
        handler: &dyn Fn(&str, &str) -> Option<Reply>,
        seen: &Mutex<Vec<String>>,
    ) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_lowercase();
        let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
        seen.lock().unwrap().push(head.clone());
        let Some(reply) = handler(&path, &head) else {
            std::thread::sleep(Duration::from_secs(60));
            return;
        };
        let mut out = format!(
            "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
            reply.status,
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let sent = reply.stall_at.unwrap_or(reply.body.len());
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&reply.body[..sent]);
        let _ = stream.write_all(&bytes);
        let _ = stream.flush();
        if reply.stall_at.is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    fn download(endpoint: &str, deadline: Duration, stall_timeout: Duration) -> Download {
        Download {
            stall_timeout,
            retries: 2,
            retry_base: Duration::from_millis(1),
            ..Download::with_endpoint(endpoint, deadline)
        }
    }

    #[test]
    fn a_stalled_server_fails_within_the_deadline() {
        let (base, _) = server(|_, _| None);
        let dir = tempfile::tempdir().unwrap();
        let d = download(&base, Duration::from_secs(1), Duration::from_secs(30));
        let started = Instant::now();
        let err = fetch(REPO, &FILES, dir.path(), &d).unwrap_err().to_string();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(3),
            "{elapsed:?}"
        );
        assert!(err.contains("did not finish within 1s"), "{err}");
        assert!(
            err.contains(&format!("{base}/org/model/resolve/main/config.json")),
            "{err}"
        );
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
    }

    #[test]
    fn a_stalled_transfer_is_retried_and_resumed() {
        let (base, requests) = server(|path, head| match path {
            "/org/model/resolve/main/config.json" => Some(
                Reply::new(302, b"")
                    .header("location", "/cdn/config.json")
                    .header("x-repo-commit", "abc123"),
            ),
            "/cdn/config.json" => Some(Reply::new(200, b"{}")),
            "/org/model/resolve/abc123/onnx/model.onnx" if head.contains("range: bytes=4-") => {
                Some(Reply::new(206, b"456789"))
            }
            "/org/model/resolve/abc123/onnx/model.onnx" => Some(Reply {
                stall_at: Some(4),
                ..Reply::new(200, b"0123456789")
            }),
            _ => Some(Reply::new(404, b"")),
        });
        let dir = tempfile::tempdir().unwrap();
        let d = download(&base, Duration::from_secs(10), Duration::from_millis(300));
        let snapshot = fetch(REPO, &FILES, dir.path(), &d).unwrap();

        let repo_dir = dir.path().join("models--org--model");
        assert_eq!(snapshot, repo_dir.join("snapshots/abc123"));
        assert_eq!(std::fs::read(snapshot.join("config.json")).unwrap(), b"{}");
        assert_eq!(
            std::fs::read(snapshot.join("onnx/model.onnx")).unwrap(),
            b"0123456789"
        );
        assert!(!snapshot.join("onnx/model.onnx.incomplete").exists());
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("refs/main")).unwrap(),
            "abc123"
        );
        let model_requests: Vec<String> = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.contains("model.onnx"))
            .cloned()
            .collect();
        assert_eq!(model_requests.len(), 2, "{model_requests:?}");
        assert!(!model_requests[0].contains("range:"));
        assert!(model_requests[1].contains("range: bytes=4-"));

        // A second start finds the files and asks for nothing.
        let before = requests.lock().unwrap().len();
        assert_eq!(fetch(REPO, &FILES, dir.path(), &d).unwrap(), snapshot);
        assert_eq!(requests.lock().unwrap().len(), before);
    }

    #[test]
    fn a_client_error_is_not_retried() {
        let (base, requests) =
            server(|_, _| Some(Reply::new(404, b"").header("x-repo-commit", "abc123")));
        let dir = tempfile::tempdir().unwrap();
        let d = download(&base, Duration::from_secs(10), Duration::from_secs(10));
        let err = fetch(REPO, &FILES, dir.path(), &d).unwrap_err().to_string();
        assert!(err.contains("HTTP 404"), "{err}");
        assert!(
            err.contains(&format!("{base}/org/model/resolve/main/config.json")),
            "{err}"
        );
        assert!(err.contains(&dir.path().display().to_string()), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn server_errors_are_retried_then_reported() {
        let (base, requests) = server(|_, _| Some(Reply::new(503, b"")));
        let dir = tempfile::tempdir().unwrap();
        let d = download(&base, Duration::from_secs(10), Duration::from_secs(10));
        let err = fetch(REPO, &FILES, dir.path(), &d).unwrap_err().to_string();
        assert!(err.contains("3 attempts"), "{err}");
        assert!(err.contains("HTTP 503"), "{err}");
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn an_existing_cache_needs_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("models--org--model");
        let snapshot = repo_dir.join("snapshots/abc123");
        std::fs::create_dir_all(snapshot.join("onnx")).unwrap();
        std::fs::write(snapshot.join("config.json"), "{}").unwrap();
        std::fs::write(snapshot.join("onnx/model.onnx"), "model").unwrap();
        std::fs::create_dir_all(repo_dir.join("refs")).unwrap();
        std::fs::write(repo_dir.join("refs/main"), "abc123").unwrap();
        // Nothing listens on port 9.
        let d = download("http://127.0.0.1:9", Duration::from_secs(1), STALL_TIMEOUT);
        assert_eq!(fetch(REPO, &FILES, dir.path(), &d).unwrap(), snapshot);
    }

    #[test]
    fn the_endpoint_comes_from_hf_endpoint() {
        assert_eq!(endpoint(None), DEFAULT_ENDPOINT);
        assert_eq!(endpoint(Some(" ".into())), DEFAULT_ENDPOINT);
        assert_eq!(
            endpoint(Some("https://hf-mirror.com/".into())),
            "https://hf-mirror.com"
        );
    }
}
