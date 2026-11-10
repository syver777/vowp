//! Token counts, cost amounts and their provenance. Spec: `docs/design/types.md` §4 and
//! `docs/design/crates/vowp-protocol.md` §4 rule 7.
//!
//! A cost is never assumed free: when no price is known `usd` is `None` and the source is
//! [`CostSource::Unknown`]. Adding an unknown cost to a known one keeps the partial `usd`
//! value but degrades the source.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// Where a cost amount came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum CostSource {
    /// The harness reported it (`opencode` `step_finish.cost`, `claude` `result.total_cost_usd`).
    #[serde(rename = "receipt")]
    Receipt,
    /// Estimated from `[providers.*.prices]` in `vowp.toml`.
    #[serde(rename = "price-table")]
    PriceTable,
    /// No usable price: `usd` is `None`.
    #[serde(rename = "unknown")]
    Unknown,
}

impl CostSource {
    /// Returns the weaker (less trustworthy) of two sources.
    fn weaker(self, other: Self) -> Self {
        fn rank(source: CostSource) -> u8 {
            match source {
                CostSource::Receipt => 2,
                CostSource::PriceTable => 1,
                CostSource::Unknown => 0,
            }
        }
        if rank(self) <= rank(other) { self } else { other }
    }
}

/// Token usage broken down by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tokens {
    /// Non-cached input tokens.
    #[serde(default)]
    pub input: u64,
    /// Output tokens.
    #[serde(default)]
    pub output: u64,
    /// Reasoning tokens, when the harness reports them separately.
    #[serde(default)]
    pub reasoning: u64,
    /// Tokens read from the prompt cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write: u64,
}

impl Tokens {
    /// Adds another token count field by field.
    pub fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }

    /// Returns the sum of every token field.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// A cost amount with its token usage and provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Cost {
    /// Amount in USD; `None` exactly when `source` is [`CostSource::Unknown`].
    pub usd: Option<f64>,
    /// Token usage that produced the amount.
    pub tokens: Tokens,
    /// Where the amount came from.
    pub source: CostSource,
}

impl Cost {
    /// Creates an unknown cost: no USD amount, no tokens, source [`CostSource::Unknown`].
    pub fn unknown() -> Self {
        Self {
            usd: None,
            tokens: Tokens::default(),
            source: CostSource::Unknown,
        }
    }

    /// Creates a cost reported by the harness.
    pub fn receipt(usd: f64, tokens: Tokens) -> Self {
        Self {
            usd: Some(usd),
            tokens,
            source: CostSource::Receipt,
        }
    }

    /// Adds another cost: tokens add, USD sums when both sides know it, and the source
    /// degrades to the weaker of the two. A partial amount keeps its USD value.
    pub fn add(&mut self, other: &Cost) {
        self.tokens.add(&other.tokens);
        self.usd = match (self.usd, other.usd) {
            (Some(left), Some(right)) => Some(left + right),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        };
        self.source = self.source.weaker(other.source);
    }

    /// Checks the invariant that `usd` is `Some` exactly when the source is not
    /// [`CostSource::Unknown`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match (self.usd, self.source) {
            (Some(usd), CostSource::Unknown) => Err(ProtocolError::Config(format!(
                "inconsistent cost: usd is {usd} but source is unknown"
            ))),
            (None, CostSource::Receipt | CostSource::PriceTable) => Err(ProtocolError::Config(format!(
                "inconsistent cost: usd is missing but source is {:?}",
                self.source
            ))),
            _ => Ok(()),
        }
    }
}
