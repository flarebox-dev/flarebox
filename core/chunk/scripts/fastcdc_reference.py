#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Independent reference for the V0 chunk boundaries, used to write tests/golden.rs.

This is a from-scratch implementation of FastCDC 2020 as fastcdc-rs 5.0.0 defines it: the Gear table
is the first eight bytes (big-endian) of MD5 over 64 repetitions of each byte value, with seed 0;
normalization level 1 picks the masks for log2(avg) + 1 and log2(avg) - 1; the hash is a one-byte gear
roll checked from the minimum size on, and a byte at an odd final position is never a cut point.
It shares no code with the Rust crate. Run: python3 core/chunk/scripts/fastcdc_reference.py > golden.txt,
then paste the output into tests/golden.rs.
"""

import hashlib
import sys

MIB = 1 << 20
M64 = (1 << 64) - 1
GEAR = [int.from_bytes(hashlib.md5(bytes([b]) * 64).digest()[:8], "big") for b in range(256)]
# Normalization masks by log2 of the target size (1 MiB to 16 MiB), from the FastCDC 2020 mask table.
MASKS = {
    20: 0x0000D91747537000,
    21: 0x0000D91767537000,
    22: 0x0000D93767537000,
    23: 0x0000D93777537000,
    24: 0x0000D93777577000,
}


def cut(data, start, min_size, avg_size, max_size):
    remaining = len(data) - start
    if remaining <= min_size:
        return remaining
    center = avg_size
    if remaining > max_size:
        remaining = max_size
    elif remaining < center:
        center = remaining
    assert avg_size & (avg_size - 1) == 0, "the reference supports power-of-two averages only"
    bits = avg_size.bit_length() - 1
    mask_s, mask_l = MASKS[bits + 1], MASKS[bits - 1]
    strict_end = center // 2 * 2
    end = remaining // 2 * 2
    h = 0
    i = min_size
    while i < end:
        h = ((h << 1) + GEAR[data[start + i]]) & M64
        if h & (mask_s if i < strict_end else mask_l) == 0:
            return i
        i += 1
    return remaining


def split(data, min_size=MIB, avg_size=4 * MIB, max_size=16 * MIB, threshold=4 * MIB):
    if len(data) < threshold:
        return [len(data)] if data else []
    lengths, start = [], 0
    while start < len(data):
        n = cut(data, start, min_size, avg_size, max_size)
        lengths.append(n)
        start += n
    return lengths


def noise(length, seed):
    """xorshift64*, the generator of tests/chunking.rs."""
    out = bytearray()
    while len(out) < length:
        seed ^= seed >> 12
        seed ^= (seed << 25) & M64
        seed ^= seed >> 27
        out += ((seed * 0x2545F4914F6CDD1D) & M64).to_bytes(8, "little")
    return bytes(out[:length])


CASES = [
    ("noise", 4 * MIB - 1, 2),
    ("noise", 4 * MIB, 2),
    ("noise", 4 * MIB + 1, 2),
    ("noise", 16 * MIB, 5),
    ("noise", 16 * MIB + 1, 5),
    ("noise", 40 * MIB + 12345, 11),
    ("zeros", 33 * MIB + 1, None),
]


def main():
    assert GEAR[0] == 0x3B5D3C7D207E37DC
    print("const GOLDEN: &[Golden] = &[")
    for kind, length, seed in CASES:
        data = noise(length, seed) if kind == "noise" else bytes(length)
        lengths = split(data)
        offset = 0
        source = f"Noise({length}, {seed})" if kind == "noise" else f"Zeros({length})"
        print(f"    Golden {{ input: Input::{source}, chunks: &[")
        for n in lengths:
            digest = hashlib.sha256(data[offset:offset + n]).hexdigest()
            print(f'        ({offset}, {n}, "{digest}"),')
            offset += n
        print("    ] },")
        sys.stdout.flush()
    print("];")


if __name__ == "__main__":
    main()
