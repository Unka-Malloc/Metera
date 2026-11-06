pub const PROTOCOL: &str = "metera.kernel.v1";
pub mod store;
pub use acop_usage_core::{ReadPlan, SchemaCapture, SqliteUsageSpec, UsageSession};
#[cfg(feature = "host")]
mod host;
#[cfg(feature = "host")]
mod store_host;
#[cfg(feature = "host")]
pub use host::{Error, Kernel};
#[cfg(all(feature = "host", test))]
mod store_host_tests;
