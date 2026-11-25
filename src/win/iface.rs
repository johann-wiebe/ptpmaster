//! Interface-Liste über GetAdaptersAddresses.

use std::net::Ipv4Addr;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

use crate::iface::InterfaceInfo;

pub fn list() -> Result<Vec<InterfaceInfo>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 32 * 1024;
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: Puffer hat mindestens `size` Bytes und ist 8-Byte-ausgerichtet.
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_INET.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut size,
            )
        };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if rc != NO_ERROR.0 {
            bail!("GetAdaptersAddresses fehlgeschlagen: Fehler {rc}");
        }
        break;
    }

    let mut out = Vec::new();
    let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: Windows hat eine verkettete Liste innerhalb von `buf` aufgebaut.
    unsafe {
        while !cur.is_null() {
            let a = &*cur;
            cur = a.Next;

            let mut ipv4 = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = &*u;
                u = ua.Next;
                let sa = ua.Address.lpSockaddr;
                if !sa.is_null() && (*sa).sa_family == AF_INET {
                    let sin = &*(sa as *const SOCKADDR_IN);
                    ipv4.push(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)));
                }
            }
            if ipv4.is_empty() {
                continue;
            }
            let mac = (a.PhysicalAddressLength == 6).then(|| {
                let mut m = [0u8; 6];
                m.copy_from_slice(&a.PhysicalAddress[..6]);
                m
            });
            out.push(InterfaceInfo {
                name: super::pwstr_to_string(a.FriendlyName),
                id: a.AdapterName.to_string().unwrap_or_default(),
                description: super::pwstr_to_string(a.Description),
                mac,
                ipv4,
                up: a.OperStatus == IfOperStatusUp,
                loopback: a.IfType == IF_TYPE_SOFTWARE_LOOPBACK,
                has_gateway: !a.FirstGatewayAddress.is_null(),
                index: a.Anonymous1.Anonymous.IfIndex,
                luid: a.Luid.Value,
            });
        }
    }
    Ok(out)
}
