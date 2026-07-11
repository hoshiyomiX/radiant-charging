//! Battery sysfs reader.
//!
//! Reads from the standard Android power_supply sysfs nodes:
//!   - capacity: `/sys/class/power_supply/battery/capacity`
//!   - status:   `/sys/class/power_supply/battery/status`
//!
//! The `status` field is one of: Charging, Discharging, Full, Not charging,
//! or Unknown. We map it to a small enum for clarity.
//!
//! ## Status semantics on MTK devices
//!
//! - `Charging` — battery is actively receiving current.
//! - `Discharging` — battery is providing current to the device (charger
//!   unplugged, or charger plugged but charging path cut off).
//! - `Not charging` — MTK bypass charging mode: device runs directly on
//!   charger power with low input current, battery is idle. Battery
//!   level stays stable (does NOT drop). This is the state after rsc
//!   applies cutoff — the device keeps running on charger power while
//!   the battery is disconnected from the charging path.
//! - `Full` — battery is at 100% and charger is plugged. MTK has
//!   already cut off the charging path internally.
//! - `Unknown` — driver could not determine state (rare, usually
//!   indicates a fuel-gauge communication error).

use std::fs;

const CAPACITY_PATH: &str = "/sys/class/power_supply/battery/capacity";
const STATUS_PATH: &str = "/sys/class/power_supply/battery/status";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

impl ChargeState {
    /// True when an external power source is actively pushing current into
    /// the battery. `Full` is excluded because once the battery is full,
    /// MTK has already cut off the path and re-applying the cut command is
    /// a no-op but a delimiter toggle would be wasteful. `NotCharging` is
    /// excluded because in MTK bypass charging mode the battery is idle
    /// (not receiving current), even though the charger is plugged in.
    pub fn is_charging(&self) -> bool {
        matches!(self, ChargeState::Charging)
    }
}

impl std::fmt::Display for ChargeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChargeState::Charging => write!(f, "Charging"),
            ChargeState::Discharging => write!(f, "Discharging"),
            ChargeState::Full => write!(f, "Full"),
            ChargeState::NotCharging => write!(f, "NotCharging"),
            ChargeState::Unknown => write!(f, "Unknown"),
        }
    }
}

pub fn read_capacity() -> Result<u8, std::io::Error> {
    // RSC-018: Use read_to_string instead of a single read() syscall.
    // A single read() on sysfs may return partial data under memory
    // pressure or kernel conditions. read_to_string loops until EOF,
    // guaranteeing the full content is read. The allocation overhead
    // (1-3 bytes + newline) is negligible for a daemon that ticks
    // ~10 times per second.
    let s = fs::read_to_string(CAPACITY_PATH)?;
    s.trim()
        .parse::<u8>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn read_charge_state() -> Result<ChargeState, std::io::Error> {
    // RSC-018: Use read_to_string for the same reason as read_capacity —
    // a single read() may return partial data from sysfs.
    let s = fs::read_to_string(STATUS_PATH)?;
    let state = match s.trim() {
        "Charging" => ChargeState::Charging,
        "Discharging" => ChargeState::Discharging,
        "Full" => ChargeState::Full,
        "Not charging" => ChargeState::NotCharging,
        _ => ChargeState::Unknown,
    };
    Ok(state)
}
