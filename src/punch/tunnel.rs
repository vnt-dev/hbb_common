use std::io;
use std::time::UNIX_EPOCH;
use bytes::{BufMut, BytesMut};
use rust_p2p_core::route::route_table::RouteTable;
use crate::punch::protocol::{ping, NetPacket, ProtocolType, HEAD_LEN};

#[derive(Clone)]
pub struct TunnelRouter {
    pub(crate) route_table: RouteTable<String>,
    pub(crate) socket_manager: rust_p2p_core::tunnel::SocketManager,
}

impl TunnelRouter {
    pub fn ping(&self, dest: &String) -> io::Result<()> {
        let route = self.route_table.get_route_by_id(dest)?;
        let packet = ping(dest)?;
        self.socket_manager.try_send_to(packet.into_buffer(), &route.route_key())
    }
    pub fn try_send_to(&self, buf: &[u8], dest: &String) -> io::Result<()> {
        let route = self.route_table.get_route_by_id(dest)?;
        let bytes_mut = BytesMut::zeroed(HEAD_LEN + buf.len());
        let mut packet = NetPacket::new(bytes_mut)?;
        packet.set_protocol(ProtocolType::Raw);
        packet.reset_data_len();
        self.socket_manager.try_send_to(packet.into_buffer(), &route.route_key())
    }
}

