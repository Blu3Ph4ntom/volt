use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(name = "volt")]
struct Cli {
    #[subcommand]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init,
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
}

struct Volt {
    root: PathBuf,
    tracker_lib: PathBuf,
}

impl Volt {
    fn new() -> Result<Self> {
        let root = env::var("VOLT_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
                Path::new(&home).join(".volt_cache")
            });
        
        // Assume tracker lib is in the same directory as the executable for this prototype
        let mut tracker_lib = env::current_exe()?;
        tracker_lib.pop();
        tracker_lib.push("libvolt_tracker.so");

        Ok(Self { root, tracker_lib })
    }

    fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.root.join("objects"))?;
        println!("Volt initialized at {}", self.root.display());
        Ok(())
    }

    fn compute_hash(&self, cmd: &[String], files: &HashSet<String>) -> Result<String> {
        let mut hasher = Sha256::new();
        for arg in cmd {
            hasher.update(arg.as_bytes());
        }
        
        let mut sorted_files: Vec<_> = files.iter().collect();
        sorted_files.sort();

        for file in sorted_files {
            let path = Path::new(file);
            if path.exists() && path.is_file() {
                if let Ok(content) = fs::read(path) {
                    hasher.update(file.as_bytes());
                    hasher.update(&content);
                }
            }
        }
        Ok(hex::encode(hasher.finalize()))
    }

    fn run(&self, cmd: Vec<String>) -> Result<()> {
        if cmd.is_empty() { return Ok(()); }

        let trace_file = NamedTempFile::new()?;
        let trace_path = trace_file.path().to_path_buf();

        println!("Volt: Tracing build...");
        let status = Command::new(&cmd[0])
            .args(&cmd[1..])
            .env("LD_PRELOAD", &self.tracker_lib)
            .env("VOLT_TRACE_FILE", &trace_path)
            .status()
            .context("Failed to run command")?;

        if !status.success() {
            return Err(anyhow::anyhow!("Command failed"));
        }

        // Analyze trace
        let mut accessed_files = HashSet::new();
        let file = fs::File::open(&trace_path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if Path::new(&line).exists() {
                accessed_files.insert(line);
            }
        }

        let hash = self.compute_hash(&cmd, &accessed_files)?;
        println!("Volt: Computed build signature: {}", hash);

        Ok(())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let volt = Volt::new()?;

    match cli.command {
        Commands::Init => volt.init()?,
        Commands::Run { cmd } => volt.run(cmd)?,
    }

    Ok(())
}
