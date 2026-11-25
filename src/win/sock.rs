//! Winsock-Timestamping (SIO_TIMESTAMPING, SO_TIMESTAMP, SO_TIMESTAMP_ID, SIO_GET_TX_TIMESTAMP).
//!
//! Ablauf laut Microsoft-Doku "Winsock timestamping":
//! - `WSAIoctl(SIO_TIMESTAMPING, TIMESTAMPING_CONFIG{Flags=RX|TX, TxTimestampsBuffered})`
//! - RX: `WSARecvMsg` liefert eine Control-Message SOL_SOCKET/SO_TIMESTAMP mit UINT64
//! - TX: `WSASendMsg` mit Control-Message SOL_SOCKET/SO_TIMESTAMP_ID (UINT32-ID),
//!   danach `WSAIoctl(SIO_GET_TX_TIMESTAMP, &id) → UINT64`; WSAEWOULDBLOCK = noch nicht da.
//!
//! Die Werte sind Zählerstände: bei Software-Timestamps QPC, bei Hardware-Timestamps
//! die Uhr der Netzwerkkarte.

use std::io;
use std::mem::size_of;
use std::net::{Ipv4Addr, SocketAddrV4};

use windows::Win32::Networking::WinSock::{
    AF_INET, CMSGHDR, IN_ADDR, IN_ADDR_0, LPFN_WSARECVMSG, MSG_TRUNC, POLLRDNORM,
    SIO_GET_EXTENSION_FUNCTION_POINTER, SIO_GET_TX_TIMESTAMP, SIO_TIMESTAMPING, SIO_UDP_CONNRESET,
    SO_TIMESTAMP, SO_TIMESTAMP_ID, SOCKADDR, SOCKADDR_IN, SOCKET, SOL_SOCKET, TIMESTAMPING_CONFIG,
    TIMESTAMPING_FLAG_RX, TIMESTAMPING_FLAG_TX, WSABUF, WSAEWOULDBLOCK, WSAGetLastError,
    WSAID_WSARECVMSG, WSAIoctl, WSAMSG, WSAPOLLFD, WSAPoll, WSASendMsg,
};
use windows::core::{GUID, PSTR};

type RecvMsgFn = unsafe extern "system" fn(
    SOCKET,
    *mut WSAMSG,
    *mut u32,
    *mut windows::Win32::System::IO::OVERLAPPED,
    windows::Win32::Networking::WinSock::LPWSAOVERLAPPED_COMPLETION_ROUTINE,
) -> i32;

fn last_error() -> io::Error {
    // SAFETY: liest nur den Thread-lokalen Fehlercode.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError().0 })
}

/// Ausrichtung für Control-Messages (WSA_CMSGHDR_ALIGN / WSA_CMSGDATA_ALIGN = 8 auf x64).
const CMSG_ALIGN: usize = size_of::<usize>();
const fn align(n: usize) -> usize {
    (n + CMSG_ALIGN - 1) & !(CMSG_ALIGN - 1)
}
const CMSG_HDR_LEN: usize = align(size_of::<CMSGHDR>());

/// Rohen Timestamp-Socket-Zugriff kapseln.
pub struct WinTsSocket {
    sock: SOCKET,
    recvmsg: RecvMsgFn,
    pub rx_enabled: bool,
    pub tx_enabled: bool,
}

// SOCKET ist ein Handle; Winsock erlaubt Senden und Empfangen aus verschiedenen Threads.
unsafe impl Send for WinTsSocket {}
unsafe impl Sync for WinTsSocket {}

/// ICMP "Port unreachable" soll recv nicht mit WSAECONNRESET abbrechen.
pub fn disable_udp_connreset(raw: u64) {
    let sock = SOCKET(raw as usize);
    let flag: u32 = 0;
    let mut ret = 0u32;
    // SAFETY: Eingabepuffer ist ein BOOL (4 Byte).
    unsafe {
        WSAIoctl(
            sock,
            SIO_UDP_CONNRESET,
            Some((&flag as *const u32).cast()),
            4,
            None,
            0,
            &mut ret,
            None,
            None,
        );
    }
}

impl WinTsSocket {
    /// `raw` ist der Socket aus socket2 (`as_raw_socket`). Der Socket bleibt im Besitz des Aufrufers.
    pub fn new(raw: u64) -> io::Result<Self> {
        let sock = SOCKET(raw as usize);
        let guid: GUID = WSAID_WSARECVMSG;
        let mut f: LPFN_WSARECVMSG = None;
        let mut ret = 0u32;
        // SAFETY: Standardweg, um den WSARecvMsg-Zeiger zu bekommen.
        let rc = unsafe {
            WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                Some((&guid as *const GUID).cast()),
                size_of::<GUID>() as u32,
                Some((&mut f as *mut LPFN_WSARECVMSG).cast()),
                size_of::<LPFN_WSARECVMSG>() as u32,
                &mut ret,
                None,
                None,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        let recvmsg = f.ok_or_else(|| io::Error::other("WSARecvMsg nicht verfügbar"))?;
        Ok(Self {
            sock,
            recvmsg,
            rx_enabled: false,
            tx_enabled: false,
        })
    }

    /// SIO_TIMESTAMPING aktivieren.
    pub fn enable_timestamping(&mut self, rx: bool, tx: bool) -> io::Result<()> {
        let mut flags = 0;
        if rx {
            flags |= TIMESTAMPING_FLAG_RX;
        }
        if tx {
            flags |= TIMESTAMPING_FLAG_TX;
        }
        let cfg = TIMESTAMPING_CONFIG {
            Flags: flags,
            TxTimestampsBuffered: 16,
        };
        let mut ret = 0u32;
        // SAFETY: Eingabepuffer ist eine TIMESTAMPING_CONFIG.
        let rc = unsafe {
            WSAIoctl(
                self.sock,
                SIO_TIMESTAMPING,
                Some((&cfg as *const TIMESTAMPING_CONFIG).cast()),
                size_of::<TIMESTAMPING_CONFIG>() as u32,
                None,
                0,
                &mut ret,
                None,
                None,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        self.rx_enabled = rx;
        self.tx_enabled = tx;
        Ok(())
    }

    /// Wartet bis zu `timeout_ms` auf ein lesbares Datagramm.
    pub fn poll_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut fd = WSAPOLLFD {
            fd: self.sock,
            events: POLLRDNORM,
            revents: Default::default(),
        };
        // SAFETY: ein gültiger WSAPOLLFD-Eintrag.
        let rc = unsafe { WSAPoll(&mut fd, 1, timeout_ms) };
        if rc < 0 {
            return Err(last_error());
        }
        Ok(rc > 0)
    }

    /// Empfängt ein Datagramm samt optionalem SO_TIMESTAMP-Wert.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddrV4, Option<u64>)> {
        let mut from = SOCKADDR_IN::default();
        let mut data = WSABUF {
            len: buf.len() as u32,
            buf: PSTR(buf.as_mut_ptr()),
        };
        // Platz für mehrere Control-Messages
        let mut control = [0u64; 16];
        let mut msg = WSAMSG {
            name: (&mut from as *mut SOCKADDR_IN).cast::<SOCKADDR>(),
            namelen: size_of::<SOCKADDR_IN>() as i32,
            lpBuffers: &mut data,
            dwBufferCount: 1,
            Control: WSABUF {
                len: size_of_val(&control) as u32,
                buf: PSTR(control.as_mut_ptr().cast()),
            },
            dwFlags: 0,
        };
        let mut n = 0u32;
        // SAFETY: alle Puffer leben bis zum Ende des Aufrufs; synchroner Aufruf ohne OVERLAPPED.
        let rc = unsafe { (self.recvmsg)(self.sock, &mut msg, &mut n, std::ptr::null_mut(), None) };
        if rc != 0 {
            return Err(last_error());
        }
        if msg.dwFlags & MSG_TRUNC != 0 {
            return Err(io::Error::other("Datagramm abgeschnitten"));
        }
        let ip = Ipv4Addr::from(u32::from_be(unsafe { from.sin_addr.S_un.S_addr }));
        let src = SocketAddrV4::new(ip, u16::from_be(from.sin_port));
        // Control-Messages durchgehen
        let ctl_len = msg.Control.len as usize;
        let ctl = unsafe {
            std::slice::from_raw_parts(
                control.as_ptr().cast::<u8>(),
                ctl_len.min(size_of_val(&control)),
            )
        };
        Ok((n as usize, src, find_timestamp(ctl)))
    }

    /// Sendet mit SO_TIMESTAMP_ID-Control-Message (nur bei aktivem TX-Timestamping sinnvoll).
    pub fn send_with_id(&self, buf: &[u8], dest: SocketAddrV4, id: u32) -> io::Result<usize> {
        let mut to = sockaddr(dest);
        let mut data = WSABUF {
            len: buf.len() as u32,
            buf: PSTR(buf.as_ptr() as *mut u8),
        };
        // WSA_CMSG_SPACE(sizeof(UINT32))
        let mut control = [0u64; (CMSG_HDR_LEN + CMSG_ALIGN) / 8];
        {
            let p = control.as_mut_ptr().cast::<u8>();
            let hdr = CMSGHDR {
                cmsg_len: CMSG_HDR_LEN + size_of::<u32>(), // WSA_CMSG_LEN
                cmsg_level: SOL_SOCKET,
                cmsg_type: SO_TIMESTAMP_ID as i32,
            };
            // SAFETY: der Puffer ist groß genug und 8-Byte-ausgerichtet.
            unsafe {
                std::ptr::write(p.cast::<CMSGHDR>(), hdr);
                std::ptr::write_unaligned(p.add(CMSG_HDR_LEN).cast::<u32>(), id);
            }
        }
        let msg = WSAMSG {
            name: (&mut to as *mut SOCKADDR_IN).cast::<SOCKADDR>(),
            namelen: size_of::<SOCKADDR_IN>() as i32,
            lpBuffers: &mut data,
            dwBufferCount: 1,
            Control: WSABUF {
                len: size_of_val(&control) as u32,
                buf: PSTR(control.as_mut_ptr().cast()),
            },
            dwFlags: 0,
        };
        let mut sent = 0u32;
        // SAFETY: alle Puffer leben bis zum Ende des synchronen Aufrufs.
        let rc = unsafe { WSASendMsg(self.sock, &msg, 0, Some(&mut sent), None, None) };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(sent as usize)
    }

    /// TX-Timestamp zu einer ID abholen. `Ok(None)` = noch nicht verfügbar.
    pub fn tx_timestamp(&self, id: u32) -> io::Result<Option<u64>> {
        let mut ts = 0u64;
        let mut ret = 0u32;
        // SAFETY: Ein- und Ausgabepuffer passen zu SIO_GET_TX_TIMESTAMP (UINT32 → UINT64).
        let rc = unsafe {
            WSAIoctl(
                self.sock,
                SIO_GET_TX_TIMESTAMP,
                Some((&id as *const u32).cast()),
                4,
                Some((&mut ts as *mut u64).cast()),
                8,
                &mut ret,
                None,
                None,
            )
        };
        if rc == 0 {
            return Ok(Some(ts));
        }
        // SAFETY: Thread-lokaler Fehlercode.
        let err = unsafe { WSAGetLastError() };
        if err == WSAEWOULDBLOCK {
            Ok(None)
        } else {
            Err(io::Error::from_raw_os_error(err.0))
        }
    }
}

fn sockaddr(a: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: a.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from(*a.ip()).to_be(),
            },
        },
        sin_zero: [0; 8],
    }
}

/// SO_TIMESTAMP-Wert in einem Control-Puffer suchen (WSA_CMSG_FIRSTHDR/NXTHDR nachgebaut).
pub fn find_timestamp(ctl: &[u8]) -> Option<u64> {
    let mut off = 0usize;
    while off + size_of::<CMSGHDR>() <= ctl.len() {
        // SAFETY: Bereich liegt innerhalb von ctl; read_unaligned verlangt keine Ausrichtung.
        let hdr: CMSGHDR = unsafe { std::ptr::read_unaligned(ctl.as_ptr().add(off).cast()) };
        if hdr.cmsg_len < size_of::<CMSGHDR>() || off + hdr.cmsg_len > ctl.len() {
            break;
        }
        if hdr.cmsg_level == SOL_SOCKET
            && hdr.cmsg_type == SO_TIMESTAMP as i32
            && hdr.cmsg_len >= CMSG_HDR_LEN + 8
        {
            let p = off + CMSG_HDR_LEN;
            return Some(u64::from_ne_bytes(ctl[p..p + 8].try_into().ok()?));
        }
        off += align(hdr.cmsg_len);
    }
    None
}
