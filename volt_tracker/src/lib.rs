use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

static mut ORIG_OPEN: usize = 0;
static mut ORIG_OPEN64: usize = 0;
static mut ORIG_OPENAT: usize = 0;
static mut ORIG_OPENAT64: usize = 0;
static mut ORIG_CREAT: usize = 0;
static mut ORIG_EXECVE: usize = 0;
static mut INIT_DONE: bool = false;
static mut TRACE_FD: i32 = -1;

unsafe fn raw_openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    libc::syscall(libc::SYS_openat, dirfd, path, flags, mode) as c_int
}

unsafe fn raw_open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    raw_openat(libc::AT_FDCWD, path, flags, mode)
}

unsafe fn do_init() {
    if INIT_DONE {
        return;
    }
    INIT_DONE = true;

    if let Ok(path) = std::env::var("VOLT_TRACE_FILE") {
        let c_path = std::ffi::CString::new(path).unwrap();
        TRACE_FD = raw_open(
            c_path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        );
    }

    ORIG_OPEN = libc::dlsym(libc::RTLD_NEXT, b"open\0".as_ptr() as *const c_char) as usize;
    ORIG_OPEN64 = libc::dlsym(libc::RTLD_NEXT, b"open64\0".as_ptr() as *const c_char) as usize;
    ORIG_OPENAT = libc::dlsym(libc::RTLD_NEXT, b"openat\0".as_ptr() as *const c_char) as usize;
    ORIG_OPENAT64 = libc::dlsym(libc::RTLD_NEXT, b"openat64\0".as_ptr() as *const c_char) as usize;
    ORIG_CREAT = libc::dlsym(libc::RTLD_NEXT, b"creat\0".as_ptr() as *const c_char) as usize;
    ORIG_EXECVE = libc::dlsym(libc::RTLD_NEXT, b"execve\0".as_ptr() as *const c_char) as usize;
}

unsafe fn resolve_path(dirfd: c_int, path: *const c_char) -> Option<String> {
    if path.is_null() {
        return None;
    }
    let c_str = CStr::from_ptr(path);
    let s = c_str.to_string_lossy().to_string();

    if s.starts_with('/') {
        return Some(s);
    }

    if dirfd == libc::AT_FDCWD {
        if let Ok(cwd) = std::env::current_dir() {
            return Some(cwd.join(&s).to_string_lossy().to_string());
        }
    }

    if dirfd >= 0 {
        let fd_path = format!("/proc/self/fd/{}\0", dirfd);
        let fd_cstr = std::ffi::CString::new(&fd_path[..fd_path.len() - 1]).unwrap();
        let mut buf = [0u8; 4096];
        let n = libc::readlinkat(
            libc::AT_FDCWD,
            fd_cstr.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len() - 1,
        );
        if n > 0 {
            let dir = std::str::from_utf8(&buf[..n as usize]).unwrap_or("");
            return Some(format!("{}/{}", dir, s));
        }
    }

    None
}

unsafe fn log_access(path: *const c_char, is_write: bool) {
    if path.is_null() || TRACE_FD < 0 {
        return;
    }
    let c_str = CStr::from_ptr(path);
    let s = c_str.to_string_lossy();
    let prefix = if is_write { b"W:" } else { b"R:" };
    libc::write(TRACE_FD, prefix.as_ptr() as *const libc::c_void, 2);
    libc::write(TRACE_FD, s.as_ptr() as *const libc::c_void, s.len());
    libc::write(TRACE_FD, b"\n".as_ptr() as *const libc::c_void, 1);
}

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    do_init();
    let is_write = (flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT)) != 0;
    let result = if ORIG_OPEN != 0 {
        let orig: unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(ORIG_OPEN);
        orig(path, flags, mode)
    } else {
        raw_open(path, flags, mode)
    };
    if result != -1 {
        log_access(path, is_write);
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    do_init();
    let is_write = (flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT)) != 0;
    let result = if ORIG_OPEN64 != 0 {
        let orig: unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(ORIG_OPEN64);
        orig(path, flags, mode)
    } else {
        raw_open(path, flags, mode)
    };
    if result != -1 {
        log_access(path, is_write);
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: c_int,
) -> c_int {
    do_init();
    let is_write = (flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT)) != 0;
    let result = if ORIG_OPENAT != 0 {
        let orig: unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(ORIG_OPENAT);
        orig(dirfd, path, flags, mode)
    } else {
        raw_openat(dirfd, path, flags, mode)
    };
    if result != -1 {
        if let Some(resolved) = resolve_path(dirfd, path) {
            if !is_system_path(&resolved) {
                let prefix = if is_write { b"W:" } else { b"R:" };
                libc::write(TRACE_FD, prefix.as_ptr() as *const libc::c_void, 2);
                libc::write(
                    TRACE_FD,
                    resolved.as_ptr() as *const libc::c_void,
                    resolved.len(),
                );
                libc::write(TRACE_FD, b"\n".as_ptr() as *const libc::c_void, 1);
            }
        }
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn openat64(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: c_int,
) -> c_int {
    do_init();
    let is_write = (flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT)) != 0;
    let result = if ORIG_OPENAT64 != 0 {
        let orig: unsafe extern "C" fn(c_int, *const c_char, c_int, c_int) -> c_int =
            std::mem::transmute(ORIG_OPENAT64);
        orig(dirfd, path, flags, mode)
    } else {
        raw_openat(dirfd, path, flags, mode)
    };
    if result != -1 {
        if let Some(resolved) = resolve_path(dirfd, path) {
            if !is_system_path(&resolved) {
                let prefix = if is_write { b"W:" } else { b"R:" };
                libc::write(TRACE_FD, prefix.as_ptr() as *const libc::c_void, 2);
                libc::write(
                    TRACE_FD,
                    resolved.as_ptr() as *const libc::c_void,
                    resolved.len(),
                );
                libc::write(TRACE_FD, b"\n".as_ptr() as *const libc::c_void, 1);
            }
        }
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn creat(path: *const c_char, mode: c_int) -> c_int {
    do_init();
    let result = if ORIG_CREAT != 0 {
        let orig: unsafe extern "C" fn(*const c_char, c_int) -> c_int =
            std::mem::transmute(ORIG_CREAT);
        orig(path, mode)
    } else {
        raw_open(path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, mode)
    };
    if result != -1 {
        log_access(path, true);
    }
    result
}

unsafe fn is_system_path(p: &str) -> bool {
    p.starts_with("/lib")
        || p.starts_with("/usr/lib")
        || p.starts_with("/proc")
        || p.starts_with("/dev")
        || p.starts_with("/etc")
        || p.starts_with("/sys")
        || p.ends_with(".so")
        || p.ends_with(".so.")
}

#[no_mangle]
pub unsafe extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    do_init();
    log_access(path, false);
    let orig: unsafe extern "C" fn(
        *const c_char,
        *const *const c_char,
        *const *const c_char,
    ) -> c_int = std::mem::transmute(ORIG_EXECVE);
    orig(path, argv, envp)
}
