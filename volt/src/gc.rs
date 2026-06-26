use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_MAX_CACHE_GB: u64 = 20;
const GC_TARGET_RATIO: f64 = 0.8;

#[derive(Debug)]
pub struct GcConfig {
    pub max_cache_bytes: u64,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_cache_bytes: DEFAULT_MAX_CACHE_GB * 1024 * 1024 * 1024,
        }
    }
}

pub fn load_config() -> GcConfig {
    let config_path = dirs_config_path();
    if let Ok(data) = fs::read_to_string(&config_path) {
        parse_config(&data)
    } else {
        GcConfig::default()
    }
}

fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("volt")
        .join("config.toml")
}

fn parse_config(data: &str) -> GcConfig {
    let mut config = GcConfig::default();

    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');

            if key == "max_cache_size" {
                config.max_cache_bytes = parse_size(value).unwrap_or(DEFAULT_MAX_CACHE_GB * 1024 * 1024 * 1024);
            }
        }
    }

    config
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.ends_with("gb") || s.ends_with("g") {
        let num: u64 = s.trim_end_matches("gb").trim_end_matches('g').trim().parse().ok()?;
        Some(num * 1024 * 1024 * 1024)
    } else if s.ends_with("mb") || s.ends_with("m") {
        let num: u64 = s.trim_end_matches("mb").trim_end_matches('m').trim().parse().ok()?;
        Some(num * 1024 * 1024)
    } else if s.ends_with("tb") || s.ends_with("t") {
        let num: u64 = s.trim_end_matches("tb").trim_end_matches('t').trim().parse().ok()?;
        Some(num * 1024 * 1024 * 1024 * 1024)
    } else {
        s.parse::<u64>().ok().map(|n| n * 1024 * 1024 * 1024)
    }
}

pub fn run_gc(cache_root: &Path) -> Result<u64> {
    let config = load_config();
    let objects_dir = cache_root.join("objects");

    if !objects_dir.exists() {
        return Ok(0);
    }

    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total_size: u64 = 0;

    for entry in fs::read_dir(&objects_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let mut dir_size: u64 = 0;
        let mut newest_atime = SystemTime::UNIX_EPOCH;

        for artifact in fs::read_dir(&path)? {
            let artifact = artifact?;
            let artifact_path = artifact.path();
            if artifact_path.is_file() {
                if let Ok(meta) = fs::metadata(&artifact_path) {
                    dir_size += meta.len();
                    if let Ok(atime) = artifact_path
                        .metadata()
                        .and_then(|m| m.accessed())
                    {
                        if atime > newest_atime {
                            newest_atime = atime;
                        }
                    }
                }
            }
        }

        entries.push((path, dir_size, newest_atime));
        total_size += dir_size;
    }

    if total_size <= config.max_cache_bytes {
        return Ok(0);
    }

    let target_size = (config.max_cache_bytes as f64 * GC_TARGET_RATIO) as u64;
    let mut bytes_to_free = total_size.saturating_sub(target_size);

    entries.sort_by(|a, b| a.2.cmp(&b.2));

    let mut freed: u64 = 0;
    let mut deleted_count = 0;

    for (path, dir_size, _atime) in &entries {
        if bytes_to_free == 0 {
            break;
        }

        let manifest_hash = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if manifest_hash.is_empty() {
            continue;
        }

        let manifest_path = cache_root.join("manifests").join(manifest_hash);
        if manifest_path.exists() {
            let _ = fs::remove_file(&manifest_path);
        }

        let _ = fs::remove_dir_all(path);
        freed += dir_size;
        bytes_to_free = bytes_to_free.saturating_sub(*dir_size);
        deleted_count += 1;
    }

    if deleted_count > 0 {
        println!(
            "Volt GC: Evicted {} cached builds, freed {:.1} MB",
            deleted_count,
            freed as f64 / (1024.0 * 1024.0)
        );
    }

    Ok(freed)
}

pub fn get_cache_size(cache_root: &Path) -> Result<u64> {
    let objects_dir = cache_root.join("objects");
    if !objects_dir.exists() {
        return Ok(0);
    }

    let mut total: u64 = 0;
    for entry in fs::read_dir(&objects_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            for artifact in fs::read_dir(&path)? {
                let artifact = artifact?;
                if artifact.path().is_file() {
                    if let Ok(meta) = fs::metadata(artifact.path()) {
                        total += meta.len();
                    }
                }
            }
        }
    }

    Ok(total)
}
