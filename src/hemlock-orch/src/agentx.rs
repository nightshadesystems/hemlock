//! AgentX (RFC 2741) wire format: PDU encode/decode and the value
//! types the IF-MIB needs.
//!
//! Deliberately partial. A subagent that only *serves* a read-only MIB
//! needs Open, Register, Response and the three retrieval PDUs; Set
//! (TestSet/CommitSet/UndoSet/CleanupSet) is answered with
//! `notWritable`, and notifications are out of scope with SNMP traps.
//!
//! Everything here is pure: bytes in, bytes out. The socket loop and
//! the MIB itself live in `snmpsub`, so both can be tested without
//! either.

// The AgentX wire format and the IF-MIB are only *driven* by the
// Linux-only master session (and by the tests, which run everywhere).
// On other hosts they are dead code we still want compiled and tested,
// so the dev-host build does not report the whole module unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// AgentX protocol version.
pub const VERSION: u8 = 1;

/// The fixed PDU header size.
pub const HEADER_LEN: usize = 20;

/// `NETWORK_BYTE_ORDER` — every PDU this subagent emits is big-endian.
const FLAG_NETWORK_BYTE_ORDER: u8 = 0x10;
/// Set on a SearchRange's start OID: the range includes it.
const OID_INCLUDE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduType {
    Open,
    Close,
    Register,
    Unregister,
    Get,
    GetNext,
    GetBulk,
    TestSet,
    CommitSet,
    UndoSet,
    CleanupSet,
    Notify,
    Ping,
    Response,
    /// Anything this subagent does not implement, kept so a stray PDU
    /// can still be answered rather than desynchronising the stream.
    Other(u8),
}

impl PduType {
    pub fn code(self) -> u8 {
        match self {
            PduType::Open => 1,
            PduType::Close => 2,
            PduType::Register => 3,
            PduType::Unregister => 4,
            PduType::Get => 5,
            PduType::GetNext => 6,
            PduType::GetBulk => 7,
            PduType::TestSet => 8,
            PduType::CommitSet => 9,
            PduType::UndoSet => 10,
            PduType::CleanupSet => 11,
            PduType::Notify => 12,
            PduType::Ping => 13,
            PduType::Response => 18,
            PduType::Other(code) => code,
        }
    }

    pub fn from_code(code: u8) -> Self {
        match code {
            1 => PduType::Open,
            2 => PduType::Close,
            3 => PduType::Register,
            4 => PduType::Unregister,
            5 => PduType::Get,
            6 => PduType::GetNext,
            7 => PduType::GetBulk,
            8 => PduType::TestSet,
            9 => PduType::CommitSet,
            10 => PduType::UndoSet,
            11 => PduType::CleanupSet,
            12 => PduType::Notify,
            13 => PduType::Ping,
            18 => PduType::Response,
            other => PduType::Other(other),
        }
    }
}

/// RFC 2741 §6.2.16 error codes (only the ones this subagent returns).
pub const ERROR_NONE: u16 = 0;
pub const ERROR_NOT_WRITABLE: u16 = 17;

/// A decoded PDU header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub pdu_type: PduType,
    pub flags: u8,
    pub session_id: u32,
    pub transaction_id: u32,
    pub packet_id: u32,
    pub payload_len: u32,
}

impl Header {
    /// The payload length of a framed PDU, from its first 20 bytes.
    /// None when the header is short, the version is wrong, or the
    /// sender is little-endian (net-snmp's master is not).
    pub fn parse(bytes: &[u8]) -> Option<Header> {
        if bytes.len() < HEADER_LEN || bytes[0] != VERSION {
            return None;
        }
        let flags = bytes[2];
        if flags & FLAG_NETWORK_BYTE_ORDER == 0 {
            return None;
        }
        let word = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Some(Header {
            pdu_type: PduType::from_code(bytes[1]),
            flags,
            session_id: word(4),
            transaction_id: word(8),
            packet_id: word(12),
            payload_len: word(16),
        })
    }
}

/// One MIB value. The IF-MIB needs exactly these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Integer(i32),
    OctetString(Vec<u8>),
    Counter32(u32),
    Gauge32(u32),
    TimeTicks(u32),
    Counter64(u64),
    /// A read that fell off the end of the registered subtree.
    EndOfMibView,
    /// The OID names a column that exists but has no instance here.
    NoSuchInstance,
    /// Nothing in the MIB starts with this OID.
    NoSuchObject,
}

impl Value {
    fn tag(&self) -> u16 {
        match self {
            Value::Integer(_) => 2,
            Value::OctetString(_) => 4,
            Value::Counter32(_) => 65,
            Value::Gauge32(_) => 66,
            Value::TimeTicks(_) => 67,
            Value::Counter64(_) => 70,
            Value::NoSuchObject => 128,
            Value::NoSuchInstance => 129,
            Value::EndOfMibView => 130,
        }
    }
}

/// An object identifier as its sub-identifiers.
pub type Oid = Vec<u32>;

/// One (name, value) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarBind {
    pub name: Oid,
    pub value: Value,
}

/// One GetNext/GetBulk search range: walk from `start` (inclusive when
/// `include`) up to but not including `end` (empty = unbounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRange {
    pub start: Oid,
    pub include: bool,
    pub end: Oid,
}

/// A decoded request from the master agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub header: Header,
    pub ranges: Vec<SearchRange>,
    /// GetBulk only.
    pub non_repeaters: u16,
    pub max_repetitions: u16,
}

// ---------------------------------------------------------------- encode

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// An OID: n_subid, prefix, include, reserved, then the sub-ids.
///
/// The `prefix` shorthand (a leading `1.3.6.1.<prefix>`) is deliberately
/// not used on the wire — spelling every sub-id out costs a few bytes
/// and removes a whole class of off-by-one.
fn push_oid(out: &mut Vec<u8>, oid: &[u32], include: bool) {
    let count = u8::try_from(oid.len()).unwrap_or(u8::MAX);
    out.push(count);
    out.push(0);
    out.push(if include { OID_INCLUDE } else { 0 });
    out.push(0);
    for sub in oid.iter().take(usize::from(count)) {
        push_u32(out, *sub);
    }
}

/// An octet string: length, bytes, then padding to a 4-byte boundary.
fn push_octets(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(bytes);
    let padding = (4 - bytes.len() % 4) % 4;
    out.extend(std::iter::repeat_n(0u8, padding));
}

fn push_varbind(out: &mut Vec<u8>, bind: &VarBind) {
    push_u16(out, bind.value.tag());
    push_u16(out, 0);
    push_oid(out, &bind.name, false);
    match &bind.value {
        Value::Integer(value) => push_u32(out, *value as u32),
        Value::Counter32(value) | Value::Gauge32(value) | Value::TimeTicks(value) => {
            push_u32(out, *value)
        }
        Value::Counter64(value) => out.extend_from_slice(&value.to_be_bytes()),
        Value::OctetString(bytes) => push_octets(out, bytes),
        // The exception markers carry no value at all.
        Value::EndOfMibView | Value::NoSuchInstance | Value::NoSuchObject => {}
    }
}

/// Frame a PDU: the 20-byte header followed by `payload`.
pub fn frame(
    pdu_type: PduType,
    session_id: u32,
    transaction_id: u32,
    packet_id: u32,
    payload: Vec<u8>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push(VERSION);
    out.push(pdu_type.code());
    out.push(FLAG_NETWORK_BYTE_ORDER);
    out.push(0);
    push_u32(&mut out, session_id);
    push_u32(&mut out, transaction_id);
    push_u32(&mut out, packet_id);
    push_u32(&mut out, u32::try_from(payload.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(&payload);
    out
}

/// An `agentx-Open-PDU`: session timeout, the subagent's id OID, and
/// its description.
pub fn open_pdu(packet_id: u32, id: &[u32], description: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    // Timeout in seconds for requests on this session.
    payload.push(5);
    payload.extend_from_slice(&[0, 0, 0]);
    push_oid(&mut payload, id, false);
    push_octets(&mut payload, description.as_bytes());
    frame(PduType::Open, 0, 0, packet_id, payload)
}

/// An `agentx-Register-PDU` for one subtree. A lower `priority` wins
/// over a higher one, which is how the subagent's IF-MIB takes
/// precedence over the master's own built-in handlers.
pub fn register_pdu(session_id: u32, packet_id: u32, subtree: &[u32], priority: u8) -> Vec<u8> {
    // timeout (0 = the session default), priority, range_subid, reserved.
    let mut payload = vec![0, priority, 0, 0];
    push_oid(&mut payload, subtree, false);
    frame(PduType::Register, session_id, 0, packet_id, payload)
}

/// An `agentx-Response-PDU` carrying `binds`.
pub fn response_pdu(header: &Header, error: u16, index: u16, binds: &[VarBind]) -> Vec<u8> {
    let mut payload = Vec::new();
    // sysUpTime is the master's to fill in for a subagent response.
    push_u32(&mut payload, 0);
    push_u16(&mut payload, error);
    push_u16(&mut payload, index);
    for bind in binds {
        push_varbind(&mut payload, bind);
    }
    frame(
        PduType::Response,
        header.session_id,
        header.transaction_id,
        header.packet_id,
        payload,
    )
}

// ---------------------------------------------------------------- decode

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn u8(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn u16(&mut self) -> Option<u16> {
        let slice = self.bytes.get(self.at..self.at + 2)?;
        self.at += 2;
        Some(u16::from_be_bytes([slice[0], slice[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let slice = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    /// An OID, plus whether its include flag was set.
    fn oid(&mut self) -> Option<(Oid, bool)> {
        let count = self.u8()?;
        let prefix = self.u8()?;
        let include = self.u8()? != 0;
        let _reserved = self.u8()?;
        let mut oid = Vec::with_capacity(usize::from(count) + 5);
        // A non-zero prefix is shorthand for a leading 1.3.6.1.<prefix>.
        if prefix != 0 {
            oid.extend_from_slice(&[1, 3, 6, 1, u32::from(prefix)]);
        }
        for _ in 0..count {
            oid.push(self.u32()?);
        }
        Some((oid, include))
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

/// Decode a retrieval PDU's payload into search ranges.
pub fn parse_request(header: Header, payload: &[u8]) -> Option<Request> {
    let mut reader = Reader::new(payload);
    let (non_repeaters, max_repetitions) = if header.pdu_type == PduType::GetBulk {
        (reader.u16()?, reader.u16()?)
    } else {
        (0, 0)
    };
    let mut ranges = Vec::new();
    while !reader.done() {
        let (start, include) = reader.oid()?;
        let (end, _) = reader.oid()?;
        ranges.push(SearchRange {
            start,
            include,
            end,
        });
    }
    Some(Request {
        header,
        ranges,
        non_repeaters,
        max_repetitions,
    })
}

/// The session id a master's Open response carries (its header's
/// session field), plus the response's error code.
pub fn parse_response(header: &Header, payload: &[u8]) -> Option<(u32, u16)> {
    let mut reader = Reader::new(payload);
    let _sys_uptime = reader.u32()?;
    let error = reader.u16()?;
    Some((header.session_id, error))
}

/// Was this registration refused because the subtree is already ours?
/// (Reconnects and duplicate registrations are not fatal.)
pub fn is_duplicate_registration(error: u16) -> bool {
    // 263 duplicateRegistration, 262 requestDenied.
    matches!(error, 263)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip() {
        let framed = frame(PduType::Response, 7, 11, 13, vec![1, 2, 3, 4]);
        let header = Header::parse(&framed).unwrap();
        assert_eq!(header.pdu_type, PduType::Response);
        assert_eq!(
            (header.session_id, header.transaction_id, header.packet_id),
            (7, 11, 13)
        );
        assert_eq!(header.payload_len, 4);
        assert_eq!(&framed[HEADER_LEN..], &[1, 2, 3, 4]);
    }

    #[test]
    fn short_wrong_version_and_little_endian_headers_are_refused() {
        assert!(Header::parse(&[0u8; 19]).is_none());
        let mut framed = frame(PduType::Ping, 1, 0, 1, Vec::new());
        framed[0] = 2;
        assert!(Header::parse(&framed).is_none());
        let mut framed = frame(PduType::Ping, 1, 0, 1, Vec::new());
        framed[2] = 0;
        assert!(Header::parse(&framed).is_none());
    }

    /// Octet strings pad to a 4-byte boundary; the padding is not part
    /// of the declared length.
    #[test]
    fn octet_strings_pad_to_four() {
        for (text, expected) in [("", 4), ("a", 8), ("abcd", 8), ("abcde", 12)] {
            let mut out = Vec::new();
            push_octets(&mut out, text.as_bytes());
            assert_eq!(out.len(), expected, "padding wrong for {text:?}");
            assert_eq!(
                u32::from_be_bytes([out[0], out[1], out[2], out[3]]),
                text.len() as u32
            );
        }
    }

    /// A Get payload the master would send, decoded back into ranges.
    #[test]
    fn get_requests_decode() {
        let mut payload = Vec::new();
        push_oid(&mut payload, &[1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1], false);
        push_oid(&mut payload, &[], false);
        let framed = frame(PduType::Get, 1, 2, 3, payload);
        let header = Header::parse(&framed).unwrap();
        let request = parse_request(header, &framed[HEADER_LEN..]).unwrap();
        assert_eq!(request.ranges.len(), 1);
        assert_eq!(
            request.ranges[0].start,
            vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1]
        );
        assert!(request.ranges[0].end.is_empty());
    }

    /// The `prefix` shorthand expands to 1.3.6.1.<prefix> on decode,
    /// which is how net-snmp's master actually sends MIB-2 OIDs.
    #[test]
    fn prefixed_oids_expand() {
        // n_subid=6, prefix=2 (1.3.6.1.2), include=0.
        let mut payload = vec![6, 2, 0, 0];
        for sub in [1u32, 2, 2, 1, 8, 1] {
            payload.extend_from_slice(&sub.to_be_bytes());
        }
        push_oid(&mut payload, &[], false);
        let framed = frame(PduType::Get, 1, 2, 3, payload);
        let header = Header::parse(&framed).unwrap();
        let request = parse_request(header, &framed[HEADER_LEN..]).unwrap();
        assert_eq!(
            request.ranges[0].start,
            vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1]
        );
    }

    #[test]
    fn getbulk_carries_its_repeat_counts() {
        let mut payload = Vec::new();
        push_u16(&mut payload, 0);
        push_u16(&mut payload, 10);
        push_oid(&mut payload, &[1, 3, 6, 1, 2, 1, 2, 2, 1, 1], true);
        push_oid(&mut payload, &[], false);
        let framed = frame(PduType::GetBulk, 1, 2, 3, payload);
        let header = Header::parse(&framed).unwrap();
        let request = parse_request(header, &framed[HEADER_LEN..]).unwrap();
        assert_eq!((request.non_repeaters, request.max_repetitions), (0, 10));
        assert!(request.ranges[0].include);
    }

    /// Every value type encodes at its documented width; the exception
    /// markers carry a tag and no payload.
    #[test]
    fn values_encode_at_their_widths() {
        let sizes = [
            (Value::Integer(-1), 4),
            (Value::Counter32(1), 4),
            (Value::Gauge32(1), 4),
            (Value::TimeTicks(1), 4),
            (Value::Counter64(1), 8),
            (Value::OctetString(b"Ethernet1".to_vec()), 16),
            (Value::EndOfMibView, 0),
            (Value::NoSuchObject, 0),
            (Value::NoSuchInstance, 0),
        ];
        for (value, payload_len) in sizes {
            let mut out = Vec::new();
            let bind = VarBind {
                name: vec![1, 3, 6, 1],
                value: value.clone(),
            };
            push_varbind(&mut out, &bind);
            // 4 bytes of type header + 4 of OID header + 4 sub-ids.
            assert_eq!(out.len(), 4 + 4 + 16 + payload_len, "wrong width {value:?}");
        }
    }

    #[test]
    fn open_and_register_pdus_frame() {
        let open = open_pdu(1, &[1, 3, 6, 1, 4, 1, 99], "hemlock");
        let header = Header::parse(&open).unwrap();
        assert_eq!(header.pdu_type, PduType::Open);
        assert_eq!(header.session_id, 0);
        assert_eq!(open.len(), HEADER_LEN + header.payload_len as usize);

        let register = register_pdu(9, 2, &[1, 3, 6, 1, 2, 1, 2], 1);
        let header = Header::parse(&register).unwrap();
        assert_eq!(header.pdu_type, PduType::Register);
        assert_eq!(header.session_id, 9);
        // timeout, priority, range_subid, reserved, then the OID.
        assert_eq!(register[HEADER_LEN + 1], 1, "priority not carried");
    }

    #[test]
    fn responses_echo_the_request_ids() {
        let request = Header::parse(&frame(PduType::Get, 4, 5, 6, Vec::new())).unwrap();
        let binds = [VarBind {
            name: vec![1, 3, 6, 1, 2, 1, 2, 2, 1, 8, 1],
            value: Value::Integer(1),
        }];
        let response = response_pdu(&request, ERROR_NONE, 0, &binds);
        let header = Header::parse(&response).unwrap();
        assert_eq!(header.pdu_type, PduType::Response);
        assert_eq!(
            (header.session_id, header.transaction_id, header.packet_id),
            (4, 5, 6)
        );
        let (session, error) = parse_response(&header, &response[HEADER_LEN..]).unwrap();
        assert_eq!((session, error), (4, ERROR_NONE));
    }

    #[test]
    fn duplicate_registrations_are_recognised() {
        assert!(is_duplicate_registration(263));
        assert!(!is_duplicate_registration(0));
        assert!(!is_duplicate_registration(262));
    }
}
