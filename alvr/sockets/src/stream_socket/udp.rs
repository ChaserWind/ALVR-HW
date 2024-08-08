use crate::{Ldc, LOCAL_IP};
use alvr_common::prelude::*;
use alvr_packets::VIDEO;
use alvr_session::SocketBufferSize;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use chrono::Utc;
use futures::{
    stream::{SplitSink, SplitStream},
    StreamExt,
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Mutex},
};
use tokio_util::udp::UdpFramed;

#[allow(clippy::type_complexity)]
#[derive(Clone)]
pub struct UdpStreamSendSocket {
    pub peer_addr: SocketAddr,
    pub inner: Arc<Mutex<SplitSink<UdpFramed<Ldc>, (Bytes, SocketAddr)>>>,
}

// peer_addr is needed to check that the packet comes from the desired device. Connecting directly
// to the peer is not supported by UdpFramed.
pub struct UdpStreamReceiveSocket {
    pub peer_addr: SocketAddr,
    pub inner: SplitStream<UdpFramed<Ldc>>,
}

// Create tokio socket, convert to socket2, apply settings, convert back to tokio. This is done to
// let tokio set all the internal parameters it needs from the start.
pub async fn bind(
    port: u16,
    send_buffer_bytes: SocketBufferSize,
    recv_buffer_bytes: SocketBufferSize,
) -> StrResult<UdpSocket> {
    let socket = UdpSocket::bind((LOCAL_IP, port)).await.map_err(err!())?;
    let socket = socket2::Socket::from(socket.into_std().map_err(err!())?);

    super::set_socket_buffers(&socket, send_buffer_bytes, recv_buffer_bytes).ok();

    UdpSocket::from_std(socket.into()).map_err(err!())
}

pub async fn connect(
    socket: UdpSocket,
    peer_ip: IpAddr,
    port: u16,
) -> StrResult<(UdpStreamSendSocket, UdpStreamReceiveSocket)> {
    let peer_addr = (peer_ip, port).into();
    let socket = UdpFramed::new(socket, Ldc::new());
    let (send_socket, receive_socket) = socket.split();

    Ok((
        UdpStreamSendSocket {
            peer_addr,
            inner: Arc::new(Mutex::new(send_socket)),
        },
        UdpStreamReceiveSocket {
            peer_addr,
            inner: receive_socket,
        },
    ))
}


//8.8: 如果是视频帧，需要立刻记录包到达的时间并且添加在现在数据的一开始（i64）
pub async fn receive_loop(
    mut socket: UdpStreamReceiveSocket,
    packet_enqueuers: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<BytesMut>>>>,
) -> StrResult {
    while let Some(maybe_packet) = socket.inner.next().await {
        let (mut packet_bytes, address) = maybe_packet.map_err(err!())?;

        if address != socket.peer_addr {
            continue;
        }

        let stream_id = packet_bytes.get_u16();
        if stream_id == VIDEO {
            let current_timestamps_micros = Utc::now().timestamp_micros() as i64;
            packet_bytes.reserve(std::mem::size_of::<i64>());
            let tail = packet_bytes.split_to(packet_bytes.len());
            packet_bytes.put_i64(current_timestamps_micros);
            packet_bytes.extend_from_slice(&tail);
        }
        if let Some(enqueuer) = packet_enqueuers.lock().await.get_mut(&stream_id) {
            enqueuer.send(packet_bytes).map_err(err!())?;
        }
    }

    Ok(())
}
