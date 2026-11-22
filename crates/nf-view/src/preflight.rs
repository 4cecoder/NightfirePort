//! System-agnostic, context-aware pre-flight headroom and resource governor.
//!
//! Probes host memory, CPU parallelism, and thermal pressure once at startup to
//! choose a [`ResourceBudget`] that keeps frame pacing smooth without exhausting
//! RAM or triggering thermal throttling.

use std::fmt;
use std::sync::OnceLock;

/// Coarse performance and memory budget tier selected for the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceTier {
    Low,
    Balanced,
    High,
}

impl ResourceTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::High => "high",
        }
    }
}

impl fmt::Display for ResourceTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Host thermal state reported by the OS (or [`ThermalPressure::Nominal`] when
/// unavailable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalPressure {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl ThermalPressure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Fair => "fair",
            Self::Serious => "serious",
            Self::Critical => "critical",
        }
    }
}

impl fmt::Display for ThermalPressure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshot of host headroom gathered during pre-flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostProfile {
    pub total_ram_mb: u64,
    pub free_ram_mb: u64,
    pub cpu_cores: usize,
    pub thermal_pressure: ThermalPressure,
}

/// Concrete subsystem caps derived from [`HostProfile`] and any `NF_TIER`
/// override.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceBudget {
    pub tier: ResourceTier,
    pub movie_decode_threads: usize,
    pub movie_stream_ahead: usize,
    pub anisotropy_clamp: u16,
    pub foreign_level_cache_cap: usize,
    pub max_render_scale: f32,
}

/// Returns the canonical [`ResourceBudget`] for a given [`ResourceTier`].
pub const fn budget_for_tier(tier: ResourceTier) -> ResourceBudget {
    match tier {
        ResourceTier::Low => ResourceBudget {
            tier: ResourceTier::Low,
            movie_decode_threads: 1,
            movie_stream_ahead: 12,
            anisotropy_clamp: 4,
            foreign_level_cache_cap: 0,
            max_render_scale: 1.0,
        },
        ResourceTier::Balanced => ResourceBudget {
            tier: ResourceTier::Balanced,
            movie_decode_threads: 2,
            movie_stream_ahead: 20,
            anisotropy_clamp: 8,
            foreign_level_cache_cap: 1,
            max_render_scale: 1.5,
        },
        ResourceTier::High => ResourceBudget {
            tier: ResourceTier::High,
            movie_decode_threads: 3,
            movie_stream_ahead: 30,
            anisotropy_clamp: 16,
            foreign_level_cache_cap: 2,
            max_render_scale: 2.0,
        },
    }
}

/// Probes the current host for RAM, CPU core count, and thermal pressure.
///
/// Never panics: if an OS command or procfs file is unavailable, conservative
/// fallback values are returned.
pub fn probe_host() -> HostProfile {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    #[cfg(target_os = "macos")]
    {
        probe_macos(cpu_cores)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        probe_linux_like(cpu_cores)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        HostProfile {
            total_ram_mb: 16384,
            free_ram_mb: 8192,
            cpu_cores,
            thermal_pressure: ThermalPressure::Nominal,
        }
    }
}

/// Selects a [`ResourceBudget`] from a [`HostProfile`] and an optional
/// environment override (`"low"`, `"balanced"`, or `"high"`).
pub fn select_budget(profile: &HostProfile, env_override: Option<&str>) -> ResourceBudget {
    select_budget_for_platform(profile, env_override, cfg!(target_os = "android"))
}

fn select_budget_for_platform(
    profile: &HostProfile,
    env_override: Option<&str>,
    is_android: bool,
) -> ResourceBudget {
    if let Some(raw) = env_override {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => return budget_for_tier(ResourceTier::Low),
            "balanced" => return budget_for_tier(ResourceTier::Balanced),
            "high" => return budget_for_tier(ResourceTier::High),
            _ => {}
        }
    }

    if is_android
        || profile.free_ram_mb < 4096
        || profile.total_ram_mb <= 8192
        || matches!(
            profile.thermal_pressure,
            ThermalPressure::Serious | ThermalPressure::Critical
        )
    {
        return budget_for_tier(ResourceTier::Low);
    }

    if profile.total_ram_mb >= 24576
        && profile.free_ram_mb >= 12288
        && profile.thermal_pressure == ThermalPressure::Nominal
    {
        return budget_for_tier(ResourceTier::High);
    }

    budget_for_tier(ResourceTier::Balanced)
}

/// Returns the process-wide [`ResourceBudget`], initializing it once from
/// [`probe_host`] and the `NF_TIER` environment variable.
pub fn current_budget() -> &'static ResourceBudget {
    static BUDGET: OnceLock<ResourceBudget> = OnceLock::new();
    BUDGET.get_or_init(|| {
        let profile = probe_host();
        let env_override = std::env::var("NF_TIER").ok();
        let budget = select_budget(&profile, env_override.as_deref());
        let override_label = env_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("none");
        eprintln!(
            "[preflight] tier={} (ram={}/{}MB cores={} thermal={} override={}) movie_threads={} stream_ahead={} aniso={} foreign_cache={} scale={:.1}",
            budget.tier,
            profile.free_ram_mb,
            profile.total_ram_mb,
            profile.cpu_cores,
            profile.thermal_pressure,
            override_label,
            budget.movie_decode_threads,
            budget.movie_stream_ahead,
            budget.anisotropy_clamp,
            budget.foreign_level_cache_cap,
            budget.max_render_scale,
        );
        budget
    })
}

#[cfg(target_os = "macos")]
fn probe_macos(cpu_cores: usize) -> HostProfile {
    use std::process::Command;

    let total_ram_mb = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
        .unwrap_or(16384);

    let free_ram_mb = Command::new("vm_stat")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| parse_vm_stat_free_mb(&s))
        .map(|free| free.min(total_ram_mb))
        .unwrap_or(total_ram_mb / 2);

    let thermal_pressure = Command::new("pmset")
        .args(["-g", "therm"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_pmset_therm(&s))
        .unwrap_or(ThermalPressure::Nominal);

    HostProfile {
        total_ram_mb,
        free_ram_mb,
        cpu_cores,
        thermal_pressure,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn probe_linux_like(cpu_cores: usize) -> HostProfile {
    let (total_ram_mb, free_ram_mb) = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_proc_meminfo(&s))
        .unwrap_or((8192, 4096));

    HostProfile {
        total_ram_mb,
        free_ram_mb,
        cpu_cores,
        thermal_pressure: ThermalPressure::Nominal,
    }
}

/// Parses macOS `vm_stat` output and returns available RAM in MiB
/// (`Pages free + Pages inactive + Pages speculative`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_vm_stat_free_mb(output: &str) -> Option<u64> {
    let mut page_size: u64 = 4096;
    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    let mut speculative_pages: u64 = 0;
    let mut saw_any_pages = false;

    for line in output.lines() {
        if let Some(idx) = line.find("page size of ") {
            let rest = &line[idx + "page size of ".len()..];
            if let Some(num_str) = rest.split_whitespace().next() {
                if let Ok(parsed) = num_str.parse::<u64>() {
                    if parsed > 0 {
                        page_size = parsed;
                    }
                }
            }
            continue;
        }

        if let Some((key, val)) = line.split_once(':') {
            let pages = val.trim().trim_end_matches('.').parse::<u64>().ok();
            if let Some(p) = pages {
                match key.trim() {
                    "Pages free" => {
                        free_pages = p;
                        saw_any_pages = true;
                    }
                    "Pages inactive" => {
                        inactive_pages = p;
                        saw_any_pages = true;
                    }
                    "Pages speculative" => {
                        speculative_pages = p;
                        saw_any_pages = true;
                    }
                    _ => {}
                }
            }
        }
    }

    if !saw_any_pages {
        return None;
    }

    let total_pages = free_pages
        .saturating_add(inactive_pages)
        .saturating_add(speculative_pages);
    Some(total_pages.saturating_mul(page_size) / (1024 * 1024))
}

/// Parses macOS `pmset -g therm` output into a [`ThermalPressure`] level.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_pmset_therm(output: &str) -> ThermalPressure {
    let mut pressure = ThermalPressure::Nominal;

    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("no thermal warning")
            || lower.contains("no performance warning")
            || lower.contains("no cpu power status")
        {
            continue;
        }

        if let Some((_, rhs)) = lower.split_once('=') {
            if lower.contains("cpu_speed_limit") {
                if let Ok(limit) = rhs.trim().parse::<u32>() {
                    let level = if limit <= 50 {
                        ThermalPressure::Critical
                    } else if limit <= 75 {
                        ThermalPressure::Serious
                    } else if limit < 100 {
                        ThermalPressure::Fair
                    } else {
                        ThermalPressure::Nominal
                    };
                    pressure = max_pressure(pressure, level);
                }
            } else if lower.contains("thermal_level") {
                if let Ok(lvl) = rhs.trim().parse::<u32>() {
                    let level = match lvl {
                        0 => ThermalPressure::Nominal,
                        1 => ThermalPressure::Fair,
                        2 => ThermalPressure::Serious,
                        _ => ThermalPressure::Critical,
                    };
                    pressure = max_pressure(pressure, level);
                }
            }
        } else if lower.contains("critical") || lower.contains("heavy") {
            pressure = max_pressure(pressure, ThermalPressure::Critical);
        } else if lower.contains("serious") || lower.contains("moderate") {
            pressure = max_pressure(pressure, ThermalPressure::Serious);
        } else if lower.contains("fair") || lower.contains("light") {
            pressure = max_pressure(pressure, ThermalPressure::Fair);
        }
    }

    pressure
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const fn pressure_rank(p: ThermalPressure) -> u8 {
    match p {
        ThermalPressure::Nominal => 0,
        ThermalPressure::Fair => 1,
        ThermalPressure::Serious => 2,
        ThermalPressure::Critical => 3,
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const fn max_pressure(a: ThermalPressure, b: ThermalPressure) -> ThermalPressure {
    if pressure_rank(a) >= pressure_rank(b) {
        a
    } else {
        b
    }
}

/// Parses Linux/Android `/proc/meminfo` and returns `(total_ram_mb, free_ram_mb)`.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn parse_proc_meminfo(contents: &str) -> Option<(u64, u64)> {
    let mut total_kb: Option<u64> = None;
    let mut available_kb: Option<u64> = None;
    let mut free_kb: u64 = 0;
    let mut buffers_kb: u64 = 0;
    let mut cached_kb: u64 = 0;

    for line in contents.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = rest
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        match key.trim() {
            "MemTotal" => total_kb = Some(kb),
            "MemAvailable" => available_kb = Some(kb),
            "MemFree" => free_kb = kb,
            "Buffers" => buffers_kb = kb,
            "Cached" => cached_kb = kb,
            _ => {}
        }
    }

    let total_mb = total_kb? / 1024;
    let free_mb = available_kb
        .unwrap_or_else(|| free_kb.saturating_add(buffers_kb).saturating_add(cached_kb))
        / 1024;
    Some((total_mb, free_mb.min(total_mb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(total_ram_mb: u64, free_ram_mb: u64, thermal_pressure: ThermalPressure) -> HostProfile {
        HostProfile {
            total_ram_mb,
            free_ram_mb,
            cpu_cores: 8,
            thermal_pressure,
        }
    }

    #[test]
    fn android_phone_profile_always_selects_low_without_override() {
        let p = profile(12288, 6144, ThermalPressure::Nominal);
        let b = select_budget_for_platform(&p, None, true);
        assert_eq!(b, budget_for_tier(ResourceTier::Low));
        assert_eq!(b.movie_decode_threads, 1);
        assert_eq!(b.movie_stream_ahead, 12);
        assert_eq!(b.anisotropy_clamp, 4);
        assert_eq!(b.foreign_level_cache_cap, 0);
        assert_eq!(b.max_render_scale, 1.0);

        // Sister assertion: explicit override still works on Android.
        let forced = select_budget_for_platform(&p, Some("balanced"), true);
        assert_eq!(forced, budget_for_tier(ResourceTier::Balanced));
    }

    #[test]
    fn eight_gb_host_selects_low_and_low_free_ram_drops_to_low() {
        let p8 = profile(8192, 6000, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p8, None, false),
            budget_for_tier(ResourceTier::Low)
        );

        // Even a 32 GB machine drops to Low when free RAM < 4 GB.
        let p32_tight = profile(32768, 3500, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p32_tight, None, false),
            budget_for_tier(ResourceTier::Low)
        );

        // Sister assertion: 16 GB with >= 4 GB free is Balanced, not Low.
        let p16 = profile(16384, 4096, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p16, None, false),
            budget_for_tier(ResourceTier::Balanced)
        );
    }

    #[test]
    fn sixteen_gb_host_selects_balanced() {
        let p16_nominal = profile(16384, 14000, ThermalPressure::Nominal);
        let b = select_budget_for_platform(&p16_nominal, None, false);
        assert_eq!(b, budget_for_tier(ResourceTier::Balanced));
        assert_eq!(b.movie_decode_threads, 2);
        assert_eq!(b.movie_stream_ahead, 20);
        assert_eq!(b.anisotropy_clamp, 8);
        assert_eq!(b.foreign_level_cache_cap, 1);
        assert_eq!(b.max_render_scale, 1.5);

        // Sister assertion: 32 GB with < 12 GB free also lands in Balanced.
        let p32_half = profile(32768, 10000, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p32_half, None, false),
            budget_for_tier(ResourceTier::Balanced)
        );
    }

    #[test]
    fn high_ram_nominal_host_selects_high() {
        let p32 = profile(32768, 20000, ThermalPressure::Nominal);
        let b = select_budget_for_platform(&p32, None, false);
        assert_eq!(b, budget_for_tier(ResourceTier::High));
        assert_eq!(b.movie_decode_threads, 3);
        assert_eq!(b.movie_stream_ahead, 30);
        assert_eq!(b.anisotropy_clamp, 16);
        assert_eq!(b.foreign_level_cache_cap, 2);
        assert_eq!(b.max_render_scale, 2.0);

        // Boundary: exactly 24 GB total and 12 GB free qualifies for High.
        let p24 = profile(24576, 12288, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p24, None, false),
            budget_for_tier(ResourceTier::High)
        );
    }

    #[test]
    fn thermal_pressure_throttles_high_end_host() {
        let p_fair = profile(32768, 24000, ThermalPressure::Fair);
        assert_eq!(
            select_budget_for_platform(&p_fair, None, false),
            budget_for_tier(ResourceTier::Balanced)
        );

        let p_serious = profile(32768, 24000, ThermalPressure::Serious);
        assert_eq!(
            select_budget_for_platform(&p_serious, None, false),
            budget_for_tier(ResourceTier::Low)
        );

        let p_critical = profile(32768, 24000, ThermalPressure::Critical);
        assert_eq!(
            select_budget_for_platform(&p_critical, None, false),
            budget_for_tier(ResourceTier::Low)
        );
    }

    #[test]
    fn env_override_honors_valid_values_and_ignores_unknown() {
        let p32 = profile(32768, 24000, ThermalPressure::Nominal);
        assert_eq!(
            select_budget_for_platform(&p32, Some("low"), false),
            budget_for_tier(ResourceTier::Low)
        );
        assert_eq!(
            select_budget_for_platform(&p32, Some(" Balanced "), false),
            budget_for_tier(ResourceTier::Balanced)
        );

        let p8 = profile(8192, 2048, ThermalPressure::Critical);
        assert_eq!(
            select_budget_for_platform(&p8, Some("HIGH"), false),
            budget_for_tier(ResourceTier::High)
        );

        // Invalid override falls back to normal host selection.
        assert_eq!(
            select_budget_for_platform(&p8, Some("ultra"), false),
            budget_for_tier(ResourceTier::Low)
        );
        assert_eq!(
            select_budget_for_platform(&p32, Some(""), false),
            budget_for_tier(ResourceTier::High)
        );
    }

    #[test]
    fn parses_macos_vm_stat_and_pmset_outputs() {
        let sample_vm_stat = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                              133048.
Pages active:                            317113.
Pages inactive:                          314878.
Pages speculative:                         1734.
";
        // (133048 + 314878 + 1734) * 16384 / 1048576 = 7025 MB
        assert_eq!(parse_vm_stat_free_mb(sample_vm_stat), Some(7025));
        assert_eq!(parse_vm_stat_free_mb("unparseable"), None);

        let nominal = "\
Note: No thermal warning level has been recorded
Note: No performance warning level has been recorded
Note: No CPU power status has been recorded
";
        assert_eq!(parse_pmset_therm(nominal), ThermalPressure::Nominal);
        assert_eq!(
            parse_pmset_therm("CPU_Speed_Limit = 85\n"),
            ThermalPressure::Fair
        );
        assert_eq!(
            parse_pmset_therm("CPU_Speed_Limit = 65\n"),
            ThermalPressure::Serious
        );
        assert_eq!(
            parse_pmset_therm("CPU_Speed_Limit = 40\n"),
            ThermalPressure::Critical
        );
    }

    #[test]
    fn parses_linux_proc_meminfo() {
        let meminfo = "\
MemTotal:       16384000 kB
MemFree:         2048000 kB
MemAvailable:    8192000 kB
Buffers:          512000 kB
Cached:          4096000 kB
";
        assert_eq!(parse_proc_meminfo(meminfo), Some((16000, 8000)));

        // Fallback when MemAvailable is absent.
        let legacy = "\
MemTotal:        8388608 kB
MemFree:         1048576 kB
Buffers:          524288 kB
Cached:          1048576 kB
";
        assert_eq!(parse_proc_meminfo(legacy), Some((8192, 2560)));
    }

    #[test]
    fn live_probe_host_returns_sane_values() {
        let p = probe_host();
        assert!(p.cpu_cores >= 1);
        assert!(p.total_ram_mb > 0);
        assert!(p.free_ram_mb <= p.total_ram_mb);
    }
}
