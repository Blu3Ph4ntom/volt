# Volt - Zero-Config Compiler Cache

A transparent, build-system-agnostic compiler cache that intercepts compiler I/O at the syscall level via `LD_PRELOAD`. No configuration files, no build system integration — just works.

## Architecture

```
volt/
├── Cargo.toml              # workspace root
├── volt/                   # CLI binary (20MB)
│   └── src/
│       ├── main.rs         # core cache engine + CLI
│       ├── daemon.rs       # P2P mDNS daemon
│       ├── state.rs        # metadata state database
│       └── gc.rs           # LRU cache garbage collection
└── volt_tracker/           # LD_PRELOAD cdylib (4.3MB)
    └── src/
        └── lib.rs          # syscall hooks (open, openat, creat, execve)
```

## How It Works

1. `volt run <command>` wraps your build command with `LD_PRELOAD=libvolt_tracker.so`
2. The tracker intercepts every `open`, `openat`, `creat`, and `execve` syscall, logging reads as `R:<path>` and writes as `W:<path>` to a trace file
3. On cache miss: traces the build, hashes all input files (metadata + content SHA-256), stores output artifacts
4. On cache hit: restores artifacts via `ioctl(FICLONE)` → hardlink → copy fallback chain

### Fast-Path Metadata Hashing

The state database (`~/.volt_cache/state.bin`) stores per-file metadata (mtime, size, inode) alongside cached content hashes. On subsequent runs, if metadata hasn't changed, the cached hash is reused without reading file bytes — eliminating the CPU cost of hashing thousands of source files.

### P2P Local Network Cache

`volt daemon` runs a background process that:
- Broadcasts peer presence via UDP multicast (`239.255.255.250:13371`) every 30 seconds
- Serves cached manifests and objects over TCP (port 13370)
- Responds to `QUERY:<hash>` requests with `HIT:<hash>:<ip>:<port>`

When `volt run` encounters a local cache miss, it broadcasts a 100ms multicast query. If a peer responds, it fetches the manifest and objects over TCP with SHA-256 integrity verification, then restores locally.

### Cache Garbage Collection

Configured via `~/.config/volt/config.toml`:

```toml
max_cache_size = "20GB"   # supports GB, MB, TB suffixes
```

GC runs automatically after each cache store, evicting oldest objects (by atime) until the cache falls below 80% of the threshold. Manual cleanup: `volt gc`.

## Installation

```bash
cargo build --release
# Copy binaries to PATH
cp target/release/volt /usr/local/bin/
cp target/release/libvolt_tracker.so /usr/local/lib/
```

## Usage

```bash
# Wrap any build command
volt run cargo build
volt run gcc -o main main.c
volt run make

# Check cache size and run GC
volt gc

# Start P2P daemon (background)
volt daemon

# Benchmark cold vs cached
volt benchmark cargo build
```

## Cache Structure

```
~/.volt_cache/
├── state.bin           # metadata state database (fast-path hashing)
├── manifests/          # JSON manifests keyed by input hash
│   └── <hash>          # { input_files, entries, command, env_fingerprint, hash }
└── objects/            # cached build artifacts
    └── <hash>/
        ├── artifact_0
        ├── artifact_1
        └── ...
```

## Dependencies

Minimal external crates — no async runtime, no database:

| Crate | Purpose |
|-------|---------|
| `sha2` | SHA-256 content hashing |
| `clap` | CLI argument parsing |
| `serde` / `serde_json` | Manifest serialization |
| `libc` | FICLONE ioctl, dlsym hooks |
| `which` | Compiler path resolution |
| `anyhow` | Error handling |
| `tempfile` | Temporary trace files |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `VOLT_CACHE_DIR` | `~/.volt_cache` | Cache root directory |
| `VOLT_TRACE_FILE` | (temp file) | Trace file path for LD_PRELOAD tracker |

## Performance

Typical speedups on repeated builds:

| Scenario | Cold Build | Cached Restore | Speedup |
|----------|-----------|----------------|---------|
| cargo build (159 crates) | ~180s | ~94ms | **1913x** |
| gcc (single file) | ~2s | ~0.3ms | **5000x+** |

Fast-path (metadata unchanged): ~100µs lookup with zero file I/O.

## License

MIT
