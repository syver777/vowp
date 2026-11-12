//! Price-table cost estimation for runs whose harness reported no receipt.
//! Spec: `docs/design/crates/vowp-core.md` §5 and `docs/design/adapters.md` §6.
//!
//! The table lives in `[providers.<provider>.prices.<model>]`, in USD per million tokens.
//! An unknown price is never treated as free: [`Cost::unknown`] is returned instead.

use vowp_protocol::{Cost, CostSource, Executor, HarnessKind, Tokens, VowpConfig};

/// Estimates the USD cost of `tokens` for `exec` from the configured price table.
///
/// The provider key is [`Executor::provider`]; for opencode the model key is the text after
/// the last `/` (for example `deepseek/deepseek-flash` → `deepseek-flash`). A missing entry,
/// or a run with no tokens, returns [`Cost::unknown`] with no token count.
///
/// Cache-read and cache-write tokens without their own price use the input price; reasoning
/// tokens are priced at the output rate.
pub fn estimate(cfg: &VowpConfig, exec: &Executor, tokens: &Tokens) -> Cost {
    if tokens.total() == 0 {
        return Cost::unknown();
    }
    let model_key = match exec.harness {
        HarnessKind::Opencode => exec.model.rsplit('/').next().unwrap_or(exec.model.as_str()),
        HarnessKind::ClaudeCode | HarnessKind::Cli | HarnessKind::Acp | HarnessKind::Vowp => exec.model.as_str(),
    };
    let Some(entry) = cfg
        .providers
        .get(&exec.provider())
        .and_then(|provider| provider.prices.get(model_key))
    else {
        return Cost::unknown();
    };

    let cache_read_per_m = entry.cache_read_per_m.unwrap_or(entry.input_per_m);
    let cache_write_per_m = entry.cache_write_per_m.unwrap_or(entry.input_per_m);
    let usd = (tokens.input as f64 * entry.input_per_m
        + (tokens.output + tokens.reasoning) as f64 * entry.output_per_m
        + tokens.cache_read as f64 * cache_read_per_m
        + tokens.cache_write as f64 * cache_write_per_m)
        / 1_000_000.0;
    Cost {
        usd: Some(usd),
        tokens: *tokens,
        source: CostSource::PriceTable,
    }
}
