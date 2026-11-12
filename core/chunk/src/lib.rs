// SPDX-License-Identifier: Apache-2.0

//! Content-defined chunking and chunk hashing.
//!
//! A file smaller than [ChunkerConfig::small_file_threshold] is one chunk, and
//! an empty file has none. Larger files are cut with FastCDC 2020 at the
//! configured minimum, average and maximum sizes. A chunk is identified by the
//! SHA-256 of its bytes, and a file's [Content] carries the manifest digest
//! over its chunk list.
//!
//! Every client must cut the same bytes at the same offsets, so the boundary
//! algorithm is frozen as the `fastcdc2020` profile: the v2020 algorithm of
//! fastcdc-rs 5.0.0 with its Gear table (the first eight bytes, big-endian, of
//! MD5 over 64 repetitions of each byte value), hash seed 0 and normalization
//! level 1.

use std::fmt;
use std::io::{self, Chain, Cursor, Read};

use fastcdc::v2020::{Normalization, StreamCDC};
use koidrive_proto::v1::{ChunkRef, Content};
use sha2::{Digest, Sha256};

const MIB: u32 = 1 << 20;
const FAMILY: &str = "fastcdc2020";

/// Chunk boundary parameters. [ChunkerConfig::V0] is the spec's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    min_size: u32,
    avg_size: u32,
    max_size: u32,
    small_file_threshold: u32,
}

impl ChunkerConfig {
    /// FastCDC 2020 with 1 MiB minimum, 4 MiB average and 16 MiB maximum
    /// chunks, for files of 4 MiB or more.
    pub const V0: Self = Self {
        min_size: MIB,
        avg_size: 4 * MIB,
        max_size: 16 * MIB,
        small_file_threshold: 4 * MIB,
    };

    /// Checks the sizes against what FastCDC 2020 supports: even values with
    /// 64 B ≤ min ≤ 1 MiB, 256 B ≤ avg ≤ 4 MiB, 1 KiB ≤ max ≤ 16 MiB and
    /// min ≤ avg ≤ max. A file below the threshold becomes a single chunk, so the
    /// threshold may not exceed the maximum chunk size.
    pub fn new(
        min_size: u32,
        avg_size: u32,
        max_size: u32,
        small_file_threshold: u32,
    ) -> Result<Self, InvalidConfig> {
        use fastcdc::v2020::{
            AVERAGE_MAX, AVERAGE_MIN, MAXIMUM_MAX, MAXIMUM_MIN, MINIMUM_MAX, MINIMUM_MIN,
        };
        let in_range = |v: u32, lo: usize, hi: usize| (lo..=hi).contains(&(v as usize));
        if !in_range(min_size, MINIMUM_MIN, MINIMUM_MAX)
            || !in_range(avg_size, AVERAGE_MIN, AVERAGE_MAX)
            || !in_range(max_size, MAXIMUM_MIN, MAXIMUM_MAX)
        {
            return Err(InvalidConfig(
                "chunk size outside the range FastCDC 2020 supports",
            ));
        }
        if [min_size, avg_size, max_size]
            .iter()
            .any(|v| !v.is_multiple_of(2))
        {
            return Err(InvalidConfig("chunk sizes must be even"));
        }
        if !(min_size <= avg_size && avg_size <= max_size) {
            return Err(InvalidConfig("chunk sizes must satisfy min <= avg <= max"));
        }
        if small_file_threshold > max_size {
            return Err(InvalidConfig(
                "small-file threshold exceeds the maximum chunk size",
            ));
        }
        Ok(Self {
            min_size,
            avg_size,
            max_size,
            small_file_threshold,
        })
    }

    pub fn min_size(&self) -> u32 {
        self.min_size
    }

    pub fn avg_size(&self) -> u32 {
        self.avg_size
    }

    pub fn max_size(&self) -> u32 {
        self.max_size
    }

    pub fn small_file_threshold(&self) -> u32 {
        self.small_file_threshold
    }

    /// The identifier recorded in [Content::chunker]. It names every parameter
    /// that moves a boundary: `fastcdc2020-1m-4m-16m` is reserved for
    /// [ChunkerConfig::V0], and every other configuration also names its
    /// small-file threshold, as in `fastcdc2020-64-256-1k-t512`.
    pub fn name(&self) -> String {
        let size = |v: u32| {
            if v > 0 && v.is_multiple_of(MIB) {
                format!("{}m", v / MIB)
            } else if v > 0 && v.is_multiple_of(1024) {
                format!("{}k", v / 1024)
            } else {
                v.to_string()
            }
        };
        let sizes = format!(
            "{FAMILY}-{}-{}-{}",
            size(self.min_size),
            size(self.avg_size),
            size(self.max_size)
        );
        if *self == Self::V0 {
            sizes
        } else {
            format!("{sizes}-t{}", size(self.small_file_threshold))
        }
    }

    /// The configuration an identifier names. Only the exact spelling
    /// [ChunkerConfig::name] produces is accepted, so an identifier that could
    /// describe another profile (another family, the V0 sizes without the V0
    /// threshold, a size written another way) is rejected.
    pub fn from_name(name: &str) -> Result<Self, InvalidConfig> {
        let unknown = InvalidConfig("unknown chunker identifier");
        let size = |part: &str| -> Option<u32> {
            let (digits, unit) = match part.strip_suffix('m') {
                Some(digits) => (digits, MIB),
                None => match part.strip_suffix('k') {
                    Some(digits) => (digits, 1024),
                    None => (part, 1),
                },
            };
            digits.parse::<u32>().ok()?.checked_mul(unit)
        };
        let config = match name.split('-').collect::<Vec<_>>().as_slice() {
            [FAMILY, _, _, _] => Self::V0,
            [FAMILY, min, avg, max, threshold] => {
                let sizes = threshold
                    .strip_prefix('t')
                    .and_then(|t| Some((size(min)?, size(avg)?, size(max)?, size(t)?)));
                let Some((min, avg, max, threshold)) = sizes else {
                    return Err(unknown);
                };
                Self::new(min, avg, max, threshold)?
            }
            _ => return Err(unknown),
        };
        if config.name() == name {
            Ok(config)
        } else {
            Err(unknown)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidConfig(&'static str);

impl fmt::Display for InvalidConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidConfig {}

/// One chunk of a file: its position, its bytes and their SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub data: Vec<u8>,
    pub hash: [u8; 32],
}

impl Chunk {
    fn new(offset: u64, data: Vec<u8>) -> Self {
        let hash = chunk_hash(&data);
        Self { offset, data, hash }
    }

    pub fn chunk_ref(&self) -> ChunkRef {
        ChunkRef {
            hash: self.hash.to_vec(),
            size: self.data.len() as u32,
            encoding: 0,
        }
    }
}

/// The chunk hash: SHA-256 of the chunk bytes.
pub fn chunk_hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// SHA-256 over the concatenation of (32-byte hash ‖ u32 little-endian size)
/// for every chunk, in order (spec v0 §5.1).
pub fn manifest_digest(chunks: &[ChunkRef]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for chunk in chunks {
        hasher.update(&chunk.hash);
        hasher.update(chunk.size.to_le_bytes());
    }
    hasher.finalize().into()
}

/// Builds the [Content] of a file from its chunk list.
pub fn content(
    chunks: Vec<ChunkRef>,
    mtime_ns: i64,
    executable: bool,
    config: &ChunkerConfig,
) -> Content {
    Content {
        size: chunks.iter().map(|c| u64::from(c.size)).sum(),
        mtime_ns,
        executable,
        chunk_count: chunks.len() as u32,
        manifest_digest: manifest_digest(&chunks).to_vec(),
        chunker: config.name(),
        chunks,
    }
}

/// Splits everything `reader` yields into chunks.
///
/// Memory is bounded by the configuration, not the file: the retained prefix up
/// to the small-file threshold, a maximum-size read buffer and the chunk being
/// returned, about 36 MiB for [ChunkerConfig::V0], plus what the caller keeps.
///
/// Reads interrupted with [io::ErrorKind::Interrupted] are retried. Any other
/// read error is returned once and ends the iteration; chunk again from the
/// start. The reader is trusted to yield one version of the file: a file that
/// is truncated or rewritten while it is read gives chunks of whatever bytes
/// were read, so the caller must check that the file did not change before it
/// publishes the result.
pub fn chunks<R: Read>(reader: R, config: ChunkerConfig) -> Chunks<R> {
    Chunks {
        state: State::Start(reader, config),
    }
}

pub struct Chunks<R: Read> {
    state: State<R>,
}

type Source<R> = Chain<Cursor<Vec<u8>>, Uninterrupted<R>>;

enum State<R: Read> {
    Start(R, ChunkerConfig),
    Cdc(Box<StreamCDC<Source<R>>>),
    Done,
}

struct Uninterrupted<R>(R);

impl<R: Read> Read for Uninterrupted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl<R: Read> Iterator for Chunks<R> {
    type Item = io::Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Done => None,
            State::Start(reader, config) => {
                let mut reader = Uninterrupted(reader);
                let threshold = config.small_file_threshold as usize;
                let mut head = Vec::with_capacity(threshold);
                if let Err(err) = (&mut reader).take(threshold as u64).read_to_end(&mut head) {
                    return Some(Err(err));
                }
                if head.len() < threshold {
                    return (!head.is_empty()).then(|| Ok(Chunk::new(0, head)));
                }
                self.state = State::Cdc(Box::new(StreamCDC::with_level_and_seed(
                    Cursor::new(head).chain(reader),
                    config.min_size as usize,
                    config.avg_size as usize,
                    config.max_size as usize,
                    Normalization::Level1,
                    0,
                )));
                self.next()
            }
            State::Cdc(mut cdc) => match cdc.next()? {
                Ok(c) => {
                    self.state = State::Cdc(cdc);
                    Some(Ok(Chunk::new(c.offset, c.data)))
                }
                Err(err) => Some(Err(io::Error::from(err))),
            },
        }
    }
}
