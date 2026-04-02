use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    polymarket_btc_bot::app::run().await
}
