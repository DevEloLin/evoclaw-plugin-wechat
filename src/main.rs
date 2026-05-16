//! evoclaw-plugin-wechat — passive-reply webhook bridging WeChat Official
//! Account messages into a local `evoclaw` runtime via the stdio local-pipe
//! channel protocol.
//!
//! Run with `evoclaw-plugin-wechat run --config path/to/wechat.toml`.

mod bridge;
mod config;
mod conv_serializer;
mod digest_cache;
mod error;
mod intent;
mod util;
mod wechat;

#[cfg(test)]
mod test_fixtures;

use crate::bridge::BridgePool;
use crate::config::{Config, EncryptMode};
use crate::wechat::handler::{handle_message, verify_url, HandlerState};
use axum::{routing::get, Router};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

const DEFAULT_CONFIG_HINT: &str = "~/.evoclaw/plugins/wechat.toml";

/// Cap on incoming POST bodies. Real WeChat passive-reply payloads are
/// ~1 KB (small XML envelope). 64 KB is roughly 60× headroom — generous
/// enough to absorb future WeChat protocol additions without obviously
/// allowing abuse.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;

#[derive(Parser)]
#[command(
    name = "evoclaw-plugin-wechat",
    version,
    about = "WeChat Official Account passive-reply bridge for EvoClaw"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the webhook server.
    Run {
        /// Path to TOML config. Defaults to ~/.evoclaw/plugins/wechat.toml.
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
    /// Validate a config file without starting the server.
    Check {
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
    /// Print an example config to stdout.
    InitConfig,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::InitConfig => {
            print!("{}", include_str!("../config.example.toml"));
            Ok(())
        }
        Cmd::Check { config } => {
            let path = resolve_config_path(config)?;
            let cfg = Config::from_path(&path).await?;
            println!("✓ config valid: {}", path.display());
            println!(
                "  endpoint:     {}{}",
                cfg.server.bind, cfg.server.endpoint_path
            );
            println!("  encrypt_mode: {:?}", cfg.wechat.encrypt_mode);
            println!("  worker_count: {}", cfg.evoclaw.worker_count);
            println!("  timeout_ms:   {}", cfg.evoclaw.timeout_ms);
            println!("  max_chars:    {}", cfg.reply.max_chars);
            verify_evoclaw_binary(&cfg.evoclaw.binary).await?;
            Ok(())
        }
        Cmd::Run { config } => {
            let path = resolve_config_path(config)?;
            let cfg = Arc::new(Config::from_path(&path).await?);
            init_tracing(&cfg.log.level);
            tracing::info!(path = %path.display(), "loaded config");
            run_server(cfg).await
        }
    }
}

fn resolve_config_path(explicit: Option<PathBuf>) -> eyre::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let home =
        std::env::var("HOME").map_err(|_| eyre::eyre!("HOME not set and no --config given"))?;
    let p = PathBuf::from(home)
        .join(".evoclaw")
        .join("plugins")
        .join("wechat.toml");
    if !p.exists() {
        eyre::bail!(
            "no config at {} (expected: {}). Pass --config <path> or run \
             `evoclaw-plugin-wechat init-config > {}` to scaffold one.",
            p.display(),
            DEFAULT_CONFIG_HINT,
            p.display()
        );
    }
    Ok(p)
}

fn init_tracing(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("evoclaw_plugin_wechat={level},evoclaw=info")));
    // `.try_init()` so the binary doesn't panic if some embedder ever
    // installs a subscriber before us (e.g. inside an integration test).
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Spawn `<binary> --version` and bail if it fails. `check` runs this so
/// users find typos in `evoclaw.binary` immediately instead of at first
/// webhook hit. A 5-second timeout protects against a hung binary.
async fn verify_evoclaw_binary(binary: &str) -> eyre::Result<()> {
    use tokio::process::Command;
    use tokio::time::{timeout, Duration};
    let fut = Command::new(binary).arg("--version").output();
    let out = timeout(Duration::from_secs(5), fut)
        .await
        .map_err(|_| eyre::eyre!("`{binary} --version` took longer than 5s — wrong binary?"))?
        .map_err(|e| {
            eyre::eyre!(
                "could not spawn `{binary} --version`: {e}. \
                 Set `evoclaw.binary` to an absolute path in the config."
            )
        })?;
    if !out.status.success() {
        eyre::bail!(
            "`{binary} --version` exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v = String::from_utf8_lossy(&out.stdout);
    println!("✓ evoclaw reachable: {}", v.trim());
    Ok(())
}

async fn run_server(cfg: Arc<Config>) -> eyre::Result<()> {
    let bind: SocketAddr = cfg
        .server
        .bind
        .parse()
        .map_err(|e| eyre::eyre!("invalid server.bind '{}': {e}", cfg.server.bind))?;

    // Pre-decode the AES key once so per-request handlers don't redo it.
    let aes_key = match cfg.wechat.encrypt_mode {
        EncryptMode::Plain => None,
        EncryptMode::Compatible | EncryptMode::Safe => Some(Arc::new(
            wechat::crypto::decode_aes_key(&cfg.wechat.encoding_aes_key)
                .map_err(|e| eyre::eyre!("{e}"))?,
        )),
    };

    tracing::info!(
        binary = %cfg.evoclaw.binary,
        workers = cfg.evoclaw.worker_count,
        session_enabled = cfg.session.dir.is_some(),
        "spawning evoclaw subprocess pool"
    );
    // Compose the effective extra_args: user-supplied first, then the
    // session flags (if configured) appended. Session flags last so a
    // user can't accidentally override them with an earlier --session-dir
    // via extra_args (clap takes the LAST flag of a duplicated kind).
    let mut effective_extra_args = cfg.evoclaw.extra_args.clone();
    if let Some(dir) = &cfg.session.dir {
        // Best-effort dir creation with restrictive perms. SessionStore on
        // the EvoClaw side does the same — duplicate ensures the operator
        // can see clearly which side failed if perms / FS errors crop up.
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| eyre::eyre!("create session.dir {}: {e}", dir.display()))?;
        effective_extra_args.push("--session-dir".into());
        effective_extra_args.push(dir.display().to_string());
        effective_extra_args.push("--session-max-turns".into());
        effective_extra_args.push(cfg.session.max_turns.to_string());
        effective_extra_args.push("--session-ttl-days".into());
        effective_extra_args.push(cfg.session.ttl_days.to_string());
    }
    let pool = BridgePool::spawn(
        &cfg.evoclaw.binary,
        &effective_extra_args,
        cfg.evoclaw.worker_count,
        std::time::Duration::from_millis(cfg.evoclaw.startup_grace_ms),
    )
    .await
    .map_err(|e| eyre::eyre!("{e}"))?;
    let pool = Arc::new(pool);

    // Digest cache is always constructed (its `snapshot()` returns None
    // when `digest.enabled = false`, so it's free to ask). This keeps
    // HandlerState type stable across config modes.
    let digest_cache = Arc::new(crate::digest_cache::DigestCache::new(cfg.digest.clone()));

    // AI classifier is only built when both intent + ai_fallback are
    // enabled. Avoids spending a `BridgePool::checkout` slot per
    // request when the user opted out.
    let ai_classifier = if cfg.intent.enabled && cfg.intent.ai_fallback {
        Some(Arc::new(crate::intent::ai::AiClassifier::new(
            pool.clone(),
            std::time::Duration::from_millis(cfg.intent.ai_timeout_ms),
            cfg.intent.ai_prompt_override.clone(),
        )))
    } else {
        None
    };

    let state = HandlerState::new(cfg.clone(), pool, aes_key, digest_cache, ai_classifier);

    // Background GC for the per-cid mutex map. Even with millions of
    // distinct users over the lifetime of the process, the map stays
    // small (~current-active-fans size) because idle entries get
    // evicted. We clamp gc_interval_secs to >=60 so a tiny misconfig
    // can't make us busy-sweep.
    if cfg.session.dir.is_some() {
        let gc_serializer = state.conv_serializer.clone();
        // Clamp gc_interval to ≥60s to prevent a busy-spin from a tiny
        // misconfig. Warn loudly if we had to clamp so the operator
        // discovers it in the logs instead of being silently overruled.
        const MIN_GC_INTERVAL_SECS: u64 = 60;
        if cfg.session.gc_interval_secs < MIN_GC_INTERVAL_SECS {
            tracing::warn!(
                configured = cfg.session.gc_interval_secs,
                clamped = MIN_GC_INTERVAL_SECS,
                "session.gc_interval_secs clamped to minimum to prevent busy-spin"
            );
        }
        let gc_interval =
            std::time::Duration::from_secs(cfg.session.gc_interval_secs.max(MIN_GC_INTERVAL_SECS));
        let gc_idle = std::time::Duration::from_secs(cfg.session.cid_lock_idle_secs);
        tokio::spawn(async move {
            // Use a tokio interval so we self-correct after long pauses
            // (e.g. laptop sleep) instead of doing one sweep and dying.
            let mut interval = tokio::time::interval(gc_interval);
            // Skip the first immediate tick — process just started, map
            // is empty. Without this we'd burn one log line at boot.
            interval.tick().await;
            loop {
                interval.tick().await;
                let evicted = gc_serializer.gc(gc_idle);
                if evicted > 0 {
                    tracing::debug!(evicted, "conv_serializer: gc swept idle cid locks");
                }
            }
        });
    }

    let app = Router::new()
        .route(
            &cfg.server.endpoint_path,
            get(verify_url).post(handle_message),
        )
        .route("/healthz", get(|| async { "ok" }))
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| eyre::eyre!("bind {bind}: {e}"))?;
    tracing::info!(addr = %bind, path = %cfg.server.endpoint_path, "ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
