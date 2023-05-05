mod gateway_event;

use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite;

use gateway_event::*;

/// Wrapper around [`tokio::task::JoinHandle`] that aborts instead of detaches on Drop.
///
/// Useful for utility tasks that shouldn't outlive their "parent" task.
#[must_use = "dropping this type aborts the task"]
struct AttachedTask(tokio::task::JoinHandle<()>);
impl Drop for AttachedTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn spawn_attached(f: impl std::future::Future<Output = ()> + Send + 'static) -> AttachedTask {
    AttachedTask(tokio::spawn(f))
}

#[derive(Debug)]
pub struct ShardEvent {
    pub name: String,
}

pub enum ShardCommand {
    Shutdown,
}

pub enum ConnectionState {
    WaitingForHello,
    WaitingForResumed,
    Connected,
}

/// Contains all data about a shard connection that gets thrown out on reconnect
struct ShardConnection {
    /// Need to keep track of this because "When sending a heartbeat, your app will need to include
    /// the last sequence number your app received in the d field"
    last_seq_number: parking_lot::Mutex<u64>,
    /// Need to keep track of this because "If a client does not receive a heartbeat ACK between
    /// its attempts at sending heartbeats, this may be due to a failed or "zombied" connection.
    /// The client should immediately terminate the connection with any close code besides 1000 or
    /// 1001, then reconnect and attempt to Resume."
    has_acknowledged_last_heartbeat: parking_lot::Mutex<bool>,

    heartbeat_task: once_cell::sync::OnceCell<AttachedTask>,

    sender: tokio::sync::Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tungstenite::Message,
        >,
    >,
    // Wrapped in Arc so we can clone it out and not block the entire ShardConnection while waiting
    // for a value
    receiver: std::sync::Arc<
        tokio::sync::Mutex<
            futures::stream::SplitStream<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
            >,
        >,
    >,
}

impl ShardConnection {
    async fn send(&self, msg: GatewaySendEvent) {
        let msg = match serde_json::to_string(&msg) {
            Ok(x) => x,
            Err(e) => {
                log::warn!("failed to serialize websocket message: {}", e);
                return;
            }
        };

        log::info!("sending {}", msg);

        if let Err(e) = self.sender.lock().await.send(tungstenite::Message::Text(msg)).await {
            log::warn!("failed to send websocket message: {}", e);
        }
    }
}

#[non_exhaustive]
pub struct ShardOptions {
    pub gateway_url: String,
    pub token: String,
    pub callback: Box<dyn Fn(ShardEvent) + Send + Sync>,
    pub identify_properties: IdentifyProperties,
    pub intents: u64,
}

impl ShardOptions {
    pub fn new(gateway_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            gateway_url: gateway_url.into(),
            token: token.into(),
            callback: Box::new(|_| {}),
            identify_properties: IdentifyProperties {
                os: std::env::consts::OS.into(),
                browser: "serenity".into(),
                device: "serenity".into(),
            },
            intents: 1 << 0 | 1 << 9 | 1 << 15,
        }
    }
}

/// Contains [`ShardConnection`], as well as the data that survives a reconnect
pub struct Shard {
    connection: tokio::sync::Mutex<ShardConnection>,
    command_sender: tokio::sync::mpsc::UnboundedSender<ShardCommand>,
    command_receiver: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ShardCommand>>,
    session_id: parking_lot::Mutex<Option<String>>,
    resume_gateway_url: parking_lot::Mutex<Option<String>>,
    state: parking_lot::Mutex<ConnectionState>,
    options: ShardOptions,
}

impl Shard {
    pub async fn new(options: ShardOptions) -> Result<std::sync::Arc<Self>, tungstenite::Error> {
        let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();

        Ok(std::sync::Arc::new(Shard {
            connection: tokio::sync::Mutex::new(connect(&options.gateway_url).await?),
            command_sender,
            command_receiver: tokio::sync::Mutex::new(command_receiver),
            state: parking_lot::Mutex::new(ConnectionState::WaitingForHello),
            resume_gateway_url: parking_lot::Mutex::new(None),
            session_id: parking_lot::Mutex::new(None),
            options,
        }))
    }

    pub async fn run(self: std::sync::Arc<Self>) {
        run(self).await;
    }

    #[allow(clippy::unused_async)] // future-proof
    pub async fn send_command(&self, command: ShardCommand) {
        if let Err(e) = self.command_sender.send(command) {
            log::warn!("error while sending shard command (impossible because the shard receiver is stored in self): {}", e);
        }
    }
}

async fn connect(url: &str) -> Result<ShardConnection, tungstenite::Error> {
    log::info!("connecting to {}...", url);

    let (stream, _connect_response) = tokio_tungstenite::connect_async(url).await?;
    let (ws_sender, ws_receiver) = stream.split();

    Ok(ShardConnection {
        // last_seq_number: std::sync::atomic::AtomicU64::new(0),
        // has_acknowledged_last_heartbeat: std::sync::atomic::AtomicBool::new(false),
        last_seq_number: parking_lot::Mutex::new(0),
        has_acknowledged_last_heartbeat: parking_lot::Mutex::new(true),
        sender: tokio::sync::Mutex::new(ws_sender),
        receiver: std::sync::Arc::new(tokio::sync::Mutex::new(ws_receiver)),
        heartbeat_task: once_cell::sync::OnceCell::new(),
    })
}

async fn reconnect_and_resume(shard: &Shard, connection: &mut ShardConnection) {
    let (Some(mut gateway_url), Some(session_id)) = (shard.resume_gateway_url.lock().clone(), shard.session_id.lock().clone()) else {
        log::warn!("resume_gateway_url or session_id is None, cannot resume");
        reconnect_and_dont_resume(shard, connection).await;
        return;
    };

    let last_seq_number = *connection.last_seq_number.lock();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    *connection = connect(&gateway_url).await.unwrap();
    *connection.last_seq_number.lock() = last_seq_number;

    connection
        .send(GatewaySendEvent::Resume(Resume {
            token: shard.options.token.clone(),
            session_id,
            seq: last_seq_number,
        }))
        .await;

    *shard.state.lock() = ConnectionState::WaitingForResumed;
}

async fn reconnect_and_dont_resume(shard: &Shard, connection: &mut ShardConnection) {
    *connection = connect(&shard.options.gateway_url).await.unwrap();

    *shard.state.lock() = ConnectionState::WaitingForHello;
}

async fn process_gateway_event(shard: &std::sync::Arc<Shard>, msg: GatewayReceiveEvent) {
    // If this function was called, we're definitely connected to the shard and can rely on that
    // for the duration of this function
    let connection = &mut *shard.connection.lock().await;

    let s = format!("{:?}", msg);
    log::info!("received {}", &s[..s.len().min(200)]);

    match msg {
        GatewayReceiveEvent::Dispatch(Dispatch { event_type, event_data, seq }) => {
            *connection.last_seq_number.lock() = seq;

            if event_type.eq_ignore_ascii_case("ready") {
                #[derive(serde::Deserialize)]
                struct Ready {
                    session_id: String,
                    resume_gateway_url: String,
                }
                let ready = serde_json::from_str::<Ready>(event_data.get()).unwrap();
                *shard.resume_gateway_url.lock() = Some(ready.resume_gateway_url);
                *shard.session_id.lock() = Some(ready.session_id);
            }

            (shard.options.callback)(ShardEvent { name: event_type });
        }
        GatewayReceiveEvent::Heartbeat => {
            // Should we make sure heartbeat was ACKed here too?
            connection
                .send(GatewaySendEvent::Heartbeat(Heartbeat {
                    last_seq_number: *connection.last_seq_number.lock(),
                }))
                .await;
        }
        GatewayReceiveEvent::Hello(Hello { heartbeat_interval }) => {
            let is_resuming = match *shard.state.lock() {
                ConnectionState::WaitingForHello => false,
                ConnectionState::WaitingForResumed => true,
                ConnectionState::Connected => {
                    log::warn!("ignoring unexpected Hello event");
                    return;
                }
            };
            let heartbeat_interval = std::time::Duration::from_millis(heartbeat_interval);

            let shard2 = shard.clone();
            let old_heartbeat_task = connection.heartbeat_task.set(spawn_attached(async move {
                let shard = shard2;

                // Discord-mandated jitter
                tokio::time::sleep(heartbeat_interval.mul_f32(fastrand::f32())).await;

                let mut interval = tokio::time::interval(heartbeat_interval);
                loop {
                    interval.tick().await; // first tick completes immediately

                    // This must only be locked for a short while! Storing the lock guard in a
                    // variable is ok because the long-running interval.tick().await is above
                    let mut connection = shard.connection.lock().await;

                    let has_acknowledged_last_heartbeat =
                        *connection.has_acknowledged_last_heartbeat.lock();
                    if !has_acknowledged_last_heartbeat {
                        reconnect_and_resume(&shard, &mut connection).await;
                    }
                    *connection.has_acknowledged_last_heartbeat.lock() = false;

                    let last_seq_number = *connection.last_seq_number.lock();
                    connection
                        .send(GatewaySendEvent::Heartbeat(Heartbeat { last_seq_number }))
                        .await;
                }
            }));
            if old_heartbeat_task.is_err() {
                log::warn!("a heartbeat task was already running and has been overwritten");
            }

            if !is_resuming {
                let event = GatewaySendEvent::Identify(Identify {
                    token: shard.options.token.clone(),
                    properties: shard.options.identify_properties.clone(),
                    intents: shard.options.intents,
                });
                connection.send(event).await;
            } else {
                // connection
                //     .send(GatewaySendEvent::Resume(Resume {
                //         token: shard.options.token.clone(),
                //         session_id: shard.session_id.lock().clone().unwrap(),
                //         seq: *connection.last_seq_number.lock(),
                //     }))
                //     .await;
            }
        }
        GatewayReceiveEvent::Reconnect => {
            reconnect_and_resume(shard, connection).await;
        }
        GatewayReceiveEvent::HeartbeatAck => {
            *connection.has_acknowledged_last_heartbeat.lock() = true;
        }
        GatewayReceiveEvent::InvalidSession(InvalidSession { may_be_resumable }) => {
            if may_be_resumable {
                reconnect_and_resume(shard, connection).await;
            } else {
                reconnect_and_dont_resume(shard, connection).await;
            }
        }
    }
}

pub async fn run(shard: std::sync::Arc<Shard>) {
    // REMEMBER
    let shard2 = shard.clone();
    tokio::spawn(async move {
        let shard = shard2;

        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        log::info!("deliberately sabotaging connection to test resuming");
        shard
            .connection
            .lock()
            .await
            // .sender
            // .lock()
            // .await
            // .send(tungstenite::Message::Close(Some(tungstenite::protocol::CloseFrame {
            //     reason: "".into(),
            //     code: 1000.into(),
            // })))
            // .await
            // .unwrap();
            .send(GatewaySendEvent::Identify(Identify {
                intents: 0,
                properties: IdentifyProperties {
                    os: "".into(),
                    browser: "".into(),
                    device: "".into(),
                },
                token: "".into(),
            }))
            .await;
    });

    loop {
        tokio::select! {
            command = async {
                shard.command_receiver.lock().await.recv().await
            } => {
                match command.expect("sender can't have hung up because it's stored in Shard") {
                    ShardCommand::Shutdown => todo!(),
                }
            }
            msg = async {
                // Clone receiver out as to not lock the entire ShardConnection and block the
                // heartbeat loop
                let receiver = shard.connection.lock().await.receiver.clone();
                let msg = receiver.lock().await.next().await;
                msg
            } => match msg {
                Some(Ok(msg)) => match msg {
                    tungstenite::Message::Text(msg) => {
                        match serde_json::from_str(&msg) {
                            Ok(msg) => process_gateway_event(&shard, msg).await,
                            Err(e) => log::warn!("failed to deserialize websocket message: {}", e),
                        }
                    },
                    tungstenite::Message::Close(close_frame) => {
                        log::info!("received close frame, reconnecting: {:?}", close_frame);
                        reconnect_and_resume(&shard, &mut *shard.connection.lock().await).await;
                    }
                    other => log::info!("received other: {:?}", other),
                },
                other => {
                    log::warn!("either error or end of stream; either way we are gonna reconnect");
                    reconnect_and_resume(&shard, &mut *shard.connection.lock().await).await;
                }
                // Some(Err(e)) => log::warn!("shard error: {}", e),
                // None => {
                //     log::warn!("websocket stream exhausted, stopping shard");
                //     break;
                // },
            }
        }
    }
}
