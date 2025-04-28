use crate::punch::maintain::start_task;
use crate::punch::protocol::{ping, LengthPrefixedInitCodec};
use crate::punch::tunnel::TunnelRouter;
use crate::punch::{tunnel, KcpContext, KcpStream, KcpStreamListener};
use async_shutdown::ShutdownManager;
use parking_lot::Mutex;
use rand::prelude::SliceRandom;
use rust_p2p_core::idle::IdleRouteManager;
use rust_p2p_core::nat::{NatInfo, NatType};
use rust_p2p_core::punch::{PunchInfo, Puncher as CorePuncher};
use rust_p2p_core::route::route_table::RouteTable;
use rust_p2p_core::route::Index;
use rust_p2p_core::socket::LocalInterface;
use rust_p2p_core::tunnel::config::{LoadBalance, TunnelConfig};
use rust_p2p_core::tunnel::udp::UDPIndex;
use std::io;
use std::io::Error;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct PunchContext {
    pub(crate) oneself_id: Arc<String>,
    default_interface: Option<LocalInterface>,
    pub(crate) tcp_stun_servers: Vec<String>,
    pub(crate) udp_stun_servers: Vec<String>,
    nat_info: Arc<Mutex<NatInfo>>,
}
impl PunchContext {
    pub fn new(
        oneself_id: Arc<String>,
        default_interface: Option<LocalInterface>,
        tcp_stun_servers: Vec<String>,
        udp_stun_servers: Vec<String>,
        local_udp_ports: Vec<u16>,
        local_tcp_port: u16,
    ) -> Self {
        let public_udp_ports = vec![0; local_udp_ports.len()];
        let nat_info = NatInfo {
            nat_type: Default::default(),
            public_ips: vec![],
            public_udp_ports,
            mapping_tcp_addr: vec![],
            mapping_udp_addr: vec![],
            public_port_range: 0,
            local_ipv4: Ipv4Addr::UNSPECIFIED,
            ipv6: None,
            local_udp_ports,
            local_tcp_port,
            public_tcp_port: 0,
        };
        Self {
            oneself_id,
            default_interface,
            tcp_stun_servers,
            udp_stun_servers,
            nat_info: Arc::new(Mutex::new(nat_info)),
        }
    }
    pub fn set_public_info(
        &self,
        nat_type: NatType,
        mut ips: Vec<Ipv4Addr>,
        public_port_range: u16,
    ) {
        ips.retain(rust_p2p_core::extend::addr::is_ipv4_global);
        let mut guard = self.nat_info.lock();
        guard.public_ips = ips;
        guard.nat_type = nat_type;
        guard.public_port_range = public_port_range;
    }
    fn mapping_addr(addr: SocketAddr) -> Option<(Ipv4Addr, u16)> {
        match addr {
            SocketAddr::V4(addr) => Some((*addr.ip(), addr.port())),
            SocketAddr::V6(addr) => addr.ip().to_ipv4_mapped().map(|ip| (ip, addr.port())),
        }
    }
    pub fn update_tcp_public_addr(&self, addr: SocketAddr) {
        let (ip, port) = if let Some(r) = Self::mapping_addr(addr) {
            r
        } else {
            return;
        };
        let mut nat_info = self.nat_info.lock();
        if rust_p2p_core::extend::addr::is_ipv4_global(&ip) && !nat_info.public_ips.contains(&ip) {
            nat_info.public_ips.push(ip);
        }
        nat_info.public_tcp_port = port;
    }
    pub fn update_public_addr(&self, index: Index, addr: SocketAddr) {
        let (ip, port) = if let Some(r) = Self::mapping_addr(addr) {
            r
        } else {
            return;
        };
        let mut nat_info = self.nat_info.lock();

        if rust_p2p_core::extend::addr::is_ipv4_global(&ip) {
            if !nat_info.public_ips.contains(&ip) {
                nat_info.public_ips.push(ip);
            }
            match index {
                Index::Udp(index) => {
                    let index = match index {
                        UDPIndex::MainV4(index) => index,
                        UDPIndex::MainV6(index) => index,
                        UDPIndex::SubV4(_) => return,
                    };
                    if let Some(p) = nat_info.public_udp_ports.get_mut(index) {
                        *p = port;
                    }
                }
                Index::Tcp(_) => {
                    nat_info.public_tcp_port = port;
                }
                _ => {}
            }
        } else {
            log::debug!("not public addr: {addr:?}")
        }
    }
    pub async fn update_local_addr(&self) {
        let local_ipv4 = rust_p2p_core::extend::addr::local_ipv4().await;
        let local_ipv6 = rust_p2p_core::extend::addr::local_ipv6().await;
        let mut nat_info = self.nat_info.lock();
        if let Ok(local_ipv4) = local_ipv4 {
            nat_info.local_ipv4 = local_ipv4;
        }
        nat_info.ipv6 = local_ipv6.ok();
    }
    pub async fn update_nat_info(&self) -> io::Result<NatInfo> {
        self.update_local_addr().await;
        let mut udp_stun_servers = self.udp_stun_servers.clone();
        udp_stun_servers.shuffle(&mut rand::thread_rng());
        let udp_stun_servers = if udp_stun_servers.len() > 3 {
            &udp_stun_servers[..3]
        } else {
            &udp_stun_servers
        };
        let (nat_type, ips, port_range) = rust_p2p_core::stun::stun_test_nat(
            udp_stun_servers.to_vec(),
            self.default_interface.as_ref(),
        )
        .await?;
        self.set_public_info(nat_type, ips, port_range);
        Ok(self.nat_info())
    }
    pub fn nat_info(&self) -> NatInfo {
        self.nat_info.lock().clone()
    }
}

pub struct Puncher {
    lock: tokio::sync::Mutex<()>,
    shutdown_manager: ShutdownManager<()>,
    punch_context: Arc<PunchContext>,
    puncher: CorePuncher,
    tunnel_router: TunnelRouter,
    kcp_context: KcpContext,
}
impl Drop for Puncher {
    fn drop(&mut self) {
        let _ = self.shutdown_manager.trigger_shutdown(());
    }
}
impl Puncher {
    fn new(
        shutdown_manager: ShutdownManager<()>,
        punch_context: Arc<PunchContext>,
        puncher: CorePuncher,
        tunnel_router: TunnelRouter,
        kcp_context: KcpContext,
    ) -> Self {
        Self {
            lock: Default::default(),
            shutdown_manager,
            punch_context,
            puncher,
            tunnel_router,
            kcp_context,
        }
    }
    /// Get current NAT information
    pub fn nat_info(&self) -> NatInfo {
        self.punch_context.nat_info()
    }
    /// Initiate a hole punching attempt to the remote peer
    pub async fn punch(&self, peer_id: &Arc<String>, punch_info: PunchInfo) -> io::Result<()> {
        let Ok(_lock) = self.lock.try_lock() else {
            // Do not repeat
            return Ok(());
        };
        if peer_id == &self.punch_context.oneself_id {
            return Err(Error::new(
                io::ErrorKind::Other,
                "Cannot connect to oneself",
            ));
        }
        if !self.puncher.need_punch(&punch_info) {
            return Ok(());
        }
        if self.tunnel_router.route_table.no_need_punch(peer_id) {
            return Ok(());
        }
        let packet = ping(&self.punch_context.oneself_id)?;
        self.puncher
            .punch_now(Some(packet.buffer()), packet.buffer(), punch_info)
            .await
    }
    /// Verify peer reachability (successful hole punching)
    pub fn is_reachable(&self, peer_id: &Arc<String>) -> bool {
        self.tunnel_router.route_table.route_one(peer_id).is_some()
    }
    /// Connects to the peer after checking reachability using [`Self::is_reachable`].
    pub fn connect(&self, peer_id: Arc<String>) -> io::Result<KcpStream> {
        if peer_id == self.punch_context.oneself_id {
            return Err(Error::new(
                io::ErrorKind::Other,
                "Cannot connect to oneself",
            ));
        }
        self.kcp_context
            .new_stream(self.tunnel_router.clone(), peer_id)
    }
}

pub async fn new_tunnel_component(oneself_id: String) -> io::Result<(Puncher, KcpStreamListener)> {
    if oneself_id.len() > 255 {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "oneself_id too long",
        ));
    }
    let oneself_id = Arc::new(oneself_id);
    let config = TunnelConfig::new(Box::new(LengthPrefixedInitCodec));
    let (unified_tunnel_factory, puncher) = rust_p2p_core::tunnel::new_tunnel_component(config)?;
    let route_table = RouteTable::new(LoadBalance::default());
    let idle_route_manager = IdleRouteManager::new(Duration::from_secs(5), route_table.clone());
    let shutdown_manager = ShutdownManager::new();
    let socket_manager = unified_tunnel_factory.socket_manager();
    let local_tcp_port = if let Some(v) = socket_manager.tcp_socket_manager_as_ref() {
        v.local_addr().port()
    } else {
        0
    };
    let local_udp_ports = if let Some(v) = socket_manager.udp_socket_manager_as_ref() {
        v.local_ports()?
    } else {
        vec![]
    };
    let punch_context = PunchContext::new(
        oneself_id.clone(),
        None,
        vec![
            "stun.flashdance.cx".to_string(),
            "stun.sipnet.net".to_string(),
            "stun.nextcloud.com:443".to_string(),
        ],
        vec![
            "stun.miwifi.com".to_string(),
            "stun.chat.bilibili.com".to_string(),
            "stun.hitv.com".to_string(),
            "stun.l.google.com:19302".to_string(),
            "stun1.l.google.com:19302".to_string(),
            "stun2.l.google.com:19302".to_string(),
        ],
        local_udp_ports,
        local_tcp_port,
    );
    let punch_context = Arc::new(punch_context);
    start_task(
        oneself_id,
        idle_route_manager,
        shutdown_manager.clone(),
        route_table.clone(),
        punch_context.clone(),
        socket_manager.clone(),
    );
    let tunnel_router = TunnelRouter::new(route_table.clone(), socket_manager);
    let (kcp_context, kcp_listener) = KcpStreamListener::new(tunnel_router.clone());

    tokio::spawn(tunnel::dispatch(
        shutdown_manager.clone(),
        unified_tunnel_factory,
        route_table,
        punch_context.clone(),
        kcp_context.clone(),
    ));
    punch_context.update_local_addr().await;
    let puncher = Puncher::new(
        shutdown_manager,
        punch_context,
        puncher,
        tunnel_router.clone(),
        kcp_context,
    );

    Ok((puncher, kcp_listener))
}
