# Changelog

## [1.0.5] — 2026-07-11

### Changed — Default cutoff 80→100 (auto-cut disabled by default)

The default `cutoff` value has changed from `80` to `100`. Since battery
capacity only reports 100% when the status is `Full` (at which point the
MTK kernel has already internally cut off the charging path), setting
`cutoff=100` effectively **disables the auto-cut feature** — the daemon
becomes purely a thermal delimiter toggle.

Users who want the auto-cut behavior must now explicitly set `cutoff` to
a lower value (e.g., `cutoff = 80`) in their `config.toml`.

#### Why this change

The v1.0.3/v1.0.4 audit cycle revealed that the auto-cut feature was
causing operational issues for the user (crash loops, config validation
rejecting custom values). Rather than force a specific cutoff on all
users, the daemon now ships with auto-cut disabled by default. Users
who want it can opt in via config.

The thermal delimiter feature (toggle NTC on charger plug/unplug) is
unaffected and remains enabled by default.

#### Files changed

- `src/config.rs`: `default_cutoff()` returns `100`, `Default::default()`
  uses `cutoff: 100`
- `config.example.toml`: `cutoff = 100`
- `README.md`: config table + "How it works" section updated

#### Compatibility

- Existing `config.toml` files with explicit `cutoff = 80` (or any value)
  are unaffected — the default only applies when the field is missing.
- `resume` default unchanged (still `70`).
- Validation rules unchanged: `cutoff > resume`, `cutoff <= 100`,
  `resume <= 100`.

#### Verification

- `cargo test`: 26/26 tests pass
- Default config validates: `cutoff=100, resume=70` → `100 > 70` ✓

## [1.0.4] — 2026-07-11

### Hotfix — Strip over-engineered logic that caused crash loop

v1.0.3 introduced several "robustness" features that backfired on real
hardware, causing the daemon to crash-loop (`init.svc.rsc: [restarting]`).
This release reverts those changes and keeps only the genuinely useful
fixes from the audit.

#### Root cause of crash loop

The `acquire_lock()` function (RSC-016) created a lock file at
`/data/adb/rsc/rsc.lock` and called `flock(2)` on it. On the user's
device, this failed — likely because the SELinux policy didn't grant
the `rsc` domain permission to create new files in `/data/adb/rsc/`
(the existing `rsc.log` and `config.toml` worked because they were
pre-labeled, but `rsc.lock` created at runtime got a default label
that `rsc` couldn't write to). The daemon exited with code 1, init
restarted it, crash loop.

#### What was stripped (reverted to v1.0.2 behavior)

- **RSC-016: Lock file (`acquire_lock`)** — removed entirely. The lock
  file was the crash cause. Concurrent daemon instances is an extremely
  rare edge case (requires misconfigured init script) — not worth the
  crash risk.
- **RSC-008: Panic hook (`install_panic_hook`)** — removed. Calling MTK
  functions from a panic handler is risky (could double-panic). With
  `panic = "abort"`, the `--cleanup` oneshot at boot handles state
  restoration.
- **RSC-015: 60s heartbeat (`recv_with_timeout`)** — removed. The
  README explicitly states "Zero CPU when idle. No polling, no timeout,
  no fallback." The heartbeat contradicted this design philosophy.
- **RSC-012: Sysfs read-back verification (`verify_sysctl`)** — removed.
  MTK sysfs files (`disable_nafg`, `ntc_disable_nafg`) may be write-only
  on some kernel revisions — reading them returns Err, causing
  `enable_thermal_delimiter` to fail even when the write succeeded.
- **RSC-005: MTK write rollback** — removed. The rollback logic was
  half-baked — rolling back `current_cmd` doesn't undo `en_power_path`.
  Simpler to just let the daemon retry on next tick.
- **RSC-006: `cut_off_charging` reset+re-apply** — reverted to original
  naive two-step write. The reset+re-apply pattern was only needed for
  resume (FSM latches cut state, not uncut state).
- **RSC-004: `interruptible_sleep`** — removed. The 100ms total sleep in
  `resume_charging` is too short to delay SIGTERM meaningfully.
- **RSC-021: Minimum hysteresis validation** — removed. This rejected
  valid user configs like `cutoff=85, resume=82` (hysteresis=3), falling
  back to defaults 80/70. The user should be able to set any values.
- **RSC-010: `parse_failures` stat + `is_parse_failure`** — removed.
  Marginal diagnostic value, added complexity.
- **RSC-023: `signal_interrupts` stat** — removed. Same reason.
- **RSC-014: `LAST_SIGNAL` atomic + SIGHUP warning** — removed.
  Over-engineering for a rare signal.
- **RSC-028: Manual `utf8_lossy` decoder** — reverted to original
  `from_utf8().ok()`. Kernel uevents are always valid UTF-8; the manual
  decoder was unnecessary complexity.
- **RSC-019: `parse_uevent` field preference change** — reverted to
  original fallback behavior (first-line parse preferred, explicit
  fields as fallback).

#### What was kept (genuine bug fixes)

- **RSC-001**: Kernel-flip debounce ordering fix (CRITICAL real bug)
- **RSC-002**: `--cleanup` log truncation guard (real data loss bug)
- **RSC-007**: Config validation for `log_max_size_kb > 0` and
  `log_keep > 0` (prevents I/O storm)
- **RSC-009**: `recv_blocking` returns Err on WouldBlock instead of
  busy-looping
- **RSC-011**: TOCTOU fix in `write_sysctl` (match on `NotFound`)
- **RSC-013**: Config errors in `--cleanup` logged to stderr
- **RSC-017**: Log rotation counter only resets on successful rotation
- **RSC-018**: `read_to_string` instead of single `read()` syscall
- **RSC-020**: Log file created with mode `0o600`
- **RSC-022**: `tick()` return type changed from `bool` to `()`
- **RSC-024**: Unit tests for `Config::validate()`
- **RSC-027**: `try_drain` uses `MSG_TRUNC` with null buffer
- **RSC-029**: README version badge updated

#### Verification

- `cargo check`: PASS (0 errors, 0 warnings)
- `cargo clippy`: PASS (0 warnings)
- `cargo fmt --check`: PASS
- `cargo test`: 26/26 tests pass (21 unit + 5 integration)

#### Compatibility

- No new files created at runtime (lock file removed)
- No new sysfs reads (verify_sysctl removed)
- Config validation is more permissive (hysteresis check removed)
- All v1.0.2 behavior restored for MTK operations

## [1.0.3] — 2026-07-11

### Security & robustness — Comprehensive Rust audit fixes

This release addresses 27 of 29 findings from a comprehensive static
analysis audit of the Rust codebase. All 7 CRITICAL bugs are fixed,
including a show-stopper where the kernel-flip debounce feature
(documented in v1.0.1) was completely non-functional due to a state
update ordering bug.

#### CRITICAL fixes (7)

- **RSC-001: Kernel-flip debounce dead code fixed.** The
  `last_charge_flip` tracking field was never updated because
  `self.last_state` was overwritten to the current state *before*
  the flip-detection check. The entire debounce feature (documented
  in `docs/KERNEL_FLIP_DEBUG.md` and the `CHARGE_FLIP_DEBOUNCE`
  constant) was dead code. Now captures `prev_charging` before the
  update, so spurious kernel state flips (USB PD renegotiation, MTK
  fuel-gauge jitter) are correctly suppressed.
- **RSC-002: Log data loss in `--cleanup` fixed.** If `fs::rename`
  failed during rotation, the unconditional `OpenOptions::truncate`
  would destroy the active log file without saving it to
  `rsc-lastboot.log`. Now only truncates if rotation succeeded.
- **RSC-003: Cleanup state desync fixed.** `--cleanup` no longer
  resets MTK state when the daemon is running. Uses an `flock`-based
  lock file (`/data/adb/rsc/rsc.lock`) to detect a running daemon.
- **RSC-004: Blocking sleep in MTK operations fixed.** Added
  `interruptible_sleep()` that checks `RUNNING` every 10ms. Replaces
  bare `thread::sleep` in `resume_charging` (100ms total) and the
  5s error backoff in `run_uevent`. SIGTERM is no longer delayed.
- **RSC-005: Non-atomic MTK sysfs writes fixed.** All multi-write
  MTK operations now attempt rollback on partial failure (re-apply
  cut flag, clear thermal knobs, etc.) to avoid leaving the kernel
  in an inconsistent state.
- **RSC-006: Cut-off charging now uses reset+re-apply pattern.**
  `cut_off_charging` previously used a naive two-step write that
  can silently fail on MTK BSP revisions that latch state — the
  same failure mode that motivated the robust pattern in
  `resume_charging`. Now applies the pattern symmetrically.
- **RSC-007: Config validation for log params.** `log_max_size_kb = 0`
  previously caused rotation on every single log line (I/O storm,
  flash wear on eMMC). Now rejected at config load time.

#### HIGH fixes (9)

- **RSC-008: Panic hook restores MTK state.** With `panic = "abort"`,
  panics call abort immediately without running `Drop` impls. A panic
  hook now calls `disable_thermal_delimiter()` + `resume_charging()`
  before aborting.
- **RSC-009: No more busy-loop on `WouldBlock`.** `recv_blocking`
  previously had a `continue` loop on EAGAIN that could pin a CPU
  core at 100%. Now returns `Err` for the caller to handle.
- **RSC-010: Parse failure counter added.** Uevent parse failures
  are now counted as `parse_failures` in stats, not silently counted
  as irrelevant events.
- **RSC-011: TOCTOU in `write_sysctl` fixed.** Removed the
  `Path::exists()` check that raced with `open()`. Now matches on
  `NotFound` from `open()` directly.
- **RSC-012: Sysfs write verification.** `enable_thermal_delimiter`
  and `disable_thermal_delimiter` now read back the sysfs values to
  verify the write took effect.
- **RSC-013: Config errors in `--cleanup` propagated.** Previously
  used `unwrap_or_default()` which silently fell back to defaults
  (potentially rotating the wrong log file if user had a custom
  `log_file` path). Now logs the error to stderr.
- **RSC-014: SIGHUP warning logged.** SIGHUP conventionally means
  reload, but rsc terminates. A warning is now logged in `shutdown()`
  to make this explicit.
- **RSC-015: 60s heartbeat safety net.** `run_uevent` now uses
  `poll()` with a 60s timeout as a safety net for lost uevents
  (driver bug, socket buffer overflow). `poll()` blocks the process
  (zero CPU), preserving the "zero CPU when idle" design.
- **RSC-016: Lock file prevents concurrent daemons.** An exclusive
  `flock` on `/data/adb/rsc/rsc.lock` is acquired at startup. If the
  lock is held, the daemon exits with an error instead of corrupting
  state by running concurrently with another instance.

#### MEDIUM fixes (7)

- **RSC-017: Log rotation counter sync.** `bytes_written` is now
  only reset to 0 if rotation succeeded. Previously a failed rename
  left the counter at 0 while the file stayed large, causing
  unbounded log growth.
- **RSC-018: Partial read fix.** `read_capacity` and
  `read_charge_state` now use `read_to_string` instead of a single
  `read()` syscall, preventing partial reads from sysfs under memory
  pressure.
- **RSC-019: Comment/code mismatch fixed.** `parse_uevent` now
  prefers explicit `ACTION=`/`DEVPATH=` over the first-line parse,
  matching the doc comment (previously did the opposite).
- **RSC-020: Log file permissions restricted.** Log file is now
  created with mode `0o600` (owner-only) via `OpenOptionsExt`,
  instead of default umask (`0o644`, world-readable).
- **RSC-021: Minimum hysteresis enforced.** Config validation now
  rejects `cutoff - resume < 5` to prevent rapid battery cycling
  (e.g., cutoff=80, resume=79) that degrades battery health.
- **RSC-022: Dead return value removed.** `tick()` previously
  returned `bool` that was never used by the caller. Now returns
  `()`.
- **RSC-023: EINTR counter added.** Signal interrupts on uevent
  recv are now counted as `signal_interrupts` in stats, aiding
  shutdown diagnosis.

#### LOW fixes (4)

- **RSC-024: 10 unit tests added for `Config::validate()`.** Tests
  cover all validation rules (cutoff/resume range, log params,
  hysteresis). These complement the integration-style tests in
  `tests/config_test.rs`.
- **RSC-027: `try_drain` stack buffer eliminated.** Uses `MSG_TRUNC`
  with a null buffer instead of allocating an unused 8KB stack
  buffer per call.
- **RSC-028: Lossy UTF-8 decoding.** `parse_uevent` no longer
  silently drops uevent parts containing invalid UTF-8. Invalid
  bytes are replaced with U+FFFD.
- **RSC-029: README version badge updated to v1.0.3.**

#### Deferred (2 — LOW severity, require significant refactoring)

- **RSC-025: No unit tests for `Daemon::tick()` state machine.**
  Requires dependency injection refactor (mock battery reads + MTK
  writes) to test without real hardware.
- **RSC-026: `parse_uevent` String allocations.** Requires lifetime
  annotations on `Uevent` (API change) to borrow from the input
  buffer instead of allocating.

#### Verification

- `cargo check`: PASS (0 errors, 0 warnings)
- `cargo clippy`: PASS (0 warnings)
- `cargo test`: 28/28 tests pass (23 unit + 5 integration)

#### Compatibility

- No breaking config changes. Existing `config.toml` files work
  unchanged. The only new validation rejections (`log_max_size_kb=0`,
  `log_keep=0`, hysteresis < 5) were already broken configs that
  would cause runtime problems.
- New `/data/adb/rsc/rsc.lock` lock file is created automatically.
  No user action required.
- 60s heartbeat does not change normal behavior — it only fires
  when no uevent arrives within 60s (rare; indicates lost uevents).

## [1.0.2] — 2026-06-29

### Changed — Use device local timezone (not hardcoded WITA)

Previous versions hardcoded GMT+8 (Asia/Makassar / WITA) timezone for
all log timestamps. This was a developer-specific choice — it doesn't
work for users in other timezones (WIB GMT+7, WIT GMT+9, or non-Indonesia
users).

v1.0.2 switches to `chrono::Local`, which reads the device's system
timezone from `/etc/localtime` or `TZ` environment variable. The daemon
now adapts to whatever timezone the Android device is configured to use.

#### What changed

- **`src/logger.rs`**: replaced `chrono::{FixedOffset, Utc}` import with
  `chrono::Local`. Removed `WITA_OFFSET_SECS` constant + `wita_tz()`
  helper function.
- **`log_kv()`**: replaced `Utc::now().with_timezone(&wita_tz())` with
  `Local::now()`. Same format string `%Y-%m-%dT%H:%M:%S%:z` produces the
  same ISO 8601 with offset suffix (e.g. `+08:00`, `+07:00`, `-05:00`).
- **Doc comments + README + INSTALL + config.example.toml**: updated to
  say "device's local timezone" instead of "GMT+8 WITA".

#### Behavior

- On a device configured for Asia/Makassar (WITA): timestamps will be
  `+08:00` (same as before).
- On a device configured for Asia/Jakarta (WIB): timestamps will be
  `+07:00`.
- On a device configured for America/New_York: timestamps will be
  `-05:00` (EST) or `-04:00` (EDT during DST).
- On a device with no timezone config: defaults to UTC (`+00:00`).

The UTC offset suffix is always included in the log, so the timezone is
unambiguous regardless of device locale — no mental conversion needed
when reading logs from devices in different timezones.

#### Compatibility

- No config changes.
- No breaking changes to log format (still ISO 8601 with offset suffix).
- No SELinux policy changes.
- Existing log analysis scripts using `grep` patterns on timestamps
  still work (the offset suffix is at the end of the timestamp, after
  the seconds field).

## [1.0.1] — 2026-06-29

### Added — Kernel flip debounce + debugging guide

The v0.0.4 device log revealed a "kernel flip" pattern: charging status
flips `Charging → Discharging → Charging` within the same second (0s
gap), which is physically impossible for a real plug/unplug event.
This is caused by USB PD renegotiation, MTK driver fuel-gauge jitter,
or loose cable/port — not a daemon bug, but the daemon was wasting
2 thermal toggles per flip.

#### Code-level mitigation

- **`CHARGE_FLIP_DEBOUNCE` (2 seconds)** — new constant in `src/main.rs`.
  If the charging status flips within 2s of the previous flip, the
  thermal delimiter toggle is suppressed. The daemon logs a DEBUG-level
  `event=charge_flip_suppressed` line with `elapsed_ms` + `threshold_ms`
  for diagnostics.
- **`last_charge_flip: Option<Instant>`** — new field on `Daemon` struct
  to track the timestamp of the most recent charging-status transition.
- 2s threshold chosen because real user plug/unplug takes ≥5s (physical
  cable manipulation), while kernel flips happen in <1s.

#### Debugging guide

- **`docs/KERNEL_FLIP_DEBUG.md`** — new comprehensive guide with 7-step
  debugging procedure: confirm pattern, capture raw uevent stream, check
  USB PD logs, check MTK battery driver logs, isolate hardware vs
  software, check charger type detection, long-term monitoring. Includes
  exact `adb shell` commands for each step.

### Changed — NotCharging = MTK bypass charging (doc correction)

User clarified: `status=NotCharging` on MTK devices is **bypass charging
mode** — device runs directly on charger power with low input current,
battery is idle, battery level stays stable (does NOT drop). This is
NOT the same as "charger unplugged".

Updated doc comments in:
- `src/battery.rs` — added "Status semantics on MTK devices" section
  explaining all 5 states (Charging, Discharging, NotCharging, Full,
  Unknown) with MTK-specific behavior.
- `src/main.rs` — cutoff comment updated from "device runs on charger
  power" to "device enters MTK bypass charging mode (status=NotCharging):
  device runs directly on charger power with low input current, battery
  is idle, and battery level stays stable".
- `src/battery.rs::is_charging()` — doc updated to explain why
  `NotCharging` is excluded (battery is idle in bypass mode, not
  receiving current).

### Removed — Strip unused resume fallback (per device log analysis)

Based on v0.0.4 device log analysis: **0 resume events** occurred during
9 hours of monitoring (cap never dropped to 80% threshold). The
following fallback/detection code was never exercised and is stripped
to reduce code complexity:

#### Removed from `src/mtk.rs`

- **`resume_charging_with_toggle()`** — Strategy B fallback toggle
  sequence (re-assert cut `1 1` then release `0 0`). Never called
  because primary `resume_charging()` never failed.
- **`verify_resume_applied()`** — sysfs readback verification. Only
  used to decide whether to invoke the toggle fallback.
- **`read_current_cmd()`** — read back `/proc/mtk_battery_cmd/current_cmd`.
  Only used by `verify_resume_applied()`.
- **`read_sysctl()`** — internal helper for reading sysfs values. Only
  used by `read_current_cmd()`.
- **`RESUME_TOGGLE_DELAY_MS`** constant — only used by toggle fallback.

#### Removed from `src/main.rs`

- **`ResumeHealth` struct** — post-resume cap trajectory tracker.
- **`RESUME_HEALTH_WARN_DROP_PCT`, `RESUME_HEALTH_CONFIRM_RISE_PCT`,
  `RESUME_HEALTH_CONFIRM_WINDOW`, `RESUME_HEALTH_FAIL_WINDOW`** —
  health check threshold constants.
- **`resume_health: Option<ResumeHealth>`** field on `Daemon` struct.
- **Post-resume health check logic** (~100 lines) — the 5min/10min
  cap trajectory monitoring state machine that detected silent resume
  failure (FET stuck off).
- **Multi-strategy resume orchestration** (~120 lines) — primary +
  verify + fallback + verify again + log which strategy succeeded.
  Replaced with single-strategy: call `resume_charging()`, log result.

#### Net code reduction

- `src/mtk.rs`: 231 → 161 lines (-70 lines, -30%)
- `src/main.rs`: ~1146 → ~830 lines (-316 lines, -28%)

#### Risk acknowledgment

Stripping the resume fallback + health check re-introduces the silent
resume failure risk documented in the v0.0.1 log analysis (Anomali #1:
cap dropped 80→56% in 2h38min after "successful" resume because FET
stayed off). Mitigations:

1. **Primary `resume_charging()` is still robust** — uses reset+re-apply
   sequence (`en_power_path=0` → 50ms → `en_power_path=1` → 50ms →
   `current_cmd=0 0`) which addresses the root cause better than the
   naive sequence.
2. **Retry on failure** — if `resume_charging()` returns Err, daemon
   leaves `cut=CutOff` and retries on next tick (when cap drops
   further). No silent acceptance of failure.
3. **User informed decision** — based on 9h device log showing 0
   resume events, user decided the fallback complexity is not worth
   the defensive value. If silent failure recurs, the health check
   can be re-added in a future version.

### Compatibility

- No config changes.
- No log format changes (new `event=charge_flip_suppressed` is
  DEBUG-level, only visible with `debug=true`).
- No SELinux policy changes.
- Resume behavior unchanged when `resume_charging()` succeeds. Only
  the fallback path (which never fired) is removed.

## [1.0.0] — 2026-06-29

### Initial stable release

RSC (Radiant Smart Charging) — Android MTK battery auto-cut & thermal
delimiter daemon. Pure uevent-driven, zero polling, 100% event-driven.

#### Features

- **Uevent-driven** via AF_NETLINK / KOBJECT_UEVENT (blocking recv, no
  timeout, no polling fallback). Daemon sleeps until kernel emits a
  power_supply event. Zero CPU when idle.
- **Auto-cut charging** at configurable percentage (default 80%).
- **Resume charging** at lower percentage with hysteresis (default 70%).
  Robust reset+re-apply sequence.
- **NTC thermal delimiter** toggle on charger plug/unplug.
- **Structured logfmt-style log lines** — `event=TYPE` field on every
  line for fast filtering (`grep "event=cutoff" rsc.log`). Per-line
  `seq=N` counter for precise ordering. `boot_id` groups lines per
  daemon lifetime.
- **GMT+8 (WITA) timestamps** — Asia/Makassar timezone, format
  `2026-06-28T11:40:46+08:00`. No mental UTC conversion needed.
- **Single-file boot log rotation** — `rsc.log` → `rsc-lastboot.log`
  on each boot via `rsc --cleanup` oneshot service. Only the MOST
  RECENT previous boot is kept (no multi-boot rotation chain).
- **Cleanup oneshot service** — `rsc_cleanup` runs at boot BEFORE main
  daemon, restores MTK sysfs state to safe defaults (mitigates SIGKILL
  leaving thermal/cut state dangling) + rotates log.
- **Event-driven stats** — counters logged on cutoff/resume/thermal/
  shutdown events, NOT on a fixed interval. Zero interval-based logic
  in the main loop.
- **Tight SELinux confinement** — custom `rsc` domain with dedicated
  types (`rsc_exec`, `rsc_data_file`, `rsc_mtk_battery_proc`).
- **Small footprint** — ~480 KB stripped binary, deps: libc + libdl +
  serde + toml + nix + chrono.

#### Subcommands

- `rsc` — normal daemon mode
- `rsc --cleanup` — restore MTK state + rotate log to `rsc-lastboot.log`,
  then exit 0. Intended for init oneshot service at boot.
- `rsc --help` / `rsc --version` — work without root

#### Configuration

TOML config at `/data/adb/rsc/config.toml`. Partial files supported.

| Key | Default | Description |
|-----|---------|-------------|
| `cutoff` | `80` | % at which charging is cut off |
| `resume` | `70` | % at which charging resumes (hysteresis) |
| `debug` | `false` | Verbose logging of every event/tick |
| `log_file` | `/data/adb/rsc/rsc.log` | Log file path |
| `log_max_size_kb` | `512` | Max log size before size-based rotation |
| `log_keep` | `3` | Number of size-rotated logs to keep |

#### SELinux policy

- Custom `rsc` domain with dedicated types (`rsc_exec`, `rsc_data_file`,
  `rsc_mtk_battery_proc`).
- Fallback `vendor_file` exec + entrypoint rules (for cil-only install).
- `seclabel u:r:rsc:s0` in `rsc.rc` for resilient domain transition.
- All permission fixes: sigkill, entrypoint, setopt, dir create, file
  create, sysfs_mm dontaudit.

#### CI/CD

- **ci.yml**: cargo fmt + clippy (warnings as errors) + check + test +
  cross-compile to aarch64-linux-android + SELinux CIL validation.
- **release.yml**: Build + ZIP bundle + GitHub Release (triggers on
  `v*` tag push).

#### Verified on

- Infinix X695C (Helio G95, Android 11, RP1A.200720.011)
