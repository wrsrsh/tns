//! A native client for mosh's State Synchronization Protocol.
//!
//! tns speaks to a stock `mosh-server` itself instead of driving `mosh-client`
//! in a pty.  That gives the client loop what no wrapper can see: which of our
//! keystrokes the server has shown to the application (`echo_ack`), numbered
//! screen states, and the link's round-trip time.
//!
//! Layers, bottom up:
//! - datagram: AES-128-OCB3, an 8-byte direction+sequence nonce, and two
//!   16-bit millisecond timestamps for RTT estimation;
//! - fragments: a zlib-compressed protobuf `Instruction`, split to fit the MTU;
//! - state sync: each side sends "state NEW is state OLD plus this diff" and
//!   acknowledges the newest state it holds.  Ours is the stream of user
//!   input; the server's is its terminal, diffed as escape sequences.  OLD
//!   is usually the last state we acknowledged, not the previous one, so a
//!   diff is applied to a kept copy of that state rather than to the screen.
//!
//! This is an independent implementation of the wire protocol; the server
//! side is unmodified mosh.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aes::Aes128;
use ocb3::aead::{Aead, KeyInit};
use ocb3::Ocb3;

use crate::remote::ssh_base;
use crate::term::Screen;

const PROTOCOL_VERSION: u64 = 2;
/// State number that announces the end of a session.
const SHUTDOWN: u64 = u64::MAX;
const SEND_INTERVAL_MIN: f64 = 20.0;
const SEND_INTERVAL_MAX: f64 = 250.0;
const ACK_INTERVAL: Duration = Duration::from_millis(3000);
const ACK_DELAY: Duration = Duration::from_millis(100);
const ACTIVE_RETRY: Duration = Duration::from_secs(10);
const PORT_HOP: Duration = Duration::from_secs(10);
const REBIND: Duration = Duration::from_secs(1);
const SHUTDOWN_RETRIES: u32 = 16;
const QUENCH: Duration = Duration::from_secs(15);
/// Bytes of one fragment's contents; with headers, tag and nonce a datagram
/// stays below mosh's 500-byte application MTU.
const FRAGMENT: usize = 460;
/// Input one state may add, and input that may be in flight beyond the
/// acknowledged state.  A diff spans at most the window plus one state, far
/// below the 4 MiB at which mosh-server stops accepting an instruction; a
/// larger backlog (a huge paste, or typing into a dead link) goes out as
/// the acknowledgments come in.
const STATE_BYTES: usize = 64 << 10;
const WINDOW_BYTES: usize = 512 << 10;
const MAX_INFLATED: usize = 16 << 20;

// ---- protobuf (just the wire types these few messages use)

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_u64(out: &mut Vec<u8>, field: u32, v: u64) {
    put_varint(out, (field as u64) << 3);
    put_varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, v: &[u8]) {
    put_varint(out, (field as u64) << 3 | 2);
    put_varint(out, v.len() as u64);
    out.extend_from_slice(v);
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.0.split_first()?;
            self.0 = rest;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    /// Next (field number, value); `Some(None)` at a clean end.
    fn next(&mut self) -> Option<Option<(u32, Value<'a>)>> {
        if self.0.is_empty() {
            return Some(None);
        }
        let tag = self.varint()?;
        let value = match tag & 7 {
            0 => Value::Varint(self.varint()?),
            1 => Value::Bytes(self.take(8)?),
            2 => {
                let n = self.varint()? as usize;
                Value::Bytes(self.take(n)?)
            }
            5 => Value::Bytes(self.take(4)?),
            _ => return None,
        };
        Some(Some(((tag >> 3) as u32, value)))
    }
}

#[derive(Default, Debug, PartialEq)]
struct Instruction {
    old_num: u64,
    new_num: u64,
    ack_num: u64,
    throwaway_num: u64,
    diff: Vec<u8>,
}

impl Instruction {
    fn encode(&self, chaff: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.diff.len() + chaff.len() + 48);
        put_u64(&mut out, 1, PROTOCOL_VERSION);
        put_u64(&mut out, 2, self.old_num);
        put_u64(&mut out, 3, self.new_num);
        put_u64(&mut out, 4, self.ack_num);
        put_u64(&mut out, 5, self.throwaway_num);
        put_bytes(&mut out, 6, &self.diff);
        put_bytes(&mut out, 7, chaff);
        out
    }

    fn decode(data: &[u8]) -> Option<Instruction> {
        let mut inst = Instruction::default();
        let mut version = 0;
        let mut fields = Fields(data);
        while let Some((field, value)) = fields.next()? {
            match (field, value) {
                (1, Value::Varint(v)) => version = v,
                (2, Value::Varint(v)) => inst.old_num = v,
                (3, Value::Varint(v)) => inst.new_num = v,
                (4, Value::Varint(v)) => inst.ack_num = v,
                (5, Value::Varint(v)) => inst.throwaway_num = v,
                (6, Value::Bytes(b)) => inst.diff = b.to_vec(),
                _ => {}
            }
        }
        (version == PROTOCOL_VERSION).then_some(inst)
    }
}

/// One entry of our state: the ordered stream of everything the user did.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    Keys(Vec<u8>),
    Resize(usize, usize),
}

impl Action {
    fn bytes(&self) -> usize {
        match self {
            Action::Keys(keys) => keys.len(),
            Action::Resize(..) => 16,
        }
    }
}

/// The diff that appends `actions` to the server's copy of the stream.
fn encode_actions<'a>(actions: impl Iterator<Item = &'a Action>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut keys: Vec<u8> = Vec::new();
    let flush = |out: &mut Vec<u8>, keys: &mut Vec<u8>| {
        if !keys.is_empty() {
            let mut keystroke = Vec::with_capacity(keys.len() + 4);
            put_bytes(&mut keystroke, 4, keys);
            let mut inst = Vec::with_capacity(keystroke.len() + 4);
            put_bytes(&mut inst, 2, &keystroke);
            put_bytes(out, 1, &inst);
            keys.clear();
        }
    };
    for action in actions {
        match action {
            Action::Keys(k) => keys.extend_from_slice(k),
            Action::Resize(w, h) => {
                flush(&mut out, &mut keys);
                let mut resize = Vec::new();
                put_u64(&mut resize, 5, *w as u64);
                put_u64(&mut resize, 6, *h as u64);
                let mut inst = Vec::new();
                put_bytes(&mut inst, 3, &resize);
                put_bytes(&mut out, 1, &inst);
            }
        }
    }
    flush(&mut out, &mut keys);
    out
}

/// One step of a terminal diff from the server.
#[derive(Clone, Debug, PartialEq)]
enum HostOp {
    /// Escape sequences that turn the old screen into the new one.
    Bytes(Vec<u8>),
    Resize(usize, usize),
    /// Input up to this state number of ours has reached the application
    /// and had time to be echoed.
    EchoAck(u64),
}

fn decode_host(diff: &[u8]) -> Option<Vec<HostOp>> {
    let mut ops = Vec::new();
    let mut message = Fields(diff);
    while let Some((field, value)) = message.next()? {
        let (1, Value::Bytes(inst)) = (field, value) else { continue };
        let mut inst = Fields(inst);
        while let Some((field, value)) = inst.next()? {
            let Value::Bytes(body) = value else { continue };
            let mut body = Fields(body);
            match field {
                2 => {
                    while let Some((f, v)) = body.next()? {
                        if let (4, Value::Bytes(b)) = (f, v) {
                            ops.push(HostOp::Bytes(b.to_vec()));
                        }
                    }
                }
                3 => {
                    let (mut w, mut h) = (0, 0);
                    while let Some((f, v)) = body.next()? {
                        match (f, v) {
                            (5, Value::Varint(n)) => w = n as usize,
                            (6, Value::Varint(n)) => h = n as usize,
                            _ => {}
                        }
                    }
                    if (1..=1024).contains(&w) && (1..=1024).contains(&h) {
                        ops.push(HostOp::Resize(w, h));
                    }
                }
                7 => {
                    while let Some((f, v)) = body.next()? {
                        if let (8, Value::Varint(n)) = (f, v) {
                            ops.push(HostOp::EchoAck(n));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Some(ops)
}

// ---- fragments

/// Reassembles the fragments of the instruction currently in flight.
#[derive(Default)]
struct Assembly {
    id: Option<u64>,
    parts: Vec<Option<Vec<u8>>>,
    total: Option<usize>,
}

impl Assembly {
    fn add(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < 10 {
            return None;
        }
        let id = u64::from_be_bytes(packet[..8].try_into().unwrap());
        let word = u16::from_be_bytes([packet[8], packet[9]]);
        let (last, index) = (word & 0x8000 != 0, (word & 0x7fff) as usize);
        if self.id != Some(id) {
            *self = Assembly { id: Some(id), ..Default::default() };
        }
        if self.parts.len() <= index {
            self.parts.resize(index + 1, None);
        }
        self.parts[index] = Some(packet[10..].to_vec());
        if last {
            self.total = Some(index + 1);
        }
        let total = self.total?;
        if self.parts.len() != total || self.parts.iter().any(Option::is_none) {
            return None;
        }
        let whole = self.parts.drain(..).flatten().flatten().collect();
        self.total = None;
        Some(whole)
    }
}

fn fragments(id: u64, payload: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if payload.is_empty() { vec![payload] } else { payload.chunks(FRAGMENT).collect() };
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut f = Vec::with_capacity(chunk.len() + 10);
            f.extend_from_slice(&id.to_be_bytes());
            f.extend_from_slice(&((i as u16) | if i + 1 == n { 0x8000 } else { 0 }).to_be_bytes());
            f.extend_from_slice(chunk);
            f
        })
        .collect()
}

// ---- session key

pub fn parse_key(text: &str) -> Option<[u8; 16]> {
    if text.len() != 22 {
        return None;
    }
    let mut bits = 0u32;
    let mut have = 0;
    let mut out = Vec::with_capacity(17);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = bits << 6 | v as u32;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    out[..16].try_into().ok()
}

/// A terminal state the server has sent, as the screen it describes.
pub struct Remote {
    pub num: u64,
    pub screen: Screen,
    pub echo_ack: u64,
}

struct Sent {
    num: u64,
    at: Instant,
    end: u64, // number of actions this state includes
}

pub struct Client {
    sock: UdpSocket,
    addr: SocketAddr,
    cipher: Ocb3<Aes128>,
    t0: Instant,
    send_seq: u64,
    recv_seq: u64,
    saved_ts: Option<(u16, Instant)>,
    srtt: f64,
    rttvar: f64,
    rtt_known: bool,
    last_heard: Instant,
    heard: bool,
    last_hop: Instant,
    assembly: Assembly,
    next_fragment_id: u64,
    actions: VecDeque<Action>,
    base: u64, // index of actions[0] in the whole stream
    sent: VecDeque<Sent>,
    unsent_bytes: usize, // input queued behind the newest sent state
    dirty_since: Option<Instant>,
    next_ack: Instant,
    data_ack: Option<Instant>,
    states: Vec<Remote>,
    quench: Option<Instant>,
    closing: Option<(u32, Instant)>,
    peer_closed: bool,
    close_acks: u8,
    rng: u64,
}

impl Client {
    pub fn connect(addr: SocketAddr, key: &[u8; 16], now: Instant) -> io::Result<Client> {
        let mut seed = [0u8; 8];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut seed);
        }
        let mut initial = Screen::new(80, 24);
        initial.track_alt = false;
        // Not "just sent": the first real state must not wait out a send interval.
        let long_ago = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        Ok(Client {
            sock: Self::socket(addr)?,
            addr,
            cipher: Ocb3::new(&(*key).into()),
            t0: now,
            send_seq: 0,
            recv_seq: 0,
            saved_ts: None,
            srtt: 1000.0,
            rttvar: 500.0,
            rtt_known: false,
            last_heard: now,
            heard: false,
            last_hop: now,
            assembly: Assembly::default(),
            next_fragment_id: 0,
            actions: VecDeque::new(),
            base: 0,
            sent: VecDeque::from([Sent { num: 0, at: long_ago, end: 0 }]),
            unsent_bytes: 0,
            dirty_since: None,
            next_ack: now + ACK_INTERVAL,
            data_ack: None,
            states: vec![Remote { num: 0, screen: initial, echo_ack: 0 }],
            quench: None,
            closing: None,
            peer_closed: false,
            close_acks: 0,
            rng: u64::from_le_bytes(seed) | 1,
        })
    }

    fn socket(addr: SocketAddr) -> io::Result<UdpSocket> {
        let sock = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        sock.connect(addr)?;
        sock.set_nonblocking(true)?;
        Ok(sock)
    }

    pub fn fd(&self) -> libc::c_int {
        self.sock.as_raw_fd()
    }

    /// Smoothed round-trip time in seconds.
    pub fn srtt(&self) -> f64 {
        self.srtt / 1000.0
    }

    pub fn rtt_known(&self) -> bool {
        self.rtt_known
    }

    /// Time since the server was last heard, once it has been heard at all.
    pub fn silence(&self, now: Instant) -> Option<Duration> {
        self.heard.then(|| now.saturating_duration_since(self.last_heard))
    }

    pub fn connected(&self) -> bool {
        self.heard
    }

    pub fn latest(&self) -> &Remote {
        self.states.last().unwrap()
    }

    /// The server ended the session (its command exited).
    pub fn peer_closed(&self) -> bool {
        self.peer_closed && self.close_acks >= 2
    }

    /// Our own shutdown was acknowledged, or abandoned after its retries.
    pub fn closed(&self) -> bool {
        self.closing.is_some_and(|(tries, _)| tries >= SHUTDOWN_RETRIES) || (self.closing.is_some() && self.sent.front().is_some_and(|s| s.num == SHUTDOWN))
    }

    fn end(&self) -> u64 {
        self.base + self.actions.len() as u64
    }

    /// Returns the number of the state that will carry `action`: what is
    /// not yet sent travels together in the next new state.  (Behind a
    /// backlog of more than a state's worth it is the earliest number that
    /// can; acknowledgments sent in between take numbers too.)
    fn push(&mut self, action: Action, now: Instant) -> u64 {
        let states_ahead = (self.unsent_bytes / STATE_BYTES) as u64;
        self.unsent_bytes += action.bytes();
        self.actions.push_back(action);
        self.dirty_since.get_or_insert(now);
        self.sent.back().unwrap().num.saturating_add(1 + states_ahead)
    }

    fn span_bytes(&self, from: u64, to: u64) -> usize {
        self.actions.range((from - self.base) as usize..(to - self.base) as usize).map(Action::bytes).sum()
    }

    /// How far a state created now would reach into the queued input.
    fn next_end(&self) -> u64 {
        let sent = self.sent.back().unwrap().end;
        if sent == self.end() || self.span_bytes(self.sent[0].end, sent) >= WINDOW_BYTES {
            return sent;
        }
        let mut end = sent;
        let mut bytes = 0;
        for action in self.actions.range((sent - self.base) as usize..) {
            bytes += action.bytes();
            if end > sent && bytes > STATE_BYTES {
                break;
            }
            end += 1;
        }
        end
    }

    /// Queue typed bytes; returns the state number that will carry them.
    pub fn push_keys(&mut self, keys: &[u8], now: Instant) -> u64 {
        self.push(Action::Keys(keys.to_vec()), now)
    }

    pub fn push_resize(&mut self, cols: usize, rows: usize, now: Instant) {
        self.push(Action::Resize(cols, rows), now);
    }

    /// Tell the server to end the session; `closed()` reports completion.
    pub fn close(&mut self, now: Instant) {
        if self.closing.is_none() {
            let long_ago = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
            self.closing = Some((0, long_ago));
        }
    }

    fn send_interval(&self) -> Duration {
        Duration::from_secs_f64((self.srtt / 2.0).clamp(SEND_INTERVAL_MIN, SEND_INTERVAL_MAX) / 1000.0)
    }

    fn rto(&self) -> Duration {
        Duration::from_secs_f64((self.srtt + 4.0 * self.rttvar).ceil().clamp(50.0, 1000.0) / 1000.0)
    }

    fn ts16(&self, now: Instant) -> u16 {
        now.saturating_duration_since(self.t0).as_millis() as u16
    }

    /// The newest sent state the server has plausibly received: unacknowledged
    /// states older than a timeout are assumed lost.
    fn assumed(&self, now: Instant) -> usize {
        let grace = self.rto() + ACK_DELAY;
        let mut assumed = 0;
        for (i, s) in self.sent.iter().enumerate().skip(1) {
            if s.num == SHUTDOWN || now.saturating_duration_since(s.at) >= grace {
                break;
            }
            assumed = i;
        }
        assumed
    }

    fn send_time(&self, now: Instant) -> Option<Instant> {
        let back = self.sent.back().unwrap();
        let active = now.saturating_duration_since(self.last_heard) < ACTIVE_RETRY;
        if self.next_end() != back.end {
            Some(self.dirty_since.unwrap_or(now).max(back.at + self.send_interval()))
        } else if back.end != self.sent[self.assumed(now)].end && active {
            Some(back.at + self.send_interval())
        } else if back.end != self.sent[0].end && active {
            Some(back.at + self.rto() + ACK_DELAY)
        } else {
            None
        }
    }

    fn ack_time(&self) -> Instant {
        self.data_ack.map_or(self.next_ack, |t| t.min(self.next_ack))
    }

    /// When `tick` next has something to do.
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        if let Some((tries, last)) = self.closing {
            return (tries < SHUTDOWN_RETRIES && !self.closed()).then(|| last + self.send_interval());
        }
        if self.peer_closed {
            return (self.close_acks < 2).then_some(now);
        }
        Some(self.send_time(now).map_or(self.ack_time(), |t| t.min(self.ack_time())))
    }

    /// Send whatever is due: new input, a retransmission, or an acknowledgment.
    pub fn tick(&mut self, now: Instant) {
        if self.heard && now.saturating_duration_since(self.last_heard) > PORT_HOP && now.saturating_duration_since(self.last_hop) > PORT_HOP {
            // A new source port gets through NAT bindings that went stale.
            if let Ok(sock) = Self::socket(self.addr) {
                self.sock = sock;
            }
            self.last_hop = now;
        }
        if let Some((tries, last)) = self.closing {
            if tries < SHUTDOWN_RETRIES && now >= last + self.send_interval() && !self.closed() {
                // The final state is the newest one sent: input still queued
                // behind it is not going anywhere.
                let end = self.sent.back().unwrap().end;
                if self.sent.back().unwrap().num != SHUTDOWN {
                    self.sent.push_back(Sent { num: SHUTDOWN, at: now, end });
                }
                // Diff from the acknowledged state: the server certainly has it.
                let (old, from) = (self.sent[0].num, self.sent[0].end);
                self.transmit(now, old, SHUTDOWN, from, end);
                self.closing = Some((tries + 1, now));
            }
            return;
        }
        if self.peer_closed {
            if self.close_acks < 2 {
                let assumed = self.assumed(now);
                let (old, from) = (self.sent[assumed].num, self.sent[assumed].end);
                let back = self.sent.back().unwrap();
                let (num, end) = (back.num.saturating_add(1), back.end);
                self.transmit(now, old, num, from, end);
                self.close_acks += 1;
            }
            return;
        }
        let due_send = self.send_time(now).is_some_and(|t| now >= t);
        if !due_send && now < self.ack_time() {
            return;
        }
        let assumed = self.assumed(now);
        let (old, from) = (self.sent[assumed].num, self.sent[assumed].end);
        let end = self.next_end();
        let back = self.sent.back_mut().unwrap();
        let num = if end == back.end && from != end {
            // The same state again, for a receiver that may have missed it.
            back.at = now;
            back.num
        } else {
            // New input, or a bare acknowledgment (which is a state of its own).
            let num = back.num + 1;
            self.sent.push_back(Sent { num, at: now, end });
            if self.sent.len() > 32 {
                // Keep the acknowledged front and the recent tail.
                let middle = self.sent.len() - 16;
                self.sent.remove(middle);
            }
            self.unsent_bytes = self.span_bytes(end, self.end());
            num
        };
        self.transmit(now, old, num, from, end);
        // Input left behind the window keeps its own send timer running.
        self.dirty_since = (end != self.end()).then_some(now);
        self.data_ack = None;
        self.next_ack = now + ACK_INTERVAL;
    }

    /// Send "state `new_num` is state `old_num` plus the input in `from..to`".
    fn transmit(&mut self, now: Instant, old_num: u64, new_num: u64, from: u64, to: u64) {
        let inst = Instruction {
            old_num,
            new_num,
            ack_num: self.latest().num,
            throwaway_num: self.sent[0].num,
            diff: encode_actions(self.actions.range((from - self.base) as usize..(to - self.base) as usize)),
        };
        // Random padding hides how many keys an instruction carries.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let chaff = self.rng.to_le_bytes();
        let chaff = [chaff, chaff];
        let chaff = &chaff.as_flattened()[..(self.rng >> 60) as usize];
        let payload = miniz_oxide::deflate::compress_to_vec_zlib(&inst.encode(chaff), 6);
        self.next_fragment_id += 1;
        for fragment in fragments(self.next_fragment_id, &payload) {
            let datagram = self.seal(now, &fragment);
            let mut sent = self.sock.send(&datagram);
            for _ in 0..20 {
                // A full socket buffer in the middle of a large instruction
                // drains in no time; the fragments are no use by halves.
                let full = |e: &io::Error| e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::ENOBUFS);
                if !sent.as_ref().is_err_and(full) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
                sent = self.sock.send(&datagram);
            }
            if let Err(e) = sent {
                // The route or local address went away (new network, sleep):
                // a fresh socket takes whatever the system offers now.
                let gone = e.kind() != io::ErrorKind::WouldBlock && e.raw_os_error() != Some(libc::ENOBUFS);
                if gone && now.saturating_duration_since(self.last_hop) > REBIND {
                    if let Ok(sock) = Self::socket(self.addr) {
                        self.sock = sock;
                    }
                    self.last_hop = now;
                }
                return;
            }
        }
    }

    fn seal(&mut self, now: Instant, payload: &[u8]) -> Vec<u8> {
        let seq = self.send_seq; // top bit clear: client to server
        self.send_seq += 1;
        let reply = match self.saved_ts.take() {
            Some((ts, at)) => ts.wrapping_add(now.saturating_duration_since(at).as_millis() as u16),
            None => u16::MAX,
        };
        let mut plain = Vec::with_capacity(payload.len() + 4);
        plain.extend_from_slice(&self.ts16(now).to_be_bytes());
        plain.extend_from_slice(&reply.to_be_bytes());
        plain.extend_from_slice(payload);
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&seq.to_be_bytes());
        let mut out = seq.to_be_bytes().to_vec();
        out.extend(self.cipher.encrypt(&nonce.into(), plain.as_slice()).expect("OCB encryption cannot fail"));
        out
    }

    fn open(&mut self, now: Instant, datagram: &[u8]) -> Option<Vec<u8>> {
        if datagram.len() < 8 + 16 + 4 {
            return None;
        }
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&datagram[..8]);
        let seq = u64::from_be_bytes(datagram[..8].try_into().unwrap());
        if seq >> 63 != 1 {
            return None; // not addressed to a client
        }
        let plain = self.cipher.decrypt(&nonce.into(), &datagram[8..]).ok()?;
        let seq = seq & !(1 << 63);
        if seq >= self.recv_seq {
            // Timestamps of reordered packets would corrupt the estimate.
            self.recv_seq = seq + 1;
            let ts = u16::from_be_bytes([plain[0], plain[1]]);
            let reply = u16::from_be_bytes([plain[2], plain[3]]);
            self.saved_ts = Some((ts, now));
            if reply != u16::MAX {
                let r = self.ts16(now).wrapping_sub(reply) as f64;
                if r < 5000.0 {
                    if !self.rtt_known {
                        self.srtt = r;
                        self.rttvar = r / 2.0;
                        self.rtt_known = true;
                    } else {
                        self.rttvar = 0.75 * self.rttvar + 0.25 * (self.srtt - r).abs();
                        self.srtt = 0.875 * self.srtt + 0.125 * r;
                    }
                }
            }
            // A replayed or stale datagram is no sign of life.
            self.last_heard = now;
            self.heard = true;
        }
        Some(plain[4..].to_vec())
    }

    /// Read every waiting datagram; `latest` is the newest state they built.
    pub fn recv(&mut self, now: Instant) {
        let mut buf = [0u8; 65536];
        loop {
            let n = match self.sock.recv(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // ICMP errors surface here on a connected socket; the server
                // may simply be unreachable for now.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                Err(_) => return,
            };
            self.datagram(now, &buf[..n]);
        }
    }

    fn datagram(&mut self, now: Instant, datagram: &[u8]) {
        let Some(fragment) = self.open(now, datagram) else { return };
        let Some(packed) = self.assembly.add(&fragment) else { return };
        let Ok(plain) = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&packed, MAX_INFLATED) else { return };
        let Some(inst) = Instruction::decode(&plain) else { return };
        self.instruction(now, inst);
    }

    fn instruction(&mut self, now: Instant, inst: Instruction) {
        if let Some(i) = self.sent.iter().position(|s| s.num == inst.ack_num) {
            self.sent.drain(..i);
            let acked = (self.sent[0].end - self.base) as usize;
            self.actions.drain(..acked);
            self.base += acked as u64;
        }
        if self.states.iter().any(|s| s.num == inst.new_num) {
            return;
        }
        let Some(base) = self.states.iter().position(|s| s.num == inst.old_num) else { return };
        let Some(ops) = decode_host(&inst.diff) else { return };
        let base = &self.states[base];
        let mut next = Remote { num: inst.new_num, screen: base.screen.clone(), echo_ack: base.echo_ack };
        // The server will not refer to anything older again.
        self.states.retain(|s| s.num >= inst.throwaway_num);
        if self.states.len() > 1024 {
            // A server far ahead of our acknowledgments: let it catch up.
            if self.quench.is_some_and(|t| now < t) {
                return;
            }
            self.quench = Some(now + QUENCH);
        }
        for op in &ops {
            match op {
                HostOp::Bytes(bytes) => vte::Parser::new().advance(&mut next.screen, bytes),
                HostOp::Resize(w, h) => next.screen.resize(*w, *h),
                HostOp::EchoAck(n) => next.echo_ack = *n,
            }
        }
        if let Some(i) = self.states.iter().position(|s| s.num > inst.new_num) {
            // Late arrival of a state we have already moved past.
            self.states.insert(i, next);
            return;
        }
        self.states.push(next);
        if inst.new_num == SHUTDOWN {
            self.peer_closed = true;
        }
        if !inst.diff.is_empty() {
            self.data_ack.get_or_insert(now + ACK_DELAY);
        }
    }
}

/// Where a started `mosh-server` listens, and its session key.
pub struct Endpoint {
    pub addr: SocketAddr,
    pub key: [u8; 16],
}

/// UTF-8 locale settings to hand to mosh-server, which refuses to start
/// without one and may get none from a non-interactive ssh session: ours if
/// it is one, then two that most systems have.
fn locale_args() -> Vec<String> {
    let safe = |v: &str| v.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b));
    let set = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty() && safe(v)).map(|v| (name.to_string(), v));
    let utf8 = |v: &str| v.to_ascii_lowercase().replace('-', "").contains("utf8");
    let mut choices = Vec::new();
    // LC_ALL beats LC_CTYPE beats LANG: only the winner says what applies.
    if ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|n| set(n)).is_some_and(|(_, v)| utf8(&v)) {
        choices.push(["LANG", "LC_CTYPE", "LC_ALL"].iter().filter_map(|n| set(n)).map(|(n, v)| format!(" -l {}={}", n, v)).collect());
    }
    choices.extend([" -l LC_ALL=C.UTF-8".to_string(), " -l LC_ALL=en_US.UTF-8".to_string()]);
    choices
}

fn parse_connect(output: &str) -> Option<(u16, [u8; 16])> {
    let line = output.lines().find_map(|l| l.trim().strip_prefix("MOSH CONNECT "))?;
    let (port, key) = line.split_once(' ')?;
    Some((port.parse().ok()?, parse_key(key.trim())?))
}

/// The address ssh itself would connect to for `host` (aliases resolved).
fn ssh_target(host: &str) -> String {
    let fallback = host.rsplit('@').next().unwrap_or(host).to_string();
    let Ok(out) = Command::new("ssh").arg("-G").arg(host).stdin(Stdio::null()).stderr(Stdio::null()).output() else { return fallback };
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix("hostname ")).map_or(fallback, |h| h.trim().to_string())
}

/// Choose the UDP address: the one ssh reached when it is one of ours,
/// otherwise (NAT, port forwarding) an address of the same family.
fn pick_addr(candidates: &[SocketAddr], seen_by_server: Option<IpAddr>) -> Option<SocketAddr> {
    let seen = seen_by_server?;
    let mut all = candidates.iter().copied();
    all.clone().find(|c| c.ip() == seen).or_else(|| all.find(|c| c.is_ipv4() == seen.is_ipv4()))
}

/// The server address in ssh's `SSH_CONNECTION` (client, port, server, port).
fn server_seen(output: &str) -> Option<IpAddr> {
    let conn = output.lines().find_map(|l| l.strip_prefix("TNS_CONN="))?;
    // A link-local address comes with its interface: "fe80::1%eth0".
    conn.split_whitespace().nth(2)?.split('%').next()?.parse().ok()
}

/// Start `mosh-server` on `host` over ssh, running `command`.
pub fn start_server(host: &str, command: &[String]) -> io::Result<Endpoint> {
    let fail = |msg: String| io::Error::new(io::ErrorKind::Other, msg);
    let base = ssh_base(host);
    let mut failure = String::new();
    for locale in locale_args() {
        // Sent to `sh -s`, so the login shell never parses it.  stdin is not
        // a terminal there, which makes the server start from the 80x24
        // screen both sides assume as state zero.
        let script = format!("echo TNS_CONN=$SSH_CONNECTION\nexec mosh-server new -s -c 256{} -- {}\n", locale, command.join(" "));
        let mut child = Command::new(&base[0]).args(&base[1..]).arg("sh").arg("-s").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some((port, key)) = parse_connect(&stdout) else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if failure.is_empty() {
                failure = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("no reply from mosh-server").to_string();
            }
            if stderr.contains("UTF-8") {
                continue; // that locale does not exist there: try the next
            }
            break;
        };
        let seen = server_seen(&stdout);
        let target = ssh_target(host);
        // Resolved addresses keep what a bare IP cannot say (an IPv6 scope).
        let candidates: Vec<SocketAddr> = (target.as_str(), port).to_socket_addrs().map(Iterator::collect).unwrap_or_default();
        let addr = pick_addr(&candidates, seen).or(candidates.first().copied()).or(seen.map(|ip| SocketAddr::new(ip, port)));
        return Ok(Endpoint { addr: addr.ok_or_else(|| fail(format!("cannot resolve {}", target)))?, key });
    }
    Err(fail(format!("mosh-server did not start: {}", failure)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real mosh-server on loopback; None when mosh is not installed.
    fn local_server(script: &str) -> Option<Endpoint> {
        // The timeout reaps the server if a failing test abandons it.
        let out = Command::new("mosh-server").env("MOSH_SERVER_NETWORK_TMOUT", "20").args(["new", "-i", "127.0.0.1", "-c", "256", "-l", "LANG=en_US.UTF-8", "--", "sh", "-c", script]).stdin(Stdio::null()).output().ok()?;
        let (port, key) = parse_connect(&String::from_utf8_lossy(&out.stdout))?;
        Some(Endpoint { addr: SocketAddr::new("127.0.0.1".parse().unwrap(), port), key })
    }

    fn pump(client: &mut Client, mut done: impl FnMut(&mut Client) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let now = Instant::now();
            client.recv(now);
            client.tick(now);
            if done(client) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// The screen's non-blank rows, joined: where the server first puts
    /// output is its own business.
    fn text(client: &Client) -> String {
        let g = &client.latest().screen.grid;
        let rows = (0..g.rows).map(|y| g.row(y).iter().filter_map(|c| c.chr()).collect::<String>().trim_end().to_string());
        rows.filter(|r| !r.is_empty()).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn talks_to_a_real_mosh_server() {
        let Some(endpoint) = local_server("stty -echo; printf 'ready> '; while read -r line; do printf '<%s>\\n' \"$line\"; done") else {
            eprintln!("mosh-server not installed; skipping interoperability test");
            return;
        };
        let mut client = Client::connect(endpoint.addr, &endpoint.key, Instant::now()).unwrap();
        client.push_resize(100, 30, Instant::now());
        assert!(pump(&mut client, |c| c.latest().screen.grid.cols == 100 && text(c) == "ready>"), "no first screen: {:?}", text(&client));
        assert_eq!(client.latest().screen.grid.rows, 30);
        assert!(client.rtt_known() && client.srtt() < 0.5);

        // The echo acknowledgment covers the state that carried the keys.
        let num = client.push_keys(b"hello\r", Instant::now());
        assert!(pump(&mut client, |c| text(c) == "ready> <hello>"), "no reply: {:?}", text(&client));
        assert!(pump(&mut client, |c| c.latest().echo_ack >= num));

        // A paste larger than one datagram is fragmented and reassembled.
        // (Incompressible, and within a tty's canonical line limit.)
        let mut x = 12345u32;
        let paste: String = (0..900).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); (b'a' + (x >> 24) as u8 % 26) as char }).collect();
        client.push_keys(paste.as_bytes(), Instant::now());
        client.push_keys(b"\r", Instant::now());
        let tail = format!("{}>", &paste[880..]);
        assert!(pump(&mut client, |c| text(c).replace('|', "").ends_with(&tail)), "no paste: {:?}", text(&client));

        // End of input ends the remote command, which ends the session.
        client.push_keys(b"\x04", Instant::now());
        assert!(pump(&mut client, |c| c.peer_closed()));
    }

    #[test]
    fn a_paste_larger_than_the_servers_instruction_limit_arrives_whole() {
        // mosh-server drops an instruction that inflates to more than 4 MiB.
        const SIZE: usize = 5_000_000;
        let Some(endpoint) = local_server(&format!("stty raw -echo; printf 'go:'; head -c {SIZE} | wc -c")) else { return };
        let mut client = Client::connect(endpoint.addr, &endpoint.key, Instant::now()).unwrap();
        client.push_resize(80, 24, Instant::now());
        assert!(pump(&mut client, |c| text(c) == "go:"));
        let mut x = 1u32;
        let chunk: Vec<u8> = (0..4096).map(|_| { x = x.wrapping_mul(1664525).wrapping_add(1013904223); b'a' + (x >> 24) as u8 % 26 }).collect();
        for sent in (0..SIZE).step_by(4096) {
            client.push_keys(&chunk[..chunk.len().min(SIZE - sent)], Instant::now());
        }
        assert!(pump(&mut client, |c| text(c).replace(' ', "") == format!("go:{SIZE}")), "{:?}", text(&client));
    }

    #[test]
    fn our_shutdown_is_acknowledged() {
        let Some(endpoint) = local_server("exec cat") else { return };
        let mut client = Client::connect(endpoint.addr, &endpoint.key, Instant::now()).unwrap();
        client.push_resize(80, 24, Instant::now());
        assert!(pump(&mut client, |c| c.connected() && c.latest().num > 0));
        client.close(Instant::now());
        assert!(pump(&mut client, |c| c.closed()));
        assert!(client.sent.front().unwrap().num == SHUTDOWN, "shutdown was not acknowledged");
    }

    #[test]
    fn key_is_unpadded_base64() {
        assert_eq!(parse_key("AAAAAAAAAAAAAAAAAAAAAA"), Some([0; 16]));
        assert_eq!(parse_key("/////////////////////w"), Some([0xff; 16]));
        assert_eq!(parse_key("AAECAwQFBgcICQoLDA0ODw"), Some(std::array::from_fn(|i| i as u8)));
        assert_eq!(parse_key("short"), None);
        assert_eq!(parse_key("AAAAAAAAAAAAAAAAAAAA=="), None);
    }

    #[test]
    fn ocb_matches_rfc_7253() {
        // RFC 7253 appendix A, with its 96-bit nonces and 128-bit tags.
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let cipher: Ocb3<Aes128> = Ocb3::new(&key.into());
        let hex = |s: &str| (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let nonce: [u8; 12] = hex("BBAA99887766554433221100").try_into().unwrap();
        assert_eq!(cipher.encrypt(&nonce.into(), b"".as_slice()).unwrap(), hex("785407BFFFC8AD9EDCC5520AC9111EE6"));
        let nonce: [u8; 12] = hex("BBAA99887766554433221103").try_into().unwrap();
        assert_eq!(cipher.encrypt(&nonce.into(), hex("0001020304050607").as_slice()).unwrap(), hex("45DD69F8F5AAE72414054CD1F35D82760B2CD00D2F99BFA9"));
    }

    #[test]
    fn instruction_round_trips_and_rejects_other_versions() {
        let inst = Instruction { old_num: 3, new_num: 300, ack_num: u64::MAX, throwaway_num: 2, diff: vec![1, 2, 3] };
        let wire = inst.encode(b"chaff");
        assert_eq!(Instruction::decode(&wire), Some(inst));
        let mut other = Vec::new();
        put_u64(&mut other, 1, 3);
        assert_eq!(Instruction::decode(&other), None);
        assert_eq!(Instruction::decode(&wire[..wire.len() - 2]), None);
    }

    #[test]
    fn typed_bytes_coalesce_around_resizes() {
        let actions = [Action::Keys(b"l".to_vec()), Action::Keys(b"s".to_vec()), Action::Resize(120, 40), Action::Keys(b"\r".to_vec())];
        let wire = encode_actions(actions.iter());
        // UserMessage{ Instruction{ keystroke{ keys } }, Instruction{ resize }, Instruction{ keystroke } }
        assert_eq!(wire, b"\x0a\x06\x12\x04\x22\x02ls\x0a\x06\x1a\x04\x28\x78\x30\x28\x0a\x05\x12\x03\x22\x01\r");
    }

    #[test]
    fn host_diff_decodes_in_order() {
        let mut wire = Vec::new();
        for (field, body) in [(7u32, b"\x40\x05".as_slice()), (3, b"\x28\x50\x30\x18"), (2, b"\x22\x03abc")] {
            let mut inst = Vec::new();
            put_bytes(&mut inst, field, body);
            put_bytes(&mut wire, 1, &inst);
        }
        assert_eq!(decode_host(&wire), Some(vec![HostOp::EchoAck(5), HostOp::Resize(80, 24), HostOp::Bytes(b"abc".to_vec())]));
        assert_eq!(decode_host(&wire[..wire.len() - 1]), None);
    }

    #[test]
    fn fragments_reassemble_in_any_order_and_restart_on_a_new_id() {
        let payload: Vec<u8> = (0..1200u32).map(|i| i as u8).collect();
        let parts = fragments(7, &payload);
        assert_eq!(parts.len(), 3);
        let mut asm = Assembly::default();
        assert_eq!(asm.add(&parts[2]), None);
        assert_eq!(asm.add(&fragments(6, b"stale")[0]).as_deref(), Some(b"stale".as_slice()));
        assert_eq!(asm.add(&parts[2]), None);
        assert_eq!(asm.add(&parts[0]), None);
        assert_eq!(asm.add(&parts[0]), None);
        assert_eq!(asm.add(&parts[1]), Some(payload));
        assert_eq!(asm.add(&fragments(8, b"")[0]), Some(Vec::new()));
    }

    #[test]
    fn udp_address_prefers_what_ssh_reached() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let at = |s: &str| SocketAddr::new(ip(s), 60001);
        let candidates = [at("2001:db8::1"), at("203.0.113.9")];
        assert_eq!(pick_addr(&candidates, Some(ip("203.0.113.9"))), Some(at("203.0.113.9")));
        // Behind NAT the server sees a private address: keep the family.
        assert_eq!(pick_addr(&candidates, Some(ip("10.0.0.5"))), Some(at("203.0.113.9")));
        assert_eq!(pick_addr(&candidates, None), None);
        assert_eq!(parse_connect("TNS_CONN=1.2.3.4 5 6.7.8.9 22\n\nMOSH CONNECT 60001 AAECAwQFBgcICQoLDA0ODw\n").map(|c| c.0), Some(60001));
        assert_eq!(server_seen("TNS_CONN=1.2.3.4 5 6.7.8.9 22\n"), Some(ip("6.7.8.9")));
        // A link-local server address keeps the scope of the resolved one.
        assert_eq!(server_seen("TNS_CONN=fe80::2%en0 5 fe80::1%en0 22\n"), Some(ip("fe80::1")));
        let scoped = SocketAddr::V6(std::net::SocketAddrV6::new("fe80::1".parse().unwrap(), 60001, 0, 7));
        assert_eq!(pick_addr(&[at("192.0.2.1"), scoped], Some(ip("fe80::1"))), Some(scoped));
    }

    /// A client whose datagrams go nowhere, for the sender's own logic.
    fn unconnected() -> (Client, UdpSocket, Instant) {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let now = Instant::now();
        (Client::connect(sink.local_addr().unwrap(), &[7; 16], now).unwrap(), sink, now)
    }

    #[test]
    fn a_large_backlog_is_sent_a_window_at_a_time() {
        let (mut client, _sink, start) = unconnected();
        let first = client.push_keys(&[b'x'; 4096], start);
        for _ in 1..512 {
            client.push_keys(&[b'x'; 4096], start); // 2 MiB in all
        }
        let late = client.push_keys(b"y", start);
        assert!(first == 1 && late > 30, "{first} {late}");
        // Unacknowledged, the sender stops at the window, however long it
        // keeps retransmitting.
        let mut now = start;
        for _ in 0..400 {
            now += Duration::from_millis(50);
            client.tick(now);
        }
        let in_flight = client.span_bytes(client.sent[0].end, client.sent.back().unwrap().end);
        assert!((WINDOW_BYTES..=WINDOW_BYTES + STATE_BYTES).contains(&in_flight), "{in_flight}");
        assert!(client.sent.len() <= 32 && client.unsent_bytes > 1 << 20);
        // Each acknowledgment lets the next part out, down to the last key.
        let mut acks = 0;
        while client.end() != client.sent[0].end {
            let newest = client.sent.back().unwrap().num;
            client.instruction(now, Instruction { ack_num: newest, ..Instruction::default() });
            now += Duration::from_millis(50);
            client.tick(now);
            acks += 1;
            assert!(acks < 200, "the backlog never drained");
        }
        assert_eq!((client.actions.len(), client.unsent_bytes), (0, 0));
        assert!(client.sent[0].num >= late);
    }

    #[test]
    fn replayed_datagrams_are_not_a_sign_of_life() {
        let (mut client, _sink, start) = unconnected();
        // What the server would send: same key, direction bit set.
        let seal = |seq: u64, client: &Client| {
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&(seq | 1 << 63).to_be_bytes());
            let mut datagram = (seq | 1 << 63).to_be_bytes().to_vec();
            datagram.extend(client.cipher.encrypt(&nonce.into(), [0u8, 0, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 9, 0x80, 0].as_slice()).unwrap());
            datagram
        };
        let recorded = seal(5, &client);
        assert!(client.open(start, &recorded).is_some() && client.connected());
        let later = start + Duration::from_secs(8);
        assert!(client.open(later, &recorded).is_some());
        assert_eq!(client.silence(later), Some(Duration::from_secs(8)));
        assert!(client.open(later, &seal(6, &client)).is_some());
        assert_eq!(client.silence(later), Some(Duration::ZERO));
        // Our own datagrams (direction bit clear) and garbage are refused.
        let own = client.seal(later, b"fragment");
        assert!(client.open(later, &own).is_none());
        assert!(client.open(later, &recorded[..20]).is_none());
    }

    #[test]
    fn locale_for_the_server_is_a_utf8_one() {
        for choice in locale_args() {
            assert!(choice.to_ascii_lowercase().replace('-', "").contains("utf8"), "{choice}");
        }
    }
}
