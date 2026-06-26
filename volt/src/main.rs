use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "volt")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Clone)]
enum Commands {
    Init,
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    Benchmark {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

struct Volt {
    root: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct CacheEntry {
    path: String,
    object_hash: String,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct CacheManifest {
    input_files: Vec<String>,
    entries: Vec<CacheEntry>,
    command: Vec<String>,
    env_fingerprint: String,
    hash: String,
}

fn is_transient_output(path: &str) -> bool {
    if path.contains("/tmp/cc") || path.contains("/tmp/rustc") {
        return true;
    }
    if path.contains(".cargo-lock") || path.contains(".cargo-build-lock") || path.contains(".cargo-artifact-lock") {
        return true;
    }
    if path.contains("CACHEDIR.TAG") {
        return true;
    }
    false
}

impl Volt {
    fn new() -> Result<Self> {
        let root = env::var("VOLT_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
                Path::new(&home).join(".volt_cache")
            });
        Ok(Self { root })
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.root.join("objects"))?;
        fs::create_dir_all(self.root.join("manifests"))?;
        Ok(())
    }

    fn env_fingerprint(cmd: &[String]) -> String {
        let mut hasher = Sha256::new();
        let env_keys = ["CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS", "RUSTFLAGS", "PROFILE", "OPT_LEVEL"];
        for key in &env_keys {
            if let Ok(val) = env::var(key) {
                hasher.update(key.as_bytes());
                hasher.update(val.as_bytes());
            }
        }
        for arg in cmd {
            hasher.update(arg.as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    fn compute_file_hash(path: &Path) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(path.to_string_lossy().as_bytes());
        let content = fs::read(path).context(format!("Failed to read {}", path.display()))?;
        hasher.update(&content);
        Ok(hex::encode(hasher.finalize()))
    }

    fn compute_metadata_fast(path: &Path) -> Result<(u64, i64, i64)> {
        let meta = fs::metadata(path)?;
        Ok((meta.len(), meta.mtime(), meta.mtime_nsec()))
    }

    fn compute_content_hash(path: &Path) -> Result<String> {
        let mut hasher = Sha256::new();
        let content = fs::read(path)?;
        hasher.update(&content);
        Ok(hex::encode(hasher.finalize()))
    }

    fn compute_input_hash(&self, cmd: &[String], files: &HashSet<String>) -> Result<String> {
        let mut hasher = Sha256::new();

        let env_fp = Self::env_fingerprint(cmd);
        hasher.update(env_fp.as_bytes());

        let compiler_path = which(cmd.first().map(|s| s.as_str()).unwrap_or("cc"));
        if let Some(ref cp) = compiler_path {
            hasher.update(b"compiler:");
            hasher.update(cp.as_bytes());
            if let Ok(meta) = fs::metadata(cp) {
                hasher.update(&meta.len().to_le_bytes());
                hasher.update(&meta.mtime().to_le_bytes());
            }
        }

        let mut sorted_files: Vec<_> = files.iter().collect();
        sorted_files.sort();
        for file in sorted_files {
            let path = Path::new(file);
            if path.exists() && path.is_file() {
                if let Ok((size, mtime, mtime_nsec)) = Self::compute_metadata_fast(path) {
                    hasher.update(file.as_bytes());
                    hasher.update(&size.to_le_bytes());
                    hasher.update(&mtime.to_le_bytes());
                    hasher.update(&mtime_nsec.to_le_bytes());
                    if let Ok(content_hash) = Self::compute_content_hash(path) {
                        hasher.update(content_hash.as_bytes());
                    }
                }
            }
        }

        Ok(hex::encode(hasher.finalize()))
    }

    fn is_ephemeral_input(path: &str) -> bool {
        path.starts_with("/tmp/cc")
            || path.starts_with("/tmp/rustc")
            || path.contains("/target/")
            || path.contains(".cargo/")
            || path.contains(".rustup/")
            || path.starts_with("/rustc/")
            || path.contains("/etc/")
            || path.contains("/usr/")
            || path.contains("/lib/")
            || path.contains("/proc/")
            || path.contains("/dev/")
            || path.contains("/sys/")
    }

    fn trace_build(cmd: &[String]) -> Result<(HashSet<String>, HashSet<String>)> {
        let mut inputs = HashSet::new();
        let mut outputs = HashSet::new();

        let trace_file = tempfile::NamedTempFile::new()?;
        let trace_path = trace_file.path().to_path_buf();

        let status = Command::new("strace")
            .arg("-f")
            .arg("-e")
            .arg("trace=open,openat,creat")
            .arg("-o")
            .arg(&trace_path)
            .args(cmd)
            .stdout(Stdio::inherit())
            .status()
            .context("Failed to run strace")?;

        if !status.success() {
            anyhow::bail!("Build command failed under strace");
        }

        let trace_content = fs::read_to_string(&trace_path)?;
        for line in trace_content.lines() {
            let trimmed = line.trim();
            if trimmed.contains("ENOENT") || trimmed.contains("= -1") || trimmed.contains("= E") {
                continue;
            }

            if let Some(idx) = trimmed.find("linkat(") {
                if let Some(path) = Self::extract_linkat_dst(&trimmed[idx..]) {
                    outputs.insert(path);
                }
                continue;
            }
            if let Some(idx) = trimmed.find("renameat(") {
                if let Some(path) = Self::extract_renameat_dst(&trimmed[idx..]) {
                    outputs.insert(path);
                }
                continue;
            }
            if let Some(idx) = trimmed.find("renameat2(") {
                if let Some(path) = Self::extract_renameat_dst(&trimmed[idx..]) {
                    outputs.insert(path);
                }
                continue;
            }
            if let Some(idx) = trimmed.find("rename(") {
                if let Some(path) = Self::extract_rename_dst(&trimmed[idx..]) {
                    outputs.insert(path);
                }
                continue;
            }
            if let Some(idx) = trimmed.find("link(") {
                if let Some(path) = Self::extract_rename_dst(&trimmed[idx..]) {
                    outputs.insert(path);
                }
                continue;
            }

            let syscall_line = if let Some(idx) = trimmed.find("openat64(") {
                &trimmed[idx..]
            } else if let Some(idx) = trimmed.find("openat(") {
                &trimmed[idx..]
            } else if let Some(idx) = trimmed.find("open64(") {
                &trimmed[idx..]
            } else if let Some(idx) = trimmed.find("open(") {
                &trimmed[idx..]
            } else if let Some(idx) = trimmed.find("creat(") {
                &trimmed[idx..]
            } else {
                continue;
            };

            if let Some(p) = Self::extract_path(syscall_line) {
                let is_write = syscall_line.contains("O_WRONLY")
                    || syscall_line.contains("O_RDWR")
                    || syscall_line.contains("O_CREAT");
                if is_write {
                    outputs.insert(p);
                } else if !Self::is_ephemeral_input(&p) {
                    inputs.insert(p);
                }
            }
        }

        Ok((inputs, outputs))
    }

    fn extract_linkat_dst(arg: &str) -> Option<String> {
        let rest = arg.trim_start();
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() >= 4 {
            let path_candidate = parts[3].trim();
            if let Some(start) = path_candidate.find('"') {
                if let Some(end) = path_candidate[start + 1..].find('"') {
                    let raw = &path_candidate[start + 1..start + 1 + end];
                    if raw.starts_with('/') {
                        return Some(raw.to_string());
                    }
                }
            }
        }
        None
    }

    fn extract_renameat_dst(arg: &str) -> Option<String> {
        let rest = arg.trim_start();
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() >= 4 {
            let path_candidate = parts[3].trim();
            if let Some(start) = path_candidate.find('"') {
                if let Some(end) = path_candidate[start + 1..].find('"') {
                    let raw = &path_candidate[start + 1..start + 1 + end];
                    if raw.starts_with('/') {
                        return Some(raw.to_string());
                    }
                }
            }
        }
        None
    }

    fn extract_rename_dst(arg: &str) -> Option<String> {
        let rest = arg.trim_start();
        let parts: Vec<&str> = rest.split(',').collect();
        if parts.len() >= 2 {
            let path_candidate = parts[1].trim();
            if let Some(start) = path_candidate.find('"') {
                if let Some(end) = path_candidate[start + 1..].find('"') {
                    let raw = &path_candidate[start + 1..start + 1 + end];
                    if raw.starts_with('/') {
                        return Some(raw.to_string());
                    }
                }
            }
        }
        None
    }

    fn extract_path(arg: &str) -> Option<String> {
        let rest = arg.trim_start();
        if let Some(start) = rest.find('"') {
            if let Some(end) = rest[start + 1..].find('"') {
                let raw = &rest[start + 1..start + 1 + end];
                let unescaped = raw.replace("\\\"", "\"");
                if unescaped.starts_with("/dev/")
                    || unescaped.starts_with("/proc/")
                    || unescaped.starts_with("/sys/")
                    || unescaped.starts_with("pipe:")
                    || unescaped.starts_with("socket:")
                    || !unescaped.starts_with('/')
                {
                    return None;
                }
                return Some(unescaped);
            }
        }
        None
    }

    fn reflink_or_copy(src: &Path, dst: &Path) -> Result<()> {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        if cfg!(target_os = "linux") {
            if try_ficlone(src, dst).is_ok() {
                return Ok(());
            }
        }

        if try_hardlink(src, dst).is_ok() {
            return Ok(());
        }

        fs::copy(src, dst)?;
        Ok(())
    }

    fn store_cache_entry(
        &self,
        output_path: &str,
        cache_dir: &Path,
        entry_idx: usize,
    ) -> Result<CacheEntry> {
        let src = Path::new(output_path);
        let stored_name = format!("artifact_{}", entry_idx);
        let cached_file = cache_dir.join(&stored_name);

        let content_hash = Self::compute_file_hash(src)?;
        let (size, mtime, mtime_nsec) = Self::compute_metadata_fast(src)?;

        fs::copy(src, &cached_file)?;

        Ok(CacheEntry {
            path: output_path.to_string(),
            object_hash: content_hash,
            size,
            mtime,
            mtime_nsec,
        })
    }

    fn restore_entry(entry: &CacheEntry, cache_dir: &Path, entry_idx: usize) -> Result<bool> {
        let stored_name = format!("artifact_{}", entry_idx);
        let cached_file = cache_dir.join(&stored_name);

        if !cached_file.exists() {
            return Ok(false);
        }

        let dst = Path::new(&entry.path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        Self::reflink_or_copy(&cached_file, dst)?;
        Ok(true)
    }

    fn store_outputs(
        &self,
        hash: &str,
        outputs: &HashSet<String>,
        inputs: &HashSet<String>,
        cmd: &[String],
    ) -> Result<()> {
        self.ensure_dirs()?;
        let cache_dir = self.root.join("objects").join(hash);
        fs::create_dir_all(&cache_dir)?;

        let mut entries = Vec::new();
        let mut idx = 0;
        for output in outputs {
            if is_transient_output(output) {
                continue;
            }
            if Path::new(output).exists() {
                if let Ok(entry) = self.store_cache_entry(output, &cache_dir, idx) {
                    entries.push(entry);
                    idx += 1;
                }
            }
        }

        let manifest = CacheManifest {
            input_files: inputs.iter().cloned().collect(),
            entries,
            command: cmd.to_vec(),
            env_fingerprint: Self::env_fingerprint(cmd),
            hash: hash.to_string(),
        };

        let manifest_dir = self.root.join("manifests");
        fs::write(
            manifest_dir.join(hash),
            serde_json::to_string_pretty(&manifest)?,
        )?;

        Ok(())
    }

    fn lookup_cache(&self, hash: &str) -> Result<Option<CacheManifest>> {
        let manifest_path = self.root.join("manifests").join(hash);
        if !manifest_path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&manifest_path)?;
        let manifest: CacheManifest = serde_json::from_str(&data)?;
        Ok(Some(manifest))
    }

    fn restore_manifest(&self, manifest: &CacheManifest) -> Result<usize> {
        let cache_dir = self.root.join("objects").join(&manifest.hash);

        let mut restored = 0;
        for (idx, entry) in manifest.entries.iter().enumerate() {
            if let Ok(true) = Self::restore_entry(entry, &cache_dir, idx) {
                restored += 1;
            }
        }

        Ok(restored)
    }

    fn run(&self, cmd: Vec<String>) -> Result<()> {
        if cmd.is_empty() {
            return Ok(());
        }

        self.ensure_dirs()?;

        let env_fp = Self::env_fingerprint(&cmd);

        match self.find_cached_manifest(&cmd, &env_fp)? {
            Some(existing) => {
                println!("Volt: Cache HIT ({} outputs) - restoring directly", existing.entries.len());
                let t0 = Instant::now();
                let restored = self.restore_manifest(&existing)?;
                println!("Volt: Restored {} artifacts in {:?}", restored, t0.elapsed());
                return Ok(());
            }
            None => {}
        }

        println!("Volt: Tracing build...");
        let (inputs, outputs) = Self::trace_build(&cmd)?;

        if inputs.is_empty() && outputs.is_empty() {
            println!("Volt: No files tracked, running directly");
            let status = Command::new(&cmd[0]).args(&cmd[1..]).status()?;
            if !status.success() {
                anyhow::bail!("Build command failed");
            }
            return Ok(());
        }

        let hash = self.compute_input_hash(&cmd, &inputs)?;
        println!("Volt: Hash = {}", &hash[..16]);

        if let Some(manifest) = self.lookup_cache(&hash)? {
            println!("Volt: Cache HIT - restoring {} outputs", manifest.entries.len());
            let restored = self.restore_manifest(&manifest)?;
            println!("Volt: Restored {} artifacts", restored);
            return Ok(());
        }

        println!(
            "Volt: Storing {} outputs",
            outputs.len()
        );
        self.store_outputs(&hash, &outputs, &inputs, &cmd)?;

        Ok(())
    }

    fn find_cached_manifest(&self, cmd: &[String], env_fp: &str) -> Result<Option<CacheManifest>> {
        let manifests_dir = self.root.join("manifests");
        if !manifests_dir.exists() {
            return Ok(None);
        }

        for entry in fs::read_dir(&manifests_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(manifest) = serde_json::from_str::<CacheManifest>(&data) {
                    let cmd_eq = manifest.command == *cmd;
                    let fp_eq = manifest.env_fingerprint == *env_fp;
                    if cmd_eq && fp_eq {
                        let unchanged = self.inputs_unchanged(&manifest)?;
                        if unchanged {
                            return Ok(Some(manifest));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn inputs_unchanged(&self, manifest: &CacheManifest) -> Result<bool> {
        for file in &manifest.input_files {
            let path = Path::new(file);
            if !path.exists() {
                return Ok(false);
            }
        }

        let hash_dir = self.root.join("objects").join(&manifest.hash);
        if !hash_dir.exists() {
            return Ok(false);
        }

        for (idx, _) in manifest.entries.iter().enumerate() {
            let stored_name = format!("artifact_{}", idx);
            if !hash_dir.join(&stored_name).exists() {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn benchmark(&self, cmd: Vec<String>) -> Result<()> {
        if cmd.is_empty() {
            anyhow::bail!("No command provided");
        }

        self.ensure_dirs()?;

        println!("=== Volt Benchmark ===\n");

        println!("--- Step 1: Cold build (traced) ---");
        let t0 = Instant::now();
        let (inputs, outputs) = Self::trace_build(&cmd)?;
        let cold_time = t0.elapsed();

        if inputs.is_empty() {
            anyhow::bail!("No input files captured");
        }

        let hash = self.compute_input_hash(&cmd, &inputs)?;
        println!("Captured {} inputs, {} outputs", inputs.len(), outputs.len());
        println!("Cold build: {:?}\n", cold_time);

        println!("--- Step 2: Storing in Volt cache ---");
        self.store_outputs(&hash, &outputs, &inputs, &cmd)?;
        let manifest = self.lookup_cache(&hash)?
            .expect("Manifest should exist after store");
        println!("Stored {} artifacts, hash={}\n", manifest.entries.len(), &hash[..16]);

        println!("--- Step 3: Cleaning build artifacts ---");
        self.clean_build_artifacts(&cmd)?;

        println!("--- Step 4: Warm build (Volt cache restore) ---");
        let t1 = Instant::now();
        let restored = self.restore_manifest(&manifest)?;
        let warm_time = t1.elapsed();
        println!("Restored {} artifacts in {:?}\n", restored, warm_time);

        println!("=== Results ===");
        println!("Cold build (traced):  {:?}", cold_time);
        println!("Volt cache restore:   {:?}", warm_time);
        let speedup = cold_time.as_nanos() as f64 / warm_time.as_nanos() as f64;
        println!("Speedup:              {:.0}x faster", speedup);

        Ok(())
    }

    fn clean_build_artifacts(&self, cmd: &[String]) -> Result<()> {
        if cmd.len() >= 1 && cmd[0] == "cargo" {
            if cmd.iter().any(|a| a == "build" || a == "check" || a == "test") {
                let mut clean_cmd = vec!["cargo".to_string(), "clean".to_string()];
                if cmd.iter().any(|a| a == "--release") {
                    clean_cmd.push("--release".to_string());
                }
                println!("Running: {}", clean_cmd.join(" "));
                Command::new(&clean_cmd[0])
                    .args(&clean_cmd[1..])
                    .status()?;
            }
        }
        Ok(())
    }
}

fn which(name: &str) -> Option<String> {
    if let Ok(path) = which::which(name) {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

fn try_ficlone(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let src_file = fs::File::open(src)?;
    let dst_file = fs::File::create(dst)?;
    let src_fd = src_file.as_raw_fd();
    let dst_fd = dst_file.as_raw_fd();
    let ret = unsafe { libc::ioctl(dst_fd, libc::FICLONE, src_fd) };
    if ret != 0 {
        anyhow::bail!("FICLONE failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn try_hardlink(src: &Path, dst: &Path) -> Result<()> {
    fs::hard_link(src, dst)?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let volt = Volt::new()?;

    match cli.command {
        Commands::Init => volt.ensure_dirs()?,
        Commands::Run { cmd } => volt.run(cmd)?,
        Commands::Benchmark { cmd } => volt.benchmark(cmd)?,
    }

    Ok(())
}
