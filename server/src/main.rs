use mailorva_server::{config::Config, mail::LiveMailTransport, Service};
use std::{path::Path, sync::Arc};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mailorva_server=info".into()),
        )
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".into());
    let config = Config::load(Path::new(&path))?;
    let bind = config.bind;
    let service = Service::new(config, Arc::new(LiveMailTransport::default())).await?;
    let router = mailorva_server::api::router(service.clone())?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let events = tokio::spawn(
        service
            .events
            .clone()
            .run(service.clone(), cancellation.clone()),
    );
    let background = tokio::spawn(service.run_sync(cancellation.clone()));
    let signal = cancellation.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            signal.cancel();
        })
        .await?;
    cancellation.cancel();
    background.await?;
    events.await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("无法安装终止信号处理器");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
