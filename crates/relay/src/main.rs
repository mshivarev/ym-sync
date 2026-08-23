//! Standalone relay: a thin wrapper around [`ymsync_relay::bind`].
//!
//! The room logic lives in the library, because a player can now host a room in
//! its own process. This binary is what you run when nobody's app should have to
//! stay open — a spare machine holding the room — and it is what the integration
//! tests drive.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "ymsync-relay",
    version,
    about = "Holds a room's Yandex Music queue and playhead, and keeps its players in step"
)]
struct Args {
    /// Address to listen on. Keep it on localhost or a LAN address; put a TLS
    /// reverse proxy in front before exposing it to the internet.
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: String,

    /// Shared secret every client must present. Falls back to the
    /// YMSYNC_ROOM_TOKEN environment variable.
    #[arg(long)]
    token: Option<String>,

    /// Не отвечать на широковещательные запросы «кто держит комнаты»
    ///
    /// По умолчанию релей отвечает: так клиенты находят комнату сами, командой
    /// `ymsync rooms`. В ответе только имена комнат и число слушателей — ни
    /// токена, ни того, что играет. С этим ключом комната остаётся доступной
    /// тем, кому адрес назвали руками, и невидимой для поиска.
    #[arg(long)]
    no_discovery: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ymsync_relay=info")),
        )
        .init();

    let args = Args::parse();
    let token = args
        .token
        .or_else(|| std::env::var("YMSYNC_ROOM_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .context(
            "не задан токен комнаты: передайте --token <секрет> или переменную \
             YMSYNC_ROOM_TOKEN. То же значение впишите игрокам в room_token",
        )?;

    let mut server = ymsync_relay::bind_with(&args.bind, token, !args.no_discovery).await?;

    // Ctrl-C closes the sockets rather than having the process vanish out from
    // under its peers, which is what the embedded relay does too.
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl-C")?;
    tracing::info!("stopping");
    server.stop().await;
    Ok(())
}
