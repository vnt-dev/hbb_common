mod kcp_stream;
mod maintain;
mod protocol;
mod punch;
mod tunnel;

use crate::bytes_codec::BytesCodec;
use crate::config::Socks5Server;
use crate::tcp::DynTcpStream;
use crate::{tcp, ResultType};
use bytes::Bytes;
pub use kcp_stream::*;
pub use punch::*;
use sodiumoxide::crypto::secretbox::Key;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::ToSocketAddrs;
use tokio_socks::IntoTargetAddr;
use tokio_util::codec::Framed;

pub struct ClientFramedStream {
    peer_id: Arc<String>,
    puncher: Puncher,
    // Channel relayed from the server
    relay_framed: tcp::FramedStream,
    // directly connected KCP channel and reuse the FramedStream structure
    kcp_framed: Option<tcp::FramedStream>,
}
impl ClientFramedStream {
    pub async fn new<T: ToSocketAddrs + std::fmt::Display>(
        puncher: Puncher,
        peer_id: Arc<String>,
        remote_addr: T,
        local_addr: Option<SocketAddr>,
        ms_timeout: u64,
    ) -> ResultType<Self> {
        let relay_framed = tcp::FramedStream::new(remote_addr, local_addr, ms_timeout).await?;
        Ok(Self {
            peer_id,
            puncher,
            relay_framed,
            kcp_framed: None,
        })
    }
    pub async fn connect<'t, T>(
        puncher: Puncher,
        peer_id: Arc<String>,
        target: T,
        local_addr: Option<SocketAddr>,
        proxy_conf: &Socks5Server,
        ms_timeout: u64,
    ) -> ResultType<Self>
    where
        T: IntoTargetAddr<'t>,
    {
        let relay_framed =
            tcp::FramedStream::connect(target, local_addr, proxy_conf, ms_timeout).await?;
        Ok(Self {
            peer_id,
            puncher,
            relay_framed,
            kcp_framed: None,
        })
    }
    fn try_create_kcp_stream(&mut self) {
        if self.kcp_framed.is_some() {
            return;
        }
        if self.puncher.is_reachable(&self.peer_id) {
            if let Ok(stream) = self.puncher.connect(self.peer_id.clone()) {
                self.kcp_framed.replace(tcp::FramedStream(
                    Framed::new(DynTcpStream(Box::new(stream)), BytesCodec::new()),
                    self.relay_framed.local_addr(),
                    None,
                    0,
                ));
            }
        }
    }
}
impl ClientFramedStream {
    pub fn set_send_timeout(&mut self, ms: u64) {
        self.relay_framed.set_send_timeout(ms);
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            kcp_framed.set_send_timeout(ms);
        }
    }
    pub fn set_raw(&mut self) {
        self.relay_framed.set_raw();
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            kcp_framed.set_raw();
        }
    }
    pub async fn send_bytes(&mut self, bytes: Bytes) -> ResultType<()> {
        self.try_create_kcp_stream();
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            if self.puncher.is_reachable(&self.peer_id)
                && kcp_framed.send_bytes(bytes.clone()).await.is_ok()
            {
                return Ok(());
            }
            self.kcp_framed.take();
        }
        self.relay_framed.send_bytes(bytes).await
    }
    pub async fn send_raw(&mut self, msg: Vec<u8>) -> ResultType<()> {
        self.try_create_kcp_stream();
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            if self.puncher.is_reachable(&self.peer_id)
                && kcp_framed.send_raw(msg.clone()).await.is_ok()
            {
                return Ok(());
            }
            self.kcp_framed.take();
        }
        self.relay_framed.send_raw(msg).await
    }
    pub fn set_key(&mut self, key: Key) {
        self.relay_framed.set_key(key.clone());
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            kcp_framed.set_key(key);
        }
    }
    pub fn is_secured(&self) -> bool {
        self.relay_framed.is_secured()
    }
    pub async fn next_timeout(
        &mut self,
        timeout: u64,
    ) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        loop {
            if let Some(kcp_framed) = self.kcp_framed.as_mut() {
                tokio::select! {
                    rs=kcp_framed.next_timeout(timeout)=>{
                        if let Some(Ok(rs)) = rs{
                            return Some(Ok(rs));
                        }
                        self.kcp_framed.take();
                    }
                    rs=self.relay_framed.next_timeout(timeout)=>{
                        return rs;
                    }
                }
            }
        }
    }
    pub async fn send(&mut self, msg: &impl protobuf::Message) -> ResultType<()> {
        self.try_create_kcp_stream();
        if let Some(kcp_framed) = self.kcp_framed.as_mut() {
            if self.puncher.is_reachable(&self.peer_id) && kcp_framed.send(msg).await.is_ok() {
                return Ok(());
            }
            self.kcp_framed.take();
        }
        self.relay_framed.send(msg).await
    }
    pub async fn next(&mut self) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        loop {
            if let Some(kcp_framed) = self.kcp_framed.as_mut() {
                tokio::select! {
                    rs=kcp_framed.next()=>{
                        if let Some(Ok(rs)) = rs{
                            return Some(Ok(rs));
                        }
                        self.kcp_framed.take();
                    }
                    rs=self.relay_framed.next()=>{
                        return rs;
                    }
                }
            }
        }
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.relay_framed.local_addr()
    }
}
