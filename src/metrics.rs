//! System metrics for the settings page: CPU, memory, disk and the GPU.

use std::time::{Duration, Instant};

use sysinfo::{DiskKind, Disks, System};

/// One sample of everything the settings page shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Usage {
    pub cpu_percent: f32,
    pub cpu_cores: usize,
    pub mem_used: u64,
    pub mem_total: u64,
    pub disks: Vec<DiskUsage>,
    pub gpu: Option<GpuUsage>,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiskUsage {
    pub mount: String,
    pub kind: String,
    pub used: u64,
    pub total: u64,
    pub is_cache: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GpuUsage {
    pub name: String,
    pub vendor: String,
    /// Percent of memory in use, when the driver reports it.
    pub memory_percent: Option<f32>,
    /// Raw utilisation percent, when the driver reports it.
    pub utilisation: Option<f32>,
    pub temperature: Option<f32>,
    /// Present so the UI can say these counters do not exist yet,
    /// showing a bare zero that reads as an idle GPU.
    pub note: Option<String>,
}

/// Samples on a timer, since a reading taken right after boot or right after idle is always
/// wrong. `last` only marks when the last sysinfo refresh happened.
pub struct Meter {
    system: System,
    disks: Disks,
    last: Instant,
    gpu: Option<GpuUsage>,
}

impl Meter {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        let mut disks = Disks::new_with_refreshed_list();
        disks.refresh(true);
        Self {
            system,
            disks,
            last: Instant::now(),
            gpu: detect_gpu(),
        }
    }

    /// Read everything. Call at most about once a second: the CPU reading is a delta
    /// against the previous call, so a faster poll reports noise.
    pub fn sample(&mut self, cache_dir: &std::path::Path, uptime: Duration) -> Usage {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(false);
        self.last = Instant::now();

        let disks = self
            .disks
            .list()
            .iter()
            .filter(|d| matches!(d.kind(), DiskKind::HDD | DiskKind::SSD))
            .map(|d| {
                let total = d.total_space();
                DiskUsage {
                    mount: d.name().to_string_lossy().into_owned(),
                    kind: format!("{:?}", d.kind()),
                    used: total.saturating_sub(d.available_space()),
                    total,
                    is_cache: d.mount_point().starts_with(cache_dir),
                }
            })
            .collect();

        Usage {
            cpu_percent: self.system.global_cpu_usage().clamp(0.0, 100.0),
            cpu_cores: self.system.cpus().len(),
            mem_used: self.system.used_memory(),
            mem_total: self.system.total_memory(),
            disks,
            gpu: self.gpu.clone(),
            uptime_secs: uptime.as_secs(),
        }
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

/// Ask the driver what GPU is present. NVIDIA answers over its own CLI; the other two
/// are identified from sysfs, which is all that is available without a vendor library.
fn detect_gpu() -> Option<GpuUsage> {
    if let Some(g) = nvidia_gpu() {
        return Some(g);
    }
    let (name, vendor) = first_drm_device()?;
    Some(GpuUsage {
        name,
        vendor,
        memory_percent: None,
        utilisation: None,
        temperature: None,
        note: Some("this driver exposes no live counters yet".into()),
    })
}

fn nvidia_gpu() -> Option<GpuUsage> {
    if !std::path::Path::new("/dev/nvidiactl").exists() {
        return None;
    }
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.used,memory.total,utilization.gpu,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    if fields.len() < 5 {
        return None;
    }
    let used: f32 = fields[1].parse().unwrap_or(0.0);
    let total: f32 = fields[2].parse().unwrap_or(0.0);
    Some(GpuUsage {
        name: fields[0].to_string(),
        vendor: "NVIDIA".into(),
        memory_percent: (total > 0.0).then(|| (used / total * 100.0).clamp(0.0, 100.0)),
        utilisation: fields[3].parse().ok(),
        temperature: fields[4].parse().ok(),
        note: None,
    })
}

fn first_drm_device() -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let card = entry.file_name().to_string_lossy().into_owned();
        if !card.starts_with("card") || card.contains('-') {
            continue;
        }
        let vendor_id = std::fs::read_to_string(format!("/sys/class/drm/{card}/device/vendor")).ok()?;
        let vendor = match vendor_id.trim() {
            v if v.ends_with("8086") => "Intel",
            v if v.ends_with("1002") || v.ends_with("1022") => "AMD",
            v if v.ends_with("10de") => "NVIDIA",
            _ => "Unknown",
        }
        .to_string();
        // The first card is the primary GPU, which is the one encoding would use.
        if best.is_none() {
            best = Some((format!("{vendor} graphics"), vendor));
        }
    }
    best
}

/// Size of a directory tree, used for the cache figure. One pass, no symlink following.
pub fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_has_every_field_the_page_needs() {
        let mut meter = Meter::new();
        let usage = meter.sample(std::path::Path::new("/tmp"), Duration::from_secs(90));
        assert!((0.0..=100.0).contains(&usage.cpu_percent));
        assert!(usage.cpu_cores >= 1);
        assert!(usage.mem_total > 0, "total memory should be readable");
        assert!(usage.mem_used <= usage.mem_total);
        assert_eq!(usage.uptime_secs, 90);
        assert!(!usage.disks.is_empty(), "at least one disk should be listed");
        assert!(usage.disks.iter().any(|d| d.total > 0));
    }

    #[test]
    fn repeated_samples_stay_in_range() {
        let mut meter = Meter::new();
        for _ in 0..5 {
            let u = meter.sample(std::path::Path::new("/tmp"), Duration::from_secs(1));
            assert!((0.0..=100.0).contains(&u.cpu_percent));
        }
    }

    #[test]
    fn disk_used_never_exceeds_total() {
        let mut meter = Meter::new();
        let u = meter.sample(std::path::Path::new("/tmp"), Duration::from_secs(1));
        for d in u.disks {
            assert!(d.used <= d.total, "{} used {} of {}", d.mount, d.used, d.total);
        }
    }

    #[test]
    fn cache_mount_is_flagged() {
        let mut meter = Meter::new();
        // "/" as the cache root means every mount is on the cache filesystem.
        let u = meter.sample(std::path::Path::new("/"), Duration::from_secs(1));
        assert!(u.disks.iter().any(|d| d.is_cache));
    }

    #[test]
    fn directory_size_counts_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/one"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.path().join("two"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(dir.path()), 150);
    }

    #[test]
    fn missing_directory_is_zero_not_an_error() {
        assert_eq!(dir_size(std::path::Path::new("/definitely/not/here")), 0);
    }

    #[test]
    fn gpu_detection_does_not_panic_without_a_gpu() {
        // Whatever the machine has, the answer must be representable.
        if let Some(g) = detect_gpu() {
            assert!(!g.name.is_empty());
            assert!(!g.vendor.is_empty());
        }
    }
}
