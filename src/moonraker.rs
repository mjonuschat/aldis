use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McuInventory {
    pub mcus: Vec<Mcu>,
    /// MCUs configured in Klipper that Klippy never identified (it stopped before reaching them).
    pub unreported: Vec<UnreportedMcu>,
}

/// A configured MCU whose Moonraker object came back empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreportedMcu {
    pub name: String,
    pub transport: Option<McuTransport>,
    pub reason: UnreportedReason,
}

/// Why an unreported MCU has no firmware details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreportedReason {
    /// Klippy never reached it and aldis did not (or could not) query it directly.
    NotIdentified,
    /// aldis queried it directly and it did not answer.
    NotResponding,
}

impl std::fmt::Display for UnreportedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotIdentified => "not identified by Klipper",
            Self::NotResponding => "not responding; run `aldis reboot` or power-cycle the board",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mcu {
    pub name: String,
    pub app: Option<String>,
    pub version: Option<String>,
    pub mcu: String,
    pub canbus_frequency_hz: Option<u64>,
    /// The configured host transport, if Moonraker exposed one for this MCU.
    pub transport: Option<McuTransport>,
    pub kconfig: String,
}

/// An explicitly configured host transport for one MCU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McuTransport {
    /// A serial device path from the MCU's `serial` setting.
    Serial {
        /// The configured device path.
        device: String,
    },
    /// A CAN interface and Katapult identity from the MCU's settings.
    Can {
        /// The configured SocketCAN interface.
        interface: String,
        /// The six-byte Katapult CAN UUID.
        uuid: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MoonrakerError {
    #[error("Moonraker request failed")]
    Http(#[from] ureq::Error),
    #[error("Moonraker is running but Klipper isn't connected")]
    KlippyNotConnected,
    #[error("Klipper is still starting up: {0}")]
    KlippyStarting(String),
    #[error("Moonraker returned invalid JSON")]
    Json(#[from] serde_json::Error),
    #[error("Moonraker response is invalid: {0}")]
    InvalidResponse(String),
    #[error("Klipper could not load its configuration: {0}")]
    ConfigError(String),
}

/// A source of Klipper MCU inventory from Moonraker.
///
/// Isolates callers that only need to discover MCUs from the concrete
/// `ureq`-backed [`MoonrakerAdapter`], so they can be tested against a fake.
pub trait MoonrakerPort {
    /// Queries Moonraker for the currently configured MCU objects.
    fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError>;
}

/// Klipper's `print_stats.state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrintState {
    Standby,
    Printing,
    Paused,
    Complete,
    Cancelled,
    Error,
    /// A state this version of aldis does not recognize.
    Unknown(String),
}

impl PrintState {
    /// Whether stopping Klipper now cannot interrupt a print.
    pub fn is_idle(&self) -> bool {
        matches!(
            self,
            Self::Standby | Self::Complete | Self::Cancelled | Self::Error
        )
    }
}

impl std::fmt::Display for PrintState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Standby => "standby",
            Self::Printing => "printing",
            Self::Paused => "paused",
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::Unknown(state) => state,
        })
    }
}

/// A source of Klipper's current print state.
pub trait PrinterStatePort {
    /// Queries Klipper's `print_stats` state through Moonraker.
    fn print_state(&self) -> Result<PrintState, MoonrakerError>;
}

/// Host facts Moonraker reports about the Klipper instance it manages.
pub trait HostPort {
    /// The systemd unit Moonraker pairs with, from `machine.system_info`; `None` when unreported.
    fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError>;

    /// Klippy's state, version, and source path; `Disconnected` when Klippy is not connected.
    fn host_info(&self) -> Result<HostInfo, MoonrakerError>;

    /// Moonraker's registered `logs` root (what frontends' log browsers show).
    fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError>;
}

impl HostPort for MoonrakerAdapter {
    fn klipper_unit(&self) -> Result<Option<String>, MoonrakerError> {
        let response = self
            .agent
            .get(&self.endpoint("/machine/system_info"))
            .call()
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;
        parse_klipper_unit(&response)
    }

    fn host_info(&self) -> Result<HostInfo, MoonrakerError> {
        match self.agent.get(&self.endpoint("/printer/info")).call() {
            Ok(mut response) => parse_host_info(&response.body_mut().read_to_string()?),
            Err(ureq::Error::StatusCode(503)) => Ok(HostInfo {
                state: KlippyState::Disconnected,
                state_message: "Klipper is not connected to Moonraker".to_owned(),
                software_version: None,
                klipper_path: None,
            }),
            Err(error) => Err(MoonrakerError::Http(error)),
        }
    }

    fn logs_root(&self) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
        let response = self
            .agent
            .get(&self.endpoint("/server/files/roots"))
            .call()
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;
        parse_logs_root(&response)
    }
}

/// Extracts the `logs` root path from a `/server/files/roots` response.
pub fn parse_logs_root(response: &str) -> Result<Option<std::path::PathBuf>, MoonrakerError> {
    let response: Value = serde_json::from_str(response)?;
    Ok(response
        .get("result")
        .and_then(Value::as_array)
        .and_then(|roots| roots.iter().find(|root| root["name"] == "logs"))
        .and_then(|root| root["path"].as_str())
        .map(std::path::PathBuf::from))
}

/// Extracts `instance_ids.klipper` from a `/machine/system_info` response.
pub fn parse_klipper_unit(response: &str) -> Result<Option<String>, MoonrakerError> {
    let response: Value = serde_json::from_str(response)?;
    Ok(response
        .pointer("/result/system_info/instance_ids/klipper")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

pub struct MoonrakerAdapter {
    base_url: String,
    agent: ureq::Agent,
}

impl MoonrakerPort for MoonrakerAdapter {
    fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
        MoonrakerAdapter::discover_mcus(self)
    }
}

impl PrinterStatePort for MoonrakerAdapter {
    fn print_state(&self) -> Result<PrintState, MoonrakerError> {
        let body = serde_json::to_string(&json!({ "objects": { "print_stats": ["state"] } }))?;
        let response = self
            .agent
            .post(&self.endpoint("/printer/objects/query"))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;

        parse_print_state(&response)
    }
}

impl MoonrakerAdapter {
    pub fn new(base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build();

        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            agent: config.into(),
        }
    }

    pub fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError> {
        let response = self
            .agent
            .get(&self.endpoint("/printer/info"))
            .call()
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;
        let info: PrinterInfoResponse = serde_json::from_str(&response)?;
        check_discoverable(&info.result.state, &info.result.state_message)?;
        let object_names = self.mcu_object_names()?;
        if object_names.is_empty() {
            return Err(no_mcu_objects(
                &info.result.state,
                &info.result.state_message,
            ));
        }
        let mut objects = object_names
            .iter()
            .map(|name| (name.clone(), Value::Null))
            .collect::<BTreeMap<_, _>>();
        objects.insert("configfile".to_owned(), Value::Null);
        let body = serde_json::to_string(&json!({ "objects": objects }))?;
        let response = self
            .agent
            .post(&self.endpoint("/printer/objects/query"))
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;

        parse_inventory(&response)
    }

    fn mcu_object_names(&self) -> Result<Vec<String>, MoonrakerError> {
        let response = self
            .agent
            .get(&self.endpoint("/printer/objects/list"))
            .call()
            .map_err(map_http_error)?
            .body_mut()
            .read_to_string()?;
        let objects: ObjectListResponse = serde_json::from_str(&response)?;
        Ok(objects
            .result
            .objects
            .into_iter()
            .filter(|name| name == "mcu" || name.starts_with("mcu "))
            .collect())
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// Maps a transport failure to a `MoonrakerError`, special-casing the status
/// Moonraker returns on its `/printer/*` endpoints when Moonraker itself is
/// reachable but Klippy is not connected.
fn map_http_error(error: ureq::Error) -> MoonrakerError {
    match error {
        ureq::Error::StatusCode(503) => MoonrakerError::KlippyNotConnected,
        error => MoonrakerError::Http(error),
    }
}

/// Rejects Klippy states whose MCU objects are not populated yet.
///
/// `error` and `shutdown` are accepted: Klippy registers MCU objects at config load and fills in
/// each one's identify data before any protocol check, so they stay readable after a failure.
/// `startup` is rejected because Moonraker can list an `mcu` object before its `mcu_constants`
/// exist, which would otherwise surface as a confusing parse failure.
fn check_discoverable(state: &str, state_message: &str) -> Result<(), MoonrakerError> {
    match state {
        "ready" | "error" | "shutdown" => Ok(()),
        _ => Err(MoonrakerError::KlippyStarting(state_message.to_owned())),
    }
}

/// An errored Klippy without MCU objects failed before loading its config.
fn no_mcu_objects(state: &str, state_message: &str) -> MoonrakerError {
    if state == "error" {
        MoonrakerError::ConfigError(state_message.to_owned())
    } else {
        MoonrakerError::InvalidResponse("no MCU objects were reported".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        McuSettings, McuTransport, MoonrakerError, PrintState, check_discoverable, map_http_error,
        no_mcu_objects, parse_print_state, parse_transport,
    };

    fn print_stats_response(state: &str) -> String {
        format!(
            r#"{{"result":{{"eventtime":1.0,"status":{{"print_stats":{{"state":"{state}"}}}}}}}}"#
        )
    }

    #[test]
    fn parses_every_klipper_print_state() {
        for (raw, expected) in [
            ("standby", PrintState::Standby),
            ("printing", PrintState::Printing),
            ("paused", PrintState::Paused),
            ("complete", PrintState::Complete),
            ("cancelled", PrintState::Cancelled),
            ("error", PrintState::Error),
        ] {
            assert_eq!(
                parse_print_state(&print_stats_response(raw)).expect("state should parse"),
                expected
            );
        }
    }

    #[test]
    fn treats_only_finished_or_idle_states_as_idle() {
        assert!(PrintState::Standby.is_idle());
        assert!(PrintState::Complete.is_idle());
        assert!(PrintState::Cancelled.is_idle());
        assert!(PrintState::Error.is_idle());
        assert!(!PrintState::Printing.is_idle());
        assert!(!PrintState::Paused.is_idle());
        assert!(!PrintState::Unknown("resuming".to_owned()).is_idle());
    }

    #[test]
    fn keeps_an_unrecognized_print_state_verbatim() {
        assert_eq!(
            parse_print_state(&print_stats_response("resuming")).expect("state should parse"),
            PrintState::Unknown("resuming".to_owned())
        );
    }

    #[test]
    fn rejects_a_response_without_print_stats() {
        let error = parse_print_state(r#"{"result":{"status":{}}}"#).unwrap_err();

        assert!(matches!(error, MoonrakerError::InvalidResponse(_)));
    }

    #[test]
    fn reports_klippy_not_connected_for_a_503_status() {
        assert!(matches!(
            map_http_error(ureq::Error::StatusCode(503)),
            MoonrakerError::KlippyNotConnected
        ));
    }

    #[test]
    fn passes_other_http_errors_through_unchanged() {
        assert!(matches!(
            map_http_error(ureq::Error::StatusCode(404)),
            MoonrakerError::Http(ureq::Error::StatusCode(404))
        ));
    }

    #[test]
    fn accepts_ready_error_and_shutdown_for_discovery() {
        for state in ["ready", "error", "shutdown"] {
            assert!(check_discoverable(state, "msg").is_ok(), "{state}");
        }
    }

    #[test]
    fn reports_klipper_still_starting_with_moonrakers_own_state_message() {
        let error = check_discoverable("startup", "Loading configuration...").unwrap_err();
        assert!(matches!(
            error,
            MoonrakerError::KlippyStarting(message) if message == "Loading configuration..."
        ));
    }

    #[test]
    fn reports_a_config_error_when_an_errored_klippy_has_no_mcu_objects() {
        assert!(matches!(
            no_mcu_objects("error", "Option 'foo' is not valid"),
            MoonrakerError::ConfigError(message) if message == "Option 'foo' is not valid"
        ));
        assert!(matches!(
            no_mcu_objects("ready", "Printer is ready"),
            MoonrakerError::InvalidResponse(_)
        ));
    }

    #[test]
    fn defaults_an_omitted_canbus_interface_to_can0() {
        let settings = McuSettings {
            serial: None,
            canbus_uuid: Some("e7819ed8e7d3".to_owned()),
            canbus_interface: None,
        };

        let transport = parse_transport("mcu toolhead", &settings).unwrap();

        assert_eq!(
            transport,
            Some(McuTransport::Can {
                interface: "can0".to_owned(),
                uuid: 0xe781_9ed8_e7d3,
            })
        );
    }

    #[test]
    fn rejects_an_explicitly_empty_canbus_interface() {
        let settings = McuSettings {
            serial: None,
            canbus_uuid: Some("e7819ed8e7d3".to_owned()),
            canbus_interface: Some(String::new()),
        };

        let error = parse_transport("mcu toolhead", &settings).unwrap_err();

        assert!(matches!(error, MoonrakerError::InvalidResponse(_)));
    }
}

pub fn parse_inventory(response: &str) -> Result<McuInventory, MoonrakerError> {
    let response: ObjectQueryResponse = serde_json::from_str(response)?;
    let settings = response
        .result
        .status
        .get("configfile")
        .and_then(|status| status.settings.clone())
        .unwrap_or_default();
    let mut mcus = Vec::new();
    let mut unreported = Vec::new();
    for (name, status) in response.result.status {
        if name == "configfile" {
            continue;
        }
        // Case-insensitive: configfile.settings lowercases section names, objects.list keeps
        // the declared case (e.g. `[mcu RP2040]`).
        let transport = settings
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&name))
            .map(|(_, settings)| parse_transport(&name, settings))
            .transpose()?
            .flatten();
        if status.mcu_version.is_none() && status.mcu_constants.is_empty() {
            unreported.push(UnreportedMcu {
                name,
                transport,
                reason: UnreportedReason::NotIdentified,
            });
            continue;
        }
        let mcu = status
            .mcu_constants
            .get("MCU")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MoonrakerError::InvalidResponse(format!(
                    "MCU object {name:?} does not expose mcu_constants.MCU"
                ))
            })?
            .to_owned();
        mcus.push(Mcu {
            app: status.app,
            version: status.mcu_version,
            mcu,
            canbus_frequency_hz: status
                .mcu_constants
                .get("CANBUS_FREQUENCY")
                .and_then(Value::as_u64),
            transport,
            // Firmware not built by aldis (e.g. Beacon) has no Kconfig; `classify_mcu` treats an
            // empty one as `Unsupported` rather than failing discovery.
            kconfig: status.mcu_kconfig.unwrap_or_default(),
            name,
        });
    }

    if mcus.is_empty() && unreported.is_empty() {
        return Err(MoonrakerError::InvalidResponse(
            "query returned no MCU objects".to_owned(),
        ));
    }

    Ok(McuInventory { mcus, unreported })
}

fn parse_transport(
    name: &str,
    settings: &McuSettings,
) -> Result<Option<McuTransport>, MoonrakerError> {
    let Some(canbus_uuid) = settings.canbus_uuid.as_deref() else {
        return Ok(settings.serial.as_ref().map(|device| McuTransport::Serial {
            device: device.clone(),
        }));
    };
    // Klipper defaults `canbus_interface` to "can0" when the option is omitted
    // (klippy/mcu.py: `config.get('canbus_interface', 'can0')`).
    let interface = settings.canbus_interface.as_deref().unwrap_or("can0");
    if interface.is_empty() {
        return Err(MoonrakerError::InvalidResponse(format!(
            "MCU object {name:?} configures an empty canbus_interface"
        )));
    }
    let uuid = u64::from_str_radix(canbus_uuid.trim_start_matches("0x"), 16).map_err(|_| {
        MoonrakerError::InvalidResponse(format!(
            "MCU object {name:?} configures an invalid canbus_uuid"
        ))
    })?;
    if uuid > 0xffff_ffff_ffff {
        return Err(MoonrakerError::InvalidResponse(format!(
            "MCU object {name:?} configures a canbus_uuid larger than six bytes"
        )));
    }
    Ok(Some(McuTransport::Can {
        interface: interface.to_owned(),
        uuid,
    }))
}

/// Klippy's lifecycle state as Moonraker reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KlippyState {
    Ready,
    Startup,
    Error,
    Shutdown,
    /// Moonraker is up but Klippy is not connected (including Klipper stopped).
    Disconnected,
}

/// Klippy facts from `printer.info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub state: KlippyState,
    pub state_message: String,
    pub software_version: Option<String>,
    pub klipper_path: Option<std::path::PathBuf>,
}

#[derive(Deserialize)]
struct PrinterInfoResponse {
    result: PrinterInfo,
}

#[derive(Deserialize)]
struct PrinterInfo {
    state: String,
    state_message: String,
    #[serde(default)]
    software_version: Option<String>,
    #[serde(default)]
    klipper_path: Option<String>,
}

pub fn parse_host_info(response: &str) -> Result<HostInfo, MoonrakerError> {
    let info: PrinterInfoResponse = serde_json::from_str(response)?;
    let info = info.result;
    Ok(HostInfo {
        state: match info.state.as_str() {
            "ready" => KlippyState::Ready,
            "error" => KlippyState::Error,
            "shutdown" => KlippyState::Shutdown,
            "disconnected" => KlippyState::Disconnected,
            _ => KlippyState::Startup,
        },
        state_message: info.state_message,
        software_version: info.software_version,
        klipper_path: info.klipper_path.map(std::path::PathBuf::from),
    })
}

#[derive(Deserialize)]
struct ObjectListResponse {
    result: ObjectList,
}

#[derive(Deserialize)]
struct ObjectList {
    objects: Vec<String>,
}

fn parse_print_state(response: &str) -> Result<PrintState, MoonrakerError> {
    let response: PrintStatsResponse = serde_json::from_str(response)?;
    let state = response
        .result
        .status
        .print_stats
        .ok_or_else(|| MoonrakerError::InvalidResponse("print_stats was not reported".to_owned()))?
        .state;
    Ok(match state.as_str() {
        "standby" => PrintState::Standby,
        "printing" => PrintState::Printing,
        "paused" => PrintState::Paused,
        "complete" => PrintState::Complete,
        "cancelled" => PrintState::Cancelled,
        "error" => PrintState::Error,
        _ => PrintState::Unknown(state),
    })
}

#[derive(Deserialize)]
struct PrintStatsResponse {
    result: PrintStatsResult,
}

#[derive(Deserialize)]
struct PrintStatsResult {
    status: PrintStatsStatus,
}

#[derive(Deserialize)]
struct PrintStatsStatus {
    #[serde(default)]
    print_stats: Option<PrintStats>,
}

#[derive(Deserialize)]
struct PrintStats {
    state: String,
}

#[derive(Deserialize)]
struct ObjectQueryResponse {
    result: ObjectQueryResult,
}

#[derive(Deserialize)]
struct ObjectQueryResult {
    status: BTreeMap<String, McuStatus>,
}

#[derive(Deserialize)]
struct McuStatus {
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    mcu_version: Option<String>,
    #[serde(default)]
    mcu_constants: BTreeMap<String, Value>,
    #[serde(default)]
    mcu_kconfig: Option<String>,
    #[serde(default)]
    settings: Option<BTreeMap<String, McuSettings>>,
}

#[derive(Clone, Deserialize)]
struct McuSettings {
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    canbus_uuid: Option<String>,
    #[serde(default)]
    canbus_interface: Option<String>,
}
