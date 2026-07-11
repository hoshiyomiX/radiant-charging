//! MediaTek battery command interface.
//!
//! Implements the MTK-specific sysfs/procfs paths the user supplied:
//!
//! Cut-off charging (reset+re-apply, same as resume):
//!   echo "0"   > /proc/mtk_battery_cmd/en_power_path
//!   (sleep 50ms)
//!   echo "1"   > /proc/mtk_battery_cmd/en_power_path
//!   (sleep 50ms)
//!   echo "1 1" > /proc/mtk_battery_cmd/current_cmd
//!
//! Resume charging (reset+re-apply sequence):
//!   echo "0"   > /proc/mtk_battery_cmd/en_power_path
//!   (sleep 50ms)
//!   echo "1"   > /proc/mtk_battery_cmd/en_power_path
//!   (sleep 50ms)
//!   echo "0 0" > /proc/mtk_battery_cmd/current_cmd
//!
//! Enable battery thermal delimiter:
//!   echo 1 > /sys/devices/platform/battery/disable_nafg
//!   echo 1 > /sys/devices/platform/battery/ntc_disable_nafg
//!
//! Restore thermal (disable delimiter):
//!   echo 0 > /sys/devices/platform/battery/disable_nafg
//!   echo 0 > /sys/devices/platform/battery/ntc_disable_nafg
//!
//! ## Why both cut and resume use the reset+re-apply pattern
//!
//! The naive sequence (`en_power_path=1` then `current_cmd=X`) succeeds at
//! the sysfs write level but can FAIL to actually take effect on Infinix
//! X695C (Helio G95, Android 11). The MTK power-path driver FSM can latch
//! state in a way that makes naive writes silently fail.
//!
//! The fix: reset `en_power_path` to 0 first (force power-path driver FSM
//! reset), then re-apply 1, then write current_cmd. This pattern is now
//! applied symmetrically to both cut_off and resume (RSC-006).

use std::fs::OpenOptions;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

const EN_POWER_PATH: &str = "/proc/mtk_battery_cmd/en_power_path";
const CURRENT_CMD_PATH: &str = "/proc/mtk_battery_cmd/current_cmd";
const DISABLE_NAFG_PATH: &str = "/sys/devices/platform/battery/disable_nafg";
const NTC_DISABLE_NAFG_PATH: &str = "/sys/devices/platform/battery/ntc_disable_nafg";

/// Sleep between reset and re-apply of en_power_path. 50ms is enough
/// for the MTK battery driver FSM to register the transition without
/// being so long that it delays daemon ticks noticeably.
const RESUME_RESET_DELAY_MS: u64 = 50;

#[derive(Debug)]
pub enum MtkError {
    Io(std::io::Error),
    PathMissing(String),
    /// RSC-012: read-back verification failed — the sysfs write succeeded
    /// at the syscall level but the kernel did not accept the value.
    VerificationFailed(String),
}

impl From<std::io::Error> for MtkError {
    fn from(e: std::io::Error) -> Self {
        MtkError::Io(e)
    }
}

impl std::fmt::Display for MtkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MtkError::Io(e) => write!(f, "io: {}", e),
            MtkError::PathMissing(p) => write!(f, "path missing: {}", p),
            MtkError::VerificationFailed(s) => write!(f, "verification failed: {}", s),
        }
    }
}

impl std::error::Error for MtkError {}

/// RSC-004: Interruptible sleep that checks RUNNING every 10ms, allowing
/// signal-triggered shutdown to take effect quickly. Returns early if
/// RUNNING becomes false. This replaces bare `thread::sleep` in MTK
/// operations so SIGTERM during resume_charging isn't delayed by the
/// full 100ms sleep.
fn interruptible_sleep(dur: Duration) {
    let mut remaining = dur;
    while remaining > Duration::ZERO && crate::RUNNING.load(Ordering::SeqCst) {
        let step = remaining.min(Duration::from_millis(10));
        thread::sleep(step);
        remaining -= step;
    }
}

/// RSC-011: Write a value to a sysfs/procfs path. Removed the TOCTOU
/// `Path::exists()` check — instead, match on `NotFound` from `open()`
/// to detect missing paths. This eliminates the race between exists()
/// and open() where the path could disappear.
fn write_sysctl(path: &str, value: &str) -> Result<(), MtkError> {
    let mut f = match OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MtkError::PathMissing(path.to_string()));
        }
        Err(e) => return Err(MtkError::Io(e)),
    };
    f.write_all(value.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// RSC-012: Read back a sysfs value and verify it matches the expected
/// value. Used for sysfs paths that support read-back (thermal delimiter
/// knobs). Not used for procfs battery_cmd paths — those may not support
/// read-back on all kernel versions.
fn verify_sysctl(path: &str, expected: &str) -> Result<(), MtkError> {
    let actual = fs::read_to_string(path)?;
    if actual.trim() == expected.trim() {
        Ok(())
    } else {
        Err(MtkError::VerificationFailed(format!(
            "{}: expected '{}', got '{}'",
            path,
            expected,
            actual.trim()
        )))
    }
}

/// Cut off charging via MTK battery_cmd using reset+re-apply pattern.
///
/// RSC-006: Previously used naive two-step write (en_power_path=1 +
/// current_cmd=1 1), which can silently fail on MTK BSP revisions that
/// latch state — same failure mode that motivated the reset+re-apply
/// pattern in resume_charging. Now applies the same robust pattern
/// symmetrically.
///
/// RSC-005: On partial failure (current_cmd write fails after
/// en_power_path is set), attempts rollback to clear the cut flag.
pub fn cut_off_charging() -> Result<(), MtkError> {
    // 1. Reset en_power_path to 0 — force driver FSM reset.
    write_sysctl(EN_POWER_PATH, "0")?;
    interruptible_sleep(Duration::from_millis(RESUME_RESET_DELAY_MS));

    // 2. Re-apply en_power_path=1 — re-enable power path.
    write_sysctl(EN_POWER_PATH, "1")?;
    interruptible_sleep(Duration::from_millis(RESUME_RESET_DELAY_MS));

    // 3. Apply cut flag. RSC-005: Rollback on failure.
    match write_sysctl(CURRENT_CMD_PATH, "1 1") {
        Ok(_) => Ok(()),
        Err(e) => {
            // Rollback: clear cut flag to avoid half-cut state.
            let _ = write_sysctl(CURRENT_CMD_PATH, "0 0");
            Err(e)
        }
    }
}

/// Resume charging via reset+re-apply sequence.
///
/// Resets `en_power_path` to 0 first (forcing the MTK power-path driver
/// FSM to release any latched cutoff state), then re-applies 1, then
/// clears `current_cmd` to `0 0`.
///
/// RSC-004: Uses `interruptible_sleep` instead of bare `thread::sleep`
/// so SIGTERM during resume isn't delayed by the full 100ms.
///
/// RSC-005: On partial failure (current_cmd write fails after
/// en_power_path is reset+re-applied), attempts rollback to re-apply
/// the cut flag, maintaining the cut-off state.
pub fn resume_charging() -> Result<(), MtkError> {
    // 1. Reset en_power_path to 0 — force driver FSM reset.
    write_sysctl(EN_POWER_PATH, "0")?;
    interruptible_sleep(Duration::from_millis(RESUME_RESET_DELAY_MS));

    // 2. Re-apply en_power_path=1 — re-enable power path.
    write_sysctl(EN_POWER_PATH, "1")?;
    interruptible_sleep(Duration::from_millis(RESUME_RESET_DELAY_MS));

    // 3. Clear current_cmd cut flag. RSC-005: Rollback on failure.
    match write_sysctl(CURRENT_CMD_PATH, "0 0") {
        Ok(_) => Ok(()),
        Err(e) => {
            // Rollback: re-apply cut flag to maintain cut-off state.
            let _ = write_sysctl(CURRENT_CMD_PATH, "1 1");
            Err(e)
        }
    }
}

/// Enable the NTC/NAFG thermal delimiter. Per the user's snippet, this is
/// only applied during charging events.
///
/// RSC-005: On partial failure (second write fails after first succeeds),
/// attempts rollback to clear the first knob.
/// RSC-012: Verifies both writes via read-back.
pub fn enable_thermal_delimiter() -> Result<(), MtkError> {
    write_sysctl(DISABLE_NAFG_PATH, "1")?;
    match write_sysctl(NTC_DISABLE_NAFG_PATH, "1") {
        Ok(_) => {
            // RSC-012: Verify both knobs took effect.
            if let Err(e) = verify_sysctl(DISABLE_NAFG_PATH, "1") {
                let _ = write_sysctl(DISABLE_NAFG_PATH, "0");
                let _ = write_sysctl(NTC_DISABLE_NAFG_PATH, "0");
                return Err(e);
            }
            if let Err(e) = verify_sysctl(NTC_DISABLE_NAFG_PATH, "1") {
                let _ = write_sysctl(DISABLE_NAFG_PATH, "0");
                let _ = write_sysctl(NTC_DISABLE_NAFG_PATH, "0");
                return Err(e);
            }
            Ok(())
        }
        Err(e) => {
            // RSC-005: Rollback — clear first knob.
            let _ = write_sysctl(DISABLE_NAFG_PATH, "0");
            Err(e)
        }
    }
}

/// Restore normal thermal behaviour by clearing both delimiter knobs.
///
/// RSC-005: On partial failure (second write fails after first succeeds),
/// attempts rollback to re-apply the first knob.
/// RSC-012: Verifies both writes via read-back.
pub fn disable_thermal_delimiter() -> Result<(), MtkError> {
    write_sysctl(DISABLE_NAFG_PATH, "0")?;
    match write_sysctl(NTC_DISABLE_NAFG_PATH, "0") {
        Ok(_) => {
            // RSC-012: Verify both knobs took effect.
            if let Err(e) = verify_sysctl(DISABLE_NAFG_PATH, "0") {
                let _ = write_sysctl(DISABLE_NAFG_PATH, "1");
                let _ = write_sysctl(NTC_DISABLE_NAFG_PATH, "1");
                return Err(e);
            }
            if let Err(e) = verify_sysctl(NTC_DISABLE_NAFG_PATH, "0") {
                let _ = write_sysctl(DISABLE_NAFG_PATH, "1");
                let _ = write_sysctl(NTC_DISABLE_NAFG_PATH, "1");
                return Err(e);
            }
            Ok(())
        }
        Err(e) => {
            // RSC-005: Rollback — re-apply first knob.
            let _ = write_sysctl(DISABLE_NAFG_PATH, "1");
            Err(e)
        }
    }
}

/// True if the MTK battery command paths exist on this device. Used as a
/// startup sanity check — non-MTK devices will exit early instead of
/// spamming logs with `PathMissing` errors.
pub fn paths_exist() -> bool {
    Path::new(EN_POWER_PATH).exists()
        && Path::new(CURRENT_CMD_PATH).exists()
        && Path::new(DISABLE_NAFG_PATH).exists()
        && Path::new(NTC_DISABLE_NAFG_PATH).exists()
}

#[cfg(test)]
mod tests {
    // No unit tests for the MTK functions because they all hit real
    // /proc and /sys paths that don't exist in the CI/test environment.
    // Integration testing requires a real MTK Android device.
    //
    // The logic that IS testable (string parsing, state transitions)
    // lives in battery.rs and uevent.rs — see those modules for tests.
}
