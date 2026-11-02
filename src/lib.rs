#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

#[cfg(not(target_os = "linux"))]
compile_error!("aldis supports Linux hosts only");

pub mod agent;
pub mod build;
pub mod checkout;
pub mod config;
pub mod coordinator;
pub mod eligibility;
pub mod flash;
pub mod flash_order;
pub mod fleet;
pub mod host;
pub mod identify;
pub mod kconfig;
pub mod lock;
pub mod logging;
pub mod moonraker;
pub mod prepare;
pub mod retry;
pub mod self_update;
pub mod service;
pub mod update_run;
pub mod workspace;

/// Renders an error together with its full `source()` chain, colon-separated.
///
/// Several `#[error(...)]` messages intentionally omit their own source's
/// text (so anyhow's `{:#}` chain walk doesn't print it twice once the error
/// reaches an `anyhow::Result`). Call sites that only have a bare
/// `&dyn Error` — not an owned, `'static` value `anyhow::Error::new` could
/// wrap — use this instead, so they still show the full detail in one shot.
///
/// A cause whose text its parent already rendered (an `#[error("...: {0}")]` that also marks
/// `0` as the source) is not repeated.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    enum Outer {
        #[error("outer failed")]
        Headline(#[source] Inner),
        #[error("outer failed: {0}")]
        Embedding(#[source] Inner),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("inner failed")]
    struct Inner(#[source] std::io::Error);

    #[test]
    fn renders_each_cause_once_whether_or_not_its_parent_embedded_it() {
        let io = || std::io::Error::other("disk on fire");
        assert_eq!(
            error_chain(&Outer::Headline(Inner(io()))),
            "outer failed: inner failed: disk on fire"
        );
        assert_eq!(
            error_chain(&Outer::Embedding(Inner(io()))),
            "outer failed: inner failed: disk on fire"
        );
    }
}
