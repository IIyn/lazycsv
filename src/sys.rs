//! Hand-written bindings to the handful of libc functions we need.
//! This replaces the `libc` crate: everything here is part of POSIX and is
//! linked by Rust's std anyway.

use std::ffi::{c_int, c_ulong, c_void};

/// Opaque storage for `struct termios`. We never touch its fields: the
/// kernel fills it (`tcgetattr`), libc makes it raw (`cfmakeraw`) and we
/// hand it back (`tcsetattr`). 256 bytes is larger than any platform's
/// layout (glibc: 60, musl: 60, macOS: 72), so no per-OS struct is needed.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Termios([u8; 256]);

impl Termios {
    pub const fn zeroed() -> Self {
        Termios([0; 256])
    }
}

#[repr(C)]
#[derive(Default)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

#[cfg(target_os = "linux")]
type NfdsT = c_ulong;
#[cfg(not(target_os = "linux"))]
type NfdsT = std::ffi::c_uint;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub const TIOCGWINSZ: c_ulong = 0x5413;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const TIOCGWINSZ: c_ulong = 0x4008_7468;

pub const TCSANOW: c_int = 0;
pub const POLLIN: i16 = 1;
pub const SIGWINCH: c_int = 28;
pub const EINTR: i32 = 4;

pub const PROT_READ: c_int = 1;
pub const MAP_PRIVATE: c_int = 2;
pub const MADV_RANDOM: c_int = 1;
pub const MADV_DONTNEED: c_int = 4;
pub const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

unsafe extern "C" {
    pub fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
    pub fn tcsetattr(fd: c_int, action: c_int, t: *const Termios) -> c_int;
    pub fn cfmakeraw(t: *mut Termios);
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn poll(fds: *mut PollFd, nfds: NfdsT, timeout: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    pub fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    pub fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
}
