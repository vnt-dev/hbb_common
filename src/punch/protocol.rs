use async_trait::async_trait;
use bytes::{Buf, BufMut, BytesMut};
use rust_p2p_core::tunnel::tcp::{Decoder, Encoder, InitCodec};
use std::convert::{TryFrom, TryInto};
use std::io;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ProtocolType {
    Ping = 1,
    Pong = 2,
    KcpRaw = 3,
}
impl TryFrom<u8> for ProtocolType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        const MAX: u8 = ProtocolType::KcpRaw as u8;
        match value {
            1..=MAX => unsafe { Ok(std::mem::transmute::<u8, ProtocolType>(value)) },
            val => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid protocol:{val}"),
            )),
        }
    }
}

impl Into<u8> for ProtocolType {
    fn into(self) -> u8 {
        match self {
            ProtocolType::Ping => 1,
            ProtocolType::Pong => 2,
            ProtocolType::KcpRaw => 3,
        }
    }
}

/*

   0                                            15                                              31
   0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5  6  7  8  9  0  1
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  |1|   protocol(7)       |                 data len(16)               |       reserve(8)       |
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  |   src ID len(8)       |                  src ID(n)                                          |
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  |                                       timestamp(32)                                         |
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  protocol = ProtocolType::Ping or ProtocolType::Pong
*/

/*
   0                                            15                                              31
   0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5  6  7  8  9  0  1
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  |1|   protocol(7)       |                 data len(16)               |       reserve(8)       |
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  |                                        payload(n)                                           |
  +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
  protocol = ProtocolType::Raw
*/
pub const HEAD_LEN: usize = 4;
pub struct NetPacket<B> {
    buffer: B,
}
impl<B: AsRef<[u8]>> NetPacket<B> {
    pub fn new_unchecked(buffer: B) -> NetPacket<B> {
        Self { buffer }
    }
    pub fn new(buffer: B) -> io::Result<NetPacket<B>> {
        let len = buffer.as_ref().len();
        if len < HEAD_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too short"));
        }
        let packet = Self::new_unchecked(buffer);
        if packet.data_length() as usize != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "packet len invalid",
            ));
        }
        Ok(packet)
    }
    pub fn protocol(&self) -> io::Result<ProtocolType> {
        (self.buffer.as_ref()[0] & 0x7F).try_into()
    }
    pub fn data_length(&self) -> u16 {
        ((self.buffer.as_ref()[1] as u16) << 8) | self.buffer.as_ref()[2] as u16
    }
    pub fn buffer(&self) -> &[u8] {
        self.buffer.as_ref()
    }
    pub fn into_buffer(self) -> B {
        self.buffer
    }
    pub fn payload(&self) -> &[u8] {
        &self.buffer.as_ref()[HEAD_LEN..]
    }
}
impl<B: AsRef<[u8]> + AsMut<[u8]>> NetPacket<B> {
    pub(crate) fn set_high_flag(&mut self) {
        self.buffer.as_mut()[0] |= 0x80
    }
    pub fn set_protocol(&mut self, protocol_type: ProtocolType) {
        self.buffer.as_mut()[0] = protocol_type.into();
        self.set_high_flag()
    }
    pub fn reset_data_len(&mut self) {
        let len = self.buffer().len();
        self.buffer.as_mut()[1] = (len >> 8) as u8;
        self.buffer.as_mut()[2] = len as u8;
    }
}
pub fn now() -> io::Result<u32> {
    let time = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_e| io::Error::new(io::ErrorKind::Other, "system time error"))?
        .as_millis() as u32;
    Ok(time)
}
pub fn ping(self_id: &String) -> io::Result<NetPacket<BytesMut>> {
    ping_pong_time(ProtocolType::Ping, self_id, now()?)
}
pub fn pong(self_id: &str, time: u32) -> io::Result<NetPacket<BytesMut>> {
    ping_pong_time(ProtocolType::Pong, self_id, time)
}
pub fn ping_pong_time(
    protocol_type: ProtocolType,
    self_id: &str,
    time: u32,
) -> io::Result<NetPacket<BytesMut>> {
    let bytes = self_id.as_bytes();
    if bytes.len() > 255 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too long"));
    }
    let mut bytes_mut = BytesMut::with_capacity(HEAD_LEN + 1 + bytes.len() + 4);
    bytes_mut.resize(HEAD_LEN, 0);
    bytes_mut.put_u8(bytes.len() as u8);
    bytes_mut.put_slice(bytes);
    bytes_mut.put_u32(time);
    let mut packet = NetPacket::new(bytes_mut)?;
    packet.set_protocol(protocol_type);
    packet.reset_data_len();
    Ok(packet)
}
pub fn convert_ping_pong(payload: &[u8]) -> io::Result<(&str, u32)> {
    if payload.len() < 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too short"));
    }
    let end_index = payload[0] as usize + 1;
    if end_index + 4 != payload.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data"));
    }
    match std::str::from_utf8(&payload[1..end_index]) {
        Ok(str) => {
            let time = u32::from_be_bytes(payload[end_index..].try_into().unwrap());
            Ok((str, time))
        }
        Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "invalid data")),
    }
}

pub struct LengthPrefixedEncoder {}

pub struct LengthPrefixedDecoder {
    buf: BytesMut,
}
impl LengthPrefixedEncoder {
    pub fn new() -> Self {
        Self {}
    }
}

impl LengthPrefixedDecoder {
    pub fn new() -> Self {
        Self {
            buf: Default::default(),
        }
    }
}
#[async_trait]
impl Decoder for LengthPrefixedDecoder {
    async fn decode(&mut self, read: &mut OwnedReadHalf, src: &mut [u8]) -> io::Result<usize> {
        if src.len() < HEAD_LEN {
            return Err(io::Error::new(io::ErrorKind::Other, "too short"));
        }
        let mut offset = 0;
        loop {
            if self.buf.is_empty() {
                let len = read.read(&mut src[offset..]).await?;
                offset += len;
                if let Some(rs) = self.process_packet(src, offset) {
                    return rs;
                }
            } else if let Some(rs) = self.process_buf(src, &mut offset) {
                return rs;
            }
        }
    }

    fn try_decode(&mut self, read: &mut OwnedReadHalf, src: &mut [u8]) -> io::Result<usize> {
        if src.len() < HEAD_LEN {
            return Err(io::Error::new(io::ErrorKind::Other, "too short"));
        }
        let mut offset = 0;
        loop {
            if self.buf.is_empty() {
                match read.try_read(&mut src[offset..]) {
                    Ok(len) => {
                        offset += len;
                    }
                    Err(e) => {
                        if e.kind() == io::ErrorKind::WouldBlock && offset > 0 {
                            self.buf.extend_from_slice(&src[..offset]);
                        }
                        return Err(e);
                    }
                }
                if let Some(rs) = self.process_packet(src, offset) {
                    return rs;
                }
            } else if let Some(rs) = self.process_buf(src, &mut offset) {
                return rs;
            }
        }
    }
}
impl LengthPrefixedDecoder {
    fn process_buf(&mut self, src: &mut [u8], offset: &mut usize) -> Option<io::Result<usize>> {
        let len = self.buf.len();
        if len < HEAD_LEN {
            src[..len].copy_from_slice(self.buf.as_ref());
            *offset += len;
            self.buf.clear();
            return None;
        }
        let packet = NetPacket::new_unchecked(self.buf.as_ref());
        let data_length = packet.data_length() as usize;
        if data_length > src.len() {
            return Some(Err(io::Error::new(io::ErrorKind::Other, "too short")));
        }
        if data_length > len {
            src[..len].copy_from_slice(self.buf.as_ref());
            *offset += len;
            self.buf.clear();
            None
        } else {
            src[..data_length].copy_from_slice(&self.buf[..data_length]);
            if data_length == len {
                self.buf.clear();
            } else {
                self.buf.advance(data_length);
            }
            Some(Ok(data_length))
        }
    }
    fn process_packet(&mut self, src: &mut [u8], offset: usize) -> Option<io::Result<usize>> {
        if offset < HEAD_LEN {
            return None;
        }
        let packet = NetPacket::new_unchecked(&src);
        let data_length = packet.data_length() as usize;
        if data_length > src.len() {
            return Some(Err(io::Error::new(io::ErrorKind::Other, "too short")));
        }
        match data_length.cmp(&offset) {
            std::cmp::Ordering::Less => {
                self.buf.extend_from_slice(&src[data_length..offset]);
                Some(Ok(data_length))
            }
            std::cmp::Ordering::Equal => Some(Ok(data_length)),
            std::cmp::Ordering::Greater => None,
        }
    }
}
#[async_trait]
impl Encoder for LengthPrefixedEncoder {
    async fn encode(&mut self, write: &mut OwnedWriteHalf, data: &[u8]) -> io::Result<()> {
        let len = data.len();
        let packet = NetPacket::new_unchecked(data);
        if packet.data_length() as usize != len {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        write.write_all(data).await
    }

    async fn encode_multiple(
        &mut self,
        write: &mut OwnedWriteHalf,
        bufs: &[IoSlice<'_>],
    ) -> io::Result<()> {
        let mut index = 0;
        let mut total_written = 0;
        let total: usize = bufs.iter().map(|v| v.len()).sum();
        loop {
            let len = write.write_vectored(&bufs[index..]).await?;
            if len == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            total_written += len;
            if total_written == total {
                return Ok(());
            }
            let mut written = len;
            for buf in &bufs[index..] {
                if buf.len() > written {
                    if written != 0 {
                        index += 1;
                        total_written += buf.len() - written;
                        write.write_all(&buf[written..]).await?;
                        if index == bufs.len() {
                            return Ok(());
                        }
                    }
                    if index == bufs.len() - 1 {
                        write.write_all(buf).await?;
                        return Ok(());
                    }
                    break;
                } else {
                    index += 1;
                    written -= buf.len();
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct LengthPrefixedInitCodec;

impl InitCodec for LengthPrefixedInitCodec {
    fn codec(&self, _addr: SocketAddr) -> io::Result<(Box<dyn Decoder>, Box<dyn Encoder>)> {
        Ok((
            Box::new(LengthPrefixedDecoder::new()),
            Box::new(LengthPrefixedEncoder::new()),
        ))
    }
}
