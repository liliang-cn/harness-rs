//! The synthesis prompt that turns the deep-dive findings into a strict
//! machine-readable action list, with prior decisions injected so the agent
//! doesn't re-propose what was already done on a previous run.

pub fn synthesis_prompt(prior_decisions: &str) -> String {
    let prior = if prior_decisions.trim().is_empty() {
        "(none — this is the first run)".to_string()
    } else {
        prior_decisions.to_string()
    };
    format!(
        "You are the operations lead. Using the upstream deep-dive findings, decide the concrete \
         operational actions to take tonight.\n\n\
         Actions ALREADY taken on previous runs (do NOT repeat these):\n{prior}\n\n\
         Output ONLY a JSON array (no prose) where each element is one action:\n\
         [{{\"kind\":\"reorder\",\"sku\":\"<SKU>\",\"qty\":<int>,\"reason\":\"<data point>\"}},\n \
          {{\"kind\":\"markdown\",\"sku\":\"<SKU>\",\"pct\":<int 1-60>,\"reason\":\"<data point>\"}},\n \
          {{\"kind\":\"pause_product\",\"sku\":\"<SKU>\",\"reason\":\"<data point>\"}}]\n\n\
         Rules: kind must be exactly reorder | markdown | pause_product. Use real SKUs from the \
         findings. EVERY reorder MUST include an integer \"qty\" (copy the suggested quantity from \
         the replenishment deep-dive). EVERY markdown MUST include an integer \"pct\" between 1 and \
         60 (copy the suggested discount from the liquidation deep-dive); include some markdowns at \
         20% or less. Keep it to the highest-impact 4-8 actions. Output the JSON array and nothing else."
    )
}
