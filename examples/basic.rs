use reqwest::Client;
use reqwest_middleware::ClientBuilder;
use reqwest_middleware_cache::{managers::CACacheManager, Cache, CacheMode};

#[tokio::main]
async fn main() -> reqwest::Result<()> {
    let client = ClientBuilder::new(Client::new())
        .with(Cache {
            mode: CacheMode::Default,
            cache_manager: CACacheManager::default(),
        })
        .build();
    client
        .get("https://developer.mozilla.org/en-US/docs/Web/HTTP/Caching")
        .send()
        .await
        .unwrap();
    Ok(())
}
