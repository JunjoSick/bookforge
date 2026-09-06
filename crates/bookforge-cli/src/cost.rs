//! Thin re-export of the canonical pricing loader in
//! [`bookforge_core::providers`]. The typed schema, embedded default JSON,
//! `BOOKFORGE_PRICING_PATH` override, and schema-version checks live there so
//! the CLI, dashboard, and judge tooling share one implementation.

pub(crate) use bookforge_core::providers::{
    estimate_cost_usd_with_cached, estimate_cost_usd_with_pricing, load_pricing,
};
