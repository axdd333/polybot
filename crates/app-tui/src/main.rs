use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let profile = trading_core::config::load_profile("config/live.toml").unwrap_or_default();
    app_tui::run(profile).await
}
