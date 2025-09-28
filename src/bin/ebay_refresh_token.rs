use anyhow::{Context, Result};
use dotenvy::dotenv;
use reqwest::Client;
use serde::Deserialize;
use std::env;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    let env_s = env::var("EBAY_ENVIRONMENT").unwrap_or_else(|_| "sandbox".into());
    let host = match env_s.to_lowercase().as_str() {
        "prod" | "production" => "api.ebay.com",
        _ => "api.sandbox.ebay.com",
    };

    let client_id     = env::var("EBAY_CLIENT_ID").context("Missing EBAY_CLIENT_ID")?;
    let client_secret = env::var("EBAY_CLIENT_SECRET").context("Missing EBAY_CLIENT_SECRET")?;
    let refresh_token = env::var("EBAY_REFRESH_TOKEN").context("Missing EBAY_REFRESH_TOKEN")?;

    // Do NOT send scope on refresh; eBay uses the original scopes.
    let url  = format!("https://{}/identity/v1/oauth2/token", host);
    let form = [("grant_type", "refresh_token"), ("refresh_token", refresh_token.as_str())];

    let res = Client::new()
        .post(url)
        .basic_auth(client_id, Some(client_secret))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await
        .context("token request failed")?;

    let status = res.status();
    let text   = res.text().await.unwrap_or_default();
    if !status.is_success() {
        eprintln!("Refresh failed {}: {}", status, text);
        std::process::exit(1);
    }

    let tr: TokenResponse = serde_json::from_str(&text).context("parse token response failed")?;
    let token = &tr.access_token;
    if token.len() > 20 {
        println!("{}...{}", &token[..10], &token[token.len()-10..]);
    } else {
        println!("{}", token);
    }
    Ok(())
}
