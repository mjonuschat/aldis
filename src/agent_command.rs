//! `aldis agent`: connects to Moonraker and serves MCU status and updates.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use aldis::agent::connection::run_forever;
use aldis::agent::service::AgentService;
use aldis::agent::system::SystemBackend;

use crate::cli::AgentArgs;
use crate::fail;

pub(crate) fn agent(arguments: AgentArgs, verbose: u8) -> ExitCode {
    let sink = match aldis::logging::init_agent(verbose) {
        Ok(sink) => sink,
        Err(error) => return fail(format!("{error:#}")),
    };
    let url = arguments.moonraker.moonraker;
    let events = Arc::new(Mutex::new(None));
    let service = AgentService::new(SystemBackend::new(url.clone(), sink), Arc::clone(&events));
    run_forever(&url, service, events)
}
