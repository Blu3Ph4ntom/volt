# AGENTS.md - Volt Project Guide

## Project Overview

Volt is a zero-config, build-system-agnostic compiler cache. It intercepts compiler I/O at the syscall level via `LD_PRELOAD`, tracks all file reads/writes, and caches build artifacts for instant restoration on subsequent builds.

**Repository**: `https://github.com/Blu3Ph4ntom/volt`

## Repository Structure

```
volt/
├── Cargo.toml                    # workspace root
├── Cargo.lock
├── README.md
├── AGENTS.md                     # this file
├── volt/                         # CLI binary crate
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # core cache engine, CLI, hashing
│       ├── daemon.rs             # P2P mDNS daemon + TCP server
│       ├── state.rs              # binary state DB for fast-path metadata
│       └── gc.rs                 # LRU cache garbage collection
└── volt_tracker/                 # LD_PRELOAD cdylib
    ├── Cargo.toml
    └── src/
        └── lib.rs                # syscall hooks (open, openat, creat, execve)
```

## Build Commands

```bash
# Build everything
cargo build

# Build release
cargo build --release

# Run tests
cargo test

# Check for warnings
cargo clippy

# Format code
cargo fmt
```

## Architecture Decisions

### Why LD_PRELOAD over strace

Strace uses `ptrace` which adds context-switch overhead to every syscall. For large multi-process builds (cargo, make -j), this can slow compilation by 3-5x, defeating the purpose of a cache. LD_PRELOAD intercepts at the dynamic linker level with near-zero overhead.

The tracker hooks: `open`, `open64`, `openat`, `openat64`, `creat`, `execve`. Relative paths via `openat` are resolved to absolute paths using `/proc/self/fd/<dirfd>`.

### Why binary state DB (state.bin) instead of SQLite

SQLite adds ~2MB to the binary and requires a C dependency. The state DB is a simple binary format: header + length-prefixed entries. It fits in a single file read, handles 10K+ files in <1ms, and can be atomically replaced via tmp+rename.

### Restore chain: reflink → hardlink → copy

1. `ioctl(FICLONE)` — zero-copy CoW on Btrfs/XFS/APFS
2. `fs::hard_link` — same inode, near-zero cost on ext4
3. `fs::copy` — fallback, always works

### P2P protocol

- **Discovery**: UDP multicast `239.255.255.250:13371`, 30s keepalive
- **Data transfer**: TCP `13370`, simple text protocol
- **Integrity**: SHA-256 verified on-the-fly during download
- **Timeouts**: 500ms for control, 5s for object transfers

## Key Files in Detail

### `volt/src/main.rs`
- `Volt` struct: holds cache root path and tracker lib path
- `traced_run()`: executes command with LD_PRELOAD, parses trace file
- `compute_input_hash()`: uses state DB for fast-path, falls back to full SHA-256
- `store_outputs()` / `restore_manifest()`: cache write/read operations
- `reflink_or_copy()`: three-tier restore chain
- `is_ephemeral_input()`: filters out /target/, .cargo/, /usr/, etc.

### `volt/src/daemon.rs`
- `run_daemon()`: spawns TCP server + mDNS broadcaster + query listener threads
- `handle_tcp_client()`: serves FETCH_MANIFEST, FETCH_OBJECT, LIST_HASHES
- `query_peer_cache()`: 100ms multicast query with fail-safe fallback
- `fetch_objects_from_peer()`: streams to .tmp with SHA-256 verification

### `volt/src/state.rs`
- `StateDb::open()`: loads binary state file or creates empty
- `get_or_compute_hash()`: checks metadata, returns cached hash or computes new
- `save()`: atomic write with dirty flag

### `volt/src/gc.rs`
- `load_config()`: parses `~/.config/volt/config.toml`
- `run_gc()`: scans objects dir, sorts by atime, evicts oldest until 80% of limit
- `get_cache_size()`: totals all artifact sizes

### `volt_tracker/src/lib.rs`
- `do_init()`: resolves original syscall pointers via `dlsym(RTLD_NEXT, ...)`
- `resolve_path()`: converts relative paths to absolute via `/proc/self/fd/`
- `is_system_path()`: filters /lib, /usr/lib, /proc, /dev, /etc, /sys, .so

## Coding Conventions

- **Error handling**: Use `anyhow::Result` and `?` operator. Never `unwrap()` in production code.
- **Unsafe code**: Isolated in `volt_tracker` (LD_PRELOAD hooks) and `try_ficlone` (ioctl). All unsafe blocks have clear purpose comments.
- **Dependencies**: Minimize external crates. Prefer `std::net` over tokio for networking. The binary must stay small (<25MB).
- **Naming**: Snake_case for functions/variables, CamelCase for types. Module names match their purpose (state.rs, gc.rs, daemon.rs).
- **File I/O**: Always use atomic writes (tmp + rename) for state files. Never trust mtime alone — always verify content hash.

## Testing

```bash
# Unit tests
cargo test

# Manual integration test
volt run gcc -o test test.c && volt run gcc -o test test.c  # should hit cache

# P2P test (two terminals)
volt daemon  # terminal 1
volt run cargo build  # terminal 2 (with daemon running)

# GC test
volt gc
```

## Common Pitfalls

1. **LD_PRELOAD doesn't propagate to all child processes**: Some build systems clear LD_PRELOAD. The CLI falls back to direct execution if no outputs are captured.
2. **Cache invalidation**: Input files include source files + compiler metadata. Changing compiler version or env vars (CFLAGS, RUSTFLAGS) changes the hash.
3. **Stale peers**: P2P peers that disconnect are pruned after 90s. The 100ms query timeout prevents blocking developer flow.
4. **GC during builds**: GC runs after cache store, not during. It won't delete objects being read by an active restore.

## Commit Messages

Use conventional commits:
- `feat: add new feature`
- `fix: resolve bug`
- `refactor: improve code structure`
- `docs: update documentation`
- `perf: optimize performance`
