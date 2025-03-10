// Note: for StreamSocket, the client uses a server socket, the server uses a client socket.
// This is because of certificate management. The server needs to trust a client and its certificate
//
// StreamSender and StreamReceiver endpoints allow for convenient conversion of the header to/from
// bytes while still handling the additional byte buffer with zero copies and extra allocations.

mod tcp;
mod udp;

use alvr_common::prelude::*;
use alvr_session::{SocketBufferSize, SocketProtocol};
use bytes::{Buf, BufMut, BytesMut};
use futures::SinkExt;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::HashMap,
    marker::PhantomData,
    mem,
    net::IpAddr,
    ops::{Deref, DerefMut},
    sync::Arc, time::Duration,
};
use tcp::{TcpStreamReceiveSocket, TcpStreamSendSocket};
use tokio::net;
use tokio::sync::{mpsc, Mutex};
use udp::{UdpStreamReceiveSocket, UdpStreamSendSocket};

// 8.6:新增新的包的引用
use chrono::{Utc, TimeZone, Local, format::{strftime, StrftimeItems}};
use alvr_packets::{
    ButtonValue, ClientConnectionResult, ClientControlPacket, ClientListAction, ClientStatistics, ServerControlPacket, StreamConfigPacket, Tracking, VideoPacketHeader, AUDIO, HAPTICS, STATISTICS, TRACKING, VIDEO
};

pub fn set_socket_buffers(
    socket: &socket2::Socket,
    send_buffer_bytes: SocketBufferSize,
    recv_buffer_bytes: SocketBufferSize,
) -> StrResult {
    info!(
        "Initial socket buffer size: send: {}B, recv: {}B",
        socket.send_buffer_size().map_err(err!())?,
        socket.recv_buffer_size().map_err(err!())?
    );

    {
        let maybe_size = match send_buffer_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            if let Err(e) = socket.set_send_buffer_size(size as usize) {
                info!("Error setting socket send buffer: {e}");
            } else {
                info!(
                    "Set socket send buffer succeeded: {}",
                    socket.send_buffer_size().map_err(err!())?
                );
            }
        }
    }

    {
        let maybe_size = match recv_buffer_bytes {
            SocketBufferSize::Default => None,
            SocketBufferSize::Maximum => Some(u32::MAX),
            SocketBufferSize::Custom(size) => Some(size),
        };

        if let Some(size) = maybe_size {
            if let Err(e) = socket.set_recv_buffer_size(size as usize) {
                info!("Error setting socket recv buffer: {e}");
            } else {
                info!(
                    "Set socket recv buffer succeeded: {}",
                    socket.recv_buffer_size().map_err(err!())?
                );
            }
        }
    }

    Ok(())
}

#[derive(Clone)]
enum StreamSendSocket {
    Udp(UdpStreamSendSocket),
    Tcp(TcpStreamSendSocket),
}

enum StreamReceiveSocket {
    Udp(UdpStreamReceiveSocket),
    Tcp(TcpStreamReceiveSocket),
}

pub struct SendBufferLock<'a> {
    header_bytes: &'a mut BytesMut,
    buffer_bytes: BytesMut,
}

impl Deref for SendBufferLock<'_> {
    type Target = BytesMut;
    fn deref(&self) -> &BytesMut {
        &self.buffer_bytes
    }
}

impl DerefMut for SendBufferLock<'_> {
    fn deref_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer_bytes
    }
}

impl Drop for SendBufferLock<'_> {
    fn drop(&mut self) {
        // the extra split is to avoid moving buffer_bytes
        self.header_bytes.unsplit(self.buffer_bytes.split())
    }
}

#[derive(Clone)]
pub struct StreamSender<T> {
    stream_id: u16,
    max_packet_size: usize,
    socket: StreamSendSocket,
    header_buffer: Vec<u8>,
    // if the packet index overflows the worst that happens is a false positive packet loss
    next_packet_index: u32,
    _phantom: PhantomData<T>,
}

impl<T: Serialize> StreamSender<T> {
    async fn send_buffer(&self, buffer: BytesMut) {
        match &self.socket {
            StreamSendSocket::Udp(socket) => socket
                .inner
                .lock()
                .await
                .feed((buffer.freeze(), socket.peer_addr))
                .await
                .map_err(err!())
                .ok(),
            StreamSendSocket::Tcp(socket) => socket
                .lock()
                .await
                .feed(buffer.freeze())
                .await
                .map_err(err!())
                .ok(),
        };
    }
    
    //将message序列化成包并且发送，message有自己的header（例如帧的发送时间戳等），同时序列化之后，每个包有自己的header（帧序号，添加的路由器字段等）
    pub async fn send(&mut self, header: &T, payload_buffer: Vec<u8>) -> StrResult {
        // 当前定义的包结构:
        // 7.25: 定义当前包结构： [ 2B (流ID，用来识别一次会话中的不同流，stream ID) | 2B (流ID会被底层socket消耗，为了识别流类型还需要再复制一遍) | 4B (当前发送帧的帧序号，packet index) | 4B (当前帧包含包数量，packet shard count) 
        // | 4B (当前包序号，shard index)]
        // Modified（7.29）: 为路由器预留字段，每个包到达路由器时的ECN，队列深度，信道利用率这些信息。
        // 7.29：添加字段 | 1B ECN | 1B 队列深度 | 2B 信道利用率（CU）|
        // （to be done）:实际上只有下行的视频数据包需要添加这些字段，也就是只有对stream_id==3的才需要添加路由器信息字段
        // 使用length delimited coding进行封包，在UDP payload开始会额外预留四个字节包含当前payload长度的信息。
        // 不同流对应的流ID：上行的用户动作（0） 下行的HAPTICS对齐（1） 下行的音频（2） 下行编码的视频流（3）上行的统计数据（4）
        // 其他每个包公用的包头在这里定义
        const OFFSET: usize = 2 + 2 + 4 + 4 + 4 + 1 + 1 + 2;
        let max_shard_data_size = self.max_packet_size - OFFSET;

        // 初始化header_buffer，并且预留header_size的长度，最后序列化header
        // 所有的数据用rust的bincode包进行序列化，serialized_size返回序列化需要的长度
        let header_size = bincode::serialized_size(header).map_err(err!()).unwrap() as usize;
        self.header_buffer.clear();
        if self.header_buffer.capacity() < header_size {
            self.header_buffer.reserve(header_size);
        }
        bincode::serialize_into(&mut self.header_buffer, header)
            .map_err(err!())
            .unwrap();
        //计算一帧的包头和payload需要分别需要多少个包传输
        let header_shards = self.header_buffer.chunks(max_shard_data_size);

        let payload_shards = payload_buffer.chunks(max_shard_data_size);

        let total_shards_count = payload_shards.len() + header_shards.len();

        //需要传输的udp payload总长度：message包头+payload包头+总包数量*每个包头的长度（2+4+4+4+1+1+2）
        let mut shards_buffer = BytesMut::with_capacity(
            header_size + payload_buffer.len() + total_shards_count * OFFSET,
        );

        //封包并且发送到socket的缓冲区
        // 8.6更改包类型，再复制一遍流id
        for (shard_index, shard) in header_shards.chain(payload_shards).enumerate() {
            shards_buffer.put_u16(self.stream_id);
            shards_buffer.put_u16(self.stream_id);
            shards_buffer.put_u32(self.next_packet_index);
            shards_buffer.put_u32(total_shards_count as _);
            shards_buffer.put_u32(shard_index as u32);
            // 7.29: 后面是加入的路由器信息的一些字段
            shards_buffer.put_u8(0x88);
            shards_buffer.put_u8(0x77);
            shards_buffer.put_u16(0x55);
            shards_buffer.put_slice(shard);
            self.send_buffer(shards_buffer.split()).await;
        }
        //调用flush()函数一次性将缓冲区中的数据包发送出去
        match &self.socket {
            StreamSendSocket::Udp(socket) => {
                socket.inner.lock().await.flush().await.map_err(err!())?;
            }
            StreamSendSocket::Tcp(socket) => socket.lock().await.flush().await.map_err(err!())?,
        }

        self.next_packet_index += 1;

        Ok(())
    }
}

#[derive(Default)]
pub struct ReceiverBuffer<T> {
    inner: BytesMut,
    had_packet_loss: bool,
    _phantom: PhantomData<T>,
    pub packet_loss_count: i32,
    pub skipping_loss: bool,
    pub shard_loss_rate:f64,
    pub first_packet_receive_time:i64,
    pub last_packet_receive_time:i64,
}

impl<T> ReceiverBuffer<T> {
    pub fn new() -> Self {
        Self {
            inner: BytesMut::new(),
            had_packet_loss: false,
            _phantom: PhantomData,
            packet_loss_count:0,
            skipping_loss:false,
            shard_loss_rate:0.0,
            first_packet_receive_time:Utc::now().timestamp_micros(),
            last_packet_receive_time:Utc::now().timestamp_micros(),
        }
    }

    pub fn had_packet_loss(&self) -> bool {
        self.had_packet_loss
    }
}

impl<T: DeserializeOwned> ReceiverBuffer<T> {
    pub fn get(&self) -> StrResult<(T, &[u8])> {
        let mut data: &[u8] = &self.inner;
        let header = bincode::deserialize_from(&mut data).map_err(err!())?;

        Ok((header, data))
    }
}

pub struct StreamReceiver<T> {
    receiver: mpsc::UnboundedReceiver<BytesMut>,
    next_packet_shards: HashMap<usize, BytesMut>,
    next_packet_shards_count: Option<usize>,
    next_packet_index: u32,
    _phantom: PhantomData<T>,
}

/// Get next packet reconstructing from shards. It can store at max shards from two packets; if the
/// reordering entropy is too high, packets will never be successfully reconstructed.
/// 
/// 从接收的socket处接收包，直到收到完整的一帧画面为止。如果出现丢包会丢弃当前帧
impl<T: DeserializeOwned> StreamReceiver<T> {
    pub async fn recv_buffer(&mut self, buffer: &mut ReceiverBuffer<T>) -> StrResult {
        buffer.had_packet_loss = false;

        loop {
            // 尝试接收下一帧
            let current_packet_index = self.next_packet_index;
            self.next_packet_index += 1;

            let mut current_packet_shards =
                HashMap::with_capacity(self.next_packet_shards.capacity());
            mem::swap(&mut current_packet_shards, &mut self.next_packet_shards);

            let mut current_packet_shards_count = self.next_packet_shards_count.take();

            loop {
                // 目前hash表中存的包数量是否大于当前帧的包数量，大于的话会进行数据的组装并且返回
                // Q：目前代码中丢包检测的逻辑不完善，在当前的代码块内不会请求重传，上次会请求重传关键帧
                if let Some(shards_count) = current_packet_shards_count {
                    if current_packet_shards.len() >= shards_count {
                        buffer.inner.clear();

                        for i in 0..shards_count {
                            if let Some(shard) = current_packet_shards.get(&i) {
                                buffer.inner.put_slice(shard);
                            } else {
                                error!("Cannot find shard with given index!");
                                buffer.had_packet_loss = true;
                                buffer.packet_loss_count+=1;
                                self.next_packet_shards.clear();

                                //break;
                            }
                        }
                        if shards_count!=0{
                            buffer.shard_loss_rate=buffer.packet_loss_count as f64/shards_count as f64;

                        }
                        else {
                            buffer.shard_loss_rate=0.0;
                        }
                        
                        return Ok(());
                    }
                }

                let mut shard = self.receiver.recv().await.ok_or_else(enone!())?;

                let stream_id = shard.get_u16();
                let mut current_timestamps_micros = 0;
                if stream_id == VIDEO {
                    current_timestamps_micros = shard.get_i64();
                }
                let shard_packet_index = shard.get_u32();
                //如果是这一帧第一个包，则保存这一帧第一个包到达时间。

                let shards_count = shard.get_u32() as usize;
                let shard_index = shard.get_u32() as usize;
                // 7.29: (to be done) 需要每一帧统计一下ecn的均值，找个数据机构存一下
                //（只有是视频帧（streamid==3）的时候才需要解析统计，需要显式反馈给client的数据统计，然后通过summary反馈给发送端）
                let ecn_mark = shard.get_u8();
                let queue_depth = shard.get_u8();
                let channel_utility = shard.get_u16();

                //如果是视频帧并且是第一个包，尝试解析出时间戳
                if stream_id == VIDEO && shard_index == 0 {
                    buffer.first_packet_receive_time = current_timestamps_micros;
                }

                if stream_id == VIDEO && shard_index + 1 == shards_count{
                    buffer.last_packet_receive_time = current_timestamps_micros;
                }

                if shard_packet_index == current_packet_index {
                    current_packet_shards.insert(shard_index, shard);
                    current_packet_shards_count = Some(shards_count);
                } else if shard_packet_index >= self.next_packet_index {
                    if shard_packet_index > self.next_packet_index {
                        self.next_packet_shards.clear();
                    }

                    self.next_packet_shards.insert(shard_index, shard);
                    self.next_packet_shards_count = Some(shards_count);
                    self.next_packet_index = shard_packet_index;

                    if shard_packet_index > self.next_packet_index
                        || self.next_packet_shards.len() == shards_count
                    {
                        debug!("Skipping to next packet. Signaling packet loss.");
                        buffer.had_packet_loss = true;
                        buffer.packet_loss_count=1;
                        buffer.skipping_loss=true;
                        buffer.shard_loss_rate=1.0;
                        break;
                    }
                }
                // else: ignore old shard
            }
        }
    }

    pub async fn recv_header_only(&mut self) -> StrResult<T> {
        let mut buffer = ReceiverBuffer::new();
        self.recv_buffer(&mut buffer).await?;
        // 接收并且拼装下一帧的包，反馈一个字符串列表（message的包头和message的数据）
        Ok(buffer.get()?.0)
    }
}

pub enum StreamSocketBuilder {
    Tcp(net::TcpListener),
    Udp(net::UdpSocket),
}

impl StreamSocketBuilder {
    pub async fn listen_for_server(
        port: u16,
        stream_socket_config: SocketProtocol,
        send_buffer_bytes: SocketBufferSize,
        recv_buffer_bytes: SocketBufferSize,
    ) -> StrResult<Self> {
        Ok(match stream_socket_config {
            SocketProtocol::Udp => StreamSocketBuilder::Udp(
                udp::bind(port, send_buffer_bytes, recv_buffer_bytes).await?,
            ),
            SocketProtocol::Tcp => StreamSocketBuilder::Tcp(
                tcp::bind(port, send_buffer_bytes, recv_buffer_bytes).await?,
            ),
        })
    }

    pub async fn accept_from_server(
        self,
        server_ip: IpAddr,
        port: u16,
        max_packet_size: usize,
    ) -> StrResult<StreamSocket> {
        let (send_socket, receive_socket) = match self {
            StreamSocketBuilder::Udp(socket) => {
                let (send_socket, receive_socket) = udp::connect(socket, server_ip, port).await?;
                (
                    StreamSendSocket::Udp(send_socket),
                    StreamReceiveSocket::Udp(receive_socket),
                )
            }
            StreamSocketBuilder::Tcp(listener) => {
                let (send_socket, receive_socket) =
                    tcp::accept_from_server(listener, server_ip).await?;
                (
                    StreamSendSocket::Tcp(send_socket),
                    StreamReceiveSocket::Tcp(receive_socket),
                )
            }
        };

        Ok(StreamSocket {
            max_packet_size,
            send_socket,
            receive_socket: Arc::new(Mutex::new(Some(receive_socket))),
            packet_queues: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn connect_to_client(
        client_ip: IpAddr,
        port: u16,
        protocol: SocketProtocol,
        send_buffer_bytes: SocketBufferSize,
        recv_buffer_bytes: SocketBufferSize,
        max_packet_size: usize,
    ) -> StrResult<StreamSocket> {
        let (send_socket, receive_socket) = match protocol {
            SocketProtocol::Udp => {
                let socket = udp::bind(port, send_buffer_bytes, recv_buffer_bytes).await?;
                let (send_socket, receive_socket) = udp::connect(socket, client_ip, port).await?;
                (
                    StreamSendSocket::Udp(send_socket),
                    StreamReceiveSocket::Udp(receive_socket),
                )
            }
            SocketProtocol::Tcp => {
                let (send_socket, receive_socket) =
                    tcp::connect_to_client(client_ip, port, send_buffer_bytes, recv_buffer_bytes)
                        .await?;
                (
                    StreamSendSocket::Tcp(send_socket),
                    StreamReceiveSocket::Tcp(receive_socket),
                )
            }
        };

        Ok(StreamSocket {
            max_packet_size,
            send_socket,
            receive_socket: Arc::new(Mutex::new(Some(receive_socket))),
            packet_queues: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

pub struct StreamSocket {
    max_packet_size: usize,
    send_socket: StreamSendSocket,
    receive_socket: Arc<Mutex<Option<StreamReceiveSocket>>>,
    packet_queues: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<BytesMut>>>>,
}

impl StreamSocket {
    pub async fn request_stream<T>(&self, stream_id: u16) -> StrResult<StreamSender<T>> {
        Ok(StreamSender {
            stream_id,
            max_packet_size: self.max_packet_size,
            socket: self.send_socket.clone(),
            header_buffer: vec![],
            next_packet_index: 0,
            _phantom: PhantomData,
        })
    }

    pub async fn subscribe_to_stream<T>(&self, stream_id: u16) -> StrResult<StreamReceiver<T>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.packet_queues.lock().await.insert(stream_id, sender);

        Ok(StreamReceiver {
            receiver,
            next_packet_shards: HashMap::new(),
            next_packet_shards_count: None,
            next_packet_index: 0,
            _phantom: PhantomData,
        })
    }

    pub async fn receive_loop(&self) -> StrResult {
        match self.receive_socket.lock().await.take().unwrap() {
            StreamReceiveSocket::Udp(socket) => {
                udp::receive_loop(socket, Arc::clone(&self.packet_queues)).await
            }
            StreamReceiveSocket::Tcp(socket) => {
                tcp::receive_loop(socket, Arc::clone(&self.packet_queues)).await
            }
        }
    }
}
