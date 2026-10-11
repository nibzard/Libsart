//! The registered runner record of SPEC 17.6: `bench/environment.json`.
//!
//! Before the first measurement one dedicated developer-laptop-class
//! Linux x86-64 runner with local SSD storage is registered with its
//! actual facts: machine and CPU model, cores, RAM, storage and its
//! filesystem/mount, kernel and OS, Git and Rust versions, the
//! dependency lock hash, build flags, power policy, terminal/PTY, and
//! the benchmark tools. No hardware SKU is invented: the shipped record
//! is `unassigned`, and every field of a registered record is probed
//! from the machine that will run the measurements.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::hash::sha256_file;

/// The environment record schema version.
pub const ENVIRONMENT_SCHEMA_VERSION: u32 = 1;

/// The class SPEC 17.6 declares for the primary runner.
pub const DECLARED_CLASS: &str = "developer-laptop-class linux x86-64, local SSD storage";

/// The hardware facts of one machine, every value probed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Hardware {
    /// The machine model string, when the platform exposes one.
    pub machine_model: Option<String>,
    /// The CPU model string.
    pub cpu_model: Option<String>,
    /// Logical CPU cores.
    pub cpu_cores: Option<u32>,
    /// Physical CPU cores per package, when reported.
    pub cpu_physical_cores: Option<u32>,
    /// The architecture the toolchain reports, such as `x86_64`.
    pub architecture: String,
    /// Total memory in bytes.
    pub ram_total_bytes: Option<u64>,
    /// The storage that holds the benchmark working path.
    pub storage: Option<Storage>,
}

/// The storage one benchmark path sits on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Storage {
    /// The device the mount resolves to.
    pub device: String,
    /// The mount point that contains the path.
    pub mount: String,
    /// The filesystem type.
    pub filesystem: String,
    /// Total bytes on the filesystem.
    pub total_bytes: Option<u64>,
    /// Available bytes when probed.
    pub available_bytes: Option<u64>,
}

/// Operating system and kernel facts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Software {
    /// Kernel release.
    pub kernel: Option<String>,
    /// Operating system name and version.
    pub os: Option<String>,
    /// The `git --version` output.
    pub git_version: Option<String>,
    /// The `rustc --version` output.
    pub rustc_version: Option<String>,
    /// The `cargo --version` output.
    pub cargo_version: Option<String>,
    /// The toolchain channel of `rust-toolchain.toml`, when present.
    pub rust_toolchain: Option<String>,
}

/// How the measured executable was built.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Build {
    /// The Cargo profile the executable is built with.
    pub profile: String,
    /// The `RUSTFLAGS` the build saw, or `none`.
    pub rustflags: String,
    /// The host target triple.
    pub target_triple: Option<String>,
    /// The SHA-256 of `Cargo.lock`.
    pub dependency_lock_sha256: Option<String>,
}

/// The benchmark tools and their pinned versions (SPEC 17.5).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Tools {
    /// This harness's revision string.
    pub harness_revision: String,
    /// Hyperfine's version string, or `absent`.
    pub hyperfine: String,
    /// Whether `perf` is installed.
    pub perf: String,
}

/// Power policy and observed interference.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Power {
    /// The CPU frequency governor, when reported.
    pub governor: Option<String>,
    /// The power supply state, when reported.
    pub supply: Option<String>,
    /// Thermal zone readings in millidegrees Celsius, when reported.
    pub thermal_millidegrees: Vec<u64>,
    /// The load average observed at registration.
    pub load_average: Option<String>,
    /// Operator statement about competing jobs and conditions.
    pub conditions: Option<String>,
}

/// The terminal the interactive metrics will use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Terminal {
    /// `TERM` at registration.
    pub term: Option<String>,
    /// `COLORTERM` at registration, when set.
    pub colorterm: Option<String>,
    /// The PTY method the harness drives.
    pub pty: String,
}

/// One complete runner record. Every section is `None`/empty while the
/// record is `unassigned`; registration fills them with probed values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// `unassigned` until a runner registers; then `registered`.
    pub status: String,
    /// The stable runner identifier, assigned at registration.
    pub runner_id: Option<String>,
    /// The class SPEC 17.6 declares for the runner.
    pub declared_class: String,
    /// Whether the probed machine matches the declared class.
    pub matches_declared_class: Option<bool>,
    /// The class the probed machine actually looks like.
    pub observed_class: Option<String>,
    /// When the runner registered, RFC 3339 UTC.
    pub registered_at: Option<String>,
    /// Probed hardware.
    pub hardware: Option<Hardware>,
    /// Probed software.
    pub software: Option<Software>,
    /// Probed build settings.
    pub build: Option<Build>,
    /// Probed benchmark tools.
    pub tools: Option<Tools>,
    /// Probed power policy and interference.
    pub power: Option<Power>,
    /// Probed terminal facts.
    pub terminal: Option<Terminal>,
    /// Free-form operator notes recorded with the registration.
    pub notes: Vec<String>,
}

impl EnvironmentRecord {
    /// The initial shipped record: unassigned, no invented facts.
    pub fn unassigned() -> Self {
        EnvironmentRecord {
            schema_version: ENVIRONMENT_SCHEMA_VERSION,
            status: "unassigned".to_string(),
            runner_id: None,
            declared_class: DECLARED_CLASS.to_string(),
            matches_declared_class: None,
            observed_class: None,
            registered_at: None,
            hardware: None,
            software: None,
            build: None,
            tools: None,
            power: None,
            terminal: None,
            notes: vec![
                "No runner is registered yet; SPEC 17.6 keeps this record \
                 unassigned and every result not_measured until one is."
                    .to_string(),
            ],
        }
    }

    /// Reads a record from `path`.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let body = fs::read_to_string(path)?;
        serde_json::from_str(&body)
            .map_err(|error| std::io::Error::other(format!("{}: {error}", path.display())))
    }

    /// Writes the record, pretty-printed.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)?;
        fs::write(path, body)
    }

    /// Whether a measurement may run against this record.
    pub fn is_registered(&self) -> bool {
        self.status == "registered"
    }
}

/// Reads one whole small file, when it exists.
fn read_optional(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|body| body.trim().to_string())
}

/// One `key: value` field out of a `/proc/cpuinfo`-style file.
fn proc_field(body: &str, key: &str) -> Option<String> {
    body.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim() == key {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
}

/// Runs one command and returns its trimmed standard output on success.
fn command_stdout(argv: &[&str]) -> Option<String> {
    let output = Command::new(argv[0]).args(&argv[1..]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The CPU model string.
fn probe_cpu_model() -> Option<String> {
    if let Some(body) = read_optional("/proc/cpuinfo") {
        if let Some(model) = proc_field(&body, "model name") {
            return Some(model);
        }
    }
    #[cfg(target_os = "macos")]
    {
        command_stdout(&["sysctl", "-n", "machdep.cpu.brand_string"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Logical and physical core counts.
fn probe_cores() -> (Option<u32>, Option<u32>) {
    if let Some(body) = read_optional("/proc/cpuinfo") {
        let logical = body.lines().filter(|line| line.starts_with("processor")).count();
        let physical = proc_field(&body, "cpu cores").and_then(|value| value.parse().ok());
        if logical > 0 {
            return (Some(logical as u32), physical);
        }
    }
    (
        std::thread::available_parallelism().ok().map(|count| count.get() as u32),
        None,
    )
}

/// The machine model string of the platform.
fn probe_machine_model() -> Option<String> {
    let name = read_optional("/sys/class/dmi/id/product_name")?;
    if name.is_empty() {
        return None;
    }
    match read_optional("/sys/class/dmi/id/product_version") {
        Some(version) if !version.is_empty() && version != "None" => {
            Some(format!("{name} {version}"))
        }
        _ => Some(name),
    }
}

/// Total memory in bytes.
fn probe_ram() -> Option<u64> {
    let body = read_optional("/proc/meminfo")?;
    let total = proc_field(&body, "MemTotal")?;
    let kilobytes: u64 = total.split_whitespace().next()?.parse().ok()?;
    Some(kilobytes.saturating_mul(1024))
}

/// The mount whose directory is the longest prefix of `path`. Octal
/// escapes in `/proc/mounts` are unescaped for the common cases; a miss
/// only costs the storage section of the record.
fn longest_mount(path: &Path) -> Option<(String, String, String)> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let body = fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, String, String, String)> = None;
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        let device = fields.next()?;
        let mount = fields.next()?;
        let filesystem = fields.next()?;
        let mount_path = PathBuf::from(unescape_mount(mount));
        if mount_path != Path::new("/") && !canonical.starts_with(&mount_path) {
            continue;
        }
        let length = mount_path.as_os_str().len();
        if best.as_ref().is_none_or(|(best_length, _, _, _)| length > *best_length) {
            best = Some((length, device.to_string(), mount_path.to_string_lossy().into_owned(), filesystem.to_string()));
        }
    }
    let (_, device, mount, filesystem) = best?;
    Some((device, mount, filesystem))
}

/// Decodes the octal escapes `/proc/mounts` uses for special characters.
fn unescape_mount(raw: &str) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(position) = rest.find('\\') {
        decoded.push_str(&rest[..position]);
        let escape = rest[position..].as_bytes();
        if escape.len() >= 4 {
            let octal = std::str::from_utf8(&escape[1..4]).ok().and_then(|digits| {
                u32::from_str_radix(digits, 8).ok()
            });
            if let Some(value) = octal.map(char::from_u32).flatten() {
                decoded.push(value);
                rest = &rest[position + 4..];
                continue;
            }
        }
        decoded.push('\\');
        rest = &rest[position + 1..];
    }
    decoded.push_str(rest);
    decoded
}

/// Total and available bytes of the filesystem holding `path`, from the
/// portable `df -B1 -P` probe; a miss leaves the counts null.
fn probe_storage_bytes(path: &Path) -> (Option<u64>, Option<u64>) {
    let output = match Command::new("df").args(["-B1", "-P"]).arg(path).output() {
        Ok(output) if output.status.success() => output,
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // The data row reads: device, total, used, available, capacity, mount.
    let Some(fields) = text
        .lines()
        .find(|line| line.starts_with('/'))
        .map(|line| line.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    else {
        return (None, None);
    };
    let number = |index: usize| -> Option<u64> {
        fields.get(index).and_then(|field| field.parse().ok())
    };
    match (number(1), number(3)) {
        (Some(total), Some(available)) => (Some(total), Some(available)),
        _ => (None, None),
    }
}

/// Kernel and OS names.
fn probe_kernel_os() -> (Option<String>, Option<String>) {
    let kernel = read_optional("/proc/sys/kernel/osrelease")
        .or_else(|| command_stdout(&["uname", "-r"]));
    let os = read_optional("/etc/os-release")
        .and_then(|body| release_field(&body, "PRETTY_NAME"))
        .filter(|name| !name.is_empty())
        .or_else(|| command_stdout(&["uname", "-s"]));
    (kernel, os)
}

/// One `KEY=value` field out of an os-release-style file, with the
/// quoting stripped. The `/proc` field parser above cannot read this
/// format: os-release separates with `=`, never `:`, so a colon parser
/// records only the `uname` fallback on distributions like Ubuntu.
fn release_field(body: &str, key: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() == key {
            Some(value.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
}

/// Tool versions.
fn probe_versions() -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    let git = command_stdout(&["git", "--version"]).map(|raw| {
        raw.strip_prefix("git version ")
            .unwrap_or(&raw)
            .trim()
            .to_string()
    });
    let rustc = command_stdout(&["rustc", "--version"]);
    let cargo = command_stdout(&["cargo", "--version"]);
    let host = command_stdout(&["rustc", "-vV"])
        .and_then(|report| proc_field(&report, "host"))
        .or_else(|| command_stdout(&["uname", "-m"]));
    (git, rustc, cargo, host)
}

/// The storage, kernel, and machine facts probed for one path.
fn probe_hardware(path: &Path) -> Hardware {
    let (cores, physical) = probe_cores();
    let storage = longest_mount(path).map(|(device, mount, filesystem)| {
        let (total, available) = probe_storage_bytes(path);
        Storage {
            device,
            mount,
            filesystem,
            total_bytes: total,
            available_bytes: available,
        }
    });
    Hardware {
        machine_model: probe_machine_model(),
        cpu_model: probe_cpu_model(),
        cpu_cores: cores,
        cpu_physical_cores: physical,
        architecture: std::env::consts::ARCH.to_string(),
        ram_total_bytes: probe_ram(),
        storage,
    }
}

/// Power policy and interference observations.
fn probe_power() -> Power {
    let governor = read_optional(
        "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
    );
    let supply = fs::read_dir("/sys/class/power_supply")
        .map(|entries| {
            let mut states = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(state) = read_optional(
                    &format!("/sys/class/power_supply/{name}/online"),
                )
                .or_else(|| {
                    read_optional(&format!("/sys/class/power_supply/{name}/status"))
                }) else {
                    continue;
                };
                states.push(format!("{name}={state}"));
            }
            states.join(", ")
        })
        .ok()
        .filter(|joined| !joined.is_empty());
    let thermal = fs::read_dir("/sys/class/thermal")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    read_optional(&format!("/sys/class/thermal/{name}/temp"))
                })
                .filter_map(|value| value.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default();
    Power {
        governor,
        supply,
        thermal_millidegrees: thermal,
        load_average: read_optional("/proc/loadavg"),
        conditions: None,
    }
}

/// The build facts for `source_root`, the checkout the executable was
/// built from.
fn probe_build(source_root: Option<&Path>) -> Build {
    let rustflags = std::env::var("RUSTFLAGS").unwrap_or_else(|_| "none".to_string());
    let (_, _, _, host) = probe_versions();
    let lock = source_root
        .map(|root| root.join("Cargo.lock"))
        .filter(|path| path.exists())
        .and_then(|path| sha256_file(&path).ok());
    Build {
        profile: "release".to_string(),
        rustflags,
        target_triple: host,
        dependency_lock_sha256: lock,
    }
}

/// The toolchain channel `rust-toolchain.toml` pins, when present.
fn probe_toolchain(source_root: &Path) -> Option<String> {
    let body = read_optional(&source_root.join("rust-toolchain.toml").display().to_string())?;
    body.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() == "channel" {
            Some(value.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
}

/// RFC 3339 UTC for a system time, without pulling a date dependency.
pub fn rfc3339_utc(when: std::time::SystemTime) -> String {
    let seconds = when
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (hour, minute, second) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    // Howard Hinnant's civil-from-days algorithm.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Now, RFC 3339 UTC.
pub fn now_utc() -> String {
    rfc3339_utc(std::time::SystemTime::now())
}

/// Everything the probe found, assembled into one `registered` record.
///
/// `probe_path` is the directory benchmark corpora will live on (its
/// filesystem and mount are the recorded storage); `source_root` is the
/// checkout whose `Cargo.lock` hash and toolchain are recorded.
pub fn probe_record(
    runner_id: Option<&str>,
    probe_path: &Path,
    source_root: Option<&Path>,
    conditions: Option<&str>,
) -> EnvironmentRecord {
    let hardware = probe_hardware(probe_path);
    let (kernel, os) = probe_kernel_os();
    let (git, rustc, cargo, _) = probe_versions();
    let build = probe_build(source_root);
    let mut power = probe_power();
    power.conditions = conditions.map(|text| text.to_string());
    let tools = Tools {
        harness_revision: super::HARNESS_REVISION.to_string(),
        hyperfine: command_stdout(&["hyperfine", "--version"])
            .unwrap_or_else(|| "absent".to_string()),
        perf: if Path::new("/usr/bin/perf").exists() {
            "installed".to_string()
        } else {
            "absent".to_string()
        },
    };
    let observed_class = format!(
        "{} {}",
        os.as_deref().unwrap_or("unknown-os").to_ascii_lowercase(),
        hardware.architecture
    );
    // The declared class names the platform, and the kernel system name
    // is the fact that says `Linux`: a distribution's pretty name may
    // name only the distribution (Ubuntu's does), so the family check
    // reads `uname -s`, not the recorded OS field.
    let matches = Some(
        command_stdout(&["uname", "-s"])
            .is_some_and(|name| name.to_ascii_lowercase().contains("linux"))
            && hardware.architecture == "x86_64",
    );
    let machine_fingerprint = format!(
        "{}|{}|{}|{}",
        hardware.cpu_model.as_deref().unwrap_or("cpu"),
        hardware.cpu_cores.map(|cores| cores.to_string()).unwrap_or_default(),
        hardware.ram_total_bytes.map(|ram| ram.to_string()).unwrap_or_default(),
        hardware.machine_model.as_deref().unwrap_or("model"),
    );
    EnvironmentRecord {
        schema_version: ENVIRONMENT_SCHEMA_VERSION,
        status: "registered".to_string(),
        runner_id: Some(runner_id.map(|id| id.to_string()).unwrap_or_else(|| {
            format!(
                "margin-runner-{}",
                &super::hash::sha256_hex(machine_fingerprint.as_bytes())[..12]
            )
        })),
        declared_class: DECLARED_CLASS.to_string(),
        matches_declared_class: matches,
        observed_class: Some(observed_class),
        registered_at: Some(now_utc()),
        hardware: Some(hardware),
        software: Some(Software {
            kernel,
            os,
            git_version: git,
            rustc_version: rustc,
            cargo_version: cargo,
            rust_toolchain: source_root.and_then(probe_toolchain),
        }),
        build: Some(build),
        tools: Some(tools),
        power: Some(power),
        terminal: Some(Terminal {
            term: std::env::var("TERM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            pty: "not used by the fresh-process CLI metrics; PTY metrics record their own method"
                .to_string(),
        }),
        notes: vec![
            "Every field above is probed from this machine at registration; \
             none is transcribed from the specification."
                .to_string(),
        ],
    }
}

/// The environment variables the timed Margin child and the oracle Git
/// children run under: a private home, no inherited Git redirection, and
/// fixed locale and timezone. `home` holds the fixture identity the
/// mutation commands commit with.
pub fn child_environment(home: &Path) -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    let mut vars = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let scrubs = key.as_encoded_bytes().starts_with(b"GIT_") || key == "EMAIL";
        if !scrubs {
            vars.insert(key, value);
        }
    }
    let mut set = |key: &str, value: std::ffi::OsString| {
        vars.insert(key.into(), value);
    };
    set("HOME", home.as_os_str().to_owned());
    set("XDG_CONFIG_HOME", home.join(".config").into_os_string());
    set("GIT_CONFIG_NOSYSTEM", "1".into());
    set("GIT_TERMINAL_PROMPT", "0".into());
    set("TZ", "UTC".into());
    set("LC_ALL", "C".into());
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unassigned_record_invents_nothing() {
        let record = EnvironmentRecord::unassigned();
        assert_eq!(record.status, "unassigned");
        assert_eq!(record.runner_id, None);
        assert_eq!(record.hardware, None);
        assert_eq!(record.software, None);
        assert_eq!(record.build, None);
        assert_eq!(record.tools, None);
        assert!(record.notes.iter().any(|note| note.contains("unassigned")));
    }

    #[test]
    fn probing_this_machine_records_actual_details() {
        let scratch = std::env::temp_dir();
        let record = probe_record(None, &scratch, None, None);
        assert_eq!(record.status, "registered");
        assert!(record.runner_id.as_deref().unwrap_or("").starts_with("margin-runner-"));

        let hardware = record.hardware.expect("hardware is probed");
        assert_eq!(hardware.architecture, std::env::consts::ARCH);
        assert!(hardware.cpu_cores.unwrap_or(0) > 0, "a core count exists");
        assert!(hardware.ram_total_bytes.unwrap_or(0) > 0, "a RAM total exists");
        let storage = hardware.storage.expect("the scratch directory has a mount");
        assert!(!storage.mount.is_empty() && !storage.filesystem.is_empty());
        if let (Some(total), Some(available)) = (storage.total_bytes, storage.available_bytes) {
            assert!(available > 0 && available <= total);
        }

        // Independent cross-checks: the same facts read directly.
        if cfg!(target_os = "linux") {
            let cpuinfo = fs::read_to_string("/proc/cpuinfo").expect("cpuinfo exists");
            if let Some(model) = hardware.cpu_model.as_deref() {
                assert!(
                    cpuinfo.contains(model),
                    "the recorded CPU model {model} came from elsewhere"
                );
            }
            let cores = cpuinfo.lines().filter(|line| line.starts_with("processor")).count();
            assert_eq!(hardware.cpu_cores, Some(cores as u32));
        }

        let software = record.software.expect("software is probed");
        let git = String::from_utf8_lossy(
            &Command::new("git").arg("--version").output().expect("git runs").stdout,
        )
        .trim()
        .to_string();
        let git = git.strip_prefix("git version ").unwrap_or(&git);
        assert_eq!(software.git_version.as_deref(), Some(git));

        let build = record.build.expect("build is probed");
        assert_eq!(build.profile, "release");
    }

    #[test]
    fn a_registered_record_round_trips() {
        let scratch = std::env::temp_dir();
        let record = probe_record(Some("the-runner"), &scratch, None, Some("quiet machine"));
        let path = scratch.join(format!("margin-envrec-{}.json", std::process::id()));
        record.write(&path).expect("the record writes");
        let back = EnvironmentRecord::load(&path).expect("the record reads back");
        assert_eq!(back, record);
        assert!(back.is_registered());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn os_release_fields_parse_equals_not_colons() {
        let body = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nID=ubuntu\n";
        assert_eq!(
            release_field(body, "PRETTY_NAME").as_deref(),
            Some("Ubuntu 24.04.3 LTS")
        );
        assert_eq!(release_field(body, "ID").as_deref(), Some("ubuntu"));
        assert_eq!(release_field(body, "VERSION"), None);
        assert_eq!(
            proc_field(body, "PRETTY_NAME"),
            None,
            "the colon parser cannot read os-release; using it loses the OS name"
        );
        if cfg!(target_os = "linux") {
            let release = read_optional("/etc/os-release").expect("os-release exists on Linux");
            let pretty = release_field(&release, "PRETTY_NAME");
            assert_eq!(
                probe_kernel_os().1,
                pretty,
                "the recorded OS name is the distribution's pretty name"
            );
        }
    }

    #[test]
    fn rfc3339_matches_known_moments() {
        use std::time::Duration;
        assert_eq!(
            rfc3339_utc(std::time::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            rfc3339_utc(std::time::UNIX_EPOCH + Duration::from_secs(1_767_229_200)),
            "2026-01-01T01:00:00Z"
        );
        assert_eq!(
            rfc3339_utc(std::time::UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z",
            "a leap day crosses correctly"
        );
    }
}
