use crate::punch::tunnel::TunnelRouter;
use bytes::{Buf, BytesMut};
use kcp::Kcp;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::io;
use std::io::{Error, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::Interval;
use tokio_util::sync::PollSender;

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
    peer_id: String,
    tunnel_router: TunnelRouter,
}
impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.tunnel_router.try_send_to(buf, &self.peer_id) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            rs => rs?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[derive(Clone)]
struct Counter {
    counter: Arc<AtomicU32>,
}
impl Counter {
    fn new(v: u32) -> Self {
        Self {
            counter: Arc::new(AtomicU32::new(v)),
        }
    }
    fn add(&self) -> u32 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}
#[derive(Clone)]
pub(crate) struct KcpContext {
    conv: Counter,
    map: Map,
    sender: Sender<(String, BytesMut)>,
}

impl KcpContext {
    fn new(sender: Sender<(String, BytesMut)>) -> Self {
        Self {
            conv: Counter::new(rand::random()),
            map: Arc::new(Default::default()),
            sender,
        }
    }
    pub(crate) async fn input(&self, buf: &[u8], peer_id: String) {
        if buf.is_empty() {
            return;
        }
        let conv = kcp::get_conv(buf);
        let key = (peer_id, conv);
        let option = self.map.read().get(&key).cloned();
        if let Some(v) = option {
            if v.send(buf.into()).await.is_ok() {
                return;
            }
        }

        if self.sender.send((key.0, buf.into())).await.is_err() {
            log::warn!("input error");
        }
    }
    pub(crate) fn new_stream(
        &self,
        output: TunnelRouter,
        peer_id: String,
    ) -> io::Result<KcpStream> {
        KcpStream::new(peer_id, self.conv.add(), self.map.clone(), output)
    }
}

type Map = Arc<RwLock<HashMap<(String, u32), Sender<BytesMut>>>>;

pub struct KcpStream {
    peer_id: String,
    conv: u32,
    read: KcpStreamRead,
    write: KcpStreamWrite,
}
impl KcpStream {
    pub(crate) fn new(
        peer_id: String,
        conv: u32,
        map: Map,
        tunnel_router: TunnelRouter,
    ) -> io::Result<Self> {
        let mut guard = map.write();
        let key = (peer_id, conv);
        if guard.contains_key(&key) {
            return Err(Error::new(
                io::ErrorKind::AlreadyExists,
                "stream already exists",
            ));
        }
        let (input_sender, input_receiver) = tokio::sync::mpsc::channel(128);
        guard.insert(key.clone(), input_sender);
        drop(guard);
        let owned_kcp = OwnedKcp {
            map,
            key: key.clone(),
        };
        let (peer_id, conv) = key;
        let stream = KcpStream::new_stream(peer_id, conv, owned_kcp, input_receiver, tunnel_router);
        Ok(stream)
    }
    fn new_stream(
        peer_id: String,
        conv: u32,
        owned_kcp: OwnedKcp,
        input: Receiver<BytesMut>,
        tunnel_router: TunnelRouter,
    ) -> Self {
        let mut kcp = Kcp::new_stream(
            conv,
            KcpOutput {
                peer_id: peer_id.clone(),
                tunnel_router,
            },
        );
        kcp.set_wndsize(128, 128);
        kcp.set_wndsize(128, 128);
        kcp.set_nodelay(true, 10, 2, true);
        let (data_in_sender, data_in_receiver) = tokio::sync::mpsc::channel(128);
        let (data_out_sender, data_out_receiver) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            if let Err(e) = kcp_run(input, kcp, data_out_receiver, data_in_sender).await {
                log::warn!("kcp run: {e:?}");
            }
        });
        let owned_kcp = Arc::new(owned_kcp);
        let read = KcpStreamRead {
            owned_kcp: owned_kcp.clone(),
            last_buf: None,
            receiver: data_in_receiver,
        };
        let write = KcpStreamWrite {
            owned_kcp,
            sender: PollSender::new(data_out_sender),
        };
        KcpStream {
            peer_id,
            conv,
            read,
            write,
        }
    }
}
struct OwnedKcp {
    map: Map,
    key: (String, u32),
}
impl Drop for OwnedKcp {
    fn drop(&mut self) {
        let mut guard = self.map.write();
        let _ = guard.remove(&self.key);
    }
}

pub struct KcpStreamRead {
    #[allow(dead_code)]
    owned_kcp: Arc<OwnedKcp>,
    last_buf: Option<BytesMut>,
    receiver: Receiver<BytesMut>,
}
pub struct KcpStreamWrite {
    #[allow(dead_code)]
    owned_kcp: Arc<OwnedKcp>,
    sender: PollSender<BytesMut>,
}

impl AsyncRead for KcpStreamRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(p) = self.last_buf.as_mut() {
            let len = buf.remaining().min(p.len());
            buf.put_slice(&p[..len]);
            p.advance(len);
            if p.is_empty() {
                self.last_buf.take();
            }
            return Poll::Ready(Ok(()));
        }
        let poll = self.receiver.poll_recv(cx);
        match poll {
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Ready(Some(mut p)) => {
                if p.is_empty() {
                    self.receiver.close();
                    return Poll::Ready(Ok(()));
                }
                let len = buf.remaining().min(p.len());
                buf.put_slice(&p[..len]);
                p.advance(len);
                if !p.is_empty() {
                    self.last_buf.replace(p);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
impl AsyncWrite for KcpStreamWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        match self.sender.poll_reserve(cx) {
            Poll::Ready(Ok(_)) => {
                let len = buf.len().min(10240);
                match self.sender.send_item(buf[..len].into()) {
                    Ok(_) => Poll::Ready(Ok(len)),
                    Err(_) => Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero))),
                }
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.sender.close();
        Poll::Ready(Ok(()))
    }
}
impl KcpStream {
    pub fn split(self) -> (KcpStreamWrite, KcpStreamRead) {
        (self.write, self.read)
    }
    pub fn conv(&self) -> u32 {
        self.conv
    }
    pub fn peer_id(&self) -> &String {
        &self.peer_id
    }
}

pub struct KcpStreamListener {
    input_receiver: Receiver<(String, BytesMut)>,
    map: Map,
    output: TunnelRouter,
}
impl KcpStreamListener {
    pub(crate) fn new(output: TunnelRouter) -> (KcpContext, Self) {
        let (sender, input_receiver) = tokio::sync::mpsc::channel(128);
        let kcp_context = KcpContext::new(sender);
        let listener = KcpStreamListener {
            input_receiver,
            map: kcp_context.map.clone(),
            output,
        };
        (kcp_context, listener)
    }
    pub async fn accept(&mut self) -> io::Result<(KcpStream, String)> {
        loop {
            let (peer_id, bytes) = self
                .input_receiver
                .recv()
                .await
                .ok_or_else(|| io::Error::from(io::ErrorKind::Other))?;
            if bytes.is_empty() {
                continue;
            }
            let conv = kcp::get_conv(&bytes);
            match self.new_stream_impl(peer_id.clone(), conv) {
                Ok(stream) => {
                    self.send_data_to_kcp(peer_id.clone(), conv, bytes).await?;
                    return Ok((stream, peer_id));
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        self.send_data_to_kcp(peer_id, conv, bytes).await?;
                    }
                }
            }
        }
    }
    fn new_stream_impl(&self, peer_id: String, conv: u32) -> io::Result<KcpStream> {
        KcpStream::new(peer_id, conv, self.map.clone(), self.output.clone())
    }
    async fn send_data_to_kcp(
        &self,
        peer_id: String,
        conv: u32,
        bytes_mut: BytesMut,
    ) -> io::Result<()> {
        let sender = self.get_stream_sender(peer_id, conv)?;
        sender
            .send(bytes_mut)
            .await
            .map_err(|_| Error::new(io::ErrorKind::NotFound, "not found stream"))
    }
    fn get_stream_sender(&self, peer_id: String, conv: u32) -> io::Result<Sender<BytesMut>> {
        if let Some(v) = self.map.read().get(&(peer_id, conv)) {
            Ok(v.clone())
        } else {
            Err(Error::new(io::ErrorKind::NotFound, "not found stream"))
        }
    }
}
