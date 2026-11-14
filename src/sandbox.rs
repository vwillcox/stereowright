//! Network lockdown.
//!
//! Stereowright never needs the network. To make that a guarantee rather than a promise,
//! the very first thing `main` does is install a seccomp filter that makes the kernel
//! refuse to create any IPv4/IPv6/raw-packet/Bluetooth socket for this process and every
//! thread it ever starts (including threads inside ONNX Runtime, CUDA, the GUI toolkit,
//! etc.). io_uring is also refused because it can open sockets without the socket()
//! syscall. Local Unix sockets (the Wayland display and D-Bus for the file dialog) keep
//! working. A filter can never be removed once installed.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lockdown {
    /// Kernel-enforced: the process cannot open network sockets.
    Active,
    /// The filter could not be installed (reason).
    Failed(String),
    /// Not supported on this OS.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Unsupported,
}

impl Lockdown {
    pub fn is_active(&self) -> bool {
        matches!(self, Lockdown::Active)
    }
}

/// Install the network lockdown. Call before any threads are spawned.
pub fn lock_down() -> Lockdown {
    #[cfg(target_os = "linux")]
    {
        match linux::apply() {
            Ok(()) => Lockdown::Active,
            Err(e) => Lockdown::Failed(e.to_string()),
        }
    }
    #[cfg(not(target_os = "linux"))]
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
/// inside an offline namespace (scripts/run-offline.sh).
pub fn visible_interfaces() -> Vec<String> {
    // /proc/self/net/dev reflects *this process's* network namespace
    // (/sys/class/net would show the host's interfaces even inside a namespace).
    std::fs::read_to_string("/proc/self/net/dev")
        .map(|s| {
            s.lines()
                .skip(2)
                .filter_map(|l| l.split(':').next())
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty() && n != "lo")
                .collect()
        })
        .unwrap_or_default()
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
