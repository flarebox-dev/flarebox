// SPDX-License-Identifier: Apache-2.0

use koidrive_chunk::{ChunkerConfig, chunks};

enum Input {
    Noise(usize, u64),
    Zeros(usize),
}

struct Golden {
    input: Input,
    chunks: &'static [(u64, usize, &'static str)],
}

const GOLDEN: &[Golden] = &[
    Golden {
        input: Input::Noise(4194303, 2),
        chunks: &[(
            0,
            4194303,
            "3c83299f8887c572e52333693db926e5b179d410d3bb36d43962d1ad4a9240fd",
        )],
    },
    Golden {
        input: Input::Noise(4194304, 2),
        chunks: &[
            (
                0,
                3116686,
                "91a13fa607d55aec32fad976a9423e1b0c15dc8bb67a082b67a9d2b6b5af50ea",
            ),
            (
                3116686,
                1077618,
                "920fea36f41712e0b192dd57540340819171bbc51cb93e718a682754f97fddae",
            ),
        ],
    },
    Golden {
        input: Input::Noise(4194305, 2),
        chunks: &[
            (
                0,
                3116686,
                "91a13fa607d55aec32fad976a9423e1b0c15dc8bb67a082b67a9d2b6b5af50ea",
            ),
            (
                3116686,
                1077619,
                "cc0088aca9b6ec1c680ed9dc6aeb1a7bfddcf5179ea07c78037b1712500150e8",
            ),
        ],
    },
    Golden {
        input: Input::Noise(16777216, 5),
        chunks: &[
            (
                0,
                4499455,
                "52b8ac6990b632cae4645a36c64110c25da6a2dd2c570de915c7b269bf8fe50a",
            ),
            (
                4499455,
                6778693,
                "98f167df4fb00ea6b26f591cfb88385a683841c1baa49206dc625dea8ccb0efd",
            ),
            (
                11278148,
                5499068,
                "335a6dc9ad1152bb9be5b1604cd579a3eed24e1968ef6bef561bdcff8b427c78",
            ),
        ],
    },
    Golden {
        input: Input::Noise(16777217, 5),
        chunks: &[
            (
                0,
                4499455,
                "52b8ac6990b632cae4645a36c64110c25da6a2dd2c570de915c7b269bf8fe50a",
            ),
            (
                4499455,
                6778693,
                "98f167df4fb00ea6b26f591cfb88385a683841c1baa49206dc625dea8ccb0efd",
            ),
            (
                11278148,
                5499069,
                "278df24fe28b5d20ee708928f56e88818d5bfe2948610d246d29e74fe3b3cd9c",
            ),
        ],
    },
    Golden {
        input: Input::Noise(41955385, 11),
        chunks: &[
            (
                0,
                8111755,
                "e65d4341d19d5b6d50988232fb210cd387fb8c6d5058631f457e543dab820a12",
            ),
            (
                8111755,
                4195443,
                "fb3279e0fd6f23cdd79db9fd8931724add21a3a2bc7a1f9166beab3a4b4f58c3",
            ),
            (
                12307198,
                3964895,
                "faeecd141b4a336e44986735d2b4e3e1384487ae280b73b23c7fb29554a12d7e",
            ),
            (
                16272093,
                5670021,
                "afe319065979e495a5b27efb9211c6f665a864e0c091ddd3f02226517c3b3400",
            ),
            (
                21942114,
                2353266,
                "19a1ae46f71dabe9e3320b69cbcc260212f82baf795779ce0c65ca9108c941a3",
            ),
            (
                24295380,
                6120132,
                "835de8434ecdcf149faca568eb7aa81655a4ea2aff823d8e2497a26ebf674127",
            ),
            (
                30415512,
                4841272,
                "edb7eea2d5441e96c9afbb608de6322c85af1964d24fa413f046ced03b7fefa9",
            ),
            (
                35256784,
                3581230,
                "37d239637f767337a46fe004d719f947b17e17659670ca2dc839c78d060b42c4",
            ),
            (
                38838014,
                3042401,
                "46c895c55ac8410642ec846025cd77f3cbd87076b0d280438a91bdc7a62f9f8d",
            ),
            (
                41880415,
                74970,
                "1a8e7409b23c8f7018c9cc2e77978c28e1e00fd1df937bcdbd2298c361172ffa",
            ),
        ],
    },
    Golden {
        input: Input::Zeros(34603009),
        chunks: &[
            (
                0,
                16777216,
                "080acf35a507ac9849cfcba47dc2ad83e01b75663a516279c8b9d243b719643e",
            ),
            (
                16777216,
                16777216,
                "080acf35a507ac9849cfcba47dc2ad83e01b75663a516279c8b9d243b719643e",
            ),
            (
                33554432,
                1048577,
                "2cb74edba754a81d121c9db6833704a8e7d417e5b13d1a19f4a52f007d644264",
            ),
        ],
    },
];

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

#[test]
fn v0_matches_the_reference_vectors() {
    for golden in GOLDEN {
        let data = match golden.input {
            Input::Noise(len, seed) => noise(len, seed),
            Input::Zeros(len) => vec![0; len],
        };
        let got: Vec<(u64, usize, String)> = chunks(data.as_slice(), ChunkerConfig::V0)
            .map(|chunk| {
                let chunk = chunk.unwrap();
                let hex = chunk.hash.iter().map(|b| format!("{b:02x}")).collect();
                (chunk.offset, chunk.data.len(), hex)
            })
            .collect();
        let want: Vec<(u64, usize, String)> = golden
            .chunks
            .iter()
            .map(|&(offset, len, hash)| (offset, len, hash.to_string()))
            .collect();
        assert_eq!(got, want, "input of {} bytes", data.len());
    }
}
