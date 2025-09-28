use anyhow::Result;
use dialoguer::Confirm;
use dotenvy::dotenv;
use tracing::{error, info, warn};
use tracing_subscriber::FmtSubscriber;

use ebay_promotions_lib::{
    ApiError, Config, MarketingApi, body_has_error_1001, fetch_all_promotions,
    filter_expiring, refresh_access_token, update_env_access_token, parse_rfc3339,
    PromotionSummary, short_desc_tag, plus_days,
};

fn display(discounts: &[PromotionSummary], hours_window: i64) {
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
    
    println!("{}", "=".repeat(80));
    println!("Total expiring discounts: {}", discounts.len());
    println!("{}", "=".repeat(80));
}

async fn extend_promotion(api: &MarketingApi, p: &PromotionSummary) -> Result<()> {
    use ebay_promotions_lib::{plus_days};
    use serde_json::json;

    let id = &p.promotionId;
    let detail = if p.promotionType == "MARKDOWN_SALE" {
        api.get_markdown_promotion(id).await?
    } else {
        api.get_item_promotion(id).await?
    };
    
    let new_end = plus_days(&detail.endDate, 14)?;
    let mut body = json!({
        "name": detail.name,
        "description": crate::short_desc_tag(),
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
        body["promotionImageUrl"] = serde_json::json!(v); 
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

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    let _ = FmtSubscriber::builder().without_time().try_init();

    println!("{}", "=".repeat(80));
    println!("eBay Discount Manager");
    println!("{}", "=".repeat(80));

    let cfg = Config::from_env()?;
    info!("Environment: {}", cfg.env);
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
    display(&expiring, api.cfg.alert_window_hours);

    if !expiring.is_empty() && Confirm::new()
        .with_prompt("Extend all expiring discounts by 2 weeks?")
        .default(false)
        .interact()?
    {
        let mut ok = 0usize; 
        let mut fail = 0usize;
        
        for d in &expiring {
            match extend_promotion(&api, d).await {
                Ok(_) => ok += 1,
                Err(e) => { 
                    fail += 1; 
                    error!("Extend failed {}: {:?}", d.promotionId, e); 
                }
            }
        }
        
        println!("Finished. {} succeeded, {} failed.", ok, fail);
    }
    
    Ok(())
}
