#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

#[cfg(not(target_os = "linux"))]
compile_error!("aldis supports Linux hosts only");

pub mod build;
pub mod checkout;
pub mod coordinator;
pub mod eligibility;
pub mod flash;
pub mod logging;
pub mod moonraker;
pub mod prepare;
pub mod retry;
pub mod self_update;
pub mod service;
pub mod workspace;

/// Renders an error together with its full `source()` chain, colon-separated.
///
/// Several `#[error(...)]` messages intentionally omit their own source's
/// text (so anyhow's `{:#}` chain walk doesn't print it twice once the error
/// reaches an `anyhow::Result`). Call sites that only have a bare
/// `&dyn Error` — not an owned, `'static` value `anyhow::Error::new` could
/// wrap — use this instead, so they still show the full detail in one shot.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}
