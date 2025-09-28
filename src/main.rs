#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use dialoguer::Confirm;
use dotenvy::dotenv;
use governor::{clock::DefaultClock, state::InMemoryState, Quota, RateLimiter};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fmt;
use std::fs;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::FmtSubscriber;

// ---------------- Configuration ----------------
#[derive(Clone, Copy, Debug)]
enum Environment {
    Sandbox,
    Production,
}
impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Environment::Sandbox => write!(f, "sandbox"),
            Environment::Production => write!(f, "production"),
        }
    }
}
#[derive(Clone)]
struct Config {
    access_token: String,
    env: Environment,
    requests_per_second: u32,
    alert_window_hours: i64,
}
impl Config {
    fn from_env() -> Result<Self> {
        let access_token =
            env::var("EBAY_ACCESS_TOKEN").context("Missing EBAY_ACCESS_TOKEN")?;
        let env_s =
            env::var("EBAY_ENVIRONMENT").unwrap_or_else(|_| "sandbox".into());
        let env = match env_s.to_lowercase().as_str() {
            "prod" | "production" => Environment::Production,
            _ => Environment::Sandbox,
        };
        Ok(Self {
            access_token,
            env,
            requests_per_second: 5,
            alert_window_hours: 48,
        })
    }
    fn base_host(&self) -> &'static str {
        match self.env {
            Environment::Sandbox => "api.sandbox.ebay.com",
            Environment::Production => "api.ebay.com",
        }
    }
    fn marketplace_id(&self) -> &'static str {
        match self.env {
            Environment::Sandbox => "EBAY_AT",
            Environment::Production => "EBAY_US",
        }
    }
    fn identity_host(&self) -> &'static str {
        match self.env {
            Environment::Sandbox => "api.sandbox.ebay.com",
            Environment::Production => "api.ebay.com",
        }
    }
}

// ---------------- Models ----------------
#[derive(Debug, Deserialize, Serialize, Clone)]
struct PromotionList {
    #[serde(default)]
    promotions: Vec<PromotionSummary>,
}
#[derive(Debug, Deserialize, Serialize, Clone)]
struct PromotionSummary {
    #[serde(default)]
    promotionId: String,
    #[serde(default)]
    promotionStatus: String,
    #[serde(default)]
    promotionType: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    startDate: String,
    #[serde(default)]
    endDate: String,
    #[serde(default)]
    inventoryCriterion: Option<Value>,
    #[serde(default)]
    selectedInventoryDiscounts: Option<Value>,
    #[serde(default)]
    discountBenefit: Option<Value>,
    #[serde(default)]
    discountSpecification: Option<Value>,
    #[serde(default)]
    marketplaceId: Option<String>,
    #[serde(default)]
    promotionImageUrl: Option<String>,
    #[serde(default)]
    priority: Option<Value>,
}
#[derive(thiserror::Error, Debug)]
enum ApiError {
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error(transparent)]
    Net(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

// ---------------- API Client ----------------
struct MarketingApi {
    client: Client,
    cfg: Config,
    limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
}
type NotKeyed = governor::state::NotKeyed;
impl MarketingApi {
    fn new(cfg: Config) -> Self {
        let client = Client::builder()
            .user_agent("ebay-discount-manager-rust/0.1")
            .build()
            .unwrap();
        let quota =
            Quota::per_second(NonZeroU32::new(cfg.requests_per_second).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));
        Self { client, cfg, limiter }
    }
    fn set_token(&mut self, token: String) {
        self.cfg.access_token = token;
    }
    fn url(&self, path: &str) -> String {
        format!(
            "https://{}/sell/marketing/v1{}",
            self.cfg.base_host(),
            path
        )
    }
    async fn request_text(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<(StatusCode, String), ApiError> {
        self.limiter.until_ready().await;
        let url = self.url(path);
        let req = self
            .client
            .request(method, &url)
            .bearer_auth(&self.cfg.access_token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        let req = if let Some(b) = body { req.json(b) } else { req };
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }
    async fn request_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> std::result::Result<T, ApiError> {
        let mut attempt = 1usize;
        loop {
            info!("API {} {}", method, path);
            match self.request_text(method.clone(), path, body).await {
                Ok((status, text)) => {
                    if status.is_success() {
                        return Ok(serde_json::from_str::<T>(&text)?);
                    }
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        if attempt < 3 {
                            let backoff_ms = 1000u64 * (1 << attempt);
                            warn!("Retryable status {}. retrying in {}ms", status, backoff_ms);
                            sleep(StdDuration::from_millis(backoff_ms)).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    warn!("HTTP error {}: {}", status, text);
                    return Err(ApiError::Http { status: status.as_u16(), body: text });
                }
                Err(e) => {
                    if attempt < 3 {
                        let backoff_ms = 1000u64 * (1 << attempt);
                        warn!("Net/API error {}. retrying in {}ms", e, backoff_ms);
                        sleep(StdDuration::from_millis(backoff_ms)).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
    // Endpoints
    async fn list_promotions(
        &self,
        limit: u32,
        offset: u32,
    ) -> std::result::Result<PromotionList, ApiError> {
        let path = format!(
            "/promotion?limit={}&offset={}&marketplace_id={}",
            limit,
            offset,
            self.cfg.marketplace_id()
        );
        self.request_json(Method::GET, &path, None).await
    }
    async fn get_item_promotion(
        &self,
        id: &str,
    ) -> std::result::Result<PromotionSummary, ApiError> {
        let path = format!("/item_promotion/{}", id);
        self.request_json(Method::GET, &path, None).await
    }
    async fn get_markdown_promotion(
        &self,
        id: &str,
    ) -> std::result::Result<PromotionSummary, ApiError> {
        let path = format!("/item_price_markdown/{}", id);
        self.request_json(Method::GET, &path, None).await
    }
    async fn update_item_promotion(
        &self,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ApiError> {
        let path = format!("/item_promotion/{}", id);
        self.request_json(Method::PUT, &path, Some(body)).await
    }
    async fn update_markdown_promotion(
        &self,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ApiError> {
        let path = format!("/item_price_markdown/{}", id);
        self.request_json(Method::PUT, &path, Some(body)).await
    }
}

// ---------------- OAuth refresh ----------------
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

fn update_env_access_token(new_token: &str) -> Result<()> {
    // Read the current .env file
    let env_content = fs::read_to_string(".env").context("Failed to read .env file")?;
    
    // Split content into lines
    let mut lines: Vec<String> = env_content.lines().map(|s| s.to_string()).collect();
    
    // Find and update the EBAY_ACCESS_TOKEN line
    for line in lines.iter_mut() {
        if line.starts_with("EBAY_ACCESS_TOKEN=") {
            // Replace the line with the new token wrapped in quotes
            *line = format!("EBAY_ACCESS_TOKEN='{}'", new_token);
            break;
        }
    }
    
    // Write the updated content back to the .env file
    let updated_content = lines.join("\n");
    fs::write(".env", updated_content).context("Failed to write to .env file")?;
    
    Ok(())
}
async fn refresh_access_token(cfg: &Config) -> Result<String> {
    use anyhow::bail;
    let client_id = env::var("EBAY_CLIENT_ID").context("Missing EBAY_CLIENT_ID")?;
    let client_secret =
        env::var("EBAY_CLIENT_SECRET").context("Missing EBAY_CLIENT_SECRET")?;
    let refresh_token =
        env::var("EBAY_REFRESH_TOKEN").context("Missing EBAY_REFRESH_TOKEN")?;
    let url = format!("https://{}/identity/v1/oauth2/token", cfg.identity_host());
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
    ];
    let res = Client::new()
        .post(url)
        .basic_auth(client_id, Some(client_secret))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&form)
        .send()
        .await
        .context("token request failed")?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("refresh failed {}: {}", status, text);
    }
    let tr: TokenResponse =
        serde_json::from_str(&text).context("parse token response failed")?;
    Ok(tr.access_token)
}

// ---------------- Helpers ----------------
async fn fetch_all_promotions(
    api: &MarketingApi,
) -> std::result::Result<Vec<PromotionSummary>, ApiError> {
    let mut out = Vec::new();
    let mut offset = 0u32;
    let limit = 50u32;
    loop {
        let page = api.list_promotions(limit, offset).await?;
        let n = page.promotions.len();
        info!("Fetched {} promotions at offset {}", n, offset);
        out.extend(page.promotions);
        if n < limit as usize {
            break;
        }
        offset += limit;
    }
    Ok(out)
}
fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}
fn filter_expiring(v: &[PromotionSummary], hours: i64) -> Vec<PromotionSummary> {
    let now = Utc::now();
    let thr = now + Duration::hours(hours);
    let mut out: Vec<_> = v
        .iter()
        .cloned()
        .filter(|p| {
            let status_ok =
                matches!(p.promotionStatus.as_str(), "RUNNING" | "SCHEDULED" | "PAUSED");
            let end_ok = parse_rfc3339(&p.endDate).map_or(false, |e| e <= thr);
            status_ok && end_ok
        })
        .collect();
    out.sort_by_key(|p| parse_rfc3339(&p.endDate));
    out
}
fn plus_days(iso: &str, days: i64) -> Result<String> {
    let dt = DateTime::parse_from_rfc3339(iso)?;
    Ok((dt + Duration::days(days)).to_rfc3339())
}
fn short_desc_tag() -> String {
    let ts = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let s = format!("Auto-generated, {}", ts);
    s.chars().take(36).collect()
}
async fn extend_promotion(api: &MarketingApi, p: &PromotionSummary) -> Result<()> {
    let id = &p.promotionId;
    let detail = if p.promotionType == "MARKDOWN_SALE" {
        api.get_markdown_promotion(id).await?
    } else {
        api.get_item_promotion(id).await?
    };
    let new_end = plus_days(&detail.endDate, 14)?;
    let mut body = json!({
        "name": detail.name,
        "description": short_desc_tag(),
        "startDate": detail.startDate,
        "endDate": new_end,
        "marketplaceId": detail.marketplaceId.unwrap_or_else(|| api.cfg.marketplace_id().to_string()),
        "promotionType": detail.promotionType,
        "promotionStatus": "SCHEDULED",
    });
    if let Some(v) = &detail.inventoryCriterion {
        body["inventoryCriterion"] = v.clone();
    }
    if let Some(v) = &detail.selectedInventoryDiscounts {
        body["selectedInventoryDiscounts"] = v.clone();
    }
    if let Some(v) = &detail.promotionImageUrl {
        body["promotionImageUrl"] = json!(v);
    }
    if let Some(v) = &detail.priority {
        body["priority"] = v.clone();
    }
    if p.promotionType != "MARKDOWN_SALE" {
        if let Some(v) = detail.discountBenefit.clone() {
            body["discountBenefit"] = v;
        }
        if let Some(v) = detail.discountSpecification.clone() {
            body["discountSpecification"] = v;
        }
    }
    if p.promotionType == "MARKDOWN_SALE" {
        let _ = api.update_markdown_promotion(id, &body).await?;
    } else {
        let _ = api.update_item_promotion(id, &body).await?;
    }
    info!("Extended {} to {}", id, new_end);
    Ok(())
}
fn display(discounts: &[PromotionSummary], hours_window: i64) {
    if discounts.is_empty() {
        println!("No discounts expiring within {} hours.", hours_window);
        return;
    }
    println!("{}", "=".repeat(80));
    println!("DISCOUNTS EXPIRING WITHIN {} HOURS", hours_window);
    println!("{}", "=".repeat(80));
    for (i, d) in discounts.iter().enumerate() {
        let now = Utc::now();
        let end = parse_rfc3339(&d.endDate).unwrap_or(now);
        let left = end - now;
        let days = left.num_days().max(0);
        let hours = (left.num_hours() % 24).max(0);
        let mins = (left.num_minutes() % 60).max(0);
        let urgency = if left.num_hours() <= 6 {
            "[!!!]"
        } else if left.num_hours() <= 12 {
            "[!!] "
        } else if left.num_hours() <= 24 {
            "[!]"
        } else {
            "[*]"
        };
        println!("\n{} DISCOUNT #{}", urgency, i + 1);
        println!("Promotion ID: {}", d.promotionId);
        println!("Type: {}", d.promotionType);
        println!("Status: {}", d.promotionStatus);
        println!(
            "Name: {}",
            d.name.clone().unwrap_or_else(|| "Unnamed Promotion".into())
        );
        if let Some(desc) = &d.description {
            println!("Description: {}", desc);
        }
        println!("Start: {}", d.startDate);
        println!("End:   {}", d.endDate);
        println!("Time Remaining: {}d {}h {}m", days, hours, mins);
        if i + 1 < discounts.len() {
            println!("{}", "-".repeat(80));
        }
    }
    println!("{}", "=".repeat(80));
    println!("Total expiring discounts: {}", discounts.len());
    println!("{}", "=".repeat(80));
}

// --------------- Detect invalid-token payload ---------------
fn body_has_error_1001(body: &str) -> bool {
    #[derive(Deserialize)]
    struct ErrObj { errorId: Option<i64> }
    #[derive(Deserialize)]
    struct ErrWrap { errors: Option<Vec<ErrObj>> }

    if let Ok(v) = serde_json::from_str::<ErrWrap>(body) {
        if let Some(list) = v.errors {
            return list.iter().any(|e| e.errorId == Some(1001));
        }
    }
    body.contains("Invalid access token")
}
// ---------------- Main ----------------
#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let subscriber = FmtSubscriber::builder().without_time().finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    println!("{}", "=".repeat(80));
    println!("eBay Discount Manager - 48 Hour Expiry Monitor (Rust)");
    println!("{}", "=".repeat(80));

    let cfg = Config::from_env()?;
    info!("Environment: {}", cfg.env);
    info!("Alert window: {} hours", cfg.alert_window_hours);

    let mut api = MarketingApi::new(cfg.clone());

    // fetch with auto-refresh-on-1001 once
    let all = match fetch_all_promotions(&api).await {
        Ok(v) => v,
        Err(ApiError::Http { status, body }) if status == 401 && body_has_error_1001(&body) => {
            warn!("401 Invalid token. Attempting auto-refresh via OAuth…");
            let new_token = refresh_access_token(&api.cfg).await?;
            // Update the .env file with the new token
            update_env_access_token(&new_token)?;
            api.set_token(new_token);
            fetch_all_promotions(&api).await?
        }
        Err(e) => return Err(e.into()),
    };

    let expiring = filter_expiring(&all, api.cfg.alert_window_hours);
    display(&expiring, api.cfg.alert_window_hours);

    if !expiring.is_empty() {
        let yes = Confirm::new()
            .with_prompt("Extend all expiring discounts by 2 weeks?")
            .default(false)
            .interact()?;
        if yes {
            let mut ok = 0usize;
            let mut fail = 0usize;
            for d in &expiring {
                match extend_promotion(&api, d).await {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        fail += 1;
                        if let Some(ApiError::Http { status, body }) =
                            e.downcast_ref::<ApiError>()
                        {
                            if *status == 404 {
                                warn!("{} not found. Possibly deleted.", d.promotionId);
                            } else {
                                error!(
                                    "Extend failed {}: {} {}",
                                    d.promotionId, status, body
                                );
                            }
                        } else {
                            error!("Extend failed {}: {:?}", d.promotionId, e);
                        }
                    }
                }
            }
            info!("Finished. {} succeeded, {} failed.", ok, fail);
        }
    }

    Ok(())
}
