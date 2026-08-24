//! Relay connection.
//!
//! Split into three small tasks — writer, reader, clock prober — so that no two
//! of them ever need the same mutable half of the socket. The reader owns the
//! clock sampler and publishes its estimate through atomics, which lets the sync
//! loops read the current relay time without awaiting anything.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::debug;
use ymsync_proto::{
    ClientMsg, ClockSampler, Command, Event, PROTOCOL_VERSION, ServerMsg, sample_from_roundtrip,
    unix_ms,
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = futures_util::stream::SplitSink<Ws, Message>;
type WsRead = futures_util::stream::SplitStream<Ws>;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Probes sent during startup, before the clock is needed for playback.
const PRIMING_PROBES: usize = 5;
const PRIMING_GAP: Duration = Duration::from_millis(50);

/// The relay's clock as seen from this machine.
#[derive(Debug, Default)]
pub struct Clock {
    offset_ms: AtomicI64,
    rtt_ms: AtomicI64,
    valid: AtomicBool,
}

impl Clock {
    fn publish(&self, offset_ms: i64, rtt_ms: i64) {
        self.offset_ms.store(offset_ms, Ordering::Relaxed);
        self.rtt_ms.store(rtt_ms, Ordering::Relaxed);
        self.valid.store(true, Ordering::Release);
    }

    fn publish_from(&self, sampler: &ClockSampler) {
        if let (Some(offset), Some(rtt)) = (sampler.offset_ms(), sampler.rtt_ms()) {
            self.publish(offset, rtt);
        }
    }

    /// Add to the local clock to get relay time.
    pub fn offset_ms(&self) -> Option<i64> {
        self.valid
            .load(Ordering::Acquire)
            .then(|| self.offset_ms.load(Ordering::Relaxed))
    }

    /// Best round trip seen recently: how much to trust [`Clock::offset_ms`].
    pub fn rtt_ms(&self) -> Option<i64> {
        self.valid
            .load(Ordering::Acquire)
            .then(|| self.rtt_ms.load(Ordering::Relaxed))
    }

    /// Relay time right now.
    pub fn now_server_ms(&self) -> Option<i64> {
        self.offset_ms().map(|offset| unix_ms() + offset)
    }
}

/// A live connection to a relay room.
///
/// Dropping it tears the connection down. When the connection dies on its own,
/// the paired event receiver ends, which is how the sync loops notice.
pub struct Link {
    out: mpsc::UnboundedSender<ClientMsg>,
    clock: Arc<Clock>,
    peers_at_join: usize,
    tasks: Vec<JoinHandle<()>>,
}

impl Link {
    pub async fn connect(
        relay: &str,
        room: &str,
        room_token: &str,
        probe_every: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let (ws, _response) = tokio_tungstenite::connect_async(relay)
            .await
            .with_context(|| format!("connecting to the relay at {relay}"))?;

        // Small state and clock frames must not wait on Nagle's algorithm.
        if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
            let _ = tcp.set_nodelay(true);
        }

        let (mut sink, mut read) = ws.split();

        send_msg(
            &mut sink,
            &ClientMsg::Hello {
                protocol: PROTOCOL_VERSION,
                room: room.to_string(),
                token: room_token.to_string(),
                client: client_name(),
            },
        )
        .await?;

        // Anything the relay says before the clock is ready. The room replay —
        // the sharing map, the queue, the state — arrives immediately after the
        // welcome, which is *during* the probes below, and dropping those frames
        // is exactly what made a returning peer show an empty queue: the relay
        // only resends a queue when it changes, and a station-fed room whose
        // feeder has left never changes it again.
        let mut replay: Vec<ServerMsg> = Vec::new();

        let peers_at_join = loop {
            match next_server_msg(&mut read, HANDSHAKE_TIMEOUT).await? {
                ServerMsg::Welcome { protocol, peers, .. } => {
                    if protocol != PROTOCOL_VERSION {
                        bail!(
                            "релей говорит на версии протокола {protocol}, эта сборка — \
                             на {PROTOCOL_VERSION}: обновите обе стороны"
                        );
                    }
                    break peers;
                }
                ServerMsg::Error { code, message } => {
                    bail!("релей отказал в подключении ({code}): {message}")
                }
                other => replay.push(other),
            }
        };

        // Establish the clock before anything depends on it.
        let mut sampler = ClockSampler::new();
        for _ in 0..PRIMING_PROBES {
            let c0 = unix_ms();
            send_msg(&mut sink, &ClientMsg::TimeReq { c0 }).await?;
            loop {
                match next_server_msg(&mut read, PROBE_TIMEOUT).await? {
                    ServerMsg::TimeRes { c0, s } => {
                        sampler.push(sample_from_roundtrip(c0, s, unix_ms()));
                        break;
                    }
                    ServerMsg::Error { code, message } => {
                        bail!("релей сообщил об ошибке ({code}): {message}")
                    }
                    other => replay.push(other),
                }
            }
            tokio::time::sleep(PRIMING_GAP).await;
        }

        let clock = Arc::new(Clock::default());
        clock.publish_from(&sampler);
        if clock.offset_ms().is_none() {
            bail!("не удалось оценить смещение часов относительно релея");
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // Held back frames go in ahead of anything the reader will see, keeping
        // the relay's order: the queue before the state that indexes into it.
        for msg in replay {
            if let Some(event) = as_event(msg)
                && event_tx.send(event).is_err()
            {
                break;
            }
        }

        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if send_msg(&mut sink, &msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reader = {
            let clock = Arc::clone(&clock);
            tokio::spawn(async move { read_loop(read, sampler, clock, event_tx).await })
        };

        let prober = {
            let out = out_tx.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(probe_every);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    // Stamped when queued rather than when written. The writer
                    // only ever carries a handful of tiny frames, so the
                    // difference stays far below the millisecond we measure in.
                    if out.send(ClientMsg::TimeReq { c0: unix_ms() }).is_err() {
                        break;
                    }
                }
            })
        };

        Ok((
            Self {
                out: out_tx,
                clock,
                peers_at_join,
                tasks: vec![writer, reader, prober],
            },
            event_rx,
        ))
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn peers_at_join(&self) -> usize {
        self.peers_at_join
    }

    /// Asks the relay to mutate the room. `false` means the connection is gone.
    ///
    /// Nothing is applied locally: the relay is what decides, and its answer
    /// comes back as [`Event::State`] like any other peer's would.
    #[must_use]
    pub fn send_command(&self, command: Command) -> bool {
        self.out.send(ClientMsg::Do { command }).is_ok()
    }

    /// Offers this machine's cached tracks to the rest of the room, or withdraws
    /// the offer with `port: None`.
    ///
    /// Only the port goes out. The address is composed by the relay from the
    /// socket this very message arrives on, which is the one address the room is
    /// known to be able to reach — a machine with a VPN, a virtual switch and
    /// Wi-Fi cannot pick that out from the inside.
    #[must_use]
    pub fn announce_share(&self, port: Option<u16>, tracks: Vec<String>) -> bool {
        self.out.send(ClientMsg::Share { port, tracks }).is_ok()
    }

    pub fn say_goodbye(&self) {
        let _ = self.out.send(ClientMsg::Bye);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn read_loop(
    mut read: WsRead,
    mut sampler: ClockSampler,
    clock: Arc<Clock>,
    events: mpsc::UnboundedSender<Event>,
) {
    while let Some(frame) = read.next().await {
        let msg = match frame {
            Ok(msg) => msg,
            Err(err) => {
                debug!(%err, "relay read failed");
                break;
            }
        };

        let text = match msg {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };

        match serde_json::from_str::<ServerMsg>(text.as_str()) {
            // Clock replies never reach the sync loop: they are the clock.
            Ok(ServerMsg::TimeRes { c0, s }) => {
                sampler.push(sample_from_roundtrip(c0, s, unix_ms()));
                clock.publish_from(&sampler);
            }
            Ok(msg) => {
                if let Some(event) = as_event(msg)
                    && events.send(event).is_err()
                {
                    break;
                }
            }
            Err(err) => debug!(%err, "undecodable frame from the relay"),
        }
    }
    // Dropping `events` here is what tells the sync loop the link is dead.
}

/// What the sync loop is told about a frame, if anything.
///
/// Shared with the handshake, which holds frames back until the event channel
/// exists: two copies of this mapping would drift, and the one in the handshake
/// is the one nobody notices — its frames only arrive when joining a room that is
/// already playing.
fn as_event(msg: ServerMsg) -> Option<Event> {
    match msg {
        ServerMsg::State { state } => Some(Event::State(state)),
        ServerMsg::Queue { revision, tracks } => Some(Event::Queue { revision, tracks }),
        ServerMsg::Peer { joined, peers } => Some(Event::Peer { joined, peers }),
        ServerMsg::Station { id, yours } => Some(Event::Station { id, yours }),
        ServerMsg::Shares { peers } => Some(Event::Shares { peers }),
        ServerMsg::Error { code, message } => Some(Event::Error { code, message }),
        // Handled where the connection is set up, and meaningless afterwards.
        ServerMsg::Welcome { .. } | ServerMsg::TimeRes { .. } => None,
    }
}

async fn send_msg(sink: &mut WsSink, msg: &ClientMsg) -> Result<()> {
    let text = serde_json::to_string(msg).context("encoding a relay message")?;
    sink.send(Message::text(text))
        .await
        .context("sending to the relay")?;
    Ok(())
}

async fn next_server_msg(read: &mut WsRead, timeout: Duration) -> Result<ServerMsg> {
    loop {
        let frame = tokio::time::timeout(timeout, read.next())
            .await
            .map_err(|_| anyhow!("релей не ответил за {timeout:?}"))?;

        match frame {
            None => bail!("релей закрыл соединение"),
            Some(Err(err)) => return Err(anyhow!(err).context("reading from the relay")),
            Some(Ok(Message::Close(_))) => bail!("релей закрыл соединение"),
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<ServerMsg>(text.as_str()) {
                    Ok(msg) => return Ok(msg),
                    Err(err) => debug!(%err, "undecodable frame from the relay"),
                }
            }
            Some(Ok(_)) => {}
        }
    }
}

fn client_name() -> String {
    format!(
        "ymsync/{} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_has_no_estimate_until_a_probe_lands() {
        let clock = Clock::default();
        assert_eq!(clock.offset_ms(), None);
        assert_eq!(clock.rtt_ms(), None);
        assert_eq!(clock.now_server_ms(), None);
    }

    #[test]
    fn clock_reports_the_published_estimate() {
        let clock = Clock::default();
        clock.publish(1_500, 20);
        assert_eq!(clock.offset_ms(), Some(1_500));
        assert_eq!(clock.rtt_ms(), Some(20));
        let now = clock.now_server_ms().expect("estimate");
        assert!((now - (unix_ms() + 1_500)).abs() < 50, "now was {now}");
    }

    #[test]
    fn an_empty_sampler_publishes_nothing() {
        let clock = Clock::default();
        clock.publish_from(&ClockSampler::new());
        assert_eq!(clock.offset_ms(), None);
    }
}
