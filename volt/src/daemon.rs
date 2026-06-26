use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VOLT_PORT: u16 = 13370;
const MULTICAST_ADDR: &str = "239.255.255.250";
const MULTICAST_PORT: u16 = 13371;
const KEEPALIVE_INTERVAL: u64 = 30;
const PEER_TIMEOUT: u64 = 90;
const TCP_TIMEOUT_MS: u64 = 500;
const TCP_LONG_TIMEOUT_MS: u64 = 5000;
const P2P_QUERY_TIMEOUT_MS: u64 = 100;

#[derive(Debug, Clone)]
struct PeerInfo {
    _addr: String,
    _port: u16,
    last_seen: Instant,
}

struct DaemonState {
    peers: Mutex<HashMap<String, PeerInfo>>,
    cache_root: PathBuf,
}

impl DaemonState {
    fn new(cache_root: PathBuf) -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            cache_root,
        }
    }

    fn update_peer(&self, ip: &str, port: u16) {
        let key = format!("{}:{}", ip, port);
        let mut peers = self.peers.lock().unwrap();
        peers.insert(
            key,
            PeerInfo {
                _addr: ip.to_string(),
                _port: port,
                last_seen: Instant::now(),
            },
        );
    }

    fn prune_stale_peers(&self) {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|_, peer| peer.last_seen.elapsed() < Duration::from_secs(PEER_TIMEOUT));
    }
}

pub fn run_daemon(cache_root: PathBuf) -> Result<()> {
    let state = Arc::new(DaemonState::new(cache_root.clone()));

    println!("Volt Daemon starting...");
    println!("  TCP server: 0.0.0.0:{}", VOLT_PORT);
    println!("  mDNS multicast: {}:{}", MULTICAST_ADDR, MULTICAST_PORT);
    println!("  Cache root: {}", cache_root.display());

    let tcp_state = Arc::clone(&state);
    let tcp_handle = std::thread::spawn(move || {
        if let Err(e) = run_tcp_server(tcp_state) {
            eprintln!("TCP server error: {}", e);
        }
    });

    let mdns_state = Arc::clone(&state);
    let mdns_cache_root = cache_root.clone();
    let mdns_handle = std::thread::spawn(move || {
        if let Err(e) = run_mdns_broadcaster(mdns_state, mdns_cache_root) {
            eprintln!("mDNS broadcaster error: {}", e);
        }
    });

    let query_state = Arc::clone(&state);
    let query_cache_root = cache_root.clone();
    let query_handle = std::thread::spawn(move || {
        if let Err(e) = run_query_listener(query_state, query_cache_root) {
            eprintln!("Query listener error: {}", e);
        }
    });

    println!("Volt Daemon running. Press Ctrl+C to stop.");

    tcp_handle.join().unwrap_or(());
    mdns_handle.join().unwrap_or(());
    query_handle.join().unwrap_or(());

    Ok(())
}

fn run_tcp_server(state: Arc<DaemonState>) -> Result<()> {
    let listener = TcpListener::bind(format!("0.0.0.0:{}", VOLT_PORT))
        .context("Failed to bind TCP server")?;
    listener.set_nonblocking(true)?;

    loop {
        match listener.accept() {
            Ok((stream, addr)) => {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    if let Err(e) = handle_tcp_client(stream, addr, state) {
                        eprintln!("Client handler error: {}", e);
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            Err(e) => {
                eprintln!("Accept error: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_tcp_client(
    mut stream: TcpStream,
    _addr: std::net::SocketAddr,
    state: Arc<DaemonState>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim().to_string();

    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    match parts[0] {
        "FETCH_MANIFEST" => {
            let hash = parts.get(1).unwrap_or(&"");
            let manifest_path = state.cache_root.join("manifests").join(hash);
            if manifest_path.exists() {
                let data = fs::read(&manifest_path)?;
                let mut resp_stream = stream;
                resp_stream.write_all(b"MANIFEST ")?;
                resp_stream.write_all(&(data.len() as u32).to_le_bytes())?;
                resp_stream.write_all(&data)?;
            } else {
                stream.write_all(b"NOT_FOUND")?;
            }
        }
        "FETCH_OBJECT" => {
            let rest = parts.get(1).unwrap_or(&"");
            let obj_parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if obj_parts.len() == 2 {
                let hash = obj_parts[0];
                let artifact = obj_parts[1];
                let obj_path = state
                    .cache_root
                    .join("objects")
                    .join(hash)
                    .join(artifact);
                if obj_path.exists() {
                    let data = fs::read(&obj_path)?;
                    let mut resp_stream = stream;
                    resp_stream.write_all(b"OBJECT ")?;
                    resp_stream.write_all(&(data.len() as u32).to_le_bytes())?;
                    resp_stream.write_all(&data)?;
                } else {
                    stream.write_all(b"NOT_FOUND")?;
                }
            } else {
                stream.write_all(b"ERROR bad request")?;
            }
        }
        "LIST_HASHES" => {
            let manifests_dir = state.cache_root.join("manifests");
            let mut hashes = Vec::new();
            if manifests_dir.exists() {
                for entry in fs::read_dir(&manifests_dir)? {
                    let entry = entry?;
                    if entry.path().is_file() {
                        if let Some(name) = entry.path().file_name() {
                            hashes.push(name.to_string_lossy().to_string());
                        }
                    }
                }
            }
            let count = hashes.len() as u32;
            stream.write_all(&count.to_le_bytes())?;
            for hash in hashes {
                let hbytes = hash.as_bytes();
                stream.write_all(&(hbytes.len() as u32).to_le_bytes())?;
                stream.write_all(hbytes)?;
            }
        }
        _ => {
            stream.write_all(b"ERROR unknown command")?;
        }
    }

    Ok(())
}

fn run_mdns_broadcaster(state: Arc<DaemonState>, _cache_root: PathBuf) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("Failed to bind UDP socket")?;
    socket.set_multicast_loop_v4(true)?;
    let multicast: std::net::Ipv4Addr = MULTICAST_ADDR.parse()?;
    socket.join_multicast_v4(&multicast, &"0.0.0.0".parse()?)?;

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    loop {
        let msg = format!("VOLT_PEER:{}:{}", local_ip, VOLT_PORT);
        let _ = socket.send_to(
            msg.as_bytes(),
            format!("{}:{}", MULTICAST_ADDR, MULTICAST_PORT),
        );

        state.prune_stale_peers();

        std::thread::sleep(Duration::from_secs(KEEPALIVE_INTERVAL));
    }
}

fn run_query_listener(state: Arc<DaemonState>, cache_root: PathBuf) -> Result<()> {
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", MULTICAST_PORT))
        .context("Failed to bind query listener")?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let multicast: std::net::Ipv4Addr = MULTICAST_ADDR.parse()?;
    socket.join_multicast_v4(&multicast, &"0.0.0.0".parse()?)?;

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    let mut buf = [0u8; 2048];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((len, src)) => {
                let msg = String::from_utf8_lossy(&buf[..len]);
                let msg = msg.trim();

                if msg.starts_with("VOLT_PEER:") {
                    let peer_info = &msg[10..];
                    if let Some(colon_pos) = peer_info.rfind(':') {
                        let ip = &peer_info[..colon_pos];
                        if let Ok(port) = peer_info[colon_pos + 1..].parse::<u16>() {
                            if ip != local_ip {
                                state.update_peer(ip, port);
                            }
                        }
                    }
                } else if msg.starts_with("QUERY:") {
                    let query_hash = &msg[6..];
                    let manifest_path = cache_root.join("manifests").join(query_hash);
                    if manifest_path.exists() {
                        let resp = format!("HIT:{}:{}", query_hash, local_ip);
                        let _ = socket.send_to(resp.as_bytes(), (src.ip(), MULTICAST_PORT));
                    }
                } else if msg.starts_with("HIT:") {
                    let hit_info = &msg[4..];
                    let hit_parts: Vec<&str> = hit_info.splitn(3, ':').collect();
                    if hit_parts.len() == 3 {
                        let hit_hash = hit_parts[0];
                        let hit_ip = hit_parts[1];
                        if let Ok(hit_port) = hit_parts[2].parse::<u16>() {
                            let state_clone = Arc::clone(&state);
                            let cache_root_clone = cache_root.clone();
                            let hit_hash = hit_hash.to_string();
                            let hit_ip = hit_ip.to_string();
                            std::thread::spawn(move || {
                                if let Err(e) = fetch_from_peer(
                                    &state_clone,
                                    &cache_root_clone,
                                    &hit_hash,
                                    &hit_ip,
                                    hit_port,
                                ) {
                                    eprintln!("Peer fetch error: {}", e);
                                }
                            });
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {}
        }
    }
}

fn fetch_from_peer(
    _state: &DaemonState,
    cache_root: &Path,
    hash: &str,
    peer_ip: &str,
    peer_port: u16,
) -> Result<()> {
    let peer_addr = format!("{}:{}", peer_ip, peer_port);

    let mut stream = TcpStream::connect(&peer_addr)
        .context(format!("Failed to connect to peer {}", peer_addr))?;
    stream.set_read_timeout(Some(Duration::from_millis(TCP_LONG_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;

    stream.write_all(format!("FETCH_MANIFEST {}\n", hash).as_bytes())?;

    let mut header = [0u8; 9];
    stream.read_exact(&mut header)?;
    let header_str = std::str::from_utf8(&header)?;

    if header_str.starts_with("MANIFEST ") {
        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data)?;

        let manifest_dir = cache_root.join("manifests");
        fs::create_dir_all(&manifest_dir)?;
        fs::write(manifest_dir.join(hash), &data)?;

        let manifest: crate::CacheManifest = serde_json::from_slice(&data)?;
        let objects_dir = cache_root.join("objects").join(hash);
        fs::create_dir_all(&objects_dir)?;

        for (idx, entry) in manifest.entries.iter().enumerate() {
            let artifact_name = format!("artifact_{}", idx);
            let artifact_path = objects_dir.join(&artifact_name);
            if artifact_path.exists() {
                continue;
            }

            let mut obj_stream = TcpStream::connect(&peer_addr)?;
            obj_stream.set_read_timeout(Some(Duration::from_millis(TCP_LONG_TIMEOUT_MS)))?;
            obj_stream.set_write_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;

            obj_stream.write_all(
                format!("FETCH_OBJECT {} {}\n", hash, artifact_name).as_bytes(),
            )?;

            let mut obj_header = [0u8; 7];
            obj_stream.read_exact(&mut obj_header)?;
            let obj_header_str = std::str::from_utf8(&obj_header)?;

            if obj_header_str.starts_with("OBJECT ") {
                let mut obj_len_bytes = [0u8; 4];
                obj_stream.read_exact(&mut obj_len_bytes)?;
                let obj_len = u32::from_le_bytes(obj_len_bytes) as usize;

                let tmp_path = artifact_path.with_extension("tmp");
                let mut hasher = Sha256::new();
                let mut file = fs::File::create(&tmp_path)?;
                let mut remaining = obj_len;
                let mut buf = [0u8; 8192];

                while remaining > 0 {
                    let to_read = remaining.min(buf.len());
                    match obj_stream.read(&mut buf[..to_read]) {
                        Ok(0) => {
                            let _ = fs::remove_file(&tmp_path);
                            anyhow::bail!("Connection closed while reading object from peer");
                        }
                        Ok(n) => {
                            file.write_all(&buf[..n])?;
                            hasher.update(&buf[..n]);
                            remaining -= n;
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                            let _ = fs::remove_file(&tmp_path);
                            anyhow::bail!("Timeout reading object from peer");
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                            continue;
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&tmp_path);
                            anyhow::bail!("Network error reading object: {}", e);
                        }
                    }
                }

                let downloaded_hash = hex::encode(hasher.finalize());
                let expected_hash = &entry.object_hash;

                if downloaded_hash != *expected_hash {
                    let _ = fs::remove_file(&tmp_path);
                    anyhow::bail!(
                        "Integrity check failed: expected {}, got {}",
                        &expected_hash[..16],
                        &downloaded_hash[..16]
                    );
                }

                fs::rename(&tmp_path, &artifact_path)?;
            } else {
                let _ = fs::remove_file(artifact_path.with_extension("tmp"));
            }
        }

        println!("Volt P2P: Fetched {} from peer {}", &hash[..16], peer_addr);
    }

    Ok(())
}

pub fn query_peer_cache(cache_root: &Path, hash: &str) -> Result<Option<String>> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("Failed to bind query socket")?;
    socket.set_read_timeout(Some(Duration::from_millis(P2P_QUERY_TIMEOUT_MS)))?;
    socket.set_multicast_loop_v4(true)?;
    let multicast: std::net::Ipv4Addr = MULTICAST_ADDR.parse()?;
    socket.join_multicast_v4(&multicast, &"0.0.0.0".parse()?)?;

    let query = format!("QUERY:{}", hash);
    let _ = socket.send_to(
        query.as_bytes(),
        format!("{}:{}", MULTICAST_ADDR, MULTICAST_PORT),
    );

    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_millis(P2P_QUERY_TIMEOUT_MS);

    loop {
        if Instant::now() >= deadline {
            break;
        }

        match socket.recv_from(&mut buf) {
            Ok((len, _src)) => {
                let msg = String::from_utf8_lossy(&buf[..len]);
                let msg = msg.trim();

                if msg.starts_with("HIT:") {
                    let hit_info = &msg[4..];
                    let hit_parts: Vec<&str> = hit_info.splitn(3, ':').collect();
                    if hit_parts.len() == 3 && hit_parts[0] == hash {
                        let hit_ip = hit_parts[1];
                        if let Ok(hit_port) = hit_parts[2].parse::<u16>() {
                            let peer_addr = format!("{}:{}", hit_ip, hit_port);
                            println!("Volt P2P: Cache HIT from peer {}", peer_addr);

                            match fetch_manifest_from_peer(cache_root, hash, &peer_addr) {
                                Ok(()) => {}
                                Err(e) => {
                                    eprintln!("Volt P2P: Failed to fetch manifest: {}", e);
                                    return Ok(None);
                                }
                            }

                            match fetch_objects_from_peer(cache_root, hash, &peer_addr) {
                                Ok(()) => {}
                                Err(e) => {
                                    eprintln!("Volt P2P: Failed to fetch objects: {}", e);
                                    return Ok(None);
                                }
                            }

                            return Ok(Some(peer_addr));
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => {
                break;
            }
        }
    }

    Ok(None)
}

fn fetch_manifest_from_peer(
    cache_root: &Path,
    hash: &str,
    peer_addr: &str,
) -> Result<()> {
    let mut stream = TcpStream::connect(peer_addr)
        .context(format!("Failed to connect to peer {}", peer_addr))?;
    stream.set_read_timeout(Some(Duration::from_millis(TCP_LONG_TIMEOUT_MS)))?;
    stream.set_write_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;

    stream.write_all(format!("FETCH_MANIFEST {}\n", hash).as_bytes())?;

    let mut header = [0u8; 9];
    stream.read_exact(&mut header)?;
    let header_str = std::str::from_utf8(&header)?;

    if header_str.starts_with("MANIFEST ") {
        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data)?;

        let manifest_dir = cache_root.join("manifests");
        fs::create_dir_all(&manifest_dir)?;
        fs::write(manifest_dir.join(hash), &data)?;
    }

    Ok(())
}

fn fetch_objects_from_peer(
    cache_root: &Path,
    hash: &str,
    peer_addr: &str,
) -> Result<()> {
    let manifest_path = cache_root.join("manifests").join(hash);
    if !manifest_path.exists() {
        return Ok(());
    }

    let data = fs::read(&manifest_path)?;
    let manifest: crate::CacheManifest = serde_json::from_slice(&data)?;

    let objects_dir = cache_root.join("objects").join(hash);
    fs::create_dir_all(&objects_dir)?;

    for (idx, entry) in manifest.entries.iter().enumerate() {
        let artifact_name = format!("artifact_{}", idx);
        let artifact_path = objects_dir.join(&artifact_name);
        if artifact_path.exists() {
            continue;
        }

        let mut stream = match TcpStream::connect(peer_addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Volt P2P: Connection to peer failed: {}", e);
                continue;
            }
        };
        stream.set_read_timeout(Some(Duration::from_millis(TCP_LONG_TIMEOUT_MS)))?;
        stream.set_write_timeout(Some(Duration::from_millis(TCP_TIMEOUT_MS)))?;

        if let Err(e) = stream.write_all(
            format!("FETCH_OBJECT {} {}\n", hash, artifact_name).as_bytes(),
        ) {
            eprintln!("Volt P2P: Failed to send request: {}", e);
            continue;
        }

        let mut obj_header = [0u8; 7];
        if let Err(e) = stream.read_exact(&mut obj_header) {
            eprintln!("Volt P2P: Failed to read object header: {}", e);
            continue;
        }
        let obj_header_str = match std::str::from_utf8(&obj_header) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !obj_header_str.starts_with("OBJECT ") {
            continue;
        }

        let mut obj_len_bytes = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut obj_len_bytes) {
            eprintln!("Volt P2P: Failed to read object length: {}", e);
            continue;
        }
        let obj_len = u32::from_le_bytes(obj_len_bytes) as usize;

        let tmp_path = artifact_path.with_extension("tmp");
        let mut hasher = Sha256::new();
        let mut file = match fs::File::create(&tmp_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Volt P2P: Failed to create temp file: {}", e);
                continue;
            }
        };
        let mut remaining = obj_len;
        let mut buf = [0u8; 8192];
        let mut integrity_ok = true;

        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            match stream.read(&mut buf[..to_read]) {
                Ok(0) => {
                    eprintln!("Volt P2P: Connection closed mid-transfer");
                    integrity_ok = false;
                    break;
                }
                Ok(n) => {
                    if file.write_all(&buf[..n]).is_err() {
                        integrity_ok = false;
                        break;
                    }
                    hasher.update(&buf[..n]);
                    remaining -= n;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    eprintln!("Volt P2P: Timeout during transfer");
                    integrity_ok = false;
                    break;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(e) => {
                    eprintln!("Volt P2P: Network error: {}", e);
                    integrity_ok = false;
                    break;
                }
            }
        }

        if !integrity_ok {
            let _ = fs::remove_file(&tmp_path);
            continue;
        }

        let downloaded_hash = hex::encode(hasher.finalize());
        if downloaded_hash != entry.object_hash {
            eprintln!(
                "Volt P2P: Integrity mismatch for {}: expected {}.., got {}..",
                artifact_name,
                &entry.object_hash[..16],
                &downloaded_hash[..16]
            );
            let _ = fs::remove_file(&tmp_path);
            continue;
        }

        if fs::rename(&tmp_path, &artifact_path).is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
    }

    Ok(())
}

fn get_local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}
