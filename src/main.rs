use anyhow::{Context, Result};
use rmcp::{ServiceExt, transport::stdio};

mod config;
mod confluence;
mod cql;
mod hash;
mod image_cache;
mod markdown;
mod server;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conf-mcp: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = config::Config::load()?;
    let images = image_cache::ImageCache::new(config.download_dir.clone());
    let client = confluence::ConfluenceClient::new(config)?;
    let service = server::ConfluenceServer::new(client, images)
        .serve(stdio())
        .await
        .context("Не удалось инициализировать MCP через stdio")?;
    service.waiting().await.context("Ошибка задачи MCP")?;
    Ok(())
}
