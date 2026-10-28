//! The host side of the raw zone: fghjd listens on each virtual IP and port
//! itself and copies the bytes to the port Docker published on `127.0.0.1`.
//! A socket rather than kernel NAT, so nothing another program does to the
//! firewall can switch it off.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{Context, Result, bail};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;

use super::RouteSpec;
use crate::daemon_log;
use crate::platform::Loopback;

type Addr = (Ipv4Addr, u16);

struct Listener {
    /// Swapped in place when the container is recreated on a new port, so the
    /// listener, and connections already open, survive it.
    host_port: Arc<AtomicU16>,
    task: AbortHandle,
}

#[derive(Default)]
pub struct Forwarder {
    listeners: Mutex<BTreeMap<Addr, Listener>>,
    aliases: Mutex<BTreeSet<Ipv4Addr>>,
}

impl Forwarder {
    /// The complete desired state. Returns the addresses another program
    /// already holds, so the caller can move those names elsewhere.
    pub fn apply(&self, routes: &[RouteSpec], loopback: &dyn Loopback) -> Result<Vec<Addr>> {
        let desired: BTreeMap<Addr, u16> = routes
            .iter()
            .map(|r| ((r.virtual_ip, r.container_port), r.host_port))
            .collect();
        let mut listeners = self.listeners.lock().unwrap();

        listeners.retain(|addr, listener| {
            let keep = desired.contains_key(addr);
            if !keep {
                listener.task.abort();
            }
            keep
        });

        let mut taken = Vec::new();
        let mut errors = Vec::new();
        for (&addr, &host_port) in &desired {
            if let Some(listener) = listeners.get(&addr) {
                listener.host_port.store(host_port, Ordering::Relaxed);
                continue;
            }
            match self.listen(addr, host_port, loopback) {
                Ok(listener) => {
                    listeners.insert(addr, listener);
                }
                Err(e) if is_addr_in_use(&e) => taken.push(addr),
                Err(e) => errors.push(format!("{}:{}: {e:#}", addr.0, addr.1)),
            }
        }

        let in_use: BTreeSet<Ipv4Addr> = listeners.keys().map(|(ip, _)| *ip).collect();
        let mut aliases = self.aliases.lock().unwrap();
        for ip in aliases.difference(&in_use).copied().collect::<Vec<_>>() {
            if let Err(e) = loopback.remove_alias(ip) {
                daemon_log::warn(format!("fghjd: failed to remove the {ip} alias: {e:#}"));
            }
            aliases.remove(&ip);
        }

        if !errors.is_empty() {
            bail!("failed to listen on {}", errors.join("; "));
        }
        Ok(taken)
    }

    fn listen(&self, addr: Addr, host_port: u16, loopback: &dyn Loopback) -> Result<Listener> {
        let (ip, port) = addr;
        let mut aliases = self.aliases.lock().unwrap();
        if !aliases.contains(&ip) {
            loopback.add_alias(ip)?;
            aliases.insert(ip);
        }
        let std_listener = std::net::TcpListener::bind((ip, port))?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)
            .context("the raw-zone forwarder needs a tokio runtime")?;
        let host_port = Arc::new(AtomicU16::new(host_port));
        let task = tokio::spawn(accept_loop(listener, host_port.clone())).abort_handle();
        Ok(Listener { host_port, task })
    }

    pub fn clear(&self, loopback: &dyn Loopback) {
        for (_, listener) in std::mem::take(&mut *self.listeners.lock().unwrap()) {
            listener.task.abort();
        }
        for ip in std::mem::take(&mut *self.aliases.lock().unwrap()) {
            let _ = loopback.remove_alias(ip);
        }
    }

    /// What's listening now, with each address's current target.
    pub fn status(&self) -> Vec<RouteSpec> {
        self.listeners
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| !l.task.is_finished())
            .map(|(&(virtual_ip, container_port), l)| RouteSpec {
                virtual_ip,
                container_port,
                host_port: l.host_port.load(Ordering::Relaxed),
            })
            .collect()
    }
}

fn is_addr_in_use(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == ErrorKind::AddrInUse)
}

async fn accept_loop(listener: TcpListener, host_port: Arc<AtomicU16>) {
    loop {
        let (inbound, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Per-connection (the peer gave up, or we're out of descriptors for
            // a moment): the listener itself is fine.
            Err(_) => continue,
        };
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, host_port.load(Ordering::Relaxed)));
        tokio::spawn(async move {
            // A refused connect closes `inbound`, which the client sees as the
            // container not answering, which it isn't.
            if let Ok(mut outbound) = TcpStream::connect(target).await {
                let mut inbound = inbound;
                let _ = inbound.set_nodelay(true);
                let _ = outbound.set_nodelay(true);
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct NoAliases;
    impl Loopback for NoAliases {
        fn add_alias(&self, _ip: Ipv4Addr) -> Result<()> {
            Ok(())
        }
        fn remove_alias(&self, _ip: Ipv4Addr) -> Result<()> {
            Ok(())
        }
        fn alias_present(&self, _ip: Ipv4Addr) -> Option<Result<bool>> {
            None
        }
        fn interface(&self) -> &'static str {
            "test"
        }
        fn needs_aliases(&self) -> bool {
            false
        }
    }

    /// An echo server standing in for the container's published port.
    async fn echo() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0; 64];
                    let n = s.read(&mut buf).await.unwrap();
                    s.write_all(&buf[..n]).await.unwrap();
                });
            }
        });
        port
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn round_trip(port: u16) -> String {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.unwrap();
        buf
    }

    /// Listens on a free port, retrying when a parallel test takes the one
    /// `free_port` found before the forwarder binds it.
    fn listen_on_free_port(forwarder: &Forwarder, host_port: u16) -> u16 {
        loop {
            let port = free_port();
            if forwarder
                .apply(&[route(port, host_port)], &NoAliases)
                .unwrap()
                .is_empty()
            {
                return port;
            }
        }
    }

    fn route(container_port: u16, host_port: u16) -> RouteSpec {
        RouteSpec {
            virtual_ip: Ipv4Addr::LOCALHOST,
            container_port,
            host_port,
        }
    }

    #[tokio::test]
    async fn bytes_reach_the_published_port() {
        let forwarder = Forwarder::default();
        let listen = listen_on_free_port(&forwarder, echo().await);
        assert_eq!(round_trip(listen).await, "ping");
    }

    #[tokio::test]
    async fn a_new_host_port_is_picked_up_without_a_new_listener() {
        let forwarder = Forwarder::default();
        let listen = listen_on_free_port(&forwarder, free_port());
        let echo_port = echo().await;
        forwarder
            .apply(&[route(listen, echo_port)], &NoAliases)
            .unwrap();
        assert_eq!(forwarder.status(), [route(listen, echo_port)]);
        assert_eq!(round_trip(listen).await, "ping");
    }

    #[tokio::test]
    async fn an_address_someone_else_holds_is_reported_not_fatal() {
        let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let forwarder = Forwarder::default();
        let taken = forwarder.apply(&[route(port, 1)], &NoAliases).unwrap();
        assert_eq!(taken, [(Ipv4Addr::LOCALHOST, port)]);
        assert!(forwarder.status().is_empty());
    }

    #[tokio::test]
    async fn a_dropped_route_stops_listening() {
        let forwarder = Forwarder::default();
        let listen = listen_on_free_port(&forwarder, echo().await);
        forwarder.apply(&[], &NoAliases).unwrap();
        assert!(forwarder.status().is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, listen))
                .await
                .is_err()
        );
    }
}
