use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub mtime_secs: i64,
    pub mtime_nsecs: i64,
    pub size: u64,
    pub inode: u64,
    pub content_hash: String,
}

const STATE_VERSION: u32 = 1;
const HEADER_SIZE: usize = 8;

pub struct StateDb {
    path: PathBuf,
    entries: Mutex<HashMap<String, FileMeta>>,
    dirty: Mutex<bool>,
}

impl StateDb {
    pub fn open(cache_root: &Path) -> Result<Self> {
        let path = cache_root.join("state.bin");
        let entries = if path.exists() {
            Self::load_from_file(&path).unwrap_or_else(|_| HashMap::new())
        } else {
            HashMap::new()
        };

        Ok(Self {
            path,
            entries: Mutex::new(entries),
            dirty: Mutex::new(false),
        })
    }

    fn load_from_file(path: &Path) -> Result<HashMap<String, FileMeta>> {
        let data = fs::read(path).context("Failed to read state.bin")?;
        if data.len() < HEADER_SIZE {
            return Ok(HashMap::new());
        }

        let version = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let entry_count = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        if version != STATE_VERSION {
            return Ok(HashMap::new());
        }

        let mut map = HashMap::new();
        let mut offset = HEADER_SIZE;

        for _ in 0..entry_count {
            if offset + 4 > data.len() {
                break;
            }
            let path_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + path_len > data.len() {
                break;
            }
            let file_path =
                String::from_utf8(data[offset..offset + path_len].to_vec()).unwrap_or_default();
            offset += path_len;

            if offset + 32 > data.len() {
                break;
            }
            let mtime_secs = i64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            let mtime_nsecs = i64::from_le_bytes([
                data[offset + 8],
                data[offset + 9],
                data[offset + 10],
                data[offset + 11],
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ]);
            let size = u64::from_le_bytes([
                data[offset + 16],
                data[offset + 17],
                data[offset + 18],
                data[offset + 19],
                data[offset + 20],
                data[offset + 21],
                data[offset + 22],
                data[offset + 23],
            ]);
            let inode = u64::from_le_bytes([
                data[offset + 24],
                data[offset + 25],
                data[offset + 26],
                data[offset + 27],
                data[offset + 24],
                data[offset + 25],
                data[offset + 26],
                data[offset + 27],
            ]);
            offset += 32;

            if offset + 4 > data.len() {
                break;
            }
            let hash_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            if offset + hash_len > data.len() {
                break;
            }
            let content_hash =
                String::from_utf8(data[offset..offset + hash_len].to_vec()).unwrap_or_default();
            offset += hash_len;

            map.insert(
                file_path,
                FileMeta {
                    mtime_secs,
                    mtime_nsecs,
                    size,
                    inode,
                    content_hash,
                },
            );
        }

        Ok(map)
    }

    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let entries = self.entries.lock().unwrap();
        let mut dirty = self.dirty.lock().unwrap();

        if !*dirty {
            return Ok(());
        }

        let mut data = Vec::new();

        data.extend_from_slice(&STATE_VERSION.to_le_bytes());
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());

        for (path, meta) in entries.iter() {
            let path_bytes = path.as_bytes();
            data.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(path_bytes);

            data.extend_from_slice(&meta.mtime_secs.to_le_bytes());
            data.extend_from_slice(&meta.mtime_nsecs.to_le_bytes());
            data.extend_from_slice(&meta.size.to_le_bytes());
            data.extend_from_slice(&meta.inode.to_le_bytes());

            let hash_bytes = meta.content_hash.as_bytes();
            data.extend_from_slice(&(hash_bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(hash_bytes);
        }

        let tmp_path = self.path.with_extension("bin.tmp");
        fs::write(&tmp_path, &data).context("Failed to write state.bin.tmp")?;
        fs::rename(&tmp_path, &self.path).context("Failed to rename state.bin.tmp to state.bin")?;

        *dirty = false;
        Ok(())
    }

    pub fn get_or_compute_hash<F>(
        &self,
        file_path: &str,
        compute: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Result<String>,
    {
        let meta = fs::metadata(file_path)?;

        let mtime = meta.mtime();
        let mtime_nsec = meta.mtime_nsec();
        let size = meta.len();
        let inode = meta.ino();

        let mut entries = self.entries.lock().unwrap();

        if let Some(existing) = entries.get(file_path) {
            if existing.mtime_secs == mtime
                && existing.mtime_nsecs == mtime_nsec
                && existing.size == size
                && existing.inode == inode
            {
                return Ok(existing.content_hash.clone());
            }
        }

        let content_hash = compute()?;

        entries.insert(
            file_path.to_string(),
            FileMeta {
                mtime_secs: mtime,
                mtime_nsecs: mtime_nsec,
                size,
                inode,
                content_hash: content_hash.clone(),
            },
        );

        let mut dirty = self.dirty.lock().unwrap();
        *dirty = true;

        Ok(content_hash)
    }

    pub fn compute_input_hash_fast(
        &self,
        cmd: &[String],
        files: &std::collections::HashSet<String>,
        env_fingerprint: &str,
    ) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(env_fingerprint.as_bytes());

        let compiler_path = which_first(cmd);
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
                let content_hash = self.get_or_compute_hash(file, || {
                    let mut h = Sha256::new();
                    let content = fs::read(path)?;
                    h.update(&content);
                    Ok(hex::encode(h.finalize()))
                })?;

                let meta = fs::metadata(path)?;
                hasher.update(file.as_bytes());
                hasher.update(&meta.len().to_le_bytes());
                hasher.update(&meta.mtime().to_le_bytes());
                hasher.update(&meta.mtime_nsec().to_le_bytes());
                hasher.update(content_hash.as_bytes());
            }
        }

        Ok(hex::encode(hasher.finalize()))
    }
}

fn which_first(cmd: &[String]) -> Option<String> {
    let name = cmd.first().map(|s| s.as_str()).unwrap_or("cc");
    if let Ok(path) = which::which(name) {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}
