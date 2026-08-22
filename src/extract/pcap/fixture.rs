//! Capture files built byte by byte, for the tests in this module and its two
//! children.
//!
//! Written out rather than checked in as sample files on purpose. Every field
//! that matters to the reader — the magics, the block lengths, the header
//! lengths the decoder trusts — is a number in this file, so a test can bend
//! exactly one of them and leave the rest correct. A binary fixture would hide
//! all of that behind an opaque blob.

/// An Ethernet frame around an IPv4 packet.
pub fn ethernet(ether_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0xFF; 6];
    frame.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    frame.extend_from_slice(&ether_type.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// An IPv4 header with `payload` behind it.
pub fn ipv4(protocol: u8, source: [u8; 4], destination: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut packet = vec![0x45, 0x00];
    packet.extend_from_slice(&(total as u16).to_be_bytes());
    // id, then flags and fragment offset, both zero: this is a whole packet.
    packet.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
    packet.extend_from_slice(&[64, protocol, 0x00, 0x00]);
    packet.extend_from_slice(&source);
    packet.extend_from_slice(&destination);
    packet.extend_from_slice(payload);
    packet
}

/// A TCP header with no payload, carrying `flags`.
pub fn tcp(source: u16, destination: u16, flags: u8) -> Vec<u8> {
    let mut segment = Vec::new();
    segment.extend_from_slice(&source.to_be_bytes());
    segment.extend_from_slice(&destination.to_be_bytes());
    // Sequence and acknowledgement numbers, which nothing here reads.
    segment.extend_from_slice(&[0; 8]);
    // A five-word header, then the flags.
    segment.extend_from_slice(&[0x50, flags]);
    // Window, checksum, urgent pointer.
    segment.extend_from_slice(&[0; 6]);
    segment
}

/// A UDP header with `payload` behind it.
pub fn udp(source: u16, destination: u16, payload: &[u8]) -> Vec<u8> {
    let mut datagram = Vec::new();
    datagram.extend_from_slice(&source.to_be_bytes());
    datagram.extend_from_slice(&destination.to_be_bytes());
    datagram.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    datagram.extend_from_slice(&[0, 0]);
    datagram.extend_from_slice(payload);
    datagram
}

/// A DNS message asking for `name`, or answering about it when `response`.
pub fn dns_query(name: &str, response: bool) -> Vec<u8> {
    let mut message = vec![0x12, 0x34];
    message.extend_from_slice(&if response { 0x8180u16 } else { 0x0100 }.to_be_bytes());
    // One question, no answers, no authority, no additional.
    message.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.push(0);
    // Type A, class IN.
    message.extend_from_slice(&[0, 1, 0, 1]);
    message
}

/// The whole of an ordinary conversation: a client opening a connection to a
/// server, being answered, and both ends closing it.
pub fn handshake(client: [u8; 4], server: [u8; 4], port: u16) -> Vec<Vec<u8>> {
    let syn = 0x02;
    let syn_ack = 0x12;
    let fin_ack = 0x11;
    vec![
        ethernet(0x0800, &ipv4(6, client, server, &tcp(51_000, port, syn))),
        ethernet(
            0x0800,
            &ipv4(6, server, client, &tcp(port, 51_000, syn_ack)),
        ),
        ethernet(
            0x0800,
            &ipv4(6, client, server, &tcp(51_000, port, fin_ack)),
        ),
    ]
}

/// A classic `.pcap` file: the 24-byte global header, then one record per
/// packet. Little-endian and microseconds unless told otherwise.
pub fn classic(link_type: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    classic_with_magic([0xD4, 0xC3, 0xB2, 0xA1], link_type, packets)
}

pub fn classic_with_magic(magic: [u8; 4], link_type: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = magic.to_vec();
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&65_535u32.to_le_bytes());
    out.extend_from_slice(&link_type.to_le_bytes());

    for (index, packet) in packets.iter().enumerate() {
        // One packet a second from an arbitrary but real-looking moment.
        out.extend_from_slice(&(1_700_000_000u32 + index as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        out.extend_from_slice(packet);
    }
    out
}

/// A `.pcapng` file: a section header, one interface, then an enhanced packet
/// block per packet.
pub fn pcapng(link_type: u16, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();

    // Section header block: type, length, byte-order magic, version, an
    // unspecified section length, then the length again.
    out.extend_from_slice(&0x0A0D_0D0Au32.to_le_bytes());
    out.extend_from_slice(&28u32.to_le_bytes());
    out.extend_from_slice(&0x1A2B_3C4Du32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(-1i64).to_le_bytes());
    out.extend_from_slice(&28u32.to_le_bytes());

    // Interface description block, with no options.
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&20u32.to_le_bytes());
    out.extend_from_slice(&link_type.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&65_535u32.to_le_bytes());
    out.extend_from_slice(&20u32.to_le_bytes());

    for (index, packet) in packets.iter().enumerate() {
        // The body is padded to a multiple of four; the padding is not counted
        // in the captured length.
        let padding = packet.len().next_multiple_of(4) - packet.len();
        let total = 32 + packet.len() + padding;
        out.extend_from_slice(&6u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        // Microseconds since the epoch, split high and low, one second apart.
        let ticks = (1_700_000_000u64 + index as u64) * 1_000_000;
        out.extend_from_slice(&((ticks >> 32) as u32).to_le_bytes());
        out.extend_from_slice(&(ticks as u32).to_le_bytes());
        out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
        out.extend_from_slice(packet);
        out.extend(std::iter::repeat_n(0u8, padding));
        out.extend_from_slice(&(total as u32).to_le_bytes());
    }
    out
}

/// Writes `bytes` to a uniquely named file in the temporary directory.
pub fn on_disk(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}
