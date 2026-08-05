//! Compile a query to SQL — the Rust counterpart of `di explain`, so the two
//! implementations can be diffed against the same model.
//!
//!   cargo run -p harness-rs-semantic --example explain -- <model.yaml> <metrics> [group_by] [dialect] [grain]

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: explain <model.yaml> <m1,m2> [d1,d2] [dialect] [grain]");
        std::process::exit(2);
    }
    let src = std::fs::read_to_string(&a[0]).expect("read model");
    let m = harness_semantic::Model::from_yaml(&src).expect("parse model");
    let split = |s: &str| -> Vec<String> {
        s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
    };
    let q = harness_semantic::Query {
        metrics: split(&a[1]),
        group_by: a.get(2).map(|s| split(s)).unwrap_or_default(),
        time_grain: a.get(4).cloned().unwrap_or_default(),
        ..Default::default()
    };
    let d = harness_semantic::dialect::by_name(a.get(3).map(String::as_str).unwrap_or("postgres"))
        .expect("unknown dialect");
    match harness_semantic::compile(&m, &q, d.as_ref()) {
        Ok(c) => println!("{}", c.sql),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
