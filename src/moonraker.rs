use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McuInventory {
    pub mcus: Vec<Mcu>,
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
    #[error("Moonraker returned invalid JSON")]
    Json(#[from] serde_json::Error),
    #[error("Moonraker response is invalid: {0}")]
    InvalidResponse(String),
}

/// A source of Klipper MCU inventory from Moonraker.
///
/// Isolates callers that only need to discover MCUs from the concrete
/// `ureq`-backed [`MoonrakerAdapter`], so they can be tested against a fake.
pub trait MoonrakerPort {
    /// Queries Moonraker for the currently configured MCU objects.
    fn discover_mcus(&self) -> Result<McuInventory, MoonrakerError>;
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
        let object_names = self.mcu_object_names()?;
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
            .send(body)?
            .body_mut()
            .read_to_string()?;

        parse_inventory(&response)
    }

    fn mcu_object_names(&self) -> Result<Vec<String>, MoonrakerError> {
        let response = self
            .agent
            .get(&self.endpoint("/printer/objects/list"))
            .call()?
            .body_mut()
            .read_to_string()?;
        let objects: ObjectListResponse = serde_json::from_str(&response)?;
        let mcu_names = objects
            .result
            .objects
            .into_iter()
            .filter(|name| name == "mcu" || name.starts_with("mcu "))
            .collect::<Vec<_>>();

        if mcu_names.is_empty() {
            return Err(MoonrakerError::InvalidResponse(
                "no MCU objects were reported".to_owned(),
            ));
        }

        Ok(mcu_names)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
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
    let mcus = response
        .result
        .status
        .into_iter()
        .filter(|(name, _)| name != "configfile")
        .map(|(name, status)| {
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
            // Firmware not built by aldis (e.g. Beacon) has no Kconfig to report;
            // `classify_mcu` already treats an empty Kconfig as `Unsupported`
            // rather than failing MCU discovery outright.
            let kconfig = status.mcu_kconfig.unwrap_or_default();
            // Klipper lowercases config section names in configfile.settings,
            // but printer.objects.list preserves the declared case, so an
            // exact-match lookup here misses sections like `[mcu RP2040]`.
            let transport = settings
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(&name))
                .map(|(_, settings)| parse_transport(&name, settings))
                .transpose()?
                .flatten();

            Ok(Mcu {
                name,
                app: status.app,
                version: status.mcu_version,
                mcu,
                canbus_frequency_hz: status
                    .mcu_constants
                    .get("CANBUS_FREQUENCY")
                    .and_then(Value::as_u64),
                transport,
                kconfig,
            })
        })
        .collect::<Result<Vec<_>, MoonrakerError>>()?;

    if mcus.is_empty() {
        return Err(MoonrakerError::InvalidResponse(
            "query returned no MCU objects".to_owned(),
        ));
    }

    Ok(McuInventory { mcus })
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
    let interface = settings.canbus_interface.as_deref().ok_or_else(|| {
        MoonrakerError::InvalidResponse(format!(
            "MCU object {name:?} configures canbus_uuid without canbus_interface"
        ))
    })?;
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

#[derive(Deserialize)]
struct ObjectListResponse {
    result: ObjectList,
}

#[derive(Deserialize)]
struct ObjectList {
    objects: Vec<String>,
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
