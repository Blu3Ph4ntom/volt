use std::ffi::{CStr, CString};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::Mutex;
use once_cell::sync::Lazy;

static TRACE_FILE: Lazy<Option<Mutex<std::fs::File>>> = Lazy::new(|| {
    std::env::var("VOLT_TRACE_FILE").ok().and_then(|path| {
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    })
});

fn log_access(path: *const c_char) {
    if path.is_null() { return; }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = c_str.to_string_lossy();

    // Filter out system paths to reduce noise
    if path_str.starts_with("/lib") || 
       path_str.starts_with("/usr/lib") || 
       path_str.starts_with("/proc") || 
       path_str.starts_with("/dev") ||
       path_str.starts_with("/etc") {
        return;
    }

    if let Some(ref mutex) = *TRACE_FILE {
        if let Ok(mut file) = mutex.lock() {
            let _ = writeln!(file, "{}", path_str);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    let orig_open: unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int =
        std::mem::transmute(libc::dlsym(libc::RTLD_NEXT, b"open\0".as_ptr() as *const c_char));
    
    let result = orig_open(path, flags, mode);
    if result != -1 {
        log_access(path);
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    let orig_open: unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int =
        std::mem::transmute(libc::dlsym(libc::RTLD_NEXT, b"open64\0".as_ptr() as *const c_char));
    
    let result = orig_open(path, flags, mode);
    if result != -1 {
        log_access(path);
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> c_int {
    let orig_execve: unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int =
        std::mem::transmute(libc::dlsym(libc::RTLD_NEXT, b"execve\0".as_ptr() as *const c_char));
    
    log_access(path);
    orig_execve(path, argv, envp)
}
