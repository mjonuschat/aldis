use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
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
    pub kconfig: String,
}

#[derive(Debug)]
pub enum MoonrakerError {
    Http(ureq::Error),
    Json(serde_json::Error),
    InvalidResponse(String),
}

impl fmt::Display for MoonrakerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "Moonraker request failed: {error}"),
            Self::Json(error) => write!(formatter, "Moonraker returned invalid JSON: {error}"),
            Self::InvalidResponse(message) => {
                write!(formatter, "Moonraker response is invalid: {message}")
            }
        }
    }
}

impl StdError for MoonrakerError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidResponse(_) => None,
        }
    }
}

impl From<ureq::Error> for MoonrakerError {
    fn from(error: ureq::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for MoonrakerError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub struct MoonrakerClient {
    base_url: String,
    agent: ureq::Agent,
}

impl MoonrakerClient {
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
        let body = serde_json::to_string(&json!({
            "objects": object_names
                .iter()
                .map(|name| (name, Value::Null))
                .collect::<BTreeMap<_, _>>(),
        }))?;
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
    let mcus = response
        .result
        .status
        .into_iter()
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
            let kconfig = status.mcu_kconfig.ok_or_else(|| {
                MoonrakerError::InvalidResponse(format!(
                    "MCU object {name:?} does not expose mcu_kconfig"
                ))
            })?;

            Ok(Mcu {
                name,
                app: status.app,
                version: status.mcu_version,
                mcu,
                canbus_frequency_hz: status
                    .mcu_constants
                    .get("CANBUS_FREQUENCY")
                    .and_then(Value::as_u64),
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
    app: Option<String>,
    mcu_version: Option<String>,
    mcu_constants: BTreeMap<String, Value>,
    mcu_kconfig: Option<String>,
}
