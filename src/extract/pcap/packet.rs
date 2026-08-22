//! Working out who was talking to whom, from the bytes of one packet.
//!
//! Only as far up the stack as the summary needs: the two addresses, the
//! protocol, the two ports, the TCP flags that say whether a connection opened
//! and closed properly, and — for DNS alone — one field of the payload.
//!
//! The names looked up are worth that exception. Addresses are the least
//! memorable thing in a capture and mean nothing read aloud; the domain names
//! asked for beside them are what actually says what a machine was doing, and
//! they are the difference between "a conversation with 93.184.216.34" and "it
//! fetched something from example.com".
//!
//! Nothing here trusts a length. Every field is read through a bounds-checked
//! accessor, so a packet whose header claims more than the capture holds
//! produces `None` rather than a panic — which, in an app that reads files
//! recorded off a hostile network, is the whole game. A packet that cannot be
//! decoded is counted and dropped, never guessed at.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The link layers worth decoding, by the number the capture files them under.
pub mod link {
    /// BSD loopback: a four-byte address family, then IP.
    pub const NULL: u32 = 0;
    pub const ETHERNET: u32 = 1;
    /// A capture taken on `any` on Linux: a sixteen-byte pseudo-header.
    pub const LINUX_SLL: u32 = 113;
    /// Its replacement, twenty bytes and with the protocol at the front.
    pub const LINUX_SLL2: u32 = 276;
    /// IP with no link layer at all, as a tunnel interface produces.
    pub const RAW: u32 = 101;
    pub const IPV4: u32 = 228;
    pub const IPV6: u32 = 229;
}

/// What one packet turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// An IP conversation, which is nearly all of any capture.
    Ip(Flow),
    /// Address resolution: no IP layer, and worth counting rather than
    /// decoding, since a burst of it is a machine looking for something.
    Arp,
    /// Understood as far as the link layer and no further.
    Other,
}

/// One packet's worth of an IP conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub protocol: Protocol,
    /// `None` for a protocol with no ports, and for a fragment that is not the
    /// first — where the ports are in a different packet.
    pub ports: Option<(u16, u16)>,
    pub tcp_flags: u8,
    /// The name asked for, when this is a DNS query this app could read.
    pub query: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
    Icmp,
    /// Anything else, kept as its number so the summary can say what it saw.
    Other(u8),
}

impl Protocol {
    /// How the protocol is said aloud.
    pub fn name(self) -> String {
        match self {
            Self::Tcp => "TCP".to_string(),
            Self::Udp => "UDP".to_string(),
            Self::Icmp => "ICMP".to_string(),
            // The handful worth naming; the rest are said by number, which is
            // at least honest.
            Self::Other(2) => "IGMP".to_string(),
            Self::Other(47) => "GRE".to_string(),
            Self::Other(50) => "ESP".to_string(),
            Self::Other(51) => "AH".to_string(),
            Self::Other(58) => "ICMPv6".to_string(),
            Self::Other(89) => "OSPF".to_string(),
            Self::Other(132) => "SCTP".to_string(),
            Self::Other(n) => format!("IP protocol {n}"),
        }
    }
}

/// TCP flag bits, as far as the summary cares.
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const FIN: u8 = 0x01;
pub const ACK: u8 = 0x10;

/// Reads one packet as far as it can be read.
pub fn decode(link_type: u32, data: &[u8]) -> Option<Decoded> {
    match link_type {
        link::ETHERNET => ethernet(data),
        // The four-byte family is written in the capturing host's byte order,
        // so both ends are tried; the values are small enough that only one of
        // them can be a family.
        link::NULL => {
            let family = data.get(..4)?;
            let value = u32::from_ne_bytes([family[0], family[1], family[2], family[3]]);
            let swapped = u32::from_be_bytes([family[0], family[1], family[2], family[3]]);
            let rest = data.get(4..)?;
            match (value, swapped) {
                (2, _) | (_, 2) => ipv4(rest).map(Decoded::Ip),
                // 24, 28 and 30 are all AF_INET6 on one BSD or another.
                (24 | 28 | 30, _) | (_, 24 | 28 | 30) => ipv6(rest).map(Decoded::Ip),
                _ => Some(Decoded::Other),
            }
        }
        link::RAW | link::IPV4 | link::IPV6 => ip(data).map(Decoded::Ip),
        link::LINUX_SLL => {
            let ether_type = u16::from_be_bytes([*data.get(14)?, *data.get(15)?]);
            by_ether_type(ether_type, data.get(16..)?)
        }
        link::LINUX_SLL2 => {
            let ether_type = u16::from_be_bytes([*data.first()?, *data.get(1)?]);
            by_ether_type(ether_type, data.get(20..)?)
        }
        _ => None,
    }
}

fn ethernet(data: &[u8]) -> Option<Decoded> {
    let mut at = 12;
    let mut ether_type = u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]);
    at += 2;
    // VLAN tags, which sit between the addresses and the type and each push the
    // real type four bytes further in. Stacked tags are legal, so this loops —
    // bounded, since each turn consumes four bytes of a finite packet.
    while matches!(ether_type, 0x8100 | 0x88A8 | 0x9100) {
        at += 2;
        ether_type = u16::from_be_bytes([*data.get(at)?, *data.get(at + 1)?]);
        at += 2;
    }
    by_ether_type(ether_type, data.get(at..)?)
}

fn by_ether_type(ether_type: u16, rest: &[u8]) -> Option<Decoded> {
    match ether_type {
        0x0800 => ipv4(rest).map(Decoded::Ip),
        0x86DD => ipv6(rest).map(Decoded::Ip),
        0x0806 => Some(Decoded::Arp),
        _ => Some(Decoded::Other),
    }
}

/// IP of whichever version the first nibble says.
fn ip(data: &[u8]) -> Option<Flow> {
    match data.first()? >> 4 {
        4 => ipv4(data),
        6 => ipv6(data),
        _ => None,
    }
}

fn ipv4(data: &[u8]) -> Option<Flow> {
    let header_len = ((data.first()? & 0x0F) as usize) * 4;
    if header_len < 20 || data.len() < header_len {
        return None;
    }
    let protocol = protocol_of(*data.get(9)?);
    let source = IpAddr::V4(Ipv4Addr::new(
        *data.get(12)?,
        *data.get(13)?,
        *data.get(14)?,
        *data.get(15)?,
    ));
    let destination = IpAddr::V4(Ipv4Addr::new(
        *data.get(16)?,
        *data.get(17)?,
        *data.get(18)?,
        *data.get(19)?,
    ));

    // The low thirteen bits of the fragment field. Anything but zero means the
    // transport header is in an earlier packet, not this one.
    let fragment_offset = u16::from_be_bytes([*data.get(6)?, *data.get(7)?]) & 0x1FFF;
    let payload = if fragment_offset == 0 {
        data.get(header_len..)
    } else {
        None
    };

    Some(transport(source, destination, protocol, payload))
}

fn ipv6(data: &[u8]) -> Option<Flow> {
    if data.len() < 40 {
        return None;
    }
    let address = |at: usize| -> Option<IpAddr> {
        let bytes: [u8; 16] = data.get(at..at + 16)?.try_into().ok()?;
        Some(IpAddr::V6(Ipv6Addr::from(bytes)))
    };
    let source = address(8)?;
    let destination = address(24)?;

    // Extension headers, each of which names the next and says how long it is
    // in eight-byte units past the first eight. Walked rather than assumed
    // absent, since a packet carrying one would otherwise be read as if its
    // options were the transport header.
    let mut next = *data.get(6)?;
    let mut at = 40usize;
    let mut steps = 0;
    // Hop-by-hop, routing and destination options, all of which measure
    // themselves the same way. Authentication headers are deliberately not in
    // the list: they count their length in different units, and reporting one
    // as "AH" is better than walking off the end of it into nothing.
    while matches!(next, 0 | 43 | 60) && steps < 8 {
        let length = (*data.get(at + 1)? as usize + 1) * 8;
        next = *data.get(at)?;
        at += length;
        steps += 1;
    }
    // A fragment header means the transport header may not be here at all, and
    // its own layout differs; the flow is still worth counting without ports.
    if next == 44 {
        return Some(transport(source, destination, Protocol::Other(44), None));
    }

    Some(transport(
        source,
        destination,
        protocol_of(next),
        data.get(at..),
    ))
}

fn protocol_of(number: u8) -> Protocol {
    match number {
        1 => Protocol::Icmp,
        6 => Protocol::Tcp,
        17 => Protocol::Udp,
        other => Protocol::Other(other),
    }
}

/// Pulls the ports and TCP flags out of whatever the IP header was carrying.
fn transport(
    source: IpAddr,
    destination: IpAddr,
    protocol: Protocol,
    payload: Option<&[u8]>,
) -> Flow {
    let mut flow = Flow {
        source,
        destination,
        protocol,
        ports: None,
        tcp_flags: 0,
        query: None,
    };
    let Some(payload) = payload else {
        return flow;
    };

    let ports = |data: &[u8]| -> Option<(u16, u16)> {
        Some((
            u16::from_be_bytes([*data.first()?, *data.get(1)?]),
            u16::from_be_bytes([*data.get(2)?, *data.get(3)?]),
        ))
    };

    match protocol {
        Protocol::Tcp => {
            flow.ports = ports(payload);
            flow.tcp_flags = payload.get(13).copied().unwrap_or(0);
        }
        Protocol::Udp => {
            flow.ports = ports(payload);
            // The one payload worth opening. A DNS message starts eight bytes
            // in, past the UDP header.
            if let Some((from, to)) = flow.ports
                && (from == 53 || to == 53)
            {
                flow.query = payload.get(8..).and_then(dns_question);
            }
        }
        _ => {}
    }
    flow
}

/// The name in the first question of a DNS message, when there is one.
///
/// Only queries are read, and only the question section. An answer's name is
/// usually a compression pointer back into the question, and following pointers
/// through a file recorded off a network means guarding against a pointer loop
/// for a fact this summary already has from the query.
///
/// The name is capped rather than trusted: 255 bytes is the protocol's own
/// limit for an encoded name, and a message that exceeds it is malformed.
fn dns_question(message: &[u8]) -> Option<String> {
    // Header: a two-byte id, then the two-byte flags, then four counts. The
    // top bit of the flags is set on a response.
    let flags = u16::from_be_bytes([*message.get(2)?, *message.get(3)?]);
    if flags & 0x8000 != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([*message.get(4)?, *message.get(5)?]);
    if questions == 0 {
        return None;
    }

    let mut at = 12usize;
    let mut labels: Vec<String> = Vec::new();
    let mut used = 0usize;
    loop {
        let length = *message.get(at)? as usize;
        // A length whose top two bits are set is a compression pointer, which
        // has no business being the first thing in a question.
        if length & 0xC0 != 0 {
            return None;
        }
        at += 1;
        if length == 0 {
            break;
        }
        used += length + 1;
        if used > 255 {
            return None;
        }
        let label = message.get(at..at + length)?;
        // The narrowest thing that is still a hostname: letters, digits,
        // hyphens, and the underscore that `_dmarc` and `_sip._tcp` need.
        //
        // This is the one string in a capture that travels — it is spoken
        // aloud, and it is placed inside the prompt given to a text model —
        // and every byte of it was chosen by whoever sent the traffic. Holding
        // it to what a name may actually contain costs nothing (a real lookup
        // passes) and takes the punctuation an injected instruction would want
        // off the table. It cannot make the name *meaningless* — see the note
        // on untrusted names in [`super::transcript`] — but there is no reason
        // to carry more than a name.
        if !label
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
        {
            return None;
        }
        labels.push(String::from_utf8_lossy(label).to_string());
        at += length;
    }

    (!labels.is_empty()).then(|| labels.join("."))
}

/// The service a well-known port stands for, said the way someone would say it.
///
/// Only the ports worth naming aloud. A number is a poor thing to hear and a
/// good half of a capture is one of these; anything not on the list is left as
/// its number, which is better than a wrong guess.
pub fn service_name(port: u16) -> Option<&'static str> {
    Some(match port {
        20 | 21 => "FTP",
        22 => "SSH",
        23 => "Telnet",
        25 | 465 | 587 => "email being sent",
        53 => "DNS",
        67 | 68 => "DHCP",
        69 => "TFTP",
        80 | 8080 => "HTTP",
        110 | 995 => "email being collected",
        123 => "network time",
        143 | 993 => "IMAP",
        161 | 162 => "SNMP",
        389 | 636 => "LDAP",
        443 | 8443 => "HTTPS",
        445 => "Windows file sharing",
        548 => "Apple file sharing",
        631 => "printing",
        853 => "DNS over TLS",
        1433 => "SQL Server",
        1900 => "device discovery",
        3306 => "MySQL",
        3389 => "remote desktop",
        5060 | 5061 => "SIP",
        5353 => "Bonjour",
        5432 => "PostgreSQL",
        6379 => "Redis",
        27017 => "MongoDB",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::pcap::fixture;

    fn ip(a: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(a))
    }

    #[test]
    fn an_ethernet_frame_gives_up_its_addresses_ports_and_flags() {
        let frame = fixture::ethernet(
            0x0800,
            &fixture::ipv4(
                6,
                [192, 168, 1, 5],
                [93, 184, 216, 34],
                &fixture::tcp(51_000, 443, SYN),
            ),
        );
        let Some(Decoded::Ip(flow)) = decode(link::ETHERNET, &frame) else {
            panic!("an ordinary TCP packet should decode");
        };
        assert_eq!(flow.source, ip([192, 168, 1, 5]));
        assert_eq!(flow.destination, ip([93, 184, 216, 34]));
        assert_eq!(flow.protocol, Protocol::Tcp);
        assert_eq!(flow.ports, Some((51_000, 443)));
        assert_eq!(flow.tcp_flags & SYN, SYN);
        assert_eq!(flow.tcp_flags & ACK, 0);
    }

    /// A tagged frame pushes the real type four bytes further in. Read without
    /// allowing for it, every packet on a VLAN decodes as nothing at all.
    #[test]
    fn vlan_tags_do_not_hide_the_packet_behind_them() {
        let inner = fixture::ipv4(
            17,
            [10, 0, 0, 1],
            [10, 0, 0, 2],
            &fixture::udp(5000, 53, &fixture::dns_query("example.com", false)),
        );
        // A frame with the type replaced by a tag, then the real type.
        let mut frame = vec![0xFF; 12];
        frame.extend_from_slice(&0x8100u16.to_be_bytes());
        frame.extend_from_slice(&0x0064u16.to_be_bytes());
        frame.extend_from_slice(&0x0800u16.to_be_bytes());
        frame.extend_from_slice(&inner);

        let Some(Decoded::Ip(flow)) = decode(link::ETHERNET, &frame) else {
            panic!("a tagged frame should decode to the packet inside it");
        };
        assert_eq!(flow.protocol, Protocol::Udp);
        assert_eq!(flow.query.as_deref(), Some("example.com"));
    }

    /// The names asked for are the one part of a capture that says what a
    /// machine was actually doing.
    #[test]
    fn a_dns_question_gives_up_the_name_it_asks_for() {
        let query = fixture::udp(
            51_000,
            53,
            &fixture::dns_query("updates.example.com", false),
        );
        let packet = fixture::ipv4(17, [10, 0, 0, 1], [10, 0, 0, 53], &query);
        let Some(Decoded::Ip(flow)) = decode(link::RAW, &packet) else {
            panic!("a DNS query should decode");
        };
        assert_eq!(flow.query.as_deref(), Some("updates.example.com"));
    }

    /// Only questions are read. An answer's name is usually a compression
    /// pointer, and following those through a file recorded off a network buys
    /// a fact the query already gave.
    #[test]
    fn a_dns_answer_is_left_alone() {
        let answer = fixture::udp(53, 51_000, &fixture::dns_query("updates.example.com", true));
        let packet = fixture::ipv4(17, [10, 0, 0, 53], [10, 0, 0, 1], &answer);
        let Some(Decoded::Ip(flow)) = decode(link::RAW, &packet) else {
            panic!("a DNS answer should still decode as a flow");
        };
        assert_eq!(flow.query, None);
    }

    /// A compression pointer where a length should be, which is the shape of a
    /// malformed or hostile query.
    #[test]
    fn a_compression_pointer_in_a_question_is_refused_rather_than_followed() {
        let mut message = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        // 0xC0 0x0C: "the name is back at offset 12", which is here.
        message.extend_from_slice(&[0xC0, 0x0C]);
        assert_eq!(dns_question(&message), None);
    }

    /// A capture is a recording of what arrived off a network, so every length
    /// in it is attacker-controlled. None of these may panic.
    #[test]
    fn packets_that_end_early_produce_nothing_rather_than_panicking() {
        let full = fixture::ethernet(
            0x0800,
            &fixture::ipv4(6, [1, 2, 3, 4], [5, 6, 7, 8], &fixture::tcp(80, 80, SYN)),
        );
        // Every prefix of a real packet, which is every way one can be cut off.
        for length in 0..full.len() {
            let _ = decode(link::ETHERNET, &full[..length]);
        }
        // A header claiming a length the packet does not have.
        let mut lying = full.clone();
        lying[14] = 0x4F; // twenty words of IPv4 header in a twenty-byte one
        assert!(decode(link::ETHERNET, &lying).is_none());
        assert_eq!(decode(link::ETHERNET, &[]), None);
    }

    /// A fragment that is not the first carries no transport header, so
    /// reading one as though it did would invent two ports out of payload.
    #[test]
    fn a_later_fragment_reports_no_ports_rather_than_reading_payload_as_them() {
        let mut packet = fixture::ipv4(6, [1, 2, 3, 4], [5, 6, 7, 8], &fixture::tcp(80, 443, 0));
        // A fragment offset of 185, which is what the second fragment of a
        // 1500-byte packet carries.
        packet[6] = 0x00;
        packet[7] = 0xB9;
        let Some(Decoded::Ip(flow)) = decode(link::RAW, &packet) else {
            panic!("a fragment is still a packet between two addresses");
        };
        assert_eq!(flow.ports, None);
    }

    #[test]
    fn arp_is_recognised_without_being_decoded() {
        let frame = fixture::ethernet(0x0806, &[0; 28]);
        assert_eq!(decode(link::ETHERNET, &frame), Some(Decoded::Arp));
    }

    /// A number is a poor thing to hear, and a good half of a capture is one of
    /// these.
    #[test]
    fn well_known_ports_are_named_and_the_rest_are_left_as_numbers() {
        assert_eq!(service_name(443), Some("HTTPS"));
        assert_eq!(service_name(53), Some("DNS"));
        assert_eq!(service_name(22), Some("SSH"));
        assert_eq!(service_name(51_000), None);
    }

    #[test]
    fn protocols_without_a_name_are_said_by_number_rather_than_guessed_at() {
        assert_eq!(Protocol::Tcp.name(), "TCP");
        assert_eq!(Protocol::Other(58).name(), "ICMPv6");
        assert_eq!(Protocol::Other(200).name(), "IP protocol 200");
    }
}

/// The decoder's half of the malformed-input tests — see the `robustness`
/// module in [`super::file`], which covers the containers.
#[cfg(test)]
mod robustness {
    use super::*;

    /// Every link type the decoder claims to handle, fed bytes that are not a
    /// packet. No file is involved, so this runs in a moment and can afford to
    /// be exhaustive about lengths.
    #[test]
    fn arbitrary_bytes_under_every_link_type_decode_to_something_or_nothing() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };

        let types = [
            link::NULL,
            link::ETHERNET,
            link::RAW,
            link::LINUX_SLL,
            link::LINUX_SLL2,
            link::IPV4,
            link::IPV6,
            // One the decoder does not know, which must simply say so.
            9_999,
        ];

        for link_type in types {
            // Every length from empty to past a full header, so a field read
            // one byte beyond what is present is caught rather than assumed
            // impossible.
            for length in 0..80usize {
                for _ in 0..40 {
                    let packet: Vec<u8> = (0..length).map(|_| next() as u8).collect();
                    let _ = decode(link_type, &packet);
                }
            }
        }
    }

    /// A DNS message is the one payload the decoder opens, and the only place
    /// a capture's bytes become a string that is spoken aloud and put into a
    /// model's prompt. It gets its own pass.
    #[test]
    fn a_dns_message_of_arbitrary_bytes_never_panics_and_never_yields_junk() {
        let mut state = 0xFEED_FACE_DEAD_BEEFu64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };

        for length in 0..64usize {
            for _ in 0..200 {
                let mut message: Vec<u8> = (0..length).map(|_| next() as u8).collect();
                // Half of them forced to look like a query, so the walk over
                // the question is actually reached rather than turned back at
                // the flags.
                if length >= 6 && next() % 2 == 0 {
                    message[2] = 0x01;
                    message[3] = 0x00;
                    message[4] = 0x00;
                    message[5] = 0x01;
                }
                if let Some(name) = dns_question(&message) {
                    // Whatever comes back is going to be read out and put in a
                    // prompt, so it has to be a name and nothing else.
                    assert!(name.len() <= 255, "{name}");
                    assert!(
                        name.bytes().all(|b| b.is_ascii_alphanumeric()
                            || b == b'-'
                            || b == b'_'
                            || b == b'.'),
                        "a name reached the summary that is not one: {name:?}"
                    );
                }
            }
        }
    }
}
