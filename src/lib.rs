#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

#[cfg(not(target_os = "linux"))]
compile_error!("mcu-update supports Linux hosts only");

pub mod build;
pub mod checkout;
pub mod coordinator;
pub mod eligibility;
pub mod flash;
pub mod moonraker;
pub mod plan;
pub mod prepare;
pub mod run_log;
pub mod service;
pub mod workspace;
