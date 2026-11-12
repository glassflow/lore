//! The per-call GitHub token of `POST /v1/repos/{id}/sync`: it reaches a private remote, and it
//! is never stored or logged. The remote is a real HTTPS server (a self-signed certificate, git
//! told to trust it) that answers only a request with the right basic-auth header and then
//! serves a bare repo through `git http-backend`.

mod common;

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{
    body_json, create_tenant_key, fixture_repo, git, head, send, test_router, test_state_with,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{ServerConfig, crypto::ring};
use tower::ServiceExt;
use uuid::Uuid;

// Fake tokens, each test its own so one test's logs cannot hide another's leak.
const TOKEN: &str = "ghp_FakeTokenForTheHappyPath0001";
const REFUSED: &str = "No token of this kind is accepted";

// ---- captured logs ----

#[derive(Clone)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> LogSink {
        self.clone()
    }
}

/// Everything logged by this test binary so far, at every level. The subscriber is global
/// because a sync runs in a spawned task on another thread.
fn logs() -> String {
    static SINK: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    let sink = SINK.get_or_init(|| {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(LogSink(sink.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        sink
    });
    String::from_utf8_lossy(&sink.lock().unwrap()).into_owned()
}

// ---- the private remote ----

struct Shared {
    /// The directory that holds `repo.git`.
    root: PathBuf,
    /// The token the remote accepts.
    token: String,
    /// A wrong token is answered with a git error that repeats what the client sent, as a
    /// remote that echoes credentials would.
    echo: bool,
    /// Requests that carried the right token.
    authed: AtomicUsize,
}

struct Remote {
    url: String,
    shared: Arc<Shared>,
    cert_pem: String,
    _dir: TempDir,
}

struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok((stream, addr)) = self.tcp.accept().await else {
                continue;
            };
            if let Ok(io) = self.acceptor.accept(stream).await {
                return (io, addr);
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

/// A bare copy of `source` served over HTTPS at `url`, for a client that sends `token`.
async fn serve_remote(source: &Path, token: &str, echo: bool) -> Remote {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("repo.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &source.to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );

    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.cert.der().to_vec())], key)
        .unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let listener = TlsListener {
        tcp,
        acceptor: TlsAcceptor::from(Arc::new(config)),
    };

    let shared = Arc::new(Shared {
        root: dir.path().to_path_buf(),
        token: token.to_string(),
        echo,
        authed: AtomicUsize::new(0),
    });
    let app = Router::new().fallback(handle).with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Remote {
        url: format!("https://{addr}/repo.git"),
        shared,
        cert_pem: cert.cert.pem(),
        _dir: dir,
    }
}

async fn handle(State(shared): State<Arc<Shared>>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let want = format!(
        "Basic {}",
        STANDARD.encode(format!("x-access-token:{}", shared.token))
    );
    let got = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != want {
        if shared.echo && !got.is_empty() {
            let sent = got.trim_start_matches("Basic ");
            let sent =
                String::from_utf8_lossy(&STANDARD.decode(sent).unwrap_or_default()).into_owned();
            let msg = format!("denied: you sent {got} which is {sent}");
            // One pkt-line `ERR`, which git prints as "remote error: ...".
            let pkt = format!("{:04x}ERR {msg}\n", msg.len() + 9);
            return Response::builder()
                .header(
                    "content-type",
                    "application/x-git-upload-pack-advertisement",
                )
                .body(Body::from(pkt))
                .unwrap();
        }
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("www-authenticate", "Basic realm=\"fake\"")
            .body(Body::empty())
            .unwrap();
    }
    shared.authed.fetch_add(1, Ordering::SeqCst);

    let body = axum::body::to_bytes(body, 64 << 20).await.unwrap();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_PROJECT_ROOT", &shared.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("HTTP_GIT_PROTOCOL", header("git-protocol"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let out = child.wait_with_output().await.unwrap().stdout;
    feed.await.unwrap();
    cgi_response(&out)
}

/// A CGI answer (headers, a blank line, the body) as an HTTP response.
fn cgi_response(out: &[u8]) -> Response<Body> {
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("CGI headers");
    let mut res = Response::builder();
    for line in String::from_utf8_lossy(&out[..split]).lines() {
        let (name, value) = line.split_once(": ").expect("CGI header");
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().unwrap();
            res = res.status(code.parse::<u16>().unwrap());
        } else {
            res = res.header(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
    }
    res.body(Body::from(out[split + 4..].to_vec())).unwrap()
}

// ---- the lore side ----

struct Env {
    router: Router,
    data: TempDir,
    key: String,
}

async fn env(pool: PgPool, tenant: &str) -> Env {
    logs(); // starts the capture before anything is logged
    let data = tempfile::tempdir().unwrap();
    let state = test_state_with(pool, data.path(), |c| c.sync_allow_file_remotes = true);
    let router = test_router(state);
    let (_, key) = create_tenant_key(&router, tenant, &["ingest", "read"]).await;
    Env { router, data, key }
}

/// Creates the repo `app` with `remote_url`. For an HTTPS remote, the mirror is made first with
/// git told to trust the remote's certificate (lore leaves a mirror that exists alone).
async fn create_repo(env: &Env, remote_url: &str, cert_pem: Option<&str>) -> Uuid {
    let body = json!({ "name": "app", "remote_url": remote_url });
    let (status, repo) = send(
        &env.router,
        "POST",
        "/v1/repos",
        &env.key,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{repo}");
    let id: Uuid = repo["id"].as_str().unwrap().parse().unwrap();
    if let Some(pem) = cert_pem {
        let repo_dir = env.data.path().join("repos").join(id.to_string());
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("ca.pem"), pem).unwrap();
        let mirror = repo_dir.join("mirror.git");
        git(&repo_dir, &["init", "--bare", "-q", "mirror.git"]);
        git(
            &mirror,
            &[
                "config",
                "http.sslCAInfo",
                &repo_dir.join("ca.pem").to_string_lossy(),
            ],
        );
    }
    id
}

/// An HTTPS source whose remote accepts `token`.
async fn private_repo(pool: PgPool, tenant: &str, token: &str, echo: bool) -> (Env, Remote, Uuid) {
    let env = env(pool, tenant).await;
    let upstream = fixture_repo();
    let remote = serve_remote(upstream.path(), token, echo).await;
    let id = create_repo(&env, &remote.url, Some(&remote.cert_pem)).await;
    // The sample repo's head, for the tests to compare with.
    std::fs::write(env.data.path().join("head"), head(&upstream)).unwrap();
    (env, remote, id)
}

fn upstream_head(env: &Env) -> String {
    std::fs::read_to_string(env.data.path().join("head")).unwrap()
}

/// `POST /v1/repos/{id}/sync` with `body` (none: no body and no content-type at all).
async fn sync(env: &Env, id: Uuid, body: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::post(format!("/v1/repos/{id}/sync"))
        .header("authorization", format!("Bearer {}", env.key));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let res = env
        .router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (status, body_json(res).await)
}

async fn sync_with_token(env: &Env, id: Uuid, token: &str) -> (StatusCode, Value) {
    sync(env, id, Some(&json!({ "token": token }).to_string())).await
}

async fn last_sync_error(pool: &PgPool, id: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT last_sync_error FROM sources WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The token as it could appear: itself, and as the basic-auth value git sends.
fn needles(token: &str) -> Vec<String> {
    vec![
        token.to_string(),
        STANDARD.encode(format!("x-access-token:{token}")),
    ]
}

/// Every place the token could have been kept: the logs, each table (a whole row as text, so
/// every column, vectors and json included), the admin overview and the data directory.
/// Returns what holds a needle.
async fn find_leaks(env: &Env, pool: &PgPool, token: &str) -> Vec<String> {
    let mut found = Vec::new();
    let log = logs();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(tables.len() > 5, "the tables to scan: {tables:?}");
    let overview = {
        let req = Request::get("/v1/admin/overview")
            .header("authorization", "Bearer t")
            .body(Body::empty())
            .unwrap();
        let res = env.router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        body_json(res).await.to_string()
    };
    for needle in needles(token) {
        if log.contains(&needle) {
            found.push(format!("log has {needle}"));
        }
        if overview.contains(&needle) {
            found.push(format!("admin overview has {needle}"));
        }
        for table in &tables {
            let sql = format!("SELECT count(*) FROM \"{table}\" r WHERE strpos(r::text, $1) > 0");
            let n: i64 = sqlx::query_scalar(&sql)
                .bind(&needle)
                .fetch_one(pool)
                .await
                .unwrap();
            if n > 0 {
                found.push(format!("table {table} has {needle}"));
            }
        }
        for file in files_under(env.data.path()) {
            let bytes = std::fs::read(&file).unwrap();
            if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                found.push(format!("{} has {needle}", file.display()));
            }
        }
    }
    found
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

// ---- tests ----

#[sqlx::test(migrations = "./migrations")]
async fn a_private_repo_syncs_with_the_token_in_the_body(pool: PgPool) {
    let (env, remote, id) = private_repo(pool.clone(), "acme", TOKEN, false).await;

    let (status, report) = sync_with_token(&env, id, TOKEN).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["indexed_commit"], upstream_head(&env));
    assert_eq!(report["full"], true);
    assert!(remote.shared.authed.load(Ordering::SeqCst) > 0);
    assert_eq!(last_sync_error(&pool, id).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn without_a_token_the_answer_is_a_clear_502(pool: PgPool) {
    let (env, remote, id) = private_repo(pool.clone(), "acme", TOKEN, false).await;

    for body in [None, Some("{}"), Some(r#"{"token": ""}"#)] {
        let (status, err) = sync(&env, id, body).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{err}");
        let message = err["error"].as_str().unwrap_or_else(|| panic!("{err}"));
        assert!(
            message.contains("the remote refused authentication"),
            "{err}"
        );
        assert!(message.contains("token"), "it says what to send: {err}");
    }
    assert_eq!(remote.shared.authed.load(Ordering::SeqCst), 0);
    let stored = last_sync_error(&pool, id)
        .await
        .expect("the failure is recorded");
    assert!(
        stored.starts_with("remote refused authentication"),
        "{stored}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_token_is_kept_nowhere_after_a_sync(pool: PgPool) {
    let token = "ghp_FakeTokenThatMustNotBeStored02";
    let (env, _remote, id) = private_repo(pool.clone(), "acme", token, false).await;

    let (status, report) = sync_with_token(&env, id, token).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    // Syncing again goes through the same paths with an up to date mirror.
    let (status, again) = sync_with_token(&env, id, token).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["up_to_date"], true);

    assert!(!logs().is_empty(), "the log capture works");
    let leaks = find_leaks(&env, &pool, token).await;
    assert!(leaks.is_empty(), "{leaks:?}");
    assert!(!report.to_string().contains(token));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_wrong_token_is_a_502_that_does_not_leak_it(pool: PgPool) {
    // The remote echoes what it was sent in its error, so git's message holds the wrong token.
    let wrong = "ghp_FakeWrongTokenThatMustBeScrubbed03";
    let (env, _remote, id) = private_repo(pool.clone(), "acme", REFUSED, true).await;

    let (status, err) = sync_with_token(&env, id, wrong).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{err}");
    let stored = last_sync_error(&pool, id)
        .await
        .expect("the failure is recorded");
    // The echo reached git's message and was scrubbed there: that is what is stored.
    assert!(stored.contains("denied: you sent"), "{stored}");
    assert!(stored.contains("[token]"), "{stored}");
    assert!(!err.to_string().contains(wrong), "{err}");
    let leaks = find_leaks(&env, &pool, wrong).await;
    assert!(leaks.is_empty(), "{leaks:?}");
    assert!(
        logs().contains("remote fetch failed"),
        "the failure is logged"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_rejected_wrong_token_is_a_502_too(pool: PgPool) {
    let wrong = "ghp_FakeWrongTokenThatIsRejected04";
    let (env, _remote, id) = private_repo(pool.clone(), "acme", REFUSED, false).await;

    let (status, err) = sync_with_token(&env, id, wrong).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{err}");
    assert!(err.to_string().contains("refused authentication"), "{err}");
    let leaks = find_leaks(&env, &pool, wrong).await;
    assert!(leaks.is_empty(), "{leaks:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn an_empty_body_syncs_as_before(pool: PgPool) {
    let env = env(pool.clone(), "acme").await;
    let remote = fixture_repo();
    let id = create_repo(&env, &format!("file://{}", remote.path().display()), None).await;

    // No body and no content-type, the way a plain `curl -X POST` sends it.
    let (status, report) = sync(&env, id, None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["indexed_commit"], head(&remote));
    for body in ["", "  \n", "{}", r#"{"token": null}"#, r#"{"token": ""}"#] {
        let (status, again) = sync(&env, id, Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{body:?}: {again}");
        assert_eq!(again["up_to_date"], true);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_malformed_body_is_a_400_that_does_not_echo_it(pool: PgPool) {
    let env = env(pool.clone(), "acme").await;
    let remote = fixture_repo();
    let id = create_repo(&env, &format!("file://{}", remote.path().display()), None).await;

    let secret = "ghp_FakeTokenInABadBody05";
    let too_long = json!({ "token": "x".repeat(2000) }).to_string();
    let control = json!({ "token": format!("{secret}\nmore") }).to_string();
    let bodies = [
        format!("not json {secret}"),
        format!(r#"{{"tokne": "{secret}"}}"#),
        format!(r#"{{"token": ["{secret}"]}}"#),
        format!(r#"{{"token": 5, "x": "{secret}"}}"#),
        too_long,
        control,
    ];
    for body in &bodies {
        let (status, err) = sync(&env, id, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:.60}: {err}");
        assert!(!err.to_string().contains(secret), "{err}");
    }
    // Nothing ran: the source was never synced.
    let commit: Option<String> =
        sqlx::query_scalar("SELECT indexed_commit FROM sources WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(commit, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_call_token_wins_over_the_environment_token(pool: PgPool) {
    // The tenant name, so the variable, is this test's own.
    let tenant = "envtoken";
    let (env, _remote, id) = private_repo(pool.clone(), tenant, TOKEN, false).await;
    let var = lore::repo::sync::token_var(tenant);
    // SAFETY: the variable is read only by syncs of this tenant, which only this test makes.
    unsafe { std::env::set_var(&var, "ghp_FakeWrongEnvToken06") };

    let (status, err) = sync(&env, id, None).await;
    assert_eq!(
        status,
        StatusCode::BAD_GATEWAY,
        "the env token is wrong: {err}"
    );
    let (status, report) = sync_with_token(&env, id, TOKEN).await;
    assert_eq!(status, StatusCode::OK, "the call token wins: {report}");

    // SAFETY: as above.
    unsafe { std::env::set_var(&var, TOKEN) };
    let (status, report) = sync(&env, id, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the env token is the fallback: {report}"
    );
    assert_eq!(report["up_to_date"], true);
    // SAFETY: as above.
    unsafe { std::env::remove_var(&var) };
}
