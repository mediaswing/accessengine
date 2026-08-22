//! The two container formats a capture arrives in, and nothing above them.
//!
//! A `.pcap` file is a 24-byte header followed by packets, each behind a
//! 16-byte record of when it was captured and how much of it was kept. A
//! `.pcapng` file is a stream of typed blocks instead, of which three matter
//! here: the section header that says which way round the numbers are, the
//! interface descriptions that say what link layer the packets are from and how
//! finely their timestamps are counted, and the packet blocks themselves.
//!
//! Both are read as a stream and handed one packet at a time to a callback,
//! never collected. A capture is the one kind of file this app opens that has
//! no natural size — an hour of a busy link is gigabytes — and the summary is
//! built from running totals, so there is no reason to hold more than one
//! packet at a time.
//!
//! Everything here is defensive to the point of tedium. A capture is a file
//! full of attacker-controlled bytes by definition: it is a recording of what
//! arrived off a network, and the lengths inside it are as untrustworthy as
//! the packets they describe. Every length is checked against what is actually
//! there before it is used, and nothing is allocated on the strength of a
//! number read out of the file.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::time::Duration;

/// One captured packet, borrowed from the reader's own buffer for the length of
/// the callback.
pub struct Packet<'a> {
    /// When it was captured, as time since the Unix epoch. `None` for a format
    /// or a block that carries no clock.
    pub at: Option<Duration>,
    /// The link layer the bytes start with — see [`super::packet::LinkType`].
    pub link_type: u32,
    /// As much of the packet as the capture kept.
    pub data: &'a [u8],
    /// How long it was on the wire, which is larger than `data` whenever the
    /// capture was taken with a snap length.
    pub original_len: u32,
}

/// The most one packet may claim to be. Well past a jumbo frame, and short
/// enough that a corrupt length cannot ask for an allocation that matters.
const MAX_PACKET_BYTES: u32 = 8 * 1024 * 1024;

/// The most one pcapng block may claim to be, for the same reason.
const MAX_BLOCK_BYTES: u32 = 16 * 1024 * 1024;

/// The most packets that will be read from one capture.
///
/// Every packet costs a little arithmetic and nothing else, so this is high;
/// it exists so that a capture of a saturated link cannot leave the app looking
/// hung with no way out, not because the totals would stop being meaningful.
pub const MAX_PACKETS: u64 = 5_000_000;

/// Reads `path` and hands each packet to `on_packet`, which returns `false` to
/// stop — how the Cancel button reaches a file that may be gigabytes long.
///
/// Returns how many packets were read, and whether the file ran out before it
/// said it would — a truncated capture is extremely common, since it is what
/// every capture stopped with Ctrl-C looks like, and is worth reporting rather
/// than failing on.
pub fn for_each_packet(
    path: &std::path::Path,
    on_packet: &mut impl FnMut(&Packet<'_>) -> bool,
) -> Result<Summary> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);

    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).with_context(|| {
        format!(
            "{} is too short to be a capture file",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
    })?;

    match magic {
        // A section header block: the file is pcapng. Its own byte order lives
        // further in, so the magic alone is the same either way round.
        [0x0A, 0x0D, 0x0D, 0x0A] => read_pcapng(&mut reader, on_packet),
        [0xD4, 0xC3, 0xB2, 0xA1] => read_classic(&mut reader, on_packet, Endian::Little, 1_000),
        [0xA1, 0xB2, 0xC3, 0xD4] => read_classic(&mut reader, on_packet, Endian::Big, 1_000),
        // The nanosecond variants, which differ only in what the second field
        // of each timestamp counts.
        [0x4D, 0x3C, 0xB2, 0xA1] => read_classic(&mut reader, on_packet, Endian::Little, 1),
        [0xA1, 0xB2, 0x3C, 0x4D] => read_classic(&mut reader, on_packet, Endian::Big, 1),
        _ => bail!(
            "{} does not look like a capture file — its first four bytes are not a pcap or \
             pcapng signature",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    }
}

/// What reading the file established about the file itself, as opposed to about
/// the traffic in it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub packets: u64,
    /// The file ended part-way through a packet or block. Ordinary rather than
    /// alarming: it is what a capture interrupted at the keyboard looks like.
    pub truncated: bool,
    /// Reading stopped at [`MAX_PACKETS`] with more still in the file.
    pub capped: bool,
    /// Packets whose link layer this app does not decode, counted so the
    /// summary can admit to them rather than quietly under-reporting.
    pub unknown_link_type: u64,
    /// The callback asked to stop, which in this app means the user did.
    pub stopped: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u32(self, bytes: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }
}

/// Reads exactly `buf.len()` bytes, reporting a clean end-of-file as `false`
/// rather than as an error — which is the difference between "the capture
/// finished" and "the capture is unreadable".
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => return Ok(false),
            n => filled += n,
        }
    }
    Ok(true)
}

/// Throws away `count` bytes without allocating room for them, so a block this
/// app does not understand costs nothing to step over however large it claims
/// to be.
fn skip(reader: &mut impl Read, count: u64) -> Result<bool> {
    let mut left = count;
    let mut scratch = [0u8; 4096];
    while left > 0 {
        let want = left.min(scratch.len() as u64) as usize;
        if !fill(reader, &mut scratch[..want])? {
            return Ok(false);
        }
        left -= want as u64;
    }
    Ok(true)
}

/// The classic format: one global header, already partly consumed, then a flat
/// run of packet records.
///
/// `tick_divisor` turns the second timestamp field into nanoseconds — 1,000 for
/// the microsecond magics and 1 for the nanosecond ones — which is the only
/// difference between the two pairs.
fn read_classic(
    reader: &mut impl Read,
    on_packet: &mut impl FnMut(&Packet<'_>) -> bool,
    endian: Endian,
    tick_divisor: u32,
) -> Result<Summary> {
    // The rest of the 24-byte global header; the magic is already gone.
    let mut rest = [0u8; 20];
    if !fill(reader, &mut rest)? {
        bail!("this capture file ends inside its own header");
    }
    let link_type = endian.u32([rest[16], rest[17], rest[18], rest[19]]);

    let mut summary = Summary::default();
    let mut buffer = Vec::new();

    loop {
        let mut record = [0u8; 16];
        if !fill(reader, &mut record)? {
            break;
        }
        let seconds = endian.u32([record[0], record[1], record[2], record[3]]);
        let ticks = endian.u32([record[4], record[5], record[6], record[7]]);
        let captured = endian.u32([record[8], record[9], record[10], record[11]]);
        let original = endian.u32([record[12], record[13], record[14], record[15]]);

        if captured > MAX_PACKET_BYTES {
            bail!(
                "this capture claims a single packet of {} bytes, which is not a real packet — \
                 the file is damaged",
                captured
            );
        }

        buffer.clear();
        buffer.resize(captured as usize, 0);
        if !fill(reader, &mut buffer)? {
            summary.truncated = true;
            break;
        }

        // Nanoseconds are multiplied up rather than the seconds divided down,
        // so a microsecond capture keeps its full precision.
        let at = Duration::new(
            seconds as u64,
            ticks.saturating_mul(tick_divisor).min(999_999_999),
        );
        let keep_going = on_packet(&Packet {
            at: Some(at),
            link_type,
            data: &buffer,
            original_len: original,
        });

        summary.packets += 1;
        if !keep_going {
            summary.stopped = true;
            break;
        }
        if summary.packets >= MAX_PACKETS {
            summary.capped = true;
            break;
        }
    }
    Ok(summary)
}

/// One interface's worth of the pcapng header blocks, since a packet block says
/// only which interface it came from.
#[derive(Clone, Copy)]
struct Interface {
    link_type: u32,
    /// How many of the timestamp's units make a second, as a power of ten.
    /// Six — microseconds — unless the interface says otherwise.
    ts_resolution: u32,
    /// Set for the rare interface that counts in something other than a power
    /// of ten, whose timestamps are then not worth guessing at.
    ts_unusable: bool,
}

impl Default for Interface {
    fn default() -> Self {
        Self {
            link_type: 1,
            ts_resolution: 6,
            ts_unusable: false,
        }
    }
}

fn read_pcapng(
    reader: &mut impl Read,
    on_packet: &mut impl FnMut(&Packet<'_>) -> bool,
) -> Result<Summary> {
    // The section header block, whose body begins with the magic that says
    // which way round every number in this section is written.
    let mut length_bytes = [0u8; 4];
    if !fill(reader, &mut length_bytes)? {
        bail!("this capture file ends inside its section header");
    }
    let mut order_bytes = [0u8; 4];
    if !fill(reader, &mut order_bytes)? {
        bail!("this capture file ends inside its section header");
    }
    let endian = match order_bytes {
        [0x1A, 0x2B, 0x3C, 0x4D] => Endian::Big,
        [0x4D, 0x3C, 0x2B, 0x1A] => Endian::Little,
        _ => bail!("this capture file's section header does not say which byte order it uses"),
    };

    let mut summary = Summary::default();
    let mut interfaces: Vec<Interface> = Vec::new();
    let mut body = Vec::new();

    // Step over the rest of the section header block, options and all.
    let first_length = endian.u32(length_bytes);
    if let Some(rest) = first_length.checked_sub(12)
        && (rest > MAX_BLOCK_BYTES || !skip(reader, rest as u64)?)
    {
        return Ok(summary);
    }

    loop {
        let mut header = [0u8; 8];
        if !fill(reader, &mut header)? {
            break;
        }
        let block_type = endian.u32([header[0], header[1], header[2], header[3]]);
        let total_length = endian.u32([header[4], header[5], header[6], header[7]]);

        // The trailing copy of the length makes 12 the smallest a block can be,
        // and the format requires a multiple of four.
        if total_length < 12 || total_length % 4 != 0 || total_length > MAX_BLOCK_BYTES {
            bail!(
                "this capture has a block claiming to be {} bytes long, which cannot be right — \
                 the file is damaged",
                total_length
            );
        }
        let body_length = (total_length - 12) as usize;

        body.clear();
        body.resize(body_length, 0);
        if !fill(reader, &mut body)? {
            summary.truncated = true;
            break;
        }
        // The trailing length, which is only there to allow reading backwards.
        let mut trailer = [0u8; 4];
        if !fill(reader, &mut trailer)? {
            summary.truncated = true;
            break;
        }

        match block_type {
            // A new section: its own byte order could differ from this one's,
            // and following that properly means starting again. Stopping here
            // reports everything up to the join rather than nonsense after it.
            0x0A0D_0D0A => break,
            // Interface description.
            0x0000_0001 => interfaces.push(read_interface(&body, endian)),
            // Enhanced packet: the ordinary one, with a timestamp and an
            // interface to interpret it by.
            0x0000_0006 => {
                if body.len() < 20 {
                    summary.truncated = true;
                    break;
                }
                let interface_id = endian.u32([body[0], body[1], body[2], body[3]]) as usize;
                let high = endian.u32([body[4], body[5], body[6], body[7]]) as u64;
                let low = endian.u32([body[8], body[9], body[10], body[11]]) as u64;
                let captured = endian.u32([body[12], body[13], body[14], body[15]]) as usize;
                let original = endian.u32([body[16], body[17], body[18], body[19]]);

                let Some(data) = body.get(20..20 + captured) else {
                    summary.truncated = true;
                    break;
                };
                let interface = interfaces.get(interface_id).copied().unwrap_or_default();
                let keep_going = on_packet(&Packet {
                    at: timestamp(high << 32 | low, &interface),
                    link_type: interface.link_type,
                    data,
                    original_len: original,
                });
                summary.packets += 1;
                if !keep_going {
                    summary.stopped = true;
                    break;
                }
            }
            // Simple packet: no timestamp, and the whole rest of the block is
            // the packet.
            0x0000_0003 => {
                if body.len() < 4 {
                    summary.truncated = true;
                    break;
                }
                let original = endian.u32([body[0], body[1], body[2], body[3]]);
                let interface = interfaces.first().copied().unwrap_or_default();
                let keep_going = on_packet(&Packet {
                    at: None,
                    link_type: interface.link_type,
                    data: &body[4..],
                    original_len: original,
                });
                summary.packets += 1;
                if !keep_going {
                    summary.stopped = true;
                    break;
                }
            }
            // Name resolution, statistics, decryption secrets and the rest:
            // nothing this summary reads, and already stepped over.
            _ => {}
        }

        if summary.packets >= MAX_PACKETS {
            summary.capped = true;
            break;
        }
    }
    Ok(summary)
}

/// Reads an interface description block: its link type, and the `if_tsresol`
/// option if it carries one.
fn read_interface(body: &[u8], endian: Endian) -> Interface {
    let mut interface = Interface::default();
    if body.len() < 8 {
        return interface;
    }
    interface.link_type = endian.u16([body[0], body[1]]) as u32;

    // Options: a 2-byte code, a 2-byte length, then that many bytes padded up
    // to a multiple of four. Code 9 is the timestamp resolution.
    let mut at = 8;
    while at + 4 <= body.len() {
        let code = endian.u16([body[at], body[at + 1]]);
        let length = endian.u16([body[at + 2], body[at + 3]]) as usize;
        let value_at = at + 4;
        if code == 0 {
            break;
        }
        if let Some(value) = body.get(value_at..value_at + length) {
            if code == 9 && !value.is_empty() {
                // The top bit set means the low seven bits are a power of two
                // rather than of ten. Vanishingly rare, and a timestamp read
                // with the wrong scale is worse than one left out.
                if value[0] & 0x80 != 0 {
                    interface.ts_unusable = true;
                } else {
                    interface.ts_resolution = value[0] as u32;
                }
            }
        } else {
            break;
        }
        // Options are padded to four bytes; the padding is not counted in the
        // length.
        at = value_at + length.div_ceil(4) * 4;
    }
    interface
}

/// Turns a pcapng packet's raw tick count into a time since the epoch, using
/// the resolution its interface declared.
fn timestamp(ticks: u64, interface: &Interface) -> Option<Duration> {
    if interface.ts_unusable || interface.ts_resolution > 9 {
        return None;
    }
    let per_second = 10u64.checked_pow(interface.ts_resolution)?;
    let seconds = ticks / per_second;
    let remainder = ticks % per_second;
    // Scaled up to nanoseconds, which is the finest a Duration holds.
    let nanos = remainder.checked_mul(1_000_000_000 / per_second)?;
    Some(Duration::new(seconds, nanos.min(999_999_999) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::pcap::fixture;

    /// One packet as a test holds it: when it was captured, what link layer it
    /// came off, and its bytes.
    type Captured = (Option<Duration>, u32, Vec<u8>);

    /// Reads a capture from a slice of bytes, collecting what came back.
    fn read_all(name: &str, bytes: &[u8]) -> (Summary, Vec<Captured>) {
        let path = fixture::on_disk(name, bytes);
        let mut packets = Vec::new();
        let summary = for_each_packet(&path, &mut |packet| {
            packets.push((packet.at, packet.link_type, packet.data.to_vec()));
            true
        })
        .expect("this fixture should read");
        std::fs::remove_file(&path).ok();
        (summary, packets)
    }

    #[test]
    fn a_classic_capture_gives_back_its_packets_with_their_times() {
        let frames = fixture::handshake([192, 168, 1, 5], [93, 184, 216, 34], 443);
        let (summary, packets) = read_all(
            "soe-pcap-classic.pcap",
            &fixture::classic(super::super::packet::link::ETHERNET, &frames),
        );

        assert_eq!(summary.packets, 3);
        assert!(!summary.truncated);
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].1, super::super::packet::link::ETHERNET);
        assert_eq!(packets[0].2, frames[0]);
        // One second apart, as the fixture wrote them.
        let (first, last) = (packets[0].0.unwrap(), packets[2].0.unwrap());
        assert_eq!(last - first, Duration::from_secs(2));
    }

    /// The same file written the other way round has to read identically —
    /// the magic is the only thing that says which it is.
    #[test]
    fn a_big_endian_capture_reads_the_same_as_a_little_endian_one() {
        let frames = fixture::handshake([10, 0, 0, 1], [10, 0, 0, 2], 22);
        // The same file, written out in the other byte order throughout.
        let big = {
            let mut out = vec![0xA1, 0xB2, 0xC3, 0xD4];
            out.extend_from_slice(&2u16.to_be_bytes());
            out.extend_from_slice(&4u16.to_be_bytes());
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&65_535u32.to_be_bytes());
            out.extend_from_slice(&1u32.to_be_bytes());
            for (index, frame) in frames.iter().enumerate() {
                out.extend_from_slice(&(1_700_000_000u32 + index as u32).to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes());
                out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                out.extend_from_slice(frame);
            }
            out
        };

        let (summary, packets) = read_all("soe-pcap-big.pcap", &big);
        assert_eq!(summary.packets, 3);
        assert_eq!(packets[0].2, frames[0]);
        assert_eq!(packets[0].0, Some(Duration::from_secs(1_700_000_000)));
    }

    /// The nanosecond magics differ from the microsecond ones only in what the
    /// second field of each timestamp counts, and reading one as the other is
    /// out by a factor of a thousand.
    #[test]
    fn the_nanosecond_variant_is_not_read_as_microseconds() {
        let frames = vec![fixture::ethernet(0x0806, &[0; 28])];
        let mut bytes = fixture::classic_with_magic([0x4D, 0x3C, 0xB2, 0xA1], 1, &frames);
        // Put half a second, counted in nanoseconds, into the record.
        let ticks = 500_000_000u32.to_le_bytes();
        let at = 24 + 4;
        bytes[at..at + 4].copy_from_slice(&ticks);

        let (_, packets) = read_all("soe-pcap-nanos.pcap", &bytes);
        assert_eq!(packets[0].0.unwrap().subsec_nanos(), 500_000_000);
    }

    #[test]
    fn a_pcapng_capture_reads_its_interface_and_its_packets() {
        let frames = fixture::handshake([192, 168, 1, 5], [1, 1, 1, 1], 443);
        let (summary, packets) = read_all("soe-pcap-ng.pcapng", &fixture::pcapng(1, &frames));

        assert_eq!(summary.packets, 3);
        assert!(!summary.truncated);
        assert_eq!(packets[0].1, 1);
        // The block pads its body to four bytes, and the padding must not
        // arrive as part of the packet.
        assert_eq!(packets[0].2, frames[0]);
        assert_eq!(packets[0].0, Some(Duration::from_secs(1_700_000_000)));
    }

    /// What every capture stopped with Ctrl-C looks like. Reporting it is the
    /// right answer; failing on it would refuse a great many real files.
    #[test]
    fn a_capture_that_stops_mid_packet_reports_what_it_had() {
        let frames = fixture::handshake([10, 0, 0, 1], [10, 0, 0, 2], 80);
        let mut bytes = fixture::classic(1, &frames);
        bytes.truncate(bytes.len() - 20);

        let (summary, packets) = read_all("soe-pcap-cut.pcap", &bytes);
        assert!(summary.truncated);
        assert_eq!(summary.packets, 2);
        assert_eq!(packets.len(), 2);
    }

    /// The Cancel button's route into a file that may be gigabytes long.
    #[test]
    fn a_callback_that_asks_to_stop_is_obeyed() {
        let frames = fixture::handshake([10, 0, 0, 1], [10, 0, 0, 2], 80);
        let path = fixture::on_disk("soe-pcap-stop.pcap", &fixture::classic(1, &frames));
        let mut seen = 0;
        let summary = for_each_packet(&path, &mut |_| {
            seen += 1;
            false
        })
        .unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(seen, 1);
        assert!(summary.stopped);
    }

    /// A file of attacker-controlled bytes must fail as a message, never as an
    /// allocation the size of whatever number was in it.
    #[test]
    fn a_block_claiming_an_impossible_length_is_refused_rather_than_allocated() {
        let mut bytes = fixture::pcapng(1, &[fixture::ethernet(0x0806, &[0; 28])]);
        // The interface description block's length, made enormous.
        let at = 28 + 4;
        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        let path = fixture::on_disk("soe-pcap-huge.pcapng", &bytes);
        let error = for_each_packet(&path, &mut |_| true)
            .unwrap_err()
            .to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("damaged"), "{error}");
    }

    /// A packet length larger than any real packet is the same attack in the
    /// classic format.
    #[test]
    fn a_packet_claiming_to_be_gigabytes_is_refused() {
        let mut bytes = fixture::classic(1, &[fixture::ethernet(0x0806, &[0; 28])]);
        let at = 24 + 8;
        bytes[at..at + 4].copy_from_slice(&(1024u32 * 1024 * 1024).to_le_bytes());

        let path = fixture::on_disk("soe-pcap-fat.pcap", &bytes);
        let error = for_each_packet(&path, &mut |_| true)
            .unwrap_err()
            .to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("damaged"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_capture_at_all_says_so_by_name() {
        let path = fixture::on_disk("soe-pcap-notacapture.pcap", b"this is just some text");
        let error = for_each_packet(&path, &mut |_| true)
            .unwrap_err()
            .to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("soe-pcap-notacapture.pcap"), "{error}");
        assert!(error.contains("signature"), "{error}");
    }
}

/// Malformed-input tests, kept apart from the ones above because they are
/// about a different question: not "does this read a capture correctly" but
/// "can any sequence of bytes get this to panic".
///
/// A capture file is a recording of what arrived off a network, so every
/// length, count and offset in it was chosen by whoever sent the traffic. The
/// reader is written to treat all of it as hostile; these tests are what says
/// so out loud, since a bounds check is easy to remove by accident and
/// impossible to miss here.
#[cfg(test)]
mod robustness {
    use super::*;
    use crate::extract::pcap::fixture;

    /// A deterministic scrambler. A real fuzzer belongs in CI, not in a unit
    /// test; what this gives is the same few thousand hostile files on every
    /// run and on every machine, which is what makes a failure something a
    /// developer can reproduce rather than a flake.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // xorshift64*, chosen for being four lines rather than for being
            // good — this only has to spread bytes about.
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// Reads `bytes` as a capture and throws the result away. Anything but a
    /// panic is a pass: a malformed file is entitled to be refused, and to be
    /// read as far as it goes and no further.
    fn read_without_panicking(path: &std::path::Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        let mut packets = 0u64;
        let _ = for_each_packet(path, &mut |packet| {
            // The body is walked too, so a length that got past the container
            // cannot slip past the decoder either.
            let _ = crate::extract::pcap::packet::decode(packet.link_type, packet.data);
            packets += 1;
            packets < 10_000
        });
    }

    /// A capture holding one of everything the reader knows how to look at, so
    /// the mutations below have real structure to corrupt.
    fn representative_packets() -> Vec<Vec<u8>> {
        let mut frames = fixture::handshake([192, 168, 1, 5], [93, 184, 216, 34], 443);
        frames.push(fixture::ethernet(
            0x0800,
            &fixture::ipv4(
                17,
                [192, 168, 1, 5],
                [192, 168, 1, 1],
                &fixture::udp(
                    51_000,
                    53,
                    &fixture::dns_query("updates.example.com", false),
                ),
            ),
        ));
        frames.push(fixture::ethernet(0x0806, &[0x11; 28]));
        frames.push(fixture::ethernet(0x86DD, &[0x60; 64]));
        frames
    }

    /// Every prefix of a real capture. A file that stops anywhere — which is
    /// what a capture interrupted at the keyboard is — must never panic.
    #[test]
    fn a_capture_truncated_anywhere_at_all_is_refused_or_read_but_never_fatal() {
        let path = std::env::temp_dir().join("soe-pcap-robust-truncate.pcap");
        for build in [
            fixture::classic(1, &representative_packets()),
            fixture::pcapng(1, &representative_packets()),
        ] {
            for length in 0..build.len() {
                read_without_panicking(&path, &build[..length]);
            }
        }
        std::fs::remove_file(&path).ok();
    }

    /// Every byte of a real capture, set to each of the values most likely to
    /// break something: zero, the maximum, and the two ends of a length field.
    /// The lengths and counts are where a reader gets hurt, and this reaches
    /// all of them without having to know which bytes they are.
    #[test]
    fn no_single_byte_of_a_capture_can_be_changed_into_a_crash() {
        let path = std::env::temp_dir().join("soe-pcap-robust-bytes.pcap");
        for build in [
            fixture::classic(1, &representative_packets()),
            fixture::pcapng(1, &representative_packets()),
        ] {
            for at in 0..build.len() {
                for value in [0x00, 0x01, 0x7F, 0xFF] {
                    let mut corrupt = build.clone();
                    corrupt[at] = value;
                    read_without_panicking(&path, &corrupt);
                }
            }
        }
        std::fs::remove_file(&path).ok();
    }

    /// Wholesale scrambling, for the damage a single byte cannot do: a length
    /// and the field it describes changed together, several headers corrupt at
    /// once, or a run of bytes that was never a capture in the first place.
    #[test]
    fn thoroughly_scrambled_captures_are_refused_rather_than_fatal() {
        let path = std::env::temp_dir().join("soe-pcap-robust-scramble.pcap");
        let mut rng = Rng(0x5EED_1234_ABCD_9876);

        for build in [
            fixture::classic(1, &representative_packets()),
            fixture::pcapng(1, &representative_packets()),
        ] {
            for _ in 0..600 {
                let mut corrupt = build.clone();
                // Between one and sixteen bytes, anywhere but the magic — which
                // has its own test, and which every run would otherwise spend
                // most of its time being rejected by.
                let changes = (rng.next() % 16 + 1) as usize;
                for _ in 0..changes {
                    let at = 4 + (rng.next() as usize % (corrupt.len() - 4));
                    corrupt[at] = rng.next() as u8;
                }
                read_without_panicking(&path, &corrupt);
            }
        }

        // And files that were never captures: random bytes behind each valid
        // signature, which is the shortest path to the parsers themselves.
        for magic in [
            [0xD4u8, 0xC3, 0xB2, 0xA1],
            [0xA1, 0xB2, 0xC3, 0xD4],
            [0x4D, 0x3C, 0xB2, 0xA1],
            [0x0A, 0x0D, 0x0D, 0x0A],
        ] {
            for _ in 0..300 {
                let mut bytes = magic.to_vec();
                let length = rng.next() as usize % 512;
                bytes.extend((0..length).map(|_| rng.next() as u8));
                read_without_panicking(&path, &bytes);
            }
        }
        std::fs::remove_file(&path).ok();
    }
}
