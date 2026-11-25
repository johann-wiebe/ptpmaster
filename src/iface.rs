//! Netzwerkinterfaces auflisten und auswählen.

use std::net::Ipv4Addr;

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceInfo {
    /// Anzeigename (Windows: FriendlyName, z. B. "Ethernet"; Linux: ifname)
    pub name: String,
    /// Technischer Name (Windows: Adapter-GUID; Linux: ifname)
    pub id: String,
    /// Beschreibung (Windows: Treibername, z. B. "Intel(R) Ethernet Controller I210")
    pub description: String,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<Ipv4Addr>,
    pub up: bool,
    pub loopback: bool,
    pub has_gateway: bool,
    pub index: u32,
    /// NET_LUID (nur Windows, sonst 0)
    pub luid: u64,
}

impl InterfaceInfo {
    /// Erste IPv4-Adresse, bevorzugt keine Link-Local-Adresse.
    pub fn primary_ipv4(&self) -> Option<Ipv4Addr> {
        self.ipv4
            .iter()
            .copied()
            .find(|a| !a.is_link_local())
            .or_else(|| self.ipv4.first().copied())
    }

    pub fn mac_string(&self) -> String {
        match self.mac {
            Some(m) => m
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":"),
            None => "-".into(),
        }
    }

    fn matches(&self, spec: &str) -> bool {
        let s = spec.trim();
        self.name.eq_ignore_ascii_case(s)
            || self.id.eq_ignore_ascii_case(s)
            || self.description.eq_ignore_ascii_case(s)
            || self
                .id
                .trim_matches(['{', '}'])
                .eq_ignore_ascii_case(s.trim_matches(['{', '}']))
    }
}

/// Interface auswählen: nach IPv4-Adresse, Name, Beschreibung oder ID.
/// Ohne Angabe: erstes aktives Nicht-Loopback-Interface mit IPv4, bevorzugt mit Gateway.
pub fn select(list: &[InterfaceInfo], spec: Option<&str>) -> Result<(InterfaceInfo, Ipv4Addr)> {
    if let Some(spec) = spec {
        if let Ok(ip) = spec.trim().parse::<Ipv4Addr>() {
            if let Some(i) = list.iter().find(|i| i.ipv4.contains(&ip)) {
                return Ok((i.clone(), ip));
            }
            bail!(
                "Kein Interface mit der Adresse {ip} gefunden. Verfügbar:\n{}",
                summary(list)
            );
        }
        let Some(i) = list.iter().find(|i| i.matches(spec)) else {
            bail!(
                "Interface '{spec}' nicht gefunden. Verfügbar:\n{}",
                summary(list)
            );
        };
        let Some(ip) = i.primary_ipv4() else {
            bail!("Interface '{}' hat keine IPv4-Adresse", i.name);
        };
        if !i.up {
            tracing::warn!("Interface '{}' ist nicht verbunden", i.name);
        }
        return Ok((i.clone(), ip));
    }

    let candidates = || {
        list.iter()
            .filter(|i| i.up && !i.loopback && i.primary_ipv4().is_some())
    };
    let best = candidates()
        .find(|i| i.has_gateway && !i.primary_ipv4().is_some_and(|a| a.is_link_local()))
        .or_else(|| candidates().next());
    match best {
        Some(i) => Ok((i.clone(), i.primary_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED))),
        None => bail!(
            "Kein aktives Netzwerkinterface mit IPv4-Adresse gefunden. Verfügbar:\n{}",
            summary(list)
        ),
    }
}

fn summary(list: &[InterfaceInfo]) -> String {
    list.iter()
        .map(|i| {
            format!(
                "  - {} ({}) {:?}{}",
                i.name,
                i.description,
                i.ipv4,
                if i.up { "" } else { " [nicht verbunden]" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Alle Interfaces mit IPv4 auflisten.
pub fn list() -> Result<Vec<InterfaceInfo>> {
    platform::list()
}

#[cfg(windows)]
mod platform {
    pub use crate::win::iface::list;
}

#[cfg(unix)]
mod platform {
    use super::InterfaceInfo;
    use anyhow::{Result, bail};
    use std::collections::BTreeMap;
    use std::ffi::CStr;
    use std::net::Ipv4Addr;

    pub fn list() -> Result<Vec<InterfaceInfo>> {
        let mut map: BTreeMap<String, InterfaceInfo> = BTreeMap::new();
        // SAFETY: getifaddrs liefert eine verkettete Liste, die wir mit freeifaddrs freigeben.
        unsafe {
            let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut addrs) != 0 {
                bail!(
                    "getifaddrs fehlgeschlagen: {}",
                    std::io::Error::last_os_error()
                );
            }
            let mut cur = addrs;
            while !cur.is_null() {
                let ifa = &*cur;
                cur = ifa.ifa_next;
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                let entry = map.entry(name.clone()).or_insert_with(|| InterfaceInfo {
                    name: name.clone(),
                    id: name.clone(),
                    description: String::new(),
                    mac: read_mac(&name),
                    ipv4: Vec::new(),
                    up: ifa.ifa_flags & libc::IFF_UP as u32 != 0
                        && ifa.ifa_flags & libc::IFF_RUNNING as u32 != 0,
                    loopback: ifa.ifa_flags & libc::IFF_LOOPBACK as u32 != 0,
                    has_gateway: false,
                    index: libc::if_nametoindex(ifa.ifa_name),
                    luid: 0,
                });
                if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    entry
                        .ipv4
                        .push(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                }
            }
            libc::freeifaddrs(addrs);
        }
        let gw = default_route_ifaces();
        Ok(map
            .into_values()
            .filter(|i| !i.ipv4.is_empty())
            .map(|mut i| {
                i.has_gateway = gw.contains(&i.name);
                i
            })
            .collect())
    }

    fn read_mac(name: &str) -> Option<[u8; 6]> {
        let s = std::fs::read_to_string(format!("/sys/class/net/{name}/address")).ok()?;
        let bytes: Vec<u8> = s
            .trim()
            .split(':')
            .filter_map(|p| u8::from_str_radix(p, 16).ok())
            .collect();
        let mac: [u8; 6] = bytes.try_into().ok()?;
        (mac != [0; 6]).then_some(mac)
    }

    fn default_route_ifaces() -> Vec<String> {
        std::fs::read_to_string("/proc/net/route")
            .unwrap_or_default()
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() > 2 && f[1] == "00000000").then(|| f[0].to_string())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: [u8; 4], up: bool, lo: bool, gw: bool) -> InterfaceInfo {
        InterfaceInfo {
            name: name.into(),
            id: format!("{{{name}-GUID}}"),
            description: format!("{name} Adapter"),
            mac: Some([0x00, 0x1b, 0x21, 0x01, 0x02, 0x03]),
            ipv4: vec![Ipv4Addr::from(ip)],
            up,
            loopback: lo,
            has_gateway: gw,
            index: 1,
            luid: 0,
        }
    }

    fn sample() -> Vec<InterfaceInfo> {
        vec![
            iface("Loopback", [127, 0, 0, 1], true, true, false),
            iface("WLAN", [192, 168, 1, 50], true, false, false),
            iface("Ethernet", [192, 168, 1, 20], true, false, true),
            iface("Ethernet 2", [10, 0, 0, 5], false, false, false),
        ]
    }

    #[test]
    fn auto_prefers_gateway() {
        let (i, ip) = select(&sample(), None).unwrap();
        assert_eq!(i.name, "Ethernet");
        assert_eq!(ip, Ipv4Addr::new(192, 168, 1, 20));
    }

    #[test]
    fn select_by_name_ip_description_guid() {
        let l = sample();
        assert_eq!(select(&l, Some("wlan")).unwrap().0.name, "WLAN");
        assert_eq!(select(&l, Some("10.0.0.5")).unwrap().0.name, "Ethernet 2");
        assert_eq!(
            select(&l, Some("Ethernet Adapter")).unwrap().0.name,
            "Ethernet"
        );
        assert_eq!(
            select(&l, Some("Ethernet-GUID")).unwrap().0.name,
            "Ethernet"
        );
        assert_eq!(select(&l, Some("127.0.0.1")).unwrap().0.name, "Loopback");
        assert!(select(&l, Some("eth9")).is_err());
        assert!(select(&l, Some("1.2.3.4")).is_err());
    }

    #[test]
    fn mac_formatting() {
        assert_eq!(sample()[0].mac_string(), "00:1b:21:01:02:03");
    }
}
