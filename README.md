# eBay Promotions Manager

A Rust application for managing eBay promotions and automatically extending those that are about to expire.

## Features

- Fetches all active eBay promotions
- Identifies promotions expiring within a specified time window (default: 48 hours)
- Automatically extends expiring promotions by 2 weeks
- Handles OAuth token refresh automatically

## GitHub Actions

This repository uses GitHub Actions for CI with rust-cache to speed up builds:

- [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) - Caches Cargo registry, index, and build artifacts

## Setup

1. Create a `.env` file with your eBay API credentials:
   ```
   EBAY_CLIENT_ID=your_client_id
   EBAY_CLIENT_SECRET=your_client_secret
   EBAY_REFRESH_TOKEN=your_refresh_token
   EBAY_ACCESS_TOKEN=your_access_token
   EBAY_ENVIRONMENT=production
   ```

2. Run the application:
   ```bash
   cargo run --bin ebay-promotions
   ```

3. Run the token refresh utility:
   ```bash
   cargo run --bin ebay_refresh_token
   ```
