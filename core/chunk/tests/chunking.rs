// SPDX-License-Identifier: Apache-2.0

use std::io::{self, Read};

use flarebox_chunk::{Chunk, ChunkerConfig, chunk_hash, chunks, content, manifest_digest};
use flarebox_proto::v1::ChunkRef;
use proptest::prelude::*;
use sha2::{Digest, Sha256};

fn small_config() -> ChunkerConfig {
    ChunkerConfig::new(64, 256, 1024, 512).unwrap()
}

fn split(data: &[u8], config: ChunkerConfig) -> Vec<Chunk> {
    chunks(data, config).collect::<io::Result<_>>().unwrap()
}

/// Deterministic pseudo-random bytes (xorshift64*), so tests need no RNG crate.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.extend_from_slice(&seed.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Yields at most a few bytes per read call, like a slow pipe.
struct Trickle<'a>(&'a [u8], usize);

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.1 = self.1 % 7 + 1;
        let n = self.1.min(buf.len()).min(self.0.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

fn check_partition(data: &[u8], parts: &[Chunk], config: ChunkerConfig) {
    let mut offset = 0u64;
    for (i, chunk) in parts.iter().enumerate() {
        assert_eq!(
            chunk.offset, offset,
            "chunk {i} starts where the previous one ended"
        );
        assert_eq!(chunk.hash, chunk_hash(&chunk.data));
        assert!(
            chunk.data.len() <= config.max_size() as usize,
            "chunk {i} exceeds the maximum"
        );
        if data.len() >= config.small_file_threshold() as usize && i + 1 < parts.len() {
            assert!(
                chunk.data.len() >= config.min_size() as usize,
                "chunk {i} is below the minimum"
            );
        }
        offset += chunk.data.len() as u64;
    }
    let joined: Vec<u8> = parts.iter().flat_map(|c| c.data.iter().copied()).collect();
    assert_eq!(joined, data);
}

#[test]
fn v0_matches_the_spec() {
    let v0 = ChunkerConfig::V0;
    assert_eq!(
        (
            v0.min_size(),
            v0.avg_size(),
            v0.max_size(),
            v0.small_file_threshold()
        ),
        (1 << 20, 4 << 20, 16 << 20, 4 << 20)
    );
    assert_eq!(v0.name(), "fastcdc2020-1m-4m-16m");
    assert_eq!(small_config().name(), "fastcdc2020-64-256-1k");
}

#[test]
fn rejects_sizes_fastcdc_cannot_use() {
    assert!(ChunkerConfig::new(32, 256, 1024, 0).is_err());
    assert!(ChunkerConfig::new(64, 256, 1024, 2048).is_err());
    assert!(ChunkerConfig::new(512, 256, 1024, 0).is_err());
    assert!(ChunkerConfig::new(65, 256, 1024, 0).is_err());
    assert!(ChunkerConfig::new(64, 256, 32 << 20, 0).is_err());
}

#[test]
fn empty_file_has_no_chunks() {
    assert!(split(&[], ChunkerConfig::V0).is_empty());
    let c = content(vec![], 0, false, &ChunkerConfig::V0);
    assert_eq!((c.size, c.chunk_count), (0, 0));
    assert_eq!(c.manifest_digest, Sha256::digest([]).to_vec());
}

#[test]
fn file_below_threshold_is_one_chunk() {
    let threshold = ChunkerConfig::V0.small_file_threshold() as usize;
    for len in [1, 1000, threshold - 1] {
        let data = noise(len, 7);
        let parts = split(&data, ChunkerConfig::V0);
        assert_eq!(parts.len(), 1, "{len} bytes");
        assert_eq!(parts[0].data, data);
    }
}

#[test]
fn v0_cuts_large_files_within_bounds() {
    let data = noise(40 << 20, 11);
    let parts = split(&data, ChunkerConfig::V0);
    assert!(parts.len() >= 3, "got {} chunks", parts.len());
    check_partition(&data, &parts, ChunkerConfig::V0);
}

#[test]
fn chunk_hash_is_sha256() {
    assert_eq!(
        chunk_hash(b"abc").to_vec(),
        hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn manifest_digest_hashes_hash_and_le_size() {
    let refs = vec![
        ChunkRef {
            hash: vec![1; 32],
            size: 5,
            encoding: 0,
        },
        ChunkRef {
            hash: vec![2; 32],
            size: 0x0102_0304,
            encoding: 0,
        },
    ];
    let mut bytes = vec![1u8; 32];
    bytes.extend_from_slice(&[5, 0, 0, 0]);
    bytes.extend_from_slice(&[2u8; 32]);
    bytes.extend_from_slice(&[4, 3, 2, 1]);
    assert_eq!(
        manifest_digest(&refs).to_vec(),
        Sha256::digest(&bytes).to_vec()
    );

    let c = content(refs, 42, true, &small_config());
    assert_eq!(
        (c.size, c.chunk_count, c.mtime_ns, c.executable),
        (5 + 0x0102_0304, 2, 42, true)
    );
    assert_eq!(c.chunker, "fastcdc2020-64-256-1k");
}

#[test]
fn an_insertion_changes_only_nearby_chunks() {
    let config = small_config();
    let original = noise(64 << 10, 3);
    let mut edited = original.clone();
    edited.splice(20_000..20_000, *b"inserted");
    let before: Vec<_> = split(&original, config)
        .into_iter()
        .map(|c| c.hash)
        .collect();
    let after: Vec<_> = split(&edited, config).into_iter().map(|c| c.hash).collect();
    let shared = after.iter().filter(|h| before.contains(h)).count();
    assert!(
        shared + 4 >= before.len(),
        "only {shared} of {} chunks survived an 8-byte insertion",
        before.len()
    );
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

proptest! {
    #[test]
    fn chunks_partition_the_input(data in proptest::collection::vec(any::<u8>(), 0..6000)) {
        let config = small_config();
        let parts = split(&data, config);
        check_partition(&data, &parts, config);
        if data.len() < config.small_file_threshold() as usize {
            prop_assert_eq!(parts.len(), usize::from(!data.is_empty()));
        }
    }

    #[test]
    fn boundaries_do_not_depend_on_read_sizes(len in 0usize..6000, seed in any::<u64>()) {
        let data = noise(len, seed | 1);
        let whole = split(&data, small_config());
        let trickled: Vec<Chunk> = chunks(Trickle(&data, 0), small_config())
            .collect::<io::Result<_>>()
            .unwrap();
        prop_assert_eq!(whole, trickled);
    }
}
