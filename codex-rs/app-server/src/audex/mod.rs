mod admission;
mod backends;
pub(crate) mod capture;
pub(crate) mod controller;
mod dispatch;
#[cfg(target_os = "linux")]
mod external;
mod lifecycle;
mod report;
mod result;
mod retained;
mod reviewer;
mod routing;
pub(crate) mod snapshot;
mod source;
pub(crate) mod source_mcp;
mod store;
mod tools;

#[cfg(test)]
mod tests;
