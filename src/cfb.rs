//! Enough of [MS-CFB] to pull one named stream out of a legacy Office file.
//!
//! The Compound File Binary container is a filesystem in a file: a header, a
//! table saying which sector follows which, and a directory naming the streams
//! laid out across them. Every Office format of that generation is one, which
//! is why this sits on its own rather than inside the reader that needed it
//! first — a `.ppt` and a `.doc` differ entirely in what their streams hold
//! and not at all in how those streams are found.
//!
//! Only reading is implemented, and only of whole streams by name. What the
//! bytes then mean is the caller's problem: see [`crate::powerpoint`] for the
//! record tree of a `.ppt` and [`crate::word`] for the piece table of a `.doc`.

use anyhow::{bail, Context, Result};

/// The eight bytes every Compound File Binary container begins with. Checked
/// rather than trusting the extension: a `.docx` renamed to `.doc` on the way
/// out of a mail client is a thing that happens, and so is the reverse.
pub const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Decode the text encoding this generation of Office formats stores strings
/// in. A trailing odd byte is dropped rather than guessed at.
pub fn utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Sector numbers at or above this are markers rather than sectors: end of
/// chain, free, and the two that name the allocation tables themselves.
const FIRST_MARKER: u32 = 0xFFFF_FFFA;
pub const DIRECTORY_ENTRY_BYTES: usize = 128;
/// A ceiling on how many sectors one chain may be, so that a file whose
/// allocation table points in a circle stops rather than spins. Enough for
/// a 64 MB stream of 512-byte sectors, which is the largest file the reader
/// accepts at all. A chain is held to the smaller of this and the number of
/// sectors the file actually has: a two-sector loop would otherwise be
/// followed a quarter of a million times, turning a kilobyte of malformed
/// input into a hundred megabytes of output.
const MAX_CHAIN: usize = 256 * 1024;

pub struct CompoundFile<'a> {
    data: &'a [u8],
    sector_size: usize,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    mini_stream: Vec<u8>,
    mini_cutoff: u32,
    directory: Vec<u8>,
}

impl<'a> CompoundFile<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self> {
        if data.len() < 512 {
            bail!("the file is too short to be a compound file");
        }
        let word = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
        let long =
            |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);

        // Version 3 uses 512-byte sectors and version 4 uses 4096; nothing
        // else has ever been defined, and an arbitrary shift here would be
        // an arbitrary allocation below.
        let sector_shift = word(30);
        if !matches!(sector_shift, 9 | 12) {
            bail!("this compound file uses a sector size no version defines");
        }
        let sector_size = 1usize << sector_shift;

        let mut file = Self {
            data,
            sector_size,
            fat: Vec::new(),
            mini_fat: Vec::new(),
            mini_stream: Vec::new(),
            mini_cutoff: long(56),
            directory: Vec::new(),
        };

        // The DIFAT: 109 entries in the header, then a chain of sectors
        // holding the rest, each ending in a pointer to the next.
        let mut difat: Vec<u32> = (0..109).map(|i| long(76 + i * 4)).collect();
        let mut next = long(68);
        let mut seen = 0usize;
        while next < FIRST_MARKER && seen < MAX_CHAIN {
            let sector = file.sector(next)?;
            let entries = sector_size / 4 - 1;
            difat.extend((0..entries).map(|i| {
                u32::from_le_bytes([
                    sector[i * 4],
                    sector[i * 4 + 1],
                    sector[i * 4 + 2],
                    sector[i * 4 + 3],
                ])
            }));
            next = u32::from_le_bytes([
                sector[sector_size - 4],
                sector[sector_size - 3],
                sector[sector_size - 2],
                sector[sector_size - 1],
            ]);
            seen += 1;
        }

        let fat_sectors = long(44) as usize;
        for &sector_number in difat.iter().take(fat_sectors) {
            if sector_number >= FIRST_MARKER {
                continue;
            }
            let sector = file.sector(sector_number)?;
            file.fat.extend((0..sector_size / 4).map(|i| {
                u32::from_le_bytes([
                    sector[i * 4],
                    sector[i * 4 + 1],
                    sector[i * 4 + 2],
                    sector[i * 4 + 3],
                ])
            }));
        }
        if file.fat.is_empty() {
            bail!("this compound file has no allocation table");
        }

        file.directory = file.chain(long(48), None)?;

        // The mini stream is one ordinary stream, held by the root entry,
        // that every stream shorter than the cutoff is carved out of.
        let root = file
            .entry(0)
            .context("this compound file has no root entry")?;
        file.mini_stream = file.chain(root.start, Some(root.size))?;
        let mut mini_fat = file.chain(long(60), None)?;
        mini_fat.truncate(long(64) as usize * sector_size);
        file.mini_fat = mini_fat
            .as_chunks::<4>()
            .0
            .iter()
            .map(|four| u32::from_le_bytes(*four))
            .collect();

        Ok(file)
    }

    /// One sector's bytes, by number. Sector zero starts immediately after
    /// the 512-byte header, whatever the sector size is.
    fn sector(&self, number: u32) -> Result<&'a [u8]> {
        // Checked rather than plain arithmetic: `number` comes out of the
        // file, and on a 32-bit target a large one overflows — which is a
        // panic in a debug build and a wrong slice in a release one.
        (number as u64 + 1)
            .checked_mul(self.sector_size as u64)
            .and_then(|at| usize::try_from(at).ok())
            .and_then(|at| self.data.get(at..at.checked_add(self.sector_size)?))
            .context("this compound file points past its own end")
    }

    /// Follow a chain through the allocation table, concatenating it.
    fn chain(&self, start: u32, size: Option<u64>) -> Result<Vec<u8>> {
        let limit = MAX_CHAIN.min(self.data.len() / self.sector_size + 1);
        let mut out = Vec::new();
        let mut next = start;
        let mut visited = 0usize;
        while next < FIRST_MARKER {
            if visited >= limit {
                bail!("a chain in this compound file never ends");
            }
            out.extend_from_slice(self.sector(next)?);
            next = *self
                .fat
                .get(next as usize)
                .context("this compound file points outside its allocation table")?;
            visited += 1;
        }
        if let Some(size) = size {
            out.truncate(size as usize);
        }
        Ok(out)
    }

    /// The same, through the mini allocation table, for the short streams
    /// that live inside the mini stream rather than in sectors of their own.
    fn mini_chain(&self, start: u32, size: u64) -> Result<Vec<u8>> {
        let mini_size = 64usize;
        let limit = MAX_CHAIN.min(self.mini_stream.len() / mini_size + 1);
        let mut out = Vec::new();
        let mut next = start;
        let mut visited = 0usize;
        while next < FIRST_MARKER && (out.len() as u64) < size {
            if visited >= limit {
                bail!("a chain in this compound file never ends");
            }
            let at = usize::try_from(next as u64 * mini_size as u64).ok();
            out.extend_from_slice(
                at.and_then(|at| self.mini_stream.get(at..at.checked_add(mini_size)?))
                    .context("this compound file points past its own mini stream")?,
            );
            next = *self
                .mini_fat
                .get(next as usize)
                .context("this compound file points outside its mini allocation table")?;
            visited += 1;
        }
        out.truncate(size as usize);
        Ok(out)
    }

    fn entry(&self, index: usize) -> Option<Entry> {
        let at = index * DIRECTORY_ENTRY_BYTES;
        let raw = self.directory.get(at..at + DIRECTORY_ENTRY_BYTES)?;
        // The name is UTF-16 with its terminating nul counted in the length.
        let name_bytes = u16::from_le_bytes([raw[64], raw[65]]).saturating_sub(2) as usize;
        let units: Vec<u16> = raw
            .get(..name_bytes.min(64))?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        Some(Entry {
            name: String::from_utf16_lossy(&units),
            kind: raw[66],
            start: u32::from_le_bytes([raw[116], raw[117], raw[118], raw[119]]),
            size: u64::from_le_bytes(raw[120..128].try_into().ok()?),
        })
    }

    /// The contents of the named stream, or `None` if the file has no such
    /// stream.
    ///
    /// The two are told apart deliberately: a stream that is there and
    /// cannot be read is a different thing from one that was never there,
    /// and reporting the first as the second sends whoever reads the
    /// message looking for the wrong problem.
    pub fn stream(&self, wanted: &str) -> Result<Option<Vec<u8>>> {
        let count = self.directory.len() / DIRECTORY_ENTRY_BYTES;
        let Some(entry) = (0..count)
            .filter_map(|index| self.entry(index))
            .find(|entry| entry.kind == 2 && entry.name == wanted)
        else {
            return Ok(None);
        };
        if entry.size < self.mini_cutoff as u64 {
            self.mini_chain(entry.start, entry.size).map(Some)
        } else {
            self.chain(entry.start, Some(entry.size)).map(Some)
        }
    }
}

struct Entry {
    name: String,
    /// 1 is a storage, 2 a stream, 5 the root.
    kind: u8,
    start: u32,
    size: u64,
}
