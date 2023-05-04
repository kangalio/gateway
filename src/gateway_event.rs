#[derive(Debug)]
pub struct Dispatch {
    pub event_type: String,
    pub event_data: Box<serde_json::value::RawValue>,
    pub seq: u64,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct Heartbeat {
    pub last_seq_number: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct IdentifyProperties {
    pub os: &'static str,
    pub browser: &'static str,
    pub device: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub struct Identify {
    pub token: String,
    pub properties: IdentifyProperties,
    // STUB: other fields
    pub intents: u32,
}

#[derive(Debug, serde::Serialize)]
pub struct PresenceUpdate {
    // pub aaa: AAA,
}

#[derive(Debug, serde::Serialize)]
pub struct VoiceStateUpdate {
    // pub aaa: AAA,
}

#[derive(Debug, serde::Serialize)]
pub struct Resume {
    pub token: String,
    pub session_id: String,
    pub seq: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct RequestGuildMembers {
    // pub aaa: AAA,
}

#[derive(Debug, serde::Deserialize)]
#[serde(transparent)]
pub struct InvalidSession {
    pub may_be_resumable: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct Hello {
    pub heartbeat_interval: u64,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct RawGatewayEvent<T> {
    op: u32,
    d: T,
    s: Option<u64>,
    t: Option<String>,
}

#[derive(Debug)]
pub enum GatewaySendEvent {
    /// Fired periodically by the client to keep the connection alive.
    Heartbeat(Heartbeat),
    /// Starts a new session during the initial handshake.
    Identify(Identify),
    /// Update the client's presence.
    PresenceUpdate(PresenceUpdate),
    /// Used to join/leave or move between voice channels.
    VoiceStateUpdate(VoiceStateUpdate),
    /// Resume a previous session that was disconnected.
    Resume(Resume),
    /// Request information about offline guild members in a large guild.
    RequestGuildMembers(RequestGuildMembers),
}

impl serde::Serialize for GatewaySendEvent {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use GatewaySendEvent::*;

        match self {
            Heartbeat(d) => RawGatewayEvent { op: 1, d, s: None, t: None }.serialize(ser),
            Identify(d) => RawGatewayEvent { op: 2, d, s: None, t: None }.serialize(ser),
            PresenceUpdate(d) => RawGatewayEvent { op: 3, d, s: None, t: None }.serialize(ser),
            VoiceStateUpdate(d) => RawGatewayEvent { op: 4, d, s: None, t: None }.serialize(ser),
            Resume(d) => RawGatewayEvent { op: 6, d, s: None, t: None }.serialize(ser),
            RequestGuildMembers(d) => RawGatewayEvent { op: 8, d, s: None, t: None }.serialize(ser),
        }
    }
}

#[derive(Debug)]
pub enum GatewayReceiveEvent {
    /// An event was dispatched.
    Dispatch(Dispatch),
    /// You should immediately send another heartbeat without waiting the remainer of the current interval.
    Heartbeat,
    /// You should attempt to reconnect and resume immediately.
    Reconnect,
    /// The session has been invalidated. You should reconnect and identify/resume accordingly.
    InvalidSession(InvalidSession),
    /// Sent immediately after connecting, contains the heartbeat_interval to use.
    Hello(Hello),
    /// Sent in response to receiving a heartbeat to acknowledge that it has been received.
    HeartbeatAck,
}

impl<'de> serde::Deserialize<'de> for GatewayReceiveEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let event = RawGatewayEvent::<Box<serde_json::value::RawValue>>::deserialize(deserializer)?;
        Ok(match event.op {
            0 => Self::Dispatch(Dispatch {
                event_type: event.t.ok_or_else(|| D::Error::missing_field("t"))?,
                event_data: event.d,
                seq: event.s.ok_or_else(|| D::Error::missing_field("s"))?,
            }),
            1 => Self::Heartbeat,
            7 => Self::Reconnect,
            9 => {
                Self::InvalidSession(serde_json::from_str(event.d.get()).map_err(D::Error::custom)?)
            }
            10 => Self::Hello(serde_json::from_str(event.d.get()).map_err(D::Error::custom)?),
            11 => Self::HeartbeatAck,
            op => {
                return Err(D::Error::invalid_value(
                    serde::de::Unexpected::Unsigned(op as _),
                    &"valid gateway event (not send-only)",
                ))
            }
        })
    }
}
