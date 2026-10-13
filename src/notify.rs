//! Minimal sd_notify(3) client for service-manager readiness reporting.
//!
//! Implements only what the mount command needs: one newline-separated
//! `KEY=VALUE` state (for example `READY=1\nSTATUS=...`) sent to the AF_UNIX
//! socket named by `$NOTIFY_SOCKET`. When that variable is unset, as during
//! ordinary terminal use, every call is a successful no-op so behavior
//! outside a service manager is unchanged.

use std::io;
use std::time::Duration;

/// How long a send to the notification socket may block before failing.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Offset of `sun_path` within `sockaddr_un`; the family field precedes it.
const SUN_PATH_OFFSET: usize = std::mem::size_of::<libc::sa_family_t>();

/// A parsed `$NOTIFY_SOCKET` value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotifyAddress {
    /// Kernel abstract namespace. systemd writes these with a leading `@`,
    /// which stands for the NUL that begins the address proper.
    Abstract(Vec<u8>),
    /// Ordinary filesystem path.
    Pathname(Vec<u8>),
}

impl NotifyAddress {
    fn parse(value: &str) -> io::Result<Self> {
        if let Some(rest) = value.strip_prefix('@') {
            let mut address = vec![0u8];
            address.extend_from_slice(rest.as_bytes());
            Ok(NotifyAddress::Abstract(address))
        } else if value.len() < sun_path_capacity() {
            Ok(NotifyAddress::Pathname(value.as_bytes().to_vec()))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "notify socket path exceeds sockaddr_un::sun_path",
            ))
        }
    }

    /// Filled `sockaddr_un` plus the meaningful address length: abstract
    /// names keep every byte, pathname sockets end at their NUL terminator.
    fn sockaddr(&self) -> (libc::sockaddr_un, libc::socklen_t) {
        let bytes = match self {
            NotifyAddress::Abstract(bytes) | NotifyAddress::Pathname(bytes) => bytes,
        };
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
            *slot = *byte as libc::c_char;
        }
        let terminator = usize::from(matches!(self, NotifyAddress::Pathname(_)));
        let len = SUN_PATH_OFFSET + bytes.len() + terminator;
        (address, len as libc::socklen_t)
    }
}

/// Send one state update such as `"READY=1\nSTATUS=pinned abc123:plugin/"`.
///
/// Returns `Ok(())` without touching the network when `NOTIFY_SOCKET` is
/// unset or empty, so callers need no service-manager awareness.
pub fn notify(state: &str) -> io::Result<()> {
    let socket = std::env::var("NOTIFY_SOCKET").unwrap_or_default();
    if socket.is_empty() {
        return Ok(());
    }
    let address = NotifyAddress::parse(&socket)?;
    // libsystemd retries with SOCK_STREAM when the datagram socket refuses
    // the connection; mirror that for stream-type notify sockets.
    send_datagram(&address, state.as_bytes()).or_else(|_| send_stream(&address, state.as_bytes()))
}

fn send_datagram(address: &NotifyAddress, payload: &[u8]) -> io::Result<()> {
    let fd = socket(libc::SOCK_DGRAM)?;
    let (sockaddr, len) = address.sockaddr();
    let sent = unsafe {
        libc::sendto(
            fd,
            payload.as_ptr().cast(),
            payload.len(),
            libc::MSG_NOSIGNAL,
            (&sockaddr as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    let result = finish(fd, sent);
    if result.is_ok() {
        debug_assert_eq!(sent, payload.len() as libc::ssize_t);
    }
    result
}

fn send_stream(address: &NotifyAddress, payload: &[u8]) -> io::Result<()> {
    let fd = socket(libc::SOCK_STREAM)?;
    let (sockaddr, len) = address.sockaddr();
    let connected =
        unsafe { libc::connect(fd, (&sockaddr as *const libc::sockaddr_un).cast(), len) };
    if connected < 0 {
        let error = io::Error::last_os_error();
        close(fd);
        return Err(error);
    }
    let mut written = 0usize;
    while written < payload.len() {
        let sent = unsafe {
            libc::send(
                fd,
                payload[written..].as_ptr().cast(),
                payload.len() - written,
                libc::MSG_NOSIGNAL,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            close(fd);
            return Err(error);
        }
        written += sent as usize;
    }
    close(fd);
    Ok(())
}

/// Create a `SOCK_CLOEXEC` AF_UNIX socket with a bounded send timeout.
fn socket(kind: i32) -> io::Result<i32> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let timeout = libc::timeval {
        tv_sec: SEND_TIMEOUT.as_secs() as libc::time_t,
        tv_usec: libc::suseconds_t::from(SEND_TIMEOUT.subsec_micros()),
    };
    let set = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&timeout as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if set < 0 {
        let error = io::Error::last_os_error();
        close(fd);
        return Err(error);
    }
    Ok(fd)
}

fn finish(fd: i32, sent: libc::ssize_t) -> io::Result<()> {
    let result = if sent < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    close(fd);
    result
}

fn close(fd: i32) {
    let _ = unsafe { libc::close(fd) };
}

/// Usable bytes in `sun_path`, derived from the struct layout.
fn sun_path_capacity() -> usize {
    std::mem::size_of::<libc::sockaddr_un>() - SUN_PATH_OFFSET
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_abstract_and_pathname_forms() {
        let abstract_address = NotifyAddress::parse("@/org.freedesktop.systemd1").unwrap();
        assert_eq!(
            abstract_address,
            NotifyAddress::Abstract(b"\0/org.freedesktop.systemd1".to_vec())
        );
        let (sockaddr, len) = abstract_address.sockaddr();
        assert_eq!(sockaddr.sun_family, libc::AF_UNIX as libc::sa_family_t);
        let prefix: Vec<u8> = sockaddr.sun_path[..5].iter().map(|&b| b as u8).collect();
        assert_eq!(prefix, b"\0/org");
        assert_eq!(
            len as usize,
            SUN_PATH_OFFSET + b"\0/org.freedesktop.systemd1".len()
        );

        let path = NotifyAddress::parse("/run/systemd/notify").unwrap();
        assert_eq!(
            path,
            NotifyAddress::Pathname(b"/run/systemd/notify".to_vec())
        );
        let (sockaddr, len) = path.sockaddr();
        let prefix: Vec<u8> = sockaddr.sun_path[..6].iter().map(|&b| b as u8).collect();
        assert_eq!(prefix, b"/run/s");
        // One extra byte for the terminating NUL.
        assert_eq!(
            len as usize,
            SUN_PATH_OFFSET + b"/run/systemd/notify".len() + 1
        );

        assert!(NotifyAddress::parse(&"x".repeat(sun_path_capacity())).is_err());
    }

    #[test]
    fn unset_socket_is_a_noop_and_datagrams_reach_abstract_sockets() {
        // The only test permitted to touch NOTIFY_SOCKET: save, mutate,
        // restore, and keep the abstract name unique to this test.
        let saved = std::env::var("NOTIFY_SOCKET").ok();
        // SAFETY: single-threaded test process; no other thread reads env.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        notify("READY=1").expect("unset socket must be a successful no-op");

        let name = format!("/jj-workfuse-notify-test-{}", std::process::id());
        let receiver = bind_receiver(&name);
        // SAFETY: as above.
        unsafe { std::env::set_var("NOTIFY_SOCKET", format!("@{name}")) };
        notify("READY=1\nSTATUS=pinned test:sub").expect("abstract send");

        let mut buffer = [0u8; 128];
        let received = recv(receiver, &mut buffer);
        close(receiver);
        assert_eq!(&buffer[..received], b"READY=1\nSTATUS=pinned test:sub");

        // SAFETY: as above.
        match saved {
            Some(previous) => unsafe { std::env::set_var("NOTIFY_SOCKET", previous) },
            None => unsafe { std::env::remove_var("NOTIFY_SOCKET") },
        }
    }

    /// Raw libc datagram socket bound into the abstract namespace, since the
    /// std API has no stable abstract-address constructor.
    fn bind_receiver(name: &str) -> i32 {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0);
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        address.sun_family = libc::AF_UNIX as _;
        address.sun_path[0] = 0;
        for (slot, byte) in address.sun_path[1..].iter_mut().zip(name.as_bytes()) {
            *slot = *byte as libc::c_char;
        }
        let len = (SUN_PATH_OFFSET + 1 + name.len()) as libc::socklen_t;
        let bound = unsafe { libc::bind(fd, (&address as *const libc::sockaddr_un).cast(), len) };
        assert_eq!(bound, 0, "{}", io::Error::last_os_error());
        fd
    }

    fn recv(fd: i32, buffer: &mut [u8]) -> usize {
        let received = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
        assert!(received >= 0, "{}", io::Error::last_os_error());
        received as usize
    }
}
