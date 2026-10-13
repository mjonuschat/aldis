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
    let urls = instance_urls(file, &arguments.moonraker);
    aldis::agent::runtime::run_instances(urls, sink)
}
