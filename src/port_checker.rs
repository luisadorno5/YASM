//! TCP port preflight checker (`Services\PortChecker` equivalent).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// Check whether `port` is already in use on the local machine before starting an application.
/// Returns `true` if the port is already taken, or `false` if it is available.
pub fn is_port_in_use(port: u16) -> bool {
    // 1. Check if anything is actively accepting connections on 127.0.0.1:port or [::1]:port
    let addr_v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&addr_v4, Duration::from_millis(75)).is_ok() {
        return true;
    }
    let addr_v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&addr_v6, Duration::from_millis(75)).is_ok() {
        return true;
    }

    // 2. Check if binding 127.0.0.1:port or 0.0.0.0:port fails with AddrInUse
    if let Err(e) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return true;
        }
    }
    if let Err(e) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_port_in_use_should_detect_bound_tcp_listener() {
        // Arrange
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // Act & Assert
        assert!(is_port_in_use(port));
        drop(listener);
    }
}
