#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use governor::{clock::DefaultClock, state::InMemoryState, Quota, RateLimiter};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, fmt, fs, num::NonZeroU32, sync::Arc, time::Duration as StdDuration};
use tokio::time::sleep;
use tracing::{info, warn};

pub type NotKeyed = governor::state::NotKeyed;

// ---------------- Configuration ----------------
#[derive(Clone, Copy, Debug)]
pub enum Environment { Sandbox, Production }
impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self { Environment::Sandbox => write!(f,"sandbox"), Environment::Production => write!(f,"production") }
    }
}
#[derive(Clone)]
pub struct Config {
    pub access_token: String,
    pub env: Environment,
    pub requests_per_second: u32,
    pub alert_window_hours: i64,
}
impl Config {
    pub fn from_env() -> Result<Self> {
        let access_token = env::var("EBAY_ACCESS_TOKEN").context("Missing EBAY_ACCESS_TOKEN")?;
        let env_s = env::var("EBAY_ENVIRONMENT").unwrap_or_else(|_| "sandbox".into());
        let env = match env_s.to_lowercase().as_str() { "prod" | "production" => Environment::Production, _ => Environment::Sandbox };
        Ok(Self { access_token, env, requests_per_second: 5, alert_window_hours: 48 })
    }
    pub fn base_host(&self) -> &'static str {
        match self.env { Environment::Sandbox => "api.sandbox.ebay.com", Environment::Production => "api.ebay.com" }
    }
    pub fn marketplace_id(&self) -> &'static str {
        match self.env { Environment::Sandbox => "EBAY_AT", Environment::Production => "EBAY_US" }
    }
    pub fn identity_host(&self) -> &'static str {
        match self.env { Environment::Sandbox => "api.sandbox.ebay.com", Environment::Production => "api.ebay.com" }
    }
}

// ---------------- Models ----------------
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PromotionList { #[serde(default)] pub promotions: Vec<PromotionSummary> }

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ListingSet {
    #[serde(default)]
    pub listings: Vec<ListingDetail>,
    #[serde(default)]
    pub total: i32,
    #[serde(default)]
    pub limit: i32,
    #[serde(default)]
    pub offset: i32,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub prev: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ListingDetail {
    #[serde(default)]
    pub listingId: String,
    #[serde(default)]
    pub inventoryReferenceId: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub currentPrice: Option<Value>,
    #[serde(default)]
    pub freeShipping: Option<bool>,
    #[serde(default)]
    pub listingCategoryId: Option<String>,
    #[serde(default)]
    pub listingCondition: Option<String>,
    #[serde(default)]
    pub listingConditionId: Option<String>,
    #[serde(default)]
    pub storeCategoryId: Option<String>,
    #[serde(default)]
    pub listingPromotionStatuses: Option<Vec<ItemMarkdownStatus>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ItemMarkdownStatus {
    #[serde(default)]
    pub listingMarkdownStatus: String,
    #[serde(default)]
    pub statusChangedDate: String,
    #[serde(default)]
    pub statusMessage: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PromotionSummary {
    #[serde(default)] pub promotionId: String,
    #[serde(default)] pub promotionStatus: String,
    #[serde(default)] pub promotionType: String,
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub startDate: String,
    #[serde(default)] pub endDate: String,
    #[serde(default)] pub inventoryCriterion: Option<Value>,
    #[serde(default)] pub selectedInventoryDiscounts: Option<Value>,
    #[serde(default)] pub discountBenefit: Option<Value>,
    #[serde(default)] pub discountSpecification: Option<Value>,
    #[serde(default)] pub marketplaceId: Option<String>,
    #[serde(default)] pub promotionImageUrl: Option<String>,
    #[serde(default)] pub priority: Option<Value>,
}

#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    #[error("http {status}: {body}")] Http { status: u16, body: String },
    #[error(transparent)] Net(#[from] reqwest::Error),
    #[error(transparent)] Json(#[from] serde_json::Error),
}

// ---------------- API Client ----------------
pub struct MarketingApi {
    pub client: Client,
    pub cfg: Config,
    pub limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
}
impl MarketingApi {
    pub fn new(cfg: Config) -> Self {
        let client = Client::builder().user_agent("ebay-discount-manager-rust/0.1").build().unwrap();
        let quota = Quota::per_second(NonZeroU32::new(cfg.requests_per_second).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));
        Self { client, cfg, limiter }
    }
    pub fn set_token(&mut self, token: String) { self.cfg.access_token = token; }
    fn url(&self, path: &str) -> String { format!("https://{}/sell/marketing/v1{}", self.cfg.base_host(), path) }

    async fn request_text(&self, method: Method, path: &str, body: Option<&Value>)
        -> std::result::Result<(StatusCode, String), ApiError>
    {
        self.limiter.until_ready().await;
        let url = self.url(path);
        let req = self.client.request(method, &url)
            .bearer_auth(&self.cfg.access_token)
            .header("Accept","application/json")
            .header("Content-Type","application/json");
        let req = if let Some(b) = body { req.json(b) } else { req };
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    pub async fn request_json<T: for<'de> Deserialize<'de>>(
        &self, method: Method, path: &str, body: Option<&Value>
    ) -> std::result::Result<T, ApiError> {
        let mut attempt = 1usize;
        loop {
            info!("API {} {}", method, path);
            match self.request_text(method.clone(), path, body).await {
                Ok((status, text)) => {
                    if status.is_success() { return Ok(serde_json::from_str::<T>(&text)?); }
                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        if attempt < 3 {
                            let backoff_ms = 1000u64 * (1 << attempt);
                            warn!("Retryable status {}. retrying in {}ms", status, backoff_ms);
                            sleep(StdDuration::from_millis(backoff_ms)).await; attempt += 1; continue;
                        }
                    }
                    warn!("HTTP error {}: {}", status, text);
                    return Err(ApiError::Http { status: status.as_u16(), body: text });
                }
                Err(e) => {
                    if attempt < 3 {
                        let backoff_ms = 1000u64 * (1 << attempt);
                        warn!("Net/API error {}. retrying in {}ms", e, backoff_ms);
                        sleep(StdDuration::from_millis(backoff_ms)).await; attempt += 1; continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    // Endpoints
    pub async fn list_promotions(&self, limit: u32, offset: u32) -> std::result::Result<PromotionList, ApiError> {
        let path = format!("/promotion?limit={}&offset={}&marketplace_id={}", limit, offset, self.cfg.marketplace_id());
        self.request_json(Method::GET, &path, None).await
    }
    pub async fn get_item_promotion(&self, id: &str) -> std::result::Result<PromotionSummary, ApiError> {
        self.request_json(Method::GET, &format!("/item_promotion/{}", id), None).await
    }
    pub async fn get_markdown_promotion(&self, id: &str) -> std::result::Result<PromotionSummary, ApiError> {
        self.request_json(Method::GET, &format!("/item_price_markdown/{}", id), None).await
    }
    pub async fn update_item_promotion(&self, id: &str, body: &Value) -> std::result::Result<Value, ApiError> {
        self.request_json(Method::PUT, &format!("/item_promotion/{}", id), Some(body)).await
    }
    pub async fn update_markdown_promotion(&self, id: &str, body: &Value) -> std::result::Result<Value, ApiError> {
        self.request_json(Method::PUT, &format!("/item_price_markdown/{}", id), Some(body)).await
    }

    pub async fn get_listing_set(&self, promotion_id: &str) -> std::result::Result<ListingSet, ApiError> {
        let path = format!("/promotion/{}/get_listing_set", promotion_id);
        self.request_json(Method::GET, &path, None).await
    }

    pub async fn create_item_promotion(&self, body: &Value) -> std::result::Result<Value, ApiError> {
        self.request_json(Method::POST, "/item_promotion", Some(body)).await
    }

    pub async fn create_markdown_promotion(&self, body: &Value) -> std::result::Result<Value, ApiError> {
        self.request_json(Method::POST, "/item_price_markdown", Some(body)).await
    }
}

// ---------------- OAuth ----------------
#[derive(Deserialize)]
pub struct TokenResponse { pub access_token: String }

pub fn update_env_access_token(new_token: &str) -> Result<()> {
    let env_content = fs::read_to_string(".env").context("Failed to read .env file")?;
    let mut lines: Vec<String> = env_content.lines().map(|s| s.to_string()).collect();
    for line in lines.iter_mut() {
        if line.starts_with("EBAY_ACCESS_TOKEN=") { *line = format!("EBAY_ACCESS_TOKEN='{}'", new_token); break; }
    }
    fs::write(".env", lines.join("\n")).context("Failed to write to .env file")?;
    Ok(())
}

pub async fn refresh_access_token(cfg: &Config) -> Result<String> {
    use anyhow::bail;
    let client_id = env::var("EBAY_CLIENT_ID").context("Missing EBAY_CLIENT_ID")?;
    let client_secret = env::var("EBAY_CLIENT_SECRET").context("Missing EBAY_CLIENT_SECRET")?;
    let refresh_token = env::var("EBAY_REFRESH_TOKEN").context("Missing EBAY_REFRESH_TOKEN")?;
    let url = format!("https://{}/identity/v1/oauth2/token", cfg.identity_host());
    let form = [("grant_type","refresh_token"),("refresh_token", refresh_token.as_str())];
    let res = Client::new().post(url).basic_auth(client_id, Some(client_secret))
        .header("Content-Type","application/x-www-form-urlencoded").form(&form).send().await
        .context("token request failed")?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() { bail!("refresh failed {}: {}", status, text); }
    let tr: TokenResponse = serde_json::from_str(&text).context("parse token response failed")?;
    Ok(tr.access_token)
}

// ---------------- Helpers used by bins ----------------
pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}
pub fn filter_expiring(v: &[PromotionSummary], hours: i64) -> Vec<PromotionSummary> {
    let now = Utc::now();
    let thr = now + Duration::hours(hours);
    let mut out: Vec<_> = v.iter().cloned().filter(|p| {
        let status_ok = matches!(p.promotionStatus.as_str(),"RUNNING"|"SCHEDULED"|"PAUSED");
        let end_ok = parse_rfc3339(&p.endDate).map_or(false, |e| e <= thr);
        status_ok && end_ok
    }).collect();
    out.sort_by_key(|p| parse_rfc3339(&p.endDate));
    out
}
pub async fn fetch_all_promotions(api: &MarketingApi) -> std::result::Result<Vec<PromotionSummary>, ApiError> {
    let mut out = Vec::new(); let mut offset = 0u32; let limit = 50u32;
    loop {
        let page = api.list_promotions(limit, offset).await?; let n = page.promotions.len();
        tracing::info!("Fetched {} promotions at offset {}", n, offset);
        out.extend(page.promotions); if n < limit as usize { break; } offset += limit;
    } Ok(out)
}
pub fn body_has_error_1001(body: &str) -> bool {
    #[derive(Deserialize)] struct ErrObj{ errorId: Option<i64> }
    #[derive(Deserialize)] struct ErrWrap{ errors: Option<Vec<ErrObj>> }
    if let Ok(v) = serde_json::from_str::<ErrWrap>(body) {
        if let Some(list) = v.errors { return list.iter().any(|e| e.errorId == Some(1001)); }
    }
    body.contains("Invalid access token")
}

pub fn plus_days(iso: &str, days: i64) -> Result<String> {
    let dt = DateTime::parse_from_rfc3339(iso)?;
    Ok((dt + Duration::days(days)).to_rfc3339())
}

pub fn short_desc_tag() -> String {
    use chrono::{Utc, SecondsFormat};
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let s = format!("Auto-generated, {}", ts);
    s.chars().take(36).collect()
}
