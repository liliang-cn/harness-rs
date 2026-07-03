//! Experience layer over a CortexDB brain.
//!
//! Records an episode (situation → tools used → outcome) into CortexDB, then
//! recalls it by a *paraphrased* situation — showing the experience layer +
//! CortexDB semantic recall working together. No model needed.
//!
//! ```sh
//! CORTEXDB_MCP_BIN=/path/to/cortexdb-mcp-stdio cargo run -p experience-cortexdb
//! ```

use harness_core::Memory;
use harness_cortexdb::CortexdbMemory;
use harness_experience::{Episode, ExperienceStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bin = std::env::var("CORTEXDB_MCP_BIN").unwrap_or_else(|_| "cortexdb-mcp-stdio".into());
    println!("connecting CortexDB brain via {bin} …");
    let mem: Arc<dyn Memory> = Arc::new(
        CortexdbMemory::connect_stdio(&bin, &[])
            .await?
            .with_namespace("harness-experience-demo"),
    );
    let store = ExperienceStore::new(mem).with_source("experience-demo");

    // 1) Record an experience.
    let ep = Episode::new(
        "the user asked me to deploy the website to production",
        "read the deploy config, ran the deploy script, verified the site was live",
    )
    .with_tools(["read_file", "shell", "web_fetch"]);
    println!("\nrecording experience:\n{}\n", ep.render());
    store.record(&ep).await?;

    // 2) Recall it by a *different wording* of the same situation.
    let query = "how did I publish my site last time";
    println!("recalling with paraphrase: \"{query}\"");
    let hits = store.recall(query, 3).await;

    println!("\n=== recalled {} experience(s) ===", hits.len());
    for (i, e) in hits.iter().enumerate() {
        println!(
            "{}. situation: {}\n   tools used: {}\n   outcome: {}",
            i + 1,
            e.situation,
            e.tools.join(", "),
            e.outcome,
        );
    }
    if hits.is_empty() {
        println!("(nothing recalled — check that CortexDB is running / the brain isn't empty)");
    }
    Ok(())
}
