use anyhow::Result;
use dialoguer::Confirm;
use dotenvy::dotenv;
use tracing::{error, info, warn};
use tracing_subscriber::FmtSubscriber;
use clap::{Parser, ValueEnum};
use chrono::{Utc, Duration};

use ebay_marketing_lib::{
    ApiError, Config, MarketingApi, body_has_error_1001, fetch_all_promotions,
    filter_expiring, refresh_access_token, update_env_access_token, parse_rfc3339,
    PromotionSummary, short_desc_tag,
};
use reqwest::Method;

use std::collections::HashMap;
use std::collections::HashSet;

// Information about a listing including its cohort tag
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ListingInfo {
    listing_id: String,
    cohort: String, // A, B, or C
}

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq, Hash)]
enum FallbackType {
    #[clap(name = "order")]
    Order,
    #[clap(name = "coupon")]
    Coupon,
}

// Actions that can be performed on promotions
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
#[allow(dead_code)]
enum PromotionAction {
    /// Update the end date of a promotion
    UpdateEnd(String, String), // promotion_id, new_end_date
    /// End a promotion
    End(String), // promotion_id
    /// Start a markdown sale promotion
    StartMarkdown(Vec<ListingInfo>, String, String), // listings, start_date, end_date
    /// Start a fallback promotion
    StartFallback(FallbackType, Vec<ListingInfo>, String, String), // fallback_type, listings, start_date, end_date
    /// Remove a listing from a promotion
    RemoveListing(String, String), // promotion_id, listing_id
}

fn tally<'a, I>(iter: I) -> (usize, HashMap<String, usize>, HashMap<String, usize>)
where
    I: IntoIterator<Item = &'a PromotionSummary>,
{
    let mut total = 0usize;
    let mut by_type: HashMap<String, usize> = HashMap::new();
    let mut by_status: HashMap<String, usize> = HashMap::new();

    for p in iter {
        total += 1;
        *by_type.entry(p.promotionType.to_string()).or_default() += 1;
        *by_status.entry(p.promotionStatus.to_string()).or_default() += 1;
    }
    (total, by_type, by_status)
}

fn print_sorted_map(title: &str, map: &HashMap<String, usize>) {
    println!("{}:", title);
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_unstable_by_key(|(k, _)| k.as_str());
    for (k, v) in entries {
        println!("  {}: {}", k, v);
    }
}

// Extract listing IDs from a promotion
async fn extract_listing_ids(api: &MarketingApi, promotion: &PromotionSummary) -> Result<Vec<ListingInfo>> {
    let mut listings = Vec::new();
    
    // For markdown sales, we need to get the listing set
    if promotion.promotionType == "MARKDOWN_SALE" {
        match api.get_listing_set(&promotion.promotionId).await {
            Ok(listing_set) => {
                for listing in listing_set.listings {
                    // Default to cohort A if no cohort tag is found
                    let cohort = extract_cohort_tag(&listing.title);
                    listings.push(ListingInfo {
                        listing_id: listing.listingId,
                        cohort,
                    });
                }
                info!("Extracted {} listings from markdown sale {}", listings.len(), promotion.promotionId);
            }
            Err(e) => {
                error!("Failed to get listing set for promotion {}: {:?}", promotion.promotionId, e);
            }
        }
    } else if let Some(inventory_criterion) = &promotion.inventoryCriterion {
        // Try to extract listing IDs from inventory criterion JSON
        if let Some(listing_ids) = inventory_criterion.get("listingIds") {
            if let Some(ids) = listing_ids.as_array() {
                for id in ids {
                    if let Some(id_str) = id.as_str() {
                        listings.push(ListingInfo {
                            listing_id: id_str.to_string(),
                            cohort: "A".to_string(), // Default cohort for non-markdown promotions
                        });
                    }
                }
                info!("Extracted {} listings from inventoryCriterion for promotion {}", listings.len(), promotion.promotionId);
            }
        }
        
        // If no listingIds found, check for other inventory selection methods
        if listings.is_empty() {
            // Some promotions use category or store criteria instead of specific listings
            warn!("No specific listings found in inventoryCriterion for promotion {}, type: {}", 
                  promotion.promotionId, promotion.promotionType);
        }
    }
    
    Ok(listings)
}

// Extract cohort tag from listing title
// If no cohort tag is found, default to "A"
fn extract_cohort_tag(title: &str) -> String {
    // Look for cohort tags in format [A], [B], or [C] in the title
    if title.contains("[A]") {
        "A".to_string()
    } else if title.contains("[B]") {
        "B".to_string()
    } else if title.contains("[C]") {
        "C".to_string()
    } else {
        // Default to cohort A if no tag is found
        "A".to_string()
    }
}

// Classify a promotion based on its type and business rules
fn classify_promotion(promotion: &PromotionSummary) -> PromotionClassification {
    match promotion.promotionType.as_str() {
        "CODED_COUPON" => PromotionClassification::Coupon,
        "VOLUME_DISCOUNT" => PromotionClassification::VolumeDiscount,
        "MARKDOWN_SALE" => PromotionClassification::MarkdownSale,
        "ORDER_DISCOUNT" => PromotionClassification::Fallback,
        _ => PromotionClassification::Other,
    }
}

// Decision for markdown sales
// Returns: (should_end, should_extend, should_schedule_fallback)
fn plan_markdown_sale(promotion: &PromotionSummary, now: chrono::DateTime<Utc>) -> (bool, bool, bool) {
    // Parse the start and end dates
    let start_date = parse_rfc3339(&promotion.startDate).unwrap_or(now);
    let end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
    
    // Calculate the duration of the markdown sale
    let duration = end_date - start_date;
    
    // If the markdown sale has been running for more than 45 days, it should end
    let should_end = duration.num_days() >= 45;
    
    // If the markdown sale is expiring within the alert window and hasn't reached 45 days, extend it
    let time_left = end_date - now;
    let should_extend = !should_end && Duration::hours(48).num_hours() >= time_left.num_hours();
    
    // If the markdown sale should end, we should schedule a fallback
    let should_schedule_fallback = should_end;
    
    (should_end, should_extend, should_schedule_fallback)
}

// Decision for volume discounts
// Returns: (should_end, should_extend, should_schedule_fallback)
fn plan_volume_discount(promotion: &PromotionSummary, now: chrono::DateTime<Utc>) -> (bool, bool, bool) {
    // Volume discounts should never end, only extend if expiring
    let end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
    let time_left = end_date - now;
    let should_extend = Duration::hours(48).num_hours() >= time_left.num_hours(); // Using default alert window
    
    (false, should_extend, false)
}

// Decision for fallback promotions
// Returns: (should_end, should_extend, should_schedule_fallback)
fn plan_fallback(promotion: &PromotionSummary, now: chrono::DateTime<Utc>) -> (bool, bool, bool) {
    // Parse the start and end dates
    let start_date = parse_rfc3339(&promotion.startDate).unwrap_or(now);
    let end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
    
    // Calculate the duration of the fallback promotion
    let duration = end_date - start_date;
    
    // If the fallback promotion has been running for more than 14 days, it should end
    let should_end = duration.num_days() >= 14;
    
    // If the fallback promotion is expiring within the alert window and hasn't reached 14 days, extend it
    let time_left = end_date - now;
    let should_extend = !should_end && Duration::hours(48).num_hours() >= time_left.num_hours(); // Using default alert window
    
    (should_end, should_extend, false)
}

// Build actions for a promotion based on its classification and current state
async fn build_actions(
    api: &MarketingApi,
    promotion: &PromotionSummary,
    cli: &Cli,
) -> Result<Vec<PromotionAction>> {
    let mut actions = Vec::new();
    let now = chrono::Utc::now();
    
    // Extract listing information from the promotion
    let listings = extract_listing_ids(api, promotion).await?;
    
    // Classify the promotion
    let classification = classify_promotion(promotion);
    
    match classification {
        PromotionClassification::Coupon => {
            // Coupons need manual handling due to special fields (code, type, budget, etc.)
            warn!("Skipping coupon {}: manual handling required (code/type/budget). Manage in Seller Hub.", promotion.promotionId);
            return Ok(vec![]);
        },
        PromotionClassification::MarkdownSale => {
            let (should_end, should_extend, should_schedule_fallback) = 
                plan_markdown_sale(promotion, now);
            
            // Safety guard: Don't auto-end markdown sales with no listings (would create gap)
            if should_end && listings.is_empty() {
                warn!("Suppressing End for {}: markdown has zero listings; would create coverage gap", promotion.promotionId);
                return Ok(vec![]);
            }
            
            if should_end {
                actions.push(PromotionAction::End(promotion.promotionId.clone()));
                
                if should_schedule_fallback && !listings.is_empty() {
                    // Schedule fallback promotion for 14 days only if we have listings
                    let start_date = now.to_rfc3339();
                    let end_date = (now + chrono::Duration::days(14)).to_rfc3339();
                    actions.push(PromotionAction::StartFallback(
                        cli.fallback.clone(),
                        listings,
                        start_date,
                        end_date,
                    ));
                } else if should_schedule_fallback {
                    warn!("No listings found for fallback promotion after ending markdown sale {}", promotion.promotionId);
                }
            } else if should_extend {
                // Extend the markdown sale by 14 days (or up to the 45-day limit)
                let start_date = parse_rfc3339(&promotion.startDate).unwrap_or(now);
                let current_end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
                let max_end_date = start_date + chrono::Duration::days(45);
                
                let new_end_date = if current_end_date + chrono::Duration::days(14) > max_end_date {
                    max_end_date
                } else {
                    current_end_date + chrono::Duration::days(14)
                };
                
                // Guard: Never update to a past date, end the promotion instead
                if new_end_date <= now {
                    actions.push(PromotionAction::End(promotion.promotionId.clone()));
                    
                    // Schedule fallback if we have listings
                    if !listings.is_empty() {
                        let start_date = now.to_rfc3339();
                        let end_date = (now + chrono::Duration::days(14)).to_rfc3339();
                        actions.push(PromotionAction::StartFallback(
                            cli.fallback.clone(),
                            listings,
                            start_date,
                            end_date,
                        ));
                    } else {
                        warn!("No listings found for fallback after ending markdown sale {}", promotion.promotionId);
                    }
                } else if new_end_date != current_end_date {
                    // Only update if the date actually changes
                    actions.push(PromotionAction::UpdateEnd(
                        promotion.promotionId.clone(),
                        new_end_date.to_rfc3339(),
                    ));
                }
            }
        },
        PromotionClassification::VolumeDiscount => {
            let (_should_end, should_extend, _) = plan_volume_discount(promotion, now);
            
            // Volume discounts should never end, only extend if expiring within threshold
            if should_extend {
                let current_end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
                let new_end_date = now + chrono::Duration::days(30);
                
                // Only update if the new date is actually different and in the future
                if new_end_date > current_end_date && new_end_date > now {
                    actions.push(PromotionAction::UpdateEnd(
                        promotion.promotionId.clone(),
                        new_end_date.to_rfc3339(),
                    ));
                }
            }
        },
        PromotionClassification::Fallback => {
            let (should_end, should_extend, _) = plan_fallback(promotion, now);
            
            if should_end {
                actions.push(PromotionAction::End(promotion.promotionId.clone()));
                
                // After ending fallback, start a new 45-day markdown sale with same listings
                if !listings.is_empty() {
                    let start_date = now.to_rfc3339();
                    let end_date = (now + chrono::Duration::days(45)).to_rfc3339();
                    actions.push(PromotionAction::StartMarkdown(
                        listings,
                        start_date,
                        end_date,
                    ));
                    info!("Scheduled markdown sale to replace ending fallback {}", promotion.promotionId);
                } else {
                    warn!("No listings found to start markdown after ending fallback {}", promotion.promotionId);
                }
            } else if should_extend {
                // Extend the fallback promotion by 14 days
                let current_end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
                let new_end_date = current_end_date + chrono::Duration::days(14);
                
                // Guard: Never update to a past date
                if new_end_date <= now {
                    actions.push(PromotionAction::End(promotion.promotionId.clone()));
                } else if new_end_date != current_end_date {
                    actions.push(PromotionAction::UpdateEnd(
                        promotion.promotionId.clone(),
                        new_end_date.to_rfc3339(),
                    ));
                }
            }
        },
        PromotionClassification::Other => {
            // For other promotions, just extend if expiring
            let end_date = parse_rfc3339(&promotion.endDate).unwrap_or(now);
            let time_left = end_date - now;
            let should_extend = Duration::hours(cli.alert_window).num_hours() >= time_left.num_hours();
            
            if should_extend {
                let new_end_date = end_date + chrono::Duration::days(14);
                
                // Guard: Never update to a past date
                if new_end_date <= now {
                    actions.push(PromotionAction::End(promotion.promotionId.clone()));
                } else if new_end_date != end_date {
                    actions.push(PromotionAction::UpdateEnd(
                        promotion.promotionId.clone(),
                        new_end_date.to_rfc3339(),
                    ));
                }
            }
        },
    }
    
    Ok(actions)
}

// Execute actions in the correct order
async fn execute_actions(api: &MarketingApi, actions: &[PromotionAction], dry_run: bool) -> Result<(usize, usize)> {
    let mut idempotency_manager = IdempotencyManager::new();
    let mut ok = 0usize;
    let mut fail = 0usize;
    
    if dry_run {
        println!("\nDRY RUN MODE - No actions will be executed\n");
        for action in actions {
            println!("Would execute: {:?}", action);
        }
        return Ok((actions.len(), 0));
    }
    
    // Execution order:
    // 1. RemoveListing
    // 2. End
    // 3. UpdateEnd
    // 4. StartFallback
    // 5. StartMarkdown
    
    for action in actions {
        // Check idempotency
        if idempotency_manager.is_executed(action) {
            info!("Skipping duplicate action: {:?}", action);
            continue;
        }
        
        let result = match action {
            PromotionAction::RemoveListing(promotion_id, listing_id) => {
                execute_remove_listing(api, promotion_id, listing_id).await
            },
            PromotionAction::End(promotion_id) => {
                execute_end(api, promotion_id).await
            },
            PromotionAction::UpdateEnd(promotion_id, new_end_date) => {
                execute_update_end(api, promotion_id, new_end_date).await
            },
            PromotionAction::StartFallback(fallback_type, listings, start_date, end_date) => {
                execute_start_fallback(api, fallback_type, listings, start_date, end_date).await
            },
            PromotionAction::StartMarkdown(listings, start_date, end_date) => {
                execute_start_markdown(api, listings, start_date, end_date).await
            },
        };
        
        match result {
            Ok(_) => {
                ok += 1;
                idempotency_manager.mark_executed(action);
            },
            Err(e) => {
                fail += 1;
                error!("Action failed {:?}: {:?}", action, e);
            },
        }
    }
    
    Ok((ok, fail))
}

fn display(discounts: &[PromotionSummary], hours_window: i64, all_promos: &[PromotionSummary]) {
    use chrono::Utc;
    
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
        println!("Name: {}", d.name.clone().unwrap_or_else(|| "Unnamed Promotion".into()));
        
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
    
    // Add tally statistics
    let (total_all, by_type_all, by_status_all) = tally(all_promos);
    let (total_exp, by_type_exp, by_status_exp) = tally(discounts);
    
    println!("{}", "=".repeat(80));
    println!("TALLY");
    println!("{}", "=".repeat(80));
    println!("Recognized discounts (all): {}", total_all);
    print_sorted_map("By type (all)", &by_type_all);
    print_sorted_map("By status (all)", &by_status_all);
    println!("---");
    println!("Expiring within window: {}", total_exp);
    print_sorted_map("By type (expiring)", &by_type_exp);
    print_sorted_map("By status (expiring)", &by_status_exp);
    println!("{}", "=".repeat(80));
}

// Execute an UpdateEnd action
async fn execute_update_end(api: &MarketingApi, promotion_id: &str, new_end_date: &str) -> Result<()> {
    use serde_json::Value;
    
    // Get the full promotion details as raw JSON to preserve all fields
    let endpoint = if promotion_id.contains("@") {
        format!("/item_promotion/{}", promotion_id)
    } else {
        format!("/item_price_markdown_promotion/{}", promotion_id)
    };
    
    // Get the raw JSON response to preserve all fields
    let detail_json: Value = api.request_json(Method::GET, &endpoint, None::<&Value>).await
        .map_err(|e| anyhow::anyhow!("Failed to get promotion details: {}", e))?;
    
    // Clone the JSON and update only what we need
    let mut body = detail_json.clone();
    
    // Update the fields we need to change
    body["endDate"] = serde_json::json!(new_end_date);
    body["promotionStatus"] = serde_json::json!("SCHEDULED");  // Must use SCHEDULED for updates
    
    // Ensure description exists (some old promotions may not have it)
    if body.get("description").is_none() || body["description"].is_null() {
        body["description"] = serde_json::json!(crate::short_desc_tag());
    }
    
    // Determine promotion type and call appropriate update endpoint
    let promotion_type = body["promotionType"].as_str().unwrap_or("");
    
    if promotion_type == "MARKDOWN_SALE" {
        let _ = api.update_markdown_promotion(promotion_id, &body).await?;
    } else {
        let _ = api.update_item_promotion(promotion_id, &body).await?;
    }
    
    info!("Updated end date for promotion {} to {}", promotion_id, new_end_date);
    Ok(())
}

// Execute an End action by setting endDate to 5 minutes from now
async fn execute_end(api: &MarketingApi, promotion_id: &str) -> Result<()> {
    // To end a promotion, set endDate to 5 minutes from now
    // This avoids trying to use ENDED status which causes errors
    let end_soon = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
    
    // Just call execute_update_end with the near-future end date
    execute_update_end(api, promotion_id, &end_soon).await?;
    
    info!("Set promotion {} to end at {}", promotion_id, end_soon);
    Ok(())
}

// Execute a RemoveListing action (stub for now)
async fn execute_remove_listing(_api: &MarketingApi, promotion_id: &str, listing_id: &str) -> Result<()> {
    // TODO: Implement listing removal from promotion
    warn!("RemoveListing not yet implemented for promotion {} listing {}", promotion_id, listing_id);
    Ok(())
}

// Execute a StartMarkdown action
async fn execute_start_markdown(
    api: &MarketingApi,
    listings: &[ListingInfo],
    start_date: &str,
    end_date: &str,
) -> Result<()> {
    use serde_json::json;
    
    // Create a markdown promotion
    let listing_ids: Vec<String> = listings.iter().map(|l| l.listing_id.clone()).collect();
    
    let body = json!({
        "name": format!("Markdown Sale - {}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "description": short_desc_tag(),
        "startDate": start_date,
        "endDate": end_date,
        "marketplaceId": api.cfg.marketplace_id(),
        "promotionType": "MARKDOWN_SALE",
        "promotionStatus": "SCHEDULED",
        "selectedInventoryDiscounts": [{
            "discountBenefit": {
                "percentageOffItem": "20"
            },
            "inventoryCriterion": {
                "inventoryCriterionType": "INVENTORY_BY_VALUE",
                "listingIds": listing_ids
            }
        }]
    });
    
    let _ = api.create_markdown_promotion(&body).await?;
    info!("Started markdown sale for {} listings", listings.len());
    Ok(())
}

// Execute a StartFallback action
async fn execute_start_fallback(
    api: &MarketingApi,
    fallback_type: &FallbackType,
    listings: &[ListingInfo],
    start_date: &str,
    end_date: &str,
) -> Result<()> {
    use serde_json::json;
    
    // Create a fallback promotion (order discount or coupon)
    let listing_ids: Vec<String> = listings.iter().map(|l| l.listing_id.clone()).collect();
    
    let promotion_type = match fallback_type {
        FallbackType::Order => "ORDER_DISCOUNT",
        FallbackType::Coupon => "CODED_COUPON",
    };
    
    let body = json!({
        "name": format!("Fallback Promotion - {}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "description": short_desc_tag(),
        "startDate": start_date,
        "endDate": end_date,
        "marketplaceId": api.cfg.marketplace_id(),
        "promotionType": promotion_type,
        "promotionStatus": "SCHEDULED",
        "discountRules": [{
            "discountBenefit": {
                "percentageOffOrder": "10"
            },
            "discountSpecification": {
                "minAmount": {
                    "value": "50",
                    "currency": "USD"
                }
            }
        }],
        "inventoryCriterion": {
            "inventoryCriterionType": "INVENTORY_BY_VALUE",
            "listingIds": listing_ids
        }
    });
    
    let _ = api.create_item_promotion(&body).await?;
    info!("Started fallback promotion ({:?}) for {} listings", fallback_type, listings.len());
    Ok(())
}

#[derive(Parser)]
#[clap(name = "ebay-discount-manager", version = "0.1.0", about = "eBay Continuous Discount Manager")]
struct Cli {
    /// Policy to apply
    #[clap(long, value_enum, default_value = "us-continuous")]
    policy: Policy,

    /// Fallback promotion type
    #[clap(long, value_enum, default_value = "order")]
    fallback: FallbackType,

    /// Dry run mode - preview changes without executing
    #[clap(long, action = clap::ArgAction::SetTrue)]
    dry_run: bool,

    /// Alert window in hours
    #[clap(long, default_value = "48")]
    alert_window: i64,
}

#[derive(ValueEnum, Clone, Debug)]
enum Policy {
    #[clap(name = "us-continuous")]
    UsContinuous,
}

// Classification of promotion types
#[derive(Debug, Clone)]
enum PromotionClassification {
    /// Markdown sales that follow the 45-day max run with 14-day cool-off
    MarkdownSale,
    /// Volume discounts that should be kept continuous
    VolumeDiscount,
    /// Fallback promotions (order discount or coupon)
    Fallback,
    /// Coupon promotions (CODED_COUPON) - needs manual handling
    Coupon,
    /// Other promotion types
    Other,
}

// Idempotency key manager to prevent duplicate actions
struct IdempotencyManager {
    executed_actions: HashSet<String>,
}

impl IdempotencyManager {
    fn new() -> Self {
        Self {
            executed_actions: HashSet::new(),
        }
    }
    
    // Generate a unique key for an action
    fn generate_key(&self, action: &PromotionAction) -> String {
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        
        let mut hasher = DefaultHasher::new();
        action.hash(&mut hasher);
        format!("{}", hasher.finish())
    }
    
    // Check if an action has already been executed
    fn is_executed(&self, action: &PromotionAction) -> bool {
        let key = self.generate_key(action);
        self.executed_actions.contains(&key)
    }
    
    // Mark an action as executed
    fn mark_executed(&mut self, action: &PromotionAction) {
        let key = self.generate_key(action);
        self.executed_actions.insert(key);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let _ = FmtSubscriber::builder().without_time().try_init();
    let cli = Cli::parse();

    println!("{}", "=".repeat(80));
    println!("eBay Discount Manager");
    println!("{}", "=".repeat(80));

    let mut cfg = Config::from_env()?;
    cfg.alert_window_hours = cli.alert_window;
    info!("Environment: {}", cfg.env);
    info!("Policy: {:?}", cli.policy);
    info!("Fallback: {:?}", cli.fallback);
    info!("Dry run: {}", cli.dry_run);
    info!("Alert window: {} hours", cfg.alert_window_hours);

    let mut api = MarketingApi::new(cfg.clone());

    let all = match fetch_all_promotions(&api).await {
        Ok(v) => v,
        Err(ApiError::Http { status, body }) if status == 401 && body_has_error_1001(&body) => {
            warn!("401 Invalid token. Attempting auto-refresh via OAuth…");
            let new_token = refresh_access_token(&api.cfg).await?;
            update_env_access_token(&new_token)?;
            api.set_token(new_token);
            fetch_all_promotions(&api).await?
        }
        Err(e) => return Err(e.into()),
    };

    let expiring = filter_expiring(&all, api.cfg.alert_window_hours);
    display(&expiring, api.cfg.alert_window_hours, &all);

    // Build planned actions for RUNNING promotions only
    let mut all_actions = Vec::new();
    for promotion in &all {
        // Skip promotions that are not running
        if promotion.promotionStatus != "RUNNING" && promotion.promotionStatus != "SCHEDULED" {
            info!("Skipping {} promotion {}", promotion.promotionStatus, promotion.promotionId);
            continue;
        }
        let actions = build_actions(&api, promotion, &cli).await?;
        all_actions.extend(actions);
    }

    if !all_actions.is_empty() {
        println!("\nPlanned actions: {}", all_actions.len());
        for action in &all_actions {
            println!("  {:?}", action);
        }
        
        if !cli.dry_run && Confirm::new()
            .with_prompt("Execute planned actions?")
            .default(false)
            .interact()?
        {
            let (ok, fail) = execute_actions(&api, &all_actions, cli.dry_run).await?;
            println!("Finished. {} succeeded, {} failed.", ok, fail);
        } else if cli.dry_run {
            let (ok, fail) = execute_actions(&api, &all_actions, cli.dry_run).await?;
            println!("Finished. {} succeeded, {} failed.", ok, fail);
        }
    } else {
        println!("\nNo actions planned.");
    }
    
    Ok(())
}
