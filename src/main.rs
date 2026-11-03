use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lore::cli::{AdminCommand, AskArgs, Client, DocCommand, OtlpCommand, RepoCommand, SearchArgs};
use lore::config::{Config, LogFormat};
use lore::http::{AppState, router};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "lore",
    version,
    about = "Ask questions of your traces, sessions and code"
)]
struct Cli {
    /// Base URL of the lore server (client commands)
    #[arg(
        long,
        global = true,
        env = "LORE_URL",
        default_value = "http://localhost:8080"
    )]
    url: String,
    /// Tenant API key (client commands)
    #[arg(long, global = true, env = "LORE_KEY", hide_env_values = true)]
    key: Option<String>,
    /// Admin token (admin commands)
    #[arg(long, global = true, env = "LORE_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (applies migrations on start)
    Serve,
    /// Apply database migrations and exit
    Migrate,
    /// Manage tenants and API keys (needs the admin token)
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Show the tenant and scopes of the API key
    Me,
    /// Index git repos and list them (needs an API key)
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Send OpenTelemetry traces to an OTLP/HTTP endpoint (needs an API key)
    Otlp {
        #[command(subcommand)]
        command: OtlpCommand,
    },
    /// Write, delete and read notes and documents (needs an API key)
    Doc {
        #[command(subcommand)]
        command: DocCommand,
    },
    /// Search the tenant's indexed code, docs, sessions and notes (needs an API key)
    Search(SearchArgs),
    /// Ask a question and get an answer with citations (needs an API key)
    Ask(AskArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let config = Config::from_env()?;
            init_tracing(&config)?;
            serve(config).await
        }
        Command::Migrate => {
            let config = Config::from_env()?;
            init_tracing(&config)?;
            let pool = lore::db::connect(&config.database_url).await?;
            lore::db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Admin { command } => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_admin(&client, command).await
        }
        Command::Me => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_me(&client).await
        }
        Command::Repo { command } => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_repo(&client, command).await
        }
        Command::Otlp { command } => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_otlp(&client, command).await
        }
        Command::Doc { command } => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_doc(&client, command).await
        }
        Command::Search(args) => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_search(&client, args).await
        }
        Command::Ask(args) => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_ask(&client, args).await
        }
    }
}

fn init_tracing(config: &Config) -> Result<()> {
    let filter = EnvFilter::try_new(&config.log)
        .with_context(|| format!("invalid LORE_LOG filter {:?}", config.log))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
    Ok(())
}

async fn serve(config: Config) -> Result<()> {
    // Build the model provider first: a missing key should stop the start before anything else.
    let (llm, models) = lore::llm::from_config(&config)?;
    tracing::info!(
        llm = llm.name(),
        fast_model = %models.fast.model,
        deep_model = %models.deep.model,
        "model provider ready"
    );

    let pool = lore::db::connect(&config.database_url).await?;
    lore::db::migrate(&pool).await?;

    // Load the embedder before binding, so the first-start model download finishes before the
    // server answers anything. Loading is blocking and CPU/network heavy.
    let started = std::time::Instant::now();
    let embedder = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || lore::embed::from_config(&config))
            .await
            .context("embedder load task failed")??
    };
    tracing::info!(
        embedder = embedder.name(),
        load_ms = started.elapsed().as_millis() as u64,
        "embedder ready"
    );

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(addr = %config.listen_addr, version = %config.version, "lore listening");

    let state = AppState {
        pool,
        config: Arc::new(config),
        embedder,
        llm,
        models,
        source_locks: Default::default(),
        pushes: Default::default(),
        deep_guard: Default::default(),
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let syncer = state.config.sync_interval_secs.map(|secs| {
        tracing::info!(interval_secs = secs, "repo sync poller on");
        tokio::spawn(sync_worker(
            state.clone(),
            std::time::Duration::from_secs(secs),
            stop_rx.clone(),
        ))
    });
    let worker = tokio::spawn(session_worker(state.clone(), stop_rx));
    let served = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = stop_tx.send(true);
        })
        .await
        .context("serving");
    // If serving failed, the sender is dropped by now and the worker stops as well.
    if let Err(e) = worker.await {
        tracing::error!(error = %e, "session worker task failed");
    }
    if let Some(syncer) = syncer
        && let Err(e) = syncer.await
    {
        tracing::error!(error = %e, "sync worker task failed");
    }
    served?;
    tracing::info!("shut down");
    Ok(())
}

/// How often the worker looks for sessions that have gone quiet.
const SESSION_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Sessions indexed per poll.
const SESSION_BATCH: i64 = 20;

/// Turns quiet sessions into documents until `stop` changes. An error is logged and the next
/// poll tries again.
async fn session_worker(state: AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(SESSION_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {}
        }
        match lore::sessions::process_due_sessions(&state, SESSION_BATCH).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(sessions = n, "sessions processed"),
            Err(e) => tracing::error!(error = ?e, "session worker failed"),
        }
    }
}

/// Fetches and indexes every repo that has a remote, every `interval`, until `stop` changes. A
/// stop waits for the sync in progress to end.
async fn sync_worker(
    state: AppState,
    interval: std::time::Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {}
        }
        match lore::repo::sync::sync_all(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(repos = n, "repos synced"),
            Err(e) => tracing::error!(error = ?e, "sync worker failed"),
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
