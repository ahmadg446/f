use anyhow::Result;
use dotenvy::dotenv;

use ebay_promotions_lib::{Config, refresh_access_token, update_env_access_token};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let _ = tracing_subscriber::fmt().without_time().try_init();

    println!("{}", "=".repeat(80));
    println!("eBay Auth Utility");
    println!("{}", "=".repeat(80));

    let cfg = Config::from_env()?;
    let new_token = refresh_access_token(&cfg).await?;
    update_env_access_token(&new_token)?;
    println!("Access token refreshed and saved to .env");
    Ok(())
}
