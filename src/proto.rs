//! PTPv2-Nachrichten (IEEE 1588-2008, Kapitel 13): Serialisieren und Parsen.
//!
//! Unterstützt: Sync, Delay_Req, Follow_Up, Delay_Resp, Announce vollständig.
//! Management und Signaling werden nur so weit geparst, dass wir sie sicher
//! ignorieren können; Pdelay-Nachrichten werden erkannt und verworfen.

use std::fmt;

use crate::time::PtpTimestamp;

pub const HEADER_LEN: usize = 34;
pub const TIMESTAMP_LEN: usize = 10;
pub const PORT_IDENTITY_LEN: usize = 10;
pub const SYNC_LEN: usize = HEADER_LEN + TIMESTAMP_LEN; // 44
pub const DELAY_REQ_LEN: usize = HEADER_LEN + TIMESTAMP_LEN; // 44
pub const FOLLOW_UP_LEN: usize = HEADER_LEN + TIMESTAMP_LEN; // 44
pub const DELAY_RESP_LEN: usize = HEADER_LEN + TIMESTAMP_LEN + PORT_IDENTITY_LEN; // 54
pub const ANNOUNCE_LEN: usize = HEADER_LEN + 30; // 64
pub const PTP_VERSION: u8 = 2;

/// logMessageInterval für Nachrichten ohne festes Intervall (Delay_Req, Management).
pub const LOG_INTERVAL_UNSPECIFIED: i8 = 0x7F;

/// flagField (zwei Oktette, hier als u16 big-endian: Oktett 0 = high byte).
pub mod flags {
    pub const ALTERNATE_MASTER: u16 = 0x0100;
    pub const TWO_STEP: u16 = 0x0200;
    pub const UNICAST: u16 = 0x0400;
    pub const PROFILE_SPECIFIC_1: u16 = 0x2000;
    pub const PROFILE_SPECIFIC_2: u16 = 0x4000;
    pub const LEAP_61: u16 = 0x0001;
    pub const LEAP_59: u16 = 0x0002;
    pub const CURRENT_UTC_OFFSET_VALID: u16 = 0x0004;
    pub const PTP_TIMESCALE: u16 = 0x0008;
    pub const TIME_TRACEABLE: u16 = 0x0010;
    pub const FREQUENCY_TRACEABLE: u16 = 0x0020;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageType {
    Sync = 0x0,
    DelayReq = 0x1,
    PdelayReq = 0x2,
    PdelayResp = 0x3,
    FollowUp = 0x8,
    DelayResp = 0x9,
    PdelayRespFollowUp = 0xA,
    Announce = 0xB,
    Signaling = 0xC,
    Management = 0xD,
}

impl MessageType {
    pub fn from_nibble(n: u8) -> Option<Self> {
        Some(match n & 0x0F {
            0x0 => Self::Sync,
            0x1 => Self::DelayReq,
            0x2 => Self::PdelayReq,
            0x3 => Self::PdelayResp,
            0x8 => Self::FollowUp,
            0x9 => Self::DelayResp,
            0xA => Self::PdelayRespFollowUp,
            0xB => Self::Announce,
            0xC => Self::Signaling,
            0xD => Self::Management,
            _ => return None,
        })
    }

    /// controlField (13.3.2.13, nur für Rückwärtskompatibilität zu PTPv1).
    pub fn control_field(self) -> u8 {
        match self {
            Self::Sync => 0,
            Self::DelayReq => 1,
            Self::FollowUp => 2,
            Self::DelayResp => 3,
            Self::Management => 4,
            _ => 5,
        }
    }

    /// Event-Nachrichten werden auf Port 319 gesendet und bekommen Zeitstempel.
    pub fn is_event(self) -> bool {
        matches!(
            self,
            Self::Sync | Self::DelayReq | Self::PdelayReq | Self::PdelayResp
        )
    }
}

/// clockIdentity (EUI-64).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClockIdentity(pub [u8; 8]);

impl ClockIdentity {
    /// EUI-64 aus EUI-48 (MAC) nach IEEE 1588-2008, 7.5.2.2.2:
    /// OUI (3 Oktette) + FF FE + restliche 3 Oktette.
    pub fn from_mac(mac: [u8; 6]) -> Self {
        Self([mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]])
    }

    /// Hexstring mit oder ohne Trennzeichen (":", "-", ".") parsen.
    pub fn parse(s: &str) -> Option<Self> {
        let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() != 16
            || s.chars()
                .any(|c| !c.is_ascii_hexdigit() && !":-.".contains(c))
        {
            return None;
        }
        let mut out = [0u8; 8];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Display for ClockIdentity {
    /// Schreibweise wie linuxptp: 001b21.fffe.010203
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}.{:02x}{:02x}.{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]
        )
    }
}

/// portIdentity: clockIdentity + portNumber.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortIdentity {
    pub clock_identity: ClockIdentity,
    pub port_number: u16,
}

impl fmt::Display for PortIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.clock_identity, self.port_number)
    }
}

/// clockQuality (5.3.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ClockQuality {
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub offset_scaled_log_variance: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub transport_specific: u8,
    pub message_type: MessageType,
    /// versionPTP (untere 4 Bit), 2 für PTPv2
    pub version: u8,
    /// minorVersionPTP (obere 4 Bit, IEEE 1588-2019); wir senden 0
    pub minor_version: u8,
    pub message_length: u16,
    pub domain_number: u8,
    pub flags: u16,
    /// Nanosekunden × 2^16
    pub correction: i64,
    pub source_port_identity: PortIdentity,
    pub sequence_id: u16,
    pub control_field: u8,
    pub log_message_interval: i8,
}

impl Header {
    pub fn new(
        message_type: MessageType,
        domain_number: u8,
        source_port_identity: PortIdentity,
        sequence_id: u16,
        log_message_interval: i8,
    ) -> Self {
        Self {
            transport_specific: 0,
            message_type,
            version: PTP_VERSION,
            minor_version: 0,
            message_length: 0,
            domain_number,
            flags: 0,
            correction: 0,
            source_port_identity,
            sequence_id,
            control_field: message_type.control_field(),
            log_message_interval,
        }
    }

    pub fn has_flag(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    fn write(&self, out: &mut Vec<u8>, length: u16) {
        out.push((self.transport_specific << 4) | (self.message_type as u8 & 0x0F));
        out.push((self.minor_version << 4) | (self.version & 0x0F));
        out.extend_from_slice(&length.to_be_bytes());
        out.push(self.domain_number);
        out.push(0);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.correction.to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        write_port_identity(out, &self.source_port_identity);
        out.extend_from_slice(&self.sequence_id.to_be_bytes());
        out.push(self.control_field);
        out.push(self.log_message_interval as u8);
    }

    /// Nur den Header parsen (z. B. um Domain und Absender früh zu prüfen).
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() < HEADER_LEN {
            return Err(ParseError::TooShort {
                needed: HEADER_LEN,
                got: buf.len(),
            });
        }
        let version = buf[1] & 0x0F;
        if version != PTP_VERSION {
            return Err(ParseError::UnsupportedVersion(version));
        }
        let message_type = MessageType::from_nibble(buf[0])
            .ok_or(ParseError::UnknownMessageType(buf[0] & 0x0F))?;
        let message_length = u16::from_be_bytes([buf[2], buf[3]]);
        Ok(Self {
            transport_specific: buf[0] >> 4,
            message_type,
            version,
            minor_version: buf[1] >> 4,
            message_length,
            domain_number: buf[4],
            flags: u16::from_be_bytes([buf[6], buf[7]]),
            correction: i64::from_be_bytes(buf[8..16].try_into().unwrap_or_default()),
            source_port_identity: read_port_identity(&buf[20..30]),
            sequence_id: u16::from_be_bytes([buf[30], buf[31]]),
            control_field: buf[32],
            log_message_interval: buf[33] as i8,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Announce {
    pub origin_timestamp: PtpTimestamp,
    pub current_utc_offset: i16,
    pub grandmaster_priority1: u8,
    pub grandmaster_clock_quality: ClockQuality,
    pub grandmaster_priority2: u8,
    pub grandmaster_identity: ClockIdentity,
    pub steps_removed: u16,
    pub time_source: u8,
}

/// Management-Nachricht (15.4): nur Kopf der Nutzlast, TLVs bleiben roh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Management {
    pub target_port_identity: PortIdentity,
    pub starting_boundary_hops: u8,
    pub boundary_hops: u8,
    pub action: u8,
    /// tlvType und managementId des ersten TLV, falls vorhanden
    pub first_tlv: Option<(u16, u16)>,
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Sync {
        origin_timestamp: PtpTimestamp,
    },
    DelayReq {
        origin_timestamp: PtpTimestamp,
    },
    FollowUp {
        precise_origin_timestamp: PtpTimestamp,
    },
    DelayResp {
        receive_timestamp: PtpTimestamp,
        requesting_port_identity: PortIdentity,
    },
    Announce(Announce),
    Management(Management),
    Signaling {
        target_port_identity: PortIdentity,
        raw: Vec<u8>,
    },
    /// Pdelay_Req/Pdelay_Resp/Pdelay_Resp_Follow_Up: erkannt, nicht ausgewertet
    Other {
        raw: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    TooShort {
        needed: usize,
        got: usize,
    },
    UnsupportedVersion(u8),
    UnknownMessageType(u8),
    /// messageLength im Header ist größer als das empfangene Datagramm
    LengthMismatch {
        header: u16,
        got: usize,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::TooShort { needed, got } => {
                write!(
                    f,
                    "Nachricht zu kurz ({got} statt mindestens {needed} Bytes)"
                )
            }
            ParseError::UnsupportedVersion(v) => write!(f, "PTP-Version {v} nicht unterstützt"),
            ParseError::UnknownMessageType(t) => write!(f, "unbekannter messageType 0x{t:x}"),
            ParseError::LengthMismatch { header, got } => {
                write!(f, "messageLength {header} > empfangen {got}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl Message {
    pub fn new(header: Header, body: Body) -> Self {
        Self { header, body }
    }

    pub fn message_type(&self) -> MessageType {
        self.header.message_type
    }

    /// Serialisiert die Nachricht; messageLength wird berechnet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(32);
        match &self.body {
            Body::Sync { origin_timestamp } | Body::DelayReq { origin_timestamp } => {
                write_timestamp(&mut body, origin_timestamp)
            }
            Body::FollowUp {
                precise_origin_timestamp,
            } => write_timestamp(&mut body, precise_origin_timestamp),
            Body::DelayResp {
                receive_timestamp,
                requesting_port_identity,
            } => {
                write_timestamp(&mut body, receive_timestamp);
                write_port_identity(&mut body, requesting_port_identity);
            }
            Body::Announce(a) => {
                write_timestamp(&mut body, &a.origin_timestamp);
                body.extend_from_slice(&a.current_utc_offset.to_be_bytes());
                body.push(0);
                body.push(a.grandmaster_priority1);
                body.push(a.grandmaster_clock_quality.clock_class);
                body.push(a.grandmaster_clock_quality.clock_accuracy);
                body.extend_from_slice(
                    &a.grandmaster_clock_quality
                        .offset_scaled_log_variance
                        .to_be_bytes(),
                );
                body.push(a.grandmaster_priority2);
                body.extend_from_slice(&a.grandmaster_identity.0);
                body.extend_from_slice(&a.steps_removed.to_be_bytes());
                body.push(a.time_source);
            }
            Body::Management(m) => body.extend_from_slice(&m.raw),
            Body::Signaling { raw, .. } | Body::Other { raw } => body.extend_from_slice(raw),
        }
        let len = (HEADER_LEN + body.len()) as u16;
        let mut out = Vec::with_capacity(len as usize);
        self.header.write(&mut out, len);
        out.extend_from_slice(&body);
        out
    }

    /// Parst eine Nachricht. Bytes hinter messageLength (Padding) werden ignoriert,
    /// angehängte TLVs (z. B. PATH_TRACE im Announce) ebenso.
    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        let header = Header::parse(buf)?;
        let len = header.message_length as usize;
        if len > buf.len() {
            return Err(ParseError::LengthMismatch {
                header: header.message_length,
                got: buf.len(),
            });
        }
        if len < HEADER_LEN {
            return Err(ParseError::TooShort {
                needed: HEADER_LEN,
                got: len,
            });
        }
        let msg = &buf[..len];
        let need = |n: usize| -> Result<(), ParseError> {
            if msg.len() < n {
                Err(ParseError::TooShort {
                    needed: n,
                    got: msg.len(),
                })
            } else {
                Ok(())
            }
        };
        let p = &msg[HEADER_LEN..];
        let body = match header.message_type {
            MessageType::Sync => {
                need(SYNC_LEN)?;
                Body::Sync {
                    origin_timestamp: read_timestamp(p),
                }
            }
            MessageType::DelayReq => {
                need(DELAY_REQ_LEN)?;
                Body::DelayReq {
                    origin_timestamp: read_timestamp(p),
                }
            }
            MessageType::FollowUp => {
                need(FOLLOW_UP_LEN)?;
                Body::FollowUp {
                    precise_origin_timestamp: read_timestamp(p),
                }
            }
            MessageType::DelayResp => {
                need(DELAY_RESP_LEN)?;
                Body::DelayResp {
                    receive_timestamp: read_timestamp(p),
                    requesting_port_identity: read_port_identity(&p[10..20]),
                }
            }
            MessageType::Announce => {
                need(ANNOUNCE_LEN)?;
                Body::Announce(Announce {
                    origin_timestamp: read_timestamp(p),
                    current_utc_offset: i16::from_be_bytes([p[10], p[11]]),
                    grandmaster_priority1: p[13],
                    grandmaster_clock_quality: ClockQuality {
                        clock_class: p[14],
                        clock_accuracy: p[15],
                        offset_scaled_log_variance: u16::from_be_bytes([p[16], p[17]]),
                    },
                    grandmaster_priority2: p[18],
                    grandmaster_identity: ClockIdentity(p[19..27].try_into().unwrap_or_default()),
                    steps_removed: u16::from_be_bytes([p[27], p[28]]),
                    time_source: p[29],
                })
            }
            MessageType::Management => {
                need(HEADER_LEN + 14)?;
                let first_tlv = (p.len() >= 18).then(|| {
                    (
                        u16::from_be_bytes([p[14], p[15]]),
                        if p.len() >= 20 {
                            u16::from_be_bytes([p[18], p[19]])
                        } else {
                            0
                        },
                    )
                });
                Body::Management(Management {
                    target_port_identity: read_port_identity(&p[0..10]),
                    starting_boundary_hops: p[10],
                    boundary_hops: p[11],
                    action: p[12] & 0x0F,
                    first_tlv,
                    raw: p.to_vec(),
                })
            }
            MessageType::Signaling => {
                need(HEADER_LEN + PORT_IDENTITY_LEN)?;
                Body::Signaling {
                    target_port_identity: read_port_identity(&p[0..10]),
                    raw: p.to_vec(),
                }
            }
            MessageType::PdelayReq | MessageType::PdelayResp | MessageType::PdelayRespFollowUp => {
                Body::Other { raw: p.to_vec() }
            }
        };
        Ok(Self { header, body })
    }
}

fn write_timestamp(out: &mut Vec<u8>, ts: &PtpTimestamp) {
    let s = ts.seconds.to_be_bytes();
    out.extend_from_slice(&s[2..8]);
    out.extend_from_slice(&ts.nanoseconds.to_be_bytes());
}

fn read_timestamp(b: &[u8]) -> PtpTimestamp {
    let mut s = [0u8; 8];
    s[2..8].copy_from_slice(&b[0..6]);
    PtpTimestamp {
        seconds: u64::from_be_bytes(s),
        nanoseconds: u32::from_be_bytes([b[6], b[7], b[8], b[9]]),
    }
}

fn write_port_identity(out: &mut Vec<u8>, p: &PortIdentity) {
    out.extend_from_slice(&p.clock_identity.0);
    out.extend_from_slice(&p.port_number.to_be_bytes());
}

fn read_port_identity(b: &[u8]) -> PortIdentity {
    PortIdentity {
        clock_identity: ClockIdentity(b[0..8].try_into().unwrap_or_default()),
        port_number: u16::from_be_bytes([b[8], b[9]]),
    }
}

#[cfg(test)]
mod tests {
    //! Testvektoren: von Hand nach IEEE 1588-2008 Kap. 13 zusammengesetzt, im Aufbau
    //! so, wie Wireshark (Filter `ptp`) die Pakete eines linuxptp-Masters zeigt.
    //! Jede Zeile entspricht einem Feld; die Vektoren sind bewusst NICHT mit dem
    //! eigenen Serializer erzeugt.
    use super::*;

    const CLOCK: ClockIdentity = ClockIdentity([0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03]);
    const SRC: PortIdentity = PortIdentity {
        clock_identity: CLOCK,
        port_number: 1,
    };
    const SLAVE: PortIdentity = PortIdentity {
        clock_identity: ClockIdentity([0xac, 0xde, 0x48, 0xff, 0xfe, 0x00, 0x11, 0x22]),
        port_number: 1,
    };
    // 1700000000 s = 0x6553F100, 123456789 ns = 0x075BCD15
    const TS: PtpTimestamp = PtpTimestamp::new(1_700_000_000, 123_456_789);

    #[rustfmt::skip]
    const SYNC_TWO_STEP: [u8; 44] = [
        0x00, 0x02, 0x00, 0x2c,                         // Sync, v2, Länge 44
        0x00, 0x00,                                     // Domain 0, reserviert
        0x02, 0x00,                                     // flagField: twoStepFlag
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // correctionField
        0x00, 0x00, 0x00, 0x00,                         // reserviert
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03, // clockIdentity
        0x00, 0x01,                                     // portNumber
        0x00, 0x2a,                                     // sequenceId 42
        0x00,                                           // controlField Sync
        0x00,                                           // logMessageInterval 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,             // originTimestamp.s (0 bei Two-Step)
        0x00, 0x00, 0x00, 0x00,                         // originTimestamp.ns
    ];

    #[rustfmt::skip]
    const SYNC_ONE_STEP_AES67: [u8; 44] = [
        0x00, 0x02, 0x00, 0x2c,
        0x00, 0x00,
        0x00, 0x00,                                     // keine Flags (One-Step)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x80, 0x00, // correction 1,5 ns
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03,
        0x00, 0x01,
        0xff, 0xff,                                     // sequenceId 65535
        0x00,
        0xfd,                                           // logMessageInterval -3 (8/s)
        0x00, 0x00, 0x65, 0x53, 0xf1, 0x00,             // 1700000000 s
        0x07, 0x5b, 0xcd, 0x15,                         // 123456789 ns
    ];

    #[rustfmt::skip]
    const FOLLOW_UP: [u8; 44] = [
        0x08, 0x02, 0x00, 0x2c,                         // Follow_Up, v2, Länge 44
        0x00, 0x00,
        0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03,
        0x00, 0x01,
        0x00, 0x2a,                                     // gleiche sequenceId wie Sync
        0x02,                                           // controlField Follow_Up
        0x00,
        0x00, 0x00, 0x65, 0x53, 0xf1, 0x00,             // preciseOriginTimestamp
        0x07, 0x5b, 0xcd, 0x15,
    ];

    #[rustfmt::skip]
    const DELAY_REQ: [u8; 44] = [
        0x01, 0x02, 0x00, 0x2c,                         // Delay_Req, v2, Länge 44
        0x00, 0x00,
        0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0xac, 0xde, 0x48, 0xff, 0xfe, 0x00, 0x11, 0x22, // Slave
        0x00, 0x01,
        0x01, 0x00,                                     // sequenceId 256
        0x01,                                           // controlField Delay_Req
        0x7f,                                           // logMessageInterval 0x7F
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    #[rustfmt::skip]
    const DELAY_RESP: [u8; 54] = [
        0x09, 0x02, 0x00, 0x36,                         // Delay_Resp, v2, Länge 54
        0x00, 0x00,
        0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03,
        0x00, 0x01,
        0x01, 0x00,                                     // sequenceId des Delay_Req
        0x03,                                           // controlField Delay_Resp
        0x00,                                           // logMinDelayReqInterval 0
        0x00, 0x00, 0x65, 0x53, 0xf1, 0x00,             // receiveTimestamp
        0x07, 0x5b, 0xcd, 0x15,
        0xac, 0xde, 0x48, 0xff, 0xfe, 0x00, 0x11, 0x22, // requestingPortIdentity
        0x00, 0x01,
    ];

    #[rustfmt::skip]
    const ANNOUNCE: [u8; 64] = [
        0x0b, 0x02, 0x00, 0x40,                         // Announce, v2, Länge 64
        0x00, 0x00,
        0x00, 0x0c,                                     // ptpTimescale | currentUtcOffsetValid
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03,
        0x00, 0x01,
        0x12, 0x34,                                     // sequenceId 0x1234
        0x05,                                           // controlField "other"
        0x01,                                           // logAnnounceInterval 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,             // originTimestamp
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x25,                                     // currentUtcOffset 37
        0x00,                                           // reserviert
        0x80,                                           // grandmasterPriority1 128
        0xf8,                                           // clockClass 248
        0xfe,                                           // clockAccuracy unbekannt
        0xff, 0xff,                                     // offsetScaledLogVariance
        0x80,                                           // grandmasterPriority2 128
        0x00, 0x1b, 0x21, 0xff, 0xfe, 0x01, 0x02, 0x03, // grandmasterIdentity
        0x00, 0x00,                                     // stepsRemoved 0
        0x50,                                           // timeSource NTP
    ];

    fn hdr(t: MessageType, seq: u16, log: i8) -> Header {
        Header::new(t, 0, SRC, seq, log)
    }

    #[test]
    fn sync_two_step() {
        let mut h = hdr(MessageType::Sync, 42, 0);
        h.flags = flags::TWO_STEP;
        let m = Message::new(
            h,
            Body::Sync {
                origin_timestamp: PtpTimestamp::default(),
            },
        );
        assert_eq!(m.to_bytes(), SYNC_TWO_STEP);
        let p = Message::parse(&SYNC_TWO_STEP).unwrap();
        assert_eq!(p.header.message_length, 44);
        assert!(p.header.has_flag(flags::TWO_STEP));
        assert_eq!(p.header.sequence_id, 42);
        assert_eq!(p.header.source_port_identity, SRC);
        assert_eq!(p.body, m.body);
    }

    #[test]
    fn sync_one_step_with_correction() {
        let mut h = hdr(MessageType::Sync, 0xffff, -3);
        h.correction = 0x0001_8000; // 1,5 ns
        let m = Message::new(
            h,
            Body::Sync {
                origin_timestamp: TS,
            },
        );
        assert_eq!(m.to_bytes(), SYNC_ONE_STEP_AES67);
        let p = Message::parse(&SYNC_ONE_STEP_AES67).unwrap();
        assert_eq!(p.header.log_message_interval, -3);
        assert_eq!(crate::time::correction_to_nanos(p.header.correction), 1.5);
        assert!(!p.header.has_flag(flags::TWO_STEP));
        assert_eq!(
            p.body,
            Body::Sync {
                origin_timestamp: TS
            }
        );
    }

    #[test]
    fn follow_up() {
        let m = Message::new(
            hdr(MessageType::FollowUp, 42, 0),
            Body::FollowUp {
                precise_origin_timestamp: TS,
            },
        );
        assert_eq!(m.to_bytes(), FOLLOW_UP);
        assert_eq!(Message::parse(&FOLLOW_UP).unwrap(), {
            let mut m = m;
            m.header.message_length = 44;
            m
        });
    }

    #[test]
    fn delay_req() {
        let m = Message::new(
            Header::new(
                MessageType::DelayReq,
                0,
                SLAVE,
                256,
                LOG_INTERVAL_UNSPECIFIED,
            ),
            Body::DelayReq {
                origin_timestamp: PtpTimestamp::default(),
            },
        );
        assert_eq!(m.to_bytes(), DELAY_REQ);
        let p = Message::parse(&DELAY_REQ).unwrap();
        assert_eq!(p.header.control_field, 1);
        assert_eq!(p.header.source_port_identity, SLAVE);
        assert_eq!(p.header.log_message_interval, 0x7f);
    }

    #[test]
    fn delay_resp() {
        let m = Message::new(
            hdr(MessageType::DelayResp, 256, 0),
            Body::DelayResp {
                receive_timestamp: TS,
                requesting_port_identity: SLAVE,
            },
        );
        assert_eq!(m.to_bytes(), DELAY_RESP);
        let p = Message::parse(&DELAY_RESP).unwrap();
        assert_eq!(
            p.body,
            Body::DelayResp {
                receive_timestamp: TS,
                requesting_port_identity: SLAVE
            }
        );
    }

    #[test]
    fn announce() {
        let mut h = hdr(MessageType::Announce, 0x1234, 1);
        h.flags = flags::PTP_TIMESCALE | flags::CURRENT_UTC_OFFSET_VALID;
        let a = Announce {
            origin_timestamp: PtpTimestamp::default(),
            current_utc_offset: 37,
            grandmaster_priority1: 128,
            grandmaster_clock_quality: ClockQuality {
                clock_class: 248,
                clock_accuracy: 0xfe,
                offset_scaled_log_variance: 0xffff,
            },
            grandmaster_priority2: 128,
            grandmaster_identity: CLOCK,
            steps_removed: 0,
            time_source: 0x50,
        };
        let m = Message::new(h, Body::Announce(a));
        assert_eq!(m.to_bytes(), ANNOUNCE);
        let p = Message::parse(&ANNOUNCE).unwrap();
        assert_eq!(p.body, Body::Announce(a));
        assert!(p.header.has_flag(flags::PTP_TIMESCALE));
        assert!(p.header.has_flag(flags::CURRENT_UTC_OFFSET_VALID));
        assert!(!p.header.has_flag(flags::LEAP_61));
    }

    #[test]
    fn announce_with_path_trace_tlv() {
        // Wie ptp4l mit path_trace_enabled: PATH_TRACE-TLV (0x0008) mit einer clockIdentity.
        let mut buf = ANNOUNCE.to_vec();
        buf.extend_from_slice(&[0x00, 0x08, 0x00, 0x08]);
        buf.extend_from_slice(&CLOCK.0);
        buf[2..4].copy_from_slice(&76u16.to_be_bytes());
        let p = Message::parse(&buf).unwrap();
        assert!(matches!(p.body, Body::Announce(a) if a.grandmaster_priority1 == 128));
    }

    #[test]
    fn ptp_2019_minor_version_and_padding() {
        // IEEE 1588-2019: versionPTP-Oktett 0x12 (minorVersion 1). Dazu Padding hinter der Nachricht.
        let mut buf = SYNC_TWO_STEP.to_vec();
        buf[1] = 0x12;
        buf.extend_from_slice(&[0u8; 20]);
        let p = Message::parse(&buf).unwrap();
        assert_eq!(p.header.minor_version, 1);
        assert_eq!(p.header.message_type, MessageType::Sync);
    }

    #[test]
    fn management_get_is_parsed_and_ignorable() {
        // pmc -u -b 0 'GET CURRENT_DATA_SET': Management, action GET (0), TLV MANAGEMENT (0x0001),
        // managementId CURRENT_DATA_SET (0x2001).
        #[rustfmt::skip]
        let buf: [u8; 54] = [
            0x0d, 0x02, 0x00, 0x36,
            0x00, 0x00,
            0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0xac, 0xde, 0x48, 0xff, 0xfe, 0x00, 0x11, 0x22,
            0x00, 0x01,
            0x00, 0x07,
            0x04,                                           // controlField Management
            0x7f,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // targetPortIdentity: alle
            0xff, 0xff,
            0x00,                                           // startingBoundaryHops
            0x00,                                           // boundaryHops
            0x00,                                           // actionField GET
            0x00,
            0x00, 0x01,                                     // tlvType MANAGEMENT
            0x00, 0x02,                                     // lengthField
            0x20, 0x01,                                     // managementId CURRENT_DATA_SET
        ];
        let p = Message::parse(&buf).unwrap();
        match p.body {
            Body::Management(m) => {
                assert_eq!(m.action, 0);
                assert_eq!(m.first_tlv, Some((0x0001, 0x2001)));
                assert_eq!(m.target_port_identity.port_number, 0xffff);
            }
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn signaling_and_pdelay_are_recognised() {
        let mut buf = DELAY_RESP.to_vec();
        buf[0] = 0x0c; // Signaling
        assert!(matches!(
            Message::parse(&buf).unwrap().body,
            Body::Signaling { .. }
        ));
        buf[0] = 0x02; // Pdelay_Req
        assert!(matches!(
            Message::parse(&buf).unwrap().body,
            Body::Other { .. }
        ));
    }

    #[test]
    fn errors() {
        assert!(matches!(
            Message::parse(&SYNC_TWO_STEP[..20]),
            Err(ParseError::TooShort { .. })
        ));
        let mut v1 = SYNC_TWO_STEP;
        v1[1] = 0x01;
        assert_eq!(Message::parse(&v1), Err(ParseError::UnsupportedVersion(1)));
        let mut unk = SYNC_TWO_STEP;
        unk[0] = 0x05;
        assert_eq!(Message::parse(&unk), Err(ParseError::UnknownMessageType(5)));
        let mut long = SYNC_TWO_STEP;
        long[3] = 0x50;
        assert!(matches!(
            Message::parse(&long),
            Err(ParseError::LengthMismatch { .. })
        ));
        // Announce mit zu kleinem messageLength
        let mut short = ANNOUNCE;
        short[3] = 44;
        assert!(matches!(
            Message::parse(&short),
            Err(ParseError::TooShort { .. })
        ));
    }

    #[test]
    fn parse_never_panics_on_garbage() {
        // Einfacher deterministischer Fuzz: alle Präfixe und bitgekippte Varianten.
        let vectors: [&[u8]; 6] = [
            &SYNC_TWO_STEP,
            &FOLLOW_UP,
            &DELAY_REQ,
            &DELAY_RESP,
            &ANNOUNCE,
            &SYNC_ONE_STEP_AES67,
        ];
        let mut x: u32 = 0x1234_5678;
        for v in vectors {
            for n in 0..=v.len() {
                let _ = Message::parse(&v[..n]);
            }
            for _ in 0..2000 {
                let mut b = v.to_vec();
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let i = x as usize % b.len();
                b[i] ^= (x >> 8) as u8;
                b.truncate((x >> 16) as usize % (b.len() + 1));
                let _ = Message::parse(&b);
            }
        }
    }

    #[test]
    fn clock_identity_from_mac() {
        let id = ClockIdentity::from_mac([0x00, 0x1b, 0x21, 0x01, 0x02, 0x03]);
        assert_eq!(id, CLOCK);
        assert_eq!(id.to_string(), "001b21.fffe.010203");
        assert_eq!(ClockIdentity::parse("001b21.fffe.010203"), Some(CLOCK));
        assert_eq!(ClockIdentity::parse("00:1b:21:ff:fe:01:02:03"), Some(CLOCK));
        assert_eq!(ClockIdentity::parse("001b21fffe0102"), None);
        assert_eq!(ClockIdentity::parse("001b21fffe01020x"), None);
    }

    #[test]
    fn event_classification() {
        assert!(MessageType::Sync.is_event());
        assert!(MessageType::DelayReq.is_event());
        assert!(!MessageType::FollowUp.is_event());
        assert!(!MessageType::Announce.is_event());
    }
}
