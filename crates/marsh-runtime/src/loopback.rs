//! Loopback listeners for port forwards: `127.0.0.1:P` and `[::1]:P`, never a
//! wildcard, without `SO_REUSEPORT`. Shared by a job's worker (inside the job's
//! network namespace) and a shell's guest relay (in the shell VM).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, TcpListener};

/// Why a port could not be bound. A collision is another listener already
/// holding the port; it always wins.
#[derive(Debug)]
pub enum BindError {
    Collision {
        address: &'static str,
    },
    Failed {
        address: &'static str,
        reason: String,
    },
}

enum Bind {
    Collision,
    /// The namespace has no such address family.
    Absent,
    Failed(String),
}

fn listener<A: rustix::net::addr::SocketAddrArg>(
    family: rustix::net::AddressFamily,
    address: &A,
    v6_only: bool,
) -> Result<TcpListener, Bind> {
    use rustix::{io::Errno, net};
    let socket = net::socket(family, net::SocketType::STREAM, None).map_err(|error| {
        if error == Errno::AFNOSUPPORT {
            Bind::Absent
        } else {
            Bind::Failed(format!("socket: {error}"))
        }
    })?;
    rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)
        .map_err(|error| Bind::Failed(format!("fcntl: {error}")))?;
    // `SO_REUSEADDR` only lets a restart bind past connections in TIME_WAIT: a
    // listening socket still conflicts, so an existing listener always wins.
    // Never `SO_REUSEPORT`.
    net::sockopt::set_socket_reuseaddr(&socket, true)
        .map_err(|error| Bind::Failed(format!("setsockopt: {error}")))?;
    if v6_only {
        net::sockopt::set_ipv6_v6only(&socket, true)
            .map_err(|error| Bind::Failed(format!("setsockopt: {error}")))?;
    }
    net::bind(&socket, address).map_err(|error| match error {
        Errno::ADDRINUSE => Bind::Collision,
        Errno::ADDRNOTAVAIL | Errno::AFNOSUPPORT if v6_only => Bind::Absent,
        other => Bind::Failed(format!("bind: {other}")),
    })?;
    net::listen(&socket, 128).map_err(|error| Bind::Failed(format!("listen: {error}")))?;
    let listener = TcpListener::from(socket);
    listener
        .set_nonblocking(true)
        .map_err(|error| Bind::Failed(format!("nonblocking: {error}")))?;
    Ok(listener)
}

/// `127.0.0.1:port` and `[::1]:port`, the second only where the namespace has
/// IPv6. A collision or any other failure on either leaves no listener at all.
///
/// # Errors
/// [`BindError::Collision`] when something already listens there; otherwise
/// the failing step.
pub fn bind_loopback(port: u16) -> Result<(TcpListener, Option<TcpListener>), BindError> {
    let v4 = listener(
        rustix::net::AddressFamily::INET,
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
        false,
    )
    .map_err(|bind| error("127.0.0.1", bind))?;
    match listener(
        rustix::net::AddressFamily::INET6,
        &SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0),
        true,
    ) {
        Ok(v6) => Ok((v4, Some(v6))),
        Err(Bind::Absent) => Ok((v4, None)),
        Err(bind) => Err(error("[::1]", bind)),
    }
}

fn error(address: &'static str, bind: Bind) -> BindError {
    match bind {
        Bind::Collision => BindError::Collision { address },
        Bind::Absent => BindError::Failed {
            address,
            reason: "no such address".into(),
        },
        Bind::Failed(reason) => BindError::Failed { address, reason },
    }
}
