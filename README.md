# hash-loop

Search for cycles in the repeated SHA-1 mapping. By default, each state keeps the first 32 SHA-1 bits and zeroes the rest, making the cycle search practical. Use `--bits 160` to search the full SHA-1 state space; a generic full-state cycle is expected to require about $2^{80}$ hash evaluations.

Known SHA-1 collision attacks do not directly produce an iteration cycle. They find two different messages with the same digest, while a cycle requires a state to eventually return to itself. The reduced state search below uses the same birthday-bound idea while preserving a deterministic SHA-1 transition.

## Usage

```
hash-loop 0.1.0
Search for cycles in the repeated SHA-1 mapping

USAGE:
    hash-loop.exe [FLAGS] [OPTIONS] [max]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information
    -v               Switch on verbosity

OPTIONS:
    --bits <bits>      Number of leading SHA-1 bits retained in each state [default: 32]
    --trials <trials>  Number of independent seeds to test [default: 1]

ARGS:
    <max>    Maximum search length, positive integer [default: 4294967296]
```

Run the practical search in release mode:

```
cargo run --release -- --bits 32
```

Use `--trials` to keep several independent walks and report the shortest cycle found:

```
cargo run --release -- --bits 32 --trials 100
```

## Witness Log

Each witness below is the padded 160-bit state printed by a release run. The significant prefix contains `n` bits; the remaining bits are zero. Multiple entries at one width are independent successful runs.

| Prefix bits | Cycle witness | Cycle length |
| ---: | --- | ---: |
| 8 | `2D00000000000000000000000000000000000000` | 17 |
| 8 | `5800000000000000000000000000000000000000` | 17 |
| 12 | `26B0000000000000000000000000000000000000` | 18 |
| 12 | `B9A0000000000000000000000000000000000000` | 52 |
| 16 | `5004000000000000000000000000000000000000` | 146 |
| 16 | `E843000000000000000000000000000000000000` | 95 |
| 16 | `1248000000000000000000000000000000000000` | 95 |
| 20 | `80A1000000000000000000000000000000000000` | 180 |
| 24 | `505AD40000000000000000000000000000000000` | 363 |
| 24 | `69CD370000000000000000000000000000000000` | 5,885 |
| 24 | `AFE4690000000000000000000000000000000000` | 102 |
| 28 | `4DC1EC2000000000000000000000000000000000` | 6,644 |
| 32 | `C2CEBC7700000000000000000000000000000000` | 3,251 |
| 32 | `9B2FC22A00000000000000000000000000000000` | 23,038 |
| 36 | `D2D09D67E0000000000000000000000000000000` | 25,246 |
| 40 | `09B61DAFB9000000000000000000000000000000` | 129,661 |
| 44 | `A407994AAAF00000000000000000000000000000` | 493,661 |
| 48 | `C9529AD035A10000000000000000000000000000` | 9,288,468 |
| 52 | `FFCE3AB848F43000000000000000000000000000` | 11,941,080 |
| 56 | `4BB597D6D356A900000000000000000000000000` | 134,127,825 |
| 60 | `8A0273C0DEDDF0F0000000000000000000000000` | 28,438,441 |
| 64 | `BE5E52F3CFD5F6E4000000000000000000000000` | 149,463,600 |

A 68-bit release probe was stopped after about three minutes without finding a witness. The 64-bit result is therefore the highest-width successful run currently recorded here.
