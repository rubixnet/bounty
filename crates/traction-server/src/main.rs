//! `traction-server [--config <traction-server.toml>]`: refuses to start on a
//! secret written inline in the config; the database URL comes from the
//! environment variable (or file) the config names.

use std::path::PathBuf;
use std::process::ExitCode;

use traction_server::app::router;
use traction_server::config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("traction-server: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("traction-server.toml");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = PathBuf::from(args.next().ok_or("--config needs a path")?),
            _ => return Err("usage: traction-server [--config <path>]".into()),
        }
    }
    let config = if path.exists() {
        Config::load(&path).map_err(|e| e.to_string())?
    } else {
        Config::default()
    };
    let database_url = config
        .database_url(|name| std::env::var(name).ok())
        .map_err(|e| e.to_string())?;
    let bind = config.server.bind.clone();
    let state = traction_server::start(config, &database_url).await?;
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("cannot listen on {bind}: {e}"))?;
    eprintln!("traction-server: listening on {bind}");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
