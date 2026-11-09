//! `aldis agent`: connects to every configured Moonraker and serves MCU status and updates.

use std::process::ExitCode;

use aldis::config::{FleetConfig, instance_urls};

use crate::cli::AgentArgs;
use crate::fail;

pub(crate) fn agent(arguments: AgentArgs, verbose: u8) -> ExitCode {
    let file = match arguments.config.as_deref().map(FleetConfig::load) {
        Some(Ok(file)) => Some(file),
        Some(Err(error)) => return fail(aldis::error_chain(&error)),
        None => None,
    };
    let sink = match aldis::logging::init_agent(verbose) {
        Ok(sink) => sink,
        Err(error) => return fail(format!("{error:#}")),
    };
    let sources = aldis::config::FirmwareSources::from_config(file.as_ref());
    let options = file
        .as_ref()
        .map(aldis::config::FleetConfig::tls_options)
        .unwrap_or_default();
    // Fail at startup rather than logging a failed connection every retry forever.
    let tls = match aldis::tls::Tls::new(&options) {
        Ok(tls) => tls,
        Err(error) => return fail(aldis::error_chain(&error)),
    };
    if options.insecure_skip_verify {
        tracing::warn!(
            "insecure_skip_verify is set: the certificate of an https:// Moonraker is not checked"
        );
    }
    let urls = instance_urls(file, &arguments.moonraker);
    if let Some(url) = aldis::config::unsupported_url(&urls) {
        return fail(format!(
            "the agent connects to Moonraker over http:// or https:// and cannot use {url}"
        ));
    }
    aldis::agent::runtime::run_instances(urls, sources, tls, sink)
}
