#![allow(clippy::if_same_then_else)]

use crate::{
    decoder::{self, DECODER_INIT_CONFIG},
    platform,
    sockets::AnnouncerSocket,
    statistics::StatisticsManager,
    storage::Config,
    ClientCoreEvent, CONTROL_CHANNEL_SENDER, DISCONNECT_NOTIFIER, EVENT_QUEUE, IS_ALIVE,
    IS_RESUMED, IS_STREAMING, STATISTICS_MANAGER, STATISTICS_SENDER, TRACKING_SENDER,
};
use alvr_audio::AudioDevice;
use alvr_common::{glam::UVec2, prelude::*, ALVR_VERSION, HEAD_ID};
use alvr_packets::{
    BatteryPacket, ClientConnectionResult, ClientControlPacket, Haptics, ServerControlPacket,
    StreamConfigPacket, VideoPacketHeader, VideoStreamingCapabilities, AUDIO, HAPTICS, STATISTICS,
    TRACKING, VIDEO,
};
use alvr_session::{settings_schema::Switch, SessionDesc};
use alvr_sockets::{
    spawn_cancelable, PeerType, ProtoControlSocket, ReceiverBuffer, StreamSocketBuilder,
};
use futures::future::BoxFuture;
use serde_json as json;
use std::{
    collections::HashMap,
    future,
    net::IpAddr,
    sync::Arc,
    thread,
    time::{Duration, Instant},
    net::TcpStream,
    io::{Read, Write},
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc as tmpsc, Mutex},
    time,
};

#[cfg(target_os = "android")]
use crate::audio;
#[cfg(not(target_os = "android"))]
use alvr_audio as audio;

const INITIAL_MESSAGE: &str = concat!(
    "Searching for streamer...\n",
    "Open ALVR on your PC then click \"Trust\"\n",
    "next to the client entry",
);
const NETWORK_UNREACHABLE_MESSAGE: &str = "Cannot connect to the internet";
// const INCOMPATIBLE_VERSIONS_MESSAGE: &str = concat!(
//     "Streamer and client have\n",
//     "incompatible types.\n",
//     "Please update either the app\n",
//     "on the PC or on the headset",
// );
const STREAM_STARTING_MESSAGE: &str = "The stream will begin soon\nPlease wait...";
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";
const SERVER_DISCONNECTED_MESSAGE: &str = "The streamer has disconnected.";

const DISCOVERY_RETRY_PAUSE: Duration = Duration::from_millis(500);
const RETRY_CONNECT_MIN_INTERVAL: Duration = Duration::from_secs(1);
const NETWORK_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(60);

fn set_hud_message(message: &str) {
    let message = format!(
        "ALVR v{}\nhostname: {}\nIP: {}\n\n{message}",
        *ALVR_VERSION,
        Config::load().hostname,
        platform::local_ip(),
    );

    EVENT_QUEUE
        .lock()
        .push_back(ClientCoreEvent::UpdateHudMessage(message));
}

// 处理断连恢复的函数
pub fn connection_lifecycle_loop(
    recommended_view_resolution: UVec2,
    supported_refresh_rates: Vec<f32>,
) -> IntResult {
    set_hud_message(INITIAL_MESSAGE);

    let decoder_guard = Arc::new(Mutex::new(()));

    loop {
        check_interrupt!(IS_ALIVE.value());

        if IS_RESUMED.value() {
            // 重连
            if let Err(e) = connection_pipeline(
                recommended_view_resolution,
                supported_refresh_rates.clone(),
                Arc::clone(&decoder_guard),
            ) {
                match e {
                    InterruptibleError::Interrupted => return Ok(()),
                    InterruptibleError::Other(_) => {
                        let message =
                            format!("Connection error:\n{e}\nCheck the PC for more details");
                        error!("{message}");
                        set_hud_message(&message);
                    }
                }
            }
        }
        // 断连之后每秒钟进行尝试重连，CONNECTION_RETRY
        thread::sleep(CONNECTION_RETRY_INTERVAL);
    }
}

fn connection_pipeline(
    recommended_view_resolution: UVec2,
    supported_refresh_rates: Vec<f32>,
    decoder_guard: Arc<Mutex<()>>,
) -> IntResult {
    let runtime = Runtime::new().map_err(to_int_e!())?;

    // 广播，尝试和发送端建立连接，返回发送端socket和IP
    let (mut proto_control_socket, server_ip) = {
        let config = Config::load();
        let announcer_socket = AnnouncerSocket::new(&config.hostname).map_err(to_int_e!())?;
        let listener_socket = runtime
            .block_on(alvr_sockets::get_server_listener())
            .map_err(to_int_e!())?;

        loop {
            check_interrupt!(IS_ALIVE.value());

            if let Err(e) = announcer_socket.broadcast() {
                warn!("Broadcast error: {e}");

                set_hud_message(NETWORK_UNREACHABLE_MESSAGE);

                thread::sleep(RETRY_CONNECT_MIN_INTERVAL);

                set_hud_message(INITIAL_MESSAGE);

                return Ok(());
            }

            let maybe_pair = runtime.block_on(async {
                tokio::select! {
                    maybe_pair = ProtoControlSocket::connect_to(PeerType::Server(&listener_socket)) => {
                        maybe_pair.map_err(to_int_e!())
                    },
                    _ = time::sleep(DISCOVERY_RETRY_PAUSE) => Err(InterruptibleError::Interrupted)
                }
            });

            if let Ok(pair) = maybe_pair {
                break pair;
            }
        }
    };

    let microphone_sample_rate = AudioDevice::new_input(None)
        .unwrap()
        .input_sample_rate()
        .unwrap();

    // 向发送端发送接收端信息数据包：接收端版本，接收端支持的刷新率，分辨率以及声音采样频率
    // 调用rust runtime的block_on，阻塞当前线程
    runtime
        .block_on(
            proto_control_socket.send(&ClientConnectionResult::ConnectionAccepted {
                client_protocol_id: alvr_common::protocol_id(),
                display_name: platform::device_model(),
                server_ip,
                streaming_capabilities: Some(VideoStreamingCapabilities {
                    default_view_resolution: recommended_view_resolution,
                    supported_refresh_rates,
                    microphone_sample_rate,
                }),
            }),
        )
        .map_err(to_int_e!())?;
    let config_packet = runtime
        .block_on(proto_control_socket.recv::<StreamConfigPacket>())
        .map_err(to_int_e!())?;

    // 建立数据发送的pipeline，并且阻塞在当前线程
    runtime
        .block_on(stream_pipeline(
            proto_control_socket,
            config_packet,
            server_ip,
            decoder_guard,
        ))
        .map_err(to_int_e!())
}

async fn stream_pipeline(
    proto_socket: ProtoControlSocket,
    stream_config: StreamConfigPacket,
    server_ip: IpAddr,
    decoder_guard: Arc<Mutex<()>>,
) -> StrResult {
    let settings = {
        let mut session_desc = SessionDesc::default();
        session_desc.merge_from_json(&json::from_str(&stream_config.session).map_err(err!())?)?;
        session_desc.to_settings()
    };

    let negotiated_config =
        json::from_str::<HashMap<String, json::Value>>(&stream_config.negotiated)
            .map_err(err!())?;

    let view_resolution = negotiated_config
        .get("view_resolution")
        .and_then(|v| json::from_value(v.clone()).ok())
        .unwrap_or(UVec2::ZERO);
    let refresh_rate_hint = negotiated_config
        .get("refresh_rate_hint")
        .and_then(|v| v.as_f64())
        .unwrap_or(60.0) as f32;
    let game_audio_sample_rate = negotiated_config
        .get("game_audio_sample_rate")
        .and_then(|v| v.as_u64())
        .unwrap_or(44100) as u32;

    let streaming_start_event = ClientCoreEvent::StreamingStarted {
        view_resolution,
        refresh_rate_hint,
        settings: Box::new(settings.clone()),
    };

    let (control_sender, mut control_receiver) = proto_socket.split();
    let control_sender = Arc::new(Mutex::new(control_sender));

    match control_receiver.recv().await {
        Ok(ServerControlPacket::StartStream) => {
            info!("Stream starting");
            set_hud_message(STREAM_STARTING_MESSAGE);
        }
        Ok(ServerControlPacket::Restarting) => {
            info!("Server restarting");
            set_hud_message(SERVER_RESTART_MESSAGE);
            return Ok(());
        }
        Err(e) => {
            info!("Server disconnected. Cause: {e}");
            set_hud_message(SERVER_DISCONNECTED_MESSAGE);
            return Ok(());
        }
        _ => {
            info!("Unexpected packet");
            set_hud_message("Unexpected packet");
            return Ok(());
        }
    }

    *STATISTICS_MANAGER.lock() = Some(StatisticsManager::new(
        settings.connection.statistics_history_size as _,
        Duration::from_secs_f32(1.0 / refresh_rate_hint),
        if let Switch::Enabled(config) = settings.headset.controllers {
            config.steamvr_pipeline_frames
        } else {
            0.0
        },
    ));

    let stream_socket_builder = StreamSocketBuilder::listen_for_server(
        settings.connection.stream_port,
        settings.connection.stream_protocol,
        settings.connection.client_send_buffer_bytes,
        settings.connection.client_recv_buffer_bytes,
    )
    .await?;

    if let Err(e) = control_sender
        .lock()
        .await
        .send(&ClientControlPacket::StreamReady)
        .await
    {
        info!("Server disconnected. Cause: {e}");
        set_hud_message(SERVER_DISCONNECTED_MESSAGE);
        return Ok(());
    }

    let stream_socket = tokio::select! {
        res = stream_socket_builder.accept_from_server(
            server_ip,
            settings.connection.stream_port,
            settings.connection.packet_size as _
        ) => res?,
        _ = time::sleep(Duration::from_secs(5)) => {
            return fmt_e!("Timeout while setting up streams");
        }
    };
    let stream_socket = Arc::new(stream_socket);

    info!("Connected to server");

    // create this before initializing the stream on cpp side
    let (control_channel_sender, mut control_channel_receiver) = tmpsc::unbounded_channel();
    *CONTROL_CHANNEL_SENDER.lock() = Some(control_channel_sender);

    {
        let config = &mut *DECODER_INIT_CONFIG.lock();

        config.max_buffering_frames = settings.video.max_buffering_frames;
        config.buffering_history_weight = settings.video.buffering_history_weight;
        config.options = settings.video.mediacodec_extra_options;
    }

    let tracking_send_loop = {
        let mut socket_sender = stream_socket.request_stream(TRACKING).await?;
        async move {
            let (data_sender, mut data_receiver) = tmpsc::unbounded_channel();
            *TRACKING_SENDER.lock() = Some(data_sender);

            while let Some(tracking) = data_receiver.recv().await {
                socket_sender.send(&tracking, vec![]).await.ok();

                // Note: this is not the best place to report the acquired input. Instead it should
                // be done as soon as possible (or even just before polling the input). Instead this
                // is reported late to partially compensate for lack of network latency measurement,
                // so the server can just use total_pipeline_latency as the postTimeoffset.
                // This hack will be removed once poseTimeOffset can be calculated more accurately.
                if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                    stats.report_input_acquired(tracking.target_timestamp);
                }
            }

            Ok(())
        }
    };

    let statistics_send_loop = {
        let mut socket_sender = stream_socket.request_stream(STATISTICS).await?;
        async move {
            let (data_sender, mut data_receiver) = tmpsc::unbounded_channel();
            *STATISTICS_SENDER.lock() = Some(data_sender);

            while let Some(stats) = data_receiver.recv().await {
                socket_sender.send(&stats, vec![]).await.ok();
            }

            Ok(())
        }
    };
    //bandwidth packet recev (wz) wz bandwidth
    let bandwidth_packet_loop={
        let mut stream = TcpStream::connect(server_ip.to_string()+":8080").unwrap();
        println!("{}",server_ip.to_string()+":8080");
        async move {
            // receive and respond to multiple probe packets
            //let num_packets = 10;
            let packet_size = 100;
            let mut recv_packet = vec![0; packet_size];
            //let mut send_packet = vec![0; packet_size];

            loop{
                // receive the probe packet from the server
                stream.read_exact(&mut recv_packet).unwrap();

                // send the probe packet back to the server
                stream.write_all(&recv_packet).unwrap();
    
                //println!("Packet {} received and responded", i);
            } 
            
        }
        
    };


    IS_STREAMING.set(true);

    let video_receive_loop = {
        let mut receiver = stream_socket
            .subscribe_to_stream::<VideoPacketHeader>(VIDEO)
            .await?;
        async move {
            let _decoder_guard = decoder_guard.lock().await;

            // close stream on Drop (manual disconnection or execution canceling)
            struct StreamCloseGuard;

            impl Drop for StreamCloseGuard {
                fn drop(&mut self) {
                    EVENT_QUEUE
                        .lock()
                        .push_back(ClientCoreEvent::StreamingStopped);

                    IS_STREAMING.set(false);

                    #[cfg(target_os = "android")]
                    {
                        *crate::decoder::DECODER_ENQUEUER.lock() = None;
                        *crate::decoder::DECODER_DEQUEUER.lock() = None;
                    }
                }
            }

            let _stream_guard = StreamCloseGuard;

            EVENT_QUEUE.lock().push_back(streaming_start_event);

            let mut receiver_buffer = ReceiverBuffer::new();
            let mut stream_corrupted = false;
            //calculate packet loss rate
            let mut total_packets = 0;
            let mut lost_packets = 0;
            let mut last_time = Instant::now();
            loop {
                //在这里放置一个timer，每秒积累loss值÷总数得到packetlossrate，在statistics中添加report plr 用对应的timestamp找到对应的frame的clientstats中存入plr send回server，÷0的情况
                receiver.recv_buffer(&mut receiver_buffer).await?;
                let (header, nal) = receiver_buffer.get()?;

                if !IS_RESUMED.value() {
                    break Ok(());
                }

                if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                    total_packets+=1;
                    stats.report_video_packet_received(header.timestamp, receiver_buffer.first_packet_receive_time, receiver_buffer.last_packet_receive_time, nal.len());
                }
                if receiver_buffer.had_packet_loss(){
                    if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        stats.report_shard_loss_rate(header.timestamp, receiver_buffer.shard_loss_rate);
                    }
                }

                if header.is_idr {
                    if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        stats.report_is_idr(header.timestamp);
                    }
                    stream_corrupted = false;
                } else if receiver_buffer.had_packet_loss() {
                    stream_corrupted = true;
                    lost_packets+=1;
                    if let Some(sender) = &*CONTROL_CHANNEL_SENDER.lock() {
                        sender.send(ClientControlPacket::RequestIdr).ok();
                    }
                    if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        stats.report_bug_reason(header.timestamp, 3);
                    }
                    warn!("Network dropped video packet");
                }
                // 每300ms计算一次丢包率，但是这里的包的概念指的是一帧，所以应该是300ms的丢帧率
                let elapsed = last_time.elapsed();
                if elapsed >= Duration::from_millis(300) {
                    let mut packet_loss_rate=0.0;
                    if total_packets!=0{
                        packet_loss_rate = lost_packets as f64 / total_packets as f64;
                    }
                    
                    //println!("Packet loss rate: {:.2}%", packet_loss_rate * 100.0);
                    if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        
                         
                        stats.report_plr(header.timestamp, packet_loss_rate);
                    }
                    // Reset counters and timer
                    total_packets = 0;
                    lost_packets = 0;
                    last_time = Instant::now();
                }

                if !stream_corrupted || !settings.connection.avoid_video_glitching {
                    if !decoder::push_nal(header.timestamp, nal) {
                        stream_corrupted = true;
                        if let Some(sender) = &*CONTROL_CHANNEL_SENDER.lock() {
                            sender.send(ClientControlPacket::RequestIdr).ok();
                        }
                        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                            stats.report_bug_reason(header.timestamp, 1);
                        }
                        warn!("Dropped video packet. Reason: Decoder saturation")
                    }
                } else {
                    if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        stats.report_bug_reason(header.timestamp, 2);
                    }
                    warn!("Dropped video packet. Reason: Waiting for IDR frame")
                }
                // if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                        
                //     if receiver_buffer.skipping_loss{
                //         stats.report_plr(header.timestamp, receiver_buffer.packet_loss_count);
                //         receiver_buffer.packet_loss_count=0;
                //     }
                //     else {
                //        stats.report_plr(header.timestamp, receiver_buffer.packet_loss_count);
                //        receiver_buffer.packet_loss_count=0;
                //     }
                    
                // }
            }
        }
    };

    let haptics_receive_loop = {
        let mut receiver = stream_socket
            .subscribe_to_stream::<Haptics>(HAPTICS)
            .await?;
        async move {
            loop {
                let haptics = receiver.recv_header_only().await?;

                EVENT_QUEUE.lock().push_back(ClientCoreEvent::Haptics {
                    device_id: haptics.device_id,
                    duration: haptics.duration,
                    frequency: haptics.frequency,
                    amplitude: haptics.amplitude,
                });
            }
        }
    };

    let game_audio_loop: BoxFuture<_> = if let Switch::Enabled(config) = settings.audio.game_audio {
        let device = AudioDevice::new_output(None, None).map_err(err!())?;

        let game_audio_receiver = stream_socket.subscribe_to_stream(AUDIO).await?;
        Box::pin(audio::play_audio_loop(
            device,
            2,
            game_audio_sample_rate,
            config.buffering,
            game_audio_receiver,
        ))
    } else {
        Box::pin(future::pending())
    };

    let microphone_loop: BoxFuture<_> = if matches!(settings.audio.microphone, Switch::Enabled(_)) {
        let device = AudioDevice::new_input(None).map_err(err!())?;

        let microphone_sender = stream_socket.request_stream(AUDIO).await?;
        Box::pin(audio::record_audio_loop(
            device,
            1,
            false,
            microphone_sender,
        ))
    } else {
        Box::pin(future::pending())
    };

    // Poll for events that need a constant thread (mainly for the JNI env)
    thread::spawn(|| {
        #[cfg(target_os = "android")]
        let vm = platform::vm();
        #[cfg(target_os = "android")]
        let _env = vm.attach_current_thread();

        let mut previous_hmd_battery_status = (0.0, false);
        let mut battery_poll_deadline = Instant::now();

        while IS_STREAMING.value() {
            if battery_poll_deadline < Instant::now() {
                let new_hmd_battery_status = platform::battery_status();

                if new_hmd_battery_status != previous_hmd_battery_status {
                    if let Some(sender) = &*CONTROL_CHANNEL_SENDER.lock() {
                        sender
                            .send(ClientControlPacket::Battery(BatteryPacket {
                                device_id: *HEAD_ID,
                                gauge_value: new_hmd_battery_status.0,
                                is_plugged: new_hmd_battery_status.1,
                            }))
                            .ok();

                        previous_hmd_battery_status = new_hmd_battery_status;
                    }
                }

                battery_poll_deadline += BATTERY_POLL_INTERVAL;
            }

            thread::sleep(Duration::from_secs(1));
        }
    });

    let keepalive_sender_loop = {
        let control_sender = Arc::clone(&control_sender);
        async move {
            loop {
                let res = control_sender
                    .lock()
                    .await
                    .send(&ClientControlPacket::KeepAlive)
                    .await;
                if let Err(e) = res {
                    info!("Server disconnected. Cause: {e}");
                    set_hud_message(SERVER_DISCONNECTED_MESSAGE);
                    break Ok(());
                }

                time::sleep(NETWORK_KEEPALIVE_INTERVAL).await;
            }
        }
    };

    let control_send_loop = async move {
        while let Some(packet) = control_channel_receiver.recv().await {
            control_sender.lock().await.send(&packet).await.ok();
        }

        Ok(())
    };

    let control_receive_loop = async move {
        loop {
            match control_receiver.recv().await {
                Ok(ServerControlPacket::InitializeDecoder(config)) => {
                    decoder::create_decoder(config);
                }
                Ok(ServerControlPacket::Restarting) => {
                    info!("{SERVER_RESTART_MESSAGE}");
                    set_hud_message(SERVER_RESTART_MESSAGE);
                    break Ok(());
                }
                Ok(_) => (),
                Err(e) => {
                    info!("{SERVER_DISCONNECTED_MESSAGE} Cause: {e}");
                    set_hud_message(SERVER_DISCONNECTED_MESSAGE);
                    break Ok(());
                }
            }
        }
    };

    let receive_loop = async move { stream_socket.receive_loop().await };

    tokio::spawn(async {
        if let Err(e) = spawn_cancelable(receive_loop).await {
            info!("Server disconnected. Cause: {e}");
            set_hud_message(SERVER_DISCONNECTED_MESSAGE);
        }
    });

    // Run many tasks concurrently. Threading is managed by the runtime, for best performance.
    tokio::select! {
        // res = spawn_cancelable(receive_loop) => {
        //     if let Err(e) = res {
        //         info!("Server disconnected. Cause: {e}");
        //     }
        //     set_hud_message(
        //         SERVER_DISCONNECTED_MESSAGE
        //     );

        //     Ok(())
        // },
        res = spawn_cancelable(game_audio_loop) => res,
        res = spawn_cancelable(microphone_loop) => res,
        res = spawn_cancelable(tracking_send_loop) => res,
        res = spawn_cancelable(statistics_send_loop) => res,
        res = spawn_cancelable(video_receive_loop) => res,
        res = spawn_cancelable(haptics_receive_loop) => res,
        res = spawn_cancelable(control_send_loop) => res,
        res = spawn_cancelable(bandwidth_packet_loop) => res,//wz

        // keep these loops on the current task
        res = keepalive_sender_loop => res,
        res = control_receive_loop => res,

        _ = DISCONNECT_NOTIFIER.notified() => Ok(()),
    }
}
