use crate::punch::kcp_stream::KcpContext;
use crate::punch::protocol::{convert_ping_pong, now, pong, NetPacket, ProtocolType, HEAD_LEN};
use crate::punch::PunchContext;
use async_shutdown::ShutdownManager;
use bytes::BytesMut;
use rust_p2p_core::route::route_table::{Route, RouteTable};
use rust_p2p_core::route::RouteKey;
use rust_p2p_core::tunnel::{SocketManager, Tunnel, TunnelDispatcher};
use std::io;
use std::io::Error;
use std::sync::Arc;

#[derive(Clone)]
pub struct TunnelRouter {
    pub(crate) route_table: RouteTable<Arc<String>>,
    pub(crate) socket_manager: SocketManager,
}

impl TunnelRouter {
    pub(crate) fn new(route_table: RouteTable<Arc<String>>, socket_manager: SocketManager) -> Self {
        Self {
            route_table,
            socket_manager,
        }
    }

    pub fn try_send_to(&self, buf: &[u8], dest: &Arc<String>) -> io::Result<()> {
        let route = self.route_table.get_route_by_id(dest)?;
        let bytes_mut = BytesMut::zeroed(HEAD_LEN + buf.len());
        let mut packet = NetPacket::new(bytes_mut)?;
        packet.set_protocol(ProtocolType::KcpRaw);
        packet.reset_data_len();
        self.socket_manager
            .try_send_to(packet.into_buffer(), &route.route_key())
    }
    #[allow(dead_code)]
    pub async fn send_to(&self, buf: &[u8], dest: &Arc<String>) -> io::Result<()> {
        let route = self.route_table.get_route_by_id(dest)?;
        let bytes_mut = BytesMut::zeroed(HEAD_LEN + buf.len());
        let mut packet = NetPacket::new(bytes_mut)?;
        packet.set_protocol(ProtocolType::KcpRaw);
        packet.reset_data_len();
        self.socket_manager
            .send_to(packet.into_buffer(), &route.route_key())
            .await
    }
}
pub async fn dispatch(
    shutdown_manager: ShutdownManager<()>,
    mut tunnel_dispatcher: TunnelDispatcher,
    route_table: RouteTable<Arc<String>>,
    punch_context: Arc<PunchContext>,
    kcp_context: KcpContext,
) {
    loop {
        if shutdown_manager.is_shutdown_triggered() {
            return;
        }
        let Ok(tunnel) = shutdown_manager
            .wrap_cancel(tunnel_dispatcher.dispatch())
            .await
        else {
            return;
        };
        let tunnel = match tunnel {
            Ok(tunnel) => tunnel,
            Err(e) => {
                log::warn!("tunnel:{e:?}");
                return;
            }
        };
        tokio::spawn(tunnel_handle(
            shutdown_manager.clone(),
            tunnel,
            route_table.clone(),
            punch_context.clone(),
            kcp_context.clone(),
        ));
    }
}
async fn tunnel_handle(
    shutdown_manager: ShutdownManager<()>,
    mut tunnel: Tunnel,
    route_table: RouteTable<Arc<String>>,
    punch_context: Arc<PunchContext>,
    kcp_context: KcpContext,
) {
    const BUF_SIZE: usize = 16;
    let mut bufs = Vec::with_capacity(BUF_SIZE);
    let mut sizes = vec![0; BUF_SIZE];
    let mut addrs = vec![RouteKey::default(); BUF_SIZE];
    while bufs.len() < BUF_SIZE {
        bufs.push(BytesMut::zeroed(65536));
    }
    loop {
        let Ok(result) = shutdown_manager
            .wrap_cancel(tunnel.batch_recv_from(&mut bufs, &mut sizes, &mut addrs))
            .await
        else {
            return;
        };
        let Some(result) = result else {
            return;
        };
        let num = match result {
            Ok(len) => len,
            Err(e) => {
                log::debug!(
                    "batch_recv_from {e:?},{:?} {:?}",
                    tunnel.protocol(),
                    tunnel.remote_addr()
                );
                return;
            }
        };
        for index in 0..num {
            let len = sizes[index];
            let route_key = std::mem::take(&mut addrs[index]);
            if let Err(e) = data_handle(
                &tunnel,
                &route_table,
                &punch_context,
                &bufs[index][..len],
                route_key,
                &kcp_context,
            )
            .await
            {
                log::warn!("data_handle route_key={route_key:?},{e:?}");
            }
        }
    }
}
async fn data_handle(
    tunnel: &Tunnel,
    route_table: &RouteTable<Arc<String>>,
    punch_context: &PunchContext,
    buf: &[u8],
    route_key: RouteKey,
    kcp_context: &KcpContext,
) -> io::Result<()> {
    if rust_p2p_core::stun::is_stun_response(buf) {
        if let Some(pub_addr) = rust_p2p_core::stun::recv_stun_response(buf) {
            punch_context.update_public_addr(route_key.index(), pub_addr);
            return Ok(());
        }
    }
    let packet = NetPacket::new(buf)?;
    let protocol_type = packet.protocol()?;
    match protocol_type {
        ProtocolType::Ping => {
            let (peer_id, time) = convert_ping_pong(packet.payload())?;
            if peer_id == punch_context.oneself_id.as_str() {
                return Err(Error::new(
                    io::ErrorKind::Other,
                    "Cannot connect to oneself",
                ));
            }
            route_table.add_route_if_absent(
                Arc::new(peer_id.to_string()),
                Route::from_default_rt(route_key, 0),
            );
            let packet = pong(&punch_context.oneself_id, time)?;
            tunnel
                .send_to(packet.into_buffer(), route_key.addr())
                .await?;
        }
        ProtocolType::Pong => {
            let (peer_id, time) = convert_ping_pong(packet.payload())?;
            let now = now()?;
            route_table.add_route(
                Arc::new(peer_id.to_string()),
                Route::from(route_key, 0, now.saturating_sub(time)),
            );
        }
        ProtocolType::KcpRaw => {
            if let Some(peer_id) = route_table.get_id_by_route_key(&route_key) {
                route_table
                    .add_route_if_absent(peer_id.clone(), Route::from_default_rt(route_key, 0));
                kcp_context.input(packet.payload(), peer_id).await;
            }
        }
    }
    Ok(())
}
