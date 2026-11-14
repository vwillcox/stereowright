//! Network lockdown.
//!
//! Stereowright never needs the network. To make that a guarantee rather than a promise,
//! the very first thing `main` does is install a seccomp filter that makes the kernel
//! refuse to create any IPv4/IPv6/raw-packet/Bluetooth socket for this process and every
//! thread it ever starts (including threads inside ONNX Runtime, CUDA, the GUI toolkit,
//! etc.). io_uring is also refused because it can open sockets without the socket()
//! syscall. Local Unix sockets (the Wayland display and D-Bus for the file dialog) keep
//! working. A filter can never be removed once installed.
//!
//! On macOS the same guarantee comes from the kernel's sandbox (Seatbelt): `sandbox_init`
//! with Apple's built-in "no-network" profile. It also covers every thread and every child
//! process (ffmpeg), and cannot be lifted once applied.
//!
//! Windows has no way for a program to lock itself out of the network, so there the block
//! is reported as unavailable (the app still contains no networking code).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lockdown {
    /// Kernel-enforced: the process cannot open network sockets.
    #[cfg_attr(windows, allow(dead_code))]
    Active,
    /// The filter could not be installed (reason).
    #[cfg_attr(windows, allow(dead_code))]
    Failed(String),
    /// Not supported on this OS.
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    Unsupported,
}

impl Lockdown {
    pub fn is_active(&self) -> bool {
        matches!(self, Lockdown::Active)
    }
}

/// What enforces the block on this OS, for messages.
pub const MECHANISM: &str = if cfg!(target_os = "linux") {
    "kernel seccomp filter"
} else if cfg!(target_os = "macos") {
    "macOS kernel sandbox, no-network profile"
} else {
    "none available on this OS"
};

/// Install the network lockdown. Call before any threads are spawned.
pub fn lock_down() -> Lockdown {
    #[cfg(target_os = "linux")]
    {
        match linux::apply() {
            Ok(()) => Lockdown::Active,
            Err(e) => Lockdown::Failed(e.to_string()),
        }
    }
    #[cfg(target_os = "macos")]
    {
        match macos::apply() {
            Ok(()) => Lockdown::Active,
            Err(e) => Lockdown::Failed(e),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Lockdown::Unsupported
    }
}

/// Prove the lockdown works by trying to open sockets on *loopback* (127.0.0.1 / ::1),
/// so even if the block failed nothing could leave the machine during the test.
/// Returns Ok if every attempt was refused by the kernel.
pub fn self_test() -> Result<(), String> {
    use std::io::ErrorKind::PermissionDenied;
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::time::Duration;

    let mut leaks = Vec::new();
    match UdpSocket::bind("127.0.0.1:0") {
        Err(e) if e.kind() == PermissionDenied => {}
        Ok(_) => leaks.push("UDP/IPv4 socket was allowed".to_string()),
        Err(e) => leaks.push(format!("UDP/IPv4: unexpected error {e}")),
    }
    match TcpListener::bind("[::1]:0") {
        Err(e) if e.kind() == PermissionDenied => {}
        Ok(_) => leaks.push("TCP/IPv6 socket was allowed".to_string()),
        // No IPv6 on this host at all is also fine.
        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {}
        Err(e) => leaks.push(format!("TCP/IPv6: unexpected error {e}")),
    }
    let addr = "127.0.0.1:9".parse().expect("valid addr");
    match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        Err(e) if e.kind() == PermissionDenied => {}
        Ok(_) => leaks.push("TCP/IPv4 connection was allowed".to_string()),
        Err(e) => leaks.push(format!("TCP/IPv4: socket was created ({e})")),
    }
    if leaks.is_empty() {
        Ok(())
    } else {
        Err(leaks.join("; "))
    }
}

/// Network interfaces visible to this process other than loopback. Empty when running
/// inside an offline namespace (scripts/run-offline.sh). `None` where this can't be told
/// (not Linux).
pub fn visible_interfaces() -> Option<Vec<String>> {
    // /proc/self/net/dev reflects *this process's* network namespace
    // (/sys/class/net would show the host's interfaces even inside a namespace).
    let s = std::fs::read_to_string("/proc/self/net/dev").ok()?;
    Some(
        s.lines()
            .skip(2)
            .filter_map(|l| l.split(':').next())
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty() && n != "lo")
            .collect(),
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;

    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };

    pub fn apply() -> Result<(), Box<dyn std::error::Error>> {
        let families = [
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_PACKET,
            libc::AF_BLUETOOTH,
            libc::AF_X25,
            libc::AF_AX25,
            libc::AF_APPLETALK,
            libc::AF_IPX,
            libc::AF_DECnet,
            libc::AF_ROSE,
            libc::AF_NETROM,
            libc::AF_RDS,
            libc::AF_TIPC,
            libc::AF_VSOCK,
            libc::AF_XDP,
            libc::AF_MPLS,
            libc::AF_CAN,
        ];
        let rules = families
            .iter()
            .map(|&f| {
                SeccompRule::new(vec![SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Eq,
                    f as u64,
                )?])
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut map: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        map.insert(libc::SYS_socket, rules);
        // An empty rule list = always matches: io_uring can create sockets without socket().
        map.insert(libc::SYS_io_uring_setup, vec![]);

        let arch: TargetArch = std::env::consts::ARCH.try_into()?;
        let filter = SeccompFilter::new(
            map,
            SeccompAction::Allow,                     // anything not listed
            SeccompAction::Errno(libc::EACCES as u32), // listed: refuse
            arch,
        )?;
        let prog: BpfProgram = filter.try_into()?;
        seccompiler::apply_filter_all_threads(&prog)?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, c_char, c_int};

    // libsandbox is part of libSystem, which every macOS program links. The API is marked
    // deprecated in the headers but is what `sandbox-exec` uses, and the kernel enforces it.
    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(errorbuf: *mut c_char);
    }
    /// Flags 0: `profile` is a sandbox profile written out (not one of Apple's named ones;
    /// the built-in "no-network" profile kills the process on macOS 27).
    const PROFILE_SOURCE: u64 = 0;
    /// Everything allowed except IP networking in either direction, including loopback.
    /// Local Unix sockets keep working, as on Linux.
    const NO_NETWORK: &CStr = c"(version 1)
        (allow default)
        (deny network-outbound (remote ip))
        (deny network-inbound (local ip))
        (deny network-bind (local ip))";

    pub fn apply() -> Result<(), String> {
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: C API with a NUL-terminated profile; the error buffer is freed with the
        // matching function.
        let rc = unsafe { sandbox_init(NO_NETWORK.as_ptr(), PROFILE_SOURCE, &mut err) };
        if rc == 0 {
            return Ok(());
        }
        let msg = if err.is_null() {
            format!("sandbox_init failed ({rc})")
        } else {
            // SAFETY: on failure libsandbox hands back a NUL-terminated message.
            let m = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
            unsafe { sandbox_free_error(err) };
            m
        };
        Err(msg)
    }
}
