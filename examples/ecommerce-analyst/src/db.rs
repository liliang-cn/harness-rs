//! Schema + a realistic synthetic seed for a small online shop.
//!
//! The generator is seeded (`StdRng::seed_from_u64`) so the data is
//! reproducible, but it is shaped to look and behave like a real store:
//! bestsellers and dead stock, varying margins, items below their reorder
//! level, a promo-weekend sales spike, and a few products whose quality
//! drags their review ratings down. The analyst agents discover all of this
//! by querying the live database — nothing is pre-summarized.

use chrono::{DateTime, Duration, Utc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, QueryBuilder, Row};

pub const DEFAULT_URL: &str = "postgres://postgres:ecom@localhost:38520/shop";

/// Connect with a small pool. Reads `DATABASE_URL`, else the docker default.
pub async fn connect() -> anyhow::Result<PgPool> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await?;
    Ok(pool)
}

pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    Ok(())
}

pub async fn is_seeded(pool: &PgPool) -> anyhow::Result<bool> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM products")
        .fetch_one(pool)
        .await?;
    let n: i64 = row.try_get("n")?;
    Ok(n > 0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS products (
    id               INT PRIMARY KEY,
    sku              TEXT UNIQUE NOT NULL,
    name             TEXT NOT NULL,
    category         TEXT NOT NULL,
    brand            TEXT NOT NULL,
    unit_price_cents INT  NOT NULL,
    unit_cost_cents  INT  NOT NULL,
    stock_qty        INT  NOT NULL,
    reorder_level    INT  NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS customers (
    id         INT PRIMARY KEY,
    name       TEXT NOT NULL,
    email      TEXT UNIQUE NOT NULL,
    city       TEXT NOT NULL,
    country    TEXT NOT NULL,
    segment    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE IF NOT EXISTS orders (
    id          INT PRIMARY KEY,
    customer_id INT NOT NULL REFERENCES customers(id),
    status      TEXT NOT NULL,
    channel     TEXT NOT NULL,
    ordered_at  TIMESTAMPTZ NOT NULL,
    total_cents INT NOT NULL
);
CREATE TABLE IF NOT EXISTS order_items (
    id               INT PRIMARY KEY,
    order_id         INT NOT NULL REFERENCES orders(id),
    product_id       INT NOT NULL REFERENCES products(id),
    qty              INT NOT NULL,
    unit_price_cents INT NOT NULL
);
CREATE TABLE IF NOT EXISTS reviews (
    id          INT PRIMARY KEY,
    product_id  INT NOT NULL REFERENCES products(id),
    customer_id INT NOT NULL REFERENCES customers(id),
    rating      INT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL
);
"#;

// ── catalogue building blocks ────────────────────────────────────────────

struct Cat {
    name: &'static str,
    code: &'static str,
    brands: &'static [&'static str],
    nouns: &'static [&'static str],
    price_lo: i32,
    price_hi: i32,
}

const CATS: &[Cat] = &[
    Cat {
        name: "Electronics",
        code: "ELC",
        brands: &["Aurora", "Nimbus", "Volt", "Pulse", "Kestrel"],
        nouns: &[
            "Wireless Earbuds Pro",
            "Smart Watch S2",
            "Bluetooth Speaker Mini",
            "4K Action Cam",
            "USB-C Hub 8-in-1",
            "Mechanical Keyboard",
            "Noise-Canceling Headphones",
            "Portable SSD 1TB",
        ],
        price_lo: 1900,
        price_hi: 29900,
    },
    Cat {
        name: "Home",
        code: "HOM",
        brands: &["CasaNova", "Hearth", "Lumora", "NestWell"],
        nouns: &[
            "Air Purifier X3",
            "Robot Vacuum",
            "Ceramic Knife Set",
            "Espresso Maker",
            "LED Desk Lamp",
            "Weighted Blanket",
            "Cast Iron Skillet",
        ],
        price_lo: 1500,
        price_hi: 39900,
    },
    Cat {
        name: "Beauty",
        code: "BTY",
        brands: &["Lume", "Verde", "Aura", "Skinsei"],
        nouns: &[
            "Vitamin C Serum",
            "Hydrating Cream",
            "Matte Lipstick",
            "Sheet Mask Pack",
            "Hair Repair Oil",
        ],
        price_lo: 900,
        price_hi: 6900,
    },
    Cat {
        name: "Sports",
        code: "SPT",
        brands: &["Apex", "Trailhead", "Vortex", "Stride"],
        nouns: &[
            "Yoga Mat Pro",
            "Resistance Band Set",
            "Insulated Bottle 1L",
            "Running Belt",
            "Foam Roller",
            "Adjustable Dumbbell",
        ],
        price_lo: 1200,
        price_hi: 12900,
    },
    Cat {
        name: "Toys",
        code: "TOY",
        brands: &["Wobble", "BrightBlox", "PlayNest"],
        nouns: &[
            "Wooden Blocks 100pc",
            "STEM Robot Kit",
            "Plush Dragon",
            "Puzzle 1000pc",
            "RC Car Turbo",
        ],
        price_lo: 990,
        price_hi: 8900,
    },
];

const FIRST: &[&str] = &[
    "Liam", "Olivia", "Noah", "Emma", "Oliver", "Ava", "Elijah", "Sophia", "Mateo", "Isabella",
    "Lucas", "Mia", "Levi", "Amelia", "Ethan", "Harper", "Mason", "Evelyn", "Wei", "Yuki", "Aarav",
    "Sofia", "Hugo", "Lina", "Marco", "Nadia",
];
const LAST: &[&str] = &[
    "Smith",
    "Johnson",
    "Williams",
    "Brown",
    "Jones",
    "Garcia",
    "Miller",
    "Davis",
    "Rodriguez",
    "Martinez",
    "Chen",
    "Tanaka",
    "Patel",
    "Müller",
    "Rossi",
    "Dubois",
    "Kowalski",
    "Andersson",
];
const PLACES: &[(&str, &str)] = &[
    ("New York", "US"),
    ("Los Angeles", "US"),
    ("Chicago", "US"),
    ("London", "UK"),
    ("Manchester", "UK"),
    ("Berlin", "DE"),
    ("Munich", "DE"),
    ("Toronto", "CA"),
    ("Sydney", "AU"),
    ("Singapore", "SG"),
    ("Paris", "FR"),
    ("Tokyo", "JP"),
];
const SEGMENTS: &[&str] = &["new", "returning", "returning", "returning", "vip"];
const CHANNELS: &[&str] = &["web", "web", "mobile", "mobile", "marketplace"];

struct Prod {
    id: i32,
    quality: u8,     // 1..5, hidden — biases reviews & demand
    popularity: f64, // relative demand weight
    price: i32,
}

// ── seed ─────────────────────────────────────────────────────────────────

/// Generate and insert the full dataset. Assumes empty tables.
pub async fn seed(pool: &PgPool) -> anyhow::Result<SeedStats> {
    let mut rng = StdRng::seed_from_u64(20260601);
    let now: DateTime<Utc> = DateTime::from_timestamp(1_780_000_000, 0).unwrap(); // fixed "now"

    // --- products ---
    let mut products: Vec<Prod> = Vec::new();
    {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO products (id, sku, name, category, brand, unit_price_cents, unit_cost_cents, stock_qty, reorder_level, created_at) ",
        );
        let mut rows: Vec<ProductRow> = Vec::new();
        let mut id = 0;
        for cat in CATS {
            for noun in cat.nouns {
                for brand in cat
                    .brands
                    .iter()
                    .take(if cat.nouns.len() > 6 { 1 } else { 2 })
                {
                    id += 1;
                    let price = rng.gen_range(cat.price_lo..=cat.price_hi);
                    let margin = rng.gen_range(0.35..0.62); // cost = price * (1-margin)
                    let cost = ((price as f64) * (1.0 - margin)).round() as i32;
                    let quality: u8 = [5u8, 4, 4, 3, 3, 2][rng.gen_range(0..6)];
                    let popularity = rng.gen_range(0.2f64..1.0).powf(2.0) + 0.05;
                    // dead stock: low popularity products carry more inventory
                    let stock = if popularity < 0.25 {
                        rng.gen_range(180..600)
                    } else {
                        rng.gen_range(0..260)
                    };
                    let reorder = rng.gen_range(20..60);
                    let created = now - Duration::days(rng.gen_range(200..500));
                    rows.push(ProductRow {
                        id,
                        sku: format!("{}-{:04}", cat.code, id),
                        name: format!("{brand} {noun}"),
                        category: cat.name.to_string(),
                        brand: brand.to_string(),
                        price,
                        cost,
                        stock,
                        reorder,
                        created,
                    });
                    products.push(Prod {
                        id,
                        quality,
                        popularity,
                        price,
                    });
                }
            }
        }
        qb.push_values(&rows, |mut b, p| {
            b.push_bind(p.id)
                .push_bind(&p.sku)
                .push_bind(&p.name)
                .push_bind(&p.category)
                .push_bind(&p.brand)
                .push_bind(p.price)
                .push_bind(p.cost)
                .push_bind(p.stock)
                .push_bind(p.reorder)
                .push_bind(p.created);
        });
        qb.build().execute(pool).await?;
    }

    // --- customers ---
    let n_customers = 400;
    {
        let mut rows: Vec<CustomerRow> = Vec::new();
        for id in 1..=n_customers {
            let first = FIRST[rng.gen_range(0..FIRST.len())];
            let last = LAST[rng.gen_range(0..LAST.len())];
            let (city, country) = PLACES[rng.gen_range(0..PLACES.len())];
            rows.push(CustomerRow {
                id,
                name: format!("{first} {last}"),
                email: format!(
                    "{}.{}{}@example.com",
                    first.to_lowercase(),
                    last.to_lowercase(),
                    id
                ),
                city: city.to_string(),
                country: country.to_string(),
                segment: SEGMENTS[rng.gen_range(0..SEGMENTS.len())].to_string(),
                created_at: now - Duration::days(rng.gen_range(10..400)),
            });
        }
        insert_customers(pool, &rows).await?;
    }

    // --- orders + items ---
    let n_orders = 3000;
    let mut order_rows: Vec<OrderRow> = Vec::with_capacity(n_orders as usize);
    let mut item_rows: Vec<ItemRow> = Vec::new();
    let mut item_id = 0;
    // popularity-weighted product picker
    let total_pop: f64 = products.iter().map(|p| p.popularity).sum();
    for oid in 1..=n_orders {
        // days_ago: weighted toward recent, with a promo spike ~45 days ago.
        let mut days_ago = rng.gen_range(0..180);
        if rng.gen_bool(0.12) {
            days_ago = rng.gen_range(44..48); // promo weekend cluster
        }
        let ordered_at = now - Duration::days(days_ago) - Duration::minutes(rng.gen_range(0..1440));
        let status = match rng.gen_range(0..100) {
            0..=84 => "completed",
            85..=92 => "shipped",
            93..=97 => "refunded",
            _ => "cancelled",
        };
        let channel = CHANNELS[rng.gen_range(0..CHANNELS.len())];
        let customer_id = rng.gen_range(1..=n_customers);
        let n_items = rng.gen_range(1..=4);
        let mut total = 0i32;
        for _ in 0..n_items {
            // weighted pick
            let mut t = rng.gen_range(0.0..total_pop);
            let mut chosen = &products[0];
            for p in &products {
                t -= p.popularity;
                if t <= 0.0 {
                    chosen = p;
                    break;
                }
            }
            let qty = rng.gen_range(1..=3);
            // occasional small discount
            let unit = if rng.gen_bool(0.15) {
                (chosen.price as f64 * 0.9).round() as i32
            } else {
                chosen.price
            };
            item_id += 1;
            item_rows.push(ItemRow {
                id: item_id,
                order_id: oid,
                product_id: chosen.id,
                qty,
                unit_price_cents: unit,
            });
            total += unit * qty;
        }
        order_rows.push(OrderRow {
            id: oid,
            customer_id,
            status: status.to_string(),
            channel: channel.to_string(),
            ordered_at,
            total_cents: total,
        });
    }
    insert_orders(pool, &order_rows).await?;
    insert_items(pool, &item_rows).await?;

    // --- reviews (rating biased by hidden product quality) ---
    let mut review_rows: Vec<ReviewRow> = Vec::new();
    let mut review_id = 0;
    let by_id: std::collections::HashMap<i32, &Prod> = products.iter().map(|p| (p.id, p)).collect();
    for it in &item_rows {
        if !rng.gen_bool(0.18) {
            continue; // ~18% of items get reviewed
        }
        let p = by_id[&it.product_id];
        let rating = biased_rating(&mut rng, p.quality);
        let (title, body) = review_text(&mut rng, rating);
        review_id += 1;
        review_rows.push(ReviewRow {
            id: review_id,
            product_id: it.product_id,
            customer_id: order_rows[(it.order_id - 1) as usize].customer_id,
            rating,
            title,
            body,
            created_at: order_rows[(it.order_id - 1) as usize].ordered_at
                + Duration::days(rng.gen_range(1..14)),
        });
    }
    insert_reviews(pool, &review_rows).await?;

    Ok(SeedStats {
        products: products.len(),
        customers: n_customers as usize,
        orders: order_rows.len(),
        order_items: item_rows.len(),
        reviews: review_rows.len(),
    })
}

fn biased_rating(rng: &mut StdRng, quality: u8) -> i32 {
    // Center the distribution on quality, clamp to 1..5.
    let noise: f64 = rng.gen_range(-1.2..1.2);
    ((quality as f64) + noise).round().clamp(1.0, 5.0) as i32
}

fn review_text(rng: &mut StdRng, rating: i32) -> (String, String) {
    let (titles, bodies): (&[&str], &[&str]) = match rating {
        5 => (
            &[
                "Absolutely love it",
                "Exceeded expectations",
                "Best purchase this year",
            ],
            &[
                "Works flawlessly and feels premium. Would buy again.",
                "Shipping was fast and the quality is outstanding.",
                "Exactly as described — five stars.",
            ],
        ),
        4 => (
            &["Very good", "Happy with it", "Solid choice"],
            &[
                "Does the job well, minor nitpicks but overall great.",
                "Good value for the price. Recommended.",
                "Pretty good, would have liked a longer cable.",
            ],
        ),
        3 => (
            &["It's okay", "Average", "Does the job"],
            &[
                "Nothing special but it works.",
                "Fine for the price, don't expect too much.",
                "Mixed feelings — fine but not impressed.",
            ],
        ),
        2 => (
            &["Disappointed", "Not great", "Expected more"],
            &[
                "Quality feels cheap and it stopped working after a week.",
                "Not as described, a bit flimsy.",
                "Had issues out of the box. Meh.",
            ],
        ),
        _ => (
            &["Terrible", "Waste of money", "Do not buy"],
            &[
                "Broke on day two. Asking for a refund.",
                "Poor build quality, very disappointed.",
                "Stopped working almost immediately.",
            ],
        ),
    };
    (
        titles[rng.gen_range(0..titles.len())].to_string(),
        bodies[rng.gen_range(0..bodies.len())].to_string(),
    )
}

#[derive(Debug)]
pub struct SeedStats {
    pub products: usize,
    pub customers: usize,
    pub orders: usize,
    pub order_items: usize,
    pub reviews: usize,
}

// ── row structs + chunked inserts ────────────────────────────────────────

struct ProductRow {
    id: i32,
    sku: String,
    name: String,
    category: String,
    brand: String,
    price: i32,
    cost: i32,
    stock: i32,
    reorder: i32,
    created: DateTime<Utc>,
}
struct CustomerRow {
    id: i32,
    name: String,
    email: String,
    city: String,
    country: String,
    segment: String,
    created_at: DateTime<Utc>,
}
struct OrderRow {
    id: i32,
    customer_id: i32,
    status: String,
    channel: String,
    ordered_at: DateTime<Utc>,
    total_cents: i32,
}
struct ItemRow {
    id: i32,
    order_id: i32,
    product_id: i32,
    qty: i32,
    unit_price_cents: i32,
}
struct ReviewRow {
    id: i32,
    product_id: i32,
    customer_id: i32,
    rating: i32,
    title: String,
    body: String,
    created_at: DateTime<Utc>,
}

async fn insert_customers(pool: &PgPool, rows: &[CustomerRow]) -> anyhow::Result<()> {
    for chunk in rows.chunks(500) {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO customers (id, name, email, city, country, segment, created_at) ",
        );
        qb.push_values(chunk, |mut b, c| {
            b.push_bind(c.id)
                .push_bind(&c.name)
                .push_bind(&c.email)
                .push_bind(&c.city)
                .push_bind(&c.country)
                .push_bind(&c.segment)
                .push_bind(c.created_at);
        });
        qb.build().execute(pool).await?;
    }
    Ok(())
}

async fn insert_orders(pool: &PgPool, rows: &[OrderRow]) -> anyhow::Result<()> {
    for chunk in rows.chunks(800) {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO orders (id, customer_id, status, channel, ordered_at, total_cents) ",
        );
        qb.push_values(chunk, |mut b, o| {
            b.push_bind(o.id)
                .push_bind(o.customer_id)
                .push_bind(&o.status)
                .push_bind(&o.channel)
                .push_bind(o.ordered_at)
                .push_bind(o.total_cents);
        });
        qb.build().execute(pool).await?;
    }
    Ok(())
}

async fn insert_items(pool: &PgPool, rows: &[ItemRow]) -> anyhow::Result<()> {
    for chunk in rows.chunks(1000) {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO order_items (id, order_id, product_id, qty, unit_price_cents) ",
        );
        qb.push_values(chunk, |mut b, i| {
            b.push_bind(i.id)
                .push_bind(i.order_id)
                .push_bind(i.product_id)
                .push_bind(i.qty)
                .push_bind(i.unit_price_cents);
        });
        qb.build().execute(pool).await?;
    }
    Ok(())
}

async fn insert_reviews(pool: &PgPool, rows: &[ReviewRow]) -> anyhow::Result<()> {
    for chunk in rows.chunks(800) {
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO reviews (id, product_id, customer_id, rating, title, body, created_at) ",
        );
        qb.push_values(chunk, |mut b, r| {
            b.push_bind(r.id)
                .push_bind(r.product_id)
                .push_bind(r.customer_id)
                .push_bind(r.rating)
                .push_bind(&r.title)
                .push_bind(&r.body)
                .push_bind(r.created_at);
        });
        qb.build().execute(pool).await?;
    }
    Ok(())
}
