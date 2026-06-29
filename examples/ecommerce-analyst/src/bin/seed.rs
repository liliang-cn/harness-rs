//! Seed the shop database with realistic synthetic data.
//!
//! ```sh
//! ./examples/ecommerce-analyst/setup.sh          # start postgres in docker
//! export DATABASE_URL=postgres://postgres:ecom@localhost:38520/shop
//! cargo run -p ecommerce-analyst --bin seed
//! ```

use ecommerce_analyst::db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = db::connect().await?;
    db::ensure_schema(&pool).await?;

    if db::is_seeded(&pool).await? {
        println!("database already seeded — nothing to do (drop tables to reseed).");
        return Ok(());
    }

    println!("generating realistic shop data …");
    let stats = db::seed(&pool).await?;
    println!(
        "seeded: {} products, {} customers, {} orders, {} order_items, {} reviews",
        stats.products, stats.customers, stats.orders, stats.order_items, stats.reviews
    );
    Ok(())
}
