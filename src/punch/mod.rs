use bytes::{Buf, BytesMut};
use kcp::Kcp;
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rust_p2p_core::nat::{NatInfo, NatType};
use rust_p2p_core::punch::{PunchInfo, Puncher as CorePuncher};
use rust_p2p_core::route::route_table::RouteTable;
use rust_p2p_core::route::Index;
use rust_p2p_core::socket::LocalInterface;
use rust_p2p_core::tunnel::udp::{UDPIndex, WeakUdpTunnelSender};
use rust_p2p_core::tunnel::SocketManager;
use std::collections::HashMap;
use std::io;
use std::io::{Error, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Interval;
use tokio_util::sync::PollSender;
use crate::punch::protocol::ping;
use crate::punch::tunnel::TunnelRouter;

mod tunnel;
mod protocol;

pub(crate) struct PunchContext {
    default_interface: Option<LocalInterface>,
    tcp_stun_servers: Vec<String>,
    udp_stun_servers: Vec<String>,
    nat_info: Arc<Mutex<NatInfo>>,
}
impl PunchContext {
    pub fn new(
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

#[derive(Clone)]
pub struct Puncher {
    punch_context: Arc<PunchContext>,
    puncher: CorePuncher,
    tunnel_router: TunnelRouter,
}
impl Puncher {
    async fn new(
        default_interface: Option<LocalInterface>,
        tcp_stun_servers: Vec<String>,
        udp_stun_servers: Vec<String>,
        puncher: CorePuncher,
        tunnel_router: TunnelRouter,
    ) -> io::Result<Self> {
        let socket_manager = &tunnel_router.socket_manager;
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
        let punch_context = Arc::new(PunchContext::new(
            default_interface,
            tcp_stun_servers,
            udp_stun_servers,
            local_udp_ports,
            local_tcp_port,
        ));
        punch_context.update_local_addr().await;
        Ok(Self {
            punch_context,
            puncher,
            tunnel_router,
        })
    }

    pub async fn punch_conv(&self, peer_id: &String, punch_info: PunchInfo) -> io::Result<()> {

        if !self.puncher.need_punch(&punch_info) {
            return Ok(());
        }
        if self.tunnel_router.route_table.no_need_punch(peer_id){
            return Ok(());
        }
        let packet = ping(peer_id)?;
        self.puncher
            .punch_now(Some(packet.buffer()), packet.buffer(), punch_info)
            .await
    }
    pub fn nat_info(&self) -> NatInfo {
        self.punch_context.nat_info()
    }
    pub fn connect(&self, peer_id: &String) -> KcpStream {
        todo!()
    }
}
type Map = Arc<RwLock<HashMap<(u32, u32), Sender<BytesMut>>>>;
pub struct KcpStream {
    peer_id: u32,
    route_table: RouteTable<u32>,
    sender: PollSender<BytesMut>,
    receiver: Receiver<BytesMut>,
}

impl KcpStream {
    pub(crate) fn new(
        peer_id: u32,
        conv: u32,
        map: Map,
        output_sender: Sender<(u32, BytesMut)>,
    ) -> io::Result<Self> {
        // let mut guard = map.write();
        // if guard.contains_key(&(node_id, conv)) {
        //     return Err(Error::new(
        //         io::ErrorKind::AlreadyExists,
        //         "stream already exists",
        //     ));
        // }
        // let (input_sender, input_receiver) = tokio::sync::mpsc::channel(128);
        // guard.insert((node_id, conv), input_sender);
        // drop(guard);
        // let stream = KcpStream::new_stream(node_id, conv, map, input_receiver, output_sender);
        // Ok(stream)
        todo!()
    }
}
impl KcpStream {
    pub fn is_reachable(&self) -> bool {
        self.route_table.route_one(&self.peer_id).is_some()
    }
}

async fn kcp_run(
    mut input: Receiver<BytesMut>,
    mut kcp: Kcp<KcpOutput>,
    mut data_out_receiver: Receiver<BytesMut>,
    data_in_sender: Sender<BytesMut>,
) -> io::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    let mut buf = [0; 65536];
    let mut input_data = Option::<BytesMut>::None;

    loop {
        let event = if kcp.wait_snd() >= kcp.snd_wnd() as usize {
            input_event(&mut input, &mut interval).await?
        } else if input_data.is_some() {
            output_event(&mut data_out_receiver, &mut interval).await?
        } else {
            all_event(&mut input, &mut data_out_receiver, &mut interval).await?
        };
        if let Some(mut buf) = input_data.take() {
            let len = kcp
                .input(&buf)
                .map_err(|e| Error::new(io::ErrorKind::Other, e))?;
            if len < buf.len() {
                buf.advance(len);
                input_data.replace(buf);
            }
        }
        match event {
            Event::Input(mut buf) => {
                let len = kcp
                    .input(&buf)
                    .map_err(|e| Error::new(io::ErrorKind::Other, e))?;
                if len < buf.len() {
                    buf.advance(len);
                    input_data.replace(buf);
                }
            }
            Event::Output(buf) => {
                kcp.send(&buf)
                    .map_err(|e| Error::new(io::ErrorKind::Other, e))?;
            }
            Event::Timeout => {
                let now = std::time::SystemTime::now();
                let millis = now
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                kcp.update(millis.as_millis() as _)
                    .map_err(|e| Error::new(io::ErrorKind::Other, e))?;
            }
        }

        if let Ok(len) = kcp.recv(&mut buf) {
            if data_in_sender.send(buf[..len].into()).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}
async fn all_event(
    input: &mut Receiver<BytesMut>,
    data_out_receiver: &mut Receiver<BytesMut>,
    interval: &mut Interval,
) -> io::Result<Event> {
    tokio::select! {
        rs=input.recv()=>{
            let buf = rs.ok_or(Error::new(io::ErrorKind::Other, "input close"))?;
            Ok(Event::Input(buf))
        }
        rs=data_out_receiver.recv()=>{
            let buf = rs.ok_or(Error::new(io::ErrorKind::Other, "output close"))?;
            Ok(Event::Output(buf))
        }
        _=interval.tick()=>{
            Ok(Event::Timeout)
        }
    }
}
async fn input_event(input: &mut Receiver<BytesMut>, interval: &mut Interval) -> io::Result<Event> {
    tokio::select! {
        rs=input.recv()=>{
            let buf = rs.ok_or(Error::new(io::ErrorKind::Other, "input close"))?;
            Ok(Event::Input(buf))
        }
        _=interval.tick()=>{
            Ok(Event::Timeout)
        }
    }
}
async fn output_event(
    data_out_receiver: &mut Receiver<BytesMut>,
    interval: &mut Interval,
) -> io::Result<Event> {
    tokio::select! {
        rs=data_out_receiver.recv()=>{
            let buf = rs.ok_or(Error::new(io::ErrorKind::Other, "output close"))?;
            Ok(Event::Output(buf))
        }
        _=interval.tick()=>{
            Ok(Event::Timeout)
        }
    }
}
#[derive(Debug)]
enum Event {
    Output(BytesMut),
    Input(BytesMut),
    Timeout,
}

struct KcpOutput {
    peer_id: u32,
    sender: Sender<(u32, BytesMut)>,
}
impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(e) = self.sender.try_send((self.peer_id, buf.into())) {
            match e {
                TrySendError::Full(_) => {}
                TrySendError::Closed(_) => return Err(Error::from(io::ErrorKind::WriteZero)),
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub async fn new_tunnel_component() {}
