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
    --max-cycle-length <n>  Ignore cycles whose length is not less than this value
    --bits <bits>      Number of leading SHA-1 bits retained in each state [default: 32]
    --trials <trials>  Number of independent seeds to test [default: 1]
    --gpu-restarts <n>  Number of fresh random seed batches to test on the GPU [default: 4]
    --exhaustive        Enumerate the entire truncated state space for the exact global minimum cycle (bits <= 31)
    --timeout-secs <n>  Stop exhaustive search after n seconds and report the best cycle found so far
    --gpu               Run sampled trials on the CUDA GPU
    --gpu-batch-size <n>  Number of sampled trials submitted in one GPU dispatch [default: 65536]
    --gpu-benchmark     Run a fixed-transition GPU throughput benchmark instead of cycle search
    --gpu-benchmark-hashes <n>  SHA-1 transitions per benchmark trial [default: 256]
    --gpu-block-size <n>  CUDA threads per block [default: 256]
    --gpu-steps-per-dispatch <n>  SHA-1 transitions per GPU kernel dispatch and trial [default: 65536]

ARGS:
    <max>    Maximum search length, positive integer [default: 34359738368]
```

## Performance

Hash throughput is measured with a dedicated benchmark test (normally `#[ignore]`d): `cargo test --release -- --ignored --nocapture bench_hash_throughput`.

| Configuration | Single-thread | Aggregate (20 cores) |
| --- | ---: | ---: |
| Baseline release build | 24.61M hashes/sec | 362.10M hashes/sec |
| + LTO, `codegen-units = 1`, `panic = "abort"` (`[profile.release]`) | 29.85M hashes/sec | 519.49M hashes/sec |
| + `-C target-cpu=native` (via `.cargo/config.toml`) | 32.37M hashes/sec | 830.47M hashes/sec |

The `sha1` crate (0.11) already dispatches to hardware-accelerated SHA-NI instructions at runtime via `cpufeatures` CPU detection, with a software fallback, so no code changes were needed to use available hardware acceleration — it was already active. The remaining gains came purely from compiler configuration: link-time optimization, a single codegen unit (more cross-function inlining), disabling unwind tables, and targeting the exact build machine's instruction set. Together these more than doubled aggregate throughput (+129%) with no algorithmic changes.

This does not change what's fundamentally reachable: expected work still scales as $2^{\text{bits}/2}$, so a ~30% single-thread speedup only extends the practical frontier by about $\log_2(1.3) \approx 0.4$ bits. It meaningfully speeds up searches in the 68-80 bit range (a given time budget goes further), but 140+ bit searches remain many orders of magnitude out of reach, and known SHA-1 collision/differential attacks (Wang et al., SHAttered, chosen-prefix) do not help either — they exploit control over two independently chosen colliding messages, which has no counterpart in this single-argument iterated-map search.

`.cargo/config.toml` sets `target-cpu=native`, so builds are tuned for this machine's CPU and may not be portable to a different machine without adjusting or removing that file.

### CUDA GPU performance

The CUDA backend runs one independent trial per GPU thread and submits trials in batches. The state transitions within an individual trial remain serial because each SHA-1 input depends on the previous output. Cycle-search state is retained between bounded kernel dispatches, so a trial can reach long cycle lengths without requiring one watchdog-prone kernel. `--gpu-steps-per-dispatch` controls that chunk size; 65,536 is a practical default for Windows WDDM. GPU sampled searches use four fresh random seed batches by default; increase or reduce this with `--gpu-restarts`. The cycle-length cutoff adapts downward whenever a better witness is found, while the positional discovery maximum remains unchanged so long-tail seeds are still searched. Each improving witness is printed and flushed immediately after its dispatch is read back, so witnesses found before a timeout or manual stop remain visible; the final line still reports the best result from the complete run. CUDA 13.3 and an NVIDIA RTX 3070 (8 GiB, compute capability 8.6) were used for the measurements below.

Run the fixed-transition benchmark on Windows after adding the CUDA 13.3 DLL directories to `PATH`:

```
$env:Path = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3\bin;C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3\bin\x64;$env:Path"
.\target\x86_64-pc-windows-msvc\release\hash-loop.exe --gpu-benchmark --bits 160 --trials 1048576 --gpu-batch-size 65536 --gpu-benchmark-hashes 256 --gpu-block-size 256
```

Each run processed 1,048,576 independent trials, with 256 full 160-bit SHA-1 transitions per trial, for 268,435,456 total transitions. Averaged over 3 runs per block size at the default `--gpu-batch-size 65536`:

| CUDA block size | Elapsed | Throughput |
| ---: | ---: | ---: |
| 128 | 0.036 s | 7,482.75M hashes/sec |
| 256 | 0.036 s | 7,547.93M hashes/sec |
| 512 | 0.036 s | **7,567.49M hashes/sec** |

Block size alone makes little difference (within run-to-run noise) as long as `--gpu-batch-size` stays at its default of 65,536. The bigger lever turned out to be batch size itself: at 65,536 trials per dispatch, a 1,048,576-trial run needs 16 separate kernel launches, and launch/sync overhead dominates the timed interval. Raising `--gpu-batch-size` to match (or approach) the total trial count collapses that to one dispatch:

| `--gpu-batch-size` | Block size | Throughput |
| ---: | ---: | ---: |
| 65,536 (default) | 512 | 7,567.49M hashes/sec |
| 1,048,576 (= trial count) | 512 | **8,524.05M hashes/sec** |

That is a genuine +13% improvement at the same trial count and hardware, not measurement noise — confirmed at larger scale too: 16,777,216 trials in one batch sustained 8,519.91M hashes/sec, and 33,554,432 / 67,108,864 trials sustained 8,520.68M / 8,517.10M hashes/sec, so ~8,500-8,520M hashes/sec is the throughput ceiling on this GPU/kernel, reached once per-dispatch overhead is amortized over a large enough batch. **Recommended settings for this hardware: `--gpu-block-size 512` with `--gpu-batch-size` set as large as the workload's memory budget allows** (ideally equal to the trial count for one-shot benchmarks; for long cycle searches, `--gpu-steps-per-dispatch` still bounds per-dispatch kernel *duration* for WDDM/TDR safety — raising `--gpu-batch-size` only adds more independent trials per dispatch, it does not lengthen any single trial's kernel time):

```
.\target\x86_64-pc-windows-msvc\release\hash-loop.exe --gpu-benchmark --bits 160 --trials 1048576 --gpu-batch-size 1048576 --gpu-benchmark-hashes 256 --gpu-block-size 512
```

The benchmark timing covers GPU kernel launch and synchronization for each batch. Seed upload, result download, allocation, and one-time NVRTC compilation are outside the timed interval. The best tuned GPU result (~8,520M hashes/sec) is about 10.3x the previously recorded 830.47M hashes/sec aggregate CPU result, but that comparison is directional: the CPU table measures a 32-bit Rayon workload, while this GPU benchmark measures full 160-bit chains.

This throughput does not make full 160-bit cycle discovery practical. At 8.52 billion transitions per second, the expected $2^{80}$ work is still roughly 4.5 million years, before accounting for cycle-detection overhead. The GPU improves the number of independent trials that can be explored; it cannot parallelize the dependent transitions within one trial.

### CUDA kernel-level optimization

`--gpu-benchmark` only exercises the `benchmark_hashes` kernel, which reads a seed once, loops entirely in registers, and writes one output word — `ptxas -v` confirms 0 bytes of register spill and disassembly shows the SHA-1 round function and rotate already collapse to single `LOP3.LUT` / funnel-shift (`SHF`) instructions per op, so there was no headroom left to hand-optimize there. The actual cycle-search kernel, `find_cycles_chunk`, is different: every one of its (up to `--gpu-steps-per-dispatch`) loop iterations was reading and writing the 17-word per-trial state directly through a global-memory pointer, since the compiler cannot assume the `seeds`/`states`/`results` kernel arguments don't alias each other. Disassembling the original kernel showed 60 `LDG` + 128 `STG` instructions versus only 5 `LDG` + 1 `STG` for the benchmark kernel of similar size.

The fix: load the per-trial state into a local (register-resident) array once at kernel entry, run the entire dispatch loop against that local copy, and write it back to global memory once at the end, instead of touching global memory every iteration. Register usage for `find_cycles_chunk` rose from 40 to 64 per thread (still 0 spill), and disassembly confirmed `LDG`/`STG` dropped to 22/27 for the whole kernel (init + loop + epilogue combined, not per iteration). Measured on a controlled, deterministic workload (`--bits 80` guarantees no trial finds a repeat within the step budget, so both versions run the exact same amount of work with no early-exit variance): `--gpu --bits 80 --trials 65536 --gpu-batch-size 65536 --gpu-steps-per-dispatch 65536 --gpu-restarts 1 3276800` (50 dispatches of 65,536 trials × 65,536 steps) dropped from 45.0s to ~31.4-32.0s — about a **30% reduction in real cycle-search wall time**, with cycle-length results verified identical to the un-refactored kernel on known witnesses (16-bit length 1, 52-bit length 11,941,080, including the multi-dispatch case) before and after the change.

Run the practical search in release mode:

```
cargo run --release -- --bits 32
```

Use `--trials` to keep several independent walks and report the shortest cycle found:

```
cargo run --release -- --bits 32 --trials 100
```

Use the CUDA backend for sampled trials (not exhaustive search):

```
cargo run --release -- --gpu --bits 32 --trials 100 --gpu-batch-size 65536
```

That command tests 400 independent seeds by default. To run an exact GPU seed replay, pass `--gpu-restarts 1` together with `--trials 1` and `--seed`.

For long cycle searches, keep the dispatch size bounded and raise the positional maximum as needed:

```
cargo run --release -- --gpu --bits 76 --trials 256 --gpu-batch-size 256 --gpu-steps-per-dispatch 65536 34359738368
```

To search for an improvement over an existing record, use `--max-cycle-length` as a strict cutoff. Brent's discovery limit remains the positional `max`, so long-tail seeds are not discarded prematurely:

```
cargo run --release -- --gpu --bits 52 --trials 256 --gpu-batch-size 256 --max-cycle-length 11941080 34359738368
```

At 76 bits, a random walk is expected to require roughly $2^{38}$ transitions before its first repeat, so even a fast GPU does not make broad 76-100-bit witness searches quick. A bounded run can still produce a useful witness, but a no-result run is only a sample within its time and transition limits.

The default maximum search length is 34,359,738,368 ($2^{35}$), which gives Brent's cycle detector enough headroom to reach and confirm cycle lengths around the current 72-bit witness (10,300,411,851). The same limit can be supplied explicitly as the positional `max` argument:

```
cargo run --release -- --gpu --bits 72 --trials 100 --gpu-batch-size 65536 34359738368
```

## Lowest Observed Cycles

The table below keeps the shortest cycle found for each prefix width. Widths up to 31 bits use `--exhaustive` and are proven global minima; wider entries are empirical minima from `--trials` sampling, not guaranteed global minima.

| Prefix bits | Shortest witness | Cycle length | Method |
| ---: | --- | ---: | --- |
| 8 | `AD00000000000000000000000000000000000000` | 2 | Exhaustive (exact) |
| 12 | `D110000000000000000000000000000000000000` | 5 | Exhaustive (exact) |
| 16 | `268A000000000000000000000000000000000000` | 1 | Exhaustive (exact) |
| 20 | `933D200000000000000000000000000000000000` | 2 | Exhaustive (exact) |
| 24 | `CFE4090000000000000000000000000000000000` | 1 | Exhaustive (exact) |
| 26 | `F732B3C000000000000000000000000000000000` | 2 | Exhaustive (exact) |
| 28 | `5DBFF44000000000000000000000000000000000` | 2 | Exhaustive (exact) |
| 30 | `1DAF2CA400000000000000000000000000000000` | 7 | Exhaustive (exact) |
| 31 | `4FCF0E9000000000000000000000000000000000` | 4 | Exhaustive (exact) |
| 32 | `CC52028900000000000000000000000000000000` | 10 | Sampled (262,144 GPU trials) |
| 36 | `36F1D56E40000000000000000000000000000000` | 925 | Sampled (8,192 GPU trials) |
| 40 | `634A125FBB000000000000000000000000000000` | 265 | Sampled (32,768 GPU trials) |
| 44 | `4920594847700000000000000000000000000000` | 465 | Sampled (32,768 GPU trials) |
| 48 | `7A8012951C550000000000000000000000000000` | 9,085 | Sampled (4,096 GPU trials) |
| 52 | `F2F4B1035E898000000000000000000000000000` | 92,058 | Sampled (2,048 GPU trials) |
| 56 | `FD403B2699646800000000000000000000000000` | 62,075 | Sampled (8 trials) |
| 60 | `392D02E1E7030320000000000000000000000000` | 4,094,338 | Sampled (16 trials) |
| 64 | `FC9585309F356C50000000000000000000000000` | 3,256,762 | Sampled (1 trial) |
| 68 | `867930C10D01F41A900000000000000000000000` | 1,502,305,172 | Sampled (40 trials) |
| 72 | `0BD98290330A1AEB8F0000000000000000000000` | 10,300,411,851 | Sampled (40 trials) |

## Why Exhaustive Search Finds Shorter Cycles

Random restart sampling has a structural blind spot: in a random mapping's functional graph, the starting point a random walk reaches is biased toward whichever cycle has the largest total basin (tail nodes feeding into it), not the shortest cycle. This is why, for example, dozens of independent 60-bit random walks kept landing on the same 28,438,441-length cycle — it likely has an outsized basin, while much shorter cycles (fixed points, 2-cycles) exist in the same graph but have tiny basins that are rarely sampled by chance.

Because each state here is truncated to its leading `bits` bits (the rest zeroed), the reachable state space has exactly $2^{\text{bits}}$ distinct values. For `bits <= 31` that space is small enough to enumerate completely: `--exhaustive` computes the SHA-1 successor of every state in parallel, then finds every cycle in the resulting functional graph with a single linear-time pass (path coloring packed into one flat array, no hash map, no sampling), and reports the true shortest one. This is why 16, 20, 28, 30, and 31 bits dropped from sampled minima of 3, 46, 174, and no prior samples down to proven minima of 1, 2, 2, 7, and 4 — those short cycles exist but have basins far too small to hit by random restarts. The 31-bit search takes roughly 4 GB per array (two arrays, `next[]` and a packed state array, 4 bytes/state each) and completed within a 20-minute budget, which is why exhaustive search is capped there; wider widths still rely on sampling.

Since the traversal is a single sequential pass, `--exhaustive` prints every new best cycle immediately (`... (new best so far)`) and periodic progress to stderr, so a run can be interrupted or bounded with `--timeout-secs` and still report an honest partial result (`... (exhaustive, partial: X/Y states processed, not proven global minimum)`) instead of only reporting at the very end.

## Bits vs. Cycle Length

Cycle lengths above span roughly 8 orders of magnitude, so the chart below plots $\log_2(\text{cycle length})$ against retained prefix bits. Widths at or below 31 bits are exact (exhaustive), so their reference value is 0 by construction: exhaustive search is equivalent to sampling all $2^{\text{bits}}$ states, so $\text{bits}/2 - \log_2(2^{\text{bits}})= -\text{bits}/2$, clamped at 0. For the remaining sampled widths, each table entry is the minimum over `T` independent random walks (Rayon trials), so a flat $\text{bits}/2$ line overstates the expected minimum; if $P(\text{length} \le x) \approx x / 2^{\text{bits}/2}$ near the origin, the minimum of `T` samples scales as $2^{\text{bits}/2} / T$, i.e. $\log_2(\text{cycle length}) \approx \text{bits}/2 - \log_2(T)$ (also clamped at 0). Mermaid has no dedicated scatter/dot chart type, so `xychart-beta` `line` is used; each data point is still rendered as a marker.

```mermaid
xychart-beta
    title "Lowest observed cycle length vs retained prefix bits (log2 scale)"
    x-axis [8, 12, 16, 20, 24, 26, 28, 30, 31, 32, 36, 40, 44, 48, 52, 56, 60, 64]
    y-axis "log2(cycle length)" 0 --> 32
    line [1.00, 2.32, 0.00, 1.00, 0.00, 1.00, 1.00, 2.81, 2.00, 3.46, 9.85, 8.05, 8.86, 14.21, 21.22, 22.78, 24.76, 27.16]
    line [0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 11, 15, 18, 21, 24, 26, 26, 32]
```

The reference line sits at or below the observed minimum for nearly every width. It sits slightly above the data at 44-60 bits (sampling noise from very few trials) and meets it exactly at 64 bits, where the single completed random walk used for the chart is still the shortest of the two successful 64-bit searches on record: a follow-up 8-trial search completed with a much longer cycle (length 1,525,552,541), reinforcing that shorter completed walks are somewhat favored just by having finished within the practical execution window. A 68-bit probe has not yet completed within that window.

## Reference

https://crypto.stackexchange.com/questions/19493/is-there-a-string-thats-hash-is-equal-to-itself
https://www.reddit.com/r/computerscience/comments/p99xqz/what_happens_if_you_apply_a_hash_continually_on/
https://crypto.stackexchange.com/questions/70474/smallest-guaranteed-hash-collision-cycle-length
https://crypto.stackexchange.com/questions/24623/cycles-in-sha-256

For a uniform random function with \(N\) possible output values, the average cycle length is on the order of the square root of \(N\), specifically \(\sqrt{\frac{\pi N}{8}}\).

### Key Statistics for Random Mappings

When you repeatedly apply a uniform random function to an input (creating a sequence like \(x, f(x), f(f(x)), \dots\)), the trajectory eventually falls into a repeating loop (a cycle) preceded by a non-repeating path (a tail).Average Cycle Length: \(\approx 1.253 \times \sqrt{N}\) (or \(\sqrt{\frac{\pi N}{8}}\))Average Tail Length: \(\approx 1.253 \times \sqrt{N}\) (the number of steps before hitting a previously seen value)Total Path Length (Tail + Cycle): \(\approx 1.253 \times \sqrt{2N}\) (or \(\sqrt{\frac{\pi N}{2}}\)), which is the expected number of evaluations required to find a collision using algorithms like Pollard's rho.

#### Practical Application

If a cryptographic hash function has a \(b\)-bit output, then \(N = 2^b\).The expected cycle and collision lengths scale as \(\sqrt{2^b} = 2^{b/2}\).For example, a 256-bit hash function (\(N = 2^{256}\)) has an expected cycle length on the order of \(2^{128}\).Would you like to explore how this applies to collision-finding algorithms like Pollard's rho, or do you need help with a specific bit-length calculation?Cryptography Stack ExchangeCycles in SHA-256Mar 25, 2015 — SHA-256 is designed to behave as a random function. Under this assumption, it is expected that for most 256-bit values, there is n...RedditWhat happens if you apply a hash continually on itself? Will it ...Aug 22, 2021 — seedubjay_ • 5y ago If the hash function forms a permutation cipher, the cycle will have on average (N+1)/2 hashes. If the hash fu...Semantic Scholar[PDF] Random Mapping Statistics - Semantic ScholarRandom Mappings with Restricted Preimages · A. MacFieD. Panario. Computer Science, Mathematics. LATINCRYPT. 2012. TLDR. An element...
